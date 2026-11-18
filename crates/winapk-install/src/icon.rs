//! Icon extraction → .ico writer.
//! PNG-in-ICO is legal since Vista, so a launcher PNG embeds verbatim.
//! Non-PNG icons (webp, adaptive-XML) fall back to a generated flat ICO
//! tinted from the package name — deterministic and honest.

use std::path::Path;

use winapk_core::error::Result;

/// Wrap PNG bytes into a single-image ICO container.
pub fn png_to_ico(png: &[u8]) -> Result<Vec<u8>> {
    let (w, h) = png_size(png).unwrap_or((256, 256));
    let mut ico = Vec::with_capacity(png.len() + 22);
    // ICONDIR
    ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    // ICONDIRENTRY
    ico.push(if w >= 256 { 0 } else { w as u8 });
    ico.push(if h >= 256 { 0 } else { h as u8 });
    ico.extend_from_slice(&[0, 0]); // palette, reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // planes
    ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes()); // data offset
    ico.extend_from_slice(png);
    Ok(ico)
}

fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    Some((
        u32::from_be_bytes([png[16], png[17], png[18], png[19]]),
        u32::from_be_bytes([png[20], png[21], png[22], png[23]]),
    ))
}

pub fn is_png(b: &[u8]) -> bool {
    b.len() > 8 && &b[..8] == b"\x89PNG\r\n\x1a\n"
}

fn is_binary_xml(b: &[u8]) -> bool {
    // AXML magic: 0x03 0x00 0x08 0x00
    b.len() > 4 && b[0] == 0x03 && b[1] == 0x00 && b[2] == 0x08 && b[3] == 0x00
}

/// Decode any raster (PNG/WebP) into PNG bytes.
fn raster_to_png(b: &[u8]) -> Option<Vec<u8>> {
    if is_png(b) {
        return Some(b.to_vec());
    }
    let img = image::load_from_memory(b).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Android 8+ adaptive icons: res/*.xml (binary AXML) with
/// <adaptive-icon><background android:drawable="@ref"/><foreground …/>
/// Composite bg+fg onto a 256px canvas — the assets are drawn full-bleed
/// (108dp) so a straight overlay is the correct flatten.
fn adaptive_to_png(apk_path: &Path, xml: &[u8]) -> Option<Vec<u8>> {
    use crate::arsc::{ResTable, ResValue};
    use crate::axml::{self, AttrValue};

    let doc = axml::parse_axml(xml).ok()?;
    // parse_axml hands back a synthetic root — the real element is its
    // single child (<adaptive-icon>).
    let root = if doc.name == "adaptive-icon" {
        &doc
    } else {
        doc.children
            .iter()
            .find(|n| n.name == "adaptive-icon")?
    };
    // resource table for @ref resolution
    let arsc = crate::apk::entry_bytes(apk_path, "resources.arsc").ok()?;
    let table = ResTable::parse(&arsc).ok()?;

    let mut layers: Vec<Vec<u8>> = Vec::new();
    for layer in ["background", "foreground"] {
        for ch in &root.children {
            if ch.name != layer {
                continue;
            }
            let path = ch
                .attr("drawable")
                .and_then(|a| a.ref_id)
                .and_then(|r| table.resolve(r, true, 0))
                .and_then(|v| match v {
                    ResValue::Str(s) => Some(s),
                    _ => None,
                })
                .or_else(|| match ch.attr_value("drawable") {
                    Some(AttrValue::Str(s)) => Some(s.clone()),
                    _ => None,
                });
            let Some(p) = path.filter(|p| p.starts_with("res/")) else {
                continue;
            };
            if let Ok(b) = crate::apk::entry_bytes(apk_path, &p) {
                if let Some(png) = raster_to_png(&b) {
                    layers.push(png);
                }
            }
        }
    }
    if layers.is_empty() {
        return None;
    }
    const SZ: u32 = 256;
    let mut canvas = image::RgbaImage::from_pixel(SZ, SZ, image::Rgba([0, 0, 0, 0]));
    for l in layers {
        let img = image::load_from_memory(&l).ok()?.to_rgba8();
        let img = image::imageops::resize(
            &img,
            SZ,
            SZ,
            image::imageops::FilterType::Triangle,
        );
        image::imageops::overlay(&mut canvas, &img, 0, 0);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(canvas)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// Resolve an icon entry to PNG bytes: PNG verbatim, WebP decoded,
/// adaptive-XML composited. None → caller uses the flat fallback.
fn load_icon_png(apk_path: &Path, entry: &str) -> Option<Vec<u8>> {
    let b = crate::apk::entry_bytes(apk_path, entry).ok()?;
    if is_binary_xml(&b) || entry.ends_with(".xml") {
        adaptive_to_png(apk_path, &b)
    } else {
        raster_to_png(&b)
    }
}

/// Deterministic flat-color ICO (32×32 BMP) derived from the package name,
/// for apps whose icon isn't a raster (adaptive XML/webp).
pub fn fallback_ico(pkg: &str) -> Vec<u8> {
    let mut h: u32 = 5381;
    for b in pkg.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    let (r, g, bl) = (
        (96 + (h & 0x5f)) as u8,
        (96 + ((h >> 8) & 0x5f)) as u8,
        (96 + ((h >> 16) & 0x5f)) as u8,
    );
    const SZ: usize = 32;
    let mut bmp = Vec::with_capacity(40 + SZ * SZ * 4 + SZ * SZ / 8);
    // BITMAPINFOHEADER (BI_RGB, 32bpp, height doubled for XOR+AND)
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(SZ as i32).to_le_bytes());
    bmp.extend_from_slice(&(SZ as i32 * 2).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&[0u8; 16]);
    // XOR pixels bottom-up: two rounded-rect tones
    for y in 0..SZ {
        for x in 0..SZ {
            let inner = (4..SZ - 4).contains(&x) && (4..SZ - 4).contains(&y);
            let (cr, cg, cb) = if inner {
                (r, g, bl)
            } else {
                ((r as u16 * 3 / 4) as u8, (g as u16 * 3 / 4) as u8, (bl as u16 * 3 / 4) as u8)
            };
            bmp.extend_from_slice(&[cb, cg, cr, 255]);
        }
    }
    // AND mask: all opaque (rows padded to 32-bit → SZ*1bpp = 4 bytes/row)
    bmp.extend(std::iter::repeat_n(0u8, SZ * 4));

    let mut ico = Vec::with_capacity(22 + bmp.len());
    ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    ico.extend_from_slice(&[SZ as u8, SZ as u8, 0, 0]);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&(bmp.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    ico.extend_from_slice(&bmp);
    ico
}

/// Build the .ico for a package from its best raster entry (or fallback).
pub fn write_ico(apk_path: &Path, entry: Option<&str>, pkg: &str, dest: &Path) -> Result<()> {
    let bytes = entry
        .and_then(|e| load_icon_png(apk_path, e))
        .map(|png| png_to_ico(&png))
        .transpose()?
        .unwrap_or_else(|| fallback_ico(pkg));
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(dest, bytes)?;
    Ok(())
}
