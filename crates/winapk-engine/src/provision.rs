//! Runtime provisioning from dl.google.com — same sources as the Android
//! SDK Manager, but fetched directly (no sdkmanager dependency):
//!
//!   repository2-3.xml                → emulator-windows_x64-*.zip
//!   sys-img/google_apis_playstore/…  → x86_64-34_rNN.zip
//!   platform-tools-latest-windows.zip, aehd-windows_v2.2.zip (fallback URLs)
//!
//! Nothing is slim-cut: the full emulator tree (Vulkan backend, gfxstream,
//! Qt UI) is kept so `-gpu host` can actually pass through.

use std::fs;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use winapk_core::error::{Error, Result};
use winapk_core::{info, paths::Paths, proc};

const REPO: &str = "https://dl.google.com/android/repository";
const SDK_INDEX: &str = "https://dl.google.com/android/repository/repository2-3.xml";
// google_apis (NOT google_apis_playstore): the playstore flavour is a `user`
// build (ro.debuggable=0, ro.adb.secure=1) whose ADB auth dialog cannot be
// auto-accepted — headless automation dead-ends on `unauthorized`. The
// google_apis image is userdebug: adbd trusts seeded keys and boots straight
// to `device`. It still ships Google APIs, only the Play Store app is absent.
const PLAY_IMG_INDEX: &str =
    "https://dl.google.com/android/repository/sys-img/google_apis/sys-img2-3.xml";
const PLATFORM_TOOLS: &str =
    "https://dl.google.com/android/repository/platform-tools-latest-windows.zip";
const AEHD_FALLBACK: &str = "https://dl.google.com/android/repository/aehd-windows_v2.2.zip";
const EMULATOR_FALLBACK: &str =
    "https://dl.google.com/android/repository/emulator-windows_x64-16847572.zip";
const SYSIMG_FALLBACK: &str =
    "https://dl.google.com/android/repository/sys-img/google_apis/x86_64-34_r10.zip";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeManifest {
    pub emulator_zip: String,
    pub sysimg_zip: String,
    pub platform_tools: String,
    pub aehd: String,
    /// Points elsewhere when a custom AOSP image is dropped in — the engine
    /// just reads whatever manifest says.
    pub sysimg_dir: String,
}

impl Default for RuntimeManifest {
    fn default() -> Self {
        Self {
            emulator_zip: EMULATOR_FALLBACK.into(),
            sysimg_zip: SYSIMG_FALLBACK.into(),
            platform_tools: PLATFORM_TOOLS.into(),
            aehd: AEHD_FALLBACK.into(),
            sysimg_dir: String::new(),
        }
    }
}

pub struct Provisioner<'a> {
    paths: &'a Paths,
}

/// What still needs to be fetched/installed.
#[derive(Debug, Default)]
pub struct Status {
    pub platform_tools: bool,
    pub emulator: bool,
    pub sysimg: bool,
}

impl<'a> Provisioner<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        Self { paths }
    }

    pub fn status(&self) -> Status {
        Status {
            platform_tools: self.paths.adb_exe().exists(),
            emulator: self.paths.emulator_exe().exists(),
            sysimg: self.paths.sysimg_dir().join("system.img").exists()
                || self.paths.sysimg_dir().join("system-qemu.img").exists(),
        }
    }

    pub fn is_complete(&self) -> bool {
        let s = self.status();
        s.platform_tools && s.emulator && s.sysimg
    }

    /// Full provision: platform-tools → emulator → sysimg → aehd.
    /// `progress` gets (stage, bytes_done, bytes_total_or_0).
    pub fn provision<F: Fn(&str, u64, u64)>(&self, progress: F) -> Result<RuntimeManifest> {
        self.paths.ensure_all()?;
        let mut manifest = self.load_manifest().unwrap_or_default();
        self.resolve_latest(&mut manifest);

        let s = self.status();
        if !s.platform_tools {
            progress("platform-tools", 0, 0);
            let zip = self.paths.tmp_dir().join("platform-tools.zip");
            fetch_or_bundle("platform-tools.zip", &manifest.platform_tools, &zip)?;
            extract_zip(&zip, &self.paths.runtime())?;
            let _ = fs::remove_file(&zip);
        }
        if !s.emulator {
            progress("emulator", 0, 0);
            let zip = self.paths.tmp_dir().join("emulator.zip");
            fetch_or_bundle("emulator.zip", &manifest.emulator_zip, &zip)?;
            // emulator-*.zip roots a top-level "emulator/" directory.
            extract_zip(&zip, &self.paths.runtime())?;
            let _ = fs::remove_file(&zip);
            if !self.paths.emulator_exe().exists() {
                return Err(Error::Provision(
                    "emulator.exe missing after extract".into(),
                ));
            }
        }
        if !s.sysimg {
            progress("system-image", 0, 0);
            let zip = self.paths.tmp_dir().join("sysimg.zip");
            fetch_or_bundle("sysimg.zip", &manifest.sysimg_zip, &zip)?;
            // The zip wraps a top-level dir (x86_64/) — extract to tmp,
            // then move the dir containing system.img into place.
            let tmp = self.paths.tmp_dir().join("sysimg-extract");
            let _ = fs::remove_dir_all(&tmp);
            extract_zip(&zip, &tmp)?;
            let _ = fs::remove_file(&zip);
            let dest = self.paths.sysimg_dir();
            let img_root = find_dir_with(&tmp, "system.img")
                .ok_or_else(|| Error::Provision("system.img not in sysimg zip".into()))?;
            if dest.exists() {
                fs::remove_dir_all(&dest)?;
            }
            if let Some(d) = dest.parent() {
                fs::create_dir_all(d)?;
            }
            fs::rename(&img_root, &dest)?;
            let _ = fs::remove_dir_all(&tmp);
        }
        // AEHD is best-effort: WHPX is the fallback accelerator.
        let aehd_zip = self.paths.tmp_dir().join("aehd.zip");
        if !self.paths.aehd_dir().join("gvm.dll").exists() {
            if fetch_or_bundle("aehd.zip", &manifest.aehd, &aehd_zip).is_ok() {
                let _ = extract_zip(&aehd_zip, &self.paths.aehd_dir());
            }
            let _ = fs::remove_file(&aehd_zip);
        }
        manifest.sysimg_dir = self.paths.sysimg_dir().to_string_lossy().into();
        self.save_manifest(&manifest)?;
        Ok(manifest)
    }

    /// Query the SDK indexes for the newest matching zips; silently keeps
    /// the baked-in fallback URLs if the index can't be reached.
    fn resolve_latest(&self, m: &mut RuntimeManifest) {
        if let Ok(text) = fetch_text(SDK_INDEX, Duration::from_secs(20)) {
            if let Some(u) = pick_emulator_url(&text) {
                m.emulator_zip = abs(REPO, &u);
            }
        }
        if let Ok(text) = fetch_text(PLAY_IMG_INDEX, Duration::from_secs(20)) {
            if let Some(u) = pick_sysimg_url(&text) {
                m.sysimg_zip = abs(&format!("{REPO}/sys-img/google_apis"), &u);
            }
        }
    }

    fn load_manifest(&self) -> Option<RuntimeManifest> {
        let p = self.paths.runtime_manifest();
        serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
    }

    /// sysimg dir recorded in the manifest (empty → default layout).
    pub fn load_manifest_dir(&self) -> Option<String> {
        self.load_manifest()
            .map(|m| m.sysimg_dir)
            .filter(|s| !s.is_empty())
    }

    fn save_manifest(&self, m: &RuntimeManifest) -> Result<()> {
        let p = self.paths.runtime_manifest();
        if let Some(d) = p.parent() {
            fs::create_dir_all(d)?;
        }
        fs::write(p, serde_json::to_string_pretty(m).unwrap())?;
        Ok(())
    }
}

/// Minimal HTTP GET → String for the SDK XML indexes (curl.exe, 20s cap).
fn fetch_text(url: &str, timeout: Duration) -> Result<String> {
    let mut c = std::process::Command::new("curl.exe");
    c.args(["-fsSL", "--connect-timeout", "10", url]);
    let out = proc::run(&mut c, Some(timeout))?;
    if !out.status.success() {
        return Err(Error::Provision(format!(
            "GET {url}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn fetch(url: &str, dest: &Path) -> Result<()> {
    info!("download {url}");
    proc::curl_download(url, dest, Duration::from_secs(60 * 30))
}

/// MSIX package ships the runtime zips under `<exe>/bundle/` — Store
/// policy frowns on downloading executables post-install, so when a
/// bundled copy exists it wins over the network fetch.
fn bundle_zip(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("bundle").join(name);
    p.exists().then_some(p)
}

fn fetch_or_bundle(name: &str, url: &str, dest: &Path) -> Result<()> {
    if let Some(b) = bundle_zip(name) {
        info!("bundled runtime: {}", b.display());
        fs::copy(&b, dest).map_err(Error::Io)?;
        return Ok(());
    }
    fetch(url, dest)
}

fn abs(base: &str, rel_or_abs: &str) -> String {
    if rel_or_abs.starts_with("http") {
        rel_or_abs.to_string()
    } else {
        format!("{base}/{rel_or_abs}")
    }
}

/// Find the newest `emulator-windows_x64` archive in repository2-3.xml.
fn pick_emulator_url(xml: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut best_rev = 0u64;
    // Archives look like: <url>emulator-windows_x64-12345678.zip</url>
    let mut rest = xml;
    while let Some(i) = rest.find("emulator-windows_x64-") {
        let tail = &rest[i..];
        let end = tail.find('<').unwrap_or(tail.len());
        let name = &tail[..end];
        let rev = name
            .trim_start_matches("emulator-windows_x64-")
            .trim_end_matches(".zip")
            .parse::<u64>()
            .unwrap_or(0);
        if rev > best_rev {
            best_rev = rev;
            best = Some(name.to_string());
        }
        rest = &tail[end..];
    }
    best
}

/// Find the api-34 x86_64 archive inside the playstore sys-img index.
/// Entries look like `x86_64-34_r14.zip`; pick the highest revision.
fn pick_sysimg_url(xml: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut best_rev = 0u64;
    let mut rest = xml;
    while let Some(i) = rest.find("x86_64-34_r") {
        let tail = &rest[i..];
        let end = tail.find('<').unwrap_or(tail.len());
        let name = &tail[..end];
        if !name.ends_with(".zip") {
            rest = &tail[end.min(11)..];
            continue;
        }
        let rev = name
            .trim_start_matches("x86_64-34_r")
            .trim_end_matches(".zip")
            .parse::<u64>()
            .unwrap_or(0);
        if rev >= best_rev {
            best_rev = rev;
            best = Some(name.to_string());
        }
        rest = &tail[end..];
    }
    best
}

/// Extract a zip, preserving directory structure, skipping paths that
/// would escape `dest` (zip-slip guard).
pub fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let zerr = |e: zip::result::ZipError| Error::Provision(format!("zip: {e}"));
    let f = std::fs::File::open(zip_path)?;
    let mut ar = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(zerr)?;
    fs::create_dir_all(dest)?;
    for i in 0..ar.len() {
        let mut e = ar.by_index(i).map_err(zerr)?;
        let Some(name) = e.enclosed_name() else {
            continue;
        };
        let out: PathBuf = dest.join(name);
        if e.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(d) = out.parent() {
            fs::create_dir_all(d)?;
        }
        let mut w = std::io::BufWriter::new(fs::File::create(&out)?);
        std::io::copy(&mut e, &mut w)?;
        // Restore the exec bit is meaningless on Windows; skipped.
    }
    Ok(())
}

/// Peek at a single entry's contents (used by the APK parser's callers too).
pub fn zip_entry_bytes(zip_path: &Path, name: &str) -> Result<Vec<u8>> {
    let zerr = |e: zip::result::ZipError| Error::Provision(format!("zip: {e}"));
    let f = std::fs::File::open(zip_path)?;
    let mut ar = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(zerr)?;
    let mut e = ar.by_name(name).map_err(zerr)?;
    let mut buf = Vec::with_capacity(e.size() as usize);
    e.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Needed to satisfy Seek bound on some zip versions.
#[allow(dead_code)]
fn _assert_seek<T: Read + Seek>(_t: T) {}

/// Depth-first search for the directory that directly contains `file`.
fn find_dir_with(root: &Path, file: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if d.join(file).exists() {
            return Some(d);
        }
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                }
            }
        }
    }
    None
}
