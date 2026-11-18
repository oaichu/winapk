//! Accelerator detection: AEHD (Google's hypervisor driver, a.k.a. gvm),
//! WHPX (Windows Hypervisor Platform), or TCG fallback.
//!
//! `emulator -accel-check` is the authoritative probe; the registry check
//! for the gvm service is a fast pre-check that avoids spawning when the
//! driver plainly isn't there.

use std::path::Path;
use std::time::Duration;

use winapk_core::proc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// Android Emulator Hypervisor Driver (gvm) — preferred on Win11.
    Aehd,
    /// Windows Hypervisor Platform — always available once the feature is on.
    Whpx,
    /// No hardware accel; QEMU TCG. Unusable for real apps — reported so the
    /// CLI can refuse boot with a useful message instead of hanging 20 min.
    None,
}

impl Accel {
    pub fn arg(self) -> &'static str {
        match self {
            // Emulator 37.x accepts only on/off/auto — "whpx"/"aehd"
            // literals are rejected outright. "on" forces the best
            // hardware accel available (AEHD > WHPX) or fails fast.
            Accel::Aehd | Accel::Whpx => "on",
            Accel::None => "off",
        }
    }
}

pub fn gvm_installed() -> bool {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SYSTEM\\CurrentControlSet\\Services\\gvm")
        .is_ok()
}

pub fn hyperv_available() -> bool {
    // WHPX capability lives in the OS registry, not a service key.
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Virtualization")
        .is_ok()
}

/// Ask the emulator binary itself which accelerators it can use.
/// Output contains lines like:
///   "WHPX: Windows Hypervisor Platform accelerator is supported."
///   "AEHD: Android Emulator hypervisor driver is installed."
pub fn accel_check(emulator_exe: &Path) -> (Accel, String) {
    let mut c = std::process::Command::new(emulator_exe);
    c.arg("-accel-check");
    let (out_text, ok) = match proc::run(&mut c, Some(Duration::from_secs(30))) {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (s, true)
        }
        Err(e) => (e.to_string(), false),
    };
    if !ok {
        return (Accel::None, out_text);
    }
    let lc = out_text.to_lowercase();
    // Output phrasing varies by emulator version:
    //   "WHPX(10.0.19045) is installed and usable." / "AEHD ... is installed."
    // Negatives contain "not installed"/"not supported"/"cannot".
    let neg = |s: &str| {
        s.contains("not installed") || s.contains("not supported")
            || s.contains("cannot") || s.contains("error") || s.contains("failed")
    };
    let has = |name: &str| {
        lc.lines()
            .filter(|l| l.contains(name))
            .any(|l| {
                !neg(l)
                    && (l.contains("usable") || l.contains("supported")
                        || l.contains("installed") || l.contains("works"))
            })
    };
    let accel = if has("aehd") || has("gvm") {
        Accel::Aehd
    } else if has("whpx") {
        Accel::Whpx
    } else if gvm_installed() {
        Accel::Aehd
    } else {
        Accel::None
    };
    (accel, out_text)
}
