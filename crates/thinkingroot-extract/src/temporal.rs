//! Mechanical temporal-expression extraction — zero LLM.
//!
//! LongMemEval's temporal-reasoning category (and the engine's `/claims/as-of`
//! bitemporal queries) need a fact anchored to its *event* date, not its
//! ingestion time. The Witness already carries a `valid_from` field; by default
//! that is the compile timestamp. When a fact's text states an absolute date
//! ("On April 10, 2023 I bought a car."), anchoring `valid_from` to that date
//! makes time-scoped retrieval correct without any LLM.
//!
//! This extractor is deliberately conservative — it recognises only
//! *unambiguous absolute* dates (numeric ISO/slash forms and `Month D, YYYY`
//! variants). Relative expressions ("yesterday", "last March") need a reference
//! time and a calendar model; they are intentionally NOT handled here to avoid
//! guessing a wrong anchor.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use regex::Regex;
use std::sync::OnceLock;

fn iso_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // 2023-04-10 or 2023/04/10  (year 19xx–20xx, month 1–12, day 1–31)
    RE.get_or_init(|| {
        Regex::new(r"\b((?:19|20)\d{2})[-/](0?[1-9]|1[0-2])[-/](0?[1-9]|[12]\d|3[01])\b")
            .expect("iso date regex")
    })
}

fn month_dmy_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // "April 10, 2023" / "Apr 10 2023" / "10 April 2023" / "10 Apr 2023"
    RE.get_or_init(|| {
        Regex::new(
            r"(?ix)
            \b(?:
                (?P<m1>jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?\s+
                (?P<d1>0?[1-9]|[12]\d|3[01])(?:st|nd|rd|th)?,?\s+(?P<y1>(?:19|20)\d{2})
              |
                (?P<d2>0?[1-9]|[12]\d|3[01])(?:st|nd|rd|th)?\s+
                (?P<m2>jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?,?\s+
                (?P<y2>(?:19|20)\d{2})
            )\b",
        )
        .expect("month date regex")
    })
}

fn month_num(s: &str) -> Option<u32> {
    let m = s.to_ascii_lowercase();
    let m = &m[..m.len().min(3)];
    Some(match m {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

fn date_to_utc(y: i32, m: u32, d: u32) -> Option<DateTime<Utc>> {
    let nd = NaiveDate::from_ymd_opt(y, m, d)?;
    let ndt = nd.and_hms_opt(0, 0, 0)?;
    Some(Utc.from_utc_datetime(&ndt))
}

/// Extract the FIRST unambiguous absolute date in `text` as a UTC midnight
/// timestamp. Returns `None` when no absolute date is present.
pub fn extract_event_date(text: &str) -> Option<DateTime<Utc>> {
    // Prefer numeric ISO/slash forms (least ambiguous), then Month D, YYYY.
    if let Some(c) = iso_re().captures(text) {
        let y: i32 = c.get(1)?.as_str().parse().ok()?;
        let m: u32 = c.get(2)?.as_str().parse().ok()?;
        let d: u32 = c.get(3)?.as_str().parse().ok()?;
        if let Some(dt) = date_to_utc(y, m, d) {
            return Some(dt);
        }
    }
    if let Some(c) = month_dmy_re().captures(text) {
        let (mon, day, year) = if let Some(m1) = c.name("m1") {
            (m1.as_str(), c.name("d1")?.as_str(), c.name("y1")?.as_str())
        } else {
            (
                c.name("m2")?.as_str(),
                c.name("d2")?.as_str(),
                c.name("y2")?.as_str(),
            )
        };
        let m = month_num(mon)?;
        let d: u32 = day.parse().ok()?;
        let y: i32 = year.parse().ok()?;
        if let Some(dt) = date_to_utc(y, m, d) {
            return Some(dt);
        }
    }
    None
}

/// True when `text` contains at least one absolute date.
pub fn has_event_date(text: &str) -> bool {
    extract_event_date(text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn ymd(dt: DateTime<Utc>) -> (i32, u32, u32) {
        (dt.year(), dt.month(), dt.day())
    }

    #[test]
    fn parses_iso_and_slash() {
        assert_eq!(ymd(extract_event_date("met on 2023-04-10 today").unwrap()), (2023, 4, 10));
        assert_eq!(ymd(extract_event_date("2023/12/01 (Fri) 09:00").unwrap()), (2023, 12, 1));
    }

    #[test]
    fn parses_month_name_forms() {
        assert_eq!(ymd(extract_event_date("On April 10, 2023 I bought a car.").unwrap()), (2023, 4, 10));
        assert_eq!(ymd(extract_event_date("It happened on 3 Apr 2024.").unwrap()), (2024, 4, 3));
        assert_eq!(ymd(extract_event_date("Due Sept 5 2025 sharp").unwrap()), (2025, 9, 5));
    }

    #[test]
    fn rejects_non_dates_and_relative() {
        assert!(extract_event_date("I'll go yesterday or last March").is_none());
        assert!(extract_event_date("version 2.0.1 of the tool").is_none());
        assert!(extract_event_date("call me at 555 1234").is_none());
        // Out-of-range month/day must not parse.
        assert!(extract_event_date("2023-13-40").is_none());
    }
}
