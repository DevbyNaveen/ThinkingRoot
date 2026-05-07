//! Quantity extractor — Compile Completeness Contract §5.3.
//!
//! Decorates `ExtractedClaim` with a `Vec<ExtractedQuantity>` so Phase 6.7
//! can populate the `quantities` table. Multiple quantities per claim
//! are routine ("p99=120ms at 50K rps").
//!
//! This is a **regex-first hybrid**: a fast deterministic pass extracts
//! `(value, unit, qualifier, is_live)` tuples for the standard SI /
//! financial / web-perf vocabularies. The LLM-prompt addendum
//! (`prompts.rs`) classifies `metric_name` for tuples the regex
//! couldn't categorise — kept simple in v1, no magic-comment surface
//! (per contract §15 Q4).

use std::sync::OnceLock;

use regex::Regex;

use crate::schema::ExtractedQuantity;

/// Extract every quantity tuple from `text`. Byte offsets in the
/// returned tuples are **relative to the start of `text`** (the caller
/// adds `chunk.byte_start` to make them absolute file-local bytes).
pub fn extract(text: &str) -> Vec<ExtractedQuantity> {
    let mut out = Vec::new();
    if text.trim().is_empty() {
        return out;
    }

    let re = quantity_regex();
    for caps in re.captures_iter(text) {
        let Some(value_match) = caps.name("value") else {
            continue;
        };
        let Some(unit_match) = caps.name("unit") else {
            continue;
        };
        let Ok(value) = value_match.as_str().parse::<f64>() else {
            continue;
        };

        // Find the surrounding window for qualifier + is_live detection.
        // The byte offsets must land on UTF-8 char boundaries — text may
        // contain multi-byte chars like '→' (3 bytes) and slicing inside
        // one panics ("byte index N is not a char boundary").  Walk
        // outward to the nearest boundary so we always slice cleanly.
        let raw_start = value_match.start().saturating_sub(24);
        let raw_end = (unit_match.end() + 32).min(text.len());
        let span_start = floor_char_boundary(text, raw_start);
        let span_end = ceil_char_boundary(text, raw_end);
        let window = &text[span_start..span_end];

        let qualifier = detect_qualifier(window);
        let is_live = detect_is_live(window);
        let metric_name = guess_metric_name(unit_match.as_str(), is_live);

        out.push(ExtractedQuantity {
            metric_name,
            value,
            unit: normalise_unit(unit_match.as_str()),
            qualifier,
            is_live,
            byte_start: value_match.start() as u64,
            byte_end: unit_match.end() as u64,
        });
    }
    out
}

/// Walk left until we land on a UTF-8 char boundary.  Stable equivalent
/// of nightly `str::floor_char_boundary` so we can safely slice text
/// that contains multi-byte chars like `→`.
fn floor_char_boundary(text: &str, idx: usize) -> usize {
    let mut i = idx.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Walk right until we land on a UTF-8 char boundary.
fn ceil_char_boundary(text: &str, idx: usize) -> usize {
    let mut i = idx.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn quantity_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // value: integer or decimal.
        // unit: a closed set of recognisable units.
        // trailing `[^A-Za-z0-9_]|$` is consumed in the overall match but
        // not in the named `unit` capture — that lets `%` (a non-word char)
        // get a proper boundary check the way `\b` cannot, since `\b`
        // never matches between two non-word characters.
        Regex::new(
            r"(?P<value>\d+(?:\.\d+)?)\s*(?P<unit>rps|qps|tps|ms|µs|us|ns|seconds?|minutes?|hours?|days?|GB|MB|KB|TB|GiB|MiB|KiB|%|USD|EUR|GBP|users?|reqs?|cores?|GHz|MHz|kHz)(?:[^A-Za-z0-9_]|$)",
        )
        .expect("valid quantity regex")
    })
}

fn detect_qualifier(window: &str) -> String {
    let re = qualifier_regex();
    if let Some(caps) = re.captures(window) {
        if let Some(m) = caps.get(1) {
            return m.as_str().to_lowercase();
        }
    }
    String::new()
}

fn qualifier_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\b(p99|p95|p90|p50|max|min|avg|mean|median|peak|monthly|daily|weekly|yearly)\b")
            .expect("valid qualifier regex")
    })
}

fn detect_is_live(window: &str) -> bool {
    let re = live_regex();
    re.is_match(window)
}

fn live_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:rate|rps|qps|tps|throughput|current|live|now|present|demand|active|in[- ]flight)\b",
        )
        .expect("valid live regex")
    })
}

fn guess_metric_name(unit: &str, is_live: bool) -> String {
    let unit_lower = unit.to_lowercase();
    match unit_lower.as_str() {
        "rps" | "qps" | "tps" => "throughput".to_string(),
        "ms" | "µs" | "us" | "ns" | "seconds" | "second" => "latency".to_string(),
        "minutes" | "minute" | "hours" | "hour" | "days" | "day" => "duration".to_string(),
        "gb" | "mb" | "kb" | "tb" | "gib" | "mib" | "kib" => "bytes".to_string(),
        "%" => "share".to_string(),
        "usd" | "eur" | "gbp" => "price".to_string(),
        "users" | "user" => "count".to_string(),
        "reqs" | "req" => {
            if is_live { "throughput".to_string() } else { "count".to_string() }
        }
        "cores" | "core" => "compute".to_string(),
        "ghz" | "mhz" | "khz" => "frequency".to_string(),
        _ => String::new(),
    }
}

fn normalise_unit(unit: &str) -> String {
    let lower = unit.to_lowercase();
    match lower.as_str() {
        "us" => "µs".to_string(),
        "second" | "seconds" => "s".to_string(),
        "minute" | "minutes" => "min".to_string(),
        "hour" | "hours" => "h".to_string(),
        "day" | "days" => "d".to_string(),
        "user" | "users" => "users".to_string(),
        "req" | "reqs" => "reqs".to_string(),
        "core" | "cores" => "cores".to_string(),
        // Casing-preserving units.
        "rps" | "qps" | "tps" | "ms" | "µs" | "ns" | "%" => lower,
        "gb" | "mb" | "kb" | "tb" => unit.to_uppercase(),
        "gib" | "mib" | "kib" => {
            // Title-case (first letter cap, rest lower).
            let mut chars = unit.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
        "usd" | "eur" | "gbp" => unit.to_uppercase(),
        "ghz" | "mhz" | "khz" => unit.to_string(), // preserve original casing
        _ => unit.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        assert!(extract("").is_empty());
    }

    #[test]
    fn does_not_panic_on_multibyte_char_near_match_boundary() {
        // Pins the production panic surfaced 2026-05-07 from the
        // thinkingroot-cloud workspace compile: "byte index 55 is not a
        // char boundary; it is inside '→' (bytes 53..56) of …".  The
        // window slice around the matched quantity (`60 minutes`) was
        // landing 24 bytes upstream — straight inside the 3-byte `→`.
        let text = "Integration tests must cover publish pack → revoke → client refuses within 60 minutes";
        let qs = extract(text); // must not panic
        assert_eq!(qs.len(), 1, "expected one quantity (60 minutes)");
        assert_eq!(qs[0].value, 60.0);
        assert_eq!(qs[0].unit, "min"); // normalise_unit: minutes → min
    }

    #[test]
    fn does_not_panic_on_multibyte_char_around_match() {
        // Stress: the `→` arrow is in the trailing window too.
        let text = "Within 30 seconds → user gets receipt → push notification fires";
        let qs = extract(text);
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].value, 30.0);
        assert_eq!(qs[0].unit, "s"); // normalise_unit: seconds → s
    }

    #[test]
    fn does_not_panic_on_multibyte_at_text_start() {
        // Match at the very start with multibyte chars at end.
        let text = "5 ms → upstream → downstream";
        let qs = extract(text); // must not panic
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].value, 5.0);
    }

    #[test]
    fn no_quantity() {
        assert!(extract("Rust is fast.").is_empty());
    }

    #[test]
    fn rps_classified_as_throughput_and_live() {
        let qs = extract("Endpoint sustains 50000 rps under load.");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].value, 50000.0);
        assert_eq!(qs[0].unit, "rps");
        assert_eq!(qs[0].metric_name, "throughput");
        assert!(qs[0].is_live);
    }

    #[test]
    fn ms_classified_as_latency_with_p99_qualifier() {
        let qs = extract("p99 latency is 120 ms.");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].value, 120.0);
        assert_eq!(qs[0].unit, "ms");
        assert_eq!(qs[0].metric_name, "latency");
        assert_eq!(qs[0].qualifier, "p99");
    }

    #[test]
    fn usd_classified_as_price() {
        let qs = extract("Cost is 0.05 USD per request.");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].value, 0.05);
        assert_eq!(qs[0].unit, "USD");
        assert_eq!(qs[0].metric_name, "price");
    }

    #[test]
    fn percent_classified_as_share() {
        let qs = extract("Cache hit ratio is 95%.");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].value, 95.0);
        assert_eq!(qs[0].unit, "%");
        assert_eq!(qs[0].metric_name, "share");
    }

    #[test]
    fn multiple_quantities_in_one_statement() {
        let qs = extract("p99=120 ms at 50000 rps");
        assert_eq!(qs.len(), 2);
        assert!(qs.iter().any(|q| q.unit == "ms" && q.qualifier == "p99"));
        assert!(qs.iter().any(|q| q.unit == "rps" && q.is_live));
    }

    #[test]
    fn byte_offsets_relative_to_input() {
        let txt = "padding 50000 rps tail";
        let qs = extract(txt);
        assert_eq!(qs.len(), 1);
        assert!(qs[0].byte_start > 0);
        assert!(qs[0].byte_end <= txt.len() as u64);
        // Sanity: the slice is exactly "50000 rps".
        let s = qs[0].byte_start as usize;
        let e = qs[0].byte_end as usize;
        assert_eq!(&txt[s..e], "50000 rps");
    }

    #[test]
    fn gb_normalised_uppercase() {
        let qs = extract("RAM is 16 GB.");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].unit, "GB");
        assert_eq!(qs[0].metric_name, "bytes");
    }
}
