//! The menu's glyphs, from Lucide (ISC, https://lucide.dev) rather than drawn
//! by hand, the same set Café and Porta use.
//!
//! `assets/icons/make.sh` rasterises them to 32 px PNGs, black on clear, which
//! are compiled in; decoding uses the `png` crate the tray library already
//! brings along. The ring in the menu bar is still drawn by `icon.rs`.

use tray_icon::menu::Icon;

macro_rules! table {
    ($($name:literal)*) => {
        &[$(($name, include_bytes!(concat!("../assets/icons/glyph/", $name, ".png")))),*]
    };
}

const GLYPHS: &[(&str, &[u8])] = table!(
    "file-cog" "gauge" "power" "refresh-cw" "scroll-text" "settings"
);

/// A glyph for a menu row. macOS draws it as a template image, tinted to suit
/// a light or dark menu; Windows tray menus are always light, so there it is
/// drawn in their text colour.
pub fn glyph(name: &str) -> Option<Icon> {
    let (mut px, edge) = decode(GLYPHS.iter().find(|(k, _)| *k == name)?.1)?;
    if cfg!(windows) {
        for p in px.chunks_mut(4) {
            p[..3].copy_from_slice(&[0x3a, 0x3a, 0x3c]);
        }
    }
    Icon::from_rgba(px, edge, edge).ok()
}

/// RGBA and its edge, from a square 8-bit PNG.
fn decode(png_bytes: &[u8]) -> Option<(Vec<u8>, u32)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.width != info.height || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    let px = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf
            .chunks(2)
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        _ => return None,
    };
    Some((px, info.width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_decodes() {
        for (name, png) in GLYPHS {
            let (px, edge) = decode(png).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(px.len(), (edge * edge * 4) as usize, "{name}");
            assert!(px.chunks(4).any(|p| p[3] > 0), "{name} is blank");
            assert!(glyph(name).is_some(), "{name}");
        }
    }
}
