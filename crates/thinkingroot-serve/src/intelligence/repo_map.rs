//! E3 — repo-map: a PageRank-ranked, token-budgeted file→symbol skeleton.
//!
//! Aider's insight: an LLM navigating an unfamiliar codebase wants a *map* —
//! the most central files and symbols — not the raw bytes. We build that map
//! from the compiled code graph: nodes are code-def claims (functions/types),
//! edges are resolved `function_calls` (+ import edges lifted to their sources'
//! representative symbols). PageRank ranks structural centrality; an optional
//! query personalizes the walk toward relevant symbols; `to_tree` renders the
//! top-ranked symbols into a compact, deterministic skeleton that fits a token
//! budget.
//!
//! CozoDB's graph-algo feature is OFF (Cargo.toml: minimal,rayon), so PageRank
//! is a plain Rust power-iteration — not a Cozo rule.
//!
//! Empty graph → empty map (honesty rule: never fabricate structure).

use std::collections::BTreeMap;

/// Request for `QueryEngine::repo_map`.
#[derive(Debug, Clone)]
pub struct RepoMapRequest {
    /// Approximate token budget for the rendered tree (chars/4 estimate).
    pub budget_tokens: usize,
    /// Optional query — seeds PageRank personalization toward matching
    /// symbols so the map is biased to the area of interest.
    pub query: Option<String>,
}

/// One ranked symbol in the map.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RankedSymbol {
    pub claim_id: String,
    pub symbol: String,
    pub source_path: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub rank: f32,
}

/// The rendered repo-map.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RepoMap {
    /// File→symbol skeleton, token-budgeted.
    pub tree: String,
    /// The symbols included in `tree`, rank-descending.
    pub symbols: Vec<RankedSymbol>,
    /// Total symbols in the graph (so the caller knows how much was elided).
    pub total_symbols: usize,
}

/// PageRank via power-iteration. `edges` are directed `(from, to)` index
/// pairs; out-of-range indices are ignored. `personalization`, when supplied
/// and non-zero, replaces the uniform teleport/dangling distribution (its
/// values need not be normalised — they are normalised internally). Standard
/// defaults: `damping = 0.85`. Returns one score per node (n entries).
pub fn pagerank(
    n: usize,
    edges: &[(usize, usize)],
    personalization: Option<&[f32]>,
    damping: f32,
    max_iters: usize,
    tol: f32,
) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(u, v) in edges {
        if u < n && v < n {
            out[u].push(v);
        }
    }
    // Teleport / dangling base distribution.
    let base: Vec<f32> = match personalization {
        Some(p) if p.len() == n && p.iter().any(|&x| x > 0.0) => {
            let sum: f32 = p.iter().map(|x| x.max(0.0)).sum();
            p.iter().map(|x| x.max(0.0) / sum).collect()
        }
        _ => vec![1.0 / n as f32; n],
    };

    let mut rank = vec![1.0 / n as f32; n];
    for _ in 0..max_iters.max(1) {
        // Teleport mass.
        let mut next: Vec<f32> = base.iter().map(|b| (1.0 - damping) * b).collect();
        // Dangling-node mass (no out-edges) redistributed via base.
        let dangling: f32 = (0..n).filter(|&i| out[i].is_empty()).map(|i| rank[i]).sum();
        for i in 0..n {
            next[i] += damping * dangling * base[i];
        }
        // Edge mass.
        for u in 0..n {
            if out[u].is_empty() {
                continue;
            }
            let share = damping * rank[u] / out[u].len() as f32;
            for &v in &out[u] {
                next[v] += share;
            }
        }
        let delta: f32 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if delta < tol {
            break;
        }
    }
    rank
}

/// Render the top-ranked symbols into a file→symbol skeleton that fits
/// `budget_tokens` (chars/4 estimate). Files are ordered by their best
/// symbol's rank; symbols within a file by rank. Deterministic tie-break by
/// (rank desc, claim_id). Returns `(tree_string, included_symbols)`.
pub fn to_tree(mut symbols: Vec<RankedSymbol>, budget_tokens: usize) -> (String, Vec<RankedSymbol>) {
    // Global rank order (deterministic).
    symbols.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.claim_id.cmp(&b.claim_id))
    });

    let budget_chars = budget_tokens.saturating_mul(4);
    // Greedily admit symbols in rank order until the char budget is hit, then
    // group the admitted set by file for rendering.
    let mut admitted: Vec<RankedSymbol> = Vec::new();
    let mut used_chars = 0usize;
    // Track file headers already counted so a file's header cost is paid once.
    let mut seen_files: BTreeMap<String, ()> = BTreeMap::new();
    for sym in symbols {
        let header_cost = if seen_files.contains_key(&sym.source_path) {
            0
        } else {
            sym.source_path.len() + 2 // "path:\n"
        };
        let line_cost = sym.symbol.len() + 3; // "  sym\n"
        if used_chars + header_cost + line_cost > budget_chars && !admitted.is_empty() {
            break;
        }
        used_chars += header_cost + line_cost;
        seen_files.insert(sym.source_path.clone(), ());
        admitted.push(sym);
    }

    // Group admitted symbols by file; order files by best rank within.
    let mut by_file: BTreeMap<String, Vec<RankedSymbol>> = BTreeMap::new();
    for sym in &admitted {
        by_file.entry(sym.source_path.clone()).or_default().push(sym.clone());
    }
    let mut files: Vec<(String, Vec<RankedSymbol>)> = by_file.into_iter().collect();
    files.sort_by(|a, b| {
        let a_best = a.1.iter().map(|s| s.rank).fold(0.0_f32, f32::max);
        let b_best = b.1.iter().map(|s| s.rank).fold(0.0_f32, f32::max);
        b_best
            .partial_cmp(&a_best)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut tree = String::new();
    for (path, mut syms) in files {
        syms.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.claim_id.cmp(&b.claim_id))
        });
        tree.push_str(&path);
        tree.push_str(":\n");
        for s in syms {
            tree.push_str("  ");
            tree.push_str(&s.symbol);
            tree.push('\n');
        }
    }
    (tree, admitted)
}

/// Build a repo-map directly from a `GraphStore` (no engine/workspace lookup).
/// Shared by `QueryEngine::repo_map` (REST/MCP) and `FnCapabilities::ws_repo_map`
/// (the Root Function `ctx.workspace` surface). Empty graph → empty map.
pub fn build_repo_map(
    graph: &thinkingroot_graph::graph::GraphStore,
    budget_tokens: usize,
    query: Option<&str>,
) -> thinkingroot_core::Result<RepoMap> {
    let entities = graph.list_code_entities()?;
    if entities.is_empty() {
        return Ok(RepoMap { tree: String::new(), symbols: Vec::new(), total_symbols: 0 });
    }
    let mut idx: BTreeMap<String, usize> = BTreeMap::new();
    for (i, e) in entities.iter().enumerate() {
        idx.insert(e.claim_id.clone(), i);
    }
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for call in graph.list_resolved_function_calls()? {
        if let (Some(&u), Some(&v)) =
            (idx.get(&call.caller_claim_id), idx.get(&call.callee_claim_id))
        {
            edges.push((u, v));
        }
    }
    let personalization: Option<Vec<f32>> = match query {
        Some(q) if !q.trim().is_empty() => {
            let hits = graph.search_entity(q)?;
            let mut p = vec![0.0_f32; entities.len()];
            for h in &hits {
                if let Some(&i) = idx.get(&h.claim_id) {
                    p[i] = 1.0;
                }
            }
            if p.iter().any(|&x| x > 0.0) { Some(p) } else { None }
        }
        _ => None,
    };
    let ranks = pagerank(entities.len(), &edges, personalization.as_deref(), 0.85, 30, 1e-6);
    let symbols: Vec<RankedSymbol> = entities
        .iter()
        .enumerate()
        .map(|(i, e)| RankedSymbol {
            claim_id: e.claim_id.clone(),
            symbol: e.symbol.clone(),
            source_path: e.source_path.clone(),
            byte_start: e.byte_start,
            byte_end: e.byte_end,
            rank: ranks.get(i).copied().unwrap_or(0.0),
        })
        .collect();
    let total_symbols = symbols.len();
    let (tree, included) = to_tree(symbols, budget_tokens);
    Ok(RepoMap { tree, symbols: included, total_symbols })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(id: &str, name: &str, file: &str, rank: f32) -> RankedSymbol {
        RankedSymbol {
            claim_id: id.into(),
            symbol: name.into(),
            source_path: file.into(),
            byte_start: 0,
            byte_end: 1,
            rank,
        }
    }

    #[test]
    fn pagerank_ranks_hub_highest() {
        // 0,1,2 all call 3 → 3 is the hub and must rank highest.
        let edges = vec![(0, 3), (1, 3), (2, 3)];
        let r = pagerank(4, &edges, None, 0.85, 30, 1e-6);
        let hub = r[3];
        assert!(
            hub > r[0] && hub > r[1] && hub > r[2],
            "hub {hub} must exceed leaves {:?}",
            &r[..3]
        );
    }

    #[test]
    fn pagerank_converges_before_max_iters() {
        // A simple ring converges quickly; ranks must sum to ~1.
        let edges = vec![(0, 1), (1, 2), (2, 0)];
        let r = pagerank(3, &edges, None, 0.85, 100, 1e-6);
        let total: f32 = r.iter().sum();
        assert!((total - 1.0).abs() < 1e-3, "ranks should sum to ~1, got {total}");
        // Symmetric ring → near-equal ranks.
        assert!((r[0] - r[1]).abs() < 1e-3 && (r[1] - r[2]).abs() < 1e-3);
    }

    #[test]
    fn personalization_biases_toward_seed() {
        // Two disconnected nodes; personalize node 1 → it must rank higher.
        let edges: Vec<(usize, usize)> = vec![];
        let pers = vec![0.0, 1.0];
        let r = pagerank(2, &edges, Some(&pers), 0.85, 30, 1e-6);
        assert!(r[1] > r[0], "personalized node must dominate: {r:?}");
    }

    #[test]
    fn pagerank_empty_graph_is_empty() {
        assert!(pagerank(0, &[], None, 0.85, 30, 1e-6).is_empty());
    }

    #[test]
    fn to_tree_respects_budget() {
        let symbols = vec![
            sym("a", "alpha", "a.rs", 0.9),
            sym("b", "beta", "a.rs", 0.8),
            sym("c", "gamma", "b.rs", 0.7),
            sym("d", "delta", "c.rs", 0.6),
        ];
        // Tiny budget → only the top file/symbol(s) fit.
        let (small, included_small) = to_tree(symbols.clone(), 3);
        let (full, included_full) = to_tree(symbols, 1000);
        assert!(included_small.len() < included_full.len(), "budget must elide");
        assert!(small.len() <= full.len());
        assert_eq!(included_full.len(), 4, "generous budget includes all");
        // Highest-ranked symbol always survives the budget.
        assert!(included_small.iter().any(|s| s.symbol == "alpha"));
        // Output is byte-budget bounded (chars/4 ≈ tokens, +1 file header slop).
        assert!(small.len() <= 3 * 4 + 8);
    }

    #[test]
    fn to_tree_groups_by_file_rank_order() {
        let symbols = vec![
            sym("a", "low", "z.rs", 0.1),
            sym("b", "high", "a.rs", 0.9),
        ];
        let (tree, _) = to_tree(symbols, 1000);
        // a.rs (rank .9) must render before z.rs (rank .1) despite name order.
        let a_pos = tree.find("a.rs").unwrap();
        let z_pos = tree.find("z.rs").unwrap();
        assert!(a_pos < z_pos, "higher-ranked file first:\n{tree}");
    }
}
