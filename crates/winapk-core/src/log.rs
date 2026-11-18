//! Minimal leveled logger — stderr lines + optional file under logs/.

use std::io::Write;
use std::sync::Mutex;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn init(log_file: Option<&std::path::Path>, verbose: bool) {
    VERBOSE.store(verbose, std::sync::atomic::Ordering::Relaxed);
    if let Some(p) = log_file {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            *FILE.lock().unwrap() = Some(f);
        }
    }
}

#[macro_export]
macro_rules! wlog {
    ($lvl:expr, $($t:tt)*) => {{
        $crate::log::emit($lvl, &format!($($t)*));
    }};
}
#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::wlog!("INFO", $($t)*) };
}
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::wlog!("WARN", $($t)*) };
}
#[macro_export]
macro_rules! err {
    ($($t:tt)*) => { $crate::wlog!("ERR ", $($t)*) };
}
#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { $crate::wlog!("DBG ", $($t)*) };
}

pub fn emit(level: &str, msg: &str) {
    let show = level != "DBG "
        || VERBOSE.load(std::sync::atomic::Ordering::Relaxed);
    let line = format!(
        "[{} {}] {}",
        level.trim(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        msg
    );
    if show {
        eprintln!("{line}");
    }
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}
