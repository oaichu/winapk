//! ADB client bound to our own server port (default 5038) so we never
//! fight the user's SDK adb server — the process-level fix for upstream's
//! port-5044 collisions.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::proc;

#[derive(Debug, Clone)]
pub struct Adb {
    exe: PathBuf,
    port: u16,
    /// Device serial, e.g. "emulator-5554".
    serial: Option<String>,
}

impl Adb {
    pub fn new(exe: PathBuf, port: u16) -> Self {
        Self {
            exe,
            port,
            serial: None,
        }
    }

    pub fn for_serial(mut self, serial: impl Into<String>) -> Self {
        self.serial = Some(serial.into());
        self
    }

    fn base(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.env("ADB_SERVER_PORT", self.port.to_string());
        // Pin the vendor key dir: playstore images force ADB auth, so the
        // server and the emulator must present the same adbkey pair —
        // upstream learned this the hard way (repeated auth dialogs).
        if std::env::var_os("ADB_VENDOR_KEYS").is_none() {
            if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
                let kd = Path::new(&home).join(".android");
                if kd.exists() {
                    c.env("ADB_VENDOR_KEYS", &kd);
                }
            }
        }
        c.arg("-P").arg(self.port.to_string());
        if let Some(s) = &self.serial {
            c.arg("-s").arg(s);
        }
        c
    }

    /// Raw adb invocation; returns trimmed stdout on exit-0.
    pub fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut c = self.base();
        c.args(args);
        let out = proc::run(&mut c, Some(timeout))?;
        if !out.status.success() {
            return Err(Error::Adb(format!(
                "adb {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(proc::stdout_string(&out))
    }

    /// Same but tolerates non-zero exit (returns stderr too, for probes).
    pub fn try_run(&self, args: &[&str], timeout: Duration) -> (bool, String) {
        let mut c = self.base();
        c.args(args);
        match proc::run(&mut c, Some(timeout)) {
            Ok(o) => {
                let text = if o.status.success() {
                    proc::stdout_string(&o)
                } else {
                    format!(
                        "{}{}",
                        proc::stdout_string(&o),
                        String::from_utf8_lossy(&o.stderr).trim()
                    )
                };
                (o.status.success(), text)
            }
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn shell(&self, cmd: &str, timeout: Duration) -> Result<String> {
        self.run(&["shell", cmd], timeout)
    }

    pub fn try_shell(&self, cmd: &str, timeout: Duration) -> (bool, String) {
        self.try_run(&["shell", cmd], timeout)
    }

    pub fn start_server(&self) -> Result<()> {
        self.run(&["start-server"], Duration::from_secs(20))?;
        Ok(())
    }

    pub fn devices(&self) -> Result<Vec<String>> {
        let out = self.run(&["devices"], Duration::from_secs(15))?;
        Ok(out
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                match (it.next(), it.next()) {
                    (Some(s), Some("device")) => Some(s.to_string()),
                    _ => None,
                }
            })
            .collect())
    }

    /// All attached serials regardless of state (device/offline/
    /// unauthorized) — `devices()` alone hides booting VMs.
    pub fn serials(&self) -> Vec<String> {
        let (_, out) = self.try_run(&["devices"], Duration::from_secs(15));
        out.lines()
            .skip(1)
            .filter_map(|l| l.split_whitespace().next().map(String::from))
            .collect()
    }

    /// Raw state column for a serial: "device" / "unauthorized" /
    /// "offline" — None when absent. Needed to distinguish a VM that is
    /// booted-but-blocked (auth dialog) from one still starting.
    pub fn device_state(&self, serial: &str) -> Option<String> {
        let (_, out) = self.try_run(&["devices"], Duration::from_secs(10));
        for line in out.lines().skip(1) {
            let mut it = line.split_whitespace();
            if let (Some(s), Some(st)) = (it.next(), it.next()) {
                if s == serial {
                    return Some(st.to_string());
                }
            }
        }
        None
    }

    /// Block until the device appears, then until boot completes.
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        // Phase 1: device visible to adb.
        loop {
            let (ok, _) = self.try_run(&["wait-for-device"], Duration::from_secs(10));
            if ok {
                break;
            }
            if Instant::now() > deadline {
                return Err(Error::Timeout("adb wait-for-device".into()));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        // Phase 2: sys.boot_completed + animation stopped.
        loop {
            let (_, boot) = self.try_shell("getprop sys.boot_completed", Duration::from_secs(8));
            let (_, anim) =
                self.try_shell("getprop init.svc.bootanim", Duration::from_secs(8));
            if boot.trim() == "1" && (anim.contains("stopped") || anim.trim().is_empty()) {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::Timeout("sys.boot_completed".into()));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    pub fn install(&self, apk: &Path, abi: Option<&str>, timeout: Duration) -> Result<()> {
        let mut args: Vec<String> = vec!["install".into(), "-r".into(), "-g".into()];
        if let Some(a) = abi {
            args.push("--abi".into());
            args.push(a.into());
        }
        args.push(apk.to_string_lossy().into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&refs, timeout)?;
        if out.contains("Success") {
            Ok(())
        } else {
            Err(Error::Adb(format!("install failed: {out}")))
        }
    }

    pub fn getprop(&self, name: &str) -> Option<String> {
        let (ok, v) = self.try_shell(&format!("getprop {name}"), Duration::from_secs(8));
        if ok && !v.trim().is_empty() && v.trim() != "null" {
            Some(v.trim().to_string())
        } else {
            None
        }
    }

    /// Resolve a package's launcher component "pkg/.MainActivity".
    pub fn launcher_component(&self, pkg: &str) -> Option<String> {
        let cmd = format!(
            "cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER {pkg}"
        );
        let (ok, out) = self.try_shell(&cmd, Duration::from_secs(15));
        if !ok {
            return None;
        }
        out.lines()
            .filter(|l| l.contains('/'))
            .last()
            .map(|l| l.trim().to_string())
    }

    /// Start an app. Prefer freeform (windowingMode 5) → plain start → monkey.
    pub fn start_app(&self, pkg: &str, freeform: bool) -> Result<String> {
        let comp = self.launcher_component(pkg);
        let mut attempts: Vec<String> = Vec::new();
        if let Some(c) = &comp {
            if freeform {
                attempts.push(format!("am start --windowingMode 5 -n {c}"));
            }
            attempts.push(format!("am start -n {c}"));
        }
        attempts.push(format!(
            "monkey -p {pkg} -c android.intent.category.LAUNCHER 1"
        ));
        let mut last = String::new();
        for a in attempts {
            let (ok, out) = self.try_shell(&a, Duration::from_secs(25));
            if ok
                && !out.contains("Error")
                && !out.contains("does not exist")
                && !out.contains("Exception")
            {
                return Ok(a);
            }
            last = out;
        }
        Err(Error::Adb(format!("start_app {pkg}: {last}")))
    }

    /// Freeform window bounds of the foremost window of `pkg` — used to crop
    /// the embedded display to just the app (per-app window mode).
    /// Parses `dumpsys window` for the app's mFrame/mBounds.
    pub fn freeform_bounds(&self, pkg: &str) -> Option<(i32, i32, i32, i32)> {
        let (_, out) = self.try_shell(
            "dumpsys window windows | grep -E 'mFrame|mBounds|pkg'",
            Duration::from_secs(10),
        );
        // Fallback parser: look for a line mentioning pkg then a rect.
        let mut lines = out.lines().peekable();
        while let Some(l) = lines.next() {
            if l.contains(pkg) {
                for look in [l.to_string()].into_iter().chain(
                    std::iter::from_fn(|| lines.next().map(str::to_string)).take(6),
                ) {
                    if let Some(r) = parse_rect(&look) {
                        return Some(r);
                    }
                }
            }
        }
        None
    }

    pub fn enable_freeform(&self) {
        let _ = self.try_shell(
            "settings put global enable_freeform_support 1; settings put secure freeform_mode_confirmations confirmed",
            Duration::from_secs(10),
        );
    }

    /// AOT-compile a package with the `speed` profile — kills JIT stutter.
    pub fn aot_compile(&self, pkg: &str, timeout: Duration) -> Result<()> {
        self.shell(
            &format!("cmd package compile -m speed -f {pkg}"),
            timeout,
        )?;
        Ok(())
    }

    pub fn is_installed(&self, pkg: &str) -> bool {
        let (ok, out) = self.try_shell(&format!("pm path {pkg}"), Duration::from_secs(10));
        ok && out.contains("package:")
    }
}

fn parse_rect(s: &str) -> Option<(i32, i32, i32, i32)> {
    // Matches "[l,t][r,b]" or "(l,t,r,b)" patterns in dumpsys output.
    let nums: Vec<i32> = s
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .filter(|t| !t.is_empty() && t != &"-")
        .filter_map(|t| t.parse().ok())
        .collect();
    if nums.len() >= 4 {
        let l = nums[nums.len() - 4];
        let t = nums[nums.len() - 3];
        let r = nums[nums.len() - 2];
        let b = nums[nums.len() - 1];
        if r > l && b > t {
            return Some((l, t, r, b));
        }
    }
    None
}
