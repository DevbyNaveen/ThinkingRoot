// crates/thinkingroot-serve/src/intelligence/respond.rs
//
// Output schema for the `respond()` tool — the v1.1 terminal tool the
// agent will eventually use to emit a final answer with citations as
// structured metadata instead of inline `[claim:abc]` markers.
//
// In v1.0 this module is **passive**: the strongly-typed shapes are
// here, the JSON schema is here, and the verifier (intelligence/
// verifier.rs) consumes them when post-processing agent output. The
// tool is NOT yet registered in builtin_tools.rs because text
// streaming is still the production output path — wiring it as a
// callable-but-non-terminal tool would create an unreachable code
// branch which violates our no-placeholder discipline.
//
// v1.1 ship: register the tool in `builtin_tools.rs`, mark it
// `is_terminal_tool` in the agent loop, and migrate the SSE stream
// from text deltas to `respond()` tool-input deltas (Anthropic's
// `input_json_delta` event type).
//
// (Task 11, plan 2026-05-09.)

use serde::{Deserialize, Serialize};

/// One citation attached to a span of generated text. The verifier
/// (Task 12) checks every `claim_id` against the substrate; uncited
/// assertions get auto-cited from the retrieval top-K (the
/// "auto-cite with confidence floor" policy locked at plan time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// Substring of `RespondPayload.text` this citation supports.
    /// Renderers highlight this span and attach the chip on hover.
    pub span: String,

    /// Claim ID from the substrate. MUST resolve to an existing row
    /// — the verifier rewrites the response when it doesn't.
    pub claim_id: String,

    /// Optional certificate hash from `tr-sigstore`. Present when
    /// the underlying claim was signed (DSSE / Sigstore Bundle v0.3).
    /// The trust-receipt UI (Week 2) renders a 🔒 only when this
    /// field is `Some` AND the chain verifies offline via cosign.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_hash: Option<String>,

    /// Whether the citation is direct evidence or related context.
    /// "evidence" rows back the assertion outright; "related" rows
    /// are the auto-cite negative-cite category — surfaced for the
    /// reader without claiming the assertion is entailed by them.
    /// Default `Evidence` so producers don't have to think about it
    /// when the citation is a real direct match.
    #[serde(default = "default_relevance")]
    pub relevance: Relevance,
}

fn default_relevance() -> Relevance {
    Relevance::Evidence
}

/// Citation strength. The UI renders `Evidence` as a solid blue
/// chip and `Related` as a muted gray "related context" pill so the
/// reader can tell direct support from adjacent context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Relevance {
    /// The cited claim directly entails or supports the span.
    Evidence,
    /// The cited claim is the closest related substrate row but
    /// does not strictly entail the assertion. Set by the verifier's
    /// auto-cite negative-cite category when retrieval cosine-matches
    /// surface vocabulary without semantic match.
    Related,
}

/// One-click follow-up action surfaced beneath the answer. The chat
/// UI renders these as pills (Week 2). The agent loop reads `intent`
/// to know what slash-command-equivalent to invoke when the user
/// clicks. Free-form `payload` lets each intent type carry its own
/// structured args without growing this enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedAction {
    /// Short button label (e.g. "Investigate idempotency").
    pub label: String,

    /// Which agent intent to invoke when the user clicks.
    pub intent: ActionIntent,

    /// Action-specific payload. Schema varies by intent — `investigate`
    /// expects `{question: string}`, `sandbox` expects
    /// `{template: string}`, etc. Free-form so we don't have to grow
    /// the enum every time a new intent is added.
    #[serde(default = "empty_payload", skip_serializing_if = "is_empty_object")]
    pub payload: serde_json::Value,
}

fn empty_payload() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn is_empty_object(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Object(m) if m.is_empty())
}

/// Action types the chat surface knows how to dispatch. Stable
/// across v1.0 and v1.1 — the bus emits these as suggestions, the
/// chat UI renders them, and the agent loop interprets clicks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionIntent {
    /// "Investigate this further" — re-runs hybrid retrieval with
    /// expanded scope and surfaces the result. Payload typically
    /// `{question: string, focus_entity: string}`.
    Investigate,
    /// "Try this in a sandbox" — forks an Ephemeral branch and
    /// re-asks the same question against the sandbox. Payload
    /// typically `{template: string}` (e.g. `agent-sandbox`).
    Sandbox,
    /// "Delegate to an external agent" — spawns Claude Code or
    /// Cursor in a worktree (Week 3). Payload typically
    /// `{agent: 'claude-code' | 'cursor', prompt: string}`.
    Delegate,
    /// "Run reflect on the focus entity" — surfaces gaps the
    /// substrate has inferred for the entity in question.
    Reflect,
    /// "Contribute a claim" — opens a contribute form pre-filled
    /// with whatever the agent inferred but couldn't ground.
    Contribute,
}

/// Strongly-typed payload of one `respond()` tool invocation. The
/// LLM produces this; the verifier validates citations; the chat UI
/// renders text + chips + pills. Designed to round-trip through
/// `serde_json` so the wire format matches the JSON schema below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RespondPayload {
    /// The natural-language reply, citations elided. Renderers show
    /// this as prose; chips are added by matching `Citation.span`
    /// against substrings of `text`.
    pub text: String,

    /// Citation array. Empty list is valid (e.g. for chitchat) and
    /// triggers the verifier's chitchat pass-through. Otherwise
    /// every entry's `claim_id` is checked against the substrate.
    #[serde(default)]
    pub citations: Vec<Citation>,

    /// Optional one-click follow-up suggestions. Empty / absent
    /// means "no proactive offer" — the model decided no useful
    /// next-step exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_actions: Vec<SuggestedAction>,
}

impl RespondPayload {
    /// Parse a JSON value claimed to match this schema. Used by the
    /// verifier when reading the agent's tool-call argument. Returns
    /// the strongly-typed payload or a `serde_json` error with
    /// path info — the caller logs and falls back to text-only.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(v.clone())
    }

    /// Round-trip helper: serialise to JSON for the SSE wire format.
    pub fn to_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// Validate that every `Citation.span` actually appears in
    /// `text`. The LLM occasionally hallucinates a span the chat UI
    /// then can't highlight; the verifier strips those before
    /// emitting a `trust_receipt`. Returns the list of `claim_id`s
    /// whose spans don't appear — empty when valid.
    pub fn unmatched_spans(&self) -> Vec<&str> {
        self.citations
            .iter()
            .filter(|c| !self.text.contains(&c.span))
            .map(|c| c.claim_id.as_str())
            .collect()
    }
}

/// JSON schema for the `respond()` tool — kept as a `&'static str`
/// so callers can embed it in MCP `tools/list` responses without
/// re-serialising on every call. Mirrors the field shape on
/// `RespondPayload`; the round-trip test below pins them together.
pub const RESPOND_TOOL_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "text": {
      "type": "string",
      "description": "The natural-language reply. Citations are metadata, not inline IDs — write in prose."
    },
    "citations": {
      "type": "array",
      "default": [],
      "items": {
        "type": "object",
        "properties": {
          "span": {
            "type": "string",
            "description": "Substring of `text` this citation supports."
          },
          "claim_id": {
            "type": "string",
            "description": "Substrate claim ID. Must exist; verifier rewrites uncited assertions."
          },
          "certificate_hash": {
            "type": "string",
            "description": "Optional DSSE/Sigstore certificate. Present when the claim is signed."
          },
          "relevance": {
            "type": "string",
            "enum": ["evidence", "related"],
            "default": "evidence",
            "description": "'evidence' = direct support; 'related' = closest substrate context (auto-cite fallback)."
          }
        },
        "required": ["span", "claim_id"]
      }
    },
    "suggested_actions": {
      "type": "array",
      "default": [],
      "items": {
        "type": "object",
        "properties": {
          "label": { "type": "string" },
          "intent": {
            "type": "string",
            "enum": ["investigate", "sandbox", "delegate", "reflect", "contribute"]
          },
          "payload": { "type": "object" }
        },
        "required": ["label", "intent"]
      }
    }
  },
  "required": ["text"]
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_constant_is_valid_json() {
        let v: serde_json::Value =
            serde_json::from_str(RESPOND_TOOL_SCHEMA).expect("schema must be valid JSON");
        assert_eq!(v["type"], "object");
        let required = v["required"].as_array().expect("required is array");
        assert!(required.iter().any(|x| x == "text"));
    }

    #[test]
    fn round_trip_minimal_payload() {
        let p = RespondPayload {
            text: "yeah, the webhook is broken".to_string(),
            citations: vec![],
            suggested_actions: vec![],
        };
        let v = p.to_json().unwrap();
        let p2 = RespondPayload::from_json(&v).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn round_trip_full_payload() {
        let p = RespondPayload {
            text: "the WebhookHandler validates event ids".to_string(),
            citations: vec![
                Citation {
                    span: "WebhookHandler".to_string(),
                    claim_id: "a7c2".to_string(),
                    certificate_hash: Some("0xa3f7…".to_string()),
                    relevance: Relevance::Evidence,
                },
                Citation {
                    span: "validates event ids".to_string(),
                    claim_id: "4d8e".to_string(),
                    certificate_hash: None,
                    relevance: Relevance::Related,
                },
            ],
            suggested_actions: vec![SuggestedAction {
                label: "Investigate idempotency".to_string(),
                intent: ActionIntent::Investigate,
                payload: json!({"question": "is the retry path idempotent?"}),
            }],
        };
        let v = p.to_json().unwrap();
        let p2 = RespondPayload::from_json(&v).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn relevance_defaults_to_evidence() {
        // Citation with no `relevance` field → deserialises to Evidence.
        let v = json!({
            "span": "x",
            "claim_id": "c1",
        });
        let c: Citation = serde_json::from_value(v).unwrap();
        assert_eq!(c.relevance, Relevance::Evidence);
    }

    #[test]
    fn unmatched_spans_finds_text_misalignment() {
        let p = RespondPayload {
            text: "the webhook is broken".to_string(),
            citations: vec![
                Citation {
                    span: "webhook".to_string(),
                    claim_id: "good".to_string(),
                    certificate_hash: None,
                    relevance: Relevance::Evidence,
                },
                Citation {
                    span: "non-existent fragment".to_string(),
                    claim_id: "bad".to_string(),
                    certificate_hash: None,
                    relevance: Relevance::Evidence,
                },
            ],
            suggested_actions: vec![],
        };
        let unmatched = p.unmatched_spans();
        assert_eq!(unmatched, vec!["bad"]);
    }

    #[test]
    fn unmatched_spans_empty_when_all_match() {
        let p = RespondPayload {
            text: "alpha bravo charlie".to_string(),
            citations: vec![Citation {
                span: "alpha".to_string(),
                claim_id: "a".to_string(),
                certificate_hash: None,
                relevance: Relevance::Evidence,
            }],
            suggested_actions: vec![],
        };
        assert!(p.unmatched_spans().is_empty());
    }

    #[test]
    fn rejects_payload_missing_required_text_field() {
        let v = json!({"citations": []});
        let r: Result<RespondPayload, _> = serde_json::from_value(v);
        assert!(r.is_err(), "missing `text` must be a parse error");
    }

    #[test]
    fn empty_citations_array_is_valid() {
        let v = json!({"text": "hi"});
        let p: RespondPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.text, "hi");
        assert!(p.citations.is_empty());
        assert!(p.suggested_actions.is_empty());
    }

    #[test]
    fn suggested_action_intents_round_trip() {
        // Pin every intent variant to its JSON name.
        for (intent, expected) in [
            (ActionIntent::Investigate, "investigate"),
            (ActionIntent::Sandbox, "sandbox"),
            (ActionIntent::Delegate, "delegate"),
            (ActionIntent::Reflect, "reflect"),
            (ActionIntent::Contribute, "contribute"),
        ] {
            let v = serde_json::to_value(intent).unwrap();
            assert_eq!(v.as_str().unwrap(), expected);
            let back: ActionIntent = serde_json::from_value(v).unwrap();
            assert_eq!(back, intent);
        }
    }

    #[test]
    fn relevance_round_trips_lowercase() {
        for (r, expected) in [
            (Relevance::Evidence, "evidence"),
            (Relevance::Related, "related"),
        ] {
            let v = serde_json::to_value(r).unwrap();
            assert_eq!(v.as_str().unwrap(), expected);
            let back: Relevance = serde_json::from_value(v).unwrap();
            assert_eq!(back, r);
        }
    }
}
