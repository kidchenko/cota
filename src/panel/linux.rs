//! The Linux panel: an undecorated GTK window, painted with cairo and pango.
//!
//! The counterpart to the GDI and AppKit panels. Same layout, same palette:
//! one drawing area paints the model through [`PanelModel::layout`], so the
//! picture matches the other two.
//!
//! On X11 the panel opens by the pointer, which has just clicked "Show usage"
//! in the tray menu. Wayland does not let a window choose its place, so there
//! the compositor puts it. Either way it closes when it loses focus, when it is
//! clicked, or on Esc.
//!
//! Everything here runs on the main thread (GTK demands it), so the model
//! lives in a `thread_local` rather than behind a lock, as on the other two.

use super::*;
use crate::icon;
use traykit::{ldebug, lerror};

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};

const EDGE_GAP: i32 = 6;

// ---------------------------------------------------------------------------
// State (main thread only, so a thread_local is enough)
// ---------------------------------------------------------------------------

struct PanelState {
    model: PanelModel,
    dark: bool,
    /// Suppresses dismiss-on-click, for `--preview-panel`.
    pinned: bool,
    /// When the panel last went away. See [`Panel::toggle`].
    hidden_at: Option<Instant>,
    /// X11, when the panel stands on a bottom panel: its bottom edge, so it
    /// grows upwards. `None` when it hangs from the top, or on Wayland.
    bottom: Option<i32>,
    /// The desktop's UI font, read once.
    family: Option<String>,
}

thread_local! {
    static STATE: RefCell<PanelState> = RefCell::new(PanelState {
        model: PanelModel::default(),
        dark: true,
        pinned: false,
        hidden_at: None,
        bottom: None,
        family: None,
    });
    /// Pixels per logical pixel for the Claude mark. `None` means "ask the
    /// widget"; set while `render_to_rgba` draws offscreen, where there is no
    /// widget to ask.
    static RENDER_SCALE: Cell<Option<i32>> = const { Cell::new(None) };
}

fn note_hidden() {
    STATE.with(|s| s.borrow_mut().hidden_at = Some(Instant::now()));
}

fn since_hidden() -> Option<Duration> {
    STATE.with(|s| s.borrow().hidden_at.map(|t| t.elapsed()))
}

fn pinned() -> bool {
    STATE.with(|s| s.borrow().pinned)
}

fn hide_window(window: &gtk::Window) {
    if window.is_visible() {
        window.hide();
        note_hidden();
    }
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

pub struct Panel {
    window: gtk::Window,
    area: gtk::DrawingArea,
}

impl Panel {
    pub fn new() -> Result<Self, String> {
        if !gtk::is_initialized_main_thread() {
            return Err("GTK is not running on this thread".into());
        }
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Cota");
        window.set_decorated(false);
        window.set_resizable(false);
        window.set_skip_taskbar_hint(true);
        window.set_skip_pager_hint(true);
        window.set_keep_above(true);
        window.set_type_hint(gdk::WindowTypeHint::Utility);
        window.set_icon_name(Some("cota"));
        window.set_app_paintable(true);
        // Transparent corners where a compositor can show them; square ones
        // where it cannot.
        if let Some(visual) = WidgetExt::screen(&window).and_then(|s| s.rgba_visual()) {
            window.set_visual(Some(&visual));
        }

        let area = gtk::DrawingArea::new();
        area.set_size_request(WIDTH, 200);
        area.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
        area.connect_draw(|area, cr| {
            let scale = RENDER_SCALE
                .with(|r| r.get())
                .unwrap_or_else(|| area.scale_factor());
            let composited = WidgetExt::screen(area).is_some_and(|s| s.is_composited());
            // Copied out rather than drawn under the borrow: the painter reads
            // the font family from the same state.
            let (model, dark) = STATE.with(|s| {
                let s = s.borrow();
                (s.model.clone(), s.dark)
            });
            draw_panel(cr, &model, dark, scale, composited);
            glib::Propagation::Stop
        });
        // Clicking the panel dismisses it: there is nothing in it to click, and
        // a readout that will not go away is worse than one that goes too easily.
        area.connect_button_press_event(|area, _| {
            if !pinned() {
                if let Some(w) = area.toplevel().and_downcast::<gtk::Window>() {
                    hide_window(&w);
                }
            }
            glib::Propagation::Stop
        });
        window.add(&area);

        // Click-away: the panel closes when another window takes focus.
        window.connect_focus_out_event(|w, _| {
            if !pinned() {
                hide_window(w);
            }
            glib::Propagation::Proceed
        });
        window.connect_key_press_event(|w, e| {
            if e.keyval() == gdk::keys::constants::Escape {
                hide_window(w);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.connect_delete_event(|w, _| {
            hide_window(w);
            glib::Propagation::Stop
        });
        area.show();

        Ok(Self { window, area })
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn hide(&self) {
        hide_window(&self.window);
    }

    /// Open the panel, or close it if it is already open. See
    /// [`super::toggle_action`] for the reopen-guard reasoning.
    pub fn toggle(&self, model: PanelModel, dark: bool) {
        match toggle_action(self.is_visible(), since_hidden()) {
            Toggle::Close => self.hide(),
            Toggle::Open => self.show(model, dark),
            Toggle::Ignore => ldebug!("panel dismissed a moment ago; not reopening"),
        }
    }

    /// Replace the contents of an already-open panel, leaving it where it is.
    pub fn update(&self, model: PanelModel, dark: bool) {
        if !self.is_visible() {
            return;
        }
        let before = STATE.with(|s| s.borrow().model.height());
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.dark = dark;
            s.model = model;
        });
        if STATE.with(|s| s.borrow().model.height()) != before {
            self.fit();
        }
        self.area.queue_draw();
    }

    /// Show the panel by the pointer, which has just clicked the tray menu.
    pub fn show(&self, model: PanelModel, dark: bool) {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.dark = dark;
            s.model = model;
        });
        let h = STATE.with(|s| s.borrow().model.height());
        let placed = place_by_pointer(WIDTH, h);
        STATE.with(|s| s.borrow_mut().bottom = placed.and_then(|(_, _, bottom)| bottom));
        self.fit();
        if let Some((x, y, _)) = placed {
            self.window.move_(x, y);
            ldebug!("panel shown at {x},{y} ({WIDTH}x{h})");
        }
        self.area.queue_draw();
        self.window.present();
    }

    /// Resize to the content, keeping the edge it was placed against.
    fn fit(&self) {
        let (h, bottom) = STATE.with(|s| {
            let s = s.borrow();
            (s.model.height(), s.bottom)
        });
        self.area.set_size_request(WIDTH, h);
        self.window.resize(WIDTH, h);
        if let Some(bottom) = bottom {
            let (x, _) = self.window.position();
            self.window.move_(x, bottom - h);
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        // SAFETY: the window is ours alone, and nothing touches it after this.
        unsafe { self.window.destroy() };
    }
}

/// Where the panel goes on X11: under the pointer, hanging from the top of the
/// work area or standing on its bottom, whichever edge the pointer is nearer.
/// The third value is the bottom edge when it stands on the bottom. `None` on
/// Wayland, where the compositor decides.
fn place_by_pointer(w: i32, h: i32) -> Option<(i32, i32, Option<i32>)> {
    let display = gdk::Display::default()?;
    if display.type_().name().contains("Wayland") {
        return None;
    }
    let pointer = display.default_seat()?.pointer()?;
    let (_, px, py) = pointer.position();
    let monitor = display.monitor_at_point(px, py)?;
    let area = monitor.workarea();
    let (x, y) = place(
        (px, py),
        (w, h),
        (area.x(), area.y(), area.width(), area.height()),
    );
    let bottom = (y > area.y() + EDGE_GAP).then_some(y + h);
    Some((x, y, bottom))
}

/// Apart from the display query so it can be tested.
fn place(pointer: (i32, i32), size: (i32, i32), work: (i32, i32, i32, i32)) -> (i32, i32) {
    let (px, py) = pointer;
    let (w, h) = size;
    let (ax, ay, aw, ah) = work;
    let min_x = ax + EDGE_GAP;
    let max_x = (ax + aw - w - EDGE_GAP).max(min_x);
    let x = (px - w / 2).clamp(min_x, max_x);
    let y = if py < ay + ah / 2 {
        ay + EDGE_GAP
    } else {
        (ay + ah - h - EDGE_GAP).max(ay + EDGE_GAP)
    };
    (x, y)
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw_panel(cr: &cairo::Context, model: &PanelModel, dark: bool, scale: i32, composited: bool) {
    let l = model.layout(&|v| v);
    let pal = Palette::for_theme(dark);
    let w = WIDTH as f64;
    let h = model.height() as f64;

    let pad = PAD as f64;
    let left = pad;
    let right = w - pad;

    // Clear to transparent first, so the rounded corners show the desktop.
    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();
    cr.set_operator(cairo::Operator::Over);

    // Background rounded rect + hairline border, inset half a pixel so the
    // stroke lands inside the window rather than half-off its edge. Square
    // without a compositor, which would paint the corners black.
    let radius = if composited { CORNER as f64 } else { 0.0 };
    rounded(cr, 0.0, 0.0, w, h, radius);
    set_color(cr, pal.bg);
    let _ = cr.fill();
    rounded(cr, 0.5, 0.5, w - 1.0, h - 1.0, (radius - 0.5).max(0.0));
    set_color(cr, pal.border);
    cr.set_line_width(1.0);
    let _ = cr.stroke();

    // -- header: quiet chrome, not content --------------------------------
    let f_head = font(12.0, false);
    let head_ascent = ascent(cr, &f_head);
    let mark_edge = MARK as f64;
    let mark_top = l.header_baseline as f64 - head_ascent / 2.0 - mark_edge / 2.0;
    draw_mark(cr, scale, left, mark_top);
    let header_x = left + MARK as f64 + 9.0;
    draw_text(
        cr,
        &model.header,
        header_x,
        l.header_baseline as f64,
        right - header_x,
        &f_head,
        pal.tertiary,
        Align::Left,
    );

    // -- status instead of limits -----------------------------------------
    if let (Some(status), Some(baseline)) = (&model.status, l.status_baseline) {
        draw_text(
            cr,
            status,
            left,
            baseline as f64,
            right - left,
            &font(13.0, false),
            pal.secondary,
            Align::Left,
        );
        return;
    }

    // -- hero: the limit nearest its ceiling ------------------------------
    if let (Some(hero), Some(row)) = (&l.hero, model.hero()) {
        let sev = severity_rgb(row.severity);
        draw_text(
            cr,
            &format!("{:.0}%", row.percent),
            left,
            hero.baseline as f64,
            right - left,
            &font(HERO_NUM as f64, true),
            sev,
            Align::Left,
        );
        bar(
            cr,
            left,
            right,
            hero.bar_y as f64,
            HERO_BAR_H as f64,
            row.percent,
            sev,
            pal.track,
        );
        let fs = font(12.0, false);
        sub_line(
            cr,
            row,
            left,
            right,
            hero.sub_baseline as f64,
            &fs,
            &pal,
            sev,
        );
    }

    // -- the rest, compact ------------------------------------------------
    let mut rules = l.rules.iter();
    if !l.rows.is_empty() {
        if let Some(&y) = rules.next() {
            rule(cr, left, right, y as f64, pal.rule);
        }
    }
    for (box_, row) in l.rows.iter().zip(model.rest()) {
        let sev = severity_rgb(row.severity);
        draw_text(
            cr,
            &format!("{:.0}%", row.percent),
            left,
            box_.baseline as f64,
            right - left,
            &font(13.0, true),
            pal.text,
            Align::Right,
        );
        let fs = font(12.0, false);
        sub_line(cr, row, left, right, box_.baseline as f64, &fs, &pal, sev);
        bar(
            cr,
            left,
            right,
            box_.bar_y as f64,
            ROW_BAR_H as f64,
            row.percent,
            sev,
            pal.track,
        );
    }

    // -- notes ------------------------------------------------------------
    if !l.notes.is_empty() {
        if let Some(&y) = rules.next() {
            rule(cr, left, right, y as f64, pal.rule);
        }
        let accent = model
            .hero()
            .map(|h| severity_rgb(h.severity))
            .unwrap_or(pal.tertiary);
        let f = font(12.0, false);
        let asc = ascent(cr, &f);
        for (i, (note, &baseline)) in model.notes.iter().zip(l.notes.iter()).enumerate() {
            // The first note is the projection — what happens next — so it gets
            // the accent and the brighter ink. The rest are context.
            let (dot_rgb, ink) = if i == 0 {
                (accent, pal.secondary)
            } else {
                (pal.tertiary, pal.tertiary)
            };
            let cy = baseline as f64 - asc / 2.0 + 1.0;
            dot(cr, left + DOT as f64 / 2.0, cy, DOT as f64, dot_rgb);
            let x = left + DOT as f64 + 10.0;
            draw_text(
                cr,
                note,
                x,
                baseline as f64,
                right - x,
                &f,
                ink,
                Align::Left,
            );
        }
    }
}

/// `Weekly  ·  resets in 2d 16h`, label brighter than the time, with an optional
/// live dot between them.
#[allow(clippy::too_many_arguments)]
fn sub_line(
    cr: &cairo::Context,
    row: &PanelRow,
    left: f64,
    right: f64,
    baseline: f64,
    font: &pango::FontDescription,
    pal: &Palette,
    accent: Rgb,
) {
    draw_text(
        cr,
        &row.label,
        left,
        baseline,
        right - left,
        font,
        pal.secondary,
        Align::Left,
    );

    let mut x = left + text_width(cr, &row.label, font);
    if row.active {
        let asc = ascent(cr, font);
        let gap = 7.0;
        dot(cr, x + gap, baseline - asc / 2.0 + 1.0, DOT as f64, accent);
        x += gap * 2.0;
    }
    // The dot already separates label from time; a middot as well would be
    // punctuation stacked on punctuation.
    let tail = if row.active {
        format!("   {}", row.resets)
    } else {
        format!("  \u{00b7}  {}", row.resets)
    };
    draw_text(
        cr,
        &tail,
        x,
        baseline,
        right - x,
        font,
        pal.tertiary,
        Align::Left,
    );
}

fn rule(cr: &cairo::Context, left: f64, right: f64, y: f64, rgb: Rgb) {
    cr.rectangle(left, y, right - left, 1.0);
    set_color(cr, rgb);
    let _ = cr.fill();
}

/// Track plus fill. The fill is floored at its own height so a 1% reading is a
/// dot rather than a sliver clipped to nothing by the corner radius.
#[allow(clippy::too_many_arguments)]
fn bar(
    cr: &cairo::Context,
    left: f64,
    right: f64,
    y: f64,
    h: f64,
    percent: f64,
    fill: Rgb,
    track: Rgb,
) {
    let width = right - left;
    rounded(cr, left, y, width, h, h / 2.0);
    set_color(cr, track);
    let _ = cr.fill();
    let filled = (percent / 100.0).clamp(0.0, 1.0) * width;
    if filled > 0.0 {
        rounded(cr, left, y, filled.max(h), h, h / 2.0);
        set_color(cr, fill);
        let _ = cr.fill();
    }
}

fn dot(cr: &cairo::Context, cx: f64, cy: f64, d: f64, rgb: Rgb) {
    cr.new_path();
    cr.arc(cx, cy, d / 2.0, 0.0, std::f64::consts::TAU);
    set_color(cr, rgb);
    let _ = cr.fill();
}

fn draw_mark(cr: &cairo::Context, scale: i32, x: f64, top: f64) {
    let px = (MARK * scale.max(1)).max(8) as u32;
    let rgba = icon::claude_mark(px);
    let Some(surface) = surface_from_rgba(&rgba, px, px) else {
        return;
    };
    let k = px as f64 / MARK as f64;
    surface.set_device_scale(k, k);
    let _ = cr.set_source_surface(&surface, x, top);
    let _ = cr.paint();
}

// ---------------------------------------------------------------------------
// Small cairo and pango helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

fn set_color(cr: &cairo::Context, c: Rgb) {
    cr.set_source_rgb(c.0 as f64 / 255.0, c.1 as f64 / 255.0, c.2 as f64 / 255.0);
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let rad = radius.min(w / 2.0).min(h / 2.0);
    cr.new_path();
    if rad <= 0.0 {
        cr.rectangle(x, y, w, h);
        return;
    }
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.arc(x + w - rad, y + rad, rad, -FRAC_PI_2, 0.0);
    cr.arc(x + w - rad, y + h - rad, rad, 0.0, FRAC_PI_2);
    cr.arc(x + rad, y + h - rad, rad, FRAC_PI_2, PI);
    cr.arc(x + rad, y + rad, rad, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// The desktop's UI font family (Cantarell, Ubuntu, Noto Sans...), or Sans.
fn family() -> String {
    if let Some(f) = STATE.with(|s| s.borrow().family.clone()) {
        return f;
    }
    let family = gtk::is_initialized_main_thread()
        .then(gtk::Settings::default)
        .flatten()
        .and_then(|s| s.gtk_font_name())
        .map(|name| pango::FontDescription::from_string(&name))
        .and_then(|d| d.family().map(|f| f.to_string()))
        .unwrap_or_else(|| "Sans".into());
    STATE.with(|s| s.borrow_mut().family = Some(family.clone()));
    family
}

/// The desktop font at `size` pixels; `bold` is the semibold the numbers use.
fn font(size: f64, bold: bool) -> pango::FontDescription {
    let mut f = pango::FontDescription::new();
    f.set_family(&family());
    f.set_absolute_size(size * pango::SCALE as f64);
    f.set_weight(if bold {
        pango::Weight::Semibold
    } else {
        pango::Weight::Normal
    });
    f
}

fn layout(cr: &cairo::Context, s: &str, font: &pango::FontDescription) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(font));
    layout.set_single_paragraph_mode(true);
    layout.set_text(s);
    layout
}

/// The font's ascent, in pixels: what the dots and the mark centre on.
fn ascent(cr: &cairo::Context, font: &pango::FontDescription) -> f64 {
    let context = pangocairo::functions::create_context(cr);
    let metrics = context.metrics(Some(font), None);
    metrics.ascent() as f64 / pango::SCALE as f64
}

fn text_width(cr: &cairo::Context, s: &str, font: &pango::FontDescription) -> f64 {
    let (_, logical) = layout(cr, s, font).pixel_extents();
    logical.width() as f64
}

/// Draw one line sitting on `baseline`, cut with an ellipsis if it is wider
/// than `width`. Positioned from the layout's own baseline, as the macOS side
/// is from the font's ascent, so no per-size nudge is needed.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    cr: &cairo::Context,
    s: &str,
    x: f64,
    baseline: f64,
    width: f64,
    font: &pango::FontDescription,
    rgb: Rgb,
    align: Align,
) {
    let layout = layout(cr, s, font);
    layout.set_width((width.max(0.0) * pango::SCALE as f64) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_alignment(match align {
        Align::Left => pango::Alignment::Left,
        Align::Right => pango::Alignment::Right,
    });
    let top = baseline - layout.baseline() as f64 / pango::SCALE as f64;
    set_color(cr, rgb);
    cr.move_to(x, top);
    pangocairo::functions::show_layout(cr, &layout);
}

/// Straight-alpha RGBA as a cairo surface, which is premultiplied, in the
/// machine's byte order.
fn surface_from_rgba(rgba: &[u8], w: u32, h: u32) -> Option<cairo::ImageSurface> {
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).ok()?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().ok()?;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = &rgba[(y * w as usize + x) * 4..][..4];
                let a = s[3] as u32;
                let pre = |c: u8| (c as u32 * a + 127) / 255;
                let px = (a << 24) | pre(s[0]) << 16 | pre(s[1]) << 8 | pre(s[2]);
                data[y * stride + x * 4..][..4].copy_from_slice(&px.to_ne_bytes());
            }
        }
    }
    Some(surface)
}

// ---------------------------------------------------------------------------
// Offscreen render + preview
// ---------------------------------------------------------------------------

/// Reported to the log on creation failure, where there is no panel to show it
/// in and the tray must carry on regardless.
pub fn report_failure(e: &str) {
    lerror!("panel unavailable: {e}");
}

/// Draw the panel into a bitmap and hand back its pixels, never putting a
/// window on screen: the same painter, into an image surface. Needs no
/// display; with one, it uses the desktop's font. Returns
/// `(width, height, straight-alpha RGBA)`.
pub fn render_to_rgba(model: PanelModel, dark: bool, scale: i32) -> Option<(u32, u32, Vec<u8>)> {
    let scale = scale.max(1);
    let _ = gtk::init();
    let h = model.height();
    let (pw, ph) = ((WIDTH * scale) as u32, (h * scale) as u32);
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, pw as i32, ph as i32).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        cr.scale(scale as f64, scale as f64);
        RENDER_SCALE.with(|r| r.set(Some(scale)));
        draw_panel(&cr, &model, dark, scale, true);
        RENDER_SCALE.with(|r| r.set(None));
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().ok()?;
    let mut out = vec![0u8; (pw * ph * 4) as usize];
    for y in 0..ph as usize {
        for x in 0..pw as usize {
            let px = u32::from_ne_bytes(data[y * stride + x * 4..][..4].try_into().ok()?);
            let a = px >> 24;
            if a == 0 {
                continue;
            }
            // Un-premultiply back to straight alpha, matching the other two.
            let un = |c: u32| ((c * 255 + a / 2) / a).min(255) as u8;
            let o = &mut out[(y * pw as usize + x) * 4..][..4];
            o[0] = un((px >> 16) & 0xff);
            o[1] = un((px >> 8) & 0xff);
            o[2] = un(px & 0xff);
            o[3] = a as u8;
        }
    }
    Some((pw, ph, out))
}

/// Show the panel with representative content and run GTK until the timeout,
/// so the rendering can actually be looked at. There is no tao event loop in
/// preview mode, so this pumps GTK itself.
pub fn preview(dark: bool, seconds: u64) {
    if gtk::init().is_err() {
        eprintln!("no display");
        return;
    }
    let Ok(panel) = Panel::new() else { return };
    STATE.with(|s| s.borrow_mut().pinned = true);
    panel.show(sample_model(), dark);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        while gtk::events_pending() {
            gtk::main_iteration();
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: (i32, i32, i32, i32) = (0, 32, 1920, 1048);

    #[test]
    fn the_panel_hangs_under_a_top_bar() {
        assert_eq!(place((1800, 10), (300, 400), WORK), (1614, 38));
    }

    #[test]
    fn the_panel_stands_on_a_bottom_panel() {
        let (_, y) = place((900, 1070), (300, 400), WORK);
        assert_eq!(y + 400, 32 + 1048 - EDGE_GAP);
    }

    #[test]
    fn the_panel_stays_inside_the_work_area() {
        let (x, _) = place((2, 10), (300, 400), WORK);
        assert_eq!(x, EDGE_GAP);
    }

    #[test]
    fn surfaces_premultiply() {
        let mut s = surface_from_rgba(&[255, 0, 0, 128], 1, 1).unwrap();
        let d = s.data().unwrap();
        let px = u32::from_ne_bytes(d[..4].try_into().unwrap());
        assert_eq!(px >> 24, 128);
        assert_eq!((px >> 16) & 0xff, 128, "red, premultiplied");
    }

    #[test]
    fn the_sample_renders_at_its_layout_size() {
        let model = sample_model();
        let h = model.height();
        let (w, ph, rgba) = render_to_rgba(model, true, 2).unwrap();
        assert_eq!((w, ph), (WIDTH as u32 * 2, h as u32 * 2));
        // The middle of the panel is opaque background, not a hole.
        let mid = ((ph / 2) * w + w / 2) as usize * 4;
        assert_eq!(rgba[mid + 3], 255);
    }
}
