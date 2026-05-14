//! Mermaid concept-map emitter for the Architecture section.
//!
//! Why Mermaid: every major markdown viewer (GitHub, GitLab, VS Code,
//! Notion, Obsidian, ReactMarkdown via remark-mermaid) renders
//! Mermaid blocks natively without a server-side render step. The
//! body bytes are still plain markdown, so the paper is portable.
//!
//! Why deterministic: we sort node labels and edges in stable order
//! so the same witness set always produces byte-identical Mermaid
//! source. This is what lets section-level BLAKE3 caching (v1.1)
//! short-circuit unchanged architectures.

use std::collections::BTreeMap;

/// A node in the concept map. `id` is the Mermaid node id (must be
/// alphanumeric + underscores); `label` is what the renderer shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConceptNode {
    /// Mermaid-safe id (kebab → underscore conversion done at emit
    /// time so the label is preserved verbatim for the human).
    pub id: String,
    /// Human-readable label shown inside the node.
    pub label: String,
    /// In-edge count — used to size and rank the node. Higher
    /// counts surface as the "most-connected" concepts.
    pub in_degree: u32,
}

/// A directed edge between two concept nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConceptEdge {
    /// Source node id.
    pub from: String,
    /// Target node id.
    pub to: String,
    /// Optional verb label rendered on the arrow (e.g. `"calls"`,
    /// `"documents"`). Empty for a bare arrow.
    pub label: String,
}

/// Render a `graph LR` Mermaid block from the supplied nodes + edges.
///
/// Output is deterministic: nodes sorted by id ascending, edges
/// sorted by `(from, to, label)`. Caller has already truncated to a
/// readable top-N — we don't truncate here.
///
/// Empty input produces a single placeholder node so the Mermaid
/// block is always valid (Mermaid rejects empty `graph LR` bodies
/// with a parse error that would break GitHub's renderer).
pub fn render_graph_lr(nodes: &[ConceptNode], edges: &[ConceptEdge]) -> String {
    if nodes.is_empty() {
        return "```mermaid\ngraph LR\n  empty[\"no witnesses yet\"]\n```\n".to_string();
    }

    let mut node_map: BTreeMap<&str, &ConceptNode> = BTreeMap::new();
    for n in nodes {
        node_map.insert(n.id.as_str(), n);
    }

    let mut out = String::from("```mermaid\ngraph LR\n");
    for (id, node) in &node_map {
        // Escape `"` in labels so the Mermaid parser doesn't choke.
        let label_escaped = node.label.replace('"', "\\\"");
        // Sanitise id: Mermaid requires `[A-Za-z_][A-Za-z0-9_]*` for
        // bare node ids. Kebab `-` becomes `_`.
        let safe_id = sanitise_id(id);
        out.push_str(&format!("  {safe_id}[\"{label_escaped}\"]\n"));
    }
    let mut sorted_edges = edges.to_vec();
    sorted_edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.label.cmp(&b.label))
    });
    for edge in &sorted_edges {
        let from_safe = sanitise_id(&edge.from);
        let to_safe = sanitise_id(&edge.to);
        if edge.label.is_empty() {
            out.push_str(&format!("  {from_safe} --> {to_safe}\n"));
        } else {
            let label_escaped = edge.label.replace('"', "\\\"");
            out.push_str(&format!(
                "  {from_safe} -- \"{label_escaped}\" --> {to_safe}\n"
            ));
        }
    }
    out.push_str("```\n");
    out
}

/// Sanitise a kebab or arbitrary string into a Mermaid-safe id.
fn sanitise_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else if i == 0 && ch.is_ascii_digit() {
            // Mermaid ids can't start with a digit — prefix.
            out.push('n');
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: &str, label: &str, deg: u32) -> ConceptNode {
        ConceptNode {
            id: id.into(),
            label: label.into(),
            in_degree: deg,
        }
    }
    fn e(from: &str, to: &str, label: &str) -> ConceptEdge {
        ConceptEdge {
            from: from.into(),
            to: to.into(),
            label: label.into(),
        }
    }

    #[test]
    fn empty_input_emits_placeholder_node() {
        let out = render_graph_lr(&[], &[]);
        assert!(out.contains("empty[\"no witnesses yet\"]"));
        assert!(out.starts_with("```mermaid\ngraph LR\n"));
        assert!(out.ends_with("```\n"));
    }

    #[test]
    fn output_is_byte_deterministic_across_input_permutations() {
        let nodes_a = vec![n("alpha", "Alpha", 2), n("beta", "Beta", 1)];
        let nodes_b = vec![n("beta", "Beta", 1), n("alpha", "Alpha", 2)];
        let edges_a = vec![e("alpha", "beta", "depends-on")];
        let edges_b = edges_a.clone();
        let out_a = render_graph_lr(&nodes_a, &edges_a);
        let out_b = render_graph_lr(&nodes_b, &edges_b);
        assert_eq!(
            out_a, out_b,
            "render_graph_lr must produce byte-identical output regardless of input order"
        );
    }

    #[test]
    fn kebab_ids_get_sanitised_to_underscores() {
        let nodes = vec![n("declares-function", "Declares Function", 1)];
        let out = render_graph_lr(&nodes, &[]);
        assert!(
            out.contains("declares_function["),
            "expected sanitised id, got: {out}"
        );
        assert!(out.contains("Declares Function"));
    }

    #[test]
    fn quotes_in_labels_are_escaped() {
        let nodes = vec![n("x", "Say \"hi\"", 1)];
        let out = render_graph_lr(&nodes, &[]);
        // The body should carry the escaped \" not bare " — Mermaid
        // would otherwise parse the second `"` as a label terminator.
        assert!(out.contains("Say \\\"hi\\\""));
    }

    #[test]
    fn labelled_edge_uses_arrow_with_label() {
        let nodes = vec![n("a", "A", 1), n("b", "B", 1)];
        let edges = vec![e("a", "b", "calls")];
        let out = render_graph_lr(&nodes, &edges);
        assert!(out.contains("a -- \"calls\" --> b"));
    }

    #[test]
    fn unlabelled_edge_uses_bare_arrow() {
        let nodes = vec![n("a", "A", 1), n("b", "B", 1)];
        let edges = vec![e("a", "b", "")];
        let out = render_graph_lr(&nodes, &edges);
        assert!(out.contains("a --> b"));
        assert!(!out.contains("a -- "), "unlabelled edge must omit the `-- ...` segment");
    }
}
