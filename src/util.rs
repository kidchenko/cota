//! Small helpers shared across modules. The ones every tray app needs
//! (`wide`, `shell_open`) are in `traykit::util`; these are Cota's own.
//!
//! The cross-platform pieces live at the top; the platform-specific ones are in
//! the `imp` submodule and re-exported, so callers say `util::menu_icon_edge`
//! regardless of which one they get.

/// Seconds since the Unix epoch. Saturates at 0 rather than panicking if the
/// clock is set before 1970, which is a thing that happens on dead CMOS
/// batteries and is not worth taking the process down for.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub use imp::*;

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    /// The edge Windows will draw a menu item's bitmap at.
    ///
    /// `SM_CXMENUCHECK` is the checkmark column, which is exactly the box an
    /// `hbmpItem` is painted into — and it scales with the system DPI, so asking
    /// for it beats hard-coding 16 and having the mark come out fuzzy on every
    /// machine that is not at 100%.
    pub fn menu_icon_edge() -> u32 {
        let px = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CXMENUCHECK,
            )
        };
        // A zero or absurd return means the metric was unavailable; 16 is the
        // value at 100% DPI and a perfectly serviceable guess.
        if (8..=64).contains(&px) {
            px as u32
        } else {
            16
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    /// The point size the Claude mark is rendered at for the status menu.
    ///
    /// AppKit menu item images are sized in points and it scales the bitmap for
    /// the backing store, so a fixed value is right here where Windows needs a
    /// DPI query: 18pt matches the optical weight of the menu text.
    pub fn menu_icon_edge() -> u32 {
        18
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    /// GTK menus draw an item's image at the 16px menu icon size, doubled on a
    /// HiDPI desktop by GTK itself; 16 is the size to hand it.
    pub fn menu_icon_edge() -> u32 {
        16
    }
}
