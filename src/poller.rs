//! The background thread that does all the work.
//!
//! Everything slow lives here — the HTTP call, the transcript scan, the JSON —
//! so the UI thread only ever receives a finished [`Report`] and repaints. A
//! tray app that stalls its own message pump shows up as a taskbar that stops
//! responding to right-clicks, and the cause is never obvious from the outside.
//!
//! It also owns the config file watch. Cantos gives that its own module because
//! its watcher is polling the cursor thirty-three times a second and the config
//! check has to ride along with something; here there is already a loop ticking
//! once a minute, and a second thread to check one mtime would be ceremony.

use crate::burn::{Attribution, Share};
use crate::config::Config;
use crate::log::{ldebug, lerror, linfo, lwarn};
use crate::state::{Projection, Rollover, State};
use crate::usage::{self, FetchError, Snapshot};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub type SharedConfig = Arc<Mutex<Config>>;

/// Re-attributing on every poll would read the whole transcript tail once a
/// minute for a line in a menu almost nobody has open. Five minutes is well
/// inside "fresh enough" for a seven-day share.
const ATTRIBUTION_EVERY: Duration = Duration::from_secs(300);

/// How long to stay off the endpoint after it says no. The endpoint is not
/// generous and hammering it is how a stale icon becomes a permanently stale
/// one.
///
/// This is a deadline, not a multiplier on the poll interval, specifically so
/// that a user-requested refresh cannot shorten it. An earlier version reset
/// the backoff on every `Refresh`, which meant a rate-limited app retried on
/// every click and stayed rate-limited indefinitely.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(300);

/// No two requests closer together than this, whoever asks and for whatever
/// reason. The tray icon is clickable and menu items are clickable, and the
/// limits move far too slowly for a second request within ten seconds to be
/// able to tell anyone anything new.
const MIN_REQUEST_GAP: Duration = Duration::from_secs(10);

/// Whether a tick should actually go to the network.
#[derive(PartialEq, Eq, Debug)]
enum Decision {
    Fetch,
    /// Too soon to ask again, but somebody wants an answer — give them the
    /// last one and mark it stale.
    Cached,
    /// Too soon, and nobody is waiting on it.
    Skip,
}

/// Pulled out of the loop as a pure function because the bug it prevents is
/// invisible in a running app: everything looks fine until the endpoint starts
/// refusing, and by then the cause is several minutes in the past.
fn decide(
    since_last_request: Option<Duration>,
    backoff_remaining: Option<Duration>,
    announce: bool,
) -> Decision {
    let too_soon = since_last_request.is_some_and(|d| d < MIN_REQUEST_GAP);
    let backing_off = backoff_remaining.is_some_and(|d| !d.is_zero());
    if !too_soon && !backing_off {
        Decision::Fetch
    } else if announce {
        Decision::Cached
    } else {
        Decision::Skip
    }
}

pub enum Cmd {
    /// Poll now. `announce` marks a refresh the user asked for out loud —
    /// from the tray menu or by relaunching the exe — which gets a toast.
    Refresh { announce: bool },
    Stop,
}

#[cfg(test)]
mod throttle_tests {
    use super::*;

    const LONG_AGO: Option<Duration> = Some(Duration::from_secs(600));
    const JUST_NOW: Option<Duration> = Some(Duration::from_secs(2));

    #[test]
    fn the_first_request_always_goes() {
        assert_eq!(decide(None, None, false), Decision::Fetch);
        assert_eq!(decide(None, None, true), Decision::Fetch);
    }

    #[test]
    fn a_normal_tick_fetches() {
        assert_eq!(decide(LONG_AGO, None, false), Decision::Fetch);
    }

    #[test]
    fn clicking_twice_in_a_second_only_asks_once() {
        // The bug this exists for: the tray used to fire a request per click,
        // and a handful of clicks was enough to get rate-limited.
        assert_eq!(decide(JUST_NOW, None, true), Decision::Cached);
        assert_eq!(decide(JUST_NOW, None, false), Decision::Skip);
    }

    #[test]
    fn a_refresh_cannot_shorten_the_backoff() {
        // The other half of the bug: a refresh used to reset the backoff, so a
        // rate-limited app retried on every click and never recovered.
        let backoff = Some(Duration::from_secs(120));
        assert_eq!(decide(LONG_AGO, backoff, true), Decision::Cached);
        assert_eq!(decide(LONG_AGO, backoff, false), Decision::Skip);
    }

    #[test]
    fn service_resumes_when_the_backoff_runs_out() {
        assert_eq!(
            decide(LONG_AGO, Some(Duration::ZERO), false),
            Decision::Fetch
        );
    }
}

/// One limit crossing one threshold, ready to be said out loud.
pub struct Crossing {
    pub label: String,
    pub threshold: u8,
    pub percent: f64,
    pub resets_at: Option<u64>,
}

pub struct Report {
    pub result: Result<Snapshot, FetchError>,
    pub projection: Projection,
    /// `None` when this poll did not re-attribute — which is most of them,
    /// since attribution runs on a slower cadence. Distinct from `Some(vec![])`,
    /// which means "we looked and there is nothing", and would correctly clear
    /// a stale row the UI is still showing.
    pub shares: Option<Vec<Share>>,
    pub crossings: Vec<Crossing>,
    /// Limits whose window rolled over since the last poll. Per limit, not a
    /// flag: the session bucket resetting and the weekly one resetting are very
    /// different news.
    pub rollovers: Vec<Rollover>,
    pub announce: bool,
    /// This report is a replay of an earlier reading, because asking again so
    /// soon would have been throttled. Seconds since it was actually fetched.
    pub stale_for: Option<u64>,
}

pub struct Poller {
    tx: Sender<Cmd>,
}

impl Poller {
    /// Spawn the loop. `on_report` is called from the poller thread, so it must
    /// do nothing but hand the result to the event loop.
    pub fn start<F>(cfg: SharedConfig, on_report: F) -> Self
    where
        F: Fn(Report) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Cmd>();

        let _ = std::thread::Builder::new()
            .name("cota-poller".into())
            .spawn(move || {
                let mut state = State::load();
                let mut attribution = Attribution::default();
                let mut last_attribution: Option<Instant> = None;
                let mut config_stamp: Option<SystemTime> = Config::modified();
                let mut announce = false;
                // The last successful reading, replayed when a refresh arrives
                // too soon to be worth a request.
                let mut cached: Option<Snapshot> = None;
                let mut last_request: Option<Instant> = None;
                let mut retry_after: Option<Instant> = None;

                loop {
                    let snapshot_cfg = cfg.lock().map(|c| c.clone()).unwrap_or_default();

                    if snapshot_cfg.enabled {
                        let now = Instant::now();
                        let decision = decide(
                            last_request.map(|t| now.duration_since(t)),
                            retry_after.map(|t| t.saturating_duration_since(now)),
                            announce,
                        );

                        match decision {
                            Decision::Fetch => {
                                last_request = Some(now);
                                let report = poll_once(
                                    &snapshot_cfg,
                                    &mut state,
                                    &mut attribution,
                                    &mut last_attribution,
                                    announce,
                                );
                                match &report.result {
                                    Ok(snap) => {
                                        cached = Some(snap.clone());
                                        retry_after = None;
                                    }
                                    Err(FetchError::RateLimited) => {
                                        // A deadline, so the next Refresh cannot
                                        // walk it back.
                                        retry_after = Some(now + RATE_LIMIT_BACKOFF);
                                        lwarn!(
                                            "rate limited; not asking again for {}s",
                                            RATE_LIMIT_BACKOFF.as_secs()
                                        );
                                    }
                                    Err(_) => {}
                                }
                                on_report(report);
                            }
                            Decision::Cached => {
                                ldebug!("refresh throttled; replaying the last reading");
                                on_report(replay(cached.clone(), &state, &snapshot_cfg));
                            }
                            Decision::Skip => ldebug!("tick throttled; nothing to do"),
                        }
                    } else if announce {
                        ldebug!("summoned while paused; nothing to report");
                    }
                    announce = false;

                    // While backing off, wake at the deadline rather than the
                    // poll interval, so service resumes promptly instead of on
                    // the next whole minute after it.
                    let wait = retry_after
                        .map(|t| t.saturating_duration_since(Instant::now()))
                        .filter(|d| !d.is_zero())
                        .unwrap_or(Duration::from_secs(snapshot_cfg.poll_seconds));

                    match rx.recv_timeout(wait) {
                        Ok(Cmd::Stop) => {
                            linfo!("poller stopping");
                            state.save();
                            return;
                        }
                        // Note what is deliberately NOT here: any reset of
                        // `retry_after` or `last_request`. A refresh asks to be
                        // served, not to be exempted.
                        Ok(Cmd::Refresh { announce: a }) => announce = a,
                        Err(RecvTimeoutError::Timeout) => {}
                        // The UI thread is gone; so should we be.
                        Err(RecvTimeoutError::Disconnected) => {
                            state.save();
                            return;
                        }
                    }

                    // A hand edit to config.json reaches the running app on the
                    // next tick rather than waiting for a restart.
                    let stamp = Config::modified();
                    if stamp != config_stamp {
                        config_stamp = stamp;
                        if let Some(fresh) = Config::reload() {
                            if let Ok(mut guard) = cfg.lock() {
                                if *guard != fresh {
                                    linfo!("config.json changed on disk; reloaded");
                                    *guard = fresh;
                                }
                            }
                        }
                    }
                }
            });

        Self { tx }
    }

    pub fn refresh(&self, announce: bool) {
        let _ = self.tx.send(Cmd::Refresh { announce });
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

/// A report built from the last reading rather than a fresh request.
///
/// Carries no crossings and no rollover: those are transitions, and nothing has
/// transitioned since the reading this replays. Firing them again here would
/// toast the same threshold twice for one crossing.
fn replay(cached: Option<Snapshot>, state: &State, cfg: &Config) -> Report {
    let stale_for = cached
        .as_ref()
        .map(|s| crate::util::now_unix().saturating_sub(s.fetched_at));
    let projection = match (&cached, cfg.projection) {
        (Some(s), true) => state.project(s.headline().and_then(|h| h.resets_at)),
        _ => Projection::Idle,
    };
    Report {
        result: cached.ok_or(FetchError::RateLimited),
        projection,
        shares: None,
        crossings: Vec::new(),
        rollovers: Vec::new(),
        announce: true,
        stale_for,
    }
}

fn poll_once(
    cfg: &Config,
    state: &mut State,
    attribution: &mut Attribution,
    last_attribution: &mut Option<Instant>,
    announce: bool,
) -> Report {
    let result = usage::fetch(cfg);

    let mut projection = Projection::Warming;
    let mut crossings = Vec::new();
    let mut rollovers = Vec::new();

    match &result {
        Ok(snap) => {
            rollovers = state.record(snap);
            if let Some(head) = snap.headline() {
                linfo!("{} {:.0}% ({:?})", head.label, head.percent, head.severity);
                projection = if cfg.projection {
                    state.project(head.resets_at)
                } else {
                    Projection::Idle
                };
                for t in state.newly_crossed(&head.label, head.percent, &cfg.thresholds) {
                    crossings.push(Crossing {
                        label: head.label.clone(),
                        threshold: t,
                        percent: head.percent,
                        resets_at: head.resets_at,
                    });
                }
            }
            state.save();
        }
        // Logged at error level even though we recover: a run of these is the
        // difference between "the number is stale" and "the number is wrong",
        // and only the log can tell them apart after the fact.
        Err(e) => lerror!("poll failed: {e}"),
    }

    let due = last_attribution.is_none_or(|t| t.elapsed() >= ATTRIBUTION_EVERY);
    let shares = if cfg.attribution && result.is_ok() && due {
        *last_attribution = Some(Instant::now());
        Some(attribution.refresh(crate::util::now_unix()))
    } else {
        None
    };

    Report {
        result,
        projection,
        shares,
        crossings,
        rollovers,
        announce,
        stale_for: None,
    }
}
