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
/// a light or dark menu; Windows and Linux take a bitmap as given, so there it
/// is drawn in the menu's own text colour, light or dark to match the menu.
pub fn glyph(name: &str) -> Option<Icon> {
    let (mut px, edge) = decode(GLYPHS.iter().find(|(k, _)| *k == name)?.1)?;
    let ink = menu_is_dark().map(|dark| {
        if dark {
            [0xe6, 0xe6, 0xe6]
        } else {
            [0x3a, 0x3a, 0x3c]
        }
    });
    if let Some(ink) = ink {
        for p in px.chunks_mut(4) {
            p[..3].copy_from_slice(&ink);
        }
    }
    Icon::from_rgba(px, edge, edge).ok()
}

/// Whether the menu the glyphs sit in is dark; `None` on macOS, where AppKit
/// tints them itself.
///
/// On Windows that is the *apps* theme, not the taskbar's: tao opts the
/// process into dark mode, and Windows then draws its menus to match
/// `AppsUseLightTheme`. A light-app/dark-taskbar user gets a light menu.
/// Missing means light, the Windows default for apps.
#[cfg(windows)]
fn menu_is_dark() -> Option<bool> {
    use traykit::util::{pcwstr, wide};
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            pcwstr(&key),
            pcwstr(&value),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    Some(rc.is_ok() && data == 0)
}

/// Linux menus follow the desktop, so the ink follows it too.
#[cfg(target_os = "linux")]
fn menu_is_dark() -> Option<bool> {
    Some(traykit::theme::tray_is_dark())
}

#[cfg(target_os = "macos")]
fn menu_is_dark() -> Option<bool> {
    None
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
