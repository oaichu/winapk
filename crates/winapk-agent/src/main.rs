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

/// Guest platform — Android AND linux-musl static builds alike (musl
/// has no bionic but the kernel ABI is identical; we only need sh
/// subprocesses, file IO and sockets).
#[cfg(any(target_os = "android", target_os = "linux"))]
mod plat {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::process::Command;

    /// virtserialport chardev exposed by QEMU (-device
    /// virtserialport,name=winapk.0) — zero adb involvement.
    /// Android's ueventd does NOT create /dev/virtio-ports/<name>
    /// symlinks; the node is /dev/vportXpY with the port name in
    /// /sys/class/virtio-ports/*/name (root-only to read).
    pub fn open_channel() -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        for p in ["/dev/virtio-ports/winapk.0", "/dev/virtio_ports/winapk.0"] {
            if Path::new(p).exists() {
                return open_port(p);
            }
        }
        find_named_vport("winapk.0")
    }

    /// virtio-console ports are SINGLE-OPEN devices — a second open of
    /// the same node fails EBUSY, so rd+wr must share one O_RDWR fd.
    fn open_port(path: &str) -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let f = OpenOptions::new().read(true).write(true).open(path).ok()?;
        let wr = f.try_clone().ok()?;
        Some((Box::new(f), Box::new(wr)))
    }

    /// sysfs scan: /sys/class/virtio-ports/vportXpY/name == "winapk.0"
    /// → open /dev/vportXpY. Names need root to read on some images —
    /// without it, fall back to polling every vport for host bytes.
    fn find_named_vport(want: &str) -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let sys = std::fs::read_dir("/sys/class/virtio-ports").ok()?;
        let mut any_named = false;
        for e in sys.flatten() {
            let name = std::fs::read_to_string(e.path().join("name"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if !name.is_empty() {
                any_named = true;
            }
            if name == want {
                let dev = format!("/dev/{}", e.file_name().to_string_lossy());
                if let Some(ch) = open_port(&dev) {
                    return Some(ch);
                }
            }
        }
        if any_named {
            return None; // names readable but ours missing — don't guess
        }
        poll_vports_for_host()
    }

    /// Names unreadable (non-root on userdebug): open every /dev/vport*
    /// nonblocking and poll ~60s — the port that receives the host's
    /// first JSON line is our channel.
    fn poll_vports_for_host() -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        use std::os::unix::io::AsRawFd;
        let mut fds: Vec<(std::fs::File, String)> = Vec::new();
        let Ok(d) = std::fs::read_dir("/dev") else { return None };
        for e in d.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with("vport") {
                continue;
            }
            let p = format!("/dev/{n}");
            if let Ok(f) = OpenOptions::new().read(true).write(true).open(&p) {
                let _ = set_nonblock(&f);
                fds.push((f, p));
            }
        }
        for _ in 0..600 {
            for (f, _) in &fds {
                let mut b = [0u8; 1];
                let n = unsafe {
                    libc::read(f.as_raw_fd(), b.as_mut_ptr() as *mut _, 1)
                };
                if n == 1 {
                    // Restore blocking mode BEFORE dup — O_NONBLOCK is a
                    // file-status flag shared by every dup, and serve()'s
                    // read_line must block, not spin on EAGAIN.
                    let fd = f.as_raw_fd();
                    unsafe {
                        let fl = libc::fcntl(fd, libc::F_GETFL);
                        libc::fcntl(fd, libc::F_SETFL, fl & !libc::O_NONBLOCK);
                    }
                    let (d1, d2) = unsafe { (libc::dup(fd), libc::dup(fd)) };
                    if d1 < 0 || d2 < 0 {
                        continue;
                    }
                    let rd: Box<dyn Read + Send> = Box::new(PreRead::new(
                        b,
                        unsafe { std::fs::File::from_raw_fd(d1) },
                    ));
                    let wr: Box<dyn Write + Send> =
                        Box::new(unsafe { std::fs::File::from_raw_fd(d2) });
                    return Some((rd, wr));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        None
    }

    fn set_nonblock(f: &std::fs::File) -> std::io::Result<()> {
        use std::os::unix::io::AsRawFd;
        let fd = f.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        Ok(())
    }

    use std::os::unix::io::FromRawFd;

    /// Reader that yields a pre-read byte before continuing on the fd.
    struct PreRead {
        head: Option<[u8; 1]>,
        f: std::fs::File,
    }
    impl PreRead {
        fn new(b: [u8; 1], f: std::fs::File) -> Self {
            Self { head: Some(b), f }
        }
    }
    impl Read for PreRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(b) = self.head.take() {
                if !buf.is_empty() {
                    buf[0] = b[0];
                    return Ok(1);
                }
            }
            self.f.read(buf)
        }
    }

    /// Fallback: listen on guest loopback; host reaches it via
    /// `adb forward tcp:PORT tcp:PORT`.
    pub fn open_tcp(port: u16) -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let l = TcpListener::bind(("127.0.0.1", port)).ok()?;
        let (s, _) = l.accept().ok()?;
        let wr = s.try_clone().ok()?;
        Some((Box::new(s), Box::new(wr)))
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

/// Host-test platform — run the agent on Windows to exercise the
/// protocol without an emulator (shell calls hit cmd.exe).
#[cfg(target_os = "windows")]
mod plat {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;

    #[allow(dead_code)]
    pub fn open_channel() -> Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        None
    }

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
        match reader.read_line(&mut line) {
            Ok(0) => {
                // virtio-console reports EOF while NO host is attached
                // to the chardev — the fd stays valid across reconnects,
                // so idle-wait here instead of reopen-looping.
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e),
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

fn tcp_arg(args: &[String]) -> Option<u16> {
    args.iter()
        .position(|a| a == "--tcp")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        let tcp = tcp_arg(&args);
        loop {
            // virtio-serial first (zero adb); --tcp forces guest-listen
            // mode the host reaches through `adb forward`.
            if tcp.is_none() {
                match plat::open_channel() {
                    Some((r, w)) => {
                        let _ = serve(r, w);
                    }
                    None => std::thread::sleep(std::time::Duration::from_secs(2)),
                }
            }
            if let Some(p) = tcp {
                match plat::open_tcp(p) {
                    Some((r, w)) => {
                        let _ = serve(r, w);
                    }
                    None => std::thread::sleep(std::time::Duration::from_secs(1)),
                }
            } else {
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        let port = tcp_arg(&args).unwrap_or(45678);
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
}
