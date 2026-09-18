//! The Windows panel: a borderless popup, painted with GDI.
//!
//! Painted with plain GDI rather than a WebView. This is five rows of text and
//! three rectangles; a browser engine and ~80 MB resident to draw them would be
//! an absurd trade in an app whose binary size is a stated goal.
//!
//! Everything here runs on the UI thread — the window is created on it, painted
//! on it, and its messages are pumped by tao's event loop — so the model lives
//! in a `thread_local` rather than behind a lock.

use super::*;
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

// ---------------------------------------------------------------------------
// Palette, in the COLORREF the GDI calls want
// ---------------------------------------------------------------------------

/// COLORREF is 0x00BBGGRR — the reverse of how every colour in this codebase is
/// written, which is a fine way to spend an afternoon if you forget.
const fn cr(c: Rgb) -> COLORREF {
    COLORREF((c.0 as u32) | ((c.1 as u32) << 8) | ((c.2 as u32) << 16))
}

struct WinPalette {
    bg: COLORREF,
    border: COLORREF,
    text: COLORREF,
    secondary: COLORREF,
    tertiary: COLORREF,
    track: COLORREF,
    rule: COLORREF,
}

fn win_palette(dark: bool) -> WinPalette {
    let p = Palette::for_theme(dark);
    WinPalette {
        bg: cr(p.bg),
        border: cr(p.border),
        text: cr(p.text),
        secondary: cr(p.secondary),
        tertiary: cr(p.tertiary),
        track: cr(p.track),
        rule: cr(p.rule),
    }
}

fn severity_colour(s: Severity) -> COLORREF {
    cr(severity_rgb(s))
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
        let p = win_palette(state.dark);
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
#[allow(clippy::too_many_arguments)]
fn sub_line(
    hdc: HDC,
    p: &WinPalette,
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
/// drawn down.
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
            let c = cr(Rgb(
                mix(rgba[i] as u32, br) as u8,
                mix(rgba[i + 1] as u32, bg_) as u8,
                mix(rgba[i + 2] as u32, bb) as u8,
            ));
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
/// Drawing into a DIB section removes the screen from the problem entirely: the
/// same `draw` that paints the live panel, onto a surface nothing else has
/// touched. Returns `(width, height, rgba)`.
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

        round_corners(&mut out, w as u32, h as u32, CORNER as f64 * scale as f64);
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

/// Show the panel with representative content and pump messages until the
/// timeout, so the rendering can actually be looked at.
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

    #[test]
    fn colorref_is_byte_reversed() {
        // 0x00BBGGRR. Getting this backwards silently swaps red and blue, which
        // on a severity palette means a full bucket renders green.
        assert_eq!(cr(Rgb(0xf8, 0x51, 0x49)).0, 0x0049_51f8);
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
        for (w, h) in [(300, 400), (900, 1200), (4000, 4000)] {
            let tiny = area(0, 0, 320, 240);
            let (x, y) = place_in(POINT { x: 160, y: 200 }, w, h, tiny);
            assert!(x >= 0 && y >= 0, "{w}x{h} placed at {x},{y}");
        }
    }

    #[test]
    fn a_negative_origin_monitor_is_handled() {
        let work = area(-1920, -200, 0, 880);
        let (x, y) = place_in(POINT { x: -100, y: 870 }, 300, 400, work);
        assert!(x >= -1920 && x + 300 <= 0, "x={x}");
        assert!(y >= -200 && y + 400 <= 880, "y={y}");
    }
}
