//! Samples of the reported percentage over time, and what they imply.
//!
//! This is the module that answers "when do I hit the wall", and it is worth
//! being explicit about why it works the way it does.
//!
//! The obvious approach — the one `ccusage` takes — is to read the local
//! transcripts, total the tokens, and price them. That answers "what would
//! this have cost on pay-per-use", which is a genuinely useful number and a
//! completely different one. It cannot answer this question, because the
//! subscription buckets are opaque server-side counters: cache reads, model
//! tier and effort all weigh differently, and none of those weights are
//! published. Any local token total is a proxy whose error we cannot bound.
//!
//! So we do not model the bucket at all. We sample the percentage the server
//! reports, and fit a line through it. The slope is exact regardless of how
//! the weighting works, it costs one f64 per poll, and it keeps working on the
//! day the weighting changes.
//!
//! Persisted to `%APPDATA%\Cota\state.json` so a restart does not lose the
//! slope or re-fire a toast that already fired this window.

use crate::config::Config;
use crate::log::linfo;
use crate::usage::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Samples older than this are dropped: a week-long window would otherwise
/// average this morning's burst with last Tuesday's idle and project nothing
/// useful. Three hours is long enough to be stable and short enough to react
/// to "I started a big refactor".
const WINDOW_SECS: u64 = 3 * 3600;

/// Below this span the slope is mostly noise — two polls a minute apart can
/// differ by a whole percent and imply a cap in twenty minutes.
const MIN_SPAN_SECS: u64 = 10 * 60;

/// Hard cap on retained points, so a config with a 20s poll cannot grow the
/// file without bound between resets.
const MAX_SAMPLES: usize = 600;

/// How far `resets_at` must move *forward* before it counts as a new window.
///
/// This is not defensive padding; it is load-bearing. The server computes
/// `resets_at` per request, so its sub-second part drifts between polls —
/// `08:00:00.956` on one, `08:00:01.241` on the next. Truncating the fraction
/// makes the whole-second value flip between two adjacent numbers forever.
/// Comparing those for equality reported a window rollover on roughly every
/// other poll, which fired a "limits reset" toast, wiped the samples the
/// projection needs, and re-armed every threshold. Two minutes is far above
/// the jitter and far below any real window, which is measured in hours.
const SAME_WINDOW_TOLERANCE: u64 = 120;

/// Below this, a reset is not news. The session bucket rolls over every few
/// hours; being told your allowance came back when you had used 9% of it is
/// noise, and noise is what makes people turn notifications off.
const RESET_WORTH_SAYING: f64 = 50.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Sample {
    /// Epoch seconds.
    pub t: u64,
    pub percent: f64,
}

/// What we know about one limit's current window.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Window {
    /// Epoch seconds, as last reported. Compared with a tolerance, never for
    /// equality — see [`SAME_WINDOW_TOLERANCE`].
    pub resets_at: Option<u64>,
    /// Thresholds already toasted for this window.
    pub notified: Vec<u8>,
    /// The last percentage seen, so a rollover can say whether it mattered.
    pub last_percent: f64,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    /// Keyed by limit label. Per limit, because these are independent windows
    /// on independent clocks: the session bucket rolling over says nothing
    /// about the weekly one, and an earlier version that tracked only the
    /// headline conflated the two whenever the headline changed which limit it
    /// pointed at.
    pub windows: BTreeMap<String, Window>,
    /// Which limit the samples describe. The headline can move between limits
    /// as they rise and fall past each other, and splicing one limit's
    /// percentages onto another's would produce a slope describing nothing.
    pub samples_for: String,
    pub samples: Vec<Sample>,
}

/// A limit whose window genuinely rolled over.
#[derive(Clone, Debug, PartialEq)]
pub struct Rollover {
    pub label: String,
    /// Where it stood just before it reset. The whole point of the message is
    /// "you can work again", so it is only worth sending if you could not.
    pub was_at: f64,
}

impl Rollover {
    pub fn worth_saying(&self) -> bool {
        self.was_at >= RESET_WORTH_SAYING
    }
}

/// What the slope says. Four distinct outcomes, because "no estimate" has
/// three quite different meanings and collapsing them into one blank line in
/// the menu would be the wrong kind of tidy.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Projection {
    /// Not enough span yet to say anything honest.
    Warming,
    /// Flat or falling: nothing to project.
    Idle,
    /// Seconds until the limit reaches 100% at the current rate.
    Eta(u64),
    /// Rising, but the window resets before the cap is reached. This is the
    /// happy answer, and it is not the same as "idle".
    ResetsFirst,
}

/// Has this window been replaced by a new one?
///
/// Deliberately asymmetric: only a *forward* move counts. The same jitter that
/// motivates the tolerance also moves the timestamp backwards, and a limit that
/// appears to reset one second earlier than last time has not reset at all.
fn is_rollover(old: Option<u64>, new: Option<u64>) -> bool {
    match (old, new) {
        (Some(o), Some(n)) => n > o.saturating_add(SAME_WINDOW_TOLERANCE),
        // First sighting of a limit is not a rollover; neither is a limit that
        // stops reporting a reset time.
        _ => false,
    }
}

impl State {
    pub fn path() -> Option<PathBuf> {
        Config::dir().map(|d| d.join("state.json"))
    }

    pub fn load() -> Self {
        let Some(p) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str(t.strip_prefix('\u{feff}').unwrap_or(&t)).ok())
            .unwrap_or_default()
    }

    /// Best-effort. Losing the file costs a projection for the next ten
    /// minutes and nothing else, so a failure here is never worth surfacing.
    pub fn save(&self) {
        let (Some(dir), Some(path)) = (Config::dir(), Self::path()) else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Fold a fresh snapshot in, returning every limit that genuinely rolled
    /// over since the last one.
    pub fn record(&mut self, snap: &Snapshot) -> Vec<Rollover> {
        let mut rollovers = Vec::new();

        for limit in &snap.limits {
            let entry = self.windows.entry(limit.label.clone()).or_default();
            if is_rollover(entry.resets_at, limit.resets_at) {
                linfo!(
                    "{} rolled over (was at {:.0}%)",
                    limit.label,
                    entry.last_percent
                );
                rollovers.push(Rollover {
                    label: limit.label.clone(),
                    was_at: entry.last_percent,
                });
                entry.notified.clear();
            }
            entry.resets_at = limit.resets_at;
            entry.last_percent = limit.percent;
        }

        if let Some(head) = snap.headline() {
            let switched = self.samples_for != head.label;
            let rolled = rollovers.iter().any(|r| r.label == head.label);
            if switched || rolled {
                if !self.samples.is_empty() {
                    linfo!(
                        "projection restarting ({} samples dropped): {}",
                        self.samples.len(),
                        if switched {
                            "headline moved to another limit"
                        } else {
                            "window rolled over"
                        }
                    );
                }
                self.samples.clear();
                self.samples_for = head.label.clone();
            }
            self.samples.push(Sample {
                t: snap.fetched_at,
                percent: head.percent,
            });
            self.trim(snap.fetched_at);
        }

        rollovers
    }

    fn trim(&mut self, now: u64) {
        let cutoff = now.saturating_sub(WINDOW_SECS);
        self.samples.retain(|s| s.t >= cutoff);
        if self.samples.len() > MAX_SAMPLES {
            let excess = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(..excess);
        }
    }

    /// Thresholds newly crossed by `label`, in ascending order, marking them as
    /// notified. Only the crossing fires — staying above 80% for two days must
    /// not toast every minute.
    pub fn newly_crossed(&mut self, label: &str, percent: f64, thresholds: &[u8]) -> Vec<u8> {
        let entry = self.windows.entry(label.to_string()).or_default();
        let mut crossed = Vec::new();
        for &t in thresholds {
            if percent >= t as f64 && !entry.notified.contains(&t) {
                entry.notified.push(t);
                crossed.push(t);
            }
        }
        crossed
    }

    /// Least-squares slope through the retained samples, turned into an ETA.
    ///
    /// Least squares rather than (last - first) / span because the reported
    /// percentage is quantised — it arrives as whole numbers most of the time —
    /// so consecutive samples are frequently identical and the endpoints alone
    /// swing between "flat" and "steep" as a single unit ticks over.
    pub fn project(&self, resets_at: Option<u64>) -> Projection {
        let n = self.samples.len();
        if n < 3 {
            return Projection::Warming;
        }
        let first = self.samples[0];
        let last = self.samples[n - 1];
        if last.t.saturating_sub(first.t) < MIN_SPAN_SECS {
            return Projection::Warming;
        }

        // Relative to the first sample, so the sums stay small and the fit
        // stays well-conditioned.
        let t0 = first.t as f64;
        let nf = n as f64;
        let (mut st, mut sp, mut stp, mut stt) = (0.0, 0.0, 0.0, 0.0);
        for s in &self.samples {
            let t = s.t as f64 - t0;
            st += t;
            sp += s.percent;
            stp += t * s.percent;
            stt += t * t;
        }
        let denom = nf * stt - st * st;
        if denom.abs() < f64::EPSILON {
            return Projection::Warming;
        }
        // Percent per second.
        let slope = (nf * stp - st * sp) / denom;

        // A hair above zero is drift, not a trend: 1e-6 %/s is one percent
        // every 11 days, which would otherwise render as a confident ETA.
        if slope <= 1e-6 {
            return Projection::Idle;
        }

        let remaining = (100.0 - last.percent).max(0.0);
        let eta_secs = (remaining / slope).round();
        if !eta_secs.is_finite() || eta_secs < 0.0 {
            return Projection::Idle;
        }
        let eta = eta_secs as u64;

        match resets_at {
            Some(r) if last.t + eta >= r => Projection::ResetsFirst,
            _ => Projection::Eta(eta),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::parse;

    fn snapshot(body: &str) -> Snapshot {
        parse(body).expect("test fixture should parse")
    }

    fn state_rising(from: f64, per_hour: f64, hours: f64, step_secs: u64) -> State {
        let mut s = State::default();
        let total = (hours * 3600.0) as u64;
        let mut t = 1_000_000u64;
        let end = t + total;
        while t <= end {
            let elapsed_h = (t - 1_000_000) as f64 / 3600.0;
            s.samples.push(Sample {
                t,
                percent: from + per_hour * elapsed_h,
            });
            t += step_secs;
        }
        s
    }

    // -- the jitter bug ------------------------------------------------------

    #[test]
    fn a_second_of_jitter_is_not_a_rollover() {
        // The bug this module exists to not have again. The server recomputes
        // `resets_at` per request, so the whole-second value oscillates between
        // two adjacent numbers indefinitely.
        let mut s = State::default();
        let mut rollovers = 0;
        for secs in ["00", "01", "00", "01", "00", "01"] {
            let body = format!(
                r#"{{"limits":[{{"kind":"session","percent":40,
                     "resets_at":"2026-09-17T08:00:{secs}Z"}}]}}"#
            );
            rollovers += s.record(&snapshot(&body)).len();
        }
        assert_eq!(rollovers, 0, "sub-second jitter must never read as a reset");
        assert_eq!(s.samples.len(), 6, "and must never wipe the projection");
    }

    #[test]
    fn a_real_reset_is_still_detected() {
        let mut s = State::default();
        s.record(&snapshot(
            r#"{"limits":[{"kind":"session","percent":93,
                 "resets_at":"2026-09-17T08:00:00Z"}]}"#,
        ));
        let rolled = s.record(&snapshot(
            r#"{"limits":[{"kind":"session","percent":1,
                 "resets_at":"2026-09-17T13:00:00Z"}]}"#,
        ));
        assert_eq!(rolled.len(), 1);
        assert_eq!(rolled[0].label, "Session");
        assert_eq!(rolled[0].was_at, 93.0);
        assert!(rolled[0].worth_saying());
    }

    #[test]
    fn a_reset_from_nowhere_near_the_cap_is_not_worth_saying() {
        // The session bucket rolls over every few hours all day long.
        let r = Rollover { label: "Session".into(), was_at: 9.0 };
        assert!(!r.worth_saying());
    }

    #[test]
    fn a_backwards_jump_is_never_a_rollover() {
        assert!(!is_rollover(Some(1_000_000), Some(990_000)));
        assert!(!is_rollover(None, Some(1_000_000)));
        assert!(!is_rollover(Some(1_000_000), None));
        assert!(is_rollover(Some(1_000_000), Some(1_000_000 + 3600)));
    }

    // -- per-limit independence ---------------------------------------------

    #[test]
    fn the_headline_moving_between_limits_is_not_a_rollover() {
        // Session falls below Weekly, so the headline changes which limit it
        // names. Nothing has reset.
        let mut s = State::default();
        s.record(&snapshot(
            r#"{"limits":[
                 {"kind":"session","percent":40,"resets_at":"2026-09-17T08:00:00Z"},
                 {"kind":"weekly_all","percent":24,"resets_at":"2026-09-20T00:00:00Z"}]}"#,
        ));
        let rolled = s.record(&snapshot(
            r#"{"limits":[
                 {"kind":"session","percent":20,"resets_at":"2026-09-17T08:00:00Z"},
                 {"kind":"weekly_all","percent":24,"resets_at":"2026-09-20T00:00:00Z"}]}"#,
        ));
        assert!(rolled.is_empty(), "got {rolled:?}");
        // The samples do restart, because they now describe a different limit.
        assert_eq!(s.samples_for, "Weekly");
        assert_eq!(s.samples.len(), 1);
    }

    #[test]
    fn one_limit_resetting_does_not_disturb_another() {
        let mut s = State::default();
        s.record(&snapshot(
            r#"{"limits":[
                 {"kind":"session","percent":95,"resets_at":"2026-09-17T08:00:00Z"},
                 {"kind":"weekly_all","percent":88,"resets_at":"2026-09-20T00:00:00Z"}]}"#,
        ));
        s.newly_crossed("Weekly", 88.0, &[50, 80]);

        let rolled = s.record(&snapshot(
            r#"{"limits":[
                 {"kind":"session","percent":2,"resets_at":"2026-09-17T13:00:00Z"},
                 {"kind":"weekly_all","percent":88,"resets_at":"2026-09-20T00:00:00Z"}]}"#,
        ));
        assert_eq!(rolled.len(), 1);
        assert_eq!(rolled[0].label, "Session");
        // Weekly is untouched: still notified, so it will not re-toast 80%.
        assert_eq!(s.windows["Weekly"].notified, vec![50, 80]);
    }

    #[test]
    fn thresholds_fire_once_per_window_per_limit() {
        let mut s = State::default();
        assert_eq!(s.newly_crossed("Weekly", 82.0, &[50, 80, 95]), vec![50, 80]);
        assert!(s.newly_crossed("Weekly", 84.0, &[50, 80, 95]).is_empty());
        assert_eq!(s.newly_crossed("Weekly", 96.0, &[50, 80, 95]), vec![95]);
        // A different limit keeps its own tally.
        assert_eq!(s.newly_crossed("Session", 82.0, &[50, 80, 95]), vec![50, 80]);
    }

    #[test]
    fn a_reset_re_arms_that_limits_thresholds() {
        let mut s = State::default();
        s.record(&snapshot(
            r#"{"limits":[{"kind":"session","percent":90,
                 "resets_at":"2026-09-17T08:00:00Z"}]}"#,
        ));
        s.newly_crossed("Session", 90.0, &[80]);
        s.record(&snapshot(
            r#"{"limits":[{"kind":"session","percent":5,
                 "resets_at":"2026-09-17T13:00:00Z"}]}"#,
        ));
        assert_eq!(s.newly_crossed("Session", 85.0, &[80]), vec![80]);
    }

    // -- projection ----------------------------------------------------------

    #[test]
    fn warming_until_there_is_enough_span() {
        let s = state_rising(10.0, 5.0, 0.05, 60); // 3 minutes
        assert_eq!(s.project(None), Projection::Warming);
    }

    #[test]
    fn flat_usage_projects_nothing() {
        let s = state_rising(42.0, 0.0, 2.0, 60);
        assert_eq!(s.project(None), Projection::Idle);
    }

    #[test]
    fn a_steady_climb_gives_an_eta() {
        // 20%, climbing 10 points an hour: 80 points left, so ~8 hours.
        let s = state_rising(20.0, 10.0, 1.0, 60);
        match s.project(None) {
            Projection::Eta(secs) => {
                let hours = secs as f64 / 3600.0;
                assert!((hours - 7.0).abs() < 0.2, "expected ~7h, got {hours}h");
            }
            other => panic!("expected an ETA, got {other:?}"),
        }
    }

    #[test]
    fn a_reset_before_the_cap_is_its_own_answer() {
        let s = state_rising(20.0, 10.0, 1.0, 60);
        let last = s.samples.last().unwrap().t;
        // Resets in an hour; the cap is seven hours out.
        assert_eq!(s.project(Some(last + 3600)), Projection::ResetsFirst);
    }
}
