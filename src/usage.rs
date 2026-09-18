//! The usage endpoint: fetching it, and making sense of what comes back.
//!
//! `GET https://api.anthropic.com/api/oauth/usage` is what Claude Code's own
//! `/usage` command reads. It is **undocumented**, and the response is visibly
//! a live experiment surface — alongside the real fields it returns keys named
//! `nimbus_quill`, `iguana_necktie`, `cedar_ember` and friends, all null.
//!
//! So the parsing here is written to survive the schema moving:
//!
//! * every field is optional, and unknown fields are ignored rather than
//!   rejected — a new key appearing must never break the app;
//! * `limits[]` is the primary source, with the older `five_hour` /
//!   `seven_day` objects as a fallback if it ever disappears;
//! * the codename keys are never read, by anything, on purpose;
//! * `severity` is trusted but not relied on — see [`Severity::effective`].
//!
//! When it does break, `COTA_LOG=debug` puts the whole response body in the
//! log, which is the one thing you need to fix it.

use crate::config::Config;
use crate::creds::{self, CredsError};
use crate::log::{ldebug, lwarn};
use crate::timefmt;
use serde::Deserialize;
use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// What the rest of the app sees
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    Normal,
    Warning,
    Critical,
}

impl Severity {
    fn from_label(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "normal" | "ok" | "low" => Some(Self::Normal),
            "warning" | "warn" | "approaching" | "medium" => Some(Self::Warning),
            "critical" | "exceeded" | "blocked" | "high" => Some(Self::Critical),
            _ => None,
        }
    }

    fn from_percent(p: f64) -> Self {
        if p >= 90.0 {
            Self::Critical
        } else if p >= 75.0 {
            Self::Warning
        } else {
            Self::Normal
        }
    }

    /// The worse of what the server said and what the number implies.
    ///
    /// Trusting `severity` alone would leave the icon green if the server ever
    /// stops grading, or starts grading on a scale we did not expect. Trusting
    /// the percentage alone would throw away the only signal that knows about
    /// limits we cannot see. Taking the max means a new grading scheme can
    /// only ever make us more cautious, never less.
    fn effective(reported: Option<Self>, percent: f64) -> Self {
        let derived = Self::from_percent(percent);
        match reported {
            Some(r) => r.max(derived),
            None => derived,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Limit {
    /// Human label for the menu: "Session", "Weekly", "Weekly (Fable)".
    pub label: String,
    pub percent: f64,
    pub severity: Severity,
    /// Epoch seconds, or `None` when the endpoint did not say.
    pub resets_at: Option<u64>,
    /// The server's own mark for "this is the constraint currently binding".
    pub is_active: bool,
}

/// The pay-as-you-go pool that some plans can draw on past the cap. Shown
/// because "95% of weekly" means something quite different depending on
/// whether there is extra usage behind it.
#[derive(Clone, Debug)]
pub struct Extra {
    pub enabled: bool,
    pub percent: f64,
    pub used: f64,
    pub limit: f64,
    pub currency: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub limits: Vec<Limit>,
    pub extra: Option<Extra>,
    pub fetched_at: u64,
}

impl Snapshot {
    /// The limit the icon represents: whichever is closest to its ceiling.
    ///
    /// Deliberately *not* the one the server flags `is_active`. That marks the
    /// constraint currently being drawn down, which on a quiet morning is the
    /// weekly bucket even when the five-hour one is nearly full from an hour
    /// ago. The question this app answers is "how close am I to a wall", and
    /// the nearest wall is the one that matters.
    pub fn headline(&self) -> Option<&Limit> {
        self.limits
            .iter()
            .max_by(|a, b| a.percent.total_cmp(&b.percent))
    }

}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum FetchError {
    Creds(CredsError),
    /// The token was rejected. Recoverable: Claude Code refreshes it, and our
    /// next poll re-reads the file.
    Unauthorized,
    RateLimited,
    Http(u16),
    Transport(String),
    /// A 200 we could not turn into a single usable limit. This is the one
    /// that means the schema moved.
    Malformed(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Creds(e) => write!(f, "{e}"),
            Self::Unauthorized => write!(f, "token rejected \u{2014} run claude once"),
            Self::RateLimited => write!(f, "rate limited \u{2014} backing off"),
            Self::Http(c) => write!(f, "endpoint returned HTTP {c}"),
            Self::Transport(e) => write!(f, "network: {e}"),
            Self::Malformed(e) => write!(f, "unexpected response: {e}"),
        }
    }
}

impl FetchError {
    /// Short enough for a 128-character tray tooltip.
    pub fn short(&self) -> &'static str {
        match self {
            Self::Creds(_) => "not signed in",
            Self::Unauthorized => "token rejected",
            Self::RateLimited => "rate limited",
            Self::Http(_) => "endpoint error",
            Self::Transport(_) => "offline",
            Self::Malformed(_) => "unexpected response",
        }
    }
}

// ---------------------------------------------------------------------------
// The wire shapes. Every field optional; unknown fields ignored.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawResponse {
    limits: Option<Vec<RawLimit>>,
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
    extra_usage: Option<RawExtra>,
}

#[derive(Deserialize)]
struct RawLimit {
    kind: Option<String>,
    percent: Option<f64>,
    severity: Option<String>,
    resets_at: Option<String>,
    scope: Option<RawScope>,
    is_active: Option<bool>,
}

#[derive(Deserialize)]
struct RawScope {
    model: Option<RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct RawExtra {
    is_enabled: Option<bool>,
    utilization: Option<f64>,
    used_credits: Option<f64>,
    monthly_limit: Option<f64>,
    currency: Option<String>,
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

pub fn fetch(cfg: &Config) -> Result<Snapshot, FetchError> {
    let creds = creds::load().map_err(FetchError::Creds)?;
    let url = std::env::var("COTA_USAGE_URL").unwrap_or_else(|_| USAGE_URL.into());

    // The provider has to be named explicitly: ureq's default is rustls even
    // when rustls is not compiled in, and the mismatch is a runtime panic
    // rather than a build error.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                // And the platform's roots, not the bundled ones the feature
                // also pulls in — the whole reason for schannel here is to see
                // the certificates the machine has been told to trust.
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into();

    let response = agent
        .get(&url)
        .header("Authorization", &format!("Bearer {}", creds.access_token))
        // Not cosmetic. Without a claude-code agent string the endpoint
        // rate-limits hard enough to make the app useless.
        .header("User-Agent", &cfg.user_agent)
        .header("anthropic-beta", OAUTH_BETA)
        .call();

    let mut response = match response {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(401)) | Err(ureq::Error::StatusCode(403)) => {
            return Err(FetchError::Unauthorized)
        }
        Err(ureq::Error::StatusCode(429)) => return Err(FetchError::RateLimited),
        Err(ureq::Error::StatusCode(c)) => return Err(FetchError::Http(c)),
        Err(e) => return Err(FetchError::Transport(e.to_string())),
    };

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::Transport(e.to_string()))?;

    // The single most useful thing in the log the day this stops working.
    ldebug!("usage response: {body}");

    parse(&body)
}

pub fn parse(body: &str) -> Result<Snapshot, FetchError> {
    let raw: RawResponse =
        serde_json::from_str(body).map_err(|e| FetchError::Malformed(e.to_string()))?;

    let mut limits: Vec<Limit> = raw
        .limits
        .unwrap_or_default()
        .into_iter()
        .filter_map(convert_limit)
        .collect();

    // Fallback for the day `limits[]` goes away or arrives empty. The two
    // named window objects predate it and carry the same two numbers.
    if limits.is_empty() {
        lwarn!("no limits[] in response; falling back to five_hour/seven_day");
        if let Some(l) = convert_window("Session", raw.five_hour) {
            limits.push(l);
        }
        if let Some(l) = convert_window("Weekly", raw.seven_day) {
            limits.push(l);
        }
    }

    if limits.is_empty() {
        return Err(FetchError::Malformed(
            "no limits in response (neither limits[] nor five_hour/seven_day)".into(),
        ));
    }

    // Highest first: the menu reads top-down as most-urgent-first, and the
    // headline is then simply the first row.
    limits.sort_by(|a, b| b.percent.total_cmp(&a.percent));

    Ok(Snapshot {
        limits,
        extra: raw.extra_usage.and_then(convert_extra),
        fetched_at: crate::util::now_unix(),
    })
}

fn convert_limit(r: RawLimit) -> Option<Limit> {
    // A row with no number tells us nothing and would render as an empty
    // menu line; drop it rather than show a blank.
    let percent = r.percent?;
    let kind = r.kind.unwrap_or_else(|| "unknown".into());
    let model = r
        .scope
        .and_then(|s| s.model)
        .and_then(|m| m.display_name)
        .filter(|n| !n.is_empty());

    Some(Limit {
        label: label_for(&kind, model.as_deref()),
        percent: percent.clamp(0.0, 100.0),
        severity: Severity::effective(
            r.severity.as_deref().and_then(Severity::from_label),
            percent,
        ),
        resets_at: r.resets_at.as_deref().and_then(timefmt::parse_rfc3339),
        is_active: r.is_active.unwrap_or(false),
    })
}

fn convert_window(label: &str, w: Option<RawWindow>) -> Option<Limit> {
    let w = w?;
    let percent = w.utilization?;
    Some(Limit {
        label: label.into(),
        percent: percent.clamp(0.0, 100.0),
        severity: Severity::effective(None, percent),
        resets_at: w.resets_at.as_deref().and_then(timefmt::parse_rfc3339),
        is_active: false,
    })
}

fn convert_extra(r: RawExtra) -> Option<Extra> {
    Some(Extra {
        enabled: r.is_enabled.unwrap_or(false),
        percent: r.utilization.unwrap_or(0.0),
        used: r.used_credits.unwrap_or(0.0),
        limit: r.monthly_limit?,
        currency: r.currency.unwrap_or_else(|| "USD".into()),
    })
}

/// Turn a wire `kind` into something worth putting in a menu.
///
/// Known kinds get a hand-written label; anything else gets title-cased so a
/// new bucket shows up legibly instead of as `weekly_something_new`.
fn label_for(kind: &str, model: Option<&str>) -> String {
    let base = match kind {
        "session" | "five_hour" => "Session".to_string(),
        "weekly_all" | "seven_day" => "Weekly".to_string(),
        "weekly_scoped" => "Weekly".to_string(),
        other => titlecase(other),
    };
    match model {
        Some(m) => format!("{base} ({m})"),
        None => base,
    }
}

fn titlecase(s: &str) -> String {
    s.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real response, codename keys and all.
    const SAMPLE: &str = r#"{
      "five_hour": {"utilization": 2.0, "resets_at": "2026-09-17T08:00:00.956434+00:00"},
      "seven_day": {"utilization": 19.0, "resets_at": "2026-09-20T00:00:00.956459+00:00"},
      "seven_day_opus": null,
      "nimbus_quill": {"utilization": 0.0, "resets_at": null},
      "iguana_necktie": null,
      "extra_usage": {"is_enabled": false, "monthly_limit": 5000, "used_credits": 876.0,
                      "utilization": 17.52, "currency": "USD"},
      "limits": [
        {"kind": "session", "group": "session", "percent": 2, "severity": "normal",
         "resets_at": "2026-09-17T08:00:00.241332+00:00", "scope": null, "is_active": false},
        {"kind": "weekly_all", "group": "weekly", "percent": 19, "severity": "normal",
         "resets_at": "2026-09-20T00:00:00.241353+00:00", "scope": null, "is_active": true},
        {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal",
         "resets_at": "2026-09-20T00:00:00+00:00",
         "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
         "is_active": false}
      ]
    }"#;

    #[test]
    fn reads_a_real_response() {
        let s = parse(SAMPLE).expect("should parse");
        assert_eq!(s.limits.len(), 3);
        // Sorted highest first, so the weekly bucket leads.
        assert_eq!(s.limits[0].label, "Weekly");
        assert_eq!(s.limits[0].percent, 19.0);
        assert!(s.limits[0].is_active);
        // The scoped row carries its model in the label.
        let scoped = s.limits.iter().find(|l| l.label.contains('(')).unwrap();
        assert_eq!(scoped.label, "Weekly (Fable)");
        assert_eq!(s.headline().unwrap().percent, 19.0);
        assert_eq!(s.headline().unwrap().severity, Severity::Normal);
        let extra = s.extra.expect("extra_usage present");
        assert!(!extra.enabled);
        assert_eq!(extra.limit, 5000.0);
    }

    #[test]
    fn unknown_keys_and_nulls_do_not_break_it() {
        // Every codename key in SAMPLE is unread; this asserts they are also
        // harmless, which is the property that matters when new ones appear.
        let s = parse(SAMPLE).unwrap();
        assert!(s.limits.iter().all(|l| !l.label.contains("nimbus")));
    }

    #[test]
    fn falls_back_when_limits_array_is_missing() {
        let body = r#"{"five_hour": {"utilization": 41.0, "resets_at": null},
                       "seven_day": {"utilization": 88.0, "resets_at": null}}"#;
        let s = parse(body).unwrap();
        assert_eq!(s.limits.len(), 2);
        assert_eq!(s.headline().unwrap().label, "Weekly");
        // 88% must read as a warning even though no severity was sent.
        assert_eq!(s.headline().unwrap().severity, Severity::Warning);
    }

    #[test]
    fn a_response_with_nothing_usable_is_an_error() {
        assert!(matches!(
            parse(r#"{"limits": []}"#),
            Err(FetchError::Malformed(_))
        ));
        assert!(matches!(parse("not json"), Err(FetchError::Malformed(_))));
    }

    #[test]
    fn severity_never_softens_what_the_number_says() {
        // The server calling 96% "normal" must not produce a green icon.
        let body = r#"{"limits":[{"kind":"weekly_all","percent":96,"severity":"normal"}]}"#;
        assert_eq!(parse(body).unwrap().headline().unwrap().severity, Severity::Critical);
        // ...and a server that grades harder than the number still wins.
        let body = r#"{"limits":[{"kind":"weekly_all","percent":5,"severity":"critical"}]}"#;
        assert_eq!(parse(body).unwrap().headline().unwrap().severity, Severity::Critical);
    }

    #[test]
    fn unknown_kinds_get_a_legible_label() {
        let body = r#"{"limits":[{"kind":"weekly_brand_new_bucket","percent":3}]}"#;
        let s = parse(body).unwrap();
        assert_eq!(s.limits[0].label, "Weekly Brand New Bucket");
    }
}
