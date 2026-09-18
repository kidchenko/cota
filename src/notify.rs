//! Toast notifications.
//!
//! The toast is the only part of this app that interrupts. Everything else is
//! a ring you can ignore, which is the point — so the bar for raising one is
//! "you would want to change what you are doing in the next hour", and the
//! thresholds in `config.json` are the knob for that.

use crate::config::Config;

pub use imp::show;

/// A threshold crossing. Phrased as the fact plus the consequence, because
/// "80%" on its own is a number and "80%, resets in 2d" is a decision.
pub fn threshold(cfg: &Config, limit: &str, threshold: u8, percent: f64, resets_in: &str) {
    if !cfg.notify {
        return;
    }
    show(
        &format!("{limit} at {threshold}%"),
        &format!("{percent:.0}% used, resets {resets_in}."),
        None,
    );
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------
//
// `Toast::POWERSHELL_APP_ID` is a borrowed identity: Windows will not show a
// toast for an application it has never heard of, and registering a real
// AppUserModelID means writing a Start Menu shortcut carrying it. That is the
// installer's job, not the app's, so an unpackaged run borrows PowerShell's
// registration and shows up under its name. Once the installer lands, the
// shortcut is detected and the toasts get the right name and icon.
#[cfg(windows)]
mod imp {
    use crate::log::{ldebug, lwarn};
    use tauri_winrt_notification::{Duration, Toast};

    /// Matches the `AppUserModelID` the installer stamps on the Start Menu
    /// shortcut. The two must stay in step: Windows resolves a toast's name and
    /// icon by looking this string up against registered shortcuts, and an id
    /// with no shortcut behind it shows nothing at all.
    const APP_ID: &str = "kidchenko.Cota";

    /// The shortcut is the registration. Windows has no API to ask "is this
    /// AUMID known"; it simply declines to show the toast, silently, which is a
    /// miserable thing to debug. So the check is for the artefact itself, and a
    /// portable copy with no installer behind it falls back to borrowing
    /// PowerShell's identity — wrong name on the toast, but a toast.
    fn shortcut_installed() -> bool {
        const REL: &str = r"Microsoft\Windows\Start Menu\Programs\Cota\Cota.lnk";
        ["APPDATA", "PROGRAMDATA"].iter().any(|var| {
            std::env::var_os(var)
                .map(|base| std::path::PathBuf::from(base).join(REL).exists())
                .unwrap_or(false)
        })
    }

    fn app_id() -> String {
        if let Ok(explicit) = std::env::var("COTA_APP_ID") {
            return explicit;
        }
        if shortcut_installed() {
            APP_ID.to_string()
        } else {
            Toast::POWERSHELL_APP_ID.to_string()
        }
    }

    /// Best-effort by design. A toast that cannot be shown — Focus Assist, group
    /// policy, a stripped Windows image — must never be the reason the poll loop
    /// stops or the log fills with errors.
    pub fn show(title: &str, line1: &str, line2: Option<&str>) {
        let mut toast = Toast::new(&app_id())
            .title(title)
            .text1(line1)
            .duration(Duration::Short);
        if let Some(l2) = line2 {
            toast = toast.text2(l2);
        }
        match toast.show() {
            Ok(()) => ldebug!("toast shown: {title} / {line1}"),
            Err(e) => lwarn!("could not show toast: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------
//
// `osascript`'s `display notification` is the analogue of borrowing PowerShell's
// identity on Windows: it shows under Script Editor's name rather than Cota's,
// but it needs no bundle registration, no authorization prompt, and no
// UserNotifications plumbing. Best-effort, same contract — a notification that
// cannot be shown must never stall the poll loop.
#[cfg(target_os = "macos")]
mod imp {
    use crate::log::{ldebug, lwarn};

    pub fn show(title: &str, line1: &str, line2: Option<&str>) {
        // Notification anatomy on macOS is title / subtitle / body. We map the
        // Windows title to the title, the first line to the body, and the
        // optional detail to the subtitle.
        let mut script = format!(
            "display notification {} with title {}",
            applescript_string(line1),
            applescript_string(title),
        );
        if let Some(l2) = line2 {
            script.push_str(&format!(" subtitle {}", applescript_string(l2)));
        }

        match std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(&script)
            .spawn()
        {
            Ok(_) => ldebug!("notification shown: {title} / {line1}"),
            Err(e) => lwarn!("could not show notification: {e}"),
        }
    }

    /// A double-quoted AppleScript string literal. Backslash and quote are
    /// escaped; newlines and tabs become their AppleScript escapes so a
    /// multi-line body cannot break out of the `-e` argument.
    fn applescript_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => out.push(c),
            }
        }
        out.push('"');
        out
    }
}
