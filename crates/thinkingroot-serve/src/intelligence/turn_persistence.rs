//! Phase B.2 (2026-05-17) — auto-distill chat conversation into the
//! per-session stream branch's graph so future retrievals can pull
//! prior-turn content the same way they pull compiled source-code
//! claims.
//!
//! For each completed chat turn, this module writes three coordinated
//! rows onto the stream branch's [`GraphStore`]:
//!
//! 1. A synthetic [`Source`] with URI `mcp://agent/{session}/turn/{N}/transcript`
//!    (matching the `mcp://agent/` prefix that
//!    `maintenance::branch_has_agent_contributes` already recognises, so
//!    stream branches carrying only chat turns still route through the
//!    Phase-A AutoOnSessionEnd → topic merge flow).
//! 2. A [`Claim`] whose `statement` field carries a truncated `(user_question,
//!    assistant_text)` digest — this is what `hybrid_retrieve`, `search`,
//!    and `query_claims` will match on when subsequent turns ask
//!    "what did we discuss earlier?".
//! 3. A row in the `turns` table that binds `(session_id, turn_number)`
//!    to the claim id, so the AEP `turn_provenance` query (capped at 200
//!    turns per session) can walk the conversation history without
//!    re-parsing claim contents.
//!
//! In addition to the turn-anchor claim (2), this path now runs the SAME
//! mechanical witness-mesh extraction a batch compile uses (SRX-style
//! segmentation + clause splitting + fact-quality gate + temporal anchoring,
//! 2026-06-01) to distill the turn into atomic, speaker-attributed,
//! temporally-anchored **fact** claims. This is multi-granularity indexing:
//! the anchor serves turn-level recall ("what did we discuss earlier?") while
//! the distilled facts serve fact-level recall ("what is my dog's name?") —
//! and both are carried into `main` when the stream branch auto-merges. Zero
//! LLM, byte-deterministic, consistent with the structural-only compile
//! contract.

use std::path::Path;

use chrono::{DateTime, Utc};
use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Error, Result, Source, SourceType, TrustLevel, WorkspaceId,
    types::{ExtractionTier, SourceId},
};
use thinkingroot_graph::graph::GraphStore;

/// Soft cap on the claim's `statement` field. Long enough to carry the
/// retrieval-relevant gist of a typical chat turn (question + a few
/// sentences of answer); short enough that the graph stays compact
/// across thousand-turn sessions. Truncation is char-aware so we never
/// split a UTF-8 codepoint.
const MAX_STATEMENT_CHARS: usize = 1024;

/// Minimum length (chars) for an extracted atomic fact to be stored. The
/// mechanical fact-quality gate already rejects fragments; this is a final
/// floor against trivially short units.
const MIN_FACT_CHARS: usize = 10;

/// The result of a single successful turn persistence — exposed so
/// callers and tests can verify the rows landed without re-querying.
#[derive(Debug, Clone)]
pub struct PersistedTurn {
    pub source_uri: String,
    pub source_id: String,
    /// The turn-anchor claim (the `(question → answer)` digest) — kept for
    /// turn-level recall ("what did we discuss earlier?").
    pub claim_id: String,
    /// Atomic, speaker-attributed, temporally-anchored fact claims distilled
    /// from this turn via the mechanical witness-mesh extraction (the
    /// fact-level recall units). Empty when the turn carried no extractable
    /// fact (e.g. "ok, thanks").
    pub fact_claim_ids: Vec<String>,
    pub turn_number: u64,
}

/// Distill atomic fact claims from one speaker's text using the SAME mechanical
/// extraction as a batch compile: SRX-style sentence segmentation + ClausIE-lite
/// clause splitting + the fact-quality gate + temporal anchoring. Zero LLM.
///
/// Each fact is prefixed with the speaker (`"User: …"` / `"Assistant: …"`) for
/// attribution + self-containment (the cheap-coreference move LongMemEval
/// credits for preference recall), and its bitemporal `valid_from` / `event_date`
/// is anchored to an in-text absolute date when present, else the turn time.
fn extract_turn_facts(
    text: &str,
    speaker: &str,
    source_id: SourceId,
    workspace: WorkspaceId,
    turn_time: DateTime<Utc>,
) -> Vec<Claim> {
    use thinkingroot_extract::{fact_quality, segment, temporal};

    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for sent in segment::sentence_spans(text) {
        for (s, e) in segment::clause_spans(text, sent) {
            let unit = text[s..e].trim();
            if unit.chars().count() < MIN_FACT_CHARS || !fact_quality::is_useful_fact(unit) {
                continue;
            }
            let statement = format!("{speaker}: {unit}");
            let event = temporal::extract_event_date(unit).unwrap_or(turn_time);
            let mut claim = Claim::new(statement, ClaimType::Fact, source_id, workspace)
                .with_extraction_tier(ExtractionTier::Structural)
                .with_confidence(0.9)
                .with_event_date(event);
            // Anchor the bitemporal validity window to the event time so
            // `/claims/as-of` + temporal-reasoning queries resolve correctly.
            claim.valid_from = event;
            out.push(claim);
        }
    }
    out
}

/// Supersede older facts that the freshly-written facts update.
///
/// For each new fact carrying a mechanical [`supersession_key`], scan the
/// branch's existing `Fact` claims for one with the SAME subject+attribute key
/// but a DIFFERENT value, and mark it superseded (sets `valid_until` +
/// `superseded_by`, invalidates dependent capsules/experience). New facts from
/// this same turn are excluded so we only retire genuinely prior facts.
///
/// Reused at merge/consolidation time (C): pass the merging branch's facts as
/// `new_facts` against the target workspace's graph to retire cross-session
/// stale facts. Mechanical and high-precision by construction — the general
/// (rephrased-contradiction) case is left to the query-time LLM reader.
pub fn apply_write_supersession(
    graph: &GraphStore,
    new_facts: &[(String, String)],
) -> Result<usize> {
    use std::collections::HashSet;
    use thinkingroot_extract::supersession::supersession_key;

    let keyed: Vec<(&str, (String, String))> = new_facts
        .iter()
        .filter_map(|(id, stmt)| supersession_key(stmt).map(|k| (id.as_str(), k)))
        .collect();
    if keyed.is_empty() {
        return Ok(0);
    }
    let new_ids: HashSet<&str> = new_facts.iter().map(|(id, _)| id.as_str()).collect();
    let existing = graph.get_claims_by_type("Fact")?;

    let mut superseded = 0usize;
    for (new_id, (nkey, nval)) in &keyed {
        for (eid, estmt, _src, _conf, _uri) in &existing {
            if new_ids.contains(eid.as_str()) {
                continue; // never supersede a fact written this turn
            }
            if let Some((ekey, eval)) = supersession_key(estmt) {
                if &ekey == nkey && &eval != nval {
                    graph.supersede_claim(eid, new_id)?;
                    superseded += 1;
                }
            }
        }
    }
    Ok(superseded)
}

/// Persist one completed chat turn onto a stream branch's graph.
///
/// Best-effort by design: callers MUST NOT block the user's chat
/// response on this returning Ok. Failure paths (registry locked,
/// graph open error, etc.) propagate via the `Result` so the caller
/// can log them, but a downstream caller's typical pattern is
/// `let _ = persist_chat_turn(...).await.map_err(|e| tracing::warn!(...))`.
///
/// The function intentionally does NOT mutate the in-memory
/// [`SessionContext`] — the turn-number allocator
/// (`SessionContext::next_chat_turn`) runs inside the agent loop in
/// `agent_streaming.rs`; this function only persists.
///
/// Returns the IDs of the rows written so tests + callers can verify
/// without re-querying.
pub async fn persist_chat_turn(
    workspace_root: &Path,
    branch_name: &str,
    session_id: &str,
    turn_number: u64,
    user_question: &str,
    assistant_text: &str,
) -> Result<PersistedTurn> {
    let user_q = user_question.trim();
    let assistant_a = assistant_text.trim();
    if user_q.is_empty() && assistant_a.is_empty() {
        return Err(Error::Config(
            "persist_chat_turn: refusing to write a turn with empty user and assistant text"
                .into(),
        ));
    }

    let dir = thinkingroot_branch::snapshot::resolve_data_dir(workspace_root, Some(branch_name));
    let graph_dir = dir.join("graph");
    let graph = GraphStore::init(&graph_dir)
        .map_err(|e| Error::GraphStorage(format!("open branch graph: {e}")))?;

    let content = format!("Q: {user_q}\n\nA: {assistant_a}");
    let content_hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    // URI nests under `mcp://agent/{session_id}/...` so the existing
    // `branch_has_agent_contributes` prefix check picks it up — a
    // stream branch carrying only chat turns still routes through the
    // Phase-A AutoOnSessionEnd → topic merge flow.
    let uri = format!("mcp://agent/{session_id}/turn/{turn_number}/transcript");
    let source = Source::new(uri.clone(), SourceType::ChatMessage)
        .with_trust(TrustLevel::Untrusted)
        .with_hash(ContentHash(content_hash));
    let source_id = source.id;
    graph.insert_source(&source)?;

    // Searchable statement: the user's question is the high-signal
    // anchor (it's what the user came with and what they'll search by
    // in subsequent turns), followed by an arrow-marker and the
    // assistant's reply for retrieval-time matching against the
    // answer side too. Char-aware truncate keeps UTF-8 valid.
    let combined = format!("{user_q}\n→ {assistant_a}");
    let statement = if combined.chars().count() > MAX_STATEMENT_CHARS {
        let prefix: String = combined.chars().take(MAX_STATEMENT_CHARS).collect();
        format!("{prefix}…")
    } else {
        combined
    };

    let workspace = WorkspaceId::new();
    let claim = Claim::new(statement, ClaimType::Fact, source_id, workspace);
    let claim_id = claim.id.to_string();
    graph.insert_claim(&claim)?;
    graph.link_claim_to_source(&claim_id, &source_id.to_string())?;

    // Atomic fact distillation (2026-06-01): in addition to the turn-anchor
    // claim above, run the SAME mechanical witness-mesh extraction a batch
    // compile uses, so live conversation is stored as clean, retrievable,
    // speaker-attributed, temporally-anchored facts — not just one blob. This
    // is what closes the "streaming write is dumb" gap: the stream branch (and
    // everything it later merges into main) now carries fact-level memory.
    let now = chrono::Utc::now();
    let mut fact_claim_ids: Vec<String> = Vec::new();
    let mut all_ids: Vec<String> = vec![claim_id.clone()];
    // (id, statement) of facts written this turn — fed to write-time
    // supersession so a newer fact retires the older same-subject/attribute one.
    let mut new_facts: Vec<(String, String)> = Vec::new();
    for facts in [
        extract_turn_facts(user_q, "User", source_id, workspace, now),
        extract_turn_facts(assistant_a, "Assistant", source_id, workspace, now),
    ] {
        for fact in facts {
            let fid = fact.id.to_string();
            graph.insert_claim(&fact)?;
            graph.link_claim_to_source(&fid, &source_id.to_string())?;
            new_facts.push((fid.clone(), fact.statement.clone()));
            fact_claim_ids.push(fid.clone());
            all_ids.push(fid);
        }
    }

    // Write-time supersession (B, 2026-06-01): retire older facts that this
    // turn updates. Mechanical + high-precision (only clear "my X is Y" /
    // "I live in / moved to …" patterns) so a still-true fact is never wrongly
    // invalidated. Because a stream branch is forked from `main`, the candidate
    // scan also sees prior-session facts merged into main — so this covers the
    // cross-session knowledge-update case (C's mechanical half) too.
    if let Err(e) = apply_write_supersession(&graph, &new_facts) {
        tracing::warn!(error = %e, "supersession pass failed (non-fatal)");
    }

    // Turn calendar entry: upserts on (session_id, turn_number) so a
    // caller retry with the same coordinates updates the claim_ids
    // list rather than failing. The agent_streaming turn-number
    // allocator is monotonic per session, so the typical caller never
    // hits the upsert path — it's defensive against client retries.
    // Binds the anchor AND every distilled fact claim to the turn.
    graph.record_turn(session_id, turn_number, &all_ids)?;

    Ok(PersistedTurn {
        source_uri: uri,
        source_id: source_id.to_string(),
        claim_id,
        fact_claim_ids,
        turn_number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use thinkingroot_core::{BranchKind, BranchPermissions, MergePolicy};

    /// Stand up a fresh workspace + a stream branch ready to receive
    /// turn writes. Mirrors the seed pattern used by
    /// `tests/stream_cleanup_test.rs` but kept private here so the
    /// unit tests have a stable scaffold independent of the
    /// integration tests.
    async fn seed_stream_branch(session_id: &str) -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Seed main so the branch fork has a parent layout to copy.
        let graph_dir = root.join(".thinkingroot").join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        let _ = GraphStore::init(&graph_dir).unwrap();

        // Create the stream branch.
        let branch_name = format!("stream/{session_id}");
        thinkingroot_branch::create_branch_full(
            &root,
            &branch_name,
            "main",
            None,
            Some(session_id.to_string()),
            BranchPermissions::default(),
            BranchKind::Stream {
                session_id: session_id.to_string(),
            },
            MergePolicy::AutoOnSessionEnd,
            None,
        )
        .await
        .unwrap();

        (dir, root, branch_name)
    }

    #[tokio::test]
    async fn persist_chat_turn_writes_source_claim_and_turn_calendar() {
        let session_id = "persist-sess-1";
        let (_dir, root, branch_name) = seed_stream_branch(session_id).await;

        let persisted = persist_chat_turn(
            &root,
            &branch_name,
            session_id,
            1,
            "How does the auth refresh-token flow work?",
            "It rotates on every refresh; the old token is revoked after a 30s grace window.",
        )
        .await
        .expect("persist must succeed");

        // Verify the source row exists with the expected URI shape.
        let branch_dir =
            thinkingroot_branch::snapshot::resolve_data_dir(&root, Some(&branch_name));
        let graph = GraphStore::init(&branch_dir.join("graph")).unwrap();
        let sources = graph.get_all_sources().unwrap();
        assert!(
            sources.iter().any(|(_, uri, _, _)| uri == &persisted.source_uri),
            "source must be inserted with expected URI. got: {sources:?}"
        );
        assert_eq!(
            persisted.source_uri,
            format!("mcp://agent/{session_id}/turn/1/transcript")
        );

        // The URI uses the `mcp://agent/` prefix so the
        // `branch_has_agent_contributes` check in
        // `maintenance::cleanup_once` picks it up — without this, a
        // stream branch with ONLY chat turns (no engine.contribute
        // calls) would be silently abandoned by the cleanup task
        // instead of routed to a topic branch.
        assert!(
            persisted.source_uri.starts_with("mcp://agent/"),
            "URI must use the mcp://agent/ prefix so cleanup recognises it as agent work"
        );
    }

    #[tokio::test]
    async fn persist_chat_turn_distills_atomic_speaker_facts() {
        let session_id = "persist-sess-facts";
        let (_dir, root, branch_name) = seed_stream_branch(session_id).await;

        let persisted = persist_chat_turn(
            &root,
            &branch_name,
            session_id,
            1,
            "I prefer aisle seats. My dog is named Rex.",
            "The flight departs at noon. Aisle seats are reserved for you.",
        )
        .await
        .expect("persist must succeed");

        // The turn is no longer stored as a single blob — atomic facts are
        // distilled beyond the turn-anchor claim.
        assert!(
            !persisted.fact_claim_ids.is_empty(),
            "expected distilled fact claims beyond the turn anchor, got none"
        );

        let branch_dir =
            thinkingroot_branch::snapshot::resolve_data_dir(&root, Some(&branch_name));
        let graph = GraphStore::init(&branch_dir.join("graph")).unwrap();
        let statements: Vec<String> = persisted
            .fact_claim_ids
            .iter()
            .filter_map(|fid| graph.get_claim_by_id(fid).unwrap())
            .map(|c| c.statement)
            .collect();

        // Speaker-attributed atomic facts from BOTH sides.
        assert!(
            statements
                .iter()
                .any(|s| s.starts_with("User:") && s.contains("aisle seats")),
            "expected a User-attributed atomic fact, got: {statements:?}"
        );
        assert!(
            statements.iter().any(|s| s.starts_with("Assistant:")),
            "expected an Assistant-attributed atomic fact, got: {statements:?}"
        );
    }

    #[tokio::test]
    async fn persist_chat_turn_supersedes_updated_fact() {
        let session_id = "persist-sess-supersede";
        let (_dir, root, branch_name) = seed_stream_branch(session_id).await;

        // Turn 1 establishes the fact.
        persist_chat_turn(
            &root,
            &branch_name,
            session_id,
            1,
            "My car is a Toyota.",
            "Nice, Toyotas are reliable.",
        )
        .await
        .expect("turn 1 must persist");

        // Turn 2 updates it → the Toyota fact must be superseded by the Tesla fact.
        persist_chat_turn(
            &root,
            &branch_name,
            session_id,
            2,
            "My car is now a Tesla.",
            "Congrats on the new Tesla.",
        )
        .await
        .expect("turn 2 must persist");

        let branch_dir =
            thinkingroot_branch::snapshot::resolve_data_dir(&root, Some(&branch_name));
        let graph = GraphStore::init(&branch_dir.join("graph")).unwrap();
        assert!(
            graph.count_superseded_claims().unwrap() >= 1,
            "the older 'My car is a Toyota' fact should be superseded by the Tesla update"
        );
    }

    #[tokio::test]
    async fn persist_chat_turn_truncates_long_statement_on_char_boundary() {
        let session_id = "persist-sess-long";
        let (_dir, root, branch_name) = seed_stream_branch(session_id).await;

        // 3000 multi-byte UTF-8 chars — exceeds the 1024 char cap
        // and would corrupt the graph if truncation cut on byte
        // boundaries instead of char boundaries.
        let long_q: String = "你".repeat(3000);
        let persisted = persist_chat_turn(
            &root,
            &branch_name,
            session_id,
            1,
            &long_q,
            "short answer",
        )
        .await
        .expect("persist must succeed under truncation");

        // Pull the claim back and verify statement is valid UTF-8
        // and capped at MAX_STATEMENT_CHARS + the ellipsis marker.
        let branch_dir =
            thinkingroot_branch::snapshot::resolve_data_dir(&root, Some(&branch_name));
        let graph = GraphStore::init(&branch_dir.join("graph")).unwrap();
        let claim = graph
            .get_claim_by_id(&persisted.claim_id)
            .unwrap()
            .expect("claim must be readable after persist");
        let stmt = &claim.statement;
        assert!(
            std::str::from_utf8(stmt.as_bytes()).is_ok(),
            "statement must be valid UTF-8 — no codepoint split"
        );
        // Truncated to MAX_STATEMENT_CHARS chars + the trailing
        // ellipsis marker.
        let count = stmt.chars().count();
        assert!(
            count <= MAX_STATEMENT_CHARS + 1,
            "statement char count {count} must be ≤ {MAX_STATEMENT_CHARS} + 1 ellipsis"
        );
        assert!(stmt.ends_with('…'), "truncation marker must end the string");
    }

    #[tokio::test]
    async fn persist_chat_turn_rejects_fully_empty_input() {
        let session_id = "persist-sess-empty";
        let (_dir, root, branch_name) = seed_stream_branch(session_id).await;

        let result =
            persist_chat_turn(&root, &branch_name, session_id, 1, "   ", "\n\t").await;
        assert!(
            result.is_err(),
            "fully-empty turn must be rejected — there's no signal to persist"
        );
    }

    #[tokio::test]
    async fn persist_chat_turn_records_turn_calendar_binding() {
        let session_id = "persist-sess-cal";
        let (_dir, root, branch_name) = seed_stream_branch(session_id).await;

        let p1 = persist_chat_turn(&root, &branch_name, session_id, 1, "Q1", "A1")
            .await
            .unwrap();
        let p2 = persist_chat_turn(&root, &branch_name, session_id, 2, "Q2", "A2")
            .await
            .unwrap();
        let p3 = persist_chat_turn(&root, &branch_name, session_id, 3, "Q3", "A3")
            .await
            .unwrap();

        let branch_dir =
            thinkingroot_branch::snapshot::resolve_data_dir(&root, Some(&branch_name));
        let graph = GraphStore::init(&branch_dir.join("graph")).unwrap();
        let turns = graph.query_turns_for_session(session_id).unwrap();

        assert_eq!(
            turns.len(),
            3,
            "all three turns must be in the calendar. got: {turns:?}"
        );
        // Turn calendar must contain each persisted claim id exactly
        // once. We don't assume the rows come back in turn order —
        // map turn_number → claim_ids and assert membership.
        let by_turn: std::collections::HashMap<u64, Vec<String>> = turns
            .into_iter()
            .map(|t| (t.turn_number, t.claim_ids))
            .collect();
        assert!(by_turn.get(&1).is_some_and(|v| v.contains(&p1.claim_id)));
        assert!(by_turn.get(&2).is_some_and(|v| v.contains(&p2.claim_id)));
        assert!(by_turn.get(&3).is_some_and(|v| v.contains(&p3.claim_id)));
    }

    #[tokio::test]
    async fn persist_chat_turn_handles_only_user_or_only_assistant_text() {
        // One side empty is allowed (a turn where the agent only
        // received a thumbs-up emoji is still a turn) — full-empty
        // is what we reject. Both directions tested.
        let session_id = "persist-sess-half";
        let (_dir, root, branch_name) = seed_stream_branch(session_id).await;

        assert!(
            persist_chat_turn(&root, &branch_name, session_id, 1, "only user", "")
                .await
                .is_ok(),
            "user-only turn must be allowed"
        );
        assert!(
            persist_chat_turn(&root, &branch_name, session_id, 2, "", "only assistant")
                .await
                .is_ok(),
            "assistant-only turn must be allowed"
        );
    }
}
