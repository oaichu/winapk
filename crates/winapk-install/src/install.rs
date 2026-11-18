//! Install pipeline:
//!   analyze(apk) → adb install --abi <best> → verify pm path
//!   → AOT `cmd package compile -m speed -f` → icon.ICO → .lnk → store.
//!
//! ABI order: prefer a real x86_64 build (native execution) over the
//! ARM→x86 translation layer; only fall back to no-abi when the APK is
//! pure-Java or ARM-only.

use std::path::Path;
use std::time::Duration;

use winapk_core::adb::Adb;
use winapk_core::error::{Error, Result};
use winapk_core::paths::Paths;
use winapk_core::{info, warn};

use crate::apk::{analyze, ApkInfo};
use crate::{icon, shortcut, store::{AppRecord, Store}};

#[derive(Debug)]
pub struct InstallReport {
    pub info: ApkInfo,
    pub abi_used: Option<String>,
    pub aot_done: bool,
    pub shortcut: Option<std::path::PathBuf>,
    pub ico: std::path::PathBuf,
}

fn pick_abi(abis: &[String]) -> Option<&'static str> {
    if abis.iter().any(|a| a == "x86_64") {
        Some("x86_64")
    } else if abis.iter().any(|a| a == "arm64-v8a") {
        // translated by libndk_translation inside the guest — install
        // without an --abi pin so the package manager picks its best.
        None
    } else if abis.iter().any(|a| a == "x86") {
        Some("x86")
    } else {
        None
    }
}

/// Full install. `adb` must already point at a booted device.
pub fn install_apk(
    apk: &Path,
    adb: &Adb,
    paths: &Paths,
    winapk_exe: &Path,
    make_shortcut: bool,
) -> Result<InstallReport> {
    let info = analyze(apk)?;
    info!(
        "install {} v{} ({}) abis={:?}",
        info.package,
        info.version_name.as_deref().unwrap_or("?"),
        info.label,
        info.abis
    );

    let abi = pick_abi(&info.abis).map(str::to_string);
    // PackageManager is fragile right after a cold boot: verifier
    // timeouts and "Broken pipe" while services restart. One retry
    // after a settle delay covers the transient window.
    if let Err(e) = adb.install(apk, abi.as_deref(), Duration::from_secs(300)) {
        warn!("install attempt 1 failed ({e}) — retrying in 15s");
        std::thread::sleep(Duration::from_secs(15));
        adb.install(apk, abi.as_deref(), Duration::from_secs(300))?;
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

    // AOT: kill JIT warmup stutter. Long apps take a while — 10 min cap.
    let aot_done = match adb.aot_compile(&info.package, Duration::from_secs(600)) {
        Ok(()) => {
            info!("AOT speed-compile done for {}", info.package);
            true
        }
        Err(e) => {
            warn!("aot_compile {}: {e} (app still usable via JIT)", info.package);
            false
        }
    };

    let ico = paths.icon_ico(&info.package);
    icon::write_ico(apk, info.icon_entry.as_deref(), &info.package, &ico)?;

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
        apk_path: apk.to_string_lossy().into(),
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
