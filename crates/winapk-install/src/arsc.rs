//! resources.arsc parser (compact) — Rust port of apkstart's arsc.js.
//! Resolves `@0xPPTTEEEE` references to label strings and icon file paths.

use std::collections::HashMap;

use winapk_core::error::{Error, Result};

const RES_TABLE_TYPE: u16 = 0x0002;
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_TABLE_PACKAGE_TYPE: u16 = 0x0200;
const RES_TABLE_TYPE_TYPE: u16 = 0x0201;

const TYPE_REFERENCE: u8 = 0x01;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const TYPE_INT_BOOLEAN: u8 = 0x12;

// Total readers: OOB yields 0 — crafted arsc degrades to skipped
// chunks/empty strings instead of panicking the install path.
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(
        b.get(o..o + 2)
            .and_then(|s| s.try_into().ok())
            .unwrap_or([0; 2]),
    )
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(
        b.get(o..o + 4)
            .and_then(|s| s.try_into().ok())
            .unwrap_or([0; 4]),
    )
}

const MAX_STRINGS: usize = 1 << 20;

fn read_string_pool(buf: &[u8], start: usize) -> Vec<String> {
    let header_size = u16le(buf, start + 2) as usize;
    let count = u32le(buf, start + 8) as usize;
    let flags = u32le(buf, start + 16);
    let strings_start = u32le(buf, start + 20) as usize;
    let is_utf8 = flags & 0x100 != 0;
    let count = count.min(MAX_STRINGS);
    let base = start.saturating_add(strings_start);
    let mut out = Vec::with_capacity(count.min(1 << 16));
    for i in 0..count {
        let mut p = base.saturating_add(u32le(buf, start + header_size + i * 4) as usize);
        if p >= buf.len() {
            out.push(String::new());
            continue;
        }
        if is_utf8 {
            let _ = read_len8(buf, &mut p);
            let bl = read_len8(buf, &mut p).min(1 << 20);
            match buf.get(p..p.saturating_add(bl)) {
                Some(s) => out.push(String::from_utf8_lossy(s).into_owned()),
                None => out.push(String::new()),
            }
        } else {
            let mut len = u16le(buf, p) as usize;
            p = p.saturating_add(2);
            if len & 0x8000 != 0 {
                len = ((len & 0x7fff) << 16) | u16le(buf, p) as usize;
                p = p.saturating_add(2);
            }
            let mut s = String::new();
            for i in 0..len.min(4096) {
                if p + i * 2 + 1 >= buf.len() {
                    break;
                }
                let w = u16le(buf, p + i * 2);
                if w == 0 {
                    break;
                }
                s.push(char::from_u32(w as u32).unwrap_or('\u{fffd}'));
            }
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_never_panics() {
        for len in [0usize, 1, 8, 12, 64, 512] {
            let buf = vec![0u8; len];
            let _ = ResTable::parse(&buf);
        }
        // u32::MAX pool count inside a plausible-looking header
        let mut buf = vec![0u8; 64];
        buf[0..2].copy_from_slice(&RES_TABLE_TYPE.to_le_bytes());
        buf[4..8].copy_from_slice(&64u32.to_le_bytes());
        buf[8..10].copy_from_slice(&RES_STRING_POOL_TYPE.to_le_bytes());
        buf[12..16].copy_from_slice(&56u32.to_le_bytes());
        buf[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let _ = ResTable::parse(&buf);
    }
}

fn read_len8(buf: &[u8], p: &mut usize) -> usize {
    let mut len = buf.get(*p).copied().unwrap_or(0) as usize;
    *p += 1;
    if len & 0x80 != 0 {
        len = ((len & 0x7f) << 8) | buf.get(*p).copied().unwrap_or(0) as usize;
        *p += 1;
    }
    len
}

#[derive(Debug, Clone, Default)]
struct Config {
    density: u16,
    language: String,
    sdk: u16,
    ui_mode: u8,
}

fn parse_config(buf: &[u8], off: usize) -> Config {
    let size = u32le(buf, off) as usize;
    let mut c = Config::default();
    if size >= 16 && off + 12 <= buf.len() {
        c.language = String::from_utf8_lossy(&buf[off + 8..off + 10])
            .trim_matches('\0')
            .trim()
            .to_string();
        c.density = u16le(buf, off + 14);
    }
    if size >= 28 && off + 26 <= buf.len() {
        c.sdk = u16le(buf, off + 24);
    }
    if size >= 30 && off + 29 < buf.len() {
        c.ui_mode = buf[off + 29];
    }
    c
}

/// Prefer `file` (highest density, avoid anydpi vectors) or `string`
/// (default config over localized).
fn score(c: &Config, prefer_file: bool) -> i64 {
    if prefer_file {
        let mut s = c.density as i64;
        if c.density == 0xfffe {
            s -= 500;
        }
        s
    } else {
        let mut s = 0i64;
        s -= if c.density == 0 { 0 } else { 10 };
        if !c.language.is_empty() {
            s -= 40;
        }
        s -= (c.sdk as i64) / 1000;
        if c.ui_mode != 0 {
            s -= 20;
        }
        s
    }
}

#[derive(Debug, Clone, Copy)]
struct ResVal {
    data_type: u8,
    data: u32,
}

struct TypeChunk {
    config: Config,
    entries: HashMap<u32, ResVal>,
}

struct Package {
    id: u32,
    types: HashMap<u8, Vec<TypeChunk>>,
}

pub struct ResTable {
    global_strings: Vec<String>,
    packages: Vec<Package>,
}

#[derive(Debug, Clone)]
pub enum ResValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl ResTable {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 12 || u16le(buf, 0) != RES_TABLE_TYPE {
            return Err(Error::Apk("resources.arsc bad header".into()));
        }
        let header_size = u16le(buf, 2) as usize;
        let total = u32le(buf, 4) as usize;
        let mut t = Self {
            global_strings: Vec::new(),
            packages: Vec::new(),
        };
        let mut pos = header_size;
        while pos + 8 <= total && pos + 8 <= buf.len() {
            let typ = u16le(buf, pos);
            let chunk_size = u32le(buf, pos + 4) as usize;
            if chunk_size < 8 {
                break;
            }
            if typ == RES_STRING_POOL_TYPE && t.global_strings.is_empty() {
                t.global_strings = read_string_pool(buf, pos);
            } else if typ == RES_TABLE_PACKAGE_TYPE {
                t.packages.push(Self::parse_package(buf, pos, chunk_size));
            }
            pos += chunk_size;
        }
        Ok(t)
    }

    fn parse_package(buf: &[u8], start: usize, size: usize) -> Package {
        let id = u32le(buf, start + 8);
        let header_size = u16le(buf, start + 2) as usize;
        let mut types: HashMap<u8, Vec<TypeChunk>> = HashMap::new();
        let mut pos = start + header_size;
        let end = start + size;
        while pos + 8 <= end && pos + 8 <= buf.len() {
            let typ = u16le(buf, pos);
            let chunk_size = u32le(buf, pos + 4) as usize;
            if chunk_size < 8 {
                break;
            }
            if typ == RES_TABLE_TYPE_TYPE {
                if let Some((id, tc)) = Self::parse_type(buf, pos) {
                    types.entry(id).or_default().push(tc);
                }
            }
            pos += chunk_size;
        }
        Package { id, types }
    }

    fn parse_type(buf: &[u8], start: usize) -> Option<(u8, TypeChunk)> {
        let header_size = u16le(buf, start + 2) as usize;
        let id = *buf.get(start + 8)?;
        let flags = *buf.get(start + 9)?;
        let entry_count = u32le(buf, start + 12) as usize;
        let entries_start = u32le(buf, start + 16) as usize;
        let config = parse_config(buf, start + 20);
        let offsets_base = start + header_size;
        let entries_base = start + entries_start;
        let mut entries = HashMap::new();
        for i in 0..entry_count.min(100_000) {
            let (idx, off) = if flags & 0x01 != 0 {
                let b = offsets_base + i * 4;
                if b + 4 > buf.len() {
                    break;
                }
                (
                    u16le(buf, b) as u32,
                    u16le(buf, b + 2) as usize * 4,
                )
            } else if flags & 0x02 != 0 {
                let b = offsets_base + i * 2;
                if b + 2 > buf.len() {
                    break;
                }
                let o = u16le(buf, b);
                if o == 0xffff {
                    continue;
                }
                (i as u32, o as usize * 4)
            } else {
                let b = offsets_base + i * 4;
                if b + 4 > buf.len() {
                    break;
                }
                let o = u32le(buf, b);
                if o == 0xffff_ffff {
                    continue;
                }
                (i as u32, o as usize)
            };
            if let Some(v) = Self::read_value(buf, entries_base + off) {
                entries.insert(idx, v);
            }
        }
        Some((id, TypeChunk { config, entries }))
    }

    fn read_value(buf: &[u8], at: usize) -> Option<ResVal> {
        if at + 8 > buf.len() {
            return None;
        }
        let entry_size = u16le(buf, at) as usize;
        let flags = u16le(buf, at + 2);
        if flags & 0x0001 != 0 {
            return None; // complex/bag resource — not needed for icons/labels
        }
        let v = at + entry_size;
        if v + 8 > buf.len() {
            return None;
        }
        Some(ResVal {
            data_type: buf[v + 3],
            data: u32le(buf, v + 4),
        })
    }

    /// Resolve a `0xPPTTEEEE` reference. `prefer_file` scores configs for
    /// image density; false scores for plain (default-config) strings.
    pub fn resolve(&self, r: u32, prefer_file: bool, depth: u8) -> Option<ResValue> {
        if depth > 6 || r == 0 {
            return None;
        }
        let pkg_id = (r >> 24) as u8;
        let type_id = ((r >> 16) & 0xff) as u8;
        let entry_idx = r & 0xffff;

        let pkg = self
            .packages
            .iter()
            .find(|p| p.id as u8 == pkg_id)
            .or_else(|| self.packages.first())?;
        let chunks = pkg.types.get(&type_id)?;

        let mut best: Option<ResVal> = None;
        let mut best_score = i64::MIN;
        for c in chunks {
            if let Some(v) = c.entries.get(&entry_idx) {
                let s = score(&c.config, prefer_file);
                if s > best_score {
                    best_score = s;
                    best = Some(*v);
                }
            }
        }
        let v = best?;
        match v.data_type {
            TYPE_STRING => self
                .global_strings
                .get(v.data as usize)
                .cloned()
                .map(ResValue::Str),
            TYPE_REFERENCE => self.resolve(v.data, prefer_file, depth + 1),
            TYPE_INT_BOOLEAN => Some(ResValue::Bool(v.data != 0)),
            TYPE_INT_DEC | TYPE_INT_HEX => Some(ResValue::Int(v.data as i64)),
            _ => None,
        }
    }
}
