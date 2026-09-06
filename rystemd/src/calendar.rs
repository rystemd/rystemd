//! systemd calendar-event expressions, per `systemd.time(7)`.
//!
//! Supports the full-ish grammar: abbreviations (`daily`, `hourly`, ...),
//! weekday-first or weekday-last forms, date (`*-*-*`, `2026-01-15`) and
//! time (`09:00:00`, `*:00/15`) components, and `*`/`?`/ranges/steps/lists
//! in every field, plus a carry-based next-elapse engine.
//!
//! Everything operates on civil `NaiveDateTime` (no timezone); the manager
//! converts wall-clock "now" to civil time before calling `next_elapse`.

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use std::fmt;

const MAX_SEARCH_YEARS: i32 = 6;

/// A field (year/month/day/dow/hour/minute/second) in a calendar expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// `*` / `?` — matches every value.
    Any,
    /// An explicit, sorted set of values.
    Set(Vec<u32>),
}

impl Field {
    pub fn matches(&self, v: u32) -> bool {
        match self {
            Field::Any => true,
            Field::Set(s) => s.binary_search(&v).is_ok(),
        }
    }

    /// Values in ascending order, lower-bounded by `ge` and capped at `max`.
    fn values_ge(&self, ge: u32, max: u32) -> Vec<u32> {
        match self {
            Field::Any => (ge..=max).collect(),
            Field::Set(s) => s.iter().copied().filter(|&v| v >= ge && v <= max).collect(),
        }
    }
}

/// A parsed calendar expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarSpec {
    pub year: Field,
    pub month: Field,
    pub day: Field,
    pub dow: Field,
    pub hour: Field,
    pub minute: Field,
    pub second: Field,
}

impl Default for CalendarSpec {
    fn default() -> Self {
        CalendarSpec {
            year: Field::Any,
            month: Field::Any,
            day: Field::Any,
            dow: Field::Any,
            hour: Field::Any,
            minute: Field::Any,
            second: Field::Any,
        }
    }
}

// ---- weekday / month name tables -------------------------------------------------

const WDAY_NAMES: [(&str, u32); 7] = [
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
    ("sun", 7),
];
const MONTH_NAMES: [(&str, u32); 12] = [
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

fn resolve_name(tok: &str, names: &[(&str, u32)]) -> Option<u32> {
    let low = tok.to_ascii_lowercase();
    // Full names ("monday") and 3-letter ("mon").
    for (short, val) in names {
        if low == *short || low == format!("{short}day") {
            return Some(*val);
        }
    }
    None
}

fn is_weekday_token(tok: &str) -> bool {
    if tok.contains(':') || tok.contains('-') {
        // Time or date token, not a weekday-only token.
        return false;
    }
    if tok == "*" || tok == "?" {
        return false;
    }
    // Ranges (`Mon..Fri`), lists (`Mon,Wed,Fri`) and steps are allowed; any
    // '.'/',' delimited part that names a weekday makes this a weekday token.
    tok.split(['/', ',', '.', '*'])
        .any(|p| !p.is_empty() && resolve_name(p, &WDAY_NAMES).is_some())
}

/// Parse a field expression into a set, clamping values to `[min, max]`.
fn parse_field(tok: &str, min: u32, max: u32, names: &[(&str, u32)]) -> Result<Field, String> {
    let tok = tok.trim();
    if tok == "*" || tok == "?" {
        return Ok(Field::Any);
    }
    let mut vals: Vec<u32> = Vec::new();
    for item in tok.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(format!("empty list item in `{tok}`"));
        }
        let (rng, step) = match split_step(item) {
            Some((a, b)) => (a, b),
            None => (item, None),
        };
        let mut lo;
        let mut hi;
        match rng.split_once("..") {
            Some((l, h)) => {
                lo = resolve(l.trim(), min, names)?;
                hi = resolve(h.trim(), max, names)?;
            }
            None => {
                lo = resolve(rng.trim(), min, names)?;
                // `N/step` without a range runs to the field maximum;
                // a bare `N` is a single value.
                hi = if step.is_some() { max } else { lo };
            }
        }
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        let step = step.unwrap_or(1);
        let mut v = lo;
        while v <= hi && v <= max {
            if v >= min {
                vals.push(v);
            }
            v = v.saturating_add(step);
        }
    }
    if vals.is_empty() {
        return Err(format!(
            "field `{tok}` has no valid values in [{min}..{max}]"
        ));
    }
    vals.sort_unstable();
    vals.dedup();
    Ok(Field::Set(vals))
}

fn split_step(item: &str) -> Option<(&str, Option<u32>)> {
    let (a, b) = item.split_once('/')?;
    Some((a, b.parse::<u32>().ok()))
}

fn resolve(tok: &str, default: u32, names: &[(&str, u32)]) -> Result<u32, String> {
    if tok == "*" {
        return Ok(default);
    }
    if let Some(v) = resolve_name(tok, names) {
        return Ok(v);
    }
    tok.parse::<u32>()
        .map_err(|_| format!("invalid value `{tok}`"))
}

fn days_in_month(y: i32, m: u32) -> u32 {
    if m == 2 {
        if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            29
        } else {
            28
        }
    } else if matches!(m, 4 | 6 | 9 | 11) {
        30
    } else {
        31
    }
}

/// 1 = Monday .. 7 = Sunday. Returns `None` for invalid (y, m, d) rather
/// than panicking — `panic = "abort"` would make a bad date a kernel panic
/// as PID 1. Callers should treat `None` as "no match" (the day is invalid
/// for that month/year, which can only happen if the caller's bounds check
/// is wrong).
fn weekday_number(y: i32, m: u32, d: u32) -> Option<u32> {
    let date = NaiveDate::from_ymd_opt(y, m, d)?;
    Some(date.weekday().num_days_from_monday() + 1)
}

impl CalendarSpec {
    /// Parse a calendar expression.
    pub fn parse(s: &str) -> Result<CalendarSpec, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty calendar expression".into());
        }
        if let Some(spec) = abbreviation(s) {
            return Ok(spec);
        }

        let mut toks: Vec<&str> = s.split_whitespace().collect();
        if toks.is_empty() {
            return Err(format!("empty calendar expression `{s}`"));
        }

        let mut spec = CalendarSpec::default();

        // Weekday may be first or last.
        if is_weekday_token(toks[0]) {
            spec.dow = parse_field(toks[0], 1, 7, &WDAY_NAMES)?;
            toks.remove(0);
        }
        if !toks.is_empty() && toks.len() >= 2 && is_weekday_token(toks[toks.len() - 1]) {
            spec.dow = parse_field(toks[toks.len() - 1], 1, 7, &WDAY_NAMES)?;
            toks.pop();
        }

        let mut have_time = false;
        let mut have_date = false;
        for tok in toks {
            let tok = tok.trim();
            if tok.contains(':') {
                if have_time {
                    return Err(format!("multiple time components in `{s}`"));
                }
                parse_time(tok, &mut spec)?;
                have_time = true;
            } else {
                if have_date {
                    return Err(format!("multiple date components in `{s}`"));
                }
                parse_date(tok, &mut spec)?;
                have_date = true;
            }
        }
        let _ = have_date;
        // systemd.timer(5): when an expression specifies no time component
        // (e.g. `OnCalendar=Mon` or a bare date), the elapse is at 00:00:00 on
        // the matching day — not "the current time". A Field::Any time would
        // make next_time(force_gt) pick the current wall-clock +1s on an
        // already-matching day and re-fire immediately. Default it to midnight.
        if !have_time {
            spec.hour = Field::Set(vec![0]);
            spec.minute = Field::Set(vec![0]);
            spec.second = Field::Set(vec![0]);
        }
        Ok(spec)
    }

    fn day_ok(&self, y: i32, m: u32, d: u32) -> bool {
        let Some(dow) = weekday_number(y, m, d) else {
            return false;
        };
        match (&self.day, &self.dow) {
            (Field::Any, Field::Any) => true,
            (Field::Any, dowf) => dowf.matches(dow),
            (dayf, Field::Any) => dayf.matches(d),
            (dayf, dowf) => dayf.matches(d) || dowf.matches(dow),
        }
    }

    /// Next occurrence strictly after `from`, or `None` if none within
    /// `MAX_SEARCH_YEARS`. All values are civil (timezone-free).
    pub fn next_elapse(&self, from: NaiveDateTime) -> Option<NaiveDateTime> {
        let from = local_second(from);
        let mut y = from.year();
        while y <= from.year() + MAX_SEARCH_YEARS {
            if self.year.matches(y as u32) {
                let mut m = if y == from.year() { from.month() } else { 1 };
                while m <= 12 {
                    if self.month.matches(m) {
                        let mut d = if y == from.year() && m == from.month() {
                            from.day()
                        } else {
                            1
                        };
                        let dim = days_in_month(y, m);
                        while d <= dim {
                            if self.day_ok(y, m, d) {
                                let same_day =
                                    y == from.year() && m == from.month() && d == from.day();
                                if let Some((h, mi, s)) = self.next_time(
                                    from.hour(),
                                    from.minute(),
                                    from.second(),
                                    same_day,
                                ) && let Some(nt) = NaiveDate::from_ymd_opt(y, m, d)
                                    .and_then(|dt| dt.and_hms_opt(h, mi, s))
                                    && nt > from
                                {
                                    return Some(nt);
                                }
                            }
                            d += 1;
                        }
                    }
                    m += 1;
                }
            }
            y += 1;
        }
        None
    }

    fn next_time(&self, h0: u32, mi0: u32, s0: u32, force_gt: bool) -> Option<(u32, u32, u32)> {
        for h in self.hour.values_ge(if force_gt { h0 } else { 0 }, 23) {
            for mi in self
                .minute
                .values_ge(if force_gt && h == h0 { mi0 } else { 0 }, 59)
            {
                for s in self.second.values_ge(
                    if force_gt && h == h0 && mi == mi0 {
                        s0
                    } else {
                        0
                    },
                    59,
                ) {
                    if !force_gt || (h, mi, s) != (h0, mi0, s0) {
                        return Some((h, mi, s));
                    }
                }
            }
        }
        None
    }
}

fn local_second(dt: NaiveDateTime) -> NaiveDateTime {
    dt.with_nanosecond(0).unwrap_or(dt)
}

fn parse_time(tok: &str, spec: &mut CalendarSpec) -> Result<(), String> {
    // HH:MM[:SS][/step on last component]
    let (base, step) = match tok.split_once('/') {
        Some((b, st)) => (
            b,
            Some(
                st.parse::<u32>()
                    .map_err(|_| format!("bad step in `{tok}`"))?,
            ),
        ),
        None => (tok, None),
    };
    let parts: Vec<&str> = base.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(format!("bad time `{tok}`"));
    }
    spec.hour = parse_field(parts[0], 0, 23, &[])?;
    if let Some(st) = step {
        // A step on the time applies to the last segment (minutes for 2-part,
        // seconds for 3-part) — approximate by re-parsing with a range.
        let applied = if parts.len() == 3 { 2 } else { 1 };
        let with_step = format!("{}/{}", parts[applied], st);
        if parts.len() == 3 {
            spec.minute = parse_field(parts[1], 0, 59, &[])?;
            spec.second = parse_field(&with_step, 0, 59, &[])?;
        } else {
            spec.minute = parse_field(&with_step, 0, 59, &[])?;
            spec.second = Field::Set(vec![0]);
        }
    } else {
        spec.minute = parse_field(parts[1], 0, 59, &[])?;
        if parts.len() == 3 {
            spec.second = parse_field(parts[2], 0, 59, &[])?;
        } else {
            spec.second = Field::Set(vec![0]);
        }
    }
    Ok(())
}

fn parse_date(tok: &str, spec: &mut CalendarSpec) -> Result<(), String> {
    // YYYY-MM-DD, where each component may be `*`.
    let parts: Vec<&str> = tok.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("bad date `{tok}`"));
    }
    spec.year = parse_field(parts[0], 1000, 9999, &[])?;
    spec.month = parse_field(parts[1], 1, 12, &MONTH_NAMES)?;
    spec.day = parse_field(parts[2], 1, 31, &[])?;
    Ok(())
}

fn abbreviation(s: &str) -> Option<CalendarSpec> {
    let spec = match s.to_ascii_lowercase().as_str() {
        "minutely" => CalendarSpec {
            hour: Field::Any,
            minute: Field::Any,
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        "hourly" => CalendarSpec {
            hour: Field::Any,
            minute: Field::Set(vec![0]),
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        "daily" => CalendarSpec {
            hour: Field::Set(vec![0]),
            minute: Field::Set(vec![0]),
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        "weekly" => CalendarSpec {
            dow: Field::Set(vec![1]),
            hour: Field::Set(vec![0]),
            minute: Field::Set(vec![0]),
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        "monthly" => CalendarSpec {
            day: Field::Set(vec![1]),
            hour: Field::Set(vec![0]),
            minute: Field::Set(vec![0]),
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        "quarterly" => CalendarSpec {
            month: Field::Set(vec![1, 4, 7, 10]),
            day: Field::Set(vec![1]),
            hour: Field::Set(vec![0]),
            minute: Field::Set(vec![0]),
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        "semi-annually" => CalendarSpec {
            month: Field::Set(vec![1, 7]),
            day: Field::Set(vec![1]),
            hour: Field::Set(vec![0]),
            minute: Field::Set(vec![0]),
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        "annually" | "yearly" => CalendarSpec {
            month: Field::Set(vec![1]),
            day: Field::Set(vec![1]),
            hour: Field::Set(vec![0]),
            minute: Field::Set(vec![0]),
            second: Field::Set(vec![0]),
            ..Default::default()
        },
        _ => return None,
    };
    Some(spec)
}

impl fmt::Display for CalendarSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Best-effort, abbreviated-ish rendering (used in list-timers).
        write!(
            f,
            "{}-{}-{} {:02}:{:02}:{:02}",
            field_str(&self.year),
            field_str(&self.month),
            field_str(&self.day),
            self.hour.values_ge(0, 23).first().copied().unwrap_or(0),
            self.minute.values_ge(0, 59).first().copied().unwrap_or(0),
            self.second.values_ge(0, 59).first().copied().unwrap_or(0),
        )?;
        Ok(())
    }
}

fn field_str(fl: &Field) -> String {
    match fl {
        Field::Any => "*".to_string(),
        Field::Set(s) => s
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
    }

    fn next(spec: &str, from: NaiveDateTime) -> NaiveDateTime {
        CalendarSpec::parse(spec)
            .unwrap()
            .next_elapse(from)
            .expect("no next elapse")
    }

    #[test]
    fn daily() {
        let from = dt(2026, 1, 1, 0, 0, 0);
        assert_eq!(next("daily", from), dt(2026, 1, 2, 0, 0, 0));
        // Strictly after "from": from 23:59:59 -> next midnight.
        assert_eq!(
            next("daily", dt(2026, 1, 1, 23, 59, 59)),
            dt(2026, 1, 2, 0, 0, 0)
        );
        // From exactly midnight -> next midnight (strictly after).
        assert_eq!(
            next("*-*-* 00:00:00", dt(2026, 1, 1, 0, 0, 0)),
            dt(2026, 1, 2, 0, 0, 0)
        );
    }

    #[test]
    fn hourly_and_minutely() {
        assert_eq!(
            next("hourly", dt(2026, 1, 1, 10, 20, 0)),
            dt(2026, 1, 1, 11, 0, 0)
        );
        assert_eq!(
            next("minutely", dt(2026, 1, 1, 10, 20, 30)),
            dt(2026, 1, 1, 10, 21, 0)
        );
    }

    #[test]
    fn weekly_monday() {
        // 2026-01-01 is a Thursday. Next Monday is 2026-01-05.
        assert_eq!(
            next("weekly", dt(2026, 1, 1, 0, 0, 0)),
            dt(2026, 1, 5, 0, 0, 0)
        );
        assert_eq!(
            next("Mon *-*-* 00:00:00", dt(2026, 1, 1, 0, 0, 0)),
            dt(2026, 1, 5, 0, 0, 0)
        );
    }

    #[test]
    fn weekday_only_defaults_to_midnight() {
        // `Mon` with no time component elapses at Monday 00:00:00 — even when
        // "now" is already Monday (it must advance to midnight, not re-fire at
        // the current time +1s).
        // 2026-01-05 is a Monday. 10:00 on that Monday -> next Monday 00:00.
        assert_eq!(
            next("Mon", dt(2026, 1, 5, 10, 0, 0)),
            dt(2026, 1, 12, 0, 0, 0)
        );
        // Thursday -> next Monday 00:00.
        assert_eq!(
            next("Mon", dt(2026, 1, 1, 0, 0, 0)),
            dt(2026, 1, 5, 0, 0, 0)
        );
        // Bare date likewise elapses at 00:00:00.
        assert_eq!(
            next("2026-01-15", dt(2026, 1, 14, 0, 0, 0)),
            dt(2026, 1, 15, 0, 0, 0)
        );
    }

    #[test]
    fn weekday_range_first_and_last() {
        // Mon..Fri 09:00 — must be a weekday when it falls on a Thu/Fri/etc.
        let s = "Mon..Fri 09:00:00";
        // From Friday 08:00 -> same-day Friday 09:00.
        let friday = dt(2026, 1, 2, 8, 0, 0); // Jan 2 2026 is a Friday
        assert_eq!(next(s, friday), dt(2026, 1, 2, 9, 0, 0));
        // From Friday 10:00 -> Monday 09:00.
        assert_eq!(next(s, dt(2026, 1, 2, 10, 0, 0)), dt(2026, 1, 5, 9, 0, 0));
        // Same spec with weekday last.
        let s2 = "*-*-* 09:00:00 Mon..Fri";
        assert_eq!(next(s2, friday), dt(2026, 1, 2, 9, 0, 0));
    }

    #[test]
    fn steps_on_seconds_and_minutes() {
        // Every 15 minutes: *:00/15.
        assert_eq!(
            next("*:00/15", dt(2026, 1, 1, 10, 7, 0)),
            dt(2026, 1, 1, 10, 15, 0)
        );
        assert_eq!(
            next("*:00/15", dt(2026, 1, 1, 10, 15, 0)),
            dt(2026, 1, 1, 10, 30, 0)
        );
        // Every 5 seconds of every minute, e.g. 10:20:00, :05...
        assert_eq!(
            next("*:*:00/5", dt(2026, 1, 1, 10, 20, 2)),
            dt(2026, 1, 1, 10, 20, 5)
        );
    }

    #[test]
    fn specific_datetime() {
        assert_eq!(
            next("2026-08-21 09:00:00", dt(2026, 1, 1, 0, 0, 0)),
            dt(2026, 8, 21, 9, 0, 0)
        );
        // A year-anchored date is a ONE-SHOT event (systemd semantics): once
        // that instant is reached or passed, there is no next elapse.
        assert!(
            CalendarSpec::parse("2026-08-21 09:00:00")
                .unwrap()
                .next_elapse(dt(2026, 8, 21, 9, 0, 0))
                .is_none()
        );
        // `*-08-21` (no year) recurs annually.
        assert_eq!(
            next("*-08-21 09:00:00", dt(2026, 8, 21, 9, 0, 0)),
            dt(2027, 8, 21, 9, 0, 0)
        );
    }

    #[test]
    fn ranges_and_lists() {
        // 7:00 and 19:00 daily.
        assert_eq!(
            next("*-*-* 7,19:00:00", dt(2026, 1, 1, 0, 0, 0)),
            dt(2026, 1, 1, 7, 0, 0)
        );
        assert_eq!(
            next("*-*-* 7,19:00:00", dt(2026, 1, 1, 8, 0, 0)),
            dt(2026, 1, 1, 19, 0, 0)
        );
        // Monthly on the 1st and 15th.
        assert_eq!(
            next("*-*-1,15 00:00:00", dt(2026, 1, 2, 0, 0, 0)),
            dt(2026, 1, 15, 0, 0, 0)
        );
    }

    #[test]
    fn leap_day() {
        // Next Feb 29: 2028.
        assert_eq!(
            next("*-02-29 00:00:00", dt(2026, 3, 1, 0, 0, 0)),
            dt(2028, 2, 29, 0, 0, 0)
        );
    }

    #[test]
    fn month_end() {
        // 31st only occurs in some months: skip Feb.
        assert_eq!(
            next("*-*-31 00:00:00", dt(2026, 1, 1, 0, 0, 0)),
            dt(2026, 1, 31, 0, 0, 0)
        );
        assert_eq!(
            next("*-*-31 00:00:00", dt(2026, 2, 1, 0, 0, 0)),
            dt(2026, 3, 31, 0, 0, 0)
        );
    }

    #[test]
    fn abbreviations() {
        assert_eq!(
            next("minutely", dt(2026, 1, 1, 10, 20, 45)),
            dt(2026, 1, 1, 10, 21, 0)
        );
        assert_eq!(
            next("annually", dt(2026, 6, 1, 0, 0, 0)),
            dt(2027, 1, 1, 0, 0, 0)
        );
        assert_eq!(
            next("quarterly", dt(2026, 4, 2, 0, 0, 0)),
            dt(2026, 7, 1, 0, 0, 0)
        );
    }

    #[test]
    fn parse_errors() {
        assert!(CalendarSpec::parse("").is_err());
        assert!(CalendarSpec::parse("not-a-spec").is_err());
        assert!(CalendarSpec::parse("24:00:00").is_err());
    }

    #[test]
    fn no_elapse_within_window() {
        // A date far in the future relative to "now" -> None.
        let c = CalendarSpec::parse("2099-01-01 00:00:00").unwrap();
        assert!(c.next_elapse(dt(2100, 1, 1, 0, 0, 0)).is_none());
    }
}
