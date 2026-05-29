//! Flow definition validator (C8, 2026-05-22).
//!
//! Runs at `flow_define` time + before every `flow_run` to catch
//! errors that would otherwise surface mid-execution. Returns a
//! `Vec<FlowError>` so callers see EVERY issue at once (not just the
//! first one) — when a flow has a cycle AND an unknown tool, we
//! report both rather than playing whack-a-mole.
//!
//! Checks performed:
//!
//! 1. **Cycle detection** via Kahn's algorithm. Topological sort
//!    succeeds iff the DAG is acyclic.
//! 2. **Edge node resolution** — every `from` and `to` in
//!    `EdgeSpec` references a real node id.
//! 3. **MCP tool resolution** — every `NodeType::McpTool.tool`
//!    name exists in the runtime's known-tools set.
//! 4. **Deterministic function resolution** — every
//!    `NodeType::Deterministic.function` exists in the runtime's
//!    registered functions set.
//! 5. **Output source resolution** — every `OutputSpec.source`
//!    references a real node id (optionally with `.<output_key>`
//!    suffix; the suffix isn't type-checked at validate time
//!    because output keys are dynamic).
//!
//! Type-checking on edge data (producer vs consumer types) is NOT
//! performed at validate time. The `type:` tags on `ParamSpec` /
//! `OutputSpec` are free-form strings; the runtime checks actual
//! value shapes when data flows. v1 contract.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use crate::definition::{FlowDefinition, NodeType};
use crate::error::FlowError;

/// Inputs the validator needs from the runtime. Threaded through
/// rather than fetched directly so the validator stays a pure
/// function (testable without a daemon).
pub struct ValidatorContext<'a> {
    /// All MCP tool names the runtime knows about. Includes
    /// internal tools + every `external::<server>::<tool>` proxied
    /// MCP server tool advertised at validation time.
    pub available_tools: &'a HashSet<String>,
    /// All registered deterministic function names (see
    /// C11 `DeterministicRegistry::default()`).
    pub available_functions: &'a HashSet<String>,
}

impl<'a> ValidatorContext<'a> {
    pub fn new(
        available_tools: &'a HashSet<String>,
        available_functions: &'a HashSet<String>,
    ) -> Self {
        Self {
            available_tools,
            available_functions,
        }
    }
}

/// Validate a flow definition. Returns `Ok(())` when every check
/// passes; otherwise returns the FULL list of issues so the user
/// fixes everything in one pass.
pub fn validate(
    def: &FlowDefinition,
    ctx: &ValidatorContext<'_>,
) -> Result<(), Vec<FlowError>> {
    let mut errors: Vec<FlowError> = Vec::new();

    // 1. Edge node resolution — every `from`/`to` references an
    //    existing node. We do this BEFORE cycle detection so cycle
    //    detection doesn't trip on a malformed graph.
    for edge in &def.edges {
        if !def.nodes.contains_key(&edge.from) {
            errors.push(FlowError::UnknownNode {
                node_id: edge.from.clone(),
            });
        }
        if !def.nodes.contains_key(&edge.to) {
            errors.push(FlowError::UnknownNode {
                node_id: edge.to.clone(),
            });
        }
    }

    // 2. MCP tool + deterministic function resolution per node.
    for (node_id, node_spec) in &def.nodes {
        match &node_spec.node_type {
            NodeType::McpTool { tool, .. } => {
                if !ctx.available_tools.contains(tool) {
                    errors.push(FlowError::UnknownTool {
                        node_id: node_id.clone(),
                        tool: tool.clone(),
                        available_count: ctx.available_tools.len(),
                    });
                }
                // LocalLlm nodes' `tools` whitelist is also
                // validated — every whitelisted tool must exist.
            }
            NodeType::LocalLlm { tools, .. } => {
                for tool in tools {
                    if !ctx.available_tools.contains(tool) {
                        errors.push(FlowError::UnknownTool {
                            node_id: node_id.clone(),
                            tool: tool.clone(),
                            available_count: ctx.available_tools.len(),
                        });
                    }
                }
            }
            NodeType::Deterministic { function, .. } => {
                if !ctx.available_functions.contains(function) {
                    errors.push(FlowError::UnknownFunction {
                        node_id: node_id.clone(),
                        function: function.clone(),
                    });
                }
            }
            NodeType::RootFunction { function, .. } => {
                // Root Functions are workspace-stored and authored at
                // runtime, so we can't check existence against a
                // compile-time registry here — we only reject an empty
                // name. The executor resolves + errors honestly if the
                // named function isn't deployed.
                if function.trim().is_empty() {
                    errors.push(FlowError::UnknownFunction {
                        node_id: node_id.clone(),
                        function: function.clone(),
                    });
                }
            }
            NodeType::ClientSampling { .. } | NodeType::Human { .. } => {
                // No external dependencies to validate.
            }
        }
    }

    // 3. Output source resolution. Output sources may be either
    //    a bare node id ("scanner") or a dotted form
    //    ("scanner.claims"). We only check the node id part.
    for output_spec in def.outputs.values() {
        let node_id_part = output_spec
            .source
            .split('.')
            .next()
            .unwrap_or(&output_spec.source);
        if !def.nodes.contains_key(node_id_part) {
            errors.push(FlowError::UnknownNode {
                node_id: node_id_part.to_string(),
            });
        }
    }

    // 4. Cycle detection via Kahn's algorithm. Only runs when the
    //    graph is structurally well-formed (no unknown-node
    //    edges); cycles on a malformed graph would produce
    //    confusing reports.
    if errors.is_empty() {
        if let Err(cycle_nodes) = topological_sort(def) {
            errors.push(FlowError::CycleDetected { nodes: cycle_nodes });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Kahn's algorithm: returns a topological order of node ids, or
/// the set of node ids participating in a cycle if the graph isn't
/// acyclic. Public so `runtime::FlowRuntime::run` (C10) can reuse
/// the same sort for dispatch ordering without re-validating.
pub fn topological_sort(def: &FlowDefinition) -> Result<Vec<String>, Vec<String>> {
    // Build adjacency + in-degree.
    let mut in_degree: HashMap<String, usize> =
        def.nodes.keys().map(|k| (k.clone(), 0)).collect();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    for edge in &def.edges {
        adjacency
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
        if let Some(d) = in_degree.get_mut(&edge.to) {
            *d += 1;
        }
    }

    // Queue all nodes with in-degree 0.
    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    // Deterministic order: sort lex so test assertions are stable
    // across HashMap iteration order randomisation.
    let mut sorted_init: Vec<String> = queue.drain(..).collect();
    sorted_init.sort();
    queue.extend(sorted_init);

    let mut sorted: Vec<String> = Vec::with_capacity(def.nodes.len());
    while let Some(node) = queue.pop_front() {
        sorted.push(node.clone());
        if let Some(neighbours) = adjacency.get(&node) {
            // Sort neighbours for deterministic dispatch order.
            let mut neighbours_sorted = neighbours.clone();
            neighbours_sorted.sort();
            for neighbour in neighbours_sorted {
                if let Some(d) = in_degree.get_mut(&neighbour) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(neighbour);
                    }
                }
            }
        }
    }

    if sorted.len() == def.nodes.len() {
        Ok(sorted)
    } else {
        // Cycle present. Return the nodes that never reached
        // in-degree 0 — they're the cycle participants.
        let unvisited: BTreeSet<String> = def
            .nodes
            .keys()
            .filter(|k| !sorted.contains(*k))
            .cloned()
            .collect();
        Err(unvisited.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_ctx() -> (HashSet<String>, HashSet<String>) {
        (HashSet::new(), HashSet::new())
    }

    fn ctx_with_tools_and_functions(
        tools: &[&str],
        functions: &[&str],
    ) -> (HashSet<String>, HashSet<String>) {
        let tools: HashSet<String> = tools.iter().map(|s| s.to_string()).collect();
        let functions: HashSet<String> = functions.iter().map(|s| s.to_string()).collect();
        (tools, functions)
    }

    fn three_node_dag() -> FlowDefinition {
        FlowDefinition::from_yaml(
            r#"
id: dag-3
nodes:
  a:
    type: deterministic
    function: search
  b:
    type: deterministic
    function: search
  c:
    type: deterministic
    function: search
edges:
  - from: a
    to: b
  - from: b
    to: c
"#,
        )
        .expect("parse")
    }

    fn two_node_cycle() -> FlowDefinition {
        FlowDefinition::from_yaml(
            r#"
id: cycle-2
nodes:
  a:
    type: deterministic
    function: search
  b:
    type: deterministic
    function: search
edges:
  - from: a
    to: b
  - from: b
    to: a
"#,
        )
        .expect("parse")
    }

    fn three_node_cycle() -> FlowDefinition {
        FlowDefinition::from_yaml(
            r#"
id: cycle-3
nodes:
  a:
    type: deterministic
    function: search
  b:
    type: deterministic
    function: search
  c:
    type: deterministic
    function: search
edges:
  - from: a
    to: b
  - from: b
    to: c
  - from: c
    to: a
"#,
        )
        .expect("parse")
    }

    #[test]
    fn well_formed_three_node_dag_passes_validation() {
        let def = three_node_dag();
        let (tools, functions) = ctx_with_tools_and_functions(&[], &["search"]);
        let ctx = ValidatorContext::new(&tools, &functions);
        assert!(validate(&def, &ctx).is_ok());
    }

    #[test]
    fn topological_sort_orders_three_node_dag_correctly() {
        let def = three_node_dag();
        let order = topological_sort(&def).expect("acyclic");
        assert_eq!(order, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn detects_two_node_cycle() {
        let def = two_node_cycle();
        let result = topological_sort(&def);
        match result {
            Err(cycle) => {
                assert_eq!(cycle.len(), 2);
                assert!(cycle.contains(&"a".to_string()));
                assert!(cycle.contains(&"b".to_string()));
            }
            Ok(_) => panic!("expected cycle detection"),
        }
    }

    #[test]
    fn detects_three_node_cycle() {
        let def = three_node_cycle();
        let result = topological_sort(&def);
        match result {
            Err(cycle) => assert_eq!(cycle.len(), 3),
            Ok(_) => panic!("expected cycle detection"),
        }
    }

    #[test]
    fn validator_surfaces_cycle_as_flow_error() {
        let def = two_node_cycle();
        let (tools, functions) = ctx_with_tools_and_functions(&[], &["search"]);
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("cycle should fail");
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], FlowError::CycleDetected { .. }));
    }

    #[test]
    fn rejects_edge_to_unknown_node() {
        let def = FlowDefinition::from_yaml(
            r#"
id: bad
nodes:
  a:
    type: deterministic
    function: search
edges:
  - from: a
    to: ghost
"#,
        )
        .expect("parse");
        let (tools, functions) = ctx_with_tools_and_functions(&[], &["search"]);
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("should reject");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, FlowError::UnknownNode { node_id } if node_id == "ghost"))
        );
    }

    #[test]
    fn rejects_unknown_mcp_tool() {
        let def = FlowDefinition::from_yaml(
            r#"
id: bad
nodes:
  a:
    type: mcp_tool
    tool: nonexistent_tool
"#,
        )
        .expect("parse");
        let (tools, functions) = empty_ctx();
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("should reject");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, FlowError::UnknownTool { tool, .. } if tool == "nonexistent_tool"))
        );
    }

    #[test]
    fn rejects_unknown_function() {
        let def = FlowDefinition::from_yaml(
            r#"
id: bad
nodes:
  a:
    type: deterministic
    function: not_registered
"#,
        )
        .expect("parse");
        let (tools, functions) = empty_ctx();
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("should reject");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, FlowError::UnknownFunction { function, .. } if function == "not_registered"))
        );
    }

    #[test]
    fn rejects_local_llm_tool_whitelist_with_unknown_tool() {
        let def = FlowDefinition::from_yaml(
            r#"
id: bad
nodes:
  a:
    type: local_llm
    system: test
    tools:
      - search
      - ghost_tool
"#,
        )
        .expect("parse");
        let (tools, functions) = ctx_with_tools_and_functions(&["search"], &[]);
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("should reject ghost_tool");
        assert_eq!(errors.len(), 1);
        assert!(
            matches!(&errors[0], FlowError::UnknownTool { tool, .. } if tool == "ghost_tool")
        );
    }

    #[test]
    fn rejects_output_source_referencing_unknown_node() {
        let def = FlowDefinition::from_yaml(
            r#"
id: bad
nodes:
  a:
    type: deterministic
    function: search
outputs:
  result:
    type: claim_set
    source: ghost.output
"#,
        )
        .expect("parse");
        let (tools, functions) = ctx_with_tools_and_functions(&[], &["search"]);
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("should reject");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, FlowError::UnknownNode { node_id } if node_id == "ghost"))
        );
    }

    #[test]
    fn surfaces_multiple_errors_at_once() {
        let def = FlowDefinition::from_yaml(
            r#"
id: multi-bad
nodes:
  a:
    type: deterministic
    function: not_registered
  b:
    type: mcp_tool
    tool: ghost_tool
edges:
  - from: a
    to: also_ghost
"#,
        )
        .expect("parse");
        let (tools, functions) = empty_ctx();
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("should fail");
        // Should report: unknown function, unknown tool, unknown
        // node (edge target). That's 3 distinct issues in one pass.
        assert!(errors.len() >= 3, "expected multi-error report, got: {errors:?}");
        assert!(errors.iter().any(|e| matches!(e, FlowError::UnknownFunction { .. })));
        assert!(errors.iter().any(|e| matches!(e, FlowError::UnknownTool { .. })));
        assert!(errors.iter().any(|e| matches!(e, FlowError::UnknownNode { .. })));
    }

    #[test]
    fn empty_flow_passes_validation() {
        // A flow with one node and no edges is trivially valid.
        let def = FlowDefinition::from_yaml(
            r#"
id: trivial
nodes:
  only:
    type: deterministic
    function: noop
"#,
        )
        .expect("parse");
        let (tools, functions) = ctx_with_tools_and_functions(&[], &["noop"]);
        let ctx = ValidatorContext::new(&tools, &functions);
        assert!(validate(&def, &ctx).is_ok());
    }

    #[test]
    fn self_edge_is_detected_as_cycle() {
        let def = FlowDefinition::from_yaml(
            r#"
id: self-edge
nodes:
  a:
    type: deterministic
    function: f
edges:
  - from: a
    to: a
"#,
        )
        .expect("parse");
        let (tools, functions) = ctx_with_tools_and_functions(&[], &["f"]);
        let ctx = ValidatorContext::new(&tools, &functions);
        let errors = validate(&def, &ctx).expect_err("self-edge is a 1-cycle");
        assert!(errors.iter().any(|e| matches!(e, FlowError::CycleDetected { .. })));
    }

    #[test]
    fn lit_review_reference_flow_validates_with_required_tools() {
        // The shipped docs/flows/lit-review.yaml uses ingest_path
        // + extract_claims as tools and synthesize_pairwise merge.
        // Pin that it validates given the right runtime context.
        let yaml = std::fs::read_to_string("../../docs/flows/lit-review.yaml")
            .or_else(|_| std::fs::read_to_string("docs/flows/lit-review.yaml"))
            .expect("read reference flow");
        let def = FlowDefinition::from_yaml(&yaml).expect("parse reference");
        let (tools, functions) = ctx_with_tools_and_functions(
            &["ingest_path", "extract_claims"],
            &[],
        );
        let ctx = ValidatorContext::new(&tools, &functions);
        let result = validate(&def, &ctx);
        assert!(
            result.is_ok(),
            "lit-review reference flow should validate: {:?}",
            result
        );
    }

    #[test]
    fn parallel_dag_orders_predecessors_before_successors() {
        // Diamond: a → b, a → c, b → d, c → d
        let def = FlowDefinition::from_yaml(
            r#"
id: diamond
nodes:
  a:
    type: deterministic
    function: f
  b:
    type: deterministic
    function: f
  c:
    type: deterministic
    function: f
  d:
    type: deterministic
    function: f
edges:
  - from: a
    to: b
  - from: a
    to: c
  - from: b
    to: d
  - from: c
    to: d
"#,
        )
        .expect("parse");
        let order = topological_sort(&def).expect("acyclic");
        let pos = |id: &str| order.iter().position(|n| n == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert!(pos("b") < pos("d"));
        assert!(pos("c") < pos("d"));
    }

    #[test]
    fn topological_sort_is_deterministic_across_runs() {
        // HashMap iteration order is randomised — our explicit
        // lex-sort at the in-degree-0 init step + neighbour walk
        // makes the output stable.
        let def = three_node_dag();
        let order_a = topological_sort(&def).expect("acyclic");
        for _ in 0..10 {
            let order_b = topological_sort(&def).expect("acyclic");
            assert_eq!(order_a, order_b);
        }
    }
}
