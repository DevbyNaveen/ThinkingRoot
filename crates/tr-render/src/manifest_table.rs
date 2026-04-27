//! Monospace-friendly ASCII table of manifest fields. Mirrors the
//! UNIX-tool convention of `key  value` pairs separated by a header
//! ruler — copy-paste-friendly into bug reports.

use std::fmt::Write as _;

use tr_format::Manifest;

pub(crate) fn format(manifest: &Manifest) -> String {
    let rows: Vec<(&str, String)> = vec![
        ("name", manifest.name.clone()),
        ("version", manifest.version.to_string()),
        ("license", manifest.license.clone()),
        ("trust_tier", trust_tier_str(manifest.trust_tier)),
        ("content_hash", short_hash(&manifest.content_hash)),
        ("generated_at", manifest.generated_at.to_rfc3339()),
        ("description", truncate(&manifest.description, 60)),
        ("authors", join_or_dash(&manifest.authors)),
        ("tags", join_or_dash(&manifest.tags)),
        (
            "claim_count",
            manifest
                .claim_count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        (
            "rooted_pct",
            manifest
                .rooted_pct
                .map(|p| format!("{p:.1}%"))
                .unwrap_or_else(|| "-".into()),
        ),
        ("capabilities", manifest.capabilities.summary()),
    ];

    let key_width = rows
        .iter()
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0)
        .max("key".len());
    let val_width = rows
        .iter()
        .map(|(_, v)| v.chars().count())
        .max()
        .unwrap_or(0)
        .max("value".len());

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<key_width$}  {:<val_width$}",
        "key", "value",
        key_width = key_width,
        val_width = val_width
    );
    let _ = writeln!(
        out,
        "{}  {}",
        "-".repeat(key_width),
        "-".repeat(val_width)
    );
    for (k, v) in rows {
        let _ = writeln!(
            out,
            "{:<key_width$}  {:<val_width$}",
            k, v,
            key_width = key_width,
            val_width = val_width
        );
    }
    out
}

fn trust_tier_str(tier: tr_format::TrustTier) -> String {
    use tr_format::TrustTier::*;
    match tier {
        T0 => "T0",
        T1 => "T1",
        T2 => "T2",
        T3 => "T3",
        T4 => "T4",
    }
    .to_string()
}

fn short_hash(s: &str) -> String {
    if s.chars().count() > 16 {
        let prefix: String = s.chars().take(16).collect();
        format!("{prefix}…")
    } else if s.is_empty() {
        "-".into()
    } else {
        s.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    } else if s.is_empty() {
        "-".into()
    } else {
        s.to_string()
    }
}

fn join_or_dash(v: &[String]) -> String {
    if v.is_empty() {
        "-".into()
    } else {
        v.join(", ")
    }
}
