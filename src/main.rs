// No console window: this is a tray app, and a flashing conhost on login would
// be the first thing every user complained about.
#![windows_subsystem = "windows"]

//! Startup and wiring. The work happens elsewhere:
//!
//! | module     | responsibility                                          |
//! |------------|---------------------------------------------------------|
//! | `creds`    | finds the Claude Code OAuth token                       |
//! | `usage`    | calls the usage endpoint and makes sense of the reply    |
//! | `state`    | samples the percentage over time, and projects it        |
//! | `burn`     | attributes the week to projects, from local transcripts  |
//! | `poller`   | the background thread that drives all of the above       |
//! | `icon`     | draws the ring                                           |
//! | `app`      | the tray icon and its menu                               |

mod app;
mod autostart;
mod burn;
mod config;
mod creds;
mod icon;
mod log;
mod notify;
mod panel;
mod poller;
mod single_instance;
mod state;
mod theme;
mod timefmt;
mod usage;
mod util;

use app::{App, Flow, UserEvent};
use config::Config;
use log::{lerror, linfo, lwarn};
use std::sync::{Arc, Mutex};
use tao::event_loop::{ControlFlow, EventLoopBuilder};

fn main() {
    // Writes every icon face to a directory as raw RGBA and exits. There is no
    // console under `windows_subsystem = "windows"`, so the ring is otherwise
    // only ever visible at 16x16 in the corner of a taskbar — which is a poor
    // place to notice that an arc runs the wrong way.
    if let Some(dir) = arg_value("--dump-icons") {
        dump_icons(&dir);
        return;
    }

    // Holds the panel on screen with representative content, so its rendering
    // can be looked at rather than assumed. See `--dump-icons` above.
    if let Some(theme) = arg_value("--preview-panel") {
        panel::preview(theme != "light", 20);
        return;
    }

    // Renders the panel to a file without ever putting it on screen. See the
    // note on `panel::render_to_rgba` for why a screenshot cannot do this job.
    if let Some(path) = arg_value("--render-panel") {
        let dark = arg_value("--theme").map(|t| t != "light").unwrap_or(true);
        let scale = arg_value("--scale")
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        render_panel(&path, dark, scale);
        return;
    }

    log::init();
    log_startup();

    // If we are the second launch, the first has been asked to report and
    // there is nothing left for us to do.
    let Some(signal) = single_instance::acquire() else {
        linfo!("another instance owns the tray; asked it to report, exiting");
        return;
    };

    let cfg: poller::SharedConfig = Arc::new(Mutex::new(Config::load()));
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Launching the app again is how a user asks "what is my usage right now"
    // without going to the tray.
    let signal_proxy = proxy.clone();
    signal.watch(move || {
        let _ = signal_proxy.send_event(UserEvent::Refresh { announce: true });
    });

    let mut app = match App::new(cfg, proxy.clone()) {
        Ok(app) => app,
        Err(e) => {
            lerror!("{e}");
            return;
        }
    };
    // The installer's post-install step passes this so a fresh install shows
    // the thing it just installed.
    if std::env::args().any(|a| a == "--show-panel") {
        app.open_when_ready();
    }
    linfo!("tray icon created; poller running");

    event_loop.run(move |event, _target, control_flow| {
        // Tray apps idle; nothing here needs a continuous redraw.
        *control_flow = ControlFlow::Wait;
        if app.handle(event) == Flow::Exit {
            *control_flow = ControlFlow::Exit;
        }
    });
}

/// Raw RGBA plus a sidecar with the dimensions, for the same reason
/// `--dump-icons` writes raw: a PNG encoder is a dependency the shipped app has
/// no other use for. `assets/preview.py` turns these into images.
fn render_panel(path: &str, dark: bool, scale: i32) {
    let Some((w, h, rgba)) = panel::render_to_rgba(panel::sample_model(), dark, scale) else {
        return;
    };
    let _ = std::fs::write(path, &rgba);
    let _ = std::fs::write(format!("{path}.size"), format!("{w} {h}"));
}

fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
    }
    None
}

/// See the call site in `main`. Raw RGBA rather than PNG so this costs no
/// dependency; `assets/preview.py` turns the files into something viewable.
fn dump_icons(dir: &str) {
    use icon::Face;
    use usage::Severity;

    let _ = std::fs::create_dir_all(dir);
    let mut faces: Vec<(String, Face)> = vec![
        ("paused".into(), Face::Paused),
        ("unknown".into(), Face::Unknown),
    ];
    for (percent, severity) in [
        (2.0, Severity::Normal),
        (19.0, Severity::Normal),
        (50.0, Severity::Normal),
        (80.0, Severity::Warning),
        (96.0, Severity::Critical),
        (100.0, Severity::Critical),
    ] {
        faces.push((format!("{percent:.0}"), Face::Meter { percent, severity }));
    }

    // 32 is what the tray uses; the larger set is for the landing page, drawn
    // rather than upscaled so the arc stays crisp.
    for edge in [icon::EDGE, 192] {
        for dark in [true, false] {
            let suffix = if dark { "dark" } else { "light" };
            for (name, face) in &faces {
                let path = if edge == icon::EDGE {
                    format!("{dir}/{name}-{suffix}.rgba")
                } else {
                    format!("{dir}/{name}-{suffix}@{edge}.rgba")
                };
                let _ = std::fs::write(&path, icon::render_at(*face, dark, edge));
            }
        }
    }
    for dark in [true, false] {
        let suffix = if dark { "dark" } else { "light" };
        // The Claude mark is theme-independent, but writing it under both
        // suffixes keeps the preview script's naming uniform.
        let _ = std::fs::write(
            format!("{dir}/claude-{suffix}.rgba"),
            icon::claude_mark(icon::EDGE),
        );
    }
    let _ = std::fs::write(format!("{dir}/EDGE"), icon::EDGE.to_string());
}

/// The header every log file opens with. Worth the few lines: "which build,
/// running from where, signed in as what" answers most support questions
/// before the first poll.
fn log_startup() {
    linfo!(
        "--- Cota {} starting (pid {}) ---",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
    linfo!(
        "exe: {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into())
    );
    linfo!("args: {:?}", std::env::args().skip(1).collect::<Vec<_>>());

    // Recorded at every start, because "the icon is grey" is otherwise
    // indistinguishable between never-signed-in, a stale token, and no network
    // — and this line answers the first of the three outright.
    match creds::load() {
        Ok(c) => linfo!(
            "credentials: found, plan={}, expires in {}",
            c.subscription.as_deref().unwrap_or("unknown"),
            c.expires_at_secs
                .map(|e| timefmt::humanize(e.saturating_sub(util::now_unix())))
                .unwrap_or_else(|| "unknown".into())
        ),
        Err(e) => lwarn!("credentials: {e}"),
    }
}
