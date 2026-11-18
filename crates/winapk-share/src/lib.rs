//! winapk-share: host↔guest folder sync.
//!
//! Default backend: adb push/pull with a size+mtime manifest — pushes only
//! changed files (upstream's sharer.js copied everything, twice the I/O).
//! virtio-9p/virtio-fs mount is config-gated (`engine.virtio_fs`) and needs
//! the agent + kernel support; this backend stays correct either way.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use winapk_core::adb::Adb;
use winapk_core::error::Result;
use winapk_core::paths::Paths;
use winapk_core::{info, warn};

pub const GUEST_DIR: &str = "/sdcard/WINapkShare";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    files: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    size: u64,
    mtime: u64,
}

/// POSIX single-quote for guest `sh -c` strings. `'` itself becomes
/// `'\''` (close, escaped quote, reopen) so a host filename containing
/// a quote can't break out of the quoted context.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Reject guest-provided relative paths that could escape the share dir:
/// absolute/prefix components, `..`, Windows drive/UNC separators.
fn safe_rel(rel: &str) -> Option<std::path::PathBuf> {
    if rel.is_empty() || rel.contains('\\') || rel.contains(':') {
        return None;
    }
    let p = Path::new(rel);
    if !p
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(p.to_path_buf())
}

fn scan(dir: &Path) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            // file_type() does NOT follow symlinks — a symlinked dir can't
            // send the walker outside `dir` or into a cycle.
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
                continue;
            }
            if p.file_name().map(|n| n == ".winapk-sync.json").unwrap_or(false) {
                continue;
            }
            let Ok(rel) = p.strip_prefix(dir) else { continue };
            let Ok(md) = p.metadata() else { continue };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.insert(
                rel.to_string_lossy().replace('\\', "/"),
                Entry { size: md.len(), mtime },
            );
        }
    }
    out
}

fn load_manifest(p: &Path) -> Manifest {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Push changed host files → guest `/sdcard/WINapkShare`.
pub fn push(adb: &Adb, paths: &Paths) -> Result<usize> {
    let dir = paths.share_dir();
    std::fs::create_dir_all(&dir)?;
    let cur = scan(&dir);
    let old = load_manifest(&paths.share_manifest());
    let mut pushed = 0usize;
    adb.shell(&format!("mkdir -p {GUEST_DIR}"), Duration::from_secs(10))?;
    for (rel, ent) in &cur {
        let stale = old
            .files
            .get(rel)
            .map(|o| o.size != ent.size || o.mtime != ent.mtime)
            .unwrap_or(true);
        if !stale {
            continue;
        }
        let src = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let dst = format!("{GUEST_DIR}/{rel}");
        if let Some(d) = dst.rfind('/') {
            let _ = adb.shell(&format!("mkdir -p {}", sq(&dst[..d])), Duration::from_secs(10));
        }
        adb.run(
            &["push", &src.to_string_lossy(), &dst],
            Duration::from_secs(120),
        )?;
        pushed += 1;
    }
    std::fs::write(
        paths.share_manifest(),
        serde_json::to_string_pretty(&Manifest { files: cur })?,
    )?;
    info!("share push: {pushed} file(s)");
    Ok(pushed)
}

/// Pull guest → host. Guest file list via `find` + per-file `adb pull`
/// only when size/mtime differs.
pub fn pull(adb: &Adb, paths: &Paths) -> Result<usize> {
    let dir = paths.share_dir();
    std::fs::create_dir_all(&dir)?;
    // toybox `find` has no -printf; list names, then `stat -c %s` per file.
    let (_, out) = adb.try_shell(
        &format!("cd {GUEST_DIR} && find . -type f"),
        Duration::from_secs(20),
    );
    let mut got = 0usize;
    for line in out.lines() {
        let rel = line.trim().trim_start_matches("./");
        let Some(rel_path) = safe_rel(rel) else {
            if !rel.is_empty() {
                warn!("share pull: skipping unsafe guest path {rel:?}");
            }
            continue;
        };
        let dest = dir.join(&rel_path);
        // Defense in depth: the join must stay inside the share dir.
        if !dest.starts_with(&dir) {
            warn!("share pull: rejecting escaping path {rel:?}");
            continue;
        }
        // A symlinked dest on the host would let adb pull write outside
        // the share dir through the link target.
        if dest
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            warn!("share pull: refusing to write through symlink {rel:?}");
            continue;
        }
        let remote = format!("{GUEST_DIR}/{rel}");
        let (_, sz) = adb.try_shell(&format!("stat -c %s {}", sq(&remote)), Duration::from_secs(10));
        let sz: u64 = sz.trim().parse().unwrap_or(0);
        let stale = dest.metadata().map(|m| m.len() != sz).unwrap_or(true);
        if !stale {
            continue;
        }
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d)?;
        }
        match adb.run(&["pull", &remote, &dest.to_string_lossy()], Duration::from_secs(120)) {
            Ok(_) => got += 1,
            Err(e) => warn!("pull {rel}: {e}"),
        }
    }
    info!("share pull: {got} file(s)");
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_rel_rejects_escapes() {
        assert!(safe_rel("../evil").is_none());
        assert!(safe_rel("a/../../evil").is_none());
        assert!(safe_rel("/abs/path").is_none());
        assert!(safe_rel("a\\b").is_none());
        assert!(safe_rel("C:x").is_none());
        assert!(safe_rel("").is_none());
        assert!(safe_rel("..").is_none());
    }

    #[test]
    fn safe_rel_accepts_normal_paths() {
        assert!(safe_rel("file.txt").is_some());
        assert!(safe_rel("sub/dir/file.txt").is_some());
        assert!(safe_rel("a//b").is_some());
        assert!(safe_rel("weird 'name'.txt").is_some());
    }

    #[test]
    fn sq_escapes_quotes() {
        assert_eq!(sq("plain"), "'plain'");
        assert_eq!(sq("it's"), "'it'\\''s'");
    }
}
