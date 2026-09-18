//! The readout panel: a borderless popup showing the numbers.
//!
//! The menu used to carry the numbers, as disabled items. That worked, but it
//! got the hierarchy exactly backwards: a disabled menu item renders grey, so
//! the data — the entire product — rendered dimmer than "Quit Cota". A menu can
//! only ever offer one text colour and one weight, and this app's whole job is
//! to make one number obvious at a glance.
//!
//! So the data moved here and the menu kept the commands. Left button opens the
//! panel, right button opens the menu.
//!
//! This module holds everything that does not care how pixels reach the screen:
//! the [`PanelModel`], the [`Layout`] arithmetic, the geometry constants, and
//! the dismiss-guard logic. The platform submodules own the window and the
//! drawing — GDI on Windows, AppKit on macOS — and both are painted from the
//! same layout, so the picture matches whoever draws it.

use crate::usage::Severity;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{preview, render_to_rgba, report_failure, Panel};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{preview, render_to_rgba, report_failure, Panel};

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------
// Layout, in logical pixels at 96 DPI / 1x. Everything is scaled by the
// monitor's DPI (Windows) or backing scale (macOS) at show time.
//
// The vertical metrics are deliberately a small set of repeated values rather
// than a number per gap: the first version tuned each space by eye and the
// result had no rhythm — the bar crowded its label while the rows drifted far
// apart, so three limits read as three unrelated clusters instead of a list.
pub(crate) const WIDTH: i32 = 300;
pub(crate) const PAD: i32 = 18;
pub(crate) const MARK: i32 = 15;

pub(crate) const HEADER_H: i32 = 18;
/// Header to hero. Generous on purpose: it is the one place a big gap helps,
/// because it separates chrome from content.
pub(crate) const HEADER_GAP: i32 = 18;

// The hero: the limit nearest its ceiling, given the space to be read from
// across the room. Everything else on the panel is context for this number.
pub(crate) const HERO_NUM: i32 = 34;
pub(crate) const HERO_NUM_H: i32 = 40;
pub(crate) const HERO_BAR_H: i32 = 10;
pub(crate) const HERO_SUB_H: i32 = 18;
pub(crate) const TIGHT: i32 = 6;

// Secondary limits, two lines each: label and percentage sharing a baseline,
// with the reset time between them, then a thin bar.
pub(crate) const ROW_LINE_H: i32 = 19;
pub(crate) const ROW_BAR_H: i32 = 5;
pub(crate) const ROW_GAP: i32 = 14;

pub(crate) const RULE_GAP: i32 = 15;
pub(crate) const NOTE_H: i32 = 20;
pub(crate) const DOT: i32 = 5;

/// Corner radius, matching what DWM rounds a popup with on Windows and what the
/// macOS panel rounds itself to.
pub(crate) const CORNER: i32 = 8;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------
// Written the natural 0xRRGGBB way; each platform converts to its own colour
// type (COLORREF on Windows, NSColor components on macOS).

#[derive(Clone, Copy)]
pub(crate) struct Rgb(pub u8, pub u8, pub u8);

pub(crate) struct Palette {
    pub bg: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub secondary: Rgb,
    pub tertiary: Rgb,
    pub track: Rgb,
    pub rule: Rgb,
}

impl Palette {
    pub(crate) fn for_theme(dark: bool) -> Self {
        if dark {
            Self {
                bg: Rgb(0x20, 0x20, 0x20),
                border: Rgb(0x3a, 0x3a, 0x3a),
                text: Rgb(0xff, 0xff, 0xff),
                secondary: Rgb(0xc4, 0xc4, 0xc4),
                tertiary: Rgb(0x8a, 0x8a, 0x8a),
                track: Rgb(0x38, 0x38, 0x38),
                rule: Rgb(0x33, 0x33, 0x33),
            }
        } else {
            Self {
                bg: Rgb(0xfb, 0xfb, 0xfb),
                border: Rgb(0xdd, 0xdd, 0xdd),
                text: Rgb(0x1a, 0x1a, 0x1a),
                secondary: Rgb(0x3c, 0x3c, 0x3c),
                tertiary: Rgb(0x6c, 0x6c, 0x6c),
                track: Rgb(0xe6, 0xe6, 0xe6),
                rule: Rgb(0xea, 0xea, 0xea),
            }
        }
    }
}

pub(crate) fn severity_rgb(s: Severity) -> Rgb {
    match s {
        Severity::Normal => Rgb(0x3f, 0xb9, 0x50),
        Severity::Warning => Rgb(0xd2, 0x99, 0x22),
        Severity::Critical => Rgb(0xf8, 0x51, 0x49),
    }
}

// ---------------------------------------------------------------------------
// What the panel draws
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PanelRow {
    /// Drawn upper-case; kept natural here so the model stays presentational.
    pub label: String,
    pub percent: f64,
    pub severity: Severity,
    /// Already-formatted, e.g. "resets in 1h 23m".
    pub resets: String,
    /// The limit the server flags as currently binding.
    pub active: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PanelModel {
    pub header: String,
    pub rows: Vec<PanelRow>,
    /// Projection and attribution, below the rule.
    pub notes: Vec<String>,
    /// Shown instead of the rows when there is nothing to show: an error, or
    /// "polling is paused".
    pub status: Option<String>,
}

/// Where every element goes, in scaled pixels.
///
/// Computed once and consumed by both the height calculation and the paint, so
/// the two cannot disagree. They did in an earlier version — the height was its
/// own arithmetic mirroring the drawing code, and every layout tweak had to be
/// made twice or the panel grew a band of dead space at the bottom.
pub(crate) struct Layout {
    pub header_baseline: i32,
    /// `None` when there is a status to show instead of limits.
    pub hero: Option<HeroBox>,
    pub rows: Vec<RowBox>,
    /// Horizontal rules, by y.
    pub rules: Vec<i32>,
    /// Baselines for the note lines.
    pub notes: Vec<i32>,
    pub status_baseline: Option<i32>,
    pub height: i32,
}

pub(crate) struct HeroBox {
    /// Shared by the big percentage and the label beside it.
    pub baseline: i32,
    pub bar_y: i32,
    pub sub_baseline: i32,
}

pub(crate) struct RowBox {
    pub baseline: i32,
    pub bar_y: i32,
}

impl PanelModel {
    /// Index of the limit the panel leads with: the fullest.
    ///
    /// Computed here rather than trusting `rows[0]`. `usage` does sort its
    /// limits highest-first, so taking the first would be correct today — but
    /// that is an unwritten contract between two modules with a whole app
    /// between them, and the failure mode is silent and bad: the panel would
    /// calmly lead with 46% while a 97% limit sat underneath it.
    pub(crate) fn hero_index(&self) -> Option<usize> {
        self.rows
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.percent.total_cmp(&b.percent))
            .map(|(i, _)| i)
    }

    pub(crate) fn hero(&self) -> Option<&PanelRow> {
        self.rows.get(self.hero_index()?)
    }

    /// Every limit except the hero, in their given order.
    pub(crate) fn rest(&self) -> Vec<&PanelRow> {
        let hero = self.hero_index();
        self.rows
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != hero)
            .map(|(_, r)| r)
            .collect()
    }

    pub(crate) fn layout(&self, scale: &dyn Fn(i32) -> i32) -> Layout {
        let mut y = scale(PAD);
        let header_baseline = y + scale(HEADER_H);
        y = header_baseline + scale(HEADER_GAP);

        let mut l = Layout {
            header_baseline,
            hero: None,
            rows: Vec::new(),
            rules: Vec::new(),
            notes: Vec::new(),
            status_baseline: None,
            height: 0,
        };

        if let Some(status) = &self.status {
            let _ = status;
            l.status_baseline = Some(y + scale(NOTE_H));
            l.height = y + scale(NOTE_H) + scale(PAD);
            return l;
        }

        if self.hero().is_some() {
            let baseline = y + scale(HERO_NUM_H);
            let bar_y = baseline + scale(TIGHT);
            let sub_baseline = bar_y + scale(HERO_BAR_H) + scale(HERO_SUB_H);
            l.hero = Some(HeroBox {
                baseline,
                bar_y,
                sub_baseline,
            });
            y = sub_baseline;
        }

        if !self.rest().is_empty() {
            y += scale(RULE_GAP);
            l.rules.push(y);
            y += scale(RULE_GAP);
            for i in 0..self.rest().len() {
                if i > 0 {
                    y += scale(ROW_GAP);
                }
                let baseline = y + scale(ROW_LINE_H);
                let bar_y = baseline + scale(TIGHT);
                l.rows.push(RowBox { baseline, bar_y });
                y = bar_y + scale(ROW_BAR_H);
            }
        }

        if !self.notes.is_empty() {
            y += scale(RULE_GAP);
            l.rules.push(y);
            y += scale(RULE_GAP) - scale(TIGHT);
            for _ in &self.notes {
                l.notes.push(y + scale(NOTE_H) - scale(TIGHT));
                y += scale(NOTE_H);
            }
            y -= scale(TIGHT);
        }

        l.height = y + scale(PAD);
        l
    }

    /// Panel height for this content. Computed rather than fixed because the
    /// number of limits the endpoint reports is not ours to decide — a scoped
    /// per-model limit appears and disappears on its own.
    pub(crate) fn height(&self) -> i32 {
        self.layout(&|v| v).height
    }
}

// ---------------------------------------------------------------------------
// Dismiss guard (shared by both platforms)
// ---------------------------------------------------------------------------

/// Long enough to cover the gap between the panel losing focus and the tray
/// click that caused it arriving, short enough that a deliberate second click
/// still opens the panel.
pub(crate) const REOPEN_GUARD: std::time::Duration = std::time::Duration::from_millis(400);

/// What a toggle should do. A three-way answer, because "not currently visible"
/// and "should therefore be shown" are not the same thing.
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Toggle {
    Close,
    Open,
    /// It was just dismissed by the very click being handled. Doing nothing is
    /// what makes the panel close rather than flicker.
    Ignore,
}

/// Clicking the tray icon while the panel is open takes the focus away from it,
/// so the platform hides it *before* the click event reaches us — at which point
/// a naive toggle sees a hidden panel and dutifully reopens the thing the user
/// was trying to dismiss. Neither side of that race can be removed, so instead
/// the panel remembers when it last went away and declines to come straight
/// back.
pub(crate) fn toggle_action(visible: bool, since_hidden: Option<std::time::Duration>) -> Toggle {
    if visible {
        Toggle::Close
    } else if since_hidden.is_some_and(|d| d < REOPEN_GUARD) {
        Toggle::Ignore
    } else {
        Toggle::Open
    }
}

/// The content the website and the preview both show. One definition, so the
/// picture on the page is the same thing `--preview-panel` puts on screen.
pub fn sample_model() -> PanelModel {
    PanelModel {
        header: "Claude Max".into(),
        rows: vec![
            PanelRow {
                label: "Weekly".into(),
                percent: 88.0,
                severity: Severity::Warning,
                resets: "resets in 2d 17h".into(),
                active: true,
            },
            PanelRow {
                label: "Session".into(),
                percent: 46.0,
                severity: Severity::Normal,
                resets: "resets in 1h 23m".into(),
                active: false,
            },
            PanelRow {
                label: "Weekly (Fable)".into(),
                percent: 3.0,
                severity: Severity::Normal,
                resets: "resets in 2d 17h".into(),
                active: false,
            },
        ],
        notes: vec![
            "At this rate: full in 3h 20m".into(),
            "Mostly: dotnet-saas \u{00b7} 41% of 7d, est.".into(),
        ],
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn row(percent: f64) -> PanelRow {
        PanelRow {
            label: "Session".into(),
            percent,
            severity: Severity::Normal,
            resets: "resets in 1h".into(),
            active: false,
        }
    }

    fn model(rows: usize, notes: usize) -> PanelModel {
        PanelModel {
            header: "Claude Max".into(),
            rows: (0..rows).map(|i| row(i as f64 * 10.0)).collect(),
            notes: (0..notes).map(|i| format!("note {i}")).collect(),
            status: None,
        }
    }

    #[test]
    fn height_is_whatever_the_layout_ends_at() {
        // The property the Layout type exists to guarantee. Height used to be
        // its own arithmetic mirroring the drawing code, and the two drifted:
        // every tweak had to be made twice or the panel grew dead space.
        for rows in 0..4 {
            for notes in 0..3 {
                let m = model(rows, notes);
                let l = m.layout(&|v| v);
                let lowest = l
                    .notes
                    .iter()
                    .chain(l.rows.iter().map(|r| &r.bar_y))
                    .chain(l.hero.iter().map(|h| &h.sub_baseline))
                    .chain(l.status_baseline.iter())
                    .chain(std::iter::once(&l.header_baseline))
                    .max()
                    .copied()
                    .unwrap_or(0);
                assert!(
                    m.height() > lowest,
                    "{rows} rows / {notes} notes: height {} does not clear its lowest element {lowest}",
                    m.height()
                );
                assert!(
                    m.height() - lowest <= PAD + NOTE_H,
                    "{rows} rows / {notes} notes: {} of dead space below the content",
                    m.height() - lowest
                );
            }
        }
    }

    #[test]
    fn the_hero_is_the_fullest_limit_whatever_the_order() {
        // Not rows[0]. The panel used to take the first row and would happily
        // lead with 46% while a 97% limit sat underneath it.
        let m = PanelModel {
            rows: vec![row(46.0), row(97.0), row(3.0)],
            ..model(0, 0)
        };
        assert_eq!(m.hero().unwrap().percent, 97.0);
        let rest: Vec<f64> = m.rest().iter().map(|r| r.percent).collect();
        assert_eq!(rest, vec![46.0, 3.0], "the hero must not repeat below");
        assert_eq!(m.layout(&|v| v).rows.len(), 2);
    }

    #[test]
    fn the_first_row_is_the_hero_when_sorted() {
        let m = model(3, 0);
        assert_eq!(m.hero().unwrap().percent, 20.0);
        assert_eq!(m.rest().len(), 2);
        let l = m.layout(&|v| v);
        assert!(l.hero.is_some());
        assert_eq!(l.rows.len(), 2, "the hero must not also be drawn as a row");
    }

    #[test]
    fn a_single_limit_draws_no_rule_and_no_row() {
        let l = model(1, 0).layout(&|v| v);
        assert!(l.hero.is_some());
        assert!(l.rows.is_empty());
        assert!(l.rules.is_empty(), "a rule with nothing under it is a stray line");
    }

    #[test]
    fn each_section_brings_its_own_rule() {
        assert_eq!(model(1, 2).layout(&|v| v).rules.len(), 1, "notes only");
        assert_eq!(model(3, 0).layout(&|v| v).rules.len(), 1, "rows only");
        assert_eq!(model(3, 2).layout(&|v| v).rules.len(), 2, "both");
    }

    #[test]
    fn height_grows_with_the_rows() {
        assert!(model(3, 0).height() > model(1, 0).height());
        assert!(model(3, 2).height() > model(3, 0).height());
    }

    #[test]
    fn a_status_collapses_the_panel() {
        let full = model(3, 2);
        let errored = PanelModel {
            status: Some("not signed in".into()),
            ..full.clone()
        };
        assert!(
            errored.height() < full.height(),
            "an error panel must not reserve space for rows it will not draw"
        );
        let l = errored.layout(&|v| v);
        assert!(l.hero.is_none() && l.rows.is_empty() && l.rules.is_empty());
    }

    #[test]
    fn layout_scales_with_dpi() {
        let m = model(3, 2);
        let at_100 = m.layout(&|v| v).height;
        let at_150 = m.layout(&|v| v * 3 / 2).height;
        // Allow for integer rounding across the many additions.
        assert!((at_150 as f64 / at_100 as f64 - 1.5).abs() < 0.02);
    }

    #[test]
    fn a_click_on_an_open_panel_closes_it() {
        assert_eq!(toggle_action(true, None), Toggle::Close);
        assert_eq!(
            toggle_action(true, Some(Duration::from_millis(10))),
            Toggle::Close
        );
    }

    #[test]
    fn a_click_that_already_dismissed_the_panel_does_not_reopen_it() {
        assert_eq!(
            toggle_action(false, Some(Duration::from_millis(30))),
            Toggle::Ignore
        );
    }

    #[test]
    fn a_deliberate_second_click_still_opens_it() {
        assert_eq!(toggle_action(false, None), Toggle::Open);
        assert_eq!(
            toggle_action(false, Some(Duration::from_secs(2))),
            Toggle::Open
        );
    }
}
