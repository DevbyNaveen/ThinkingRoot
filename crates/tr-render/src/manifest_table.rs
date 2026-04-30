//! Monospace-friendly ASCII table of manifest fields. Mirrors the
//! UNIX-tool convention of `key  value` pairs separated by a header
//! ruler — copy-paste-friendly into bug reports.

use std::fmt::Write as _;

use tr_format::V3Pack;

pub(crate) fn format(pack: &V3Pack) -> String {
    let m = &pack.manifest;
    let rows: Vec<(&str, String)> = vec![
        ("name", m.name.clone()),
        ("version", m.version.to_string()),
        ("format", m.format_version.clone()),
        (
            "license",
            m.license.clone().unwrap_or_else(|| "-".into()),
        ),
        ("pack_hash", short_hash(&m.pack_hash)),
        ("source_hash", short_hash(&m.source_hash)),
        ("claims_hash", short_hash(&m.claims_hash)),
        (
            "extracted_at",
            m.extracted_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "-".into()),
        ),
        (
            "extractor",
            m.extractor.clone().unwrap_or_else(|| "-".into()),
        ),
        (
            "description",
            truncate(m.description.as_deref().unwrap_or(""), 60),
        ),
        ("authors", join_or_dash(&m.authors)),
        (
            "source_files",
            m.source_files
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        (
            "claim_count",
            m.claim_count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        ("signature", signature_label(pack).to_string()),
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

fn signature_label(pack: &V3Pack) -> &'static str {
    match &pack.signature {
        None => "unsigned",
        Some(b) => match (
            b.verification_material.public_key.as_ref(),
            b.verification_material.x509_certificate_chain.as_ref(),
        ) {
            (None, Some(_)) => "sigstore-keyless",
            (Some(_), None) => "self-signed (ed25519)",
            (Some(_), Some(_)) => "self-signed+sigstore",
            (None, None) => "signed (empty)",
        },
    }
}

fn short_hash(s: &str) -> String {
    if s.chars().count() > 24 {
        let prefix: String = s.chars().take(24).collect();
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
