//! Expiration extractor — Compile Completeness Contract §5.2.
//!
//! Decorates `ExtractedClaim` with `valid_until` (ISO-8601 absolute date)
//! and `expiration_signal` (typed shape) so Phase 6.7 can populate
//! `claim_temporal.valid_until`. AEP rule `rule_temporal_collapse`
//! (validated at `crates/thinkingroot-graph/src/graph.rs:6166-6182`)
//! filters expired claims via this column — pre-contract that column
//! was always `0.0` (the never-expires sentinel) so collapse was a
//! no-op in practice.
//!
//! The extractor is regex-first; the LLM-prompt addendum (in
//! `prompts.rs`) is a fallback for phrasings the regex doesn't catch.
//! When a `HardDate` / `RelativeWindow` / `Recurring` signal is
//! detected, `valid_until` is computed at extract time so the column
//! lands a concrete ISO date the AEP rule can compare against.
//! `VersionGate` and `Unknown` shapes leave `valid_until = None` —
//! they signal "this claim's lifetime is conditional on something
//! outside the DB" and AEP surfaces them as caveats.

use std::sync::OnceLock;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use regex::Regex;
use thinkingroot_core::types::{ExpirationSignal, RecurringPattern};

/// Combined extraction result — the typed signal plus its derived
/// ISO-8601 absolute date when one exists.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedExpiration {
    pub signal: ExpirationSignal,
    pub valid_until: Option<String>,
}

/// Try to extract an expiration signal from a claim statement (or chunk
/// content). `now` is the reference time used to anchor relative
/// windows ("valid for 30 days") — pass `Utc::now()` from extractors.
/// Returns `None` when no expiration phrasing is detected.
pub fn extract(text: &str, now: DateTime<Utc>) -> Option<ExtractedExpiration> {
    if let Some(date) = match_hard_date(text) {
        let iso = date.format("%Y-%m-%d").to_string();
        return Some(ExtractedExpiration {
            signal: ExpirationSignal::HardDate { iso_date: iso.clone() },
            valid_until: Some(iso),
        });
    }
    if let Some((duration, _phrase)) = match_relative_window(text) {
        let valid_until = now + duration;
        return Some(ExtractedExpiration {
            signal: ExpirationSignal::RelativeWindow {
                duration_secs: duration.num_seconds(),
            },
            valid_until: Some(valid_until.format("%Y-%m-%d").to_string()),
        });
    }
    if let Some(pattern) = match_recurring(text) {
        // For recurring signals we anchor `valid_until` to the next fire
        // (now + one period). AEP can re-evaluate per probe — the
        // `Recurring` shape preserves the period explicitly.
        let next = now + recurring_to_duration(pattern);
        return Some(ExtractedExpiration {
            signal: ExpirationSignal::Recurring { pattern },
            valid_until: Some(next.format("%Y-%m-%d").to_string()),
        });
    }
    if let Some(semver) = match_version_gate(text) {
        return Some(ExtractedExpiration {
            signal: ExpirationSignal::VersionGate { semver },
            valid_until: None, // Tied to deployed-version state, not a date.
        });
    }
    if has_expiry_hint(text) {
        return Some(ExtractedExpiration {
            signal: ExpirationSignal::Unknown,
            valid_until: None,
        });
    }
    None
}

// ─── HardDate matching ───────────────────────────────────────────────────

fn match_hard_date(text: &str) -> Option<NaiveDate> {
    let re = hard_date_regex();
    let caps = re.captures(text)?;
    let iso = caps.get(1)?.as_str();
    NaiveDate::parse_from_str(iso, "%Y-%m-%d").ok()
}

fn hard_date_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // "until 2026-12-31", "expires on 2026-12-31", "before 2026-12-31",
        // "effective until 2026-12-31", "valid until 2026-12-31".
        Regex::new(
            r"(?i)\b(?:until|expires?\s+on|expires?\s+at|before|effective\s+until|valid\s+until)\s+(\d{4}-\d{2}-\d{2})\b",
        )
        .expect("valid hard-date regex")
    })
}

// ─── RelativeWindow matching ─────────────────────────────────────────────

fn match_relative_window(text: &str) -> Option<(Duration, String)> {
    let re = relative_window_regex();
    let caps = re.captures(text)?;
    let n: i64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str().to_lowercase();
    let duration = match unit.as_str() {
        "day" | "days" => Duration::days(n),
        "week" | "weeks" => Duration::weeks(n),
        "month" | "months" => Duration::days(n * 30), // approx — AEP can refine
        "year" | "years" => Duration::days(n * 365),
        "hour" | "hours" => Duration::hours(n),
        _ => return None,
    };
    Some((duration, caps.get(0)?.as_str().to_string()))
}

fn relative_window_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // "valid for 30 days", "expires in 6 months", "for the next 24 hours".
        Regex::new(
            r"(?i)\b(?:valid\s+for|expires?\s+in|for\s+the\s+next)\s+(\d+)\s+(day|days|week|weeks|month|months|year|years|hour|hours)\b",
        )
        .expect("valid relative-window regex")
    })
}

// ─── Recurring matching ──────────────────────────────────────────────────

fn match_recurring(text: &str) -> Option<RecurringPattern> {
    let re_named = recurring_named_regex();
    if let Some(caps) = re_named.captures(text) {
        let kind = caps.get(1)?.as_str().to_lowercase();
        return match kind.as_str() {
            "daily" | "day" => Some(RecurringPattern::Daily),
            "weekly" | "week" => Some(RecurringPattern::Weekly),
            "monthly" | "month" => Some(RecurringPattern::Monthly),
            "quarterly" | "quarter" => Some(RecurringPattern::Quarterly),
            "yearly" | "annually" | "year" => Some(RecurringPattern::Yearly),
            _ => None,
        };
    }
    let re_n_days = recurring_n_days_regex();
    if let Some(caps) = re_n_days.captures(text) {
        let n: u32 = caps.get(1)?.as_str().parse().ok()?;
        return Some(RecurringPattern::EveryNDays { n });
    }
    let re_n_hours = recurring_n_hours_regex();
    if let Some(caps) = re_n_hours.captures(text) {
        let n: u32 = caps.get(1)?.as_str().parse().ok()?;
        return Some(RecurringPattern::EveryNHours { n });
    }
    None
}

fn recurring_named_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // "rotates monthly", "every day", "weekly review".
        Regex::new(
            r"(?i)\b(?:rotates?\s+|every\s+|once\s+a\s+|each\s+)(daily|day|weekly|week|monthly|month|quarterly|quarter|yearly|annually|year)\b",
        )
        .expect("valid recurring-named regex")
    })
}

fn recurring_n_days_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // "rotates every 30 days", "every 7 days".
        Regex::new(r"(?i)\b(?:rotates?\s+every|every)\s+(\d+)\s+days?\b").expect("valid n-days regex")
    })
}

fn recurring_n_hours_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\bevery\s+(\d+)\s+hours?\b").expect("valid n-hours regex")
    })
}

fn recurring_to_duration(pattern: RecurringPattern) -> Duration {
    match pattern {
        RecurringPattern::Daily => Duration::days(1),
        RecurringPattern::Weekly => Duration::weeks(1),
        RecurringPattern::Monthly => Duration::days(30),
        RecurringPattern::Quarterly => Duration::days(90),
        RecurringPattern::Yearly => Duration::days(365),
        RecurringPattern::EveryNDays { n } => Duration::days(n as i64),
        RecurringPattern::EveryNHours { n } => Duration::hours(n as i64),
    }
}

// ─── VersionGate matching ────────────────────────────────────────────────

fn match_version_gate(text: &str) -> Option<String> {
    let re = version_gate_regex();
    let caps = re.captures(text)?;
    Some(caps.get(1)?.as_str().to_string())
}

fn version_gate_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // "deprecated in v2.0", "removed in v1.5.3", "as of v3".
        Regex::new(
            r"(?i)\b(?:deprecated|removed|sunset|as\s+of)\s+in\s+v?(\d+(?:\.\d+){0,2})\b|\bas\s+of\s+v(\d+(?:\.\d+){0,2})\b",
        )
        .expect("valid version-gate regex")
    })
}

// ─── Catch-all "expires" hint ────────────────────────────────────────────

fn has_expiry_hint(text: &str) -> bool {
    let re = expiry_hint_regex();
    re.is_match(text)
}

fn expiry_hint_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\b(?:expires?|sunset|deprecat\w+|temporary|transitional|interim)\b")
            .expect("valid hint regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-02T00:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn no_signal_returns_none() {
        assert!(extract("Rust is a systems language.", fixed_now()).is_none());
    }

    #[test]
    fn hard_date_until() {
        let r = extract("Token rotates manually until 2026-12-31.", fixed_now()).unwrap();
        assert_eq!(
            r.signal,
            ExpirationSignal::HardDate { iso_date: "2026-12-31".into() }
        );
        assert_eq!(r.valid_until.as_deref(), Some("2026-12-31"));
    }

    #[test]
    fn hard_date_effective_until() {
        let r = extract("Effective until 2027-01-15, all probes use the new gate.", fixed_now()).unwrap();
        assert!(matches!(r.signal, ExpirationSignal::HardDate { .. }));
        assert_eq!(r.valid_until.as_deref(), Some("2027-01-15"));
    }

    #[test]
    fn relative_window_days() {
        let r = extract("Cache entries are valid for 30 days.", fixed_now()).unwrap();
        match r.signal {
            ExpirationSignal::RelativeWindow { duration_secs } => {
                assert_eq!(duration_secs, 30 * 86400);
            }
            other => panic!("expected RelativeWindow, got {other:?}"),
        }
        // 2026-05-02 + 30 days = 2026-06-01
        assert_eq!(r.valid_until.as_deref(), Some("2026-06-01"));
    }

    #[test]
    fn relative_window_months() {
        let r = extract("Expires in 6 months.", fixed_now()).unwrap();
        assert!(matches!(r.signal, ExpirationSignal::RelativeWindow { .. }));
        // 6 * 30 = 180 days approx.
        assert!(r.valid_until.is_some());
    }

    #[test]
    fn recurring_named() {
        let r = extract("Key rotates monthly per the runbook.", fixed_now()).unwrap();
        assert_eq!(
            r.signal,
            ExpirationSignal::Recurring { pattern: RecurringPattern::Monthly }
        );
        assert!(r.valid_until.is_some());
    }

    #[test]
    fn recurring_every_n_days() {
        let r = extract("Rotates every 7 days.", fixed_now()).unwrap();
        match r.signal {
            ExpirationSignal::Recurring { pattern: RecurringPattern::EveryNDays { n } } => {
                assert_eq!(n, 7);
            }
            other => panic!("expected EveryNDays, got {other:?}"),
        }
    }

    #[test]
    fn version_gate_no_valid_until() {
        let r = extract("Deprecated in v2.0; use the new client.", fixed_now()).unwrap();
        assert_eq!(
            r.signal,
            ExpirationSignal::VersionGate { semver: "2.0".into() }
        );
        assert!(r.valid_until.is_none());
    }

    #[test]
    fn unknown_hint_falls_through() {
        let r = extract("This is a temporary workaround.", fixed_now()).unwrap();
        assert_eq!(r.signal, ExpirationSignal::Unknown);
        assert!(r.valid_until.is_none());
    }
}
