//! Deterministic time expressions (no natural language).
//!
//! Accepted:
//! - ISO 8601 with offset: `2026-10-16T15:00:00+08:00`, `2026-10-16T07:00:00Z`
//! - Local date-time (in --tz): `2026-10-16T15:00`, `2026-10-16 15:00:00`
//! - Date: `2026-10-16` (start of that day in --tz)
//! - Keywords: `now`, `today`, `tomorrow`, `yesterday`, `eom` (start of next month = end of this month), `som`
//! - Relative: `+7d`, `-2h`, `+30m`, `+1w`, `+1mo`. Units `d`/`w`/`mo` snap to the start of the day;
//!   `h`/`m` are relative to now.
//! - Week anchors: `<expr>/mon` .. `<expr>/sun` -> that weekday in the ISO week (Mon-Sun) of `<expr>`, 00:00.
use crate::error::{AppError, Result};
use jiff::civil::{Date, DateTime, Weekday};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, ToSpan, Zoned};

fn terr(input: &str, why: &str) -> AppError {
    AppError::new("E_TIME_PARSE", format!("cannot parse time `{input}`: {why}"))
        .hint("Use ISO 8601 (2026-10-16T15:00:00+08:00, 2026-10-16T15:00, 2026-10-16) or relative forms: now, today, tomorrow, +7d, -2h, +1w/mon, eom. Natural language is not supported; convert it yourself.")
}

pub fn resolve_tz(name: Option<&str>) -> Result<TimeZone> {
    match name {
        Some(n) if !n.is_empty() => TimeZone::get(n).map_err(|_| {
            AppError::usage(format!("unknown time zone `{n}`"))
                .hint("Use an IANA name like Asia/Shanghai or UTC.")
        }),
        _ => Ok(TimeZone::try_system().unwrap_or(TimeZone::UTC)),
    }
}

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" => Weekday::Monday,
        "tue" => Weekday::Tuesday,
        "wed" => Weekday::Wednesday,
        "thu" => Weekday::Thursday,
        "fri" => Weekday::Friday,
        "sat" => Weekday::Saturday,
        "sun" => Weekday::Sunday,
        _ => return None,
    })
}

fn start_of_day(d: Date, tz: &TimeZone) -> Result<Zoned> {
    d.to_zoned(tz.clone())
        .map_err(|e| AppError::internal(e.to_string()))
}

/// Parse a time expression relative to `now`.
pub fn parse_at(input: &str, tz: &TimeZone, now: &Zoned) -> Result<Zoned> {
    let s = input.trim();
    if s.is_empty() {
        return Err(terr(input, "empty"));
    }
    if let Some((base, anchor)) = s.rsplit_once('/') {
        if let Some(wd) = weekday(&anchor.to_ascii_lowercase()) {
            let b = parse_at(base, tz, now)?;
            let d = b.date();
            let monday = d
                .checked_sub((d.weekday().to_monday_zero_offset() as i64).days())
                .map_err(|e| terr(input, &e.to_string()))?;
            let target = monday
                .checked_add((wd.to_monday_zero_offset() as i64).days())
                .map_err(|e| terr(input, &e.to_string()))?;
            return start_of_day(target, tz);
        }
        return Err(terr(input, "unknown anchor (use /mon../sun)"));
    }
    let today = now.date();
    match s.to_ascii_lowercase().as_str() {
        "now" => return Ok(now.clone()),
        "today" => return start_of_day(today, tz),
        "tomorrow" => {
            return start_of_day(
                today.tomorrow().map_err(|e| terr(input, &e.to_string()))?,
                tz,
            );
        }
        "yesterday" => {
            return start_of_day(
                today.yesterday().map_err(|e| terr(input, &e.to_string()))?,
                tz,
            );
        }
        "som" => return start_of_day(today.first_of_month(), tz),
        "eom" => {
            let n = today
                .last_of_month()
                .tomorrow()
                .map_err(|e| terr(input, &e.to_string()))?;
            return start_of_day(n, tz);
        }
        _ => {}
    }
    if s.starts_with('+') || s.starts_with('-') {
        let neg = s.starts_with('-');
        let body = &s[1..];
        let digits: String = body.chars().take_while(|c| c.is_ascii_digit()).collect();
        let unit = &body[digits.len()..];
        let n: i64 = digits
            .parse()
            .map_err(|_| terr(input, "expected a number after +/-"))?;
        let n = if neg { -n } else { n };
        return match unit {
            "m" | "min" => now
                .checked_add(n.minutes())
                .map_err(|e| terr(input, &e.to_string())),
            "h" => now
                .checked_add(n.hours())
                .map_err(|e| terr(input, &e.to_string())),
            "d" | "w" | "mo" => {
                let span: Span = match unit {
                    "d" => n.days(),
                    "w" => n.weeks(),
                    _ => n.months(),
                };
                let d = today
                    .checked_add(span)
                    .map_err(|e| terr(input, &e.to_string()))?;
                start_of_day(d, tz)
            }
            _ => Err(terr(input, "unknown unit (use m, h, d, w, mo)")),
        };
    }
    // Absolute forms.
    if let Ok(ts) = s.parse::<Timestamp>() {
        // has explicit offset / Z: keep the instant, display in tz
        return Ok(ts.to_zoned(tz.clone()));
    }
    if let Ok(z) = s.parse::<Zoned>() {
        return Ok(z);
    }
    if let Ok(dt) = s.replace(' ', "T").parse::<DateTime>()
        && s.len() > 10
    {
        return dt
            .to_zoned(tz.clone())
            .map_err(|e| terr(input, &e.to_string()));
    }
    if let Ok(d) = s.parse::<Date>() {
        return start_of_day(d, tz);
    }
    Err(terr(input, "unrecognized format"))
}

pub fn parse(input: &str, tz: &TimeZone) -> Result<Zoned> {
    parse_at(input, tz, &Zoned::now().with_time_zone(tz.clone()))
}

/// Parse a duration like `1h`, `90m`, `1h30m`, `2d`, `-15m`, or ISO `PT15M`. Returns signed seconds.
pub fn parse_duration(input: &str) -> Result<i64> {
    let s = input.trim();
    let err = || {
        AppError::new("E_TIME_PARSE", format!("cannot parse duration `{input}`"))
            .hint("Use forms like 30m, 1h, 1h30m, 2d, -15m or ISO 8601 (PT15M).")
    };
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if body.is_empty() {
        return Err(err());
    }
    if body.starts_with('P') || body.starts_with('p') {
        let span: Span = body.parse().map_err(|_| err())?;
        let secs = span
            .total((jiff::Unit::Second, jiff::civil::date(2000, 1, 1)))
            .map_err(|_| err())? as i64;
        return Ok(if neg { -secs } else { secs });
    }
    let mut total = 0i64;
    let mut num = String::new();
    let chars = body.chars().peekable();
    for c in chars {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().map_err(|_| err())?;
        num.clear();
        let mult = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            'w' => 7 * 86400,
            _ => return Err(err()),
        };
        total += n * mult;
    }
    if !num.is_empty() {
        return Err(err());
    }
    Ok(if neg { -total } else { total })
}

/// Format seconds as an iCalendar duration (e.g. -PT15M).
pub fn ical_duration(secs: i64) -> String {
    let sign = if secs < 0 { "-" } else { "" };
    let mut s = secs.abs();
    let days = s / 86400;
    s %= 86400;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    let mut out = format!("{sign}P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if h > 0 || m > 0 || sec > 0 || days == 0 {
        out.push('T');
        if h > 0 {
            out.push_str(&format!("{h}H"));
        }
        if m > 0 {
            out.push_str(&format!("{m}M"));
        }
        if sec > 0 || (h == 0 && m == 0) {
            out.push_str(&format!("{sec}S"));
        }
    }
    out
}

pub fn fmt(z: &Zoned) -> String {
    z.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> (TimeZone, Zoned) {
        let tz = TimeZone::get("Asia/Shanghai").unwrap();
        let n = "2026-10-14T23:25:00+08:00[Asia/Shanghai]"
            .parse::<Zoned>()
            .unwrap();
        (tz, n)
    }
    fn p(s: &str) -> String {
        let (tz, n) = now();
        fmt(&parse_at(s, &tz, &n).unwrap())
    }
    #[test]
    fn keywords() {
        assert_eq!(p("today"), "2026-10-14T00:00:00+08:00");
        assert_eq!(p("tomorrow"), "2026-10-15T00:00:00+08:00");
        assert_eq!(p("eom"), "2026-11-01T00:00:00+08:00");
        assert_eq!(p("now"), "2026-10-14T23:25:00+08:00");
    }
    #[test]
    fn relative() {
        assert_eq!(p("+1d"), "2026-10-15T00:00:00+08:00");
        assert_eq!(p("-2h"), "2026-10-14T21:25:00+08:00");
        assert_eq!(p("+1w/mon"), "2026-10-19T00:00:00+08:00");
        assert_eq!(p("+1w/fri"), "2026-10-23T00:00:00+08:00");
        assert_eq!(p("today/sun"), "2026-10-18T00:00:00+08:00");
    }
    #[test]
    fn absolute() {
        assert_eq!(p("2026-10-16T15:00"), "2026-10-16T15:00:00+08:00");
        assert_eq!(p("2026-10-16"), "2026-10-16T00:00:00+08:00");
        assert_eq!(p("2026-10-16T07:00:00Z"), "2026-10-16T15:00:00+08:00");
        assert_eq!(p("2026-10-16T15:00:00+09:00"), "2026-10-16T14:00:00+08:00");
    }
    #[test]
    fn rejects_natural_language() {
        let (tz, n) = now();
        let e = parse_at("next friday", &tz, &n).unwrap_err();
        assert_eq!(e.code, "E_TIME_PARSE");
        assert_eq!(e.exit_code(), 2);
    }
    #[test]
    fn durations() {
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("-15m").unwrap(), -900);
        assert_eq!(parse_duration("PT15M").unwrap(), 900);
        assert_eq!(parse_duration("-PT1H").unwrap(), -3600);
        assert!(parse_duration("abc").is_err());
        assert_eq!(ical_duration(-900), "-PT15M");
        assert_eq!(ical_duration(86400), "P1D");
        assert_eq!(ical_duration(5400), "PT1H30M");
    }
}
