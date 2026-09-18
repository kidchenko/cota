//! The tray application: the icon, its menu, and what they say.
//!
//! Everything here runs on the main thread, driven by the event loop in
//! `main`. The poller hands over finished [`Report`]s; this module is only
//! ever formatting and painting.
//!
//! The split between the two surfaces is deliberate. Left button opens the
//! [`panel`], which holds the numbers; right button opens the menu, which holds
//! the commands. They used to be one menu with the data in disabled items, and
//! that put the entire product in the one text colour Windows reserves for
//! things you cannot click.

use crate::burn::Share;
use crate::config::Config;
use crate::icon::{self, Face};
use crate::log::{ldebug, lerror, linfo};
use crate::panel::{Panel, PanelModel, PanelRow};
use crate::poller::{Poller, Report, SharedConfig};
use crate::state::Projection;
use crate::{autostart, creds, log, notify, theme, timefmt, util};

use tao::event::Event;
use tao::event_loop::EventLoopProxy;
use tray_icon::menu::{
    CheckMenuItem, Icon as MenuImage, IconMenuItem, Menu, MenuEvent, MenuItem,
    PredefinedMenuItem,
};
use tray_icon::{
    Icon as TrayImage, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const ID_PANEL: &str = "panel";
const ID_REFRESH: &str = "refresh";
const ID_ENABLED: &str = "enabled";
const ID_AUTOSTART: &str = "autostart";
const ID_CONFIG: &str = "config";
const ID_LOG: &str = "log";
const ID_QUIT: &str = "quit";
/// Every read-only row shares this id; nothing ever dispatches on it.
const ID_ROW: &str = "row";

/// Windows truncates `szTip` at 128 characters, without saying so.
const TOOLTIP_MAX: usize = 127;

pub enum UserEvent {
    Report(Box<Report>),
    TogglePanel,
    Refresh { announce: bool },
    ToggleEnabled,
    ToggleAutostart,
    OpenConfig,
    OpenLog,
    Quit,
}

#[derive(PartialEq, Eq)]
pub enum Flow {
    Continue,
    Exit,
}

pub struct App {
    cfg: SharedConfig,
    tray: TrayIcon,
    poller: Poller,
    /// Last face painted. Re-rendering an identical ring on every poll would
    /// be harmless but pointless, and skipping it keeps the idle profile
    /// honestly flat.
    face: Option<Face>,
    dark_taskbar: bool,
    /// Attribution arrives on a slower cadence than everything else, so the
    /// last known ranking is held here rather than blanking between updates.
    shares: Vec<Share>,
    /// The last failure said out loud, so it is not said again while it lasts.
    last_spoken_error: Option<String>,
    /// The Claude mark and the size it is drawn at, rendered once at startup.
    /// The menu is rebuilt every poll and redrawing a starburst each time would
    /// be pure waste.
    mark: Vec<u8>,
    mark_edge: u32,
    /// "Max", "Pro" — whatever the credentials file says the plan is. Read once;
    /// it changes about as often as a subscription does.
    plan: Option<String>,
    /// `None` if the popup window could not be created. The tray keeps working
    /// without it — a missing panel costs the detail view, not the meter.
    panel: Option<Panel>,
    /// What the panel will draw next time it opens, rebuilt on every report so
    /// opening it is instant and never touches the network.
    panel_model: PanelModel,
    /// Open the panel as soon as there is something to put in it. Set by
    /// `--show-panel`, which the installer uses for its "Launch Cota" tick box:
    /// a tray app that installs and then appears to do nothing is the most
    /// common way a first run goes wrong.
    open_on_first_report: bool,
}

impl App {
    pub fn new(cfg: SharedConfig, proxy: EventLoopProxy<UserEvent>) -> Result<Self, String> {
        forward_menu_events(&proxy);
        forward_tray_events(&proxy);

        let dark_taskbar = theme::taskbar_is_dark();
        let face = Face::Unknown;
        let image = TrayImage::from_rgba(icon::render(face, dark_taskbar), icon::EDGE, icon::EDGE)
            .map_err(|e| format!("could not build the tray image: {e}"))?;

        let tray = TrayIconBuilder::new()
            .with_tooltip("Cota \u{2014} starting\u{2026}")
            // Left button is handled below and opens the panel; leaving this
            // false gives the menu to the right button, as Windows expects.
            //
            // Left click did once fetch-and-toast, which meant idly clicking the
            // icon a few times was enough to get rate-limited and then be told
            // so repeatedly. It now opens a panel painted entirely from the last
            // reading: no request, no toast, instant. A readout should never
            // punish being looked at.
            .with_menu_on_left_click(false)
            .with_icon(image)
            .build()
            .map_err(|e| format!("could not create the tray icon: {e}"))?;

        let poller = {
            let proxy = proxy.clone();
            Poller::start(cfg.clone(), move |report| {
                let _ = proxy.send_event(UserEvent::Report(Box::new(report)));
            })
        };

        let app = Self {
            cfg,
            tray,
            poller,
            face: Some(face),
            dark_taskbar,
            shares: Vec::new(),
            last_spoken_error: None,
            mark_edge: util::menu_icon_edge(),
            mark: icon::claude_mark(util::menu_icon_edge()),
            plan: creds::load().ok().and_then(|c| c.subscription),
            panel: Panel::new()
                .map_err(|e| {
                    crate::panel::report_failure(&e);
                    e
                })
                .ok(),
            panel_model: PanelModel::default(),
            open_on_first_report: false,
        };
        app.rebuild_menu();
        Ok(app)
    }

    /// See [`App::open_on_first_report`].
    pub fn open_when_ready(&mut self) {
        self.open_on_first_report = true;
    }

    pub fn handle(&mut self, event: Event<UserEvent>) -> Flow {
        match event {
            Event::UserEvent(UserEvent::Quit) => {
                linfo!("quit requested from the tray");
                return Flow::Exit;
            }
            Event::UserEvent(UserEvent::Report(r)) => self.apply(*r),
            Event::UserEvent(UserEvent::TogglePanel) => self.toggle_panel(),
            Event::UserEvent(UserEvent::Refresh { announce }) => self.poller.refresh(announce),
            Event::UserEvent(UserEvent::ToggleEnabled) => self.toggle_enabled(),
            Event::UserEvent(UserEvent::ToggleAutostart) => self.toggle_autostart(),
            Event::UserEvent(UserEvent::OpenLog) => log::reveal(),
            Event::UserEvent(UserEvent::OpenConfig) => {
                if let Some(d) = Config::dir() {
                    let _ = std::fs::create_dir_all(&d);
                    util::shell_open(&d.to_string_lossy());
                }
            }
            _ => {}
        }
        Flow::Continue
    }

    // -- reacting to a poll --------------------------------------------------

    fn apply(&mut self, mut report: Report) {
        // The taskbar theme can change under a running app, and the ring's
        // track is the one thing tuned to it.
        self.dark_taskbar = theme::taskbar_is_dark();

        if let Some(shares) = report.shares.take() {
            self.shares = shares;
        }

        let paused = self.cfg.lock().map(|c| !c.enabled).unwrap_or(false);
        let (face, tooltip) = match &report.result {
            _ if paused => (Face::Paused, "Cota \u{2014} paused".to_string()),
            Ok(snap) => match snap.headline() {
                Some(h) => (
                    Face::Meter {
                        percent: h.percent,
                        severity: h.severity,
                    },
                    format!(
                        "Cota \u{2014} {} {:.0}% \u{00b7} resets {}",
                        h.label,
                        h.percent,
                        timefmt::until(h.resets_at, snap.fetched_at)
                    ),
                ),
                None => (Face::Unknown, "Cota \u{2014} no limits reported".into()),
            },
            Err(e) => (Face::Unknown, format!("Cota \u{2014} {}", e.short())),
        };

        self.panel_model = self.build_panel(&report, paused);
        self.set_face(face);
        self.set_tooltip(&tooltip);
        self.rebuild_menu();
        self.repaint_panel();
        if self.open_on_first_report {
            self.open_on_first_report = false;
            self.toggle_panel();
        }
        self.announce(&report, &tooltip);
    }

    /// Everything the panel needs, assembled on each report so that opening it
    /// is a paint and nothing else.
    fn build_panel(&self, report: &Report, paused: bool) -> PanelModel {
        let header = self.header();

        if paused {
            return PanelModel {
                header,
                status: Some("Polling is paused.".into()),
                ..Default::default()
            };
        }

        let snap = match &report.result {
            Ok(s) => s,
            Err(e) => {
                return PanelModel {
                    header,
                    status: Some(format!("{e}")),
                    ..Default::default()
                }
            }
        };

        let now = snap.fetched_at;
        let rows = snap
            .limits
            .iter()
            .map(|l| PanelRow {
                label: l.label.clone(),
                percent: l.percent,
                severity: l.severity,
                resets: format!("resets {}", timefmt::until(l.resets_at, now)),
                active: l.is_active,
            })
            .collect();

        let mut notes = Vec::new();
        match report.projection {
            Projection::Eta(secs) => {
                notes.push(format!("At this rate: full in {}", timefmt::humanize(secs)))
            }
            Projection::ResetsFirst => notes.push("At this rate: resets before it fills".into()),
            Projection::Idle | Projection::Warming => {}
        }
        if let Some(top) = self.shares.first() {
            notes.push(format!(
                "Mostly: {} \u{00b7} {:.0}% of 7d, est.",
                top.project,
                top.fraction * 100.0
            ));
        }
        if let Some(extra) = &snap.extra {
            if extra.enabled {
                notes.push(format!(
                    "Extra usage: {} {:.0} of {:.0} ({:.0}%)",
                    extra.currency, extra.used, extra.limit, extra.percent
                ));
            }
        }
        if let Some(secs) = report.stale_for {
            if secs >= 30 {
                notes.push(format!("Last checked {} ago", timefmt::humanize(secs)));
            }
        }

        PanelModel {
            header,
            rows,
            notes,
            status: None,
        }
    }

    fn toggle_panel(&mut self) {
        if let Some(panel) = &self.panel {
            panel.toggle(self.panel_model.clone(), theme::taskbar_is_dark());
        }
    }

    /// Keep an already-open panel current. A poll landing while the user is
    /// looking at it should update it, not leave a stale reading on screen.
    fn repaint_panel(&self) {
        if let Some(panel) = &self.panel {
            panel.update(self.panel_model.clone(), theme::taskbar_is_dark());
        }
    }

    /// "Claude Max" / "Claude Pro" / plain "Claude" if the plan is unreadable.
    /// Title-cased because the credentials file spells it `max`, and a menu
    /// header that reads "Claude max" looks like a bug.
    fn header(&self) -> String {
        match &self.plan {
            Some(p) if !p.is_empty() => {
                let mut c = p.chars();
                let titled = c
                    .next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default();
                format!("Claude {titled}")
            }
            _ => "Claude".into(),
        }
    }

    /// Toasts. Ordered so that at most one fires per poll, loudest first — a
    /// reset and a threshold cannot both be true of the same window, and a
    /// requested refresh should not also fire the crossing it revealed.
    fn announce(&mut self, report: &Report, summary: &str) {
        let cfg = self.cfg.lock().map(|c| c.clone()).unwrap_or_default();

        if let Some(c) = report.crossings.last() {
            self.last_spoken_error = None;
            let resets = timefmt::until(c.resets_at, crate::util::now_unix());
            notify::threshold(&cfg, &c.label, c.threshold, c.percent, &resets);
            return;
        }
        // Only resets that were actually blocking you. The session bucket rolls
        // over every few hours; announcing every one of those is how a useful
        // notification becomes one you turn off.
        if cfg.notify {
            if let Some(r) = report.rollovers.iter().find(|r| r.worth_saying()) {
                self.last_spoken_error = None;
                notify::show(
                    &format!("{} limit reset", r.label),
                    &format!("Was at {:.0}%. You can work again.", r.was_at),
                    None,
                );
                return;
            }
        }
        if !report.announce {
            return;
        }

        // Saying the same failure twice in a row is how a broken poll becomes a
        // stream of identical notifications. Say it once; the ring and the menu
        // carry it from then on, and they do not interrupt.
        if let Err(e) = &report.result {
            let text = e.short().to_string();
            if self.last_spoken_error.as_deref() == Some(text.as_str()) {
                ldebug!("suppressing a repeat of the {text} toast");
                return;
            }
            self.last_spoken_error = Some(text);
        } else {
            self.last_spoken_error = None;
        }

        // Strip the leading "Cota — " that the tooltip carries; the toast
        // already has the app's name on it.
        let body = summary.split_once("\u{2014} ").map(|x| x.1).unwrap_or(summary);
        // A replayed reading must say so, or "refresh" silently becomes a word
        // for "show me something from four minutes ago".
        let body = match report.stale_for {
            Some(secs) if secs >= 30 => {
                format!("{body}  (as of {} ago)", timefmt::humanize(secs))
            }
            _ => body.to_string(),
        };
        let detail = self.panel_model.notes.first().cloned();
        notify::show("Cota", &body, detail.as_deref());
    }

    // -- tray plumbing -------------------------------------------------------

    fn set_face(&mut self, face: Face) {
        if self.face == Some(face) {
            return;
        }
        match TrayImage::from_rgba(
            icon::render(face, self.dark_taskbar),
            icon::EDGE,
            icon::EDGE,
        ) {
            Ok(image) => {
                if let Err(e) = self.tray.set_icon(Some(image)) {
                    lerror!("could not set the tray icon: {e}");
                } else {
                    self.face = Some(face);
                    ldebug!("icon now {face:?}");
                }
            }
            Err(e) => lerror!("could not build the tray image: {e}"),
        }
    }

    fn set_tooltip(&self, text: &str) {
        let trimmed: String = text.chars().take(TOOLTIP_MAX).collect();
        if let Err(e) = self.tray.set_tooltip(Some(&trimmed)) {
            lerror!("could not set the tooltip: {e}");
        }
    }

    /// The whole menu is rebuilt rather than mutated, because the read-only
    /// section changes length: a scoped weekly limit appears and disappears,
    /// the projection comes and goes. Rebuilding is a few dozen allocations
    /// once a minute and removes a class of bug where a stale row survives.
    fn rebuild_menu(&self) {
        let (enabled, _) = self
            .cfg
            .lock()
            .map(|c| (c.enabled, c.poll_seconds))
            .unwrap_or((true, 60));

        let menu = Menu::new();

        // A header naming the service, so the menu is self-identifying even
        // though the numbers now live in the panel.
        let _ = menu.append(&IconMenuItem::with_id(
            ID_ROW,
            self.header(),
            false,
            MenuImage::from_rgba(self.mark.clone(), self.mark_edge, self.mark_edge).ok(),
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());

        // Commands only. The readout moved to the panel because a menu paints
        // everything you cannot click in the same grey, which put the data
        // below "Quit Cota" in the visual hierarchy.
        let _ = menu.append(&MenuItem::with_id(ID_PANEL, "Show usage", true, None));
        let _ = menu.append(&MenuItem::with_id(ID_REFRESH, "Refresh now", true, None));
        let _ = menu.append(&CheckMenuItem::with_id(
            ID_ENABLED,
            "Polling enabled",
            true,
            enabled,
            None,
        ));
        let _ = menu.append(&CheckMenuItem::with_id(
            ID_AUTOSTART,
            "Run at login",
            true,
            autostart::is_enabled(),
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(ID_CONFIG, "Open config folder", true, None));
        let _ = menu.append(&MenuItem::with_id(ID_LOG, "Open log", true, None));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(ID_QUIT, "Quit Cota", true, None));

        self.tray.set_menu(Some(Box::new(menu)));
    }

    // -- toggles -------------------------------------------------------------

    fn toggle_enabled(&mut self) {
        let now = {
            let Ok(mut c) = self.cfg.lock() else { return };
            c.enabled = !c.enabled;
            let _ = c.save();
            c.enabled
        };
        linfo!("polling {}", if now { "resumed" } else { "paused" });
        if now {
            // Resuming should show a fresh number immediately rather than
            // leaving a stale ring until the next tick.
            self.poller.refresh(false);
        } else {
            self.set_face(Face::Paused);
            self.set_tooltip("Cota \u{2014} paused");
        }
        self.rebuild_menu();
    }

    /// On failure we report what the registry actually says rather than what
    /// was asked for, so the checkbox cannot lie.
    fn toggle_autostart(&mut self) {
        let target = !autostart::is_enabled();
        match autostart::set(target) {
            Ok(()) => linfo!("run at login: {target}"),
            Err(e) => {
                lerror!("could not change autostart: {e}");
                notify::show("Cota", "Could not change the run-at-login setting.", Some(&e));
            }
        }
        self.rebuild_menu();
    }
}

fn forward_menu_events(proxy: &EventLoopProxy<UserEvent>) {
    let proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let event = match e.id.0.as_str() {
            ID_PANEL => UserEvent::TogglePanel,
            ID_REFRESH => UserEvent::Refresh { announce: false },
            ID_ENABLED => UserEvent::ToggleEnabled,
            ID_AUTOSTART => UserEvent::ToggleAutostart,
            ID_CONFIG => UserEvent::OpenConfig,
            ID_LOG => UserEvent::OpenLog,
            ID_QUIT => UserEvent::Quit,
            _ => return,
        };
        let _ = proxy.send_event(event);
    }));
}

fn forward_tray_events(proxy: &EventLoopProxy<UserEvent>) {
    let proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        // `button_state` is the important half of this pattern. `Click` fires
        // for both press and release, so matching on the button alone toggles
        // twice per click — the panel appeared on the way down and vanished on
        // the way up, which looked exactly like a flicker.
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = e
        {
            let _ = proxy.send_event(UserEvent::TogglePanel);
        }
    }));
}
