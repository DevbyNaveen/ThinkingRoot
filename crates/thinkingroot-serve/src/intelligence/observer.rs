//! SOTA Lever 3 — Observer + Reflector substrate.
//!
//! Mirrors Mastra's Observational Memory pattern that earned 94.87% on
//! LongMemEval. Two roles:
//!
//! - **Observer** — runs in the chat lifecycle, condensing recent turn
//!   windows into structured observation text. Each observation
//!   becomes a `conversation::observation@v1` Witness in the workspace
//!   substrate, so retrieval (hybrid + AEP) picks it up alongside
//!   file-derived witnesses.
//!
//! - **Reflector** — runs when the per-session observation count
//!   crosses [`Reflector::reflect_after`]. Combines related items,
//!   drops irrelevant context, emits a `conversation::reflection@v1`
//!   Witness. The reflection's `inputs` field references the source
//!   observations via `WitnessInput::WitnessRef` so the substrate
//!   carries provenance from raw turn → observation → reflection.
//!
//! Why this lives next to the existing engram cache rather than
//! replacing it: engrams are per-session materialised probe results
//! (in-memory, ephemeral). Observations are durable substrate rows
//! that survive process restart, contribute to hybrid retrieval, and
//! travel inside `.tr` packs. The two complement each other — engrams
//! cache the current-session view; observations build long-term
//! cross-session memory.
//!
//! v1 ship contract:
//! - Mechanical condensation only (no LLM). Concatenates user prompts
//!   from the turn window into a structured observation block. A
//!   future v2 can optionally upgrade to LLM-based summarisation
//!   under `ScoringProfile.use_cross_encoder` style opt-in.
//! - Observations carry `Sensitivity::Internal` per the catalog
//!   default — conversation text often contains personal context.
//! - `content_blake3` is computed over the observation text so I-4
//!   tamper evidence carries forward to the Witness substrate.
//!
//! Wire-in points (NOT YET wired — that's a follow-up):
//! - `intelligence::respond::respond_with_brief` — after the
//!   assistant reply lands, call `Observer::record_turn`.
//! - `intelligence::react::run_react_loop` — same hook at end of
//!   each (thought, tool, observation) cycle.
//! - Background sweep: every N minutes, call
//!   `Observer::flush_to_witnesses` per active session so unflushed
//!   observations don't get lost on a process restart.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// Rule names — must match catalog 1.5.0 entries in
/// `thinkingroot-extract::rule_catalog`.
pub const RULE_OBSERVATION: &str = "conversation::observation@v1";
pub const RULE_REFLECTION: &str = "conversation::reflection@v1";

/// Witness types corresponding to the rule outputs.
const TYPE_OBSERVATION: &str = "conversation::observation";
const TYPE_REFLECTION: &str = "conversation::reflection";

/// Confidence inherited from catalog defaults.
const CONF_OBSERVATION: f32 = 0.97;
const CONF_REFLECTION: f32 = 0.95;

/// Turn window for Observer condensation. After every
/// `condense_every` turns get appended, the Observer materialises an
/// observation Witness covering that window. Default `10` matches the
/// upper-bound Mastra reports — smaller windows make the substrate
/// noisier; larger windows lose recency signal.
const DEFAULT_CONDENSE_EVERY: usize = 10;

/// Reflector trigger threshold. When a session's observation count
/// reaches this, the Reflector materialises a reflection Witness over
/// the most recent `Self::reflect_after` observations. Default `8`
/// matches Mastra's "Reflector restructures when observations
/// accumulate past a token threshold" footprint.
const DEFAULT_REFLECT_AFTER: usize = 8;

/// One recorded chat turn awaiting condensation.
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub session_id: String,
    pub turn_number: u64,
    pub user_prompt: String,
    pub assistant_reply: String,
    pub at: DateTime<Utc>,
}

/// One observation that the Observer has emitted but not yet written
/// to the witness substrate.
#[derive(Debug, Clone)]
pub struct StagedObservation {
    pub session_id: String,
    pub turn_range: (u64, u64), // inclusive
    pub at: DateTime<Utc>,
    pub text: String,
}

/// Per-session in-memory buffer the Observer uses before flushing.
#[derive(Debug, Default)]
struct SessionBuffer {
    pending_turns: Vec<ChatTurn>,
    staged_observations: Vec<StagedObservation>,
}

/// Process-wide Observer. Thread-safe (one Mutex over the per-session
/// map). Designed to be held in `QueryEngine` or as a `tower::Layer`
/// on the chat router — both wire-in patterns work because the
/// `record_turn` / `flush_to_witnesses` API is `&self`.
pub struct Observer {
    buffers: Mutex<HashMap<String, SessionBuffer>>,
    condense_every: usize,
    reflect_after: usize,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            buffers: Mutex::new(HashMap::new()),
            condense_every: DEFAULT_CONDENSE_EVERY,
            reflect_after: DEFAULT_REFLECT_AFTER,
        }
    }
}

impl Observer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the condense interval. Smaller → noisier substrate but
    /// finer-grained retrieval; larger → cleaner substrate but coarser
    /// recall. The Mastra reference range is `[5, 20]`.
    pub fn with_condense_every(mut self, n: usize) -> Self {
        self.condense_every = n.max(1);
        self
    }

    /// Override the reflect trigger. Defaults to `8` observations.
    pub fn with_reflect_after(mut self, n: usize) -> Self {
        self.reflect_after = n.max(2);
        self
    }

    /// Record a completed chat turn. When the pending-turn count
    /// reaches `condense_every`, the Observer mechanically condenses
    /// the window into a staged observation. The caller can drain
    /// staged observations via `take_staged` at session end OR every
    /// few turns.
    ///
    /// `&self` — safe for concurrent calls from multiple chat
    /// connections; the per-session buffer is serialised inside the
    /// outer `Mutex`. We DO NOT hold the lock across the
    /// `condense_window` call because that allocates a `String` and
    /// would needlessly serialise different sessions.
    pub fn record_turn(&self, turn: ChatTurn) {
        let session_id = turn.session_id.clone();
        let (should_condense, window) = {
            let mut guard = match self.buffers.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let buf = guard.entry(session_id.clone()).or_default();
            buf.pending_turns.push(turn);
            if buf.pending_turns.len() >= self.condense_every {
                let drained: Vec<ChatTurn> = buf.pending_turns.drain(..).collect();
                (true, drained)
            } else {
                (false, Vec::new())
            }
        };
        if should_condense && !window.is_empty() {
            let obs = condense_window(&session_id, &window);
            let mut guard = match self.buffers.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(buf) = guard.get_mut(&session_id) {
                buf.staged_observations.push(obs);
            }
        }
    }

    /// Drain staged observations for a session. The caller is
    /// responsible for writing them to the witness substrate via
    /// `insert_witnesses_batch` (after wrapping with the
    /// `materialise_observation_witness` helper).
    pub fn take_staged(&self, session_id: &str) -> Vec<StagedObservation> {
        let mut guard = match self.buffers.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let Some(buf) = guard.get_mut(session_id) else {
            return Vec::new();
        };
        std::mem::take(&mut buf.staged_observations)
    }

    /// How many pending (un-condensed) turns sit in a session's
    /// buffer. Useful for telemetry and for the background sweep
    /// that decides whether to force a condensation before flush.
    pub fn pending_count(&self, session_id: &str) -> usize {
        let guard = match self.buffers.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard
            .get(session_id)
            .map(|b| b.pending_turns.len())
            .unwrap_or(0)
    }

    /// Whether the Reflector should fire for `session_id`. Wire this
    /// into the chat-path flush pass — when true, call
    /// `materialise_reflection_witness` over the staged observations
    /// and persist alongside.
    pub fn should_reflect(&self, session_id: &str) -> bool {
        let guard = match self.buffers.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard
            .get(session_id)
            .map(|b| b.staged_observations.len() >= self.reflect_after)
            .unwrap_or(false)
    }

    /// Force-condense any pending turns for a session (e.g. at
    /// session-end). Even partial windows produce an observation if
    /// they have ≥ 1 turn.
    pub fn force_condense(&self, session_id: &str) {
        let window: Vec<ChatTurn> = {
            let mut guard = match self.buffers.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let Some(buf) = guard.get_mut(session_id) else {
                return;
            };
            buf.pending_turns.drain(..).collect()
        };
        if window.is_empty() {
            return;
        }
        let obs = condense_window(session_id, &window);
        let mut guard = match self.buffers.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(buf) = guard.get_mut(session_id) {
            buf.staged_observations.push(obs);
        }
    }
}

/// Mechanical condensation — concatenates structured observation
/// text from the turn window. No LLM. The format:
///
/// ```text
/// Observation for session <id>, turns N..=M (recorded <iso8601>):
///
/// - Turn N — user: <prompt[:200]> | assistant: <reply[:200]>
/// - Turn N+1 — …
/// ```
///
/// Per-line clamp to 200 chars per side keeps the observation
/// payload bounded; the substrate doesn't store transcripts verbatim,
/// only condensed pointers + key signals.
fn condense_window(session_id: &str, window: &[ChatTurn]) -> StagedObservation {
    let first = window.first().expect("non-empty window");
    let last = window.last().expect("non-empty window");
    let mut text = String::with_capacity(window.len() * 256);
    text.push_str("Observation for session ");
    text.push_str(session_id);
    text.push_str(", turns ");
    text.push_str(&first.turn_number.to_string());
    text.push_str("..=");
    text.push_str(&last.turn_number.to_string());
    text.push_str(" (recorded ");
    text.push_str(&last.at.to_rfc3339());
    text.push_str("):\n\n");
    for turn in window {
        text.push_str("- Turn ");
        text.push_str(&turn.turn_number.to_string());
        text.push_str(" — user: ");
        push_truncated(&mut text, &turn.user_prompt, 200);
        text.push_str(" | assistant: ");
        push_truncated(&mut text, &turn.assistant_reply, 200);
        text.push('\n');
    }
    StagedObservation {
        session_id: session_id.to_string(),
        turn_range: (first.turn_number, last.turn_number),
        at: last.at,
        text,
    }
}

fn push_truncated(out: &mut String, src: &str, max_chars: usize) {
    if src.chars().count() <= max_chars {
        out.push_str(src);
        return;
    }
    for (i, c) in src.chars().enumerate() {
        if i >= max_chars {
            break;
        }
        out.push(c);
    }
    out.push('…');
}

/// Construct a `conversation::observation@v1` Witness from a staged
/// observation. The Witness anchors to a synthetic span keyed by the
/// session id + turn range, with `content_blake3` over the
/// observation text. This is the storage-layer adapter — pipeline
/// callers wrap staged observations with this helper before passing
/// to `insert_witnesses_batch`.
pub fn materialise_observation_witness(
    staged: &StagedObservation,
    workspace: WorkspaceId,
    source: SourceId,
    now: DateTime<Utc>,
) -> Witness {
    let file_blake3 = synthetic_session_anchor(&staged.session_id);
    let span_start = staged.turn_range.0;
    let span_end = staged.turn_range.1 + 1; // inclusive → exclusive
    let span = WitnessSpan {
        file_blake3: file_blake3.clone(),
        start: span_start,
        end: span_end,
    };
    let mut w = Witness::new(
        RULE_OBSERVATION.to_string(),
        TYPE_OBSERVATION.to_string(),
        vec![WitnessInput::ByteRef {
            file_blake3,
            start: span_start,
            end: span_end,
        }],
        vec![span],
        source,
        workspace,
        Sensitivity::Internal,
        Confidence::new(CONF_OBSERVATION as f64),
        blake3::hash(staged.text.as_bytes()).to_hex().to_string(),
        now,
    );
    w.symbol = Some(format!(
        "{}:{}..{}",
        staged.session_id, span_start, span_end
    ));
    w
}

/// Construct a `conversation::reflection@v1` Witness over a set of
/// observation Witnesses. `inputs` references each observation by id
/// so substrate provenance carries through. Reflection text is
/// produced by `reflect_text` — a deterministic, no-LLM
/// re-structuring that drops the per-turn detail and keeps the
/// session-level signal.
pub fn materialise_reflection_witness(
    observations: &[Witness],
    workspace: WorkspaceId,
    source: SourceId,
    now: DateTime<Utc>,
) -> Option<Witness> {
    if observations.len() < 2 {
        // A reflection requires at least 2 inputs to combine; a
        // single-observation reflection wouldn't carry novel signal
        // and would inflate the substrate. Honest "not enough to
        // reflect" → None.
        return None;
    }
    let reflection_text = reflect_text(observations);
    // Use the first observation's anchor as the reflection's anchor —
    // reflection is a derived Witness so its span points at the
    // earliest observed turn range. WitnessId is derived from
    // `(rule, spans)` so this gives every reflection a content-derived
    // id keyed on the spanning window.
    let anchor = observations[0].anchor_span().clone();
    let inputs: Vec<WitnessInput> = observations
        .iter()
        .map(|o| WitnessInput::WitnessRef { id: o.id.clone() })
        .collect();
    let mut w = Witness::new(
        RULE_REFLECTION.to_string(),
        TYPE_REFLECTION.to_string(),
        inputs,
        vec![anchor],
        source,
        workspace,
        Sensitivity::Internal,
        Confidence::new(CONF_REFLECTION as f64),
        blake3::hash(reflection_text.as_bytes()).to_hex().to_string(),
        now,
    );
    // Symbol: session id + observation count, so retrieval can filter
    // reflections by session.
    if let Some(sym) = observations[0].symbol.as_ref() {
        let session_prefix = sym.split(':').next().unwrap_or("session");
        w.symbol = Some(format!("{}:reflection({})", session_prefix, observations.len()));
    }
    Some(w)
}

/// Deterministic, no-LLM reflection over a set of observations. The
/// v1 strategy: concatenate observation texts in chronological order,
/// prefix with a session-level summary header. A v2 can swap to LLM
/// summarisation while keeping this function as the offline / no-LLM
/// fallback. Determinism keeps the Witness id stable across re-runs
/// of the same input set.
fn reflect_text(observations: &[Witness]) -> String {
    let mut text = String::with_capacity(observations.len() * 512);
    text.push_str("Reflection over ");
    text.push_str(&observations.len().to_string());
    text.push_str(" observation(s):\n\n");
    for (i, o) in observations.iter().enumerate() {
        text.push_str(&format!("[{}] {} (rule {}, blake3 {})\n",
            i + 1,
            o.symbol.clone().unwrap_or_else(|| "<no-symbol>".into()),
            o.rule,
            &o.content_blake3[..8.min(o.content_blake3.len())],
        ));
    }
    text
}

/// Synthetic file_blake3 anchor for a session. We don't yet write
/// session transcripts into `source.tar.zst`, so the anchor is a
/// stable hash of the session id. Substrate consumers that need to
/// re-fetch the transcript bytes get `None` from
/// `byte_store.get_range` — the materialised-statement path falls
/// back to the witness's own content_blake3 for display. A future
/// ship can add a `session-transcript://` byte-store URI scheme so
/// observations point at real bytes.
fn synthetic_session_anchor(session_id: &str) -> String {
    blake3::hash(format!("session-anchor::{session_id}").as_bytes())
        .to_hex()
        .to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn turn(session: &str, n: u64, prompt: &str, reply: &str) -> ChatTurn {
        ChatTurn {
            session_id: session.into(),
            turn_number: n,
            user_prompt: prompt.into(),
            assistant_reply: reply.into(),
            at: Utc.timestamp_opt(1_700_000_000 + (n as i64) * 60, 0).unwrap(),
        }
    }

    #[test]
    fn record_turn_appends_to_pending_below_threshold() {
        let obs = Observer::new().with_condense_every(5);
        obs.record_turn(turn("s1", 1, "hello", "hi"));
        obs.record_turn(turn("s1", 2, "how are you", "well"));
        assert_eq!(obs.pending_count("s1"), 2);
        assert!(obs.take_staged("s1").is_empty());
    }

    #[test]
    fn record_turn_condenses_at_threshold() {
        let obs = Observer::new().with_condense_every(3);
        for i in 1..=3 {
            obs.record_turn(turn("s1", i, &format!("q{i}"), &format!("a{i}")));
        }
        assert_eq!(obs.pending_count("s1"), 0, "buffer drained at threshold");
        let staged = obs.take_staged("s1");
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].turn_range, (1, 3));
        assert!(staged[0].text.contains("q1"));
        assert!(staged[0].text.contains("q3"));
    }

    #[test]
    fn condense_window_clamps_long_text() {
        let long_prompt = "x".repeat(500);
        let obs = Observer::new().with_condense_every(1);
        obs.record_turn(turn("s1", 1, &long_prompt, "ok"));
        let staged = obs.take_staged("s1");
        assert_eq!(staged.len(), 1);
        // Text should contain ellipsis-truncated payload, not the
        // full 500-char prompt verbatim.
        assert!(staged[0].text.contains('…'), "long prompts must be truncated");
        // Sanity: the observation header is still present.
        assert!(staged[0].text.starts_with("Observation for session s1"));
    }

    #[test]
    fn force_condense_handles_partial_window() {
        let obs = Observer::new().with_condense_every(10);
        for i in 1..=4 {
            obs.record_turn(turn("s1", i, "q", "a"));
        }
        assert_eq!(obs.pending_count("s1"), 4);
        obs.force_condense("s1");
        assert_eq!(obs.pending_count("s1"), 0);
        let staged = obs.take_staged("s1");
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].turn_range, (1, 4));
    }

    #[test]
    fn force_condense_no_op_on_empty_buffer() {
        let obs = Observer::new();
        obs.force_condense("nonexistent");
        assert!(obs.take_staged("nonexistent").is_empty());
    }

    #[test]
    fn sessions_are_isolated() {
        let obs = Observer::new().with_condense_every(2);
        obs.record_turn(turn("s1", 1, "from s1", "ok"));
        obs.record_turn(turn("s2", 1, "from s2", "ok"));
        assert_eq!(obs.pending_count("s1"), 1);
        assert_eq!(obs.pending_count("s2"), 1);
        assert!(obs.take_staged("s1").is_empty());
        assert!(obs.take_staged("s2").is_empty());
    }

    #[test]
    fn should_reflect_triggers_at_threshold() {
        let obs = Observer::new()
            .with_condense_every(2)
            .with_reflect_after(3);
        // 6 turns / 2 per window = 3 observations → should_reflect.
        for i in 1..=6 {
            obs.record_turn(turn("s1", i, "q", "a"));
        }
        assert!(obs.should_reflect("s1"));
    }

    #[test]
    fn should_reflect_false_below_threshold() {
        let obs = Observer::new()
            .with_condense_every(2)
            .with_reflect_after(5);
        for i in 1..=4 {
            obs.record_turn(turn("s1", i, "q", "a"));
        }
        // 2 observations < 5 threshold.
        assert!(!obs.should_reflect("s1"));
    }

    #[test]
    fn materialise_observation_witness_round_trips_text() {
        let staged = StagedObservation {
            session_id: "s1".into(),
            turn_range: (1, 5),
            at: Utc::now(),
            text: "test observation".into(),
        };
        let w = materialise_observation_witness(
            &staged,
            WorkspaceId::new(),
            SourceId::new(),
            Utc::now(),
        );
        assert_eq!(w.rule, RULE_OBSERVATION);
        assert_eq!(w.witness_type, TYPE_OBSERVATION);
        assert_eq!(w.sensitivity, Sensitivity::Internal);
        assert_eq!(
            w.content_blake3,
            blake3::hash("test observation".as_bytes())
                .to_hex()
                .to_string()
        );
        assert_eq!(w.symbol, Some("s1:1..6".to_string()));
    }

    #[test]
    fn materialise_reflection_witness_requires_two_observations() {
        let staged = StagedObservation {
            session_id: "s1".into(),
            turn_range: (1, 2),
            at: Utc::now(),
            text: "single".into(),
        };
        let obs = materialise_observation_witness(
            &staged,
            WorkspaceId::new(),
            SourceId::new(),
            Utc::now(),
        );
        let reflection = materialise_reflection_witness(
            &[obs],
            WorkspaceId::new(),
            SourceId::new(),
            Utc::now(),
        );
        assert!(reflection.is_none(), "single observation can't reflect");
    }

    #[test]
    fn materialise_reflection_links_inputs() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let now = Utc::now();
        let o1 = materialise_observation_witness(
            &StagedObservation {
                session_id: "s1".into(),
                turn_range: (1, 2),
                at: now,
                text: "first".into(),
            },
            ws,
            src,
            now,
        );
        let o2 = materialise_observation_witness(
            &StagedObservation {
                session_id: "s1".into(),
                turn_range: (3, 4),
                at: now,
                text: "second".into(),
            },
            ws,
            src,
            now,
        );
        let reflection =
            materialise_reflection_witness(&[o1.clone(), o2.clone()], ws, src, now)
                .expect("reflection produced");
        assert_eq!(reflection.rule, RULE_REFLECTION);
        assert!(reflection.is_derived(), "reflection must carry WitnessRef inputs");
        // Inputs should reference both observation ids.
        let refs: Vec<_> = reflection
            .inputs
            .iter()
            .filter_map(|i| match i {
                WitnessInput::WitnessRef { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&o1.id));
        assert!(refs.contains(&o2.id));
    }

    #[test]
    fn reflection_content_blake3_is_deterministic() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let now = Utc::now();
        let mk = || {
            let o1 = materialise_observation_witness(
                &StagedObservation {
                    session_id: "s1".into(),
                    turn_range: (1, 2),
                    at: now,
                    text: "first".into(),
                },
                ws,
                src,
                now,
            );
            let o2 = materialise_observation_witness(
                &StagedObservation {
                    session_id: "s1".into(),
                    turn_range: (3, 4),
                    at: now,
                    text: "second".into(),
                },
                ws,
                src,
                now,
            );
            materialise_reflection_witness(&[o1, o2], ws, src, now)
                .expect("reflection produced")
        };
        let a = mk();
        let b = mk();
        assert_eq!(a.content_blake3, b.content_blake3);
        assert_eq!(a.id, b.id, "content-addressed id stable across runs");
    }
}
