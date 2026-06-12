//! §1 — the `predict` verb ("what happens next"), grounded + falsifier-gated.
//!
//! NOT the excluded generative-adapter "Mind" (§11 #30). This is the honest,
//! shippable predictive-memory verb: the workspace's OWN LLM infers the most
//! likely next outcome **only from patterns the recalled claims support**,
//! citing them inline, with a confidence — verified-or-silent. If the memory
//! gives no basis, it refuses (`INSUFFICIENT_EVIDENCE`) rather than prophesy.
//! Falsifier-gated: a prediction with no grounded citation is dropped.
//!
//! Pure, testable text core (prompt + confidence parse); the loop
//! (recall → predict → citation-gate) lives on `QueryEngine::predict`.

/// System prompt: predict ONLY from the claims, cite them, state a confidence,
/// or refuse. The literal `INSUFFICIENT_EVIDENCE` sentinel is the refusal path.
pub const PREDICT_SYSTEM: &str = "You are a predictive-memory engine. You are given memory claims \
(each with an id) and a question about what happens NEXT. Predict the single most likely next \
outcome — but ONLY from patterns the claims support; never invent facts beyond them. Cite each \
supporting claim inline as [claim:<id>]. End with a line `confidence: X` where X is 0.0-1.0. If the \
claims give no basis to predict, reply with exactly INSUFFICIENT_EVIDENCE and nothing else.";

/// Build the user prompt: the recalled grounding claims + the question.
pub fn build_predict_prompt(question: &str, claims: &[(String, String)]) -> String {
    let mut p = String::from("Memory claims:\n");
    for (id, statement) in claims {
        let s = statement.trim();
        if !s.is_empty() {
            p.push_str(&format!("[claim:{id}] {s}\n"));
        }
    }
    p.push_str("\nQuestion: ");
    p.push_str(question.trim());
    p.push_str("\nWhat happens next?");
    p
}

/// Extract the `confidence: X` value (0..=1) from the model output; `None` if
/// absent or unparseable. Case-insensitive; tolerant of trailing text.
pub fn parse_confidence(text: &str) -> Option<f64> {
    for line in text.lines() {
        let l = line.trim().to_lowercase();
        if let Some(rest) = l.strip_prefix("confidence:") {
            let num: String = rest.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            if let Ok(v) = num.parse::<f64>() {
                return Some(v.clamp(0.0, 1.0));
            }
        }
    }
    None
}

/// Did the model take the explicit refusal path?
pub fn is_refusal(text: &str) -> bool {
    text.trim().to_uppercase().contains("INSUFFICIENT_EVIDENCE")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_grounds_in_cited_claims() {
        let p = build_predict_prompt(
            "Will Zeta churn?",
            &[("c1".into(), "Zeta stopped using exports".into()), ("c2".into(), "  ".into())],
        );
        assert!(p.contains("[claim:c1] Zeta stopped using exports"));
        assert!(!p.contains("c2"), "blank claim skipped");
        assert!(p.contains("Question: Will Zeta churn?"));
    }

    #[test]
    fn parses_confidence_and_refusal() {
        assert_eq!(parse_confidence("Zeta will churn [claim:c1]\nconfidence: 0.82"), Some(0.82));
        assert_eq!(parse_confidence("Confidence: 1.5 (high)"), Some(1.0)); // clamped
        assert_eq!(parse_confidence("no confidence here"), None);
        assert!(is_refusal("INSUFFICIENT_EVIDENCE"));
        assert!(is_refusal("  insufficient_evidence  "));
        assert!(!is_refusal("Zeta will churn, confidence: 0.7"));
    }
}
