//! The tray icon: a ring that fills as the limit does.
//!
//! Drawn at runtime rather than shipped as a sprite sheet, because the icon is
//! a continuous readout — a hundred PNGs would be silly and a dozen coarse
//! steps would lose the thing being communicated. The whole renderer is a
//! nested loop over 32x32 pixels with 4x4 supersampling; it runs in well under
//! a millisecond and only when the number actually changes.
//!
//! No digits. At the 16x16 the notification area actually paints, text is a
//! smudge — the ring carries "how full", the colour carries "how worried", and
//! the tooltip carries the number.

use crate::usage::Severity;

pub const EDGE: u32 = 32;
/// 4x4 per pixel. Enough that the arc's leading edge does not stair-step at
/// the sizes Windows downscales to.
const SS: u32 = 4;

/// Ring geometry as fractions of the edge, so the same mark is drawn at the
/// 16 pixels the taskbar paints and at whatever size the website wants. Tuned
/// at tray size; everything else scales off it.
const R_OUTER_F: f64 = 15.0 / 32.0;
const R_INNER_F: f64 = 9.5 / 32.0;

type Rgba = [f64; 4];

const GREEN: Rgba = [0x3f as f64, 0xb9 as f64, 0x50 as f64, 255.0];
const AMBER: Rgba = [0xd2 as f64, 0x99 as f64, 0x22 as f64, 255.0];
const RED: Rgba = [0xf8 as f64, 0x51 as f64, 0x49 as f64, 255.0];
/// Used for both the paused and the "we do not know" faces. Deliberately not
/// red: a broken poll is not the same news as a full bucket, and colouring
/// them alike would train the user to ignore the one that matters.
const MUTED: Rgba = [0x8b as f64, 0x94 as f64, 0x9e as f64, 255.0];

/// What the icon should say right now.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Face {
    Meter { percent: f64, severity: Severity },
    /// Polling is switched off.
    Paused,
    /// We have no number: not signed in, offline, token rejected, schema moved.
    Unknown,
}

pub fn render(face: Face, dark_taskbar: bool) -> Vec<u8> {
    render_at(face, dark_taskbar, EDGE)
}

/// The ring at an arbitrary edge. The tray only ever needs [`EDGE`], but the
/// landing page wants the same art large and crisp, and re-drawing it is both
/// cheaper and sharper than upscaling a 32-pixel bitmap.
pub fn render_at(face: Face, dark_taskbar: bool, edge: u32) -> Vec<u8> {
    // The unfilled part of the ring. It has to read against the taskbar rather
    // than against the app, which is why `theme` asks about the taskbar
    // specifically.
    let track: Rgba = if dark_taskbar {
        [255.0, 255.0, 255.0, 56.0]
    } else {
        [0.0, 0.0, 0.0, 48.0]
    };

    let (fraction, fill) = match face {
        Face::Meter { percent, severity } => (
            (percent / 100.0).clamp(0.0, 1.0),
            match severity {
                Severity::Normal => GREEN,
                Severity::Warning => AMBER,
                Severity::Critical => RED,
            },
        ),
        // A full muted ring: "there is a meter here, it is just not running."
        Face::Paused => (0.0, MUTED),
        Face::Unknown => (0.0, MUTED),
    };

    let center = (edge as f64 - 1.0) / 2.0;
    let r_outer = edge as f64 * R_OUTER_F;
    let r_inner = edge as f64 * R_INNER_F;

    let mut out = vec![0u8; (edge * edge * 4) as usize];
    let samples = (SS * SS) as f64;

    for y in 0..edge {
        for x in 0..edge {
            // Premultiplied accumulation, so subsamples that land on nothing
            // darken the alpha without dragging the colour toward black.
            let (mut ar, mut ag, mut ab, mut aa) = (0.0, 0.0, 0.0, 0.0);

            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f64 + (sx as f64 + 0.5) / SS as f64 - 0.5;
                    let py = y as f64 + (sy as f64 + 0.5) / SS as f64 - 0.5;
                    let dx = px - center;
                    let dy = py - center;
                    let r = (dx * dx + dy * dy).sqrt();
                    if !(r_inner..=r_outer).contains(&r) {
                        continue;
                    }

                    let colour = match face {
                        Face::Unknown => {
                            // Dashed, so "no reading" is distinguishable from
                            // "zero" at a glance and without relying on colour
                            // alone. Four dashes and four gaps: at the 16x16
                            // the taskbar actually paints, anything finer stops
                            // reading as a dashed ring and starts reading as a
                            // damaged one.
                            if (angle_fraction(dx, dy) * 8.0) as i32 % 2 == 0 {
                                fill
                            } else {
                                continue;
                            }
                        }
                        Face::Paused => track,
                        Face::Meter { .. } => {
                            if angle_fraction(dx, dy) <= fraction {
                                fill
                            } else {
                                track
                            }
                        }
                    };

                    let a = colour[3];
                    ar += colour[0] * a;
                    ag += colour[1] * a;
                    ab += colour[2] * a;
                    aa += a;
                }
            }

            let i = ((y * edge + x) * 4) as usize;
            if aa > 0.0 {
                out[i] = (ar / aa).round().clamp(0.0, 255.0) as u8;
                out[i + 1] = (ag / aa).round().clamp(0.0, 255.0) as u8;
                out[i + 2] = (ab / aa).round().clamp(0.0, 255.0) as u8;
                out[i + 3] = (aa / samples).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// The Claude mark, drawn as a starburst.
///
/// It sits at the head of the tray menu so that a column of bare percentages
/// is self-identifying — "Weekly 24%" says nothing about *whose* weekly, and
/// this app is one of several things on a taskbar that could plausibly be
/// metering something.
///
/// Drawn rather than embedded for the same reason the ring is: it has to match
/// the menu's text colour, which depends on the theme, and shipping two PNGs
/// to avoid twenty lines of geometry would be the wrong trade in an app whose
/// binary size is a stated goal.
pub fn claude_mark(edge: u32) -> Vec<u8> {
    // Anthropic's terracotta. Legible against both a light and a dark menu
    // background, which is why the mark needs no theme variant.
    const CLAY: Rgba = [0xd9 as f64, 0x77 as f64, 0x57 as f64, 255.0];
    // Eleven, like the mark itself. An even count reads as a snowflake because
    // every spoke gains a partner directly opposite it.
    const SPOKES: usize = 11;

    let c = (edge as f64 - 1.0) / 2.0;
    let r_outer = edge as f64 * 0.48;
    let r_inner = r_outer * 0.10;
    // Tuned at 16x16, which is the size that actually ships: eleven spokes any
    // thicker than this merge into a disc with bumps on it long before they
    // reach the rim, and the mark stops reading as a starburst at all.
    let half_width = edge as f64 * 0.038;

    let mut out = vec![0u8; (edge * edge * 4) as usize];
    let samples = (SS * SS) as f64;

    for y in 0..edge {
        for x in 0..edge {
            let mut covered = 0.0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f64 + (sx as f64 + 0.5) / SS as f64 - 0.5 - c;
                    let py = y as f64 + (sy as f64 + 0.5) / SS as f64 - 0.5 - c;
                    for k in 0..SPOKES {
                        // Rotated half a step so no spoke sits dead vertical;
                        // the real mark is not axis-aligned either.
                        let theta = (k as f64 + 0.5) / SPOKES as f64 * std::f64::consts::TAU;
                        let (sin, cos) = theta.sin_cos();
                        // Project onto the spoke's axis, then measure off it.
                        let along = px * sin + py * -cos;
                        let across = px * cos + py * sin;
                        let along = along.clamp(r_inner, r_outer);
                        let dx = px - along * sin;
                        let dy = py + along * cos;
                        let _ = across;
                        if (dx * dx + dy * dy).sqrt() <= half_width {
                            covered += 1.0;
                            break;
                        }
                    }
                }
            }
            if covered > 0.0 {
                let i = ((y * edge + x) * 4) as usize;
                out[i] = CLAY[0] as u8;
                out[i + 1] = CLAY[1] as u8;
                out[i + 2] = CLAY[2] as u8;
                out[i + 3] = (255.0 * covered / samples).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Position around the ring as 0.0..1.0, starting at twelve o'clock and going
/// clockwise — the direction every dial the user has ever read goes.
fn angle_fraction(dx: f64, dy: f64) -> f64 {
    let mut theta = dx.atan2(-dy);
    if theta < 0.0 {
        theta += std::f64::consts::TAU;
    }
    theta / std::f64::consts::TAU
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(buf: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * EDGE + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn output_is_the_right_size() {
        let b = render(Face::Meter { percent: 50.0, severity: Severity::Normal }, true);
        assert_eq!(b.len() as u32, EDGE * EDGE * 4);
    }

    #[test]
    fn the_middle_is_transparent() {
        let b = render(Face::Meter { percent: 100.0, severity: Severity::Critical }, true);
        assert_eq!(pixel(&b, EDGE / 2, EDGE / 2)[3], 0, "the hole must stay a hole");
        assert_eq!(pixel(&b, 0, 0)[3], 0, "corners are outside the ring");
    }

    #[test]
    fn the_arc_starts_at_twelve_and_runs_clockwise() {
        // 30% rather than 25%: three o'clock sits exactly on the boundary at a
        // quarter, where which side of it a given pixel centre falls is a
        // rounding detail rather than the behaviour under test.
        let b = render(Face::Meter { percent: 30.0, severity: Severity::Normal }, true);
        let top = pixel(&b, EDGE / 2, 1);
        let right = pixel(&b, EDGE - 2, EDGE / 2);
        let bottom = pixel(&b, EDGE / 2, EDGE - 2);

        let greenish = |p: [u8; 4]| p[1] > p[0] && p[1] > p[2];
        assert!(greenish(top), "twelve o'clock should be filled, got {top:?}");
        assert!(greenish(right), "three o'clock should be filled, got {right:?}");
        assert!(!greenish(bottom), "six o'clock should be track, got {bottom:?}");
    }

    #[test]
    fn severity_picks_the_colour() {
        let red = render(Face::Meter { percent: 99.0, severity: Severity::Critical }, true);
        let p = pixel(&red, EDGE / 2, 1);
        assert!(p[0] > p[1] && p[0] > p[2], "critical should be red, got {p:?}");
    }

    #[test]
    fn the_track_follows_the_taskbar_theme() {
        let on_dark = render(Face::Meter { percent: 0.0, severity: Severity::Normal }, true);
        let on_light = render(Face::Meter { percent: 0.0, severity: Severity::Normal }, false);
        // Bottom of the ring is track in both; it must not be the same colour.
        assert_ne!(pixel(&on_dark, EDGE / 2, EDGE - 2), pixel(&on_light, EDGE / 2, EDGE - 2));
    }

    #[test]
    fn unknown_is_dashed_so_it_is_not_just_a_colour_difference() {
        let b = render(Face::Unknown, true);
        let ring: Vec<u8> = (0..EDGE).map(|x| pixel(&b, x, 1)[3]).collect();
        assert!(ring.iter().any(|&a| a > 0), "some of the ring should be drawn");
        assert!(ring.iter().any(|&a| a == 0), "and some of it should be gap");
    }
}
