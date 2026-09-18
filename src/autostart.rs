//! Run-at-login, via the per-user Run key.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` is deliberate: it needs
//! no elevation, it is trivially inspectable in Task Manager's Startup tab, and
//! uninstalling leaves one stale value rather than a scheduled task nobody can
//! find. A Scheduled Task would let us start with a delay, but costs admin
//! rights and user trust.
//!
//! This matters more for Cota than it did for Cantos. A hot corner you forgot
//! to start is a feature that does not fire; a usage meter you forgot to start
//! is a gap in the record — the slope in `state.rs` is only as good as the
//! samples, and the app has to be running to take them.

use crate::util::{exe_path_quoted, pcwstr, wide};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Cota";

/// Closes the key on drop, so early returns cannot leak a handle.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}

fn open(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<Key> {
    let sub = wide(RUN_KEY);
    let mut hkey = HKEY::default();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, pcwstr(&sub), None, access, &mut hkey) };
    (rc == ERROR_SUCCESS).then_some(Key(hkey))
}

/// Is the Run value present at all? We deliberately do not compare it against
/// the current executable path: if the user moved the binary we would rather
/// report "on" and let `set(true)` rewrite the path than silently show "off".
pub fn is_enabled() -> bool {
    let Some(key) = open(KEY_QUERY_VALUE) else {
        return false;
    };
    let name = wide(VALUE_NAME);
    let rc = unsafe { RegQueryValueExW(key.0, pcwstr(&name), None, None, None, None) };
    rc == ERROR_SUCCESS
}

pub fn set(enabled: bool) -> Result<(), String> {
    let Some(key) = open(KEY_SET_VALUE) else {
        return Err("could not open the HKCU Run key".into());
    };
    let name = wide(VALUE_NAME);

    if !enabled {
        let rc = unsafe { RegDeleteValueW(key.0, pcwstr(&name)) };
        // Deleting something already absent is the desired end state.
        return if rc == ERROR_SUCCESS || rc.0 == 2 {
            Ok(())
        } else {
            Err(format!("could not remove the autostart entry (error {})", rc.0))
        };
    }

    let Some(cmd) = exe_path_quoted() else {
        return Err("could not resolve the executable path".into());
    };
    let value = wide(&cmd);
    // REG_SZ length is counted in bytes and must include the terminator.
    let bytes = unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
    let rc = unsafe { RegSetValueExW(key.0, pcwstr(&name), None, REG_SZ, Some(bytes)) };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("could not write the autostart entry (error {})", rc.0))
    }
}
