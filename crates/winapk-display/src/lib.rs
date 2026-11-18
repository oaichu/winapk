//! winapk-display: spawns & supervises the native display host
//! (`winapk-display-host.exe`, C++/Win32+D3D11 — see native/).
//!
//! One host process per app window. The host owns the message loop;
//! this crate is the launcher + lifecycle link back to the engine:
//! when the host window closes, the child exits → caller calls
//! `engine.app_closed(pkg)`.

use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use winapk_core::error::Result;
use winapk_core::paths::Paths;
use winapk_core::{info, proc, warn};

#[derive(Debug, Clone)]
pub struct DisplayRequest {
    /// Emulator LAUNCHER process id — resolved to the real qemu-system
    /// descendant here, because that's the process owning the Qt window.
    pub emulator_pid: u32,
    pub title: String,
    pub ico: Option<PathBuf>,
    /// Freeform app bounds inside the guest screen (l,t,r,b, guest px).
    /// When set, the host crops the embedded display to exactly this app.
    pub crop: Option<(i32, i32, i32, i32)>,
    /// Initial window size (host px).
    pub width: u32,
    pub height: u32,
    /// Substring of the emulator's main Qt window title, e.g.
    /// "WINapk:5554" — disambiguates it from "Extended Controls" and
    /// tool windows owned by the same process.
    pub title_hint: Option<String>,
}

pub struct DisplayHost {
    child: Child,
    pub pkg: String,
}

pub fn host_exe(paths: &Paths) -> Option<PathBuf> {
    let p = paths.display_host_exe();
    if p.exists() {
        Some(p)
    } else {
        // dev workspace layout: <repo>/native/winapk-display-host/bin/
        // exe sits at <repo>/target/<profile>/winapk.exe → 3 levels up = repo root.
        std::env::current_exe().ok().and_then(|e| {
            e.parent()?.parent()?.parent()
                .map(|root| root.join("native/winapk-display-host/bin/winapk-display-host.exe"))
                .filter(|p| p.exists())
        })
    }
}

/// Launch the native host for one app. Returns None if the binary isn't
/// built yet — caller should then leave the emulator window standalone.
pub fn attach(paths: &Paths, req: &DisplayRequest, pkg: &str) -> Result<Option<DisplayHost>> {
    let Some(exe) = host_exe(paths) else {
        warn!("winapk-display-host.exe not found — leaving emulator window standalone");
        return Ok(None);
    };
    // The launcher pid owns no window (emulator.exe re-execs) — target
    // the qemu-system descendant that actually owns the Qt window.
    let emu_pid = proc::descendants_named(req.emulator_pid, "qemu-system")
        .pop()
        .unwrap_or(req.emulator_pid);
    let mut c = std::process::Command::new(exe);
    c.arg("--pid").arg(emu_pid.to_string())
        .arg("--title").arg(&req.title)
        .arg("--size").arg(format!("{}x{}", req.width, req.height));
    if let Some(hint) = &req.title_hint {
        c.arg("--match").arg(hint);
    }
    if let Some(ico) = &req.ico {
        c.arg("--icon").arg(ico);
    }
    if let Some((l, t, r, b)) = req.crop {
        c.arg("--crop").arg(format!("{l},{t},{r},{b}"));
    }
    info!("display host attach pid={emu_pid} title={:?} match={:?}", req.title, req.title_hint);
    let child = proc::spawn_quiet(&mut c)?;
    let _ = pkg;
    Ok(Some(DisplayHost {
        child,
        pkg: pkg.to_string(),
    }))
}

impl DisplayHost {
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    /// Block until the host window closes (i.e. user closed the app window).
    pub fn wait(&mut self, timeout: Option<Duration>) -> bool {
        let start = std::time::Instant::now();
        loop {
            if !self.is_alive() {
                return true;
            }
            if let Some(t) = timeout {
                if start.elapsed() > t {
                    return false;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Check whether a specific emulator window is currently embedded
/// (useful for doctor output). Not implemented: query via tasklist
/// on the host pid instead — kept simple.
pub fn alive(pid: u32) -> bool {
    proc::pid_alive(pid)
}

/// Used by CLI: locate the emulator child pid we spawned (engine keeps it).
pub fn emulator_pid_hint(paths: &Paths) -> Option<u32> {
    let p = paths.root().join("emulator.pid");
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}
