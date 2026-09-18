//! RFC 3339 parsing and duration formatting, hand-rolled.
//!
//! `chrono` would do this in one line and cost a few hundred KB. Everything
//! this app needs is: turn one fixed-shape timestamp into epoch seconds, and
//! render a difference between two epochs as "2d 16h".
//!
//! Note what is deliberately absent: any notion of local time, calendar dates,
//! or weekday names. Every time this app shows the user is a *duration* —
//! "resets in 2d 16h", "cap in 3h 20m" — never a wall-clock instant. That is
//! the friendlier phrasing anyway, and it means no timezone database, no DST
//! edge cases, and no code that breaks twice a year.

/// Parse the subset of RFC 3339 the usage endpoint emits:
/// `2026-09-20T00:00:00.241353+00:00`, `...Z`, or with any `±HH:MM` offset.
///
/// Returns epoch seconds. Fractional seconds are parsed and discarded — we
/// render whole minutes at the finest.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Everything after the seconds is an optional fraction followed by an
    // optional zone. Skip the fraction by hand rather than reaching for a
    // regex crate.
    let mut rest = &s[19..];
    if rest.starts_with('.') {
        let digits = rest[1..]
            .as_bytes()
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        rest = &rest[1 + digits..];
    }

    let offset_secs: i64 = if rest.is_empty() || rest.starts_with('Z') || rest.starts_with('z') {
        0
    } else {
        let sign = match rest.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        // Accept both `+HH:MM` and `+HHMM`.
        let body: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if body.len() < 4 {
            return None;
        }
        let oh: i64 = body.get(0..2)?.parse().ok()?;
        let om: i64 = body.get(2..4)?.parse().ok()?;
        sign * (oh * 3600 + om * 60)
    };

    let epoch = days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec
        - offset_secs;
    (epoch >= 0).then_some(epoch as u64)
}

/// Days since 1970-01-01 from a proleptic Gregorian date.
/// Howard Hinnant's `days_from_civil`, which is the standard way to do this
/// without a calendar library.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // March-based month
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// "2d 16h", "4h 12m", "47m", "<1m". Two units at most: the third never
/// changed anyone's mind about starting a refactor.
pub fn humanize(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        "<1m".into()
    }
}

/// "in 2d 16h", or "now" once the instant has passed. `None` renders as an
/// em dash so a missing `resets_at` does not become the word "None" in a menu.
pub fn until(epoch: Option<u64>, now: u64) -> String {
    match epoch {
        Some(t) if t > now => format!("in {}", humanize(t - now)),
        Some(_) => "now".into(),
        None => "\u{2014}".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_the_endpoint_emits() {
        // Fractional seconds + explicit zero offset, as seen in the wild.
        assert_eq!(
            parse_rfc3339("2026-09-20T00:00:00.241353+00:00"),
            Some(1_789_862_400)
        );
        // Zulu and zero-offset must agree.
        assert_eq!(
            parse_rfc3339("2026-09-20T00:00:00Z"),
            parse_rfc3339("2026-09-20T00:00:00+00:00")
        );
        // A real offset shifts the instant the other way.
        assert_eq!(
            parse_rfc3339("2026-09-20T02:00:00+02:00"),
            parse_rfc3339("2026-09-20T00:00:00Z")
        );
        // Colonless offsets are legal enough to accept.
        assert_eq!(
            parse_rfc3339("2026-09-20T02:00:00+0200"),
            parse_rfc3339("2026-09-20T00:00:00Z")
        );
    }

    #[test]
    fn rejects_rubbish_rather_than_guessing() {
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("not a timestamp"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-09-20"), None);
    }

    #[test]
    fn epoch_zero_round_trips() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn humanizes_to_two_units() {
        assert_eq!(humanize(2 * 86_400 + 16 * 3600), "2d 16h");
        assert_eq!(humanize(4 * 3600 + 12 * 60), "4h 12m");
        assert_eq!(humanize(47 * 60), "47m");
        assert_eq!(humanize(20), "<1m");
    }

    #[test]
    fn until_handles_past_and_missing() {
        assert_eq!(until(Some(100), 40), "in 1m");
        assert_eq!(until(Some(10), 40), "now");
        assert_eq!(until(None, 40), "\u{2014}");
    }
}
