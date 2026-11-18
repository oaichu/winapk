//! Process helpers. Every child we spawn gets CREATE_NO_WINDOW so no
//! conhost window ever appears — upstream apkstart had to run a 3-minute
//! loop hiding console windows after the fact; we never create them.

use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Run to completion, capture stdout+stderr, no window. The pipes are
/// drained on threads while we poll: a child writing more than the pipe
/// buffer before exiting would block on write and deadlock try_wait
/// (taskkill/powershell chatter is enough to hit this on slow boots).
pub fn run(cmd: &mut Command, timeout: Option<Duration>) -> Result<Output> {
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut p) = stdout {
            let _ = std::io::Read::read_to_end(&mut p, &mut v);
        }
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut p) = stderr {
            let _ = std::io::Read::read_to_end(&mut p, &mut v);
        }
        v
    });
    let status = match timeout {
        None => child.wait()?,
        Some(t) => {
            let deadline = Instant::now() + t;
            loop {
                if let Some(s) = child.try_wait()? {
                    break s;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = t_out.join();
                    let _ = t_err.join();
                    return Err(Error::Timeout(format!(
                        "process did not exit within {t:?}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    Ok(Output {
        status,
        stdout: t_out.join().unwrap_or_default(),
        stderr: t_err.join().unwrap_or_default(),
    })
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

/// Full command line of `pid` — proves a candidate emulator belongs to
/// OUR AVD (tag + console port) before adopt/stop, so we never grab or
/// kill somebody else's emulator sharing the default serial range.
#[cfg(windows)]
pub fn process_cmdline(pid: u32) -> Option<String> {
    let script =
        format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine");
    let mut c = Command::new("powershell");
    c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    run(&mut c, Some(Duration::from_secs(15)))
        .ok()
        .map(|o| stdout_string(&o))
        .filter(|s| !s.is_empty())
}

#[cfg(not(windows))]
pub fn process_cmdline(_pid: u32) -> Option<String> {
    None
}

/// A Windows Job Object configured JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
/// emulator.exe re-execs and detaches its real qemu-system child, so
/// neither Child::kill nor taskkill /T reliably covers it — but every
/// process in the tree belongs to this job, and the kernel kills the
/// whole job when the last handle closes (including on our crash).
#[cfg(windows)]
pub struct JobHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl JobHandle {
    /// Create a job and assign `pid` (and thus its whole future tree).
    /// Returns None when the process already lives in another job —
    /// callers must keep the cmdline sweep as a fallback.
    pub fn wrap(pid: u32) -> Option<Self> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::*;
        use windows::Win32::System::Threading::*;
        unsafe {
            let job = CreateJobObjectW(None, None).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            )
            .is_err()
            {
                let _ = CloseHandle(job);
                return None;
            }
            let proc = OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                false,
                pid,
            )
            .ok()?;
            let assigned = AssignProcessToJobObject(job, proc);
            let _ = CloseHandle(proc);
            if assigned.is_err() {
                let _ = CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// A named system mutex guarding boot/adopt/stop across *processes* —
/// two simultaneous `winapk.exe` invocations (e.g. two .lnk clicks)
/// would otherwise race the AVD files exactly like apkstart's
/// engine.js did. WAIT_ABANDONED still transfers ownership, so a
/// crashed holder can never wedge the lock.
#[cfg(windows)]
pub struct MutexGuard(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
pub fn acquire_named_mutex(name: &str) -> Option<MutexGuard> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    let wide: Vec<u16> = std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let m = CreateMutexW(None, false, windows::core::PCWSTR(wide.as_ptr())).ok()?;
        match WaitForSingleObject(m, u32::MAX) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Some(MutexGuard(m)),
            _ => None,
        }
    }
}

#[cfg(windows)]
impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::ReleaseMutex(self.0);
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
pub fn acquire_named_mutex(_name: &str) -> Option<()> {
    None
}

/// Pids of processes named `<name_prefix>*` among the descendants of
/// `root_pid`. The emulator launcher re-execs: the Qt window and the
/// real QEMU live in a `qemu-system-*.exe` DESCENDANT, not in the pid
/// we spawned — display-host embedding must target that pid.
#[cfg(windows)]
pub fn descendants_named(root_pid: u32, name_prefix: &str) -> Vec<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut e = PROCESSENTRY32W::default();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut rows: Vec<(u32, u32, String)> = Vec::new();
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let name = String::from_utf16_lossy(
                    &e.szExeFile[..e
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(e.szExeFile.len())],
                );
                rows.push((e.th32ProcessID, e.th32ParentProcessID, name));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let prefix = name_prefix.to_ascii_lowercase();
        let mut out = Vec::new();
        for (pid, ppid, name) in &rows {
            if !name.to_ascii_lowercase().starts_with(&prefix) {
                continue;
            }
            let mut cur = *ppid;
            for _ in 0..8 {
                if cur == root_pid {
                    out.push(*pid);
                    break;
                }
                match rows.iter().find(|r| r.0 == cur) {
                    Some(r) => cur = r.1,
                    None => break,
                }
            }
        }
        out
    }
}

#[cfg(not(windows))]
pub fn descendants_named(_root_pid: u32, _name_prefix: &str) -> Vec<u32> {
    Vec::new()
}

/// __vmd keeper body: wrap `pid`'s tree in a kill-on-close job, then
/// block until the watched process exits. Our exit closes the job
/// handle → the kernel reaps any still-running descendants.
#[cfg(windows)]
pub fn job_keepalive(pid: u32) -> i32 {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_ACCESS_RIGHTS,
    };
    const SYNCHRONIZE: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);
    let _job = JobHandle::wrap(pid); // None = sweep stays fallback
    unsafe {
        if let Ok(h) = OpenProcess(SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, u32::MAX);
            let _ = CloseHandle(h);
        }
    }
    0
}
