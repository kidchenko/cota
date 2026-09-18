//! The macOS panel: a borderless popup, painted with AppKit.
//!
//! The counterpart to the Windows GDI panel. Same layout, same palette, drawn
//! with `NSBezierPath` and `NSString` drawing into a flipped `NSView` rather
//! than with GDI — Core Graphics under the hood either way, but AppKit's string
//! drawing spares us the Core Text plumbing for what is five rows of text.
//!
//! Everything here runs on the main thread — AppKit demands it — so the model
//! lives in a `thread_local` rather than behind a lock, exactly as on Windows.

use super::*;
use crate::icon;
use crate::log::{ldebug, lerror};

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSBitmapFormat, NSBitmapImageRep, NSColor,
    NSCompositingOperation, NSDeviceRGBColorSpace, NSEvent, NSEventMask, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSImage,
    NSLineBreakMode, NSMutableParagraphStyle, NSPanel, NSParagraphStyleAttributeName, NSScreen,
    NSTextAlignment, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSMutableDictionary, NSPoint, NSRect,
    NSRunLoop, NSSize, NSString,
};

// AppKit's `NSStatusWindowLevel`: above normal and floating windows, level with
// menu-bar extras' own popups. Not bound as a constant, so it is spelled out.
const STATUS_WINDOW_LEVEL: isize = 25;
const EDGE_GAP: i32 = 8;

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
}

thread_local! {
    static STATE: RefCell<PanelState> = RefCell::new(PanelState {
        model: PanelModel::default(),
        dark: true,
        pinned: false,
        hidden_at: None,
    });
    /// Pixels-per-point to render the Claude mark bitmap at. `None` means "ask
    /// the window its backing scale"; set to the supersample factor while
    /// `render_to_rgba` is drawing offscreen, where there is no window to ask.
    static RENDER_SCALE: Cell<Option<f64>> = const { Cell::new(None) };
}

fn note_hidden() {
    STATE.with(|s| s.borrow_mut().hidden_at = Some(Instant::now()));
}

fn since_hidden() -> Option<std::time::Duration> {
    STATE.with(|s| s.borrow().hidden_at.map(|t| t.elapsed()))
}

fn pinned() -> bool {
    STATE.with(|s| s.borrow().pinned)
}

// ---------------------------------------------------------------------------
// The content view: flipped, so layout's top-down y matches, and self-drawing
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CotaPanelView"]
    struct PanelView;

    impl PanelView {
        // Top-left origin, y down — the coordinate system `layout()` computes in.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // React to the very first click even when the app is not frontmost — a
        // menu-bar popup is clicked from wherever the user already was.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: *mut NSEvent) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let scale = RENDER_SCALE
                .with(|r| r.get())
                .or_else(|| self.window().map(|w| w.backingScaleFactor()))
                .unwrap_or(2.0);
            STATE.with(|s| {
                let s = s.borrow();
                draw_panel(&s.model, s.dark, scale);
            });
        }

        // Clicking the panel dismisses it: there is nothing in it to click, and
        // a readout that will not go away is worse than one that goes too easily.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: *mut NSEvent) {
            if pinned() {
                return;
            }
            if let Some(w) = self.window() {
                w.orderOut(None);
            }
            note_hidden();
        }
    }
);

impl PanelView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm);
        unsafe { msg_send![this, initWithFrame: frame] }
    }
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

pub struct Panel {
    window: Retained<NSPanel>,
    view: Retained<PanelView>,
    /// The global click monitor, kept alive for the life of the panel; dropping
    /// it removes the monitor.
    _monitor: Option<Retained<AnyObject>>,
    mtm: MainThreadMarker,
}

impl Panel {
    pub fn new() -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("panel must be created on the main thread")?;

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH as f64, 200.0));
        let view = PanelView::new(mtm, frame);

        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let window: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );

        unsafe {
            window.setLevel(STATUS_WINDOW_LEVEL);
            window.setOpaque(false);
            window.setBackgroundColor(Some(&NSColor::clearColor()));
            window.setHasShadow(true);
            window.setHidesOnDeactivate(false);
            window.setReleasedWhenClosed(false);
            window.setContentView(Some(&view));
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
        }

        // Click-away: a mouse-down in any other application hides the panel. The
        // WM_ACTIVATE analogue; the shared reopen-guard covers the race with a
        // click on the status item itself.
        let win_for_block = window.clone();
        let handler = RcBlock::new(move |_event: NonNull<NSEvent>| {
            if pinned() {
                return;
            }
            if win_for_block.isVisible() {
                win_for_block.orderOut(None);
            }
            note_hidden();
        });
        let monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
            &handler,
        );

        Ok(Self {
            window,
            view,
            _monitor: monitor,
            mtm,
        })
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
        note_hidden();
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

    /// Replace the contents of an already-open panel, leaving it where it is —
    /// anchored under the menu bar, so a resize grows downward rather than
    /// snatching the position back once a minute while the user reads it.
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
        let after = STATE.with(|s| s.borrow().model.height());
        if after != before {
            let old = self.window.frame();
            let top = old.origin.y + old.size.height;
            let h = after as f64;
            let new = NSRect::new(
                NSPoint::new(old.origin.x, top - h),
                NSSize::new(WIDTH as f64, h),
            );
            self.window.setFrame_display(new, true);
        }
        self.view.setNeedsDisplay(true);
        self.window.invalidateShadow();
    }

    /// Show the panel just under the menu bar, horizontally near the cursor —
    /// which is over the status item the user has just clicked.
    pub fn show(&self, model: PanelModel, dark: bool) {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.dark = dark;
            s.model = model;
        });
        let h = STATE.with(|s| s.borrow().model.height()) as f64;
        let w = WIDTH as f64;

        let mouse = NSEvent::mouseLocation();
        let visible = self.visible_frame_under(mouse);
        let origin = place(mouse, w, h, visible);

        self.window
            .setFrame_display(NSRect::new(origin, NSSize::new(w, h)), true);
        self.view.setNeedsDisplay(true);
        self.window.orderFrontRegardless();
        self.window.invalidateShadow();
        ldebug!("panel shown at {},{} ({w}x{h})", origin.x, origin.y);
    }

    fn visible_frame_under(&self, mouse: NSPoint) -> NSRect {
        let screens = NSScreen::screens(self.mtm);
        for i in 0..screens.count() {
            let s = screens.objectAtIndex(i);
            let f = s.frame();
            if mouse.x >= f.origin.x
                && mouse.x <= f.origin.x + f.size.width
                && mouse.y >= f.origin.y
                && mouse.y <= f.origin.y + f.size.height
            {
                return s.visibleFrame();
            }
        }
        NSScreen::mainScreen(self.mtm)
            .map(|s| s.visibleFrame())
            .unwrap_or(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(1440.0, 900.0),
            ))
    }
}

/// Place the panel's bottom-left origin (macOS screen coordinates, y up) so its
/// top sits just under the menu bar and it is centred on the cursor, kept inside
/// the visible frame. Separated from the screen query so it can be tested.
fn place(mouse: NSPoint, w: f64, h: f64, visible: NSRect) -> NSPoint {
    let gap = EDGE_GAP as f64;
    let min_x = visible.origin.x + gap;
    let max_x = (visible.origin.x + visible.size.width - w - gap).max(min_x);
    let x = (mouse.x - w / 2.0).clamp(min_x, max_x);

    // `visibleFrame` already excludes the menu bar, so its top edge is where the
    // panel wants to hang from.
    let top = visible.origin.y + visible.size.height - gap;
    let min_y = visible.origin.y + gap;
    let y = (top - h).max(min_y);
    NSPoint::new(x, y)
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.window.close();
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw_panel(model: &PanelModel, dark: bool, pixel_scale: f64) {
    let l = model.layout(&|v| v);
    let pal = Palette::for_theme(dark);
    let w = WIDTH as f64;
    let h = model.height() as f64;

    let pad = PAD as f64;
    let left = pad;
    let right = w - pad;

    // Background rounded rect + hairline border, inset half a point so the
    // stroke lands inside the bitmap rather than half-off its edge.
    let bg_rect = NSRect::new(NSPoint::new(0.5, 0.5), NSSize::new(w - 1.0, h - 1.0));
    let bg = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        bg_rect,
        CORNER as f64,
        CORNER as f64,
    );
    color(pal.bg).setFill();
    bg.fill();
    color(pal.border).setStroke();
    bg.setLineWidth(1.0);
    bg.stroke();

    // -- header: quiet chrome, not content --------------------------------
    let f_head = sysfont(12.0, 0.0);
    let ascent = f_head.ascender();
    let mark_edge = MARK as f64;
    let mark_top = l.header_baseline as f64 - ascent / 2.0 - mark_edge / 2.0;
    draw_mark(pixel_scale, left, mark_top);
    let header_x = left + MARK as f64 + 9.0;
    draw_text(
        &model.header,
        header_x,
        l.header_baseline as f64,
        right - header_x,
        &f_head,
        pal.tertiary,
        Align::Left,
        true,
    );

    // -- status instead of limits -----------------------------------------
    if let (Some(status), Some(baseline)) = (&model.status, l.status_baseline) {
        let f = sysfont(13.0, 0.0);
        draw_text(
            status,
            left,
            baseline as f64,
            right - left,
            &f,
            pal.secondary,
            Align::Left,
            true,
        );
        return;
    }

    // -- hero: the limit nearest its ceiling ------------------------------
    if let (Some(hero), Some(row)) = (&l.hero, model.hero()) {
        let sev = severity_rgb(row.severity);
        let f = sysfont(HERO_NUM as f64, 0.3);
        draw_text(
            &format!("{:.0}%", row.percent),
            left,
            hero.baseline as f64,
            right - left,
            &f,
            sev,
            Align::Left,
            false,
        );
        bar(left, right, hero.bar_y as f64, HERO_BAR_H as f64, row.percent, sev, pal.track);
        let fs = sysfont(12.0, 0.0);
        sub_line(row, left, right, hero.sub_baseline as f64, &fs, &pal, sev);
    }

    // -- the rest, compact ------------------------------------------------
    let mut rules = l.rules.iter();
    if !l.rows.is_empty() {
        if let Some(&y) = rules.next() {
            rule(left, right, y as f64, pal.rule);
        }
    }
    for (box_, row) in l.rows.iter().zip(model.rest().into_iter()) {
        let sev = severity_rgb(row.severity);
        let f = sysfont(13.0, 0.3);
        draw_text(
            &format!("{:.0}%", row.percent),
            left,
            box_.baseline as f64,
            right - left,
            &f,
            pal.text,
            Align::Right,
            false,
        );
        let fs = sysfont(12.0, 0.0);
        sub_line(row, left, right, box_.baseline as f64, &fs, &pal, sev);
        bar(left, right, box_.bar_y as f64, ROW_BAR_H as f64, row.percent, sev, pal.track);
    }

    // -- notes ------------------------------------------------------------
    if !l.notes.is_empty() {
        if let Some(&y) = rules.next() {
            rule(left, right, y as f64, pal.rule);
        }
        let accent = model
            .hero()
            .map(|h| severity_rgb(h.severity))
            .unwrap_or(pal.tertiary);
        let f = sysfont(12.0, 0.0);
        let asc = f.ascender();
        for (i, (note, &baseline)) in model.notes.iter().zip(l.notes.iter()).enumerate() {
            // The first note is the projection — what happens next — so it gets
            // the accent and the brighter ink. The rest are context.
            let (dot_rgb, ink) = if i == 0 {
                (accent, pal.secondary)
            } else {
                (pal.tertiary, pal.tertiary)
            };
            let cy = baseline as f64 - asc / 2.0 + 1.0;
            dot(left + DOT as f64 / 2.0, cy, DOT as f64, dot_rgb);
            let x = left + DOT as f64 + 10.0;
            draw_text(note, x, baseline as f64, right - x, &f, ink, Align::Left, true);
        }
    }
}

/// `Weekly  ·  resets in 2d 16h`, label brighter than the time, with an optional
/// live dot between them.
fn sub_line(
    row: &PanelRow,
    left: f64,
    right: f64,
    baseline: f64,
    font: &NSFont,
    pal: &Palette,
    accent: Rgb,
) {
    draw_text(&row.label, left, baseline, right - left, font, pal.secondary, Align::Left, false);

    let mut x = left + text_width(&row.label, font);
    if row.active {
        let asc = font.ascender();
        let gap = 7.0;
        dot(x + gap, baseline - asc / 2.0 + 1.0, DOT as f64, accent);
        x += gap * 2.0;
    }
    // The dot already separates label from time; a middot as well would be
    // punctuation stacked on punctuation.
    let tail = if row.active {
        format!("   {}", row.resets)
    } else {
        format!("  \u{00b7}  {}", row.resets)
    };
    draw_text(&tail, x, baseline, right - x, font, pal.tertiary, Align::Left, true);
}

fn rule(left: f64, right: f64, y: f64, rgb: Rgb) {
    fill_rect(NSRect::new(NSPoint::new(left, y), NSSize::new(right - left, 1.0)), rgb);
}

/// Track plus fill. The fill is floored at its own height so a 1% reading is a
/// dot rather than a sliver clipped to nothing by the corner radius.
fn bar(left: f64, right: f64, y: f64, h: f64, percent: f64, fill: Rgb, track: Rgb) {
    let full = NSRect::new(NSPoint::new(left, y), NSSize::new(right - left, h));
    fill_rounded(full, h / 2.0, track);
    let width = right - left;
    let filled = (percent / 100.0).clamp(0.0, 1.0) * width;
    if filled > 0.0 {
        let filled = filled.max(h);
        fill_rounded(NSRect::new(NSPoint::new(left, y), NSSize::new(filled, h)), h / 2.0, fill);
    }
}

fn dot(cx: f64, cy: f64, d: f64, rgb: Rgb) {
    let r = d / 2.0;
    let rect = NSRect::new(NSPoint::new(cx - r, cy - r), NSSize::new(d, d));
    let path = NSBezierPath::bezierPathWithOvalInRect(rect);
    color(rgb).setFill();
    { path.fill() };
}

fn fill_rect(rect: NSRect, rgb: Rgb) {
    let path = NSBezierPath::bezierPathWithRect(rect);
    color(rgb).setFill();
    { path.fill() };
}

fn fill_rounded(rect: NSRect, radius: f64, rgb: Rgb) {
    let path =
        { NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius) };
    color(rgb).setFill();
    { path.fill() };
}

fn draw_mark(pixel_scale: f64, x: f64, top: f64) {
    let px = ((MARK as f64) * pixel_scale).round().max(8.0) as u32;
    let rgba = icon::claude_mark(px);
    let Some(rep) = rep_from_rgba(&rgba, px, px) else {
        return;
    };
    let img = NSImage::initWithSize(NSImage::alloc(), NSSize::new(MARK as f64, MARK as f64));
    img.addRepresentation(&rep);
    let dst = NSRect::new(NSPoint::new(x, top), NSSize::new(MARK as f64, MARK as f64));
    img.drawInRect_fromRect_operation_fraction(
        dst,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
        NSCompositingOperation::SourceOver,
        1.0,
    );
}

// ---------------------------------------------------------------------------
// Small AppKit helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

fn color(c: Rgb) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        c.0 as f64 / 255.0,
        c.1 as f64 / 255.0,
        c.2 as f64 / 255.0,
        1.0,
    )
}

fn sysfont(size: f64, weight: f64) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, weight)
}

/// The attribute dictionary a draw/measure call wants. Built with raw
/// `msg_send!` to sidestep the generic `NSCopying` key typing, which is a lot of
/// ceremony for three fixed keys.
fn text_attrs(font: &NSFont, rgb: Rgb, align: Align, truncate: bool) -> Retained<NSMutableDictionary> {
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(match align {
        Align::Left => NSTextAlignment::Left,
        Align::Right => NSTextAlignment::Right,
    });
    if truncate {
        para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    }
    let col = color(rgb);
    let attrs: Retained<NSMutableDictionary> = NSMutableDictionary::new();
    unsafe {
        let _: () = msg_send![&attrs, setObject: font, forKey: NSFontAttributeName];
        let _: () = msg_send![&attrs, setObject: &*col, forKey: NSForegroundColorAttributeName];
        let _: () = msg_send![&attrs, setObject: &*para, forKey: NSParagraphStyleAttributeName];
    }
    attrs
}

fn text_width(s: &str, font: &NSFont) -> f64 {
    let ns = NSString::from_str(s);
    let attrs = text_attrs(font, Rgb(0, 0, 0), Align::Left, false);
    let size: NSSize = unsafe { msg_send![&ns, sizeWithAttributes: &*attrs] };
    size.width
}

/// Draw one line sitting on `baseline`. Positioned from the font's own ascent —
/// asking the font where its baseline is beats centring in a box with a per-size
/// nudge, which is a correction that changes with the font and the scale.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    s: &str,
    x: f64,
    baseline: f64,
    width: f64,
    font: &NSFont,
    rgb: Rgb,
    align: Align,
    truncate: bool,
) {
    let ns = NSString::from_str(s);
    let attrs = text_attrs(font, rgb, align, truncate);
    let ascent = font.ascender();
    let descent = font.descender(); // negative
    // With the rect top at baseline - ascent, drawInRect lays the single line so
    // its baseline lands exactly on `baseline`; the width carries alignment and
    // tail-truncation.
    let rect = NSRect::new(
        NSPoint::new(x, baseline - ascent),
        NSSize::new(width.max(0.0), ascent - descent + 2.0),
    );
    unsafe {
        let _: () = msg_send![&ns, drawInRect: rect, withAttributes: &*attrs];
    }
}

/// An `NSBitmapImageRep` holding straight-alpha RGBA, its buffer filled from
/// `rgba`. Straight (non-premultiplied) so the mark's constant colour survives
/// its soft edges rather than darkening toward black.
fn rep_from_rgba(rgba: &[u8], w: u32, h: u32) -> Option<Retained<NSBitmapImageRep>> {
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w as isize,
            h as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            (w * 4) as isize,
            32,
        )
    }?;
    unsafe {
        let dst = rep.bitmapData();
        if dst.is_null() {
            return None;
        }
        let stride = rep.bytesPerRow() as usize;
        let row_bytes = (w * 4) as usize;
        for row in 0..h as usize {
            let src = &rgba[row * row_bytes..row * row_bytes + row_bytes];
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(row * stride), row_bytes);
        }
    }
    Some(rep)
}

// ---------------------------------------------------------------------------
// Offscreen render + preview (the counterparts to Windows' DIB path)
// ---------------------------------------------------------------------------

/// Reported to the log on creation failure, where there is no panel to show it
/// in and the tray must carry on regardless.
pub fn report_failure(e: &str) {
    lerror!("panel unavailable: {e}");
}

/// Draw the panel into a bitmap and hand back its pixels, without ever putting a
/// window on screen. Reuses the very `drawRect:` that paints the live panel — so
/// text and layout are whatever the real thing would show — by rendering a
/// detached view into a bitmap context. Returns `(width, height, rgba)`.
pub fn render_to_rgba(model: PanelModel, dark: bool, scale: i32) -> Option<(u32, u32, Vec<u8>)> {
    let scale = scale.max(1);
    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => {
            eprintln!("render_to_rgba: not on main thread");
            return None;
        }
    };
    // AppKit's offscreen drawing needs the graphics environment bootstrapped.
    let _ = objc2_app_kit::NSApplication::sharedApplication(mtm);

    let hp = model.height();
    let (wp, hp) = (WIDTH, hp);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.dark = dark;
        s.model = model;
    });

    let (pw, ph) = ((wp * scale) as u32, (hp * scale) as u32);
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(wp as f64, hp as f64));
    let view = PanelView::new(mtm, bounds);

    // A rep sized in pixels but told it measures `wp x hp` points, so the
    // context it backs draws at `scale` density — crisp, not upscaled.
    let rep = unsafe {
        let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            pw as isize,
            ph as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            // Premultiplied (empty flags): a CoreGraphics bitmap context cannot
            // be backed by a non-premultiplied rep, so the drawing target must
            // be premultiplied and un-premultiplied on the way out below.
            NSBitmapFormat::empty(),
            (pw * 4) as isize,
            32,
        );
        let rep = match rep {
            Some(r) => r,
            None => {
                eprintln!("render_to_rgba: NSBitmapImageRep init failed");
                return None;
            }
        };
        rep.setSize(NSSize::new(wp as f64, hp as f64));
        rep
    };

    let ctx = match NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep) {
        Some(c) => c,
        None => {
            eprintln!("render_to_rgba: graphicsContextWithBitmapImageRep returned nil");
            return None;
        }
    };
    RENDER_SCALE.with(|r| r.set(Some(scale as f64)));
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    view.displayRectIgnoringOpacity_inContext(bounds, &ctx);
    NSGraphicsContext::restoreGraphicsState_class();
    RENDER_SCALE.with(|r| r.set(None));

    // Copy the pixels out, dropping any row padding AppKit added.
    let out = unsafe {
        let data = rep.bitmapData();
        if data.is_null() {
            return None;
        }
        let stride = rep.bytesPerRow() as usize;
        let row_bytes = (pw * 4) as usize;
        let mut out = vec![0u8; (pw * ph * 4) as usize];
        for row in 0..ph as usize {
            let src = std::slice::from_raw_parts(data.add(row * stride), row_bytes);
            let dst = &mut out[row * row_bytes..row * row_bytes + row_bytes];
            // Un-premultiply back to straight alpha, matching the Windows path's
            // output contract (opaque interior, transparent outside the corners).
            for px in 0..pw as usize {
                let i = px * 4;
                let a = src[i + 3] as u32;
                if a == 0 {
                    continue;
                }
                for c in 0..3 {
                    dst[i + c] = ((src[i + c] as u32 * 255 + a / 2) / a).min(255) as u8;
                }
                dst[i + 3] = a as u8;
            }
        }
        out
    };

    Some((pw, ph, out))
}

/// Show the panel with representative content and pump the run loop until the
/// timeout, so the rendering can actually be looked at. The counterpart to
/// `--dump-icons`; there is no tao event loop in preview mode, so this stands up
/// a minimal `NSApplication` of its own.
pub fn preview(dark: bool, seconds: u64) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);

    let Ok(panel) = Panel::new() else { return };
    STATE.with(|s| s.borrow_mut().pinned = true);
    panel.show(sample_model(), dark);

    let deadline = Instant::now() + std::time::Duration::from_secs(seconds);
    let run_loop = NSRunLoop::currentRunLoop();
    while Instant::now() < deadline {
        let until = NSDate::dateWithTimeIntervalSinceNow(0.05);
        unsafe {
            let _ = run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &until);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vis(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    #[test]
    fn the_panel_hangs_from_the_top_of_the_visible_frame() {
        // visibleFrame excludes the 25pt menu bar: a 1440x900 screen gives
        // height 875, top at y=875. The panel's top must sit just under it.
        let visible = vis(0.0, 0.0, 1440.0, 875.0);
        let origin = place(NSPoint::new(1200.0, 880.0), 300.0, 400.0, visible);
        let top = origin.y + 400.0;
        assert!(top <= 875.0, "panel top {top} pokes above the visible frame");
        assert!(top >= 875.0 - 20.0, "panel should hang near the top, got {top}");
    }

    #[test]
    fn the_panel_stays_inside_the_visible_frame() {
        let visible = vis(0.0, 0.0, 1440.0, 875.0);
        let origin = place(NSPoint::new(1439.0, 880.0), 300.0, 400.0, visible);
        assert!(origin.x >= 0.0 && origin.x + 300.0 <= 1440.0, "x={}", origin.x);
        assert!(origin.y >= 0.0, "y={}", origin.y);
    }

    #[test]
    fn a_second_monitor_with_a_negative_origin_is_handled() {
        // A display to the left of the primary: coordinates go negative.
        let visible = vis(-1440.0, 0.0, 1440.0, 875.0);
        let origin = place(NSPoint::new(-700.0, 880.0), 300.0, 400.0, visible);
        assert!(origin.x >= -1440.0 && origin.x + 300.0 <= 0.0, "x={}", origin.x);
    }
}
