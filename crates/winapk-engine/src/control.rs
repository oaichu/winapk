//! Host↔guest control channel.
//!
//! Primary transport: **virtio-serial** — QEMU `-chardev socket` exposes a
//! TCP listener on the host; the guest agent reads/writes the matching
//! /dev/virtio-ports/winapk.0 node. Unlike virtio-vsock it works under WHPX
//! (no /dev/vhost-vsock needed). JSON-lines protocol shared with
//! crates/winapk-agent.
//!
//! Fallback transport: ADB shell — same protocol, no custom image needed.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use winapk_core::adb::Adb;
use winapk_core::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentCmd {
    Ping,
    Start {
        pkg: String,
        activity: Option<String>,
        freeform: bool,
    },
    Stop {
        pkg: String,
    },
    /// Ask agent to report when `pkg`'s process dies.
    WatchExit {
        pkg: String,
    },
    ClipboardSet {
        text: String,
    },
    ClipboardGet,
    MountShare {
        tag: String,
        target: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    Pong { agent_version: String },
    Ok { op: String },
    Err { op: String, msg: String },
    AppExit { pkg: String },
    Clipboard { text: String },
}

pub trait ControlChannel: Send {
    fn call(&mut self, cmd: &AgentCmd, timeout: Duration) -> Result<AgentMsg>;
    fn name(&self) -> &'static str;
}

/// virtio-serial socket channel (needs agent in guest). The reader is
/// persistent across calls: a fresh BufReader per call can buffer reply
/// bytes it never returns, desyncing the request/reply framing for every
/// subsequent command.
pub struct VirtioSerialChannel {
    port: u16,
    stream: Option<TcpStream>,
    reader: Option<BufReader<TcpStream>>,
}

impl VirtioSerialChannel {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            stream: None,
            reader: None,
        }
    }

    fn drop_conn(&mut self) {
        self.stream = None;
        self.reader = None;
    }

    fn ensure_conn(&mut self) -> Result<()> {
        if self.stream.is_none() {
            let s = TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], self.port)),
                Duration::from_secs(3),
            )
            .map_err(|e| Error::Emulator(format!("virtio-serial connect: {e}")))?;
            s.set_read_timeout(Some(Duration::from_secs(10))).ok();
            s.set_write_timeout(Some(Duration::from_secs(10))).ok();
            self.reader = Some(BufReader::new(s.try_clone()?));
            self.stream = Some(s);
        }
        Ok(())
    }
}

impl ControlChannel for VirtioSerialChannel {
    fn name(&self) -> &'static str {
        "virtio-serial"
    }

    fn call(&mut self, cmd: &AgentCmd, _timeout: Duration) -> Result<AgentMsg> {
        self.ensure_conn()?;
        let line = serde_json::to_string(cmd)
            .map_err(|e| Error::Emulator(format!("agent cmd encode: {e}")))?;
        let w = self
            .stream
            .as_mut()
            .unwrap()
            .write_all(line.as_bytes())
            .and_then(|_| self.stream.as_mut().unwrap().write_all(b"\n"));
        if w.is_err() {
            self.drop_conn();
            return Err(Error::Emulator("agent channel write".into()));
        }
        // Read lines until a *reply* arrives — async AppExit events can be
        // interleaved on the same stream and must not be consumed as (or
        // mistaken for) the reply.
        let reader = self.reader.as_mut().unwrap();
        for _ in 0..8 {
            let mut buf = String::new();
            if let Err(e) = reader.read_line(&mut buf) {
                self.drop_conn();
                return Err(Error::Emulator(format!("agent read: {e}")));
            }
            if buf.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<AgentMsg>(buf.trim()) {
                Ok(AgentMsg::AppExit { pkg }) => {
                    winapk_core::info!("agent event: app exited {pkg}");
                    continue;
                }
                Ok(msg) => return Ok(msg),
                Err(e) => {
                    return Err(Error::Emulator(format!(
                        "agent msg parse: {e} ({buf})"
                    )))
                }
            }
        }
        Err(Error::Emulator("agent reply: event flood".into()))
    }
}

/// ADB-shell channel — always available on stock images.
/// Each call is one `adb shell` round-trip carrying a JSON request on a
/// small on-guest helper when present (`/data/local/tmp/winapk-agent`),
/// else it maps the command onto native shell utilities (am, settings…).
pub struct AdbChannel {
    adb: Adb,
}

impl AdbChannel {
    pub fn new(adb: Adb) -> Self {
        Self { adb }
    }
}

impl ControlChannel for AdbChannel {
    fn name(&self) -> &'static str {
        "adb"
    }

    fn call(&mut self, cmd: &AgentCmd, timeout: Duration) -> Result<AgentMsg> {
        match cmd {
            AgentCmd::Ping => Ok(AgentMsg::Pong {
                agent_version: "adb-fallback".into(),
            }),
            AgentCmd::Start {
                pkg,
                freeform,
                ..
            } => {
                let used = self.adb.start_app(pkg, *freeform)?;
                Ok(AgentMsg::Ok { op: used })
            }
            AgentCmd::Stop { pkg } => {
                self.adb.shell(&format!("am force-stop {pkg}"), timeout)?;
                Ok(AgentMsg::Ok { op: "stop".into() })
            }
            AgentCmd::WatchExit { .. } => Ok(AgentMsg::Ok {
                op: "watch-adb-poll".into(),
            }),
            AgentCmd::ClipboardSet { text } => {
                // POSIX-escape instead of stripping quotes — 'x' → 'x'\'''.
                let esc = text.replace('\'', "'\\''");
                self.adb.shell(
                    &format!("am broadcast -a clipper.set -e text '{esc}'"),
                    timeout,
                )?;
                Ok(AgentMsg::Ok { op: "clip".into() })
            }
            AgentCmd::ClipboardGet => Ok(AgentMsg::Err {
                op: "clip-get".into(),
                msg: "needs agent".into(),
            }),
            AgentCmd::MountShare { .. } => Ok(AgentMsg::Err {
                op: "mount".into(),
                msg: "needs agent+custom image".into(),
            }),
            AgentCmd::Shutdown => {
                self.adb.shell("reboot -p", timeout).ok();
                Ok(AgentMsg::Ok { op: "shutdown".into() })
            }
        }
    }
}
