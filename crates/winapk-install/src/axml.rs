//! Binary AndroidManifest.xml (AXML) parser — Rust port of apkstart's
//! src/core/axml.js (MIT). Produces a DOM-ish tree of the manifest.

use std::collections::HashMap;

use winapk_core::error::{Error, Result};

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_START_NAMESPACE: u16 = 0x0100;
const RES_XML_END_NAMESPACE: u16 = 0x0101;
const RES_XML_START_ELEMENT: u16 = 0x0102;
const RES_XML_END_ELEMENT: u16 = 0x0103;
const RES_XML_RESOURCE_MAP: u16 = 0x0180;

pub const TYPE_REFERENCE: u8 = 0x01;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const TYPE_INT_BOOLEAN: u8 = 0x12;

pub const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";

#[derive(Debug, Clone)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Bool(bool),
    /// @0xPPTTEEEE resource reference — resolve via resources.arsc.
    Ref(u32),
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub ns: String,
    pub name: String,
    pub value: Option<AttrValue>,
    pub ref_id: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<Attr>,
    pub children: Vec<Node>,
}

impl Node {
    /// Find attribute by name in the android namespace.
    pub fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs
            .iter()
            .find(|a| a.name == name && a.ns == ANDROID_NS)
    }
    /// Attribute value (typed, not the raw ref).
    pub fn attr_value(&self, name: &str) -> Option<&AttrValue> {
        self.attr(name).and_then(|a| a.value.as_ref())
    }
    pub fn attr_str(&self, name: &str) -> Option<String> {
        match self.attr_value(name)? {
            AttrValue::Str(s) => Some(s.clone()),
            AttrValue::Int(i) => Some(i.to_string()),
            AttrValue::Bool(b) => Some(b.to_string()),
            AttrValue::Ref(r) => Some(format!("@{r:08x}")),
        }
    }
    pub fn attr_ref(&self, name: &str) -> Option<u32> {
        self.attr(name).and_then(|a| a.ref_id).or_else(|| {
            match self.attr_value(name) {
                Some(AttrValue::Ref(r)) => Some(*r),
                _ => None,
            }
        })
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    /// Depth-first walk.
    pub fn find_all<'a>(&'a self, name: &'a str, out: &mut Vec<&'a Node>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            }
            c.find_all(name, out);
        }
    }
}

// Total readers: out-of-bounds yields 0 instead of panicking — a
// malformed/crafted manifest degrades to "wrong values" (chunk types
// become unknown → skipped, counts become 0) rather than crashing the
// CLI on every truncated APK it sees.
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

const MAX_STRINGS: usize = 1 << 20; // sanity cap on string-pool count
const MAX_STR_LEN: usize = 1 << 20; // per-string char cap
const MAX_DEPTH: usize = 64; // element nesting cap (find_all recurses)

fn read_string_pool(buf: &[u8], start: usize) -> Result<(Vec<String>, usize)> {
    if u16le(buf, start) != RES_STRING_POOL_TYPE {
        return Err(Error::Apk("string-pool chunk type".into()));
    }
    let header_size = u16le(buf, start + 2) as usize;
    let chunk_size = u32le(buf, start + 4) as usize;
    let count = u32le(buf, start + 8) as usize;
    let flags = u32le(buf, start + 16);
    let strings_start = u32le(buf, start + 20) as usize;
    let is_utf8 = flags & 0x100 != 0;

    let count = count.min(MAX_STRINGS);
    let mut offsets = Vec::with_capacity(count.min(1 << 16));
    for i in 0..count {
        offsets.push(u32le(buf, start + header_size + i * 4) as usize);
    }
    let base = start + strings_start;
    let mut strings = Vec::with_capacity(count.min(1 << 16));
    for off in offsets {
        let mut p = base.saturating_add(off);
        if is_utf8 {
            let _len16 = read_len8(buf, &mut p);
            let byte_len = read_len8(buf, &mut p).min(MAX_STR_LEN);
            match buf.get(p..p.saturating_add(byte_len)) {
                Some(s) => strings.push(String::from_utf8_lossy(s).into_owned()),
                None => strings.push(String::new()),
            }
        } else {
            let mut len = u16le(buf, p) as usize;
            p = p.saturating_add(2);
            if len & 0x8000 != 0 {
                len = ((len & 0x7fff) << 16) | u16le(buf, p) as usize;
                p = p.saturating_add(2);
            }
            let len = len.min(MAX_STR_LEN);
            let mut s = String::with_capacity(len.min(1 << 16));
            for i in 0..len {
                let w = u16le(buf, p + i * 2);
                if w == 0 {
                    break;
                }
                s.push(char::from_u32(w as u32).unwrap_or('\u{fffd}'));
            }
            strings.push(s);
        }
    }
    Ok((strings, chunk_size))
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

/// Parse a binary AXML blob into the manifest root node.
pub fn parse_axml(buf: &[u8]) -> Result<Node> {
    if buf.len() < 8 || u16le(buf, 0) != RES_XML_TYPE {
        return Err(Error::Apk("not binary AXML".into()));
    }
    let mut pos = u16le(buf, 2) as usize;
    let total = u32le(buf, 4) as usize;

    let mut pool: Vec<String> = Vec::new();
    let mut prefix_by_uri: HashMap<String, String> = HashMap::new();
    let mut root = Node::default();
    // Stack of open elements by pointer-path: we emulate with indices.
    let mut path: Vec<usize> = Vec::new();

    while pos + 8 <= total && pos + 8 <= buf.len() {
        let typ = u16le(buf, pos);
        let chunk_size = u32le(buf, pos + 4) as usize;
        if chunk_size < 8 {
            break; // 0 = terminator; <8 is corrupt — stop, don't crawl
        }
        match typ {
            RES_STRING_POOL_TYPE => {
                pool = read_string_pool(buf, pos)?.0;
            }
            RES_XML_START_NAMESPACE | RES_XML_END_NAMESPACE => {
                let prefix_idx = u32le(buf, pos + 16) as usize;
                let uri_idx = u32le(buf, pos + 20) as usize;
                if typ == RES_XML_START_NAMESPACE {
                    if let (Some(u), Some(p)) =
                        (pool.get(uri_idx), pool.get(prefix_idx))
                    {
                        prefix_by_uri.insert(u.clone(), p.clone());
                    }
                }
            }
            RES_XML_START_ELEMENT => {
                let name_idx = u32le(buf, pos + 20) as usize;
                let attr_start = u16le(buf, pos + 24) as usize;
                let attr_size = u16le(buf, pos + 26) as usize;
                let attr_count = u16le(buf, pos + 28) as usize;

                let mut node = Node {
                    name: pool.get(name_idx).cloned().unwrap_or_default(),
                    ..Default::default()
                };
                let attr_base = pos + 16 + attr_start;
                for i in 0..attr_count {
                    let a = attr_base + i * attr_size;
                    if a + 20 > buf.len() {
                        break;
                    }
                    let ns_idx = u32le(buf, a);
                    let name_idx = u32le(buf, a + 4) as usize;
                    let raw_idx = u32le(buf, a + 8);
                    let data_type = buf[a + 15];
                    let data = u32le(buf, a + 16);

                    let ns = if ns_idx == 0xffff_ffff {
                        String::new()
                    } else {
                        pool.get(ns_idx as usize).cloned().unwrap_or_default()
                    };
                    let (value, ref_id) = if raw_idx != 0xffff_ffff {
                        (
                            pool.get(raw_idx as usize)
                                .cloned()
                                .map(AttrValue::Str),
                            None,
                        )
                    } else {
                        match data_type {
                            TYPE_STRING => (
                                pool.get(data as usize).cloned().map(AttrValue::Str),
                                None,
                            ),
                            TYPE_REFERENCE => (None, Some(data)),
                            TYPE_INT_BOOLEAN => {
                                (Some(AttrValue::Bool(data != 0)), None)
                            }
                            TYPE_INT_DEC | TYPE_INT_HEX => {
                                (Some(AttrValue::Int(data as i64)), None)
                            }
                            _ => (None, None),
                        }
                    };
                    node.attrs.push(Attr {
                        ns,
                        name: pool.get(name_idx).cloned().unwrap_or_default(),
                        value,
                        ref_id,
                    });
                }
                // append to parent at `path`; depth-capped so recursive
                // visitors (find_all) can't be stack-exhausted by a
                // pathological manifest.
                if path.len() < MAX_DEPTH {
                    append_at(&mut root, &path, node);
                    path.push(child_len_at(&root, &path) - 1);
                }
            }
            RES_XML_END_ELEMENT => {
                path.pop();
            }
            RES_XML_RESOURCE_MAP => {}
            _ => {}
        }
        pos += chunk_size;
    }

    // root.children[0] should be <manifest>
    Ok(root)
}

fn append_at(root: &mut Node, path: &[usize], node: Node) {
    let mut cur = root;
    for &i in path {
        cur = &mut cur.children[i];
    }
    cur.children.push(node);
}

fn child_len_at(root: &Node, path: &[usize]) -> usize {
    let mut cur = root;
    for &i in path {
        cur = &cur.children[i];
    }
    cur.children.len()
}

/// First <manifest> element (helper for callers).
pub fn manifest_root(doc: &Node) -> Option<&Node> {
    doc.children.iter().find(|c| c.name == "manifest")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_and_truncation_never_panic() {
        for len in [0usize, 1, 4, 7, 8, 16, 64, 1024] {
            let buf = vec![0u8; len];
            let _ = parse_axml(&buf); // must return, not panic
        }
    }

    #[test]
    fn hostile_string_pool_count_no_oom() {
        // RES_XML_TYPE header + string-pool claiming u32::MAX strings —
        // a crafted APK must not allocate on the attacker's say-so.
        let mut buf = vec![0u8; 64];
        buf[0..2].copy_from_slice(&RES_XML_TYPE.to_le_bytes());
        buf[4..8].copy_from_slice(&64u32.to_le_bytes());
        buf[8..10].copy_from_slice(&RES_STRING_POOL_TYPE.to_le_bytes());
        buf[12..16].copy_from_slice(&56u32.to_le_bytes());
        buf[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let _ = parse_axml(&buf);
    }
}
