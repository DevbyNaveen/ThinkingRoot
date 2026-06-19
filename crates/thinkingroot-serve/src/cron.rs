//! Minimal, dependency-free cron for the Root Function `schedule` attribute.
//!
//! Supports the standard 5-field cron expression `min hour day-of-month month
//! day-of-week`, each field one of: `*`, a number, a `a-b` range, a `*/step`,
//! or a comma list of those. Day-of-week is 0–6 (Sun=0), also accepts 7=Sun.
//!
//! We only need two operations and they are computed against real `chrono`
//! UTC time, so there is no clock drift: `matches(expr, dt)` (does this minute
//! fire?) and `next_after(expr, from)` (the next firing minute strictly after
//! `from`). `next_after` steps minute-by-minute — correct and obvious; the
//! 400-day cap bounds the (deploy-time / once-per-fire) cost for absurd exprs.

use chrono::{DateTime, Datelike, Timelike, Utc};

/// A parsed cron expression (five fields, each an allow-set over its range).
#[derive(Debug, Clone)]
pub struct Cron {
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
}

#[derive(Debug, Clone)]
struct Field {
    /// `None` = wildcard (`*`); `Some(set)` = explicit allowed values.
    allowed: Option<Vec<u32>>,
}

impl Field {
    fn matches(&self, v: u32) -> bool {
        match &self.allowed {
            None => true,
            Some(set) => set.contains(&v),
        }
    }
}

fn parse_field(spec: &str, min: u32, max: u32) -> Result<Field, String> {
    let spec = spec.trim();
    if spec == "*" {
        return Ok(Field { allowed: None });
    }
    let mut out: Vec<u32> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        // step form: `*/n` or `a-b/n`
        let (range_spec, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| format!("bad step '{s}'"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err("step cannot be 0".into());
        }
        let (lo, hi) = if range_spec == "*" {
            (min, max)
        } else if let Some((a, b)) = range_spec.split_once('-') {
            (
                a.trim().parse::<u32>().map_err(|_| format!("bad range start '{a}'"))?,
                b.trim().parse::<u32>().map_err(|_| format!("bad range end '{b}'"))?,
            )
        } else {
            let n = range_spec.parse::<u32>().map_err(|_| format!("bad number '{range_spec}'"))?;
            (n, n)
        };
        if lo > hi || lo < min || hi > max {
            return Err(format!("field value {lo}-{hi} out of range {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            out.push(v);
            v += step;
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(Field { allowed: Some(out) })
}

impl Cron {
    /// Parse a 5-field cron expression. Returns an error string on any malformed
    /// field (callers surface it; a bad schedule must never silently no-op).
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!("expected 5 cron fields, got {}", fields.len()));
        }
        // day-of-week: normalise 7 → 0 (both mean Sunday) before parsing the set.
        let dow_spec = fields[4].replace('7', "0");
        Ok(Cron {
            minute: parse_field(fields[0], 0, 59)?,
            hour: parse_field(fields[1], 0, 23)?,
            dom: parse_field(fields[2], 1, 31)?,
            month: parse_field(fields[3], 1, 12)?,
            dow: parse_field(&dow_spec, 0, 6)?,
        })
    }

    /// Does `dt` (truncated to the minute) fire this expression? Standard cron
    /// semantics: when BOTH day-of-month and day-of-week are restricted, a match
    /// on EITHER fires (the historical Vixie-cron OR rule); otherwise both the
    /// active day fields must match.
    pub fn matches(&self, dt: DateTime<Utc>) -> bool {
        let dow = dt.weekday().num_days_from_sunday(); // 0=Sun..6=Sat
        let day_ok = match (&self.dom.allowed, &self.dow.allowed) {
            (Some(_), Some(_)) => self.dom.matches(dt.day()) || self.dow.matches(dow),
            _ => self.dom.matches(dt.day()) && self.dow.matches(dow),
        };
        self.minute.matches(dt.minute())
            && self.hour.matches(dt.hour())
            && self.month.matches(dt.month())
            && day_ok
    }

    /// The next firing instant strictly after `from`, aligned to the minute.
    /// `None` if no match within ~400 days (e.g. an impossible Feb-30 expr).
    pub fn next_after(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // Start at the next whole minute after `from`.
        let mut t = (from + chrono::Duration::minutes(1))
            .with_second(0)?
            .with_nanosecond(0)?;
        for _ in 0..(400 * 24 * 60) {
            if self.matches(t) {
                return Some(t);
            }
            t += chrono::Duration::minutes(1);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn every_minute_wildcard() {
        let c = Cron::parse("* * * * *").unwrap();
        assert!(c.matches(at(2026, 6, 19, 3, 17)));
    }

    #[test]
    fn daily_at_0300() {
        let c = Cron::parse("0 3 * * *").unwrap();
        assert!(c.matches(at(2026, 6, 19, 3, 0)));
        assert!(!c.matches(at(2026, 6, 19, 3, 1)));
        assert!(!c.matches(at(2026, 6, 19, 4, 0)));
        // next after 03:00 is tomorrow 03:00
        let n = c.next_after(at(2026, 6, 19, 3, 0)).unwrap();
        assert_eq!(n, at(2026, 6, 20, 3, 0));
    }

    #[test]
    fn step_and_list() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert!(c.matches(at(2026, 6, 19, 1, 0)));
        assert!(c.matches(at(2026, 6, 19, 1, 30)));
        assert!(!c.matches(at(2026, 6, 19, 1, 7)));
        let c2 = Cron::parse("0 9,17 * * *").unwrap();
        assert!(c2.matches(at(2026, 6, 19, 9, 0)));
        assert!(c2.matches(at(2026, 6, 19, 17, 0)));
        assert!(!c2.matches(at(2026, 6, 19, 12, 0)));
    }

    #[test]
    fn weekday_range_monday_to_friday() {
        // Mon–Fri at 08:00. 2026-06-19 is a Friday; 2026-06-20 a Saturday.
        let c = Cron::parse("0 8 * * 1-5").unwrap();
        assert!(c.matches(at(2026, 6, 19, 8, 0))); // Fri
        assert!(!c.matches(at(2026, 6, 20, 8, 0))); // Sat
    }

    #[test]
    fn dom_or_dow_rule() {
        // Both restricted → OR: fires on the 1st OR on Sunday.
        let c = Cron::parse("0 0 1 * 0").unwrap();
        assert!(c.matches(at(2026, 7, 1, 0, 0))); // the 1st (a Wednesday)
        assert!(c.matches(at(2026, 6, 21, 0, 0))); // a Sunday (not the 1st)
        assert!(!c.matches(at(2026, 6, 23, 0, 0))); // Tuesday, not the 1st
    }

    #[test]
    fn rejects_malformed() {
        assert!(Cron::parse("* * * *").is_err()); // 4 fields
        assert!(Cron::parse("99 * * * *").is_err()); // minute out of range
        assert!(Cron::parse("*/0 * * * *").is_err()); // zero step
    }
}
