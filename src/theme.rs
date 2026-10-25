//! System light/dark preference — specifically, whether the strip the ring
//! lives in (the Windows taskbar, the macOS menu bar, the Linux desktop's
//! panel) is dark, so the ring's track can be tuned to the background it
//! actually sits on.

pub use imp::taskbar_is_dark;

/// Whether the bar the tray icon sits on is dark, for the ring and the menu
/// glyphs. The same answer as the panel's everywhere but GNOME, whose top bar
/// (and the menus it opens) stays dark in light mode too.
pub fn tray_is_dark() -> bool {
    let gnome = cfg!(target_os = "linux")
        && std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("gnome")))
            .unwrap_or(false);
    gnome || taskbar_is_dark()
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use crate::util::{pcwstr, wide};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
        REG_VALUE_TYPE,
    };

    const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

    /// True when the *taskbar* is dark.
    ///
    /// Deliberately `SystemUsesLightTheme` and not `AppsUseLightTheme`: the two
    /// are set independently, and the only pixels we draw live in the
    /// notification area. Reading the app value would mean a light-app/dark-
    /// taskbar user — a common combination — gets a ring track tuned for the
    /// wrong background.
    ///
    /// Defaults to dark when the value is missing: that is the Windows 11
    /// default and the safer guess.
    pub fn taskbar_is_dark() -> bool {
        read_dword(PERSONALIZE, "SystemUsesLightTheme")
            .map(|v| v == 0)
            .unwrap_or(true)
    }

    fn read_dword(subkey: &str, value: &str) -> Option<u32> {
        let sub = wide(subkey);
        let name = wide(value);
        let mut hkey = HKEY::default();

        unsafe {
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                pcwstr(&sub),
                None,
                KEY_QUERY_VALUE,
                &mut hkey,
            ) != ERROR_SUCCESS
            {
                return None;
            }

            let mut data: u32 = 0;
            let mut size: u32 = std::mem::size_of::<u32>() as u32;
            let mut kind = REG_VALUE_TYPE::default();
            let rc = RegQueryValueExW(
                hkey,
                pcwstr(&name),
                None,
                Some(&mut kind),
                Some(&mut data as *mut u32 as *mut u8),
                Some(&mut size),
            );
            let _ = RegCloseKey(hkey);

            (rc == ERROR_SUCCESS).then_some(data)
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    use objc2_foundation::{NSString, NSUserDefaults};

    /// True when the menu bar is dark.
    ///
    /// `AppleInterfaceStyle` in the global defaults is the string `"Dark"` in
    /// dark mode and absent entirely in light mode — the same signal the menu
    /// bar follows. Absent therefore means light, which is why this defaults to
    /// `false` where the Windows side defaults to `true`.
    pub fn taskbar_is_dark() -> bool {
        let key = NSString::from_str("AppleInterfaceStyle");
        {
            let defaults = NSUserDefaults::standardUserDefaults();
            defaults
                .stringForKey(&key)
                .map(|s| s.to_string().eq_ignore_ascii_case("dark"))
                .unwrap_or(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use gtk::gio::{self, prelude::*};
    use gtk::glib::Variant;

    /// True when the desktop asks apps for dark.
    ///
    /// Asked of the desktop portal, the one place GNOME, KDE and the others
    /// all answer: `color-scheme` is 1 for dark, 2 for light and 0 for no
    /// preference, which is light.
    pub fn taskbar_is_dark() -> bool {
        portal("color-scheme").and_then(|v| v.get::<u32>()) == Some(1)
    }

    /// One `org.freedesktop.appearance` setting, unwrapped from the variant
    /// the portal wraps it in. `None` with no session bus or no portal, as in
    /// a bare window manager; the caller falls back to light.
    fn portal(key: &str) -> Option<Variant> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
        let reply = bus
            .call_sync(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
                "ReadOne",
                Some(&("org.freedesktop.appearance", key).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                200,
                gio::Cancellable::NONE,
            )
            .ok()?;
        let mut v = reply.child_value(0);
        // ReadOne answers v; the older Read answers v inside v.
        while let Some(inner) = v.as_variant() {
            v = inner;
        }
        Some(v)
    }
}
