// crates/thinkingroot-serve/src/intelligence/temporal.rs
//
// Temporal anchor pre-computation — zero LLM calls.
//
// Key insight (from LongMemEval R&D, Round 3):
// LLMs are unreliable at calendar arithmetic ("last Saturday" → wrong date).
// Pre-computing anchor dates in Rust chrono and injecting them as a
// "PRE-COMPUTED DATE REFERENCES" section eliminates the arithmetic step
// entirely and pushes temporal-reasoning accuracy from ~72% to 87.2%.

use std::collections::HashMap;

use chrono::{Datelike, Duration, NaiveDate, Weekday};

/// Parse "2023/05/30 (Tue) 22:10" or "2023/05/30" → NaiveDate.
pub fn parse_question_date(s: &str) -> Option<NaiveDate> {
    let date_part = s.split_whitespace().next()?;
    NaiveDate::parse_from_str(date_part, "%Y/%m/%d").ok()
}

/// Find the most recent occurrence of `target` weekday strictly before `from`.
fn last_weekday_before(from: NaiveDate, target: Weekday) -> NaiveDate {
    let mut d = from - Duration::days(1);
    while d.weekday() != target {
        d -= Duration::days(1);
    }
    d
}

fn word_to_number(word: &str) -> Option<i64> {
    match word {
        "one" | "a" | "an" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "six" => Some(6),
        "seven" => Some(7),
        "eight" => Some(8),
        "nine" => Some(9),
        "ten" => Some(10),
        "eleven" => Some(11),
        "twelve" => Some(12),
        _ => word.parse::<i64>().ok(),
    }
}

/// Build a "PRE-COMPUTED DATE REFERENCES" block for temporal questions.
///
/// Parses the question for patterns like "last Saturday", "3 weeks ago", etc.
/// and computes concrete dates so the LLM does not need to perform calendar
/// arithmetic. Also emits a session date timeline sorted by proximity to today.
///
/// Returns an empty string when `question_date` is unparseable (non-temporal
/// questions can safely receive an empty string).
pub fn compute_temporal_anchors(
    question: &str,
    question_date: &str,
    session_dates: &HashMap<String, String>,
    answer_sids: &[String],
) -> String {
    let today = match parse_question_date(question_date) {
        Some(d) => d,
        None => return String::new(),
    };

    let q_lower = question.to_lowercase();
    let mut out = format!(
        "## PRE-COMPUTED DATE REFERENCES\nTODAY = {} ({:?})\n\n",
        today.format("%Y-%m-%d"),
        today.weekday()
    );
    let mut found = false;

    // "last [weekday]" / "past [weekday]"
    const WEEKDAYS: &[(&str, Weekday)] = &[
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
    ];
    for (name, wd) in WEEKDAYS {
        if q_lower.contains(&format!("last {name}")) || q_lower.contains(&format!("past {name}")) {
            let d = last_weekday_before(today, *wd);
            out.push_str(&format!("\"Last {}\" = {}\n", name, d.format("%Y-%m-%d")));
            found = true;
        }
    }

    // "past weekend" / "last weekend"
    if q_lower.contains("past weekend") || q_lower.contains("last weekend") {
        let sat = last_weekday_before(today, Weekday::Sat);
        let sun = sat + Duration::days(1);
        out.push_str(&format!(
            "\"Past weekend\" = {} to {}\n",
            sat.format("%Y-%m-%d"),
            sun.format("%Y-%m-%d")
        ));
        found = true;
    }

    // Scan tokens for "N days/weeks/months ago"
    let words: Vec<&str> = q_lower.split_whitespace().collect();
    for i in 0..words.len() {
        if let Some(n) = word_to_number(words[i]) {
            let unit = words.get(i + 1).copied().unwrap_or("");
            let after_unit = words.get(i + 2).copied().unwrap_or("");
            let after_after = words.get(i + 3).copied().unwrap_or("");
            let is_ago = after_unit == "ago"
                || after_after == "ago"
                || after_unit.starts_with("ago")
                || after_after.starts_with("ago");

            if unit.starts_with("day") && is_ago {
                let d = today - Duration::days(n);
                out.push_str(&format!("{} day(s) ago = {}\n", n, d.format("%Y-%m-%d")));
                found = true;
            } else if unit.starts_with("week") && is_ago {
                let d = today - Duration::weeks(n);
                out.push_str(&format!("{} week(s) ago = {}\n", n, d.format("%Y-%m-%d")));
                found = true;
            } else if unit.starts_with("month") && is_ago {
                // Exact calendar-month arithmetic (was a 30-day approximation,
                // which drifted by days over multi-month spans).
                if let Some(d) = today.checked_sub_months(chrono::Months::new(n.max(0) as u32)) {
                    out.push_str(&format!("{} month(s) ago = {}\n", n, d.format("%Y-%m-%d")));
                    found = true;
                }
            }
        }
    }

    // Session timeline — sorted by proximity to TODAY so the LLM can order events
    out.push_str("\nSESSION DATE TIMELINE:\n");
    let mut timeline: Vec<(String, NaiveDate, i64)> = answer_sids
        .iter()
        .filter_map(|asid| {
            let date_str = session_dates
                .iter()
                .find(|(sid, _)| asid.contains(sid.as_str()) || sid.contains(asid.as_str()))
                .map(|(_, d)| d.clone())
                .unwrap_or_default();
            parse_question_date(&date_str).map(|d| {
                let delta = (today - d).num_days();
                (asid.clone(), d, delta)
            })
        })
        .collect();
    timeline.sort_by_key(|(_, _, delta)| *delta);

    for (asid, date, delta) in &timeline {
        out.push_str(&format!(
            "  {}: {} ({} days before TODAY)\n",
            asid,
            date.format("%Y-%m-%d"),
            delta
        ));
        found = true;
    }
    out.push('\n');

    if found { out } else { String::new() }
}

/// Cap on rendered calendar rows — a dozen-plus dated events is signal, a
/// hundred is noise the reader has to wade through.
pub const EVENT_CALENDAR_MAX: usize = 15;

/// Build the `## EVENT CALENDAR (datetime-verified)` block for temporal
/// questions — the datetime substrate doing the two things the LLM is worst
/// at (calendar deltas + event-by-date ordering) in Rust chrono instead of
/// prose.
///
/// `events` = `(statement, event_date_epoch_secs)` pairs from claims that
/// carry a real compile-extracted `event_date`. Rows render sorted by
/// proximity to the question date with an exact pre-computed delta
/// ("3 day(s) before TODAY"). Returns `""` when the question date is
/// unparseable or no event carries a date — the prompt is then byte-identical
/// to the calendar-off path (honest degradation, never a fabricated date).
pub fn build_event_calendar(question_date: &str, events: &[(String, f64)], max: usize) -> String {
    let Some(today) = parse_question_date(question_date) else {
        return String::new();
    };
    let mut rows: Vec<(NaiveDate, i64, &str)> = events
        .iter()
        .filter_map(|(statement, epoch)| {
            if *epoch <= 0.0 {
                return None;
            }
            let d = chrono::DateTime::from_timestamp(*epoch as i64, 0)?.date_naive();
            let delta = (today - d).num_days();
            Some((d, delta, statement.as_str()))
        })
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    // Nearest-to-TODAY first; ties break older-first for a stable render.
    rows.sort_by_key(|(d, delta, _)| (delta.abs(), *d));
    rows.truncate(max);

    let mut out = String::from("## EVENT CALENDAR (datetime-verified)\n");
    for (d, delta, statement) in &rows {
        let rel = match delta {
            0 => "TODAY".to_string(),
            n if *n > 0 => format!("{n} day(s) before TODAY"),
            n => format!("{} day(s) after TODAY", -n),
        };
        out.push_str(&format!("- {} ({rel}): {statement}\n", d.format("%Y-%m-%d")));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_datetime() {
        let d = parse_question_date("2023/05/30 (Tue) 22:10").unwrap();
        assert_eq!(d.to_string(), "2023-05-30");
    }

    #[test]
    fn parse_date_only() {
        let d = parse_question_date("2023/01/15").unwrap();
        assert_eq!(d.to_string(), "2023-01-15");
    }

    #[test]
    fn last_weekday() {
        // 2023-05-30 is a Tuesday. Last Saturday = 2023-05-27.
        let today = NaiveDate::from_ymd_opt(2023, 5, 30).unwrap();
        let sat = last_weekday_before(today, Weekday::Sat);
        assert_eq!(sat.to_string(), "2023-05-27");
    }

    #[test]
    fn compute_anchors_last_saturday() {
        let anchors = compute_temporal_anchors(
            "What did I do last Saturday?",
            "2023/05/30",
            &HashMap::new(),
            &[],
        );
        assert!(
            anchors.contains("2023-05-27"),
            "Expected last Saturday date in: {anchors}"
        );
    }

    #[test]
    fn compute_anchors_n_days_ago() {
        let anchors = compute_temporal_anchors(
            "What did I buy 3 days ago?",
            "2023/05/30",
            &HashMap::new(),
            &[],
        );
        assert!(
            anchors.contains("2023-05-27"),
            "Expected 3 days ago date in: {anchors}"
        );
    }

    #[test]
    fn compute_anchors_empty_on_bad_date() {
        let anchors = compute_temporal_anchors("What happened?", "", &HashMap::new(), &[]);
        assert!(anchors.is_empty());
    }

    #[test]
    fn compute_anchors_months_ago_is_calendar_exact() {
        // 2023-05-30 minus 3 calendar months = 2023-02-28 (Feb has no 30th).
        // The old 30-day approximation produced 2023-03-01 — off by a day and
        // into the wrong month.
        let anchors = compute_temporal_anchors(
            "What did I do 3 months ago?",
            "2023/05/30",
            &HashMap::new(),
            &[],
        );
        assert!(
            anchors.contains("2023-02-28"),
            "Expected exact calendar-month date in: {anchors}"
        );
    }

    #[test]
    fn event_calendar_sorts_by_proximity_and_computes_deltas() {
        // TODAY = 2023-05-30. Epochs: 2023-05-27 (3 before), 2023-04-10
        // (50 before), 2023-06-02 (3 after).
        let d = |y: i32, m: u32, day: u32| -> f64 {
            NaiveDate::from_ymd_opt(y, m, day)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp() as f64
        };
        let events = vec![
            ("bought hiking boots".to_string(), d(2023, 4, 10)),
            ("attended sister's wedding".to_string(), d(2023, 5, 27)),
            ("dentist appointment".to_string(), d(2023, 6, 2)),
        ];
        let cal = build_event_calendar("2023/05/30", &events, EVENT_CALENDAR_MAX);
        assert!(cal.starts_with("## EVENT CALENDAR (datetime-verified)"));
        assert!(cal.contains("2023-05-27 (3 day(s) before TODAY): attended sister's wedding"));
        assert!(cal.contains("2023-06-02 (3 day(s) after TODAY): dentist appointment"));
        assert!(cal.contains("2023-04-10 (50 day(s) before TODAY): bought hiking boots"));
        // Proximity order: the wedding (|3|, older tie-break) before boots (|50|).
        let wedding = cal.find("wedding").unwrap();
        let boots = cal.find("boots").unwrap();
        assert!(wedding < boots, "nearest event must render first:\n{cal}");
    }

    #[test]
    fn event_calendar_empty_on_no_dated_events_or_bad_today() {
        assert!(build_event_calendar("", &[("x".into(), 1.0)], 15).is_empty());
        assert!(build_event_calendar("2023/05/30", &[], 15).is_empty());
        // Zero/default epochs are omitted → empty, never fabricated.
        assert!(build_event_calendar("2023/05/30", &[("x".into(), 0.0)], 15).is_empty());
    }

    #[test]
    fn event_calendar_caps_rows() {
        let events: Vec<(String, f64)> = (1..=30)
            .map(|i| {
                let epoch = NaiveDate::from_ymd_opt(2023, 4, i.min(30) as u32)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp() as f64;
                (format!("event {i}"), epoch)
            })
            .collect();
        let cal = build_event_calendar("2023/05/30", &events, EVENT_CALENDAR_MAX);
        assert_eq!(cal.matches("- 2023-").count(), EVENT_CALENDAR_MAX);
    }
}
