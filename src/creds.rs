//! Finding the Claude Code OAuth token.
//!
//! On Windows the token is a plain JSON file at `~/.claude/.credentials.json`.
//! There is no DPAPI blob and no Credential Manager entry to unwrap — macOS is
//! the platform that uses a Keychain, and that split is the one genuinely
//! platform-specific thing in this app.
//!
//! We deliberately do **not** cache the token, and deliberately do **not**
//! implement OAuth refresh. The file carries `expiresAt` and a `refreshToken`,
//! so refreshing would be possible — but Claude Code already refreshes it on
//! our behalf, so re-reading the file on every poll gets the same result for
//! none of the code, and none of the risk of two processes racing to spend a
//! single-use refresh token.

use crate::log::lwarn;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OauthBlock>,
}

#[derive(Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    /// Milliseconds since the epoch, not seconds.
    #[serde(rename = "expiresAt")]
    expires_at: Option<u64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

pub struct Credentials {
    pub access_token: String,
    pub expires_at_secs: Option<u64>,
    pub subscription: Option<String>,
}

#[derive(Debug)]
pub enum CredsError {
    /// No `.claude` directory, or no credentials file in it. Almost always
    /// means Claude Code has never been signed in on this machine.
    NotSignedIn,
    /// The file exists but we could not read or understand it.
    Unreadable(String),
}

impl std::fmt::Display for CredsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSignedIn => write!(f, "not signed in \u{2014} run claude once"),
            Self::Unreadable(e) => write!(f, "credentials unreadable: {e}"),
        }
    }
}

/// Where Claude Code keeps its state. `CLAUDE_CONFIG_DIR` wins when set,
/// which is how people relocate it off a roaming profile.
pub fn claude_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    Some(home.join(".claude"))
}

pub fn load() -> Result<Credentials, CredsError> {
    let path = claude_dir()
        .map(|d| d.join(".credentials.json"))
        .ok_or(CredsError::NotSignedIn)?;

    if !path.exists() {
        return Err(CredsError::NotSignedIn);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| CredsError::Unreadable(e.to_string()))?;
    let parsed: CredentialsFile =
        serde_json::from_str(&text).map_err(|e| CredsError::Unreadable(e.to_string()))?;

    let oauth = parsed.oauth.ok_or(CredsError::NotSignedIn)?;
    let access_token = oauth
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or(CredsError::NotSignedIn)?;

    // Milliseconds in the file, seconds everywhere in this app.
    let expires_at_secs = oauth.expires_at.map(|ms| ms / 1000);
    if let Some(exp) = expires_at_secs {
        if exp <= crate::util::now_unix() {
            // Not an error. The server is the authority on whether a token
            // still works, and refusing to try would turn a recoverable 401
            // into a permanent one whenever the clock is off.
            lwarn!("access token looks expired; trying it anyway");
        }
    }

    Ok(Credentials {
        access_token,
        expires_at_secs,
        subscription: oauth.subscription_type,
    })
}
