//! The readout panel: a borderless popup, painted with GDI.
//!
//! The menu used to carry the numbers, as disabled items. That worked, but it
//! got the hierarchy exactly backwards: Windows paints a disabled item grey, so
//! the data — the entire product — rendered dimmer than "Quit Cota". A menu can
//! only ever offer one text colour and one weight, and this app's whole job is
//! to make one number obvious at a glance.
//!
//! So the data moved here and the menu kept the commands. Left button opens the
//! panel, right button opens the menu.
//!
//! Painted with plain GDI rather than a WebView. Cantos pays for WebView2
//! because it has twenty-one actions across four corners to configure, and HTML
//! is genuinely the right tool for that. This is five rows of text and three
//! rectangles; a browser engine and ~80 MB resident to draw them would be an
//! absurd trade in an app whose binary size is a stated goal.
//!
//! Everything here runs on the UI thread — the window is created on it, painted
//! on it, and its messages are pumped by tao's event loop — so the model lives
//! in a `thread_local` rather than behind a lock.

use crate::icon;
use crate::log::{ldebug, lerror};
use crate::usage::Severity;
use crate::util::{pcwstr, wide};
use std::cell::RefCell;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: &str = "CotaPanelWindow";

// Layout, in logical pixels at 96 DPI. Everything is scaled by the monitor's
// DPI at show time.
//
// The vertical metrics are deliberately a small set of repeated values rather
// than a number per gap: the first version tuned each space by eye and the
// result had no rhythm — the bar crowded its label while the rows drifted far
// apart, so three limits read as three unrelated clusters instead of a list.
const WIDTH: i32 = 300;
const PAD: i32 = 18;
const MARK: i32 = 15;

const HEADER_H: i32 = 18;
/// Header to hero. Generous on purpose: it is the one place a big gap helps,
/// because it separates chrome from content.
const HEADER_GAP: i32 = 18;

// The hero: the limit nearest its ceiling, given the space to be read from
// across the room. Everything else on the panel is context for this number.
const HERO_NUM: i32 = 34;
const HERO_NUM_H: i32 = 40;
const HERO_BAR_H: i32 = 10;
const HERO_SUB_H: i32 = 18;
const TIGHT: i32 = 6;

// Secondary limits, two lines each: label and percentage sharing a baseline,
// with the reset time between them, then a thin bar.
const ROW_LINE_H: i32 = 19;
const ROW_BAR_H: i32 = 5;
const ROW_GAP: i32 = 14;

const RULE_GAP: i32 = 15;
const NOTE_H: i32 = 20;
const DOT: i32 = 5;

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
struct Layout {
    header_baseline: i32,
    /// `None` when there is a status to show instead of limits.
    hero: Option<HeroBox>,
    rows: Vec<RowBox>,
    /// Horizontal rules, by y.
    rules: Vec<i32>,
    /// Baselines for the note lines.
    notes: Vec<i32>,
    status_baseline: Option<i32>,
    height: i32,
}

struct HeroBox {
    /// Shared by the big percentage and the label beside it.
    baseline: i32,
    bar_y: i32,
    sub_baseline: i32,
}

struct RowBox {
    baseline: i32,
    bar_y: i32,
}

impl PanelModel {
    /// Index of the limit the panel leads with: the fullest.
    ///
    /// Computed here rather than trusting `rows[0]`. `usage` does sort its
    /// limits highest-first, so taking the first would be correct today — but
    /// that is an unwritten contract between two modules with a whole app
    /// between them, and the failure mode is silent and bad: the panel would
    /// calmly lead with 46% while a 97% limit sat underneath it.
    fn hero_index(&self) -> Option<usize> {
        self.rows
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.percent.total_cmp(&b.percent))
            .map(|(i, _)| i)
    }

    fn hero(&self) -> Option<&PanelRow> {
        self.rows.get(self.hero_index()?)
    }

    /// Every limit except the hero, in their given order.
    fn rest(&self) -> Vec<&PanelRow> {
        let hero = self.hero_index();
        self.rows
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != hero)
            .map(|(_, r)| r)
            .collect()
    }

    fn layout(&self, scale: &dyn Fn(i32) -> i32) -> Layout {
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
    fn height(&self) -> i32 {
        self.layout(&|v| v).height
    }
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

struct Palette {
    bg: COLORREF,
    border: COLORREF,
    text: COLORREF,
    secondary: COLORREF,
    tertiary: COLORREF,
    track: COLORREF,
    rule: COLORREF,
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    // COLORREF is 0x00BBGGRR — the reverse of how every colour in this codebase
    // is written, which is a fine way to spend an afternoon if you forget.
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

impl Palette {
    fn for_theme(dark: bool) -> Self {
        if dark {
            Self {
                bg: rgb(0x20, 0x20, 0x20),
                border: rgb(0x3a, 0x3a, 0x3a),
                text: rgb(0xff, 0xff, 0xff),
                secondary: rgb(0xc4, 0xc4, 0xc4),
                tertiary: rgb(0x8a, 0x8a, 0x8a),
                track: rgb(0x38, 0x38, 0x38),
                rule: rgb(0x33, 0x33, 0x33),
            }
        } else {
            Self {
                bg: rgb(0xfb, 0xfb, 0xfb),
                border: rgb(0xdd, 0xdd, 0xdd),
                text: rgb(0x1a, 0x1a, 0x1a),
                secondary: rgb(0x3c, 0x3c, 0x3c),
                tertiary: rgb(0x6c, 0x6c, 0x6c),
                track: rgb(0xe6, 0xe6, 0xe6),
                rule: rgb(0xea, 0xea, 0xea),
            }
        }
    }
}

fn severity_colour(s: Severity) -> COLORREF {
    match s {
        Severity::Normal => rgb(0x3f, 0xb9, 0x50),
        Severity::Warning => rgb(0xd2, 0x99, 0x22),
        Severity::Critical => rgb(0xf8, 0x51, 0x49),
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct PanelState {
    model: PanelModel,
    dark: bool,
    /// The Claude mark, at the size the header draws it.
    mark: Vec<u8>,
    mark_edge: u32,
    /// Set by `--preview-panel`. Suppresses dismiss-on-deactivate, because the
    /// point of the preview is to hold the panel on screen long enough to look
    /// at — and anything that looks at it takes the focus away.
    pinned: bool,
    /// When the panel last went away. See `Panel::toggle`.
    hidden_at: Option<std::time::Instant>,
}

thread_local! {
    static STATE: RefCell<PanelState> = RefCell::new(PanelState {
        model: PanelModel::default(),
        dark: true,
        mark: Vec::new(),
        mark_edge: 0,
        pinned: false,
        hidden_at: None,
    });
}

/// Long enough to cover the gap between the panel losing focus and the tray
/// click that caused it arriving, short enough that a deliberate second click
/// still opens the panel.
const REOPEN_GUARD: std::time::Duration = std::time::Duration::from_millis(400);

/// What a toggle should do. A three-way answer, because "not currently visible"
/// and "should therefore be shown" are not the same thing.
#[derive(PartialEq, Eq, Debug)]
enum Toggle {
    Close,
    Open,
    /// It was just dismissed by the very click being handled. Doing nothing is
    /// what makes the panel close rather than flicker.
    Ignore,
}

fn toggle_action(visible: bool, since_hidden: Option<std::time::Duration>) -> Toggle {
    if visible {
        Toggle::Close
    } else if since_hidden.is_some_and(|d| d < REOPEN_GUARD) {
        Toggle::Ignore
    } else {
        Toggle::Open
    }
}

fn note_hidden() {
    STATE.with(|s| s.borrow_mut().hidden_at = Some(std::time::Instant::now()));
}

fn since_hidden() -> Option<std::time::Duration> {
    STATE.with(|s| s.borrow().hidden_at.map(|t| t.elapsed()))
}

fn pinned() -> bool {
    STATE.with(|s| s.borrow().pinned)
}

pub struct Panel {
    hwnd: HWND,
}

impl Panel {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
            let class = wide(CLASS_NAME);

            let wc = WNDCLASSW {
                // CS_DROPSHADOW gives the popup a shadow on Windows 10, where
                // there is no DWM corner preference to ask for.
                style: CS_DROPSHADOW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: pcwstr(&class),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            // A zero return means the class already exists, which is fine — the
            // window below will use it either way.
            let _ = RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                // TOOLWINDOW keeps it out of the taskbar and out of Alt+Tab;
                // TOPMOST keeps it above whatever the user was looking at.
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                pcwstr(&class),
                pcwstr(&wide("Cota")),
                WS_POPUP,
                0,
                0,
                WIDTH,
                200,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;

            // Windows 11 rounds the corners for us. Older builds return an
            // error we do not care about.
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );

            Ok(Self { hwnd })
        }
    }

    pub fn is_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        note_hidden();
    }

    /// Open the panel, or close it if it is already open.
    ///
    /// The guard is the subtle half. Clicking the tray icon while the panel is
    /// open takes the focus away from it, so `WM_ACTIVATE` hides it *before*
    /// the click event reaches us — at which point a naive toggle sees a hidden
    /// panel and dutifully reopens the thing the user was trying to dismiss.
    /// Neither side of that race can be removed, so instead the panel remembers
    /// when it last went away and declines to come straight back.
    pub fn toggle(&self, model: PanelModel, dark: bool) {
        match toggle_action(self.is_visible(), since_hidden()) {
            Toggle::Close => self.hide(),
            Toggle::Open => self.show(model, dark),
            Toggle::Ignore => ldebug!("panel dismissed a moment ago; not reopening"),
        }
    }

    /// Replace the contents of an already-open panel, leaving it where it is.
    ///
    /// Emphatically not `show`. A poll lands every minute, and re-showing an
    /// open panel would re-run the placement — so the panel would jump to
    /// wherever the mouse happened to be and snatch the foreground back, once a
    /// minute, while the user was reading it.
    pub fn update(&self, model: PanelModel, dark: bool) {
        if !self.is_visible() {
            return;
        }
        let resized = STATE.with(|s| {
            let mut s = s.borrow_mut();
            let before = s.model.height();
            s.dark = dark;
            s.model = model;
            s.model.height() != before
        });
        unsafe {
            if resized {
                // A limit appeared or went away; grow or shrink in place rather
                // than clipping the new content.
                let dpi = GetDpiForWindow(self.hwnd).max(96);
                let (w, h) = STATE.with(|s| {
                    let s = s.borrow();
                    (
                        (WIDTH * dpi as i32) / 96,
                        (s.model.height() * dpi as i32) / 96,
                    )
                });
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    w,
                    h,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    /// Show the panel near the cursor, which is over the tray icon the user has
    /// just clicked.
    ///
    /// Deliberately not anchored to the tray icon's own rectangle:
    /// `Shell_NotifyIconGetRect` needs the window handle and icon id that
    /// `tray-icon` owns privately, and the cursor is within a few pixels of the
    /// icon at exactly the moment this is called anyway.
    pub fn show(&self, model: PanelModel, dark: bool) {
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd).max(96);
            let scale = |v: i32| (v * dpi as i32) / 96;

            let mark_edge = scale(MARK).max(8) as u32;
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                if s.mark_edge != mark_edge {
                    s.mark = icon::claude_mark(mark_edge);
                    s.mark_edge = mark_edge;
                }
                s.dark = dark;
                s.model = model;
            });

            let (w, h) = STATE.with(|s| {
                let s = s.borrow();
                (scale(WIDTH), scale(s.model.height()))
            });

            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            let (x, y) = place(cursor, w, h);

            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            // Focus is what makes WM_ACTIVATE fire when the user clicks away,
            // which is how the panel knows to dismiss itself.
            let _ = SetForegroundWindow(self.hwnd);
            let _ = InvalidateRect(Some(self.hwnd), None, true);
            ldebug!("panel shown at {x},{y} ({w}x{h} @ {dpi}dpi)");
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Put the panel above and left of the cursor, then pull it back inside the
/// monitor's work area. The work area rather than the screen, so it never ends
/// up underneath the taskbar it was launched from.
fn place(cursor: POINT, w: i32, h: i32) -> (i32, i32) {
    let mut area = RECT::default();
    unsafe {
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            area = info.rcWork;
        }
    }
    if area.right <= area.left {
        // No monitor info: put it at the cursor and let Windows cope.
        return (cursor.x - w, cursor.y - h);
    }
    place_in(cursor, w, h, area)
}

const EDGE_GAP: i32 = 8;

/// The placement arithmetic, separated from the monitor query so it can be
/// tested against work areas this machine does not have.
///
/// Every bound is ordered with `max` before it reaches `clamp`. `i32::clamp`
/// **panics** when `min > max`, and on a work area narrower than the panel that
/// is exactly what `area.right - w - EDGE_GAP` produces — so a small enough
/// display, or a large enough DPI scale, would have taken the whole tray app
/// down with it. `panic = "abort"` in the release profile means there is no
/// unwinding to soften that: the icon would simply vanish.
fn place_in(cursor: POINT, w: i32, h: i32, area: RECT) -> (i32, i32) {
    let min_x = area.left + EDGE_GAP;
    let max_x = (area.right - w - EDGE_GAP).max(min_x);
    let x = (cursor.x - w / 2).clamp(min_x, max_x);

    let min_y = area.top + EDGE_GAP;
    let max_y = (area.bottom - h - EDGE_GAP).max(min_y);
    // Above the cursor if there is room, below it otherwise — the taskbar can
    // be at the top of the screen.
    let preferred = if cursor.y - h - EDGE_GAP >= area.top {
        cursor.y - h - EDGE_GAP
    } else {
        cursor.y + EDGE_GAP
    };
    (x, preferred.clamp(min_y, max_y))
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        // Painted entirely in WM_PAINT off a memory DC; letting Windows erase
        // first would just flash the background colour.
        WM_ERASEBKGND => LRESULT(1),
        WM_ACTIVATE => {
            if (wp.0 & 0xffff) as u32 == WA_INACTIVE && !pinned() {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                note_hidden();
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            note_hidden();
            LRESULT(0)
        }
        // Clicking the panel dismisses it. There is nothing in it to click, and
        // a readout that will not go away is worse than one that goes away too
        // easily.
        WM_LBUTTONUP | WM_RBUTTONUP => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            note_hidden();
            LRESULT(0)
        }
        // The panel is reused for the life of the process; closing must hide it
        // rather than destroy the window out from under `Panel`.
        WM_CLOSE => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            note_hidden();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// A GDI object that is deleted when it goes out of scope. Painting creates a
/// dozen brushes and fonts per frame and every early return is a leak without
/// this.
struct Owned<T: Copy>(T, fn(T));

impl<T: Copy> Drop for Owned<T> {
    fn drop(&mut self) {
        (self.1)(self.0)
    }
}

fn brush(colour: COLORREF) -> Owned<HBRUSH> {
    Owned(unsafe { CreateSolidBrush(colour) }, |h| unsafe {
        let _ = DeleteObject(h.into());
    })
}

fn font(height: i32, weight: i32) -> Owned<HFONT> {
    let face = wide("Segoe UI");
    Owned(
        unsafe {
            CreateFontW(
                -height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                pcwstr(&face),
            )
        },
        |h| unsafe {
            let _ = DeleteObject(h.into());
        },
    )
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        // Double-buffered: a dozen fills and twenty DrawTextW calls straight to
        // the screen DC is visible tearing on a window this size.
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old = SelectObject(mem, bmp.into());

        draw(mem, rc);

        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(mem), 0, 0, SRCCOPY);

        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn draw(hdc: HDC, rc: RECT) {
    STATE.with(|state| {
        let state = state.borrow();
        let model = &state.model;
        let p = Palette::for_theme(state.dark);
        let dpi = rc.right * 96 / WIDTH.max(1);
        let scale = |v: i32| (v * dpi) / 96;
        let l = model.layout(&scale);

        let pad = scale(PAD);
        let left = pad;
        let right = rc.right - pad;

        unsafe {
            SetBkMode(hdc, TRANSPARENT);
            FillRect(hdc, &rc, brush(p.bg).0);
            FrameRect(hdc, &rc, brush(p.border).0);

            // -- header: quiet chrome, not content ------------------------
            let f = font(scale(12), 400);
            let old = SelectObject(hdc, f.0.into());
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(hdc, &mut tm);
            if state.mark_edge > 0 && !state.mark.is_empty() {
                // Centred on the header text's own cap height, so the mark and
                // the words sit on one optical line at any DPI.
                let cy = l.header_baseline - tm.tmAscent / 2;
                blit_mark(
                    hdc,
                    &state.mark,
                    state.mark_edge,
                    left,
                    cy - state.mark_edge as i32 / 2,
                    p.bg,
                );
            }
            SetTextColor(hdc, p.tertiary);
            text_baseline(
                hdc,
                &model.header,
                left + scale(MARK) + scale(9),
                right,
                l.header_baseline,
                DT_LEFT,
            );
            SelectObject(hdc, old);

            // -- status instead of limits ---------------------------------
            if let (Some(status), Some(baseline)) = (&model.status, l.status_baseline) {
                let f = font(scale(13), 400);
                let old = SelectObject(hdc, f.0.into());
                SetTextColor(hdc, p.secondary);
                text_baseline(hdc, status, left, right, baseline, DT_LEFT);
                SelectObject(hdc, old);
                return;
            }

            // -- hero: the limit nearest its ceiling ----------------------
            if let (Some(hero), Some(row)) = (&l.hero, model.hero()) {
                let colour = severity_colour(row.severity);

                let f = font(scale(HERO_NUM), 600);
                let old = SelectObject(hdc, f.0.into());
                SetTextColor(hdc, colour);
                text_baseline(
                    hdc,
                    &format!("{:.0}%", row.percent),
                    left,
                    right,
                    hero.baseline,
                    DT_LEFT,
                );
                SelectObject(hdc, old);

                bar(
                    hdc,
                    left,
                    right,
                    hero.bar_y,
                    scale(HERO_BAR_H),
                    row.percent,
                    colour,
                    p.track,
                );

                let f = font(scale(12), 400);
                let old = SelectObject(hdc, f.0.into());
                sub_line(
                    hdc,
                    &p,
                    row,
                    left,
                    right,
                    hero.sub_baseline,
                    scale(7),
                    scale(DOT),
                    scale(1),
                    colour,
                );
                SelectObject(hdc, old);
            }

            // -- the rest, compact ----------------------------------------
            let mut rules = l.rules.iter();
            if !l.rows.is_empty() {
                if let Some(&y) = rules.next() {
                    rule(hdc, left, right, y, p.rule);
                }
            }
            for (box_, row) in l.rows.iter().zip(model.rest().into_iter()) {
                let colour = severity_colour(row.severity);

                let f = font(scale(13), 600);
                let old = SelectObject(hdc, f.0.into());
                SetTextColor(hdc, p.text);
                text_baseline(
                    hdc,
                    &format!("{:.0}%", row.percent),
                    left,
                    right,
                    box_.baseline,
                    DT_RIGHT,
                );
                SelectObject(hdc, old);

                let f = font(scale(12), 400);
                let old = SelectObject(hdc, f.0.into());
                sub_line(
                    hdc,
                    &p,
                    row,
                    left,
                    right,
                    box_.baseline,
                    scale(7),
                    scale(DOT),
                    scale(1),
                    colour,
                );
                SelectObject(hdc, old);

                bar(
                    hdc,
                    left,
                    right,
                    box_.bar_y,
                    scale(ROW_BAR_H),
                    row.percent,
                    colour,
                    p.track,
                );
            }

            // -- notes ------------------------------------------------------
            if !l.notes.is_empty() {
                if let Some(&y) = rules.next() {
                    rule(hdc, left, right, y, p.rule);
                }
                let accent = model
                    .hero()
                    .map(|h| severity_colour(h.severity))
                    .unwrap_or(p.tertiary);
                let f = font(scale(12), 400);
                let old = SelectObject(hdc, f.0.into());
                let mut tm = TEXTMETRICW::default();
                let _ = GetTextMetricsW(hdc, &mut tm);
                for (i, (note, &baseline)) in model.notes.iter().zip(l.notes.iter()).enumerate() {
                    // The first note is the projection: the one line on the
                    // panel that says what happens next, so it gets the accent
                    // and the brighter ink. The rest are context.
                    let (colour, ink) = if i == 0 {
                        (accent, p.secondary)
                    } else {
                        (p.tertiary, p.tertiary)
                    };
                    dot(
                        hdc,
                        left + scale(DOT) / 2,
                        baseline - tm.tmAscent / 2 + scale(1),
                        scale(DOT),
                        colour,
                    );
                    SetTextColor(hdc, ink);
                    text_baseline(
                        hdc,
                        note,
                        left + scale(DOT) + scale(10),
                        right,
                        baseline,
                        DT_LEFT,
                    );
                }
                SelectObject(hdc, old);
            }
        }
    });
}

/// `Weekly  ·  resets in 2d 16h`, with the label brighter than the time and an
/// optional live dot between them.
///
/// Drawn as three pieces rather than one string because the label and the reset
/// time want different ink: joined at one colour the eye cannot pick the limit
/// names out of the column, and at the time's colour the whole line recedes.
#[allow(clippy::too_many_arguments)]
fn sub_line(
    hdc: HDC,
    p: &Palette,
    row: &PanelRow,
    left: i32,
    right: i32,
    baseline: i32,
    gap: i32,
    dot_size: i32,
    nudge: i32,
    accent: COLORREF,
) {
    unsafe {
        SetTextColor(hdc, p.secondary);
    }
    text_baseline(hdc, &row.label, left, right, baseline, DT_LEFT);

    let mut x = left + text_width(hdc, &row.label);
    if row.active {
        let mut tm = TEXTMETRICW::default();
        unsafe {
            let _ = GetTextMetricsW(hdc, &mut tm);
        }
        dot(hdc, x + gap, baseline - tm.tmAscent / 2 + nudge, dot_size, accent);
        x += gap * 2;
    }

    unsafe {
        SetTextColor(hdc, p.tertiary);
    }
    // The dot already separates the label from the time; a middot as well is
    // punctuation stacked on punctuation.
    let tail = if row.active {
        format!("   {}", row.resets)
    } else {
        format!("  \u{00b7}  {}", row.resets)
    };
    text_baseline(hdc, &tail, x, right, baseline, DT_LEFT);
}

fn rule(hdc: HDC, left: i32, right: i32, y: i32, colour: COLORREF) {
    let r = RECT {
        left,
        top: y,
        right,
        bottom: y + 1,
    };
    unsafe {
        FillRect(hdc, &r, brush(colour).0);
    }
}

/// Track plus fill. The fill is floored at its own height so a 1% reading is a
/// dot rather than a sliver clipped to nothing by the corner radius.
#[allow(clippy::too_many_arguments)]
fn bar(
    hdc: HDC,
    left: i32,
    right: i32,
    y: i32,
    h: i32,
    percent: f64,
    fill: COLORREF,
    track: COLORREF,
) {
    let full = RECT {
        left,
        top: y,
        right,
        bottom: y + h,
    };
    rounded(hdc, full, h, track);
    let width = right - left;
    let filled = ((percent / 100.0).clamp(0.0, 1.0) * width as f64) as i32;
    if filled > 0 {
        rounded(
            hdc,
            RECT {
                right: left + filled.max(h),
                ..full
            },
            h,
            fill,
        );
    }
}

/// Draw a single line sitting on `baseline`.
///
/// Positioned from the font's own ascent rather than by centring in a box.
/// Centring is what the first version did, with a hand-tuned nudge per font
/// size to make a 34-pixel number look level with a 13-pixel label — and it
/// visibly did not, because the correction depends on metrics that change with
/// the font and the DPI. Asking the font where its baseline is costs one call
/// and is right everywhere.
fn text_baseline(
    hdc: HDC,
    s: &str,
    left: i32,
    right: i32,
    baseline: i32,
    align: DRAW_TEXT_FORMAT,
) {
    let mut tm = TEXTMETRICW::default();
    unsafe {
        let _ = GetTextMetricsW(hdc, &mut tm);
    }
    let mut rect = RECT {
        left,
        top: baseline - tm.tmAscent,
        right,
        bottom: baseline + tm.tmDescent,
    };
    let mut buf = wide(s);
    // `wide` appends a terminator; DrawTextW counts it as a character and
    // renders a box for it.
    buf.pop();
    unsafe {
        DrawTextW(
            hdc,
            &mut buf,
            &mut rect,
            align | DT_SINGLELINE | DT_TOP | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }
}

fn text_width(hdc: HDC, s: &str) -> i32 {
    let mut buf = wide(s);
    buf.pop();
    let mut size = windows::Win32::Foundation::SIZE::default();
    unsafe {
        let _ = GetTextExtentPoint32W(hdc, &buf, &mut size);
    }
    size.cx
}

/// A filled dot, used to mark the limit the server says is currently being
/// drawn down. A word would need explaining; a dot next to a label reads as
/// "this one is live" without any.
fn dot(hdc: HDC, cx: i32, cy: i32, d: i32, colour: COLORREF) {
    unsafe {
        let b = brush(colour);
        let old_brush = SelectObject(hdc, b.0.into());
        let pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let r = d / 2;
        let _ = Ellipse(hdc, cx - r, cy - r, cx + r + 1, cy + r + 1);
        SelectObject(hdc, pen);
        SelectObject(hdc, old_brush);
    }
}

fn rounded(hdc: HDC, r: RECT, diameter: i32, colour: COLORREF) {
    unsafe {
        let b = brush(colour);
        let old_brush = SelectObject(hdc, b.0.into());
        let pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        // RoundRect's right/bottom are exclusive; without the +1 the bar is a
        // pixel short at every size.
        let _ = RoundRect(hdc, r.left, r.top, r.right + 1, r.bottom + 1, diameter, diameter);
        SelectObject(hdc, pen);
        SelectObject(hdc, old_brush);
    }
}

/// Draw the Claude mark, compositing its alpha against the panel background.
///
/// Done by hand rather than with `AlphaBlend` because the mark is the only
/// alpha source on the panel and the blend is one multiply per pixel — pulling
/// in msimg32 and a premultiplied DIB to avoid fifteen lines of arithmetic
/// would be the wrong kind of thorough.
fn blit_mark(hdc: HDC, rgba: &[u8], edge: u32, x: i32, y: i32, bg: COLORREF) {
    let (br, bg_, bb) = (
        (bg.0 & 0xff) as u32,
        ((bg.0 >> 8) & 0xff) as u32,
        ((bg.0 >> 16) & 0xff) as u32,
    );
    for py in 0..edge {
        for px in 0..edge {
            let i = ((py * edge + px) * 4) as usize;
            let a = rgba[i + 3] as u32;
            if a == 0 {
                continue;
            }
            let mix = |fg: u32, bgc: u32| (fg * a + bgc * (255 - a)) / 255;
            let c = rgb(
                mix(rgba[i] as u32, br) as u8,
                mix(rgba[i + 1] as u32, bg_) as u8,
                mix(rgba[i + 2] as u32, bb) as u8,
            );
            unsafe {
                SetPixel(hdc, x + px as i32, y + py as i32, c);
            }
        }
    }
}

/// Reported to the log on creation failure, where there is no panel to show it
/// in and the tray must carry on regardless.
pub fn report_failure(e: &str) {
    lerror!("panel unavailable: {e}");
}

/// Draw the panel straight into memory and hand back its pixels.
///
/// This exists because the obvious way to get a picture of the panel — put it
/// on screen and screenshot it — cannot produce a clean one. A real window sits
/// over a real desktop, DWM draws a soft shadow around it, and `GetWindowRect`
/// hands back a rectangle that includes that shadow margin. Whatever was behind
/// the window bleeds through it, so the crop arrives with faint rectangles of
/// other applications ghosted around the edges. Masking the corners hides some
/// of it and none of the rest.
///
/// Drawing into a DIB section removes the screen from the problem entirely: the
/// same `draw` that paints the live panel, onto a surface nothing else has
/// touched. It is also reproducible, and renders at any scale — 3x for the
/// website costs nothing where a screenshot is stuck at whatever DPI the
/// machine happens to run.
///
/// Returns `(width, height, rgba)`.
pub fn render_to_rgba(model: PanelModel, dark: bool, scale: i32) -> Option<(u32, u32, Vec<u8>)> {
    let scale = scale.max(1);
    let w = WIDTH * scale;
    let h = model.height() * scale;

    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let edge = (MARK * scale).max(8) as u32;
        s.mark = icon::claude_mark(edge);
        s.mark_edge = edge;
        s.dark = dark;
        s.model = model;
    });

    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative: top-down, so row 0 is the top and the buffer can be
                // walked in the same order as every other image in this app.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(mem, dib.into());

        draw(
            mem,
            RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            },
        );
        // GDI text and fills are drawn through the DC without touching the DIB's
        // alpha byte, which starts at zero. The panel is opaque everywhere the
        // corner mask does not cut, so alpha is restored wholesale below.
        let _ = GdiFlush();

        let len = (w * h * 4) as usize;
        let src = std::slice::from_raw_parts(bits as *const u8, len);
        let mut out = vec![0u8; len];
        for i in (0..len).step_by(4) {
            out[i] = src[i + 2]; // B G R A -> R G B A
            out[i + 1] = src[i + 1];
            out[i + 2] = src[i];
            out[i + 3] = 255;
        }

        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        round_corners(&mut out, w as u32, h as u32, 8.0 * scale as f64);
        Some((w as u32, h as u32, out))
    }
}

/// Cut the corners to transparent, matching the radius DWM rounds a popup with.
/// Supersampled, so the curve does not arrive as a staircase.
fn round_corners(rgba: &mut [u8], w: u32, h: u32, radius: f64) {
    const SS: u32 = 4;
    let (fw, fh) = (w as f64, h as f64);
    for y in 0..h {
        for x in 0..w {
            let mut covered = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f64 + (sx as f64 + 0.5) / SS as f64;
                    let fy = y as f64 + (sy as f64 + 0.5) / SS as f64;
                    let dx = (radius - fx).max(fx - (fw - radius)).max(0.0);
                    let dy = (radius - fy).max(fy - (fh - radius)).max(0.0);
                    if (dx * dx + dy * dy).sqrt() <= radius {
                        covered += 1;
                    }
                }
            }
            let total = SS * SS;
            if covered < total {
                let i = ((y * w + x) * 4 + 3) as usize;
                rgba[i] = (rgba[i] as u32 * covered / total) as u8;
            }
        }
    }
}

/// The content the website and the preview both show. One definition, so the
/// picture on the page is the same thing `-Panel` puts on screen.
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

/// Show the panel with representative content and pump messages until the
/// timeout, so the rendering can actually be looked at.
///
/// The counterpart to `--dump-icons`. A tray app's UI is otherwise only visible
/// by hand, at the moment you click it, which is a poor way to notice that a
/// bar is a pixel short or a label is clipped.
pub fn preview(dark: bool, seconds: u64) {
    let model = sample_model();

    let Ok(panel) = Panel::new() else { return };
    STATE.with(|s| s.borrow_mut().pinned = true);
    panel.show(model, dark);

    // A plain message pump: there is no tao event loop in preview mode.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    unsafe {
        let mut msg = MSG::default();
        while std::time::Instant::now() < deadline {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
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
    fn colorref_is_byte_reversed() {
        // 0x00BBGGRR. Getting this backwards silently swaps red and blue, which
        // on a severity palette means a full bucket renders green.
        assert_eq!(rgb(0xf8, 0x51, 0x49).0, 0x0049_51f8);
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
        // Clicking the tray icon while the panel is open pulls focus off it, so
        // WM_ACTIVATE hides it before the click event arrives. Without this the
        // toggle sees a hidden panel and reopens the thing being dismissed.
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

    fn area(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: r, bottom: b }
    }

    #[test]
    fn the_panel_sits_inside_the_work_area() {
        let work = area(0, 0, 1920, 1040);
        let (x, y) = place_in(POINT { x: 1890, y: 1035 }, 300, 400, work);
        assert!(x >= 0 && x + 300 <= 1920, "x={x} overflows the work area");
        assert!(y >= 0 && y + 400 <= 1040, "y={y} overflows the work area");
    }

    #[test]
    fn it_drops_below_the_cursor_when_the_taskbar_is_at_the_top() {
        let work = area(0, 48, 1920, 1080);
        let (_, y) = place_in(POINT { x: 960, y: 60 }, 300, 400, work);
        assert!(y > 60, "no room above, so it must open downwards");
    }

    #[test]
    fn a_work_area_smaller_than_the_panel_does_not_panic() {
        // i32::clamp panics when min > max, and `right - w - EDGE_GAP` goes
        // below `left + EDGE_GAP` on any display narrower than the panel. With
        // panic = "abort" in the release profile that took the whole tray app
        // down, so the icon would simply vanish on a small enough screen or a
        // large enough DPI scale.
        for (w, h) in [(300, 400), (900, 1200), (4000, 4000)] {
            let tiny = area(0, 0, 320, 240);
            let (x, y) = place_in(POINT { x: 160, y: 200 }, w, h, tiny);
            assert!(x >= 0 && y >= 0, "{w}x{h} placed at {x},{y}");
        }
    }

    #[test]
    fn a_negative_origin_monitor_is_handled() {
        // Second monitor to the left of the primary: coordinates go negative.
        let work = area(-1920, -200, 0, 880);
        let (x, y) = place_in(POINT { x: -100, y: 870 }, 300, 400, work);
        assert!(x >= -1920 && x + 300 <= 0, "x={x}");
        assert!(y >= -200 && y + 400 <= 880, "y={y}");
    }
}
