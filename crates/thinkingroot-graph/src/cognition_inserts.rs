//! Cognition-commit insert + query helpers.
//!
//! Bridges the in-memory `CognitionCommit` type (from
//! `thinkingroot-core::types::cognition`) to the CozoDB
//! `cognition_commits` table added by Phase β.1 of the design doc
//! (`docs/2026-05-15-cognition-commits-design.md`).
//!
//! Two design rules that distinguish this from `witness_inserts.rs`:
//!
//! 1. **Citation verification on insert.** Every `WitnessId` in
//!    `witnesses_added` and `citations` MUST resolve to an existing
//!    row in the `witnesses` table before we accept the commit. A
//!    commit citing a fabricated witness is exactly the kind of
//!    silent dishonesty CLAUDE.md §honesty-rules forbids — better to
//!    refuse the insert loudly than land a row downstream consumers
//!    cannot click through to verify.
//!
//! 2. **Parent existence is enforced for non-root commits.** When a
//!    `CognitionCommit.parent = Some(id)`, we verify `id` exists on
//!    the same branch. A dangling parent pointer breaks `walk_commit_
//!    ancestors` silently, so we catch it at write time.
//!
//! Both checks are mandatory; there is no `--skip-verify` flag.

use std::collections::BTreeMap;

use cozo::{DataValue, Num};
use thinkingroot_core::types::{CognitionCommit, CommitAuthor, CommitId, WitnessId};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

fn s(value: impl Into<String>) -> DataValue {
    DataValue::Str(value.into().into())
}

fn f(value: f64) -> DataValue {
    DataValue::Num(Num::Float(value))
}

/// JSON-encode a witness-id list. Surfaces encode failure rather than
/// silently writing `"[]"` — matches the witness_inserts pattern.
fn encode_witness_ids(field: &str, ids: &[WitnessId]) -> Result<String> {
    let hex: Vec<String> = ids.iter().map(|w| w.to_hex()).collect();
    serde_json::to_string(&hex).map_err(|e| {
        Error::GraphStorage(format!("encode cognition_commit.{field}: {e}"))
    })
}

fn decode_witness_ids(field: &str, json_str: &str) -> Result<Vec<WitnessId>> {
    let hex: Vec<String> = serde_json::from_str(json_str).map_err(|e| {
        Error::GraphStorage(format!("decode cognition_commit.{field}: {e}"))
    })?;
    let mut out: Vec<WitnessId> = Vec::with_capacity(hex.len());
    for h in hex {
        let id = WitnessId::from_hex(&h).map_err(|e| {
            Error::GraphStorage(format!(
                "cognition_commit.{field} contains invalid witness id `{h}`: {e}"
            ))
        })?;
        out.push(id);
    }
    Ok(out)
}

fn encode_strings(field: &str, items: &[String]) -> Result<String> {
    serde_json::to_string(items).map_err(|e| {
        Error::GraphStorage(format!("encode cognition_commit.{field}: {e}"))
    })
}

fn decode_strings(field: &str, json_str: &str) -> Result<Vec<String>> {
    serde_json::from_str(json_str).map_err(|e| {
        Error::GraphStorage(format!("decode cognition_commit.{field}: {e}"))
    })
}

impl GraphStore {
    /// Insert a single cognition commit. Verifies every cited /
    /// added witness id resolves to a real row in `witnesses`, and
    /// (for non-root commits) verifies the parent exists on the same
    /// branch. Refuses fabricated citations with a typed error so the
    /// caller can surface "your AI cited a witness that doesn't exist
    /// — refusing to commit" to the user.
    pub fn insert_cognition_commit(&self, commit: &CognitionCommit) -> Result<()> {
        // 1. Verify cited + added witnesses all exist.
        self.verify_witnesses_exist("citations", &commit.citations)?;
        self.verify_witnesses_exist("witnesses_added", &commit.witnesses_added)?;
        // 2. Verify parent (when present) exists on the same branch.
        if let Some(parent) = &commit.parent {
            let exists = self.get_cognition_commit(parent)?;
            match exists {
                Some(p) if p.branch == commit.branch => {}
                Some(p) => {
                    return Err(Error::GraphStorage(format!(
                        "cognition commit {} declares parent {} on branch `{}`, \
                         but parent is on branch `{}`",
                        commit.id, parent, commit.branch, p.branch
                    )));
                }
                None => {
                    return Err(Error::GraphStorage(format!(
                        "cognition commit {} declares parent {} which does not exist",
                        commit.id, parent
                    )));
                }
            }
        }

        let (author_kind, author_id, author_model) = match &commit.author {
            CommitAuthor::User { id } => ("user", id.clone(), String::new()),
            CommitAuthor::Agent { model, principal } => {
                ("agent", principal.clone(), model.clone())
            }
        };
        let witnesses_added_json =
            encode_witness_ids("witnesses_added", &commit.witnesses_added)?;
        let citations_json = encode_witness_ids("citations", &commit.citations)?;
        let gaps_surfaced_json =
            encode_strings("gaps_surfaced", &commit.gaps_surfaced)?;

        let mut params = BTreeMap::new();
        params.insert("id".into(), s(commit.id.to_hex()));
        params.insert(
            "parent_id".into(),
            s(commit
                .parent
                .as_ref()
                .map(|p| p.to_hex())
                .unwrap_or_default()),
        );
        params.insert("branch".into(), s(commit.branch.clone()));
        params.insert("author_kind".into(), s(author_kind));
        params.insert("author_id".into(), s(author_id));
        params.insert("author_model".into(), s(author_model));
        params.insert("prompt".into(), s(commit.prompt.clone()));
        params.insert("reasoning".into(), s(commit.reasoning.clone()));
        params.insert("witnesses_added_json".into(), s(witnesses_added_json));
        params.insert("citations_json".into(), s(citations_json));
        params.insert("gaps_surfaced_json".into(), s(gaps_surfaced_json));
        params.insert(
            "created_at".into(),
            f(commit.created_at.timestamp() as f64),
        );

        let script = "
            ?[
                id, parent_id, branch, author_kind, author_id, author_model,
                prompt, reasoning, witnesses_added_json, citations_json,
                gaps_surfaced_json, created_at
            ] <- [[
                $id, $parent_id, $branch, $author_kind, $author_id, $author_model,
                $prompt, $reasoning, $witnesses_added_json, $citations_json,
                $gaps_surfaced_json, $created_at
            ]]
            :put cognition_commits {
                id =>
                parent_id, branch, author_kind, author_id, author_model,
                prompt, reasoning, witnesses_added_json, citations_json,
                gaps_surfaced_json, created_at
            }
        ";
        self.query(script, params).map_err(|e| {
            Error::GraphStorage(format!("insert_cognition_commit({}): {e}", commit.id))
        })?;
        Ok(())
    }

    /// Verify every witness id in `ids` corresponds to a row in
    /// `witnesses`. Returns `Err(GraphStorage)` with the first
    /// missing id when a fabricated reference is detected — gives
    /// the user-facing surface a concrete "this is the witness that
    /// doesn't exist" message.
    fn verify_witnesses_exist(&self, field: &str, ids: &[WitnessId]) -> Result<()> {
        for id in ids {
            let mut params = BTreeMap::new();
            params.insert("wid".into(), s(id.to_hex()));
            let result = self
                .query(
                    "?[id] := *witnesses{id}, id = $wid",
                    params,
                )
                .map_err(|e| {
                    Error::GraphStorage(format!(
                        "verify_witnesses_exist[{field}] query: {e}"
                    ))
                })?;
            if result.rows.is_empty() {
                return Err(Error::GraphStorage(format!(
                    "cognition_commit.{field}: witness {} does not exist in this \
                     workspace — refusing to insert a commit with fabricated citation",
                    id.to_hex()
                )));
            }
        }
        Ok(())
    }

    /// Fetch a single cognition commit by id. Returns `None` when not
    /// present rather than an error — the calling MCP / REST handlers
    /// map missing to 404.
    pub fn get_cognition_commit(&self, id: &CommitId) -> Result<Option<CognitionCommit>> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), s(id.to_hex()));
        let script = "
            ?[
                id, parent_id, branch, author_kind, author_id, author_model,
                prompt, reasoning, witnesses_added_json, citations_json,
                gaps_surfaced_json, created_at
            ] := *cognition_commits{
                id, parent_id, branch, author_kind, author_id, author_model,
                prompt, reasoning, witnesses_added_json, citations_json,
                gaps_surfaced_json, created_at
            }, id = $cid
        ";
        let result = self
            .query(script, params)
            .map_err(|e| Error::GraphStorage(format!("get_cognition_commit: {e}")))?;
        match result.rows.first() {
            Some(row) => Ok(Some(parse_cognition_row(row)?)),
            None => Ok(None),
        }
    }

    /// List commits on a branch in descending `created_at` order
    /// (newest first). `limit = None` returns every commit; tests can
    /// pass `Some(N)` for stable assertions on small fixtures. The
    /// `created_at` sort is applied AFTER the Datalog filter — Cozo
    /// stratified evaluation can't always push the ORDER BY through.
    pub fn list_cognition_commits_on_branch(
        &self,
        branch: &str,
        limit: Option<usize>,
    ) -> Result<Vec<CognitionCommit>> {
        let mut params = BTreeMap::new();
        params.insert("br".into(), s(branch.to_string()));
        let script = "
            ?[
                id, parent_id, branch, author_kind, author_id, author_model,
                prompt, reasoning, witnesses_added_json, citations_json,
                gaps_surfaced_json, created_at
            ] := *cognition_commits{
                id, parent_id, branch, author_kind, author_id, author_model,
                prompt, reasoning, witnesses_added_json, citations_json,
                gaps_surfaced_json, created_at
            }, branch = $br
        ";
        let result = self
            .query(script, params)
            .map_err(|e| Error::GraphStorage(format!("list_cognition_commits: {e}")))?;
        let mut out: Vec<CognitionCommit> = Vec::with_capacity(result.rows.len());
        for row in &result.rows {
            out.push(parse_cognition_row(row)?);
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        if let Some(n) = limit {
            out.truncate(n);
        }
        Ok(out)
    }

    /// Walk the parent chain from `start_id` up to `max_depth` hops.
    /// Returns the commits in walk order: `start_id` first, then its
    /// parent, then its grandparent, etc. Stops early if a parent is
    /// missing OR `parent_id` is empty (root commit).
    pub fn walk_commit_ancestors(
        &self,
        start_id: &CommitId,
        max_depth: usize,
    ) -> Result<Vec<CognitionCommit>> {
        let mut out: Vec<CognitionCommit> = Vec::new();
        let mut current = Some(*start_id);
        for _ in 0..=max_depth {
            let id = match current {
                Some(id) => id,
                None => break,
            };
            let commit = match self.get_cognition_commit(&id)? {
                Some(c) => c,
                None => break,
            };
            let next = commit.parent;
            out.push(commit);
            current = next;
        }
        Ok(out)
    }

    /// Total commits in the workspace's `cognition_commits` table.
    /// Cheap (Cozo aggregate over the indexed table). Uses the same
    /// `count(id)` aggregate idiom as `count_witnesses` —
    /// `query_read` evaluates the aggregate in the read planner.
    pub fn count_cognition_commits(&self) -> Result<u64> {
        let script = "?[count(id)] := *cognition_commits{id}";
        let result = self
            .query_read(script)
            .map_err(|e| Error::GraphStorage(format!("count_cognition_commits: {e}")))?;
        if let Some(row) = result.rows.first()
            && let Some(DataValue::Num(Num::Int(n))) = row.first()
        {
            return Ok((*n).max(0) as u64);
        }
        Ok(0)
    }
}

/// Project a CozoDB row into `CognitionCommit`. Centralised so the
/// 12-column projection used by `get` / `list` / `walk` can't drift
/// across call sites.
fn parse_cognition_row(row: &[DataValue]) -> Result<CognitionCommit> {
    fn str_at(row: &[DataValue], idx: usize, field: &str) -> Result<String> {
        match row.get(idx) {
            Some(DataValue::Str(s)) => Ok(s.to_string()),
            _ => Err(Error::GraphStorage(format!(
                "cognition_commits row missing string field `{field}` at {idx}"
            ))),
        }
    }
    fn opt_str_at(row: &[DataValue], idx: usize) -> Option<String> {
        match row.get(idx) {
            Some(DataValue::Str(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    let id_hex = str_at(row, 0, "id")?;
    let id = CommitId::from_hex(&id_hex).map_err(|e| {
        Error::GraphStorage(format!("invalid commit id `{id_hex}` in row: {e}"))
    })?;

    let parent_hex = opt_str_at(row, 1).unwrap_or_default();
    let parent = if parent_hex.is_empty() {
        None
    } else {
        Some(CommitId::from_hex(&parent_hex).map_err(|e| {
            Error::GraphStorage(format!(
                "invalid parent commit id `{parent_hex}` in row: {e}"
            ))
        })?)
    };

    let branch = str_at(row, 2, "branch")?;
    let author_kind = str_at(row, 3, "author_kind")?;
    let author_id = str_at(row, 4, "author_id")?;
    let author_model = str_at(row, 5, "author_model")?;
    let prompt = str_at(row, 6, "prompt")?;
    let reasoning = str_at(row, 7, "reasoning")?;
    let witnesses_added_json = str_at(row, 8, "witnesses_added_json")?;
    let citations_json = str_at(row, 9, "citations_json")?;
    let gaps_surfaced_json = str_at(row, 10, "gaps_surfaced_json")?;
    let created_at_secs = match row.get(11) {
        Some(DataValue::Num(Num::Float(f))) => *f,
        Some(DataValue::Num(Num::Int(i))) => *i as f64,
        _ => 0.0,
    };

    let author = match author_kind.as_str() {
        "user" => CommitAuthor::User { id: author_id },
        "agent" => CommitAuthor::Agent {
            model: author_model,
            principal: author_id,
        },
        other => {
            return Err(Error::GraphStorage(format!(
                "cognition_commits row carries unknown author_kind `{other}`"
            )));
        }
    };

    let witnesses_added = decode_witness_ids("witnesses_added", &witnesses_added_json)?;
    let citations = decode_witness_ids("citations", &citations_json)?;
    let gaps_surfaced = decode_strings("gaps_surfaced", &gaps_surfaced_json)?;

    let created_at = chrono::DateTime::<chrono::Utc>::from_timestamp(
        created_at_secs as i64,
        0,
    )
    .unwrap_or_else(chrono::Utc::now);

    Ok(CognitionCommit {
        id,
        parent,
        branch,
        author,
        prompt,
        reasoning,
        witnesses_added,
        citations,
        gaps_surfaced,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use thinkingroot_core::types::{
        Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
    };

    fn fresh_store() -> GraphStore {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Leak the tempdir for the duration of the test process —
        // matches the witness_inserts fixture pattern; the OS reclaims
        // at process exit.
        let path = Box::leak(Box::new(tmp));
        GraphStore::init(path.path()).expect("graph store init")
    }

    fn fixture_witness(byte: u8) -> Witness {
        let file_hash = format!("{:0>64}", format!("{byte:x}"));
        let span = WitnessSpan {
            file_blake3: file_hash.clone(),
            start: byte as u64 * 16,
            end: byte as u64 * 16 + 8,
        };
        Witness::new(
            "test::fixture@v1",
            "test",
            vec![WitnessInput::ByteRef {
                file_blake3: file_hash.clone(),
                start: span.start,
                end: span.end,
            }],
            vec![span],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            blake3::hash(format!("fixture-bytes-{byte}").as_bytes())
                .to_hex()
                .to_string(),
            Utc::now(),
        )
    }

    fn agent_author() -> CommitAuthor {
        CommitAuthor::Agent {
            model: "claude-opus-4-7".to_string(),
            principal: "thinkingroot".to_string(),
        }
    }

    #[test]
    fn insert_round_trip_round_trips_every_field() {
        let store = fresh_store();
        let w = fixture_witness(1);
        store.insert_witness(&w).unwrap();

        let commit = CognitionCommit::new(
            None,
            "main".to_string(),
            agent_author(),
            "what is x?".to_string(),
            "x is y, citing [[witness:abc]]".to_string(),
            vec![w.id],
            vec![w.id],
            vec!["gap-1".to_string(), "gap-2".to_string()],
            Utc::now(),
        );
        store.insert_cognition_commit(&commit).unwrap();

        let fetched = store
            .get_cognition_commit(&commit.id)
            .unwrap()
            .expect("commit fetched");
        assert_eq!(fetched.id, commit.id);
        assert_eq!(fetched.branch, "main");
        assert_eq!(fetched.parent, None);
        assert_eq!(fetched.prompt, commit.prompt);
        assert_eq!(fetched.reasoning, commit.reasoning);
        assert_eq!(fetched.witnesses_added, vec![w.id]);
        assert_eq!(fetched.citations, vec![w.id]);
        assert_eq!(fetched.gaps_surfaced.len(), 2);
        match fetched.author {
            CommitAuthor::Agent { model, principal } => {
                assert_eq!(model, "claude-opus-4-7");
                assert_eq!(principal, "thinkingroot");
            }
            other => panic!("expected Agent author, got {other:?}"),
        }
    }

    #[test]
    fn insert_refuses_fabricated_citation() {
        let store = fresh_store();
        // Note: we do NOT insert the witness — citation refers to a
        // witness id that does not exist in this workspace.
        let fake = WitnessId::derive("nonexistent::rule@v1", &[WitnessSpan {
            file_blake3: "ff".repeat(32),
            start: 0,
            end: 1,
        }]);
        let commit = CognitionCommit::new(
            None,
            "main".to_string(),
            agent_author(),
            "q".to_string(),
            "fabricated answer".to_string(),
            vec![],
            vec![fake],
            vec![],
            Utc::now(),
        );
        let err = store.insert_cognition_commit(&commit).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("does not exist"),
            "expected fabricated-citation refusal, got: {msg}"
        );
    }

    #[test]
    fn insert_refuses_dangling_parent() {
        let store = fresh_store();
        let phantom_parent = CommitId([7u8; 32]);
        let commit = CognitionCommit::new(
            Some(phantom_parent),
            "main".to_string(),
            agent_author(),
            "q".to_string(),
            "r".to_string(),
            vec![],
            vec![],
            vec![],
            Utc::now(),
        );
        let err = store.insert_cognition_commit(&commit).unwrap_err();
        assert!(format!("{err}").contains("does not exist"));
    }

    #[test]
    fn insert_refuses_parent_on_wrong_branch() {
        let store = fresh_store();
        let parent = CognitionCommit::new(
            None,
            "main".to_string(),
            agent_author(),
            "q".to_string(),
            "r".to_string(),
            vec![],
            vec![],
            vec![],
            Utc::now(),
        );
        store.insert_cognition_commit(&parent).unwrap();

        // Child declares the parent but claims to be on `feature/x`.
        let child = CognitionCommit::new(
            Some(parent.id),
            "feature/x".to_string(),
            agent_author(),
            "q2".to_string(),
            "r2".to_string(),
            vec![],
            vec![],
            vec![],
            Utc::now(),
        );
        let err = store.insert_cognition_commit(&child).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("branch"));
    }

    #[test]
    fn list_returns_branch_scoped_commits_newest_first() {
        let store = fresh_store();
        let now = Utc::now();
        let c1 = CognitionCommit::new(
            None,
            "main".to_string(),
            agent_author(),
            "q1".to_string(),
            "r1".to_string(),
            vec![],
            vec![],
            vec![],
            now - chrono::Duration::seconds(2),
        );
        store.insert_cognition_commit(&c1).unwrap();
        let c2 = CognitionCommit::new(
            Some(c1.id),
            "main".to_string(),
            agent_author(),
            "q2".to_string(),
            "r2".to_string(),
            vec![],
            vec![],
            vec![],
            now,
        );
        store.insert_cognition_commit(&c2).unwrap();
        // Other-branch commit must not leak into main's listing.
        let other = CognitionCommit::new(
            None,
            "feature/x".to_string(),
            agent_author(),
            "qx".to_string(),
            "rx".to_string(),
            vec![],
            vec![],
            vec![],
            now,
        );
        store.insert_cognition_commit(&other).unwrap();

        let main = store
            .list_cognition_commits_on_branch("main", None)
            .unwrap();
        assert_eq!(main.len(), 2);
        // Newest first.
        assert_eq!(main[0].id, c2.id);
        assert_eq!(main[1].id, c1.id);

        let limit_1 = store
            .list_cognition_commits_on_branch("main", Some(1))
            .unwrap();
        assert_eq!(limit_1.len(), 1);
        assert_eq!(limit_1[0].id, c2.id);
    }

    #[test]
    fn walk_ancestors_returns_chain_newest_first() {
        let store = fresh_store();
        let c1 = CognitionCommit::new(
            None,
            "main".to_string(),
            agent_author(),
            "q1".to_string(),
            "r1".to_string(),
            vec![],
            vec![],
            vec![],
            Utc::now(),
        );
        store.insert_cognition_commit(&c1).unwrap();
        let c2 = CognitionCommit::new(
            Some(c1.id),
            "main".to_string(),
            agent_author(),
            "q2".to_string(),
            "r2".to_string(),
            vec![],
            vec![],
            vec![],
            Utc::now(),
        );
        store.insert_cognition_commit(&c2).unwrap();
        let c3 = CognitionCommit::new(
            Some(c2.id),
            "main".to_string(),
            agent_author(),
            "q3".to_string(),
            "r3".to_string(),
            vec![],
            vec![],
            vec![],
            Utc::now(),
        );
        store.insert_cognition_commit(&c3).unwrap();

        let chain = store.walk_commit_ancestors(&c3.id, 10).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, c3.id);
        assert_eq!(chain[1].id, c2.id);
        assert_eq!(chain[2].id, c1.id);

        // Cap at depth=1 → just c3 and its immediate parent.
        let two = store.walk_commit_ancestors(&c3.id, 1).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].id, c3.id);
        assert_eq!(two[1].id, c2.id);
    }

    #[test]
    fn count_commits_returns_total() {
        let store = fresh_store();
        assert_eq!(store.count_cognition_commits().unwrap(), 0);
        let c = CognitionCommit::new(
            None,
            "main".to_string(),
            agent_author(),
            "q".to_string(),
            "r".to_string(),
            vec![],
            vec![],
            vec![],
            Utc::now(),
        );
        store.insert_cognition_commit(&c).unwrap();
        assert_eq!(store.count_cognition_commits().unwrap(), 1);
    }

    #[test]
    fn get_returns_none_for_unknown_commit() {
        let store = fresh_store();
        let phantom = CommitId([0xab; 32]);
        assert!(store.get_cognition_commit(&phantom).unwrap().is_none());
    }
}
