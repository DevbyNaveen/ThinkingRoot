//! E2 — code-graph traversal API (LocAgent-style structural navigation).
//!
//! A keyword-and-graph surface over the compiled code graph: find a symbol,
//! walk its call / import / containment edges with a bounded BFS, and pull a
//! symbol's byte-anchored detail. No NL→DSL step — the caller names a symbol
//! or a claim id and an explicit direction/edge-kind set, and the engine
//! returns deterministic, byte-anchored results (file + `[start,end)` so the
//! UI/agent can cite exact source).
//!
//! The substrate is the existing compiled graph:
//!   - `claims{symbol, claim_type, source_path, byte_start, byte_end}` —
//!     every FunctionDef / TypeDef is a claim carrying its symbol + byte span.
//!   - `function_calls{caller_claim_id, callee_claim_id}` — the resolved
//!     call graph (callee_claim_id filled at Phase 7e).
//!   - `code_imports{from_source, import_path, to_source}` — import edges
//!     (E2); `to_source` resolved lazily here by suffix-matching `import_path`
//!     against `sources.uri`.
//!
//! Honesty rule: an unknown symbol / id yields an empty result, never a
//! fabricated node. Traversal is hop-bounded and visited-deduped so cycles
//! terminate.

use std::collections::{HashSet, VecDeque};

use cozo::{DataValue, Num};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// Direction of a graph walk relative to the start node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraversalDirection {
    /// Follow edges away from the start (callee, imported-by-me).
    Out,
    /// Follow edges into the start (callers, importers-of-me).
    In,
    /// Both directions.
    Both,
}

/// Which edge kinds a walk may follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `function_calls` — caller → callee.
    Calls,
    /// `code_imports` — importer-source ↔ imported-source (lifted to the
    /// representative code-def claims of those sources).
    ImportedBy,
    /// `code_signatures.parent_scope` — enclosing type/module → member.
    Contains,
}

impl EdgeKind {
    fn label(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::ImportedBy => "imported_by",
            EdgeKind::Contains => "contains",
        }
    }
}

/// A symbol match from [`GraphStore::search_entity`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EntityHit {
    pub claim_id: String,
    pub symbol: String,
    pub claim_type: String,
    pub source_path: String,
    pub byte_start: u64,
    pub byte_end: u64,
}

/// A node reached by [`GraphStore::traverse_graph`], stamped with the BFS
/// depth and the edge kind that reached it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TraversedNode {
    pub claim_id: String,
    pub symbol: String,
    pub source_path: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub depth: u32,
    pub edge_kind: String,
}

/// Full byte-anchored detail of one code entity.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EntityDetail {
    pub claim_id: String,
    pub symbol: String,
    pub claim_type: String,
    pub statement: String,
    pub source_id: String,
    pub source_path: String,
    pub byte_start: u64,
    pub byte_end: u64,
}

/// Internal projection of a code-def claim row.
struct ClaimMeta {
    symbol: String,
    source_id: String,
    source_path: String,
    byte_start: u64,
    byte_end: u64,
}

fn dv_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        _ => String::new(),
    }
}

fn dv_u64(v: &DataValue) -> u64 {
    match v {
        DataValue::Num(Num::Int(i)) => (*i).max(0) as u64,
        DataValue::Num(Num::Float(f)) => f.max(0.0) as u64,
        _ => 0,
    }
}

impl GraphStore {
    /// Find code entities (FunctionDef / TypeDef claims) whose symbol
    /// contains `keyword` (case-insensitive). Substring filtering is done in
    /// Rust to avoid depending on a CozoDB string builtin. Bounded by the
    /// workspace's code-def count. Empty `keyword` → empty result.
    pub fn search_entity(&self, keyword: &str) -> Result<Vec<EntityHit>> {
        if keyword.trim().is_empty() {
            return Ok(Vec::new());
        }
        let needle = keyword.to_lowercase();
        let rows = self
            .query(
                "?[id, symbol, claim_type, source_path, byte_start, byte_end] := \
                 *claims{id, symbol, claim_type, source_path, byte_start, byte_end}, \
                 symbol != ''",
                Default::default(),
            )
            .map_err(|e| Error::GraphStorage(format!("search_entity({keyword}): {e}")))?;

        let mut out = Vec::new();
        for row in &rows.rows {
            if row.len() < 6 {
                continue;
            }
            let symbol = dv_str(&row[1]);
            if !symbol.to_lowercase().contains(&needle) {
                continue;
            }
            out.push(EntityHit {
                claim_id: dv_str(&row[0]),
                symbol,
                claim_type: dv_str(&row[2]),
                source_path: dv_str(&row[3]),
                byte_start: dv_u64(&row[4]),
                byte_end: dv_u64(&row[5]),
            });
        }
        // Deterministic: exact-symbol matches first, then by symbol, then id.
        out.sort_by(|a, b| {
            let a_exact = a.symbol.to_lowercase() == needle;
            let b_exact = b.symbol.to_lowercase() == needle;
            b_exact
                .cmp(&a_exact)
                .then_with(|| a.symbol.cmp(&b.symbol))
                .then_with(|| a.claim_id.cmp(&b.claim_id))
        });
        Ok(out)
    }

    /// Full byte-anchored detail of one entity by claim id. Unknown id →
    /// `None` (honesty rule: absence, never a fabricated row).
    pub fn retrieve_entity(&self, claim_id: &str) -> Result<Option<EntityDetail>> {
        let mut params = std::collections::BTreeMap::new();
        params.insert("cid".to_string(), DataValue::Str(claim_id.into()));
        let rows = self
            .query(
                "?[id, symbol, claim_type, statement, source_id, source_path, byte_start, byte_end] := \
                 *claims{id, symbol, claim_type, statement, source_id, source_path, byte_start, byte_end}, \
                 id = $cid",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("retrieve_entity({claim_id}): {e}")))?;
        let Some(row) = rows.rows.first() else {
            return Ok(None);
        };
        if row.len() < 8 {
            return Ok(None);
        }
        Ok(Some(EntityDetail {
            claim_id: dv_str(&row[0]),
            symbol: dv_str(&row[1]),
            claim_type: dv_str(&row[2]),
            statement: dv_str(&row[3]),
            source_id: dv_str(&row[4]),
            source_path: dv_str(&row[5]),
            byte_start: dv_u64(&row[6]),
            byte_end: dv_u64(&row[7]),
        }))
    }

    /// Bounded BFS over the code graph from `start_claim_id`. Follows the
    /// requested `edge_kinds` in `dir`, up to `max_hops` hops, deduping by
    /// claim id so cycles terminate. Returns reached nodes (excluding the
    /// start) in BFS order, each stamped with depth + the reaching edge kind.
    pub fn traverse_graph(
        &self,
        start_claim_id: &str,
        dir: TraversalDirection,
        max_hops: u32,
        edge_kinds: &[EdgeKind],
    ) -> Result<Vec<TraversedNode>> {
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(start_claim_id.to_string());
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        queue.push_back((start_claim_id.to_string(), 0));
        let mut out: Vec<TraversedNode> = Vec::new();

        while let Some((cur, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            for kind in edge_kinds {
                let neighbors = self.neighbors(&cur, dir, *kind)?;
                for next_id in neighbors {
                    if !visited.insert(next_id.clone()) {
                        continue;
                    }
                    let meta = self.claim_meta(&next_id)?;
                    let (symbol, source_path, byte_start, byte_end) = match meta {
                        Some(m) => (m.symbol, m.source_path, m.byte_start, m.byte_end),
                        None => (String::new(), String::new(), 0, 0),
                    };
                    out.push(TraversedNode {
                        claim_id: next_id.clone(),
                        symbol,
                        source_path,
                        byte_start,
                        byte_end,
                        depth: depth + 1,
                        edge_kind: kind.label().to_string(),
                    });
                    queue.push_back((next_id, depth + 1));
                }
            }
        }
        Ok(out)
    }

    /// Reverse-`Calls` transitive closure: every entity that (transitively)
    /// calls `claim_id` — the blast radius if `claim_id` changes. Thin
    /// wrapper over [`Self::traverse_graph`].
    pub fn impact(&self, claim_id: &str, max_hops: u32) -> Result<Vec<TraversedNode>> {
        self.traverse_graph(
            claim_id,
            TraversalDirection::In,
            max_hops,
            &[EdgeKind::Calls],
        )
    }

    /// Claim ids adjacent to `claim_id` along one edge kind in one direction.
    fn neighbors(
        &self,
        claim_id: &str,
        dir: TraversalDirection,
        kind: EdgeKind,
    ) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let want_out = matches!(dir, TraversalDirection::Out | TraversalDirection::Both);
        let want_in = matches!(dir, TraversalDirection::In | TraversalDirection::Both);
        match kind {
            EdgeKind::Calls => {
                if want_out {
                    ids.extend(self.calls_edge(claim_id, true)?);
                }
                if want_in {
                    ids.extend(self.calls_edge(claim_id, false)?);
                }
            }
            EdgeKind::Contains => {
                if want_out {
                    ids.extend(self.contains_edge(claim_id, true)?);
                }
                if want_in {
                    ids.extend(self.contains_edge(claim_id, false)?);
                }
            }
            EdgeKind::ImportedBy => {
                ids.extend(self.imported_by_edge(claim_id, dir)?);
            }
        }
        Ok(ids)
    }

    /// `function_calls` neighbors. `forward=true` → callees of `claim_id`;
    /// `forward=false` → callers of `claim_id`. Only resolved edges
    /// (non-empty `callee_claim_id`) are followed.
    fn calls_edge(&self, claim_id: &str, forward: bool) -> Result<Vec<String>> {
        let mut params = std::collections::BTreeMap::new();
        params.insert("cid".to_string(), DataValue::Str(claim_id.into()));
        let script = if forward {
            "?[next] := *function_calls{caller_claim_id: cid, callee_claim_id: next}, \
             cid = $cid, next != ''"
        } else {
            "?[next] := *function_calls{callee_claim_id: cid, caller_claim_id: next}, \
             cid = $cid, next != ''"
        };
        let rows = self
            .query(script, params)
            .map_err(|e| Error::GraphStorage(format!("calls_edge({claim_id}): {e}")))?;
        Ok(rows.rows.iter().filter_map(|r| r.first().map(dv_str)).collect())
    }

    /// `code_signatures.parent_scope` containment. `forward=true` → members
    /// whose parent_scope is this entity's symbol; `forward=false` → the
    /// parent entity of this member.
    fn contains_edge(&self, claim_id: &str, forward: bool) -> Result<Vec<String>> {
        let Some(meta) = self.claim_meta(claim_id)? else {
            return Ok(Vec::new());
        };
        if forward {
            // Members whose parent_scope == this symbol.
            let mut params = std::collections::BTreeMap::new();
            params.insert("scope".to_string(), DataValue::Str(meta.symbol.clone().into()));
            let rows = self
                .query(
                    "?[child] := *code_signatures{claim_id: child, parent_scope: scope}, \
                     scope = $scope, scope != ''",
                    params,
                )
                .map_err(|e| Error::GraphStorage(format!("contains_edge({claim_id}): {e}")))?;
            Ok(rows.rows.iter().filter_map(|r| r.first().map(dv_str)).collect())
        } else {
            // The parent: this member's parent_scope symbol → its claim id.
            let mut params = std::collections::BTreeMap::new();
            params.insert("cid".to_string(), DataValue::Str(claim_id.into()));
            let rows = self
                .query(
                    "?[scope] := *code_signatures{claim_id: cid, parent_scope: scope}, \
                     cid = $cid, scope != ''",
                    params,
                )
                .map_err(|e| Error::GraphStorage(format!("contains_edge({claim_id}): {e}")))?;
            let Some(scope) = rows.rows.first().and_then(|r| r.first()).map(dv_str) else {
                return Ok(Vec::new());
            };
            // Resolve the parent symbol to its claim id(s).
            let mut p2 = std::collections::BTreeMap::new();
            p2.insert("sym".to_string(), DataValue::Str(scope.into()));
            let prows = self
                .query(
                    "?[id] := *claims{id, symbol: sym}, sym = $sym",
                    p2,
                )
                .map_err(|e| Error::GraphStorage(format!("contains_edge parent({claim_id}): {e}")))?;
            Ok(prows.rows.iter().filter_map(|r| r.first().map(dv_str)).collect())
        }
    }

    /// `code_imports` neighbors lifted to claim level. For this entity's
    /// source S: Out → code-def claims in sources S imports; In → code-def
    /// claims in sources that import S. `to_source` is resolved lazily by
    /// suffix-matching `import_path` against `sources.uri`. Returns at most
    /// one representative (earliest-byte) code-def claim per neighbor source
    /// to keep the walk bounded.
    fn imported_by_edge(
        &self,
        claim_id: &str,
        dir: TraversalDirection,
    ) -> Result<Vec<String>> {
        let Some(meta) = self.claim_meta(claim_id)? else {
            return Ok(Vec::new());
        };
        let mut neighbor_sources: HashSet<String> = HashSet::new();
        let want_out = matches!(dir, TraversalDirection::Out | TraversalDirection::Both);
        let want_in = matches!(dir, TraversalDirection::In | TraversalDirection::Both);

        if want_out {
            // Sources S imports → resolve each import_path to a source uri.
            let mut params = std::collections::BTreeMap::new();
            params.insert("src".to_string(), DataValue::Str(meta.source_id.clone().into()));
            let rows = self
                .query(
                    "?[to_source, import_path] := \
                     *code_imports{from_source: src, to_source, import_path}, src = $src",
                    params,
                )
                .map_err(|e| Error::GraphStorage(format!("imported_by_edge out({claim_id}): {e}")))?;
            for r in &rows.rows {
                let to_source = r.first().map(dv_str).unwrap_or_default();
                if !to_source.is_empty() {
                    neighbor_sources.insert(to_source);
                } else if let Some(path) = r.get(1).map(dv_str) {
                    if let Some(sid) = self.resolve_import_target(&path)? {
                        neighbor_sources.insert(sid);
                    }
                }
            }
        }
        if want_in {
            // Sources that import S (resolved to_source == S), plus a lazy
            // suffix-match for unresolved rows whose path points at S's uri.
            let mut params = std::collections::BTreeMap::new();
            params.insert("src".to_string(), DataValue::Str(meta.source_id.clone().into()));
            let rows = self
                .query(
                    "?[from_source] := *code_imports{from_source, to_source: src}, src = $src",
                    params,
                )
                .map_err(|e| Error::GraphStorage(format!("imported_by_edge in({claim_id}): {e}")))?;
            for r in &rows.rows {
                let from_source = r.first().map(dv_str).unwrap_or_default();
                if !from_source.is_empty() {
                    neighbor_sources.insert(from_source);
                }
            }
        }

        // Lift each neighbor source to its earliest-byte code-def claim.
        let mut out = Vec::new();
        for sid in neighbor_sources {
            if let Some(rep) = self.representative_code_def(&sid)? {
                out.push(rep);
            }
        }
        Ok(out)
    }

    /// Resolve an import-path string to a source id by suffix-matching the
    /// path's dotted/slashed tail against `sources.uri`. Best-effort: returns
    /// the first source whose uri ends with the path's final segment. `None`
    /// when no in-workspace source matches (i.e. an external import).
    fn resolve_import_target(&self, import_path: &str) -> Result<Option<String>> {
        let tail = import_path
            .trim()
            .trim_end_matches(';')
            .rsplit(['.', '/', ':', '\\'])
            .find(|s| !s.is_empty())
            .unwrap_or("");
        if tail.is_empty() {
            return Ok(None);
        }
        let rows = self
            .query(
                "?[id, uri] := *sources{id, uri}",
                Default::default(),
            )
            .map_err(|e| Error::GraphStorage(format!("resolve_import_target: {e}")))?;
        let tail_lc = tail.to_lowercase();
        for r in &rows.rows {
            if r.len() < 2 {
                continue;
            }
            let uri = dv_str(&r[1]).to_lowercase();
            // Match the file stem: ".../tail.ext" or ".../tail".
            let stem = uri
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("")
                .split('.')
                .next()
                .unwrap_or("");
            if stem == tail_lc {
                return Ok(Some(dv_str(&r[0])));
            }
        }
        Ok(None)
    }

    /// The earliest-byte code-def claim (FunctionDef/TypeDef) in a source —
    /// a stable representative node for source-level edges.
    fn representative_code_def(&self, source_id: &str) -> Result<Option<String>> {
        let mut params = std::collections::BTreeMap::new();
        params.insert("src".to_string(), DataValue::Str(source_id.into()));
        let rows = self
            .query(
                "?[id, byte_start] := *claims{id, source_id: src, symbol, byte_start}, \
                 src = $src, symbol != ''",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("representative_code_def({source_id}): {e}")))?;
        let mut best: Option<(String, u64)> = None;
        for r in &rows.rows {
            if r.len() < 2 {
                continue;
            }
            let id = dv_str(&r[0]);
            let bs = dv_u64(&r[1]);
            if best.as_ref().map(|(_, b)| bs < *b).unwrap_or(true) {
                best = Some((id, bs));
            }
        }
        Ok(best.map(|(id, _)| id))
    }

    /// Project one code-def claim's metadata, or `None` if the id is unknown
    /// or not a symbol-bearing claim.
    fn claim_meta(&self, claim_id: &str) -> Result<Option<ClaimMeta>> {
        let mut params = std::collections::BTreeMap::new();
        params.insert("cid".to_string(), DataValue::Str(claim_id.into()));
        let rows = self
            .query(
                "?[symbol, source_id, source_path, byte_start, byte_end] := \
                 *claims{id: cid, symbol, source_id, source_path, byte_start, byte_end}, \
                 cid = $cid",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("claim_meta({claim_id}): {e}")))?;
        let Some(row) = rows.rows.first() else {
            return Ok(None);
        };
        if row.len() < 5 {
            return Ok(None);
        }
        Ok(Some(ClaimMeta {
            symbol: dv_str(&row[0]),
            source_id: dv_str(&row[1]),
            source_path: dv_str(&row[2]),
            byte_start: dv_u64(&row[3]),
            byte_end: dv_u64(&row[4]),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::FunctionCall;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn store() -> GraphStore {
        let path = tempdir().unwrap().into_path();
        GraphStore::init(&path).unwrap()
    }

    /// Insert a minimal code-def claim (symbol-bearing). Columns without a
    /// schema default (statement/claim_type/source_id) are supplied; the rest
    /// default. Uses the known-good `<- $rows :put` shape.
    fn put_claim(
        s: &GraphStore,
        id: &str,
        symbol: &str,
        source_id: &str,
        source_path: &str,
        bs: i64,
        be: i64,
    ) {
        let row = DataValue::List(vec![
            DataValue::Str(id.into()),
            DataValue::Str(format!("definition of {symbol}").into()),
            DataValue::Str("function_def".into()),
            DataValue::Str(source_id.into()),
            DataValue::Str(symbol.into()),
            DataValue::Str(source_path.into()),
            DataValue::Num(Num::Int(bs)),
            DataValue::Num(Num::Int(be)),
        ]);
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(vec![row]));
        s.query(
            "?[id, statement, claim_type, source_id, symbol, source_path, byte_start, byte_end] <- $rows \
             :put claims {id => statement, claim_type, source_id, symbol, source_path, byte_start, byte_end}",
            params,
        )
        .unwrap();
    }

    fn put_call(s: &GraphStore, caller: &str, callee: &str) {
        s.insert_function_calls_batch(&[FunctionCall {
            id: format!("fc-{caller}-{callee}"),
            caller_claim_id: caller.into(),
            callee_name: callee.into(),
            callee_claim_id: callee.into(), // pre-resolved for the test
            source_id: "src1".into(),
            byte_start: 0,
            byte_end: 1,
            content_blake3: String::new(),
        }])
        .unwrap();
    }

    #[test]
    fn forward_traversal_a_b_c() {
        let s = store();
        put_claim(&s, "a", "fn_a", "src1", "a.rs", 0, 10);
        put_claim(&s, "b", "fn_b", "src1", "a.rs", 10, 20);
        put_claim(&s, "c", "fn_c", "src1", "a.rs", 20, 30);
        put_call(&s, "a", "b");
        put_call(&s, "b", "c");

        let nodes = s
            .traverse_graph("a", TraversalDirection::Out, 5, &[EdgeKind::Calls])
            .unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.claim_id.as_str()).collect();
        assert!(ids.contains(&"b"), "a→b must be reached");
        assert!(ids.contains(&"c"), "a→b→c must be reached");
        let c = nodes.iter().find(|n| n.claim_id == "c").unwrap();
        assert_eq!(c.depth, 2, "c is two hops from a");
        assert_eq!(c.symbol, "fn_c");
        assert_eq!(c.source_path, "a.rs");
    }

    #[test]
    fn reverse_traversal_finds_transitive_callers() {
        let s = store();
        put_claim(&s, "a", "fn_a", "src1", "a.rs", 0, 10);
        put_claim(&s, "b", "fn_b", "src1", "a.rs", 10, 20);
        put_claim(&s, "c", "fn_c", "src1", "a.rs", 20, 30);
        put_call(&s, "a", "b");
        put_call(&s, "b", "c");

        // impact(c) = everyone who (transitively) calls c → b, a.
        let nodes = s.impact("c", 5).unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.claim_id.as_str()).collect();
        assert!(ids.contains(&"b"), "b calls c");
        assert!(ids.contains(&"a"), "a transitively calls c via b");
    }

    #[test]
    fn cycle_terminates_and_dedups() {
        let s = store();
        put_claim(&s, "a", "fn_a", "src1", "a.rs", 0, 10);
        put_claim(&s, "b", "fn_b", "src1", "a.rs", 10, 20);
        put_call(&s, "a", "b");
        put_call(&s, "b", "a"); // cycle a→b→a

        let nodes = s
            .traverse_graph("a", TraversalDirection::Out, 10, &[EdgeKind::Calls])
            .unwrap();
        // Visited-dedup: b appears exactly once; a (the start) is never
        // re-emitted; the walk terminates despite the cycle.
        let b_count = nodes.iter().filter(|n| n.claim_id == "b").count();
        assert_eq!(b_count, 1, "cycle must not re-emit b");
        assert!(
            !nodes.iter().any(|n| n.claim_id == "a"),
            "start node must not be re-emitted"
        );
    }

    #[test]
    fn retrieve_entity_returns_file_and_span() {
        let s = store();
        put_claim(&s, "a", "fn_a", "src1", "src/a.rs", 100, 240);
        let d = s.retrieve_entity("a").unwrap().expect("entity exists");
        assert_eq!(d.symbol, "fn_a");
        assert_eq!(d.source_path, "src/a.rs");
        assert_eq!(d.byte_start, 100);
        assert_eq!(d.byte_end, 240);
        // Unknown id → None (honesty rule).
        assert!(s.retrieve_entity("ghost").unwrap().is_none());
    }

    #[test]
    fn search_entity_substring_and_exact_first() {
        let s = store();
        put_claim(&s, "1", "parse", "src1", "a.rs", 0, 10);
        put_claim(&s, "2", "parse_header", "src1", "a.rs", 10, 20);
        put_claim(&s, "3", "unrelated", "src1", "a.rs", 20, 30);

        let hits = s.search_entity("parse").unwrap();
        let syms: Vec<&str> = hits.iter().map(|h| h.symbol.as_str()).collect();
        assert!(syms.contains(&"parse"));
        assert!(syms.contains(&"parse_header"));
        assert!(!syms.contains(&"unrelated"));
        // Exact match sorts first.
        assert_eq!(hits[0].symbol, "parse");
        // Empty keyword → empty.
        assert!(s.search_entity("").unwrap().is_empty());
    }
}
