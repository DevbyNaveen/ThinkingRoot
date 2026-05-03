//! DSL parser for inline typed-predicate strings.
//!
//! Lets callers express structured filters as text:
//!
//! ```text
//! entity:Service:AuthService AND doctag:@deprecated AND authored_by:alice
//! quantity:rps[>10000] AND source.trust>=Verified
//! markers:TODO,FIXME AND in_heading:Architecture/Auth
//! ```
//!
//! Multiple terms separated by ` AND ` (case-insensitive). No OR in v1
//! (spec §17 Q1 — keep routing rigid for predictable latency). Returns
//! `Vec<TypedPredicate>` that the planner intersects with AND semantics.
//!
//! Spec: `docs/2026-05-02-hybrid-retrieval-spec.md` §4.3.

use chrono::{DateTime, Utc};
use thinkingroot_core::types::TrustLevel;

use super::hybrid_types::TypedPredicate;

/// Errors returned by the DSL parser. Wrapped at call sites into the engine
/// `Error::InvalidInput` variant so they propagate cleanly through MCP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DslError(pub String);

impl std::fmt::Display for DslError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DSL parse: {}", self.0)
    }
}

impl std::error::Error for DslError {}

/// Parse an inline DSL string into a list of typed predicates. Empty input
/// returns an empty list (no error — callers handle the "no DSL" path).
pub fn parse(input: &str) -> Result<Vec<TypedPredicate>, DslError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for term in split_and(trimmed) {
        out.push(parse_term(term.trim())?);
    }
    Ok(out)
}

fn split_and(input: &str) -> Vec<&str> {
    // Case-insensitive split on the literal token ` AND ` (with bracketing
    // spaces). We don't try to handle quoted strings — predicates' values
    // can't legitimately contain ` AND `.
    let lower = input.to_ascii_lowercase();
    let mut parts = Vec::new();
    let mut last = 0usize;
    let needle = " and ";
    let mut start = 0usize;
    while let Some(rel) = lower[start..].find(needle) {
        let abs = start + rel;
        parts.push(&input[last..abs]);
        last = abs + needle.len();
        start = last;
    }
    parts.push(&input[last..]);
    parts
}

fn parse_term(term: &str) -> Result<TypedPredicate, DslError> {
    if term.is_empty() {
        return Err(DslError("empty term".into()));
    }

    // `source.trust>=Level` has a different shape (no leading `kind:`).
    if let Some(rest) = term.strip_prefix("source.trust>=") {
        let lvl = parse_trust_level(rest.trim())?;
        return Ok(TypedPredicate::SourceTrustAtLeast { value: lvl });
    }

    let (kind, body) = term
        .split_once(':')
        .ok_or_else(|| DslError(format!("term has no `kind:` prefix: {term}")))?;
    let kind = kind.trim();
    let body = body.trim();
    match kind {
        "entity" => parse_entity(body),
        "claim_type" => Ok(TypedPredicate::ClaimType { value: body.into() }),
        "doctag" => parse_doctag(body),
        "authored_by" => Ok(TypedPredicate::AuthoredBy { value: body.into() }),
        "authored_after" => parse_authored_after(body),
        "in_call_graph_of" => parse_in_call_graph(body),
        "markers" => parse_markers(body),
        "quantity" => parse_quantity(body),
        "in_heading" => parse_heading(body),
        "supersedes" => parse_supersedes(body),
        "references" => parse_references(body),
        other => Err(DslError(format!("unknown predicate kind: {other}"))),
    }
}

fn parse_entity(body: &str) -> Result<TypedPredicate, DslError> {
    // Two valid forms:
    //   entity:Service                → EntityType
    //   entity:Service:AuthService    → EntityType + EntityName  (joined elsewhere)
    // For the AND-joined form, the parser returns EntityName when both halves
    // are present so the predicate intersection narrows to claims with both
    // type AND name. EntityType-only is the bare form.
    if let Some((etype, ename)) = body.split_once(':') {
        if !etype.is_empty() && !ename.is_empty() {
            return Ok(TypedPredicate::EntityName { value: ename.into() });
        }
    }
    if body.is_empty() {
        return Err(DslError("entity:<type> requires a type".into()));
    }
    Ok(TypedPredicate::EntityType { value: body.into() })
}

fn parse_doctag(body: &str) -> Result<TypedPredicate, DslError> {
    // Forms: doctag:@deprecated, doctag:@param:foo
    let stripped = body.strip_prefix('@').unwrap_or(body);
    if let Some((tag, target)) = stripped.split_once(':') {
        Ok(TypedPredicate::HasDocTag {
            tag_kind: tag.into(),
            target: Some(target.into()),
        })
    } else {
        Ok(TypedPredicate::HasDocTag {
            tag_kind: stripped.into(),
            target: None,
        })
    }
}

fn parse_authored_after(body: &str) -> Result<TypedPredicate, DslError> {
    // Accept RFC 3339 (e.g., 2026-01-01T00:00:00Z) or bare YYYY-MM-DD.
    let dt = if body.len() == 10 {
        let with_time = format!("{body}T00:00:00Z");
        DateTime::parse_from_rfc3339(&with_time)
            .map_err(|e| DslError(format!("authored_after: {e}")))?
            .with_timezone(&Utc)
    } else {
        DateTime::parse_from_rfc3339(body)
            .map_err(|e| DslError(format!("authored_after: {e}")))?
            .with_timezone(&Utc)
    };
    Ok(TypedPredicate::AuthoredAfter { value: dt })
}

fn parse_in_call_graph(body: &str) -> Result<TypedPredicate, DslError> {
    // Form: in_call_graph_of:login@3
    let (name, depth) = match body.split_once('@') {
        Some((n, d)) => {
            let depth: u8 = d
                .parse()
                .map_err(|e| DslError(format!("in_call_graph_of depth: {e}")))?;
            (n.to_string(), depth)
        }
        None => (body.to_string(), 1u8),
    };
    Ok(TypedPredicate::InCallGraphOf {
        entity_name: name,
        depth,
    })
}

fn parse_markers(body: &str) -> Result<TypedPredicate, DslError> {
    let kinds: Vec<String> = body
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if kinds.is_empty() {
        return Err(DslError("markers: requires at least one kind".into()));
    }
    Ok(TypedPredicate::HasMarker { kinds })
}

fn parse_quantity(body: &str) -> Result<TypedPredicate, DslError> {
    // Forms:
    //   quantity:rps[>10000]
    //   quantity:rps[5000..10000]
    //   quantity:rps[<=200]
    let (metric, range) = body
        .split_once('[')
        .ok_or_else(|| DslError(format!("quantity expects [bounds]: {body}")))?;
    let range = range
        .strip_suffix(']')
        .ok_or_else(|| DslError(format!("quantity bounds missing closing ]: {body}")))?;
    let metric = metric.trim().to_string();
    let (min, max) = if let Some(rest) = range.strip_prefix(">=") {
        (parse_f64(rest.trim())?, f64::INFINITY)
    } else if let Some(rest) = range.strip_prefix(">") {
        (parse_f64(rest.trim())?, f64::INFINITY)
    } else if let Some(rest) = range.strip_prefix("<=") {
        (f64::NEG_INFINITY, parse_f64(rest.trim())?)
    } else if let Some(rest) = range.strip_prefix("<") {
        (f64::NEG_INFINITY, parse_f64(rest.trim())?)
    } else if let Some((lo, hi)) = range.split_once("..") {
        (parse_f64(lo.trim())?, parse_f64(hi.trim())?)
    } else {
        let v = parse_f64(range.trim())?;
        (v, v)
    };
    Ok(TypedPredicate::QuantityRange { metric, min, max })
}

fn parse_heading(body: &str) -> Result<TypedPredicate, DslError> {
    // Form: in_heading:Architecture/Auth
    let path: Vec<String> = body
        .split('/')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if path.is_empty() {
        return Err(DslError("in_heading: requires a path".into()));
    }
    Ok(TypedPredicate::InHeadingPath { path })
}

fn parse_supersedes(body: &str) -> Result<TypedPredicate, DslError> {
    // Form: supersedes:c/claim-id-abc  (the leading c/ is a hint and is stripped)
    let id = body.strip_prefix("c/").unwrap_or(body).to_string();
    if id.is_empty() {
        return Err(DslError("supersedes: requires a claim id".into()));
    }
    Ok(TypedPredicate::SupersedesClaim { claim_id: id })
}

fn parse_references(body: &str) -> Result<TypedPredicate, DslError> {
    // Form: references:s/source-id-123  (leading s/ stripped)
    let id = body.strip_prefix("s/").unwrap_or(body).to_string();
    if id.is_empty() {
        return Err(DslError("references: requires a source id".into()));
    }
    Ok(TypedPredicate::ReferencedBy { source_id: id })
}

fn parse_trust_level(body: &str) -> Result<TrustLevel, DslError> {
    match body.to_ascii_lowercase().as_str() {
        "verified" => Ok(TrustLevel::Verified),
        "trusted" => Ok(TrustLevel::Trusted),
        "unknown" => Ok(TrustLevel::Unknown),
        "untrusted" => Ok(TrustLevel::Untrusted),
        "quarantined" => Ok(TrustLevel::Quarantined),
        other => Err(DslError(format!("unknown trust level: {other}"))),
    }
}

fn parse_f64(s: &str) -> Result<f64, DslError> {
    s.parse::<f64>()
        .map_err(|e| DslError(format!("number: {e} (input: {s})")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsl_parses_entity_type_and_name_compound() {
        let preds = parse("entity:Service:AuthService").expect("parse");
        assert_eq!(preds.len(), 1);
        assert!(matches!(
            &preds[0],
            TypedPredicate::EntityName { value } if value == "AuthService"
        ));
    }

    #[test]
    fn dsl_parses_quantity_range_with_gt_only() {
        let preds = parse("quantity:rps[>10000]").expect("parse");
        if let TypedPredicate::QuantityRange { metric, min, max } = &preds[0] {
            assert_eq!(metric, "rps");
            assert_eq!(*min, 10000.0);
            assert!(max.is_infinite() && max.is_sign_positive());
        } else {
            panic!("wrong variant: {:?}", preds[0]);
        }
    }

    #[test]
    fn dsl_parses_quantity_range_with_inclusive_bounds() {
        let preds = parse("quantity:rps[5000..10000]").expect("parse");
        if let TypedPredicate::QuantityRange { min, max, .. } = &preds[0] {
            assert_eq!(*min, 5000.0);
            assert_eq!(*max, 10000.0);
        } else {
            panic!();
        }
    }

    #[test]
    fn dsl_rejects_unknown_predicate_kind() {
        let err = parse("foobar:value").expect_err("should fail");
        assert!(err.0.contains("unknown predicate kind"));
    }

    #[test]
    fn dsl_and_combines_three_predicates() {
        let preds =
            parse("entity:Service AND doctag:@deprecated AND authored_by:alice").expect("parse");
        assert_eq!(preds.len(), 3);
        assert!(matches!(preds[0], TypedPredicate::EntityType { .. }));
        assert!(matches!(preds[1], TypedPredicate::HasDocTag { .. }));
        assert!(matches!(preds[2], TypedPredicate::AuthoredBy { .. }));
    }

    #[test]
    fn dsl_handles_lowercase_and_separator() {
        let preds = parse("entity:Service and authored_by:alice").expect("parse");
        assert_eq!(preds.len(), 2);
    }

    #[test]
    fn dsl_parses_markers_list() {
        let preds = parse("markers:TODO,FIXME,HACK").expect("parse");
        if let TypedPredicate::HasMarker { kinds } = &preds[0] {
            assert_eq!(kinds, &vec!["TODO".to_string(), "FIXME".into(), "HACK".into()]);
        } else {
            panic!();
        }
    }

    #[test]
    fn dsl_parses_source_trust_at_least() {
        let preds = parse("source.trust>=Verified").expect("parse");
        if let TypedPredicate::SourceTrustAtLeast { value } = &preds[0] {
            assert_eq!(*value, TrustLevel::Verified);
        } else {
            panic!();
        }
    }

    #[test]
    fn dsl_parses_in_call_graph_with_depth() {
        let preds = parse("in_call_graph_of:login@3").expect("parse");
        if let TypedPredicate::InCallGraphOf { entity_name, depth } = &preds[0] {
            assert_eq!(entity_name, "login");
            assert_eq!(*depth, 3);
        } else {
            panic!();
        }
    }

    #[test]
    fn dsl_parses_authored_after_bare_date() {
        let preds = parse("authored_after:2026-01-01").expect("parse");
        assert!(matches!(preds[0], TypedPredicate::AuthoredAfter { .. }));
    }

    #[test]
    fn dsl_empty_input_returns_empty() {
        assert_eq!(parse("").unwrap().len(), 0);
        assert_eq!(parse("   ").unwrap().len(), 0);
    }

    #[test]
    fn dsl_rejects_term_without_colon() {
        let err = parse("nokey").expect_err("should fail");
        assert!(err.0.contains("no `kind:` prefix"));
    }

    #[test]
    fn dsl_parses_in_heading_path() {
        let preds = parse("in_heading:Architecture/Auth/Tokens").expect("parse");
        if let TypedPredicate::InHeadingPath { path } = &preds[0] {
            assert_eq!(path, &vec!["Architecture", "Auth", "Tokens"]);
        } else {
            panic!();
        }
    }
}
