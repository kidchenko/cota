//! Persisted user settings, stored as JSON in `config.json` under
//! `traykit::dirs::config_dir`: `%APPDATA%\Cota` on Windows,
//! `~/Library/Application Support/Cota` on macOS, `~/.config/cota` on Linux.
//! Where the file lives and how it is read and written is traykit's; what is
//! in it, and how a hand edit is sanitised, is here.
//!
//! There is no settings window. Cantos needed one because it has twenty-one
//! actions across four corners; Cota has a poll interval and three thresholds,
//! and shipping a WebView2 dependency to edit five fields would cost more than
//! the whole rest of the app. The tray menu carries the switches that matter
//! and "Open config folder" carries the rest.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use traykit::{lerror, linfo};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Master switch, toggled from the tray. When off we stop polling
    /// entirely rather than polling and hiding the result — the point of
    /// pausing is to stop talking to the network.
    pub enabled: bool,
    /// Seconds between polls. The endpoint rate-limits, and the numbers move
    /// slowly enough that a minute is already generous.
    pub poll_seconds: u64,
    /// Percentages at which to raise a toast, once per limit per window.
    pub thresholds: Vec<u8>,
    pub notify: bool,
    /// Sent as `User-Agent`. This is not cosmetic: the endpoint rate-limits
    /// aggressively without a `claude-code/...` agent, so it is exposed here
    /// to be bumped without a rebuild when that string needs to move.
    pub user_agent: String,
    /// Estimate when the headline limit will hit 100%, from observed slope.
    pub projection: bool,
    /// Attribute the current window's tokens to projects, read from the local
    /// Claude Code transcripts.
    pub attribution: bool,
}

/// Matches a recent Claude Code release. See the `user_agent` field note.
pub const DEFAULT_USER_AGENT: &str = "claude-code/2.1.236";

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_seconds: 60,
            // 50 is "plan the week", 80 is "start being careful", 95 is
            // "finish what you are doing". Below 50 there is nothing to act on
            // and the toast is just noise.
            thresholds: vec![50, 80, 95],
            notify: true,
            user_agent: DEFAULT_USER_AGENT.into(),
            projection: true,
            attribution: true,
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        traykit::config::path()
    }

    /// Last-modified time of the config file, or `None` if it is not there.
    /// Used to notice a hand edit without parsing the file on every tick.
    pub fn modified() -> Option<std::time::SystemTime> {
        traykit::config::modified()
    }

    /// Read and parse, without logging or falling back to defaults.
    ///
    /// `None` covers both "not there" and "could not be parsed". A caller
    /// reacting to a change on disk wants to leave the running config alone in
    /// either case — an editor caught mid-save must not reset anyone's
    /// settings.
    pub fn reload() -> Option<Self> {
        traykit::config::reload::<Config>().map(Config::sanitised)
    }

    /// Never fails: a missing, unreadable, or corrupt file yields defaults.
    ///
    /// Not `traykit::config::load`: a file's values are sanitised, and the log
    /// line records what was loaded, not just where from.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            linfo!("no config at {}; using defaults", path.display());
            return Self::default();
        };
        match traykit::config::parse::<Config>(&text).map(Config::sanitised) {
            Ok(c) => {
                linfo!(
                    "config loaded: enabled={} poll={}s thresholds={:?} notify={} projection={} attribution={}",
                    c.enabled, c.poll_seconds, c.thresholds, c.notify, c.projection, c.attribution
                );
                c
            }
            Err(e) => {
                lerror!(
                    "config at {} is unreadable ({e}); falling back to defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write via a temp file + rename so an interrupted save can never leave a
    /// half-written config behind.
    pub fn save(&self) -> std::io::Result<()> {
        traykit::config::save(self)
    }

    /// Clamp anything a hand-edited config could set to a hostile value.
    /// The floor on `poll_seconds` exists to protect the user from being
    /// rate-limited into a permanently stale icon by their own config.
    pub fn sanitised(mut self) -> Self {
        self.poll_seconds = self.poll_seconds.clamp(20, 900);
        self.thresholds.retain(|t| (1..=100).contains(t));
        self.thresholds.sort_unstable();
        self.thresholds.dedup();
        if self.user_agent.trim().is_empty() {
            self.user_agent = DEFAULT_USER_AGENT.into();
        }
        self
    }
}
