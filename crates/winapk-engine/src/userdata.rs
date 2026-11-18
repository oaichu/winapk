//! Minimal valid ext4 seed image for the AVD userdata partition.
//! Rust port of apkstart's src/core/userdata.js (MIT) — the reasons it
//! must exist, verbatim from upstream battle damage:
//!
//! * The sysimg's userdata.img is a 1 MB all-zero placeholder; avdmanager
//!   normally formats it via mke2fs, which the emulator does NOT ship —
//!   skip this and /data stays zeros → "Invalid ext4 superblock" → the
//!   guest drops into a recovery loop and adbd never comes online.
//! * Block size MUST be 4096: /data rides a dm-default-key device whose
//!   logical block size is 4096; 1K-block images fail mount() EINVAL.
//! * EXT4_FEATURE_INCOMPAT_ENCRYPT (0x10000) is required or vold's
//!   `cryptfs init_user0` dies → recovery loop again. But the emulator's
//!   bundled cygwin e2fsprogs (1.42.12) refuses encrypt-flagged images —
//!   so we build WITHOUT the flag, grow with resize2fs, then set it.
//!
//! The builder also supports seeding plain files (extra_files), used to
//! drop /misc/adb/adb_keys into /data before first boot: playstore
//! images are user builds (ro.debuggable=0, ro.adb.secure=1) so the
//! "Allow USB debugging" dialog can't be pre-dismissed via cmdline —
//! but an adb_keys that already contains the host pubkey authorizes it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const BLOCK: usize = 4096;
const INODE_SIZE: usize = 128;
const FIRST_INO: usize = 11;

const COMPAT_EXT_ATTR: u32 = 0x8;
const COMPAT_DIR_INDEX: u32 = 0x20;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
const RO_COMPAT_LARGE_FILE: u32 = 0x2;

fn p16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn p32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// A file to materialize inside the seed filesystem. `path` must be a
/// fully-qualified path like "/misc/adb/adb_keys"; parent dirs are
/// created as needed with mode 0755 uid/gid 0.
pub struct SeedFile<'a> {
    pub path: &'a str,
    pub data: &'a [u8],
    pub mode: u16, // e.g. 0o640
    pub uid: u16,
    pub gid: u16,
}

struct DirEnt {
    ino: u32,
    name: Vec<u8>,
    is_dir: bool,
}

struct Node {
    ino: u32,
    mode: u16,
    uid: u16,
    gid: u16,
    children: Vec<DirEnt>, // dirs only
    data: Option<Vec<u8>>, // files only
    data_block: u32,
}

/// Build an empty ext4 (~sizeMB, single block group, 4K blocks) with
/// optional seeded files. `encrypt=false` omits the ENCRYPT bit so the
/// emulator's ancient e2fsprogs will resize it.
pub fn build_seed_ext4(size_mb: u32, encrypt: bool, files: &[SeedFile]) -> Vec<u8> {
    let size_mb = size_mb.clamp(8, 96) as usize;
    let blocks = size_mb * 1024 * 1024 / BLOCK;
    let blocks_per_group = blocks.min(8 * BLOCK);

    let mut inodes_total = (blocks / 4).max(256);
    inodes_total = (inodes_total + 7) / 8 * 8;
    let itable_blocks = (inodes_total * INODE_SIZE + BLOCK - 1) / BLOCK;

    let gd_block = 1usize;
    let bb_block = 2usize;
    let ib_block = 3usize;
    let itable_block = 4usize;
    let mut next_free_block = (itable_block + itable_blocks) as u32;

    // ---- build the inode tree ----
    let mut nodes: Vec<Node> = Vec::new();
    // root = ino 2
    nodes.push(Node {
        ino: 2,
        mode: 0o40755,
        uid: 0,
        gid: 0,
        children: Vec::new(),
        data: None,
        data_block: 0,
    });
    let mut next_ino = FIRST_INO as u32;

    for f in files {
        let parts: Vec<&str> = f.path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let mut parent = 0usize; // index of root node
        for (depth, part) in parts.iter().enumerate() {
            let leaf = depth == parts.len() - 1;
            // find existing child ino
            let found = nodes[parent]
                .children
                .iter()
                .find(|c| c.name == part.as_bytes())
                .map(|c| c.ino);
            if let Some(ino) = found {
                parent = nodes.iter().position(|n| n.ino == ino).unwrap();
                continue;
            }
            let ino = next_ino;
            next_ino += 1;
            let (mode, uid, gid, data) = if leaf {
                (
                    0o100000 | f.mode,
                    f.uid,
                    f.gid,
                    Some(f.data.to_vec()),
                )
            } else {
                (0o40755, 0, 0, None)
            };
            nodes[parent].children.push(DirEnt {
                ino,
                name: part.as_bytes().to_vec(),
                is_dir: !leaf,
            });
            nodes.push(Node {
                ino,
                mode,
                uid,
                gid,
                children: Vec::new(),
                data,
                data_block: 0,
            });
            parent = nodes.len() - 1;
        }
    }

    // ---- assign data blocks: one block per dir + ceil(size/BLOCK) per file ----
    for n in nodes.iter_mut() {
        if n.data.is_some() {
            let need = n.data.as_ref().unwrap().len().div_ceil(BLOCK).max(1) as u32;
            n.data_block = next_free_block;
            next_free_block += need;
        } else {
            n.data_block = next_free_block;
            next_free_block += 1;
        }
    }
    let used_blocks = next_free_block as usize;
    let used_inodes = next_ino as usize - 1; // inos 1..=next_ino-1

    let mut buf = vec![0u8; blocks * BLOCK];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    let sb = 1024usize;

    // ---- superblock ----
    p32(&mut buf, sb + 0x00, inodes_total as u32);
    p32(&mut buf, sb + 0x04, blocks as u32);
    p32(&mut buf, sb + 0x08, (blocks as f64 * 0.05) as u32);
    p32(&mut buf, sb + 0x0C, (blocks - used_blocks) as u32);
    p32(&mut buf, sb + 0x10, (inodes_total - (FIRST_INO - 1) - (used_inodes - FIRST_INO + 1)) as u32);
    p32(&mut buf, sb + 0x14, 0);
    p32(&mut buf, sb + 0x18, 2); // log_block_size → 4096
    p32(&mut buf, sb + 0x1C, 2); // log_cluster_size must equal block size
    p32(&mut buf, sb + 0x20, blocks_per_group as u32);
    p32(&mut buf, sb + 0x24, blocks_per_group as u32);
    p32(&mut buf, sb + 0x28, inodes_total as u32);
    p32(&mut buf, sb + 0x2C, now);
    p32(&mut buf, sb + 0x30, now);
    p16(&mut buf, sb + 0x34, 0);
    p16(&mut buf, sb + 0x36, 0xFFFF);
    p16(&mut buf, sb + 0x38, 0xEF53);
    p16(&mut buf, sb + 0x3A, 1); // state clean
    p16(&mut buf, sb + 0x3C, 1); // errors=continue
    p16(&mut buf, sb + 0x3E, 0);
    p32(&mut buf, sb + 0x40, now);
    p32(&mut buf, sb + 0x44, 0);
    p32(&mut buf, sb + 0x48, 0);
    p32(&mut buf, sb + 0x4C, 1); // rev dynamic
    p16(&mut buf, sb + 0x50, 0);
    p16(&mut buf, sb + 0x52, 0);
    p32(&mut buf, sb + 0x54, FIRST_INO as u32);
    p16(&mut buf, sb + 0x58, INODE_SIZE as u16);
    p16(&mut buf, sb + 0x5A, 0);
    p32(&mut buf, sb + 0x5C, COMPAT_EXT_ATTR | COMPAT_DIR_INDEX);
    p32(
        &mut buf,
        sb + 0x60,
        INCOMPAT_FILETYPE | if encrypt { INCOMPAT_ENCRYPT } else { 0 },
    );
    p32(&mut buf, sb + 0x64, RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE);
    buf[sb + 0x68..sb + 0x78].copy_from_slice(b"winapk-sb-v1-000");
    buf[sb + 0x78..sb + 0x78 + 4].copy_from_slice(b"data");
    buf[sb + 0x88..sb + 0x88 + 5].copy_from_slice(b"/data");
    p32(&mut buf, sb + 0xC8, 0);
    buf[sb + 0xCD] = 0;
    buf[sb + 0xCE] = 0;
    // FBE ciphers: AES-256-XTS(1) contents, AES-256-CTS(2) filenames.
    p32(&mut buf, sb + 0x274, 1);
    p32(&mut buf, sb + 0x278, 2);
    buf[sb + 0xFC] = 2; // half_md4
    p16(&mut buf, sb + 0xFE, 0);

    // ---- group descriptor (block 1) ----
    let dir_count = nodes.iter().filter(|n| n.data.is_none()).count() as u16;
    let gd = gd_block * BLOCK;
    p32(&mut buf, gd + 0x00, bb_block as u32);
    p32(&mut buf, gd + 0x04, ib_block as u32);
    p32(&mut buf, gd + 0x08, itable_block as u32);
    p16(&mut buf, gd + 0x0C, (blocks_per_group - used_blocks) as u16);
    p16(
        &mut buf,
        gd + 0x0E,
        (inodes_total - FIRST_INO + 1 - (used_inodes - FIRST_INO + 1)) as u16,
    );
    p16(&mut buf, gd + 0x10, dir_count);
    p16(&mut buf, gd + 0x12, 0);

    // ---- block bitmap (block 2): used + padding set ----
    let bb = bb_block * BLOCK;
    for blk in 0..used_blocks {
        buf[bb + (blk >> 3)] |= 1 << (blk & 7);
    }
    for bit in blocks..blocks_per_group {
        buf[bb + (bit >> 3)] |= 1 << (bit & 7);
    }

    // ---- inode bitmap (block 3): 1..=used_inodes ----
    let ib = ib_block * BLOCK;
    for i in 0..used_inodes {
        buf[ib + (i >> 3)] |= 1 << (i & 7);
    }

    // ---- write inodes + directory/file data ----
    let write_dir_block = |buf: &mut [u8], block: u32, self_ino: u32, parent_ino: u32, children: &[DirEnt]| {
        let base = block as usize * BLOCK;
        let mut off = base;
        let put = |buf: &mut [u8], ino: u32, name: &[u8], ftype: u8, rec: usize, off: usize| {
            p32(buf, off, ino);
            p16(buf, off + 4, rec as u16);
            buf[off + 6] = name.len() as u8;
            buf[off + 7] = ftype;
            buf[off + 8..off + 8 + name.len()].copy_from_slice(name);
        };
        put(buf, self_ino, b".", 2, 12, off);
        off += 12;
        put(buf, parent_ino, b"..", 2, 12, off);
        off += 12;
        for (i, c) in children.iter().enumerate() {
            let last = i == children.len() - 1;
            let rec = if last {
                BLOCK - (off - base)
            } else {
                (8 + c.name.len() + 3) & !3
            };
            let ftype = if c.is_dir { 2 } else { 1 };
            put(buf, c.ino, &c.name, ftype, rec, off);
            off += rec;
        }
    };

    for n in nodes.iter() {
        let idx = (n.ino - 1) as usize;
        let r = itable_block * BLOCK + idx * INODE_SIZE;
        let links: u16 = if n.data.is_none() {
            2 + n.children.iter().filter(|c| c.is_dir).count() as u16
        } else {
            1
        };
        p16(&mut buf, r + 0x00, n.mode);
        p16(&mut buf, r + 0x02, n.uid);
        let (size, nblk) = match &n.data {
            Some(d) => (d.len() as u32, d.len().div_ceil(BLOCK).max(1) as u32),
            None => (BLOCK as u32, (BLOCK / 512) as u32),
        };
        p32(&mut buf, r + 0x04, size);
        p32(&mut buf, r + 0x08, now);
        p32(&mut buf, r + 0x0C, now);
        p32(&mut buf, r + 0x10, now);
        p32(&mut buf, r + 0x14, 0);
        p16(&mut buf, r + 0x18, n.gid);
        p16(&mut buf, r + 0x1A, links);
        p32(&mut buf, r + 0x1C, nblk);
        p32(&mut buf, r + 0x20, 0);
        match &n.data {
            Some(d) => {
                // direct blocks
                let nbl = d.len().div_ceil(BLOCK).max(1);
                for i in 0..nbl {
                    p32(&mut buf, r + 0x28 + i * 4, n.data_block + i as u32);
                    let start = i * BLOCK;
                    let end = ((i + 1) * BLOCK).min(d.len());
                    let db = (n.data_block as usize + i) * BLOCK;
                    buf[db..db + (end - start)].copy_from_slice(&d[start..end]);
                }
            }
            None => {
                p32(&mut buf, r + 0x28, n.data_block);
                // parent ino: root's parent is itself; others = the ino
                // of whichever dir lists us.
                let parent_ino = if n.ino == 2 {
                    2
                } else {
                    nodes
                        .iter()
                        .find(|m| m.children.iter().any(|c| c.ino == n.ino))
                        .map(|m| m.ino)
                        .unwrap_or(2)
                };
                write_dir_block(&mut buf, n.data_block, n.ino, parent_ino, &n.children);
            }
        }
    }

    buf
}

/// Back-compat wrapper: empty image without seeded files.
pub fn build_empty_ext4(size_mb: u32, encrypt: bool) -> Vec<u8> {
    build_seed_ext4(size_mb, encrypt, &[])
}

/// Does the image carry a valid ext4 superblock magic?
pub fn has_ext4_superblock(file: &Path) -> bool {
    let Ok(mut f) = File::open(file) else {
        return false;
    };
    let mut head = [0u8; 2048];
    if f.read(&mut head).unwrap_or(0) < 2048 {
        return false;
    }
    u16::from_le_bytes([head[1024 + 0x38], head[1024 + 0x39]]) == 0xEF53
}

fn feature_incompat_rw(file: &Path, set: bool) -> std::io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(file)?;
    let at = 1024 + 0x60;
    f.seek(SeekFrom::Start(at as u64))?;
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    let mut v = u32::from_le_bytes(b);
    if set {
        v |= INCOMPAT_ENCRYPT;
    } else {
        v &= !INCOMPAT_ENCRYPT;
    }
    f.seek(SeekFrom::Start(at as u64))?;
    f.write_all(&v.to_le_bytes())
}

pub fn set_encrypt_flag(file: &Path) -> std::io::Result<()> {
    feature_incompat_rw(file, true)
}

pub fn clear_encrypt_flag(file: &Path) -> std::io::Result<()> {
    feature_incompat_rw(file, false)
}

/// Stamp the NTFS sparse attribute on the image so growing it with
/// qemu-img/SetEndOfFile doesn't materialize every zero block — without
/// this a 64 GiB `userdata-qemu.img` needs 64 GiB of real disk.
/// FSCTL_SET_SPARSE needs only write access to the file (no elevation).
/// No-op off Windows / on failure: correctness doesn't depend on it,
/// only disk usage does.
#[cfg(windows)]
pub fn mark_sparse(file: &Path) {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    unsafe {
        let path = HSTRING::from(file.as_os_str());
        let Ok(h) = CreateFileW(
            &path,
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) else {
            return;
        };
        let mut ret = 0u32;
        let _ = DeviceIoControl(h, FSCTL_SET_SPARSE, None, 0, None, 0, Some(&mut ret), None);
        let _ = windows::Win32::Foundation::CloseHandle(h);
    }
}

#[cfg(not(windows))]
pub fn mark_sparse(_file: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a seeded image to $WINAPK_SEED_OUT — lets us materialize a
    /// userdata seed for ad-hoc AVDs without a full engine run.
    #[test]
    fn write_seed_out() {
        let Ok(out) = std::env::var("WINAPK_SEED_OUT") else {
            return;
        };
        let key = std::env::var("USERPROFILE")
            .map(|h| std::fs::read(format!("{h}\\.android\\adbkey.pub")).unwrap_or_default())
            .unwrap_or_default();
        let files = if key.is_empty() {
            vec![]
        } else {
            vec![SeedFile {
                path: "/misc/adb/adb_keys",
                data: Box::leak(key.into_boxed_slice()),
                mode: 0o640,
                uid: 1000,
                gid: 2000,
            }]
        };
        std::fs::write(&out, build_seed_ext4(64, false, &files)).unwrap();
    }
}

/// Filesystem's own capacity in bytes (image file size can lie — the FS
/// may not have grown into it).
pub fn fs_bytes(file: &Path) -> u64 {
    let Ok(mut f) = File::open(file) else {
        return 0;
    };
    let mut sb = [0u8; 2048];
    if f.read(&mut sb).unwrap_or(0) < 2048 {
        return 0;
    }
    if u16::from_le_bytes([sb[1024 + 0x38], sb[1024 + 0x39]]) != 0xEF53 {
        return 0;
    }
    let blocks_lo = u32::from_le_bytes([
        sb[1024 + 4],
        sb[1024 + 5],
        sb[1024 + 6],
        sb[1024 + 7],
    ]) as u64;
    let log_block =
        u32::from_le_bytes([sb[1024 + 0x18], sb[1024 + 0x19], sb[1024 + 0x1A], sb[1024 + 0x1B]])
            & 0xffff;
    let block_size = 1024u64 << log_block;
    blocks_lo * block_size
}
