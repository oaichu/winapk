//! Process helpers. Every child we spawn gets CREATE_NO_WINDOW so no
//! conhost window ever appears — upstream apkstart had to run a 3-minute
//! loop hiding console windows after the fact; we never create them.

use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Run to completion, capture stdout+stderr, no window.
pub fn run(cmd: &mut Command, timeout: Option<Duration>) -> Result<Output> {
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    match timeout {
        None => Ok(child.wait_with_output()?),
        Some(t) => {
            let deadline = Instant::now() + t;
            loop {
                if let Some(_status) = child.try_wait()? {
                    return child.wait_with_output().map_err(Into::into);
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::Timeout(format!(
                        "process did not exit within {t:?}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Spawn a long-lived child detached from our console, stdio to null.
pub fn spawn_quiet(cmd: &mut Command) -> Result<Child> {
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(cmd.spawn()?)
}

/// Same but captures stdout AND stderr to a file — the emulator writes
/// fatal GPU errors to stdout, stderr-only capture leaves the log empty.
pub fn spawn_logged(cmd: &mut Command, stderr_log: &std::path::Path) -> Result<Child> {
    if let Some(d) = stderr_log.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stderr_log)?;
    let f2 = f.try_clone()?;
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::from(f2))
        .stderr(Stdio::from(f));
    Ok(cmd.spawn()?)
}

/// Kill pid and its children via taskkill (covers netsimd/conhost leftovers).
pub fn kill_tree(pid: u32) {
    let mut c = Command::new("taskkill");
    c.args(["/F", "/T", "/PID", &pid.to_string()]);
    let _ = run(&mut c, Some(Duration::from_secs(15)));
}

/// Sweep emulator/qemu leftovers by command line. `child.kill()` and
/// even taskkill /T can't reach these: emulator.exe re-execs and
/// DETACHES, so the real qemu-system process escapes the process tree
/// of whatever we spawned. Match on a stable cmdline token (the AVD
/// tag, e.g. "@WINapk") — every process WE launch carries it, other
/// people's emulators don't.
#[cfg(windows)]
pub fn kill_emulator_orphans(avd_tag: &str) {
    let script = format!(
        "Get-CimInstance Win32_Process | Where-Object {{ \
            ($_.Name -match '^(emulator|qemu-system)') -and \
            ($_.CommandLine -like '*{avd_tag}*') \
        }} | ForEach-Object {{ \
            Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue \
        }}"
    );
    let mut c = Command::new("powershell");
    c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    let _ = run(&mut c, Some(Duration::from_secs(20)));
}

#[cfg(not(windows))]
pub fn kill_emulator_orphans(_avd_tag: &str) {}

pub fn pid_alive(pid: u32) -> bool {
    let mut c = Command::new("tasklist");
    c.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    match run(&mut c, Some(Duration::from_secs(10))) {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")),
        Err(_) => false,
    }
}

/// Download via the OS-provided curl.exe (present since Win10 1803) —
/// resumeable, TLS-correct, zero extra deps.
pub fn curl_download(url: &str, dest: &std::path::Path, timeout: Duration) -> Result<()> {
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = dest.with_extension("part");
    let mut c = Command::new("curl.exe");
    c.args([
        "-fSL",
        "--retry", "3",
        "-C", "-", // resume partial
        "--connect-timeout", "20",
        "-o",
    ])
    .arg(&tmp)
    .arg(url);
    let out = run(&mut c, Some(timeout))?;
    if !out.status.success() {
        return Err(Error::Provision(format!(
            "curl {url}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

pub fn stdout_string(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
