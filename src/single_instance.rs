//! Single-instance enforcement, plus a way for the second launch to be useful.
//!
//! Exiting silently when the app is already in the tray is correct but
//! baffling: launching the app again appears to do nothing at all. So the second
//! launch asks the running instance to refresh and toast the current numbers,
//! then steps aside. That makes the binary a perfectly good "what is my usage
//! right now" command as well as a tray app — run it from a prompt, get a
//! notification, no second process left behind.
//!
//! Windows carries the signal on a named auto-reset event; macOS on a Unix
//! domain socket in the config directory. Both answer the same two questions in
//! one object: "is anyone already running?" (the event/socket exists) and "tell
//! them to report" (signal it / connect to it).

pub use imp::acquire;

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use crate::log::{linfo, lwarn};
    use crate::util::{pcwstr, wide};
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        INFINITE,
    };

    // "Local\" scopes both objects to the logon session, so two users signed in
    // at once each get their own instance rather than fighting over one — which
    // matters here, because they have different tokens and different limits.
    const MUTEX_NAME: &str = r"Local\Cota.SingleInstance";
    const EVENT_NAME: &str = r"Local\Cota.Summon";

    /// Held by the sole running instance. Owns the event other launches signal.
    pub struct Signal(isize);

    /// Returns `None` if another instance is already running, in which case it
    /// has been asked to report and this process should exit.
    pub fn acquire() -> Option<Signal> {
        let mutex_name = wide(MUTEX_NAME);

        let existing = unsafe {
            match CreateMutexW(None, true, pcwstr(&mutex_name)) {
                Ok(handle) => {
                    if GetLastError() == ERROR_ALREADY_EXISTS {
                        let _ = CloseHandle(handle);
                        true
                    } else {
                        // Deliberately never closed: the mutex must outlive
                        // `main`, and Windows releases it on process exit anyway.
                        let _ = handle;
                        false
                    }
                }
                // If the mutex cannot be created at all, assume we are alone
                // rather than refusing to start.
                Err(_) => false,
            }
        };

        if existing {
            linfo!("mutex already held: another instance is running");
            raise();
            return None;
        }

        let event_name = wide(EVENT_NAME);
        match unsafe { CreateEventW(None, false, false, pcwstr(&event_name)) } {
            Ok(handle) => Some(Signal(handle.0 as isize)),
            Err(e) => {
                // Emphatically NOT None. Returning None here would make the app
                // silently refuse to start, which from the outside is
                // indistinguishable from a crash. Losing relaunch-to-report is
                // the lesser failure by a wide margin.
                lwarn!("could not create the summon event ({e}); relaunching will not report");
                Some(Signal(0))
            }
        }
    }

    /// Ask the instance that already owns the event to report.
    fn raise() {
        let event_name = wide(EVENT_NAME);
        unsafe {
            match OpenEventW(EVENT_MODIFY_STATE, false, pcwstr(&event_name)) {
                Ok(handle) => {
                    match SetEvent(handle) {
                        Ok(()) => linfo!("signalled the running instance to report"),
                        Err(e) => lwarn!("SetEvent failed: {e}"),
                    }
                    let _ = CloseHandle(handle);
                }
                Err(e) => lwarn!("could not open the summon event: {e}"),
            }
        }
    }

    impl Signal {
        /// Run `on_signal` every time another launch asks us to report.
        ///
        /// The thread blocks forever. That is fine: the event loop never
        /// returns, so the process tears this thread down on exit.
        pub fn watch<F>(self, on_signal: F)
        where
            F: Fn() + Send + 'static,
        {
            let raw = self.0;
            if raw == 0 {
                lwarn!("no summon event; relaunch-to-report is disabled");
                return;
            }
            let _ = std::thread::Builder::new()
                .name("cota-instance".into())
                .spawn(move || {
                    let handle = HANDLE(raw as *mut std::ffi::c_void);
                    linfo!("listening for relaunch signals");
                    loop {
                        // WAIT_OBJECT_0 is the only value worth acting on; a
                        // failure means the handle is gone and looping would spin
                        // at full tilt.
                        let rc = unsafe { WaitForSingleObject(handle, INFINITE) };
                        if rc.0 != 0 {
                            lwarn!(
                                "relaunch listener stopping (WaitForSingleObject returned {})",
                                rc.0
                            );
                            break;
                        }
                        linfo!("summon received");
                        on_signal();
                    }
                });
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    use crate::log::{linfo, lwarn};
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    fn socket_path() -> Option<PathBuf> {
        // Alongside the config, well within the ~104-char sun_path limit.
        crate::config::Config::dir().map(|d| d.join("cota.sock"))
    }

    /// Held by the sole running instance. Owns the listening socket; `None` means
    /// single-instance could not be set up but the app should run anyway.
    pub struct Signal(Option<UnixListener>);

    /// Returns `None` if another instance is already running, in which case it
    /// has been asked to report and this process should exit.
    pub fn acquire() -> Option<Signal> {
        let Some(path) = socket_path() else {
            // No config dir: assume we are alone rather than refusing to start.
            lwarn!("no socket path; single-instance disabled, relaunch will not report");
            return Some(Signal(None));
        };

        // Someone already listening? Then we are the second launch: poke them and
        // step aside.
        if summon(&path) {
            linfo!("summon socket is live; signalled the running instance to report");
            return None;
        }

        // Nothing answered: a stale socket file, or none at all. Clear it and
        // take the socket ourselves.
        let _ = std::fs::remove_file(&path);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match UnixListener::bind(&path) {
            Ok(listener) => Some(Signal(Some(listener))),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Lost a startup race between our summon() and bind(): the winner
                // is up now, so behave like the second launch after all.
                let _ = summon(&path);
                linfo!("lost the startup race; asked the winner to report, exiting");
                None
            }
            Err(e) => {
                lwarn!("could not bind the summon socket ({e}); relaunch will not report");
                Some(Signal(None))
            }
        }
    }

    /// Connect to an existing instance's socket and nudge it to report. Returns
    /// true if an instance was there to nudge.
    fn summon(path: &Path) -> bool {
        match UnixStream::connect(path) {
            Ok(mut s) => {
                // The connection is the message; a byte just makes the read on
                // the other side return promptly.
                let _ = s.write_all(b"1");
                true
            }
            Err(_) => false,
        }
    }

    impl Signal {
        /// Run `on_signal` every time another launch asks us to report.
        ///
        /// The thread blocks on `accept` forever. That is fine: the event loop
        /// never returns, so the process tears this thread down on exit.
        pub fn watch<F>(self, on_signal: F)
        where
            F: Fn() + Send + 'static,
        {
            let Some(listener) = self.0 else {
                lwarn!("no summon socket; relaunch-to-report is disabled");
                return;
            };
            let _ = std::thread::Builder::new()
                .name("cota-instance".into())
                .spawn(move || {
                    linfo!("listening for relaunch signals");
                    for stream in listener.incoming() {
                        match stream {
                            Ok(mut s) => {
                                // Drain the poke byte; its arrival is the whole
                                // message.
                                let mut buf = [0u8; 8];
                                let _ = s.read(&mut buf);
                                linfo!("summon received");
                                on_signal();
                            }
                            Err(e) => {
                                lwarn!("summon accept failed ({e}); listener stopping");
                                break;
                            }
                        }
                    }
                });
        }
    }
}
