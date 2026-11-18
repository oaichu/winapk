//! QEMU Machine Protocol client — used for guest input injection.
//!
//! Playstore images are user builds (ro.adb.secure=1): the "Allow USB
//! debugging" dialog cannot be pre-dismissed via kernel cmdline, and a
//! seeded /data/misc/adb/adb_keys is not always honored (SELinux label +
//! adbd key-cache race). The deterministic unblock is clicking the
//! dialog through the VM's own input pipeline — the same virtio-input
//! path a real finger takes.
//!
//! We add `-qmp tcp:127.0.0.1:<port>,server=on,wait=off` to the QEMU
//! tail; this client speaks the JSON handshake and sends absolute
//! pointer events (axes are normalized 0..0x7fff).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use winapk_core::error::{Error, Result};

pub struct Qmp {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

impl Qmp {
    pub fn connect(port: u16, timeout: Duration) -> Result<Self> {
        let s = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| Error::Emulator(format!("qmp connect: {e}")))?;
        s.set_read_timeout(Some(timeout)).ok();
        s.set_write_timeout(Some(timeout)).ok();
        let w = s
            .try_clone()
            .map_err(|e| Error::Emulator(format!("qmp clone: {e}")))?;
        let mut me = Self {
            r: BufReader::new(s),
            w,
        };
        let _greeting = me.read_line()?;
        me.cmd("qmp_capabilities", None)?;
        Ok(me)
    }

    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        self.r
            .read_line(&mut line)
            .map_err(|e| Error::Emulator(format!("qmp read: {e}")))?;
        Ok(line)
    }

    fn cmd(&mut self, execute: &str, args: Option<String>) -> Result<String> {
        let req = match &args {
            Some(a) => format!("{{\"execute\":\"{execute}\",\"arguments\":{a}}}"),
            None => format!("{{\"execute\":\"{execute}\"}}"),
        };
        writeln!(self.w, "{req}").map_err(|e| Error::Emulator(format!("qmp write: {e}")))?;
        // Read until we see a return/error object (skip async events).
        for _ in 0..16 {
            let line = self.read_line()?;
            if line.contains("\"return\"") {
                return Ok(line);
            }
            if line.contains("\"error\"") {
                return Err(Error::Emulator(format!("qmp {execute}: {line}")));
            }
        }
        Err(Error::Emulator("qmp: no response".into()))
    }

    /// Absolute pointer move + button state, screen-pixel coords.
    /// QMP abs axes are normalized 0..0x7fff across the framebuffer.
    pub fn tap(&mut self, x_px: u32, y_px: u32, fb_w: u32, fb_h: u32) -> Result<()> {
        let norm = |v: u32, max: u32| (v * 0x7fff / max.max(1)).min(0x7fff);
        let (x, y) = (norm(x_px, fb_w), norm(y_px, fb_h));
        let ev = |down: bool| {
            // Each batch MUST end with sync or QEMU queues the events
            // unprocessed — the classic silent input-send-event failure.
            format!(
                "{{\"events\":[\
                {{\"type\":\"abs\",\"data\":{{\"axis\":\"x\",\"value\":{x}}}}},\
                {{\"type\":\"abs\",\"data\":{{\"axis\":\"y\",\"value\":{y}}}}},\
                {{\"type\":\"btn\",\"data\":{{\"button\":\"left\",\"down\":{down}}}}},\
                {{\"type\":\"sync\"}}]}}"
            )
        };
        self.cmd("input-send-event", Some(ev(true)))?;
        std::thread::sleep(Duration::from_millis(60));
        self.cmd("input-send-event", Some(ev(false)))?;
        Ok(())
    }
}
