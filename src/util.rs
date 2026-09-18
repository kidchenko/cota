//! Small helpers shared across modules.
//!
//! The cross-platform pieces live at the top; the platform-specific ones are in
//! the `imp` submodule and re-exported, so callers say `util::shell_open`
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

/// Full path to the running executable, quoted for use in a command line.
/// (Windows autostart writes it into the Run key; macOS resolves the path
/// itself for the LaunchAgent, so this is Windows-only.)
#[cfg(windows)]
pub fn exe_path_quoted() -> Option<String> {
    let p = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", p.display()))
}

pub use imp::*;

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use windows::core::PCWSTR;

    /// Null-terminated UTF-16, for the `W` half of the Win32 API.
    /// The caller must keep the returned buffer alive for as long as the
    /// `PCWSTR` derived from it is in use.
    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn pcwstr(buf: &[u16]) -> PCWSTR {
        PCWSTR(buf.as_ptr())
    }

    /// Hand a path (or URL) to the shell. Used for "Open log" and "Open config
    /// folder", both of which are support affordances rather than features.
    pub fn shell_open(target: &str) {
        let verb = wide("open");
        let file = wide(target);
        unsafe {
            let _ = windows::Win32::UI::Shell::ShellExecuteW(
                None,
                pcwstr(&verb),
                pcwstr(&file),
                windows::core::PCWSTR::null(),
                None,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
        }
    }

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
    /// Hand a path (or URL) to `open(1)`, which routes it to whatever the user
    /// has associated with it — Finder for a folder, Console for the log.
    pub fn shell_open(target: &str) {
        let _ = std::process::Command::new("/usr/bin/open").arg(target).spawn();
    }

    /// The point size the Claude mark is rendered at for the status menu.
    ///
    /// AppKit menu item images are sized in points and it scales the bitmap for
    /// the backing store, so a fixed value is right here where Windows needs a
    /// DPI query: 18pt matches the optical weight of the menu text.
    pub fn menu_icon_edge() -> u32 {
        18
    }
}
