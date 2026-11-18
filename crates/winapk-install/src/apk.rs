//! APK inspection: open the zip, parse AndroidManifest.xml (binary),
//! resolve label/icon through resources.arsc, enumerate native ABIs.

use std::io::{Read, Seek};
use std::path::Path;

use winapk_core::error::{Error, Result};

use crate::arsc::{ResTable, ResValue};
use crate::axml::{self, AttrValue, Node};

#[derive(Debug, Clone)]
pub struct ApkInfo {
    pub package: String,
    pub version_name: Option<String>,
    pub version_code: Option<i64>,
    /// Display label (resolved; falls back to package name).
    pub label: String,
    /// Launcher component "pkg/.MainActivity" when derivable.
    pub launcher: Option<String>,
    /// Native ABIs shipped under lib/ — drives `adb install --abi`.
    pub abis: Vec<String>,
    /// abi → sorted basenames of the .so files it ships. Drives the
    /// mixed-ABI check: x86_64 must COVER arm64's set or we force
    /// `--abi arm64-v8a` so translation handles the whole app.
    pub libs: std::collections::BTreeMap<String, Vec<String>>,
    /// manifest `split` attr — non-base members of a split package.
    pub split: Option<String>,
    /// Zip entry name of the best-density raster icon, if any.
    pub icon_entry: Option<String>,
    pub min_sdk: Option<i64>,
    pub extract_native_libs: bool,
}

fn read_entry<R: Read + Seek>(z: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    const MAX_ENTRY: u64 = 256 * 1024 * 1024; // manifest/arsc are small;
    let f = z.by_name(name).map_err(|e| Error::Apk(format!("{name}: {e}")))?;
    // The zip-reported size is attacker-controlled — cap the buffer both
    // in preallocation (can't OOM on a 4GB claim) and in actual reads.
    let mut b = Vec::with_capacity((f.size() as usize).min(64 << 20));
    f.take(MAX_ENTRY).read_to_end(&mut b)?;
    Ok(b)
}

pub fn analyze(apk: &Path) -> Result<ApkInfo> {
    let f = std::fs::File::open(apk)?;
    let mut z = zip::ZipArchive::new(std::io::BufReader::new(f))
        .map_err(|e| Error::Apk(format!("open zip: {e}")))?;

    let manifest_bytes = read_entry(&mut z, "AndroidManifest.xml")?;
    let doc = axml::parse_axml(&manifest_bytes)?;
    let m = axml::manifest_root(&doc).ok_or_else(|| Error::Apk("no <manifest>".into()))?;

    // <manifest package="..."> has no namespace.
    let package = m
        .attrs
        .iter()
        .find(|a| a.name == "package" && a.ns.is_empty())
        .and_then(|a| match &a.value {
            Some(AttrValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .ok_or_else(|| Error::Apk("manifest missing package".into()))?;

    // The package name lands in shell commands (am force-stop, pm path,
    // AOT compile). Reject anything outside the Java-package charset —
    // a crafted manifest can't smuggle metacharacters downstream.
    if package.is_empty()
        || package.len() > 256
        || !package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return Err(Error::Apk(format!(
            "manifest package name has invalid characters: {package:?}"
        )));
    }

    let version_code = m.attr_value("versionCode").and_then(|v| match v {
        AttrValue::Int(i) => Some(*i),
        AttrValue::Str(s) => s.parse().ok(),
        _ => None,
    });
    let version_name = m.attr_str("versionName");

    let res_bytes = read_entry(&mut z, "resources.arsc").ok();
    let table = res_bytes
        .as_deref()
        .and_then(|b| ResTable::parse(b).ok());

    // label: may be a string or @ref
    let app = m.children_named("application").next();
    let label = app
        .and_then(|a| label_of(a, table.as_ref()))
        .unwrap_or_else(|| package.clone());

    // launcher activity: <activity> with MAIN + LAUNCHER intent-filter
    let mut launcher: Option<String> = None;
    if let Some(app) = app {
        'outer: for a in app.children_named("activity").chain(app.children_named("activity-alias")) {
            let name = a
                .attr_str("name")
                .or_else(|| {
                    a.attr_str("targetActivity") // activity-alias
                });
            let Some(act) = name else { continue };
            for f in a.children_named("intent-filter") {
                let mut main = false;
                let mut launcher_cat = false;
                for ch in &f.children {
                    if ch.name == "action"
                        && ch.attr_str("name").as_deref()
                            == Some("android.intent.action.MAIN")
                    {
                        main = true;
                    }
                    if ch.name == "category"
                        && ch.attr_str("name").as_deref()
                            == Some("android.intent.category.LAUNCHER")
                    {
                        launcher_cat = true;
                    }
                }
                if main && launcher_cat {
                    launcher = Some(qualify(&package, &act));
                    break 'outer;
                }
            }
        }
    }

    // icon: @ref → arsc → res path; else direct path / mipmap scan
    let mut icon_entry = app.and_then(|a| icon_of(a, table.as_ref()));
    if icon_entry.is_none() {
        icon_entry = guess_icon_entry(&mut z);
    }

    let mut libs: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for n in z.file_names() {
        let mut it = n.split('/');
        if it.next() == Some("lib") {
            if let (Some(abi), Some(name)) = (it.next(), it.next()) {
                if !name.is_empty() {
                    libs.entry(abi.to_string())
                        .or_default()
                        .push(name.to_string());
                }
            }
        }
    }
    for v in libs.values_mut() {
        v.sort();
    }
    let mut abis: Vec<String> = libs.keys().cloned().collect();
    abis.sort();

    // <manifest split="foo"> marks a non-base split member.
    let split = m
        .attrs
        .iter()
        .find(|a| a.name == "split" && a.ns.is_empty())
        .and_then(|a| match &a.value {
            Some(AttrValue::Str(s)) => Some(s.clone()),
            _ => None,
        });

    let extract_native_libs = app
        .and_then(|a| a.attr_value("extractNativeLibs").cloned())
        .map(|v| !matches!(v, AttrValue::Bool(false)))
        .unwrap_or(true);

    // <uses-sdk> is a child of <manifest>, not <application> — the old
    // lookup under `app` could never find it (min_sdk was always None).
    let min_sdk = m
        .children_named("uses-sdk")
        .next()
        .and_then(|u| u.attr_value("minSdkVersion").cloned())
        .and_then(|v| match v {
            AttrValue::Int(i) => Some(i),
            _ => None,
        });

    Ok(ApkInfo {
        package,
        version_name,
        version_code,
        label,
        launcher,
        abis,
        libs,
        split,
        icon_entry,
        min_sdk,
        extract_native_libs,
    })
}

/// Union the lib maps of several component APKs (base + abi splits) —
/// for .xapk/.apks bundles the x86_64/arm64 payloads live in separate
/// config splits, so the coverage check must see the merged set.
pub fn merge_libs(
    maps: &[std::collections::BTreeMap<String, Vec<String>>],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out: std::collections::BTreeMap<String, Vec<String>> =
        Default::default();
    for m in maps {
        for (abi, names) in m {
            let e = out.entry(abi.clone()).or_default();
            for n in names {
                if !e.contains(n) {
                    e.push(n.clone());
                }
            }
        }
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

/// ".MainActivity" → "pkg/.MainActivity"; "com.x.A" → "pkg/com.x.A".
fn qualify(pkg: &str, act: &str) -> String {
    if act.starts_with('.') {
        format!("{pkg}{act}")
    } else if act.contains('.') {
        format!("{pkg}/{act}")
    } else {
        format!("{pkg}/.{act}")
    }
}

fn label_of(app: &Node, table: Option<&ResTable>) -> Option<String> {
    if let Some(r) = app.attr_ref("label") {
        if let Some(t) = table {
            if let Some(ResValue::Str(s)) = t.resolve(r, false, 0) {
                return Some(s);
            }
        }
    }
    match app.attr_value("label") {
        Some(AttrValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn icon_of(app: &Node, table: Option<&ResTable>) -> Option<String> {
    let a = app.attr("icon").or_else(|| app.attr("roundIcon"))?;
    if let Some(t) = table {
        if let Some(r) = a.ref_id {
            if let Some(ResValue::Str(path)) = t.resolve(r, true, 0) {
                if path.starts_with("res/") {
                    return Some(path);
                }
            }
        }
    }
    // raw string path
    if let Some(AttrValue::Str(s)) = &a.value {
        if s.starts_with("res/") {
            return Some(s.clone());
        }
    }
    None
}

/// Scan res/ for the biggest raster launcher icon when the manifest
/// points at an adaptive (XML) or unresolvable icon.
fn guess_icon_entry<R: Read + Seek>(z: &mut zip::ZipArchive<R>) -> Option<String> {
    let mut best: Option<(i32, String)> = None;
    for n in z.file_names() {
        let l = n.to_lowercase();
        if !(l.starts_with("res/") && (l.ends_with(".png") || l.ends_with(".webp"))) {
            continue;
        }
        if !(l.contains("ic_launcher") || l.contains("icon")) {
            continue;
        }
        let score = density_score(&l);
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, n.to_string()));
        }
    }
    best.map(|(_, n)| n)
}

fn density_score(path: &str) -> i32 {
    for (tag, s) in [
        ("xxxhdpi", 6),
        ("xxhdpi", 5),
        ("xhdpi", 4),
        ("hdpi", 3),
        ("mdpi", 2),
        ("ldpi", 1),
    ] {
        if path.contains(tag) {
            return s;
        }
    }
    0
}

/// Extract raw bytes of a zip entry (icon extraction).
pub fn entry_bytes(apk: &Path, name: &str) -> Result<Vec<u8>> {
    let f = std::fs::File::open(apk)?;
    let mut z = zip::ZipArchive::new(std::io::BufReader::new(f))
        .map_err(|e| Error::Apk(format!("open zip: {e}")))?;
    read_entry(&mut z, name)
}
