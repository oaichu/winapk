//! Install pipeline:
//!   expand(.xapk/.apks/.apkm → parts) → analyze(base) → smart ABI
//!   → adb install | install-multiple → verify pm path
//!   → async AOT (guest-side setsid compile, non-blocking)
//!   → icon.ICO → .lnk → store.
//!
//! ABI rule: native x86_64 only when its lib set COVERS arm64-v8a's —
//! the Unity/Unreal trap is an apk whose x86_64 dir holds a handful of
//! ad/bug-report .so files while the engine lives only under arm64;
//! pinning x86_64 there crashes on UnsatisfiedLinkError. Incomplete
//! coverage → force `--abi arm64-v8a` so libndk_translation handles
//! the app wholesale.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use winapk_core::adb::Adb;
use winapk_core::error::{Error, Result};
use winapk_core::paths::Paths;
use winapk_core::{info, warn};

use crate::apk::{analyze, merge_libs, ApkInfo};
use crate::{icon, shortcut, store::{AppRecord, Store}};

#[derive(Debug)]
pub struct InstallReport {
    pub info: ApkInfo,
    pub abi_used: Option<String>,
    /// True when a full `-m speed` compile was QUEUED in the guest —
    /// install no longer blocks on it (session 1 fix: AOT is async).
    pub aot_done: bool,
    pub shortcut: Option<std::path::PathBuf>,
    pub ico: std::path::PathBuf,
}

fn pick_abi(libs: &BTreeMap<String, Vec<String>>) -> Option<&'static str> {
    let get = |a: &str| libs.get(a).map(|v| v.as_slice()).unwrap_or(&[]);
    let (x64, x86, arm64, armv7) =
        (get("x86_64"), get("x86"), get("arm64-v8a"), get("armeabi-v7a"));
    if !x64.is_empty() {
        // Coverage: every arm64 lib must exist in x86_64 too, else the
        // app is arm-native with a mixed-ABI decoy → run it translated.
        if arm64.iter().all(|l| x64.contains(l)) {
            return Some("x86_64");
        }
        warn!(
            "mixed-ABI: x86_64 lacks {} lib(s) that arm64 ships — forcing arm64-v8a translation",
            arm64.iter().filter(|l| !x64.contains(l)).count()
        );
        return Some("arm64-v8a");
    }
    if !arm64.is_empty() || !armv7.is_empty() {
        // ARM-only: translate everything rather than let PM pick a
        // partial x86 dir from a split.
        return Some("arm64-v8a");
    }
    if !x86.is_empty() {
        return Some("x86");
    }
    None // pure-Java — let PackageManager decide
}

/// Expand a package file to component APKs. Bare .apk → itself.
/// .xapk/.apks/.apkm are zips of *.apk (+manifest.json/icon.png):
/// extract to a temp dir, return [base, splits...] plus the temp dir
/// so the caller can clean it after install.
fn expand_package(path: &Path, tmp_root: &Path) -> Result<(Vec<PathBuf>, Option<PathBuf>)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "apk" {
        return Ok((vec![path.to_path_buf()], None));
    }
    let f = std::fs::File::open(path)?;
    let mut z = zip::ZipArchive::new(std::io::BufReader::new(f))
        .map_err(|e| Error::Apk(format!("{ext} container: {e}")))?;
    let out = tmp_root.join(format!(
        "pkg-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&out)?;
    let mut names = Vec::new();
    for i in 0..z.len() {
        let mut e = z.by_index(i).map_err(|e2| Error::Apk(format!("{e2}")))?;
        let n = e.name().to_string();
        // skip dirs, non-apk payloads, and obb/expansion entries
        if e.is_dir() || !n.to_ascii_lowercase().ends_with(".apk") {
            continue;
        }
        let dest = out.join(n.rsplit('/').next().unwrap_or(&n));
        let mut w = std::fs::File::create(&dest)?;
        std::io::copy(&mut e, &mut w)?;
        names.push(dest);
    }
    if names.is_empty() {
        let _ = std::fs::remove_dir_all(&out);
        return Err(Error::Apk(format!("no .apk members inside {ext}")));
    }
    Ok((names, Some(out)))
}

/// Order parts [base, splits...] by manifest inspection: base is the
/// member whose <manifest> has no `split` attr.
fn order_parts(parts: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<Result<ApkInfo>>) {
    let mut analyzed: Vec<Result<ApkInfo>> = parts.iter().map(|p| analyze(p)).collect();
    let base_idx = analyzed
        .iter()
        .position(|r| r.as_ref().map(|i| i.split.is_none() && !i.package.is_empty()).unwrap_or(false))
        .unwrap_or(0);
    let mut ordered = Vec::with_capacity(parts.len());
    let mut ordered_info = Vec::with_capacity(analyzed.len());
    ordered.push(parts[base_idx].clone());
    ordered_info.push(analyzed.remove(base_idx));
    for (i, p) in parts.into_iter().enumerate() {
        if i != base_idx {
            ordered.push(p);
        }
    }
    // keep infos aligned with ordered tail — re-analyze cheap? no:
    // rebuild the vec (base at 0, rest in original order minus base)
    let mut tail: Vec<Result<ApkInfo>> = Vec::new();
    // analyzed had base removed; remaining order matches parts minus base — but we removed by index so it's consistent
    tail.extend(analyzed);
    ordered_info.extend(tail);
    (ordered, ordered_info)
}

/// Full install. `adb` must already point at a booted device.
pub fn install_apk(
    apk: &Path,
    adb: &Adb,
    paths: &Paths,
    winapk_exe: &Path,
    make_shortcut: bool,
) -> Result<InstallReport> {
    let (parts, cleanup_dir) = expand_package(apk, &paths.tmp_dir())?;
    let result = install_parts(apk, parts, adb, paths, winapk_exe, make_shortcut);
    if let Some(d) = cleanup_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    result
}

fn install_parts(
    src: &Path,
    parts: Vec<PathBuf>,
    adb: &Adb,
    paths: &Paths,
    winapk_exe: &Path,
    make_shortcut: bool,
) -> Result<InstallReport> {
    let (ordered, infos) = order_parts(parts);
    let mut info = infos
        .into_iter()
        .next()
        .and_then(|r| r.ok())
        .ok_or_else(|| Error::Apk("base apk unreadable".into()))?;
    let is_multi = ordered.len() > 1;
    info!(
        "install {} v{} ({}) abis={:?}{}",
        info.package,
        info.version_name.as_deref().unwrap_or("?"),
        info.label,
        info.abis,
        if is_multi { format!(" [{} parts]", ordered.len()) } else { String::new() }
    );

    // ABI decision on the UNION lib set (abi splits carry the payload).
    if is_multi {
        let other_maps: Vec<_> = ordered
            .iter()
            .skip(1)
            .filter_map(|p| analyze(p).ok())
            .map(|i| i.libs)
            .collect();
        info.libs = merge_libs(
            &std::iter::once(info.libs.clone())
                .chain(other_maps.into_iter())
                .collect::<Vec<_>>(),
        );
        info.abis = info.libs.keys().cloned().collect();
    }
    let abi = pick_abi(&info.libs).map(str::to_string);
    if let Some(a) = &abi {
        info!("abi pin: {a}");
    }

    // PackageManager is fragile right after a cold boot: verifier
    // timeouts and "Broken pipe" while services restart. One retry
    // after a settle delay covers the transient window.
    let do_install = |adb: &Adb| -> Result<()> {
        if is_multi {
            adb.install_multiple(&ordered, abi.as_deref(), Duration::from_secs(300))
        } else {
            adb.install(&ordered[0], abi.as_deref(), Duration::from_secs(300))
        }
    };
    if let Err(e) = do_install(adb) {
        warn!("install attempt 1 failed ({e}) — retrying in 15s");
        std::thread::sleep(Duration::from_secs(15));
        do_install(adb)?;
    }

    // `pm path` can also lie transiently post-boot — poll briefly.
    let mut installed = adb.is_installed(&info.package);
    for _ in 0..6 {
        if installed {
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
        installed = adb.is_installed(&info.package);
    }
    if !installed {
        return Err(Error::Adb(format!(
            "pm path reports not installed: {}",
            info.package
        )));
    }

    // Async AOT: the dexopt run happens IN-GUEST under setsid — the adb
    // shell returns immediately and the compile survives disconnects.
    // (Blocking `-m speed` on a big app stalled install 30-90s.)
    let (aot_ok, aot_out) = adb.try_shell(
        &format!(
            "setsid nohup cmd package compile -m speed -f {} >/dev/null 2>&1 &",
            info.package
        ),
        Duration::from_secs(15),
    );
    let aot_done = aot_ok;
    if aot_done {
        info!("AOT speed-compile queued for {} (async)", info.package);
    } else {
        warn!("aot queue {} failed: {} (JIT warmup instead)", info.package, aot_out);
    }

    let ico = paths.icon_ico(&info.package);
    icon::write_ico(&ordered[0], info.icon_entry.as_deref(), &info.package, &ico)?;

    let lnk = if make_shortcut {
        match shortcut::create_shortcut(&info.label, &info.package, winapk_exe, &ico) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!("shortcut: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut store = Store::load(&paths.store_file());
    store.upsert(AppRecord {
        pkg: info.package.clone(),
        label: info.label.clone(),
        version_name: info.version_name.clone(),
        version_code: info.version_code,
        apk_path: src.to_string_lossy().into(),
        icon: ico.to_string_lossy().into(),
        shortcut: lnk.as_ref().map(|p| p.to_string_lossy().into()),
        installed_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        aot_done,
    });
    store.save(&paths.store_file())?;

    Ok(InstallReport {
        info,
        abi_used: abi,
        aot_done,
        shortcut: lnk,
        ico,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn libs(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(a, ns)| {
                (a.to_string(), ns.iter().map(|s| s.to_string()).collect())
            })
            .collect()
    }

    #[test]
    fn full_x64_coverage_picks_native() {
        // x86_64 ⊇ arm64 → native
        let l = libs(&[
            ("x86_64", &["libunity.so", "libil2cpp.so", "libad.so"]),
            ("arm64-v8a", &["libunity.so", "libil2cpp.so"]),
        ]);
        assert_eq!(pick_abi(&l), Some("x86_64"));
    }

    #[test]
    fn mixed_abi_decoy_picks_arm64() {
        // Unity trap: x86_64 has only an ad lib — force translation.
        let l = libs(&[
            ("x86_64", &["libad.so"]),
            ("arm64-v8a", &["libunity.so", "libil2cpp.so", "libad.so"]),
        ]);
        assert_eq!(pick_abi(&l), Some("arm64-v8a"));
    }

    #[test]
    fn arm_only_translates() {
        let l = libs(&[("arm64-v8a", &["libunity.so"])]);
        assert_eq!(pick_abi(&l), Some("arm64-v8a"));
    }

    #[test]
    fn x64_only_picks_x64() {
        let l = libs(&[("x86_64", &["liba.so"])]);
        assert_eq!(pick_abi(&l), Some("x86_64"));
    }

    #[test]
    fn pure_java_unpinned() {
        assert_eq!(pick_abi(&BTreeMap::new()), None);
    }
}
