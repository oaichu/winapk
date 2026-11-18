//! winapk-agent — in-guest control agent.
//!
//! Transport:
//!   * Android build: reads/writes /dev/virtio-ports/winapk.0 (the named
//!     virtserialport QEMU exposes; host connects over TCP chardev).
//!   * Host/test build: `--tcp <port>` binds a local TCP listener — same
//!     protocol, lets the whole channel be tested without an emulator.
//!
//! Protocol: JSON lines — one AgentCmd per line, one AgentMsg reply per line.
//! Schema is shared verbatim with winapk-engine::control (keep in sync;
//! the types are small enough that duplicating them is honest).

use std::io::{BufRead, BufReader, Read, Write};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum AgentCmd {
    Ping,
    Start { pkg: String, activity: Option<String>, freeform: bool },
    Stop { pkg: String },
    WatchExit { pkg: String },
    ClipboardSet { text: String },
    ClipboardGet,
    MountShare { tag: String, target: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AgentMsg {
    Pong { agent_version: String },
    Ok { op: String },
    Err { op: String, msg: String },
    AppExit { pkg: String },
    Clipboard { text: String },
}

const VERSION: &str = "0.1.0";

#[cfg(target_os = "android")]
mod plat {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::path::Path;

    pub fn open_channel() -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        for p in ["/dev/virtio-ports/winapk.0", "/dev/virtio_ports/winapk.0"] {
            if Path::new(p).exists() {
                let rd = OpenOptions::new().read(true).open(p).ok()?;
                let wr = OpenOptions::new().write(true).open(p).ok()?;
                return Some((Box::new(rd), Box::new(wr)));
            }
        }
        None
    }

    pub fn shell(cmd: &str) -> (bool, String) {
        let out = Command::new("/system/bin/sh").arg("-c").arg(cmd).output();
        match out {
            Ok(o) => (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
            ),
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn clipboard_set(text: &str) -> (bool, String) {
        // cmd clipboard requires Android 10+; fall back to service call.
        let (ok, out) = shell(&format!(
            "cmd clipboard set-text '{}' 2>/dev/null || service call clipboard 3 s16 '{}' i32 0",
            text.replace('\'', ""), text.replace('\'', "")
        ));
        (ok, out)
    }
}

#[cfg(not(target_os = "android"))]
mod plat {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;

    /// Host-test mode: TCP listener, one connection at a time.
    pub fn open_tcp(port: u16) -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let l = TcpListener::bind(("127.0.0.1", port)).ok()?;
        let (s, _) = l.accept().ok()?;
        let wr = s.try_clone().ok()?;
        Some((Box::new(s), Box::new(wr)))
    }

    pub fn shell(cmd: &str) -> (bool, String) {
        let out = Command::new("cmd").arg("/C").arg(cmd).output();
        match out {
            Ok(o) => (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
            ),
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn clipboard_set(_: &str) -> (bool, String) {
        (false, "host test".into())
    }
}

fn handle(cmd: &AgentCmd) -> AgentMsg {
    match cmd {
        AgentCmd::Ping => AgentMsg::Pong { agent_version: VERSION.into() },
        AgentCmd::Start { pkg, activity, freeform } => {
            let comp = activity
                .clone()
                .map(|a| format!("{pkg}/{a}"))
                .unwrap_or_else(|| pkg.clone());
            let mode = if *freeform { "--windowingMode 5 " } else { "" };
            let (ok, out) = plat::shell(&format!("am start {mode}-n {comp} || monkey -p {pkg} -c android.intent.category.LAUNCHER 1"));
            if ok { AgentMsg::Ok { op: "start".into() } }
            else { AgentMsg::Err { op: "start".into(), msg: out } }
        }
        AgentCmd::Stop { pkg } => {
            let (ok, out) = plat::shell(&format!("am force-stop {pkg}"));
            if ok { AgentMsg::Ok { op: "stop".into() } }
            else { AgentMsg::Err { op: "stop".into(), msg: out } }
        }
        AgentCmd::WatchExit { pkg } => AgentMsg::Ok { op: format!("watch:{pkg}") },
        AgentCmd::ClipboardSet { text } => {
            let (ok, out) = plat::clipboard_set(text);
            if ok { AgentMsg::Ok { op: "clip".into() } }
            else { AgentMsg::Err { op: "clip".into(), msg: out } }
        }
        AgentCmd::ClipboardGet => AgentMsg::Err {
            op: "clip-get".into(), msg: "not implemented".into(),
        },
        AgentCmd::MountShare { tag, target } => {
            let (ok, out) = plat::shell(&format!(
                "mkdir -p {target} && mount -t 9p -o trans=virtio,version=9p2000.L {tag} {target}"
            ));
            if ok { AgentMsg::Ok { op: "mount".into() } }
            else { AgentMsg::Err { op: "mount".into(), msg: out } }
        }
        AgentCmd::Shutdown => {
            plat::shell("reboot -p || svc power shutdown");
            AgentMsg::Ok { op: "shutdown".into() }
        }
    }
}

fn serve<R: Read, W: Write>(r: R, mut w: W) -> std::io::Result<()> {
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let msg = match serde_json::from_str::<AgentCmd>(line.trim()) {
            Ok(c) => handle(&c),
            Err(e) => AgentMsg::Err { op: "parse".into(), msg: e.to_string() },
        };
        let mut s = serde_json::to_string(&msg).unwrap();
        s.push('\n');
        w.write_all(s.as_bytes())?;
        w.flush()?;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(not(target_os = "android"))]
    {
        let port = args
            .iter()
            .position(|a| a == "--tcp")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(45678u16);
        eprintln!("winapk-agent test mode on 127.0.0.1:{port} (waiting for host)");
        loop {
            match plat::open_tcp(port) {
                Some((r, w)) => {
                    let _ = serve(r, w);
                }
                None => std::thread::sleep(std::time::Duration::from_secs(1)),
            }
        }
    }
    #[cfg(target_os = "android")]
    {
        loop {
            match plat::open_channel() {
                Some((r, w)) => {
                    let _ = serve(r, w);
                }
                None => std::thread::sleep(std::time::Duration::from_secs(2)),
            }
        }
    }
}
