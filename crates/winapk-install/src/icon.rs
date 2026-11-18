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
            let inner = x >= 4 && x < SZ - 4 && y >= 4 && y < SZ - 4;
            let (cr, cg, cb) = if inner {
                (r, g, bl)
            } else {
                ((r as u16 * 3 / 4) as u8, (g as u16 * 3 / 4) as u8, (bl as u16 * 3 / 4) as u8)
            };
            bmp.extend_from_slice(&[cb, cg, cr, 255]);
        }
    }
    // AND mask: all opaque (rows padded to 32-bit → SZ*1bpp = 4 bytes/row)
    bmp.extend(std::iter::repeat(0u8).take(SZ * 4));

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
        .and_then(|e| crate::apk::entry_bytes(apk_path, e).ok())
        .filter(|b| is_png(b))
        .map(|png| png_to_ico(&png))
        .transpose()?
        .unwrap_or_else(|| fallback_ico(pkg));
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(dest, bytes)?;
    Ok(())
}
