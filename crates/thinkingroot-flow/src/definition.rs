//! Flow definition schema — the declarative wire shape both the
//! YAML and TOML loaders deserialise into. Frozen for production
//! per the user-confirmed design (plan §"Locked-in design
//! decisions"). Any change here is a wire-format break for
//! workspace-stored flow definitions; bump `FlowDefinition.version`
//! when extending.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};

/// Top-level flow definition. Stored as a Witness in the substrate
/// (`flow::definition@v1` rule per C9); also user-editable as a
/// YAML or TOML file under `<workspace_root>/.thinkingroot/flows/`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowDefinition {
    /// Human-friendly identifier, e.g. `"lit-review-v1"`. Forms
    /// part of the flow run's branch name and the Witness id.
    pub id: String,

    /// Schema version. Bump when adding required fields or
    /// changing variant semantics. Old definitions remain
    /// deserialisable when this stays 1 and we only add fields
    /// with `#[serde(default)]`.
    #[serde(default = "default_version")]
    pub version: u32,

    /// One-line description for tooltips + `flow list`.
    #[serde(default)]
    pub description: String,

    /// Input parameter schema declared at the flow level. Validated
    /// by the runtime at `flow_run` time against the caller's
    /// supplied `inputs` object.
    #[serde(default)]
    pub inputs: BTreeMap<String, ParamSpec>,

    /// Named outputs the flow promises to produce. The validator
    /// checks every `OutputSpec.source` resolves to a real node
    /// id.
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputSpec>,

    /// DAG nodes keyed by id. BTreeMap (not HashMap) so YAML/TOML
    /// round-trips are deterministic — important for content-
    /// addressed Witness storage in C9.
    pub nodes: BTreeMap<String, NodeSpec>,

    /// DAG edges declaring data + ordering dependencies.
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,

    /// Optional final merge — what happens to the leaf-node
    /// branches at flow completion. None ⇒ each leaf branch
    /// remains in its `MergePolicy` state for the user to handle.
    #[serde(default)]
    pub final_merge: Option<FinalMergeSpec>,

    /// Overall flow timeout in seconds. None ⇒ no timeout.
    /// Hit-on-timeout triggers FlowError::Cancelled.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Per-node retry cap. Each node attempt that returns a
    /// `is_retryable()` error gets retried up to this many times
    /// before failing the run. Default 1 (one retry total).
    #[serde(default = "default_max_node_retries")]
    pub max_node_retries: u32,
}

fn default_version() -> u32 {
    1
}

fn default_max_node_retries() -> u32 {
    1
}

/// A single input parameter on a flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParamSpec {
    /// Type tag — kept as a free-form string so YAML stays
    /// concise (`type: array<string>` etc.). The validator
    /// type-checks against the producer node's output type.
    #[serde(rename = "type")]
    pub type_tag: String,

    #[serde(default)]
    pub description: String,

    /// Whether the caller MUST supply this input. Default true.
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

/// A named output emitted by the flow at completion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputSpec {
    /// Type tag — same conventions as `ParamSpec.type_tag`.
    #[serde(rename = "type")]
    pub type_tag: String,

    /// Reference to the producer: either a node id (the node's
    /// primary output) or `"<node_id>.<output_key>"` for nodes
    /// that emit a named output.
    pub source: String,

    #[serde(default)]
    pub description: String,
}

/// One DAG node — a unit of executable work + how it relates to
/// the rest of the graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeSpec {
    /// The executor type + its configuration.
    #[serde(flatten)]
    pub node_type: NodeType,

    /// Branching behaviour relative to the parent run's branch.
    /// Default: `Inherit` (node writes to the parent run's branch).
    #[serde(default)]
    pub branch_strategy: BranchStrategy,

    /// How outputs of multiple predecessor branches get combined
    /// when entering THIS node. Default: `None` (node sees only
    /// the direct producer's output as-is).
    #[serde(default)]
    pub merge_strategy: MergeStrategy,

    /// Skip the runtime's automatic approval gate for this node.
    /// Honoured only by executors that consult an approval gate
    /// (the `local_llm` executor; the `human` executor IS the gate
    /// and ignores this flag).
    #[serde(default)]
    pub no_approval: bool,
}

/// Five executor types, mixable in the same DAG. Discriminated by
/// the `type:` field in YAML/TOML per serde's standard tag
/// pattern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeType {
    /// In-process LLM call using the SOTA agent loop (17-block
    /// reminder bus, retry, continuation, skill auto-surface).
    /// Reuses the in-app agent infrastructure verbatim.
    LocalLlm {
        /// System prompt steering the LLM's role + scope.
        system: String,
        /// Whitelist of MCP tool names this node may call. Empty
        /// ⇒ no tools (text-only generation).
        #[serde(default)]
        tools: Vec<String>,
        /// Iteration cap for this node's agent run. Default 10 —
        /// flow nodes should be focused, not open-ended.
        #[serde(default = "default_max_iterations")]
        max_iterations: u32,
    },

    /// Call any tool from `tools/list` — our internal tools OR
    /// proxied external MCP servers (`external::<server>::<tool>`).
    /// The `input_mapping` reshapes upstream outputs into the
    /// tool's expected argument schema.
    McpTool {
        /// Fully-qualified tool name (e.g. `"search"` or
        /// `"external::github::create_pr"`).
        tool: String,
        /// Maps tool argument names → upstream value expressions
        /// (typically `"$inputs.<key>"` or `"$nodes.<id>.<key>"`).
        #[serde(default)]
        input_mapping: BTreeMap<String, String>,
    },

    /// Back-call the connected MCP client's LLM via
    /// `sampling/createMessage` (C13). The flow runs on the
    /// daemon; the LLM call runs on the user's Claude Desktop /
    /// Claude Code subscription — zero of our API tokens.
    ClientSampling {
        /// Messages to send to the client's LLM. Templated via
        /// `{{var}}` substitution from inputs + upstream nodes.
        messages: Vec<SamplingMessage>,
        /// Model preference hints per MCP spec — substrings the
        /// client may match against its available models. Empty
        /// ⇒ neutral preferences (cost=0.5, speed=0.5,
        /// intelligence=0.5) per locked-in design decision.
        #[serde(default)]
        model_hints: Vec<String>,
        /// Required by MCP spec — max tokens the client should
        /// allow the LLM to generate.
        max_tokens: u32,
    },

    /// Call a registered Rust function in the deterministic
    /// executor's table — fast, cheap, zero LLM cost. Function
    /// names are validated against the runtime's registry at
    /// validate-time (C8).
    Deterministic {
        function: String,
        #[serde(default)]
        input_mapping: BTreeMap<String, String>,
    },

    /// Pause for human approval. Routes through `ApprovalGate`
    /// (C16). The `prompt_template` is rendered with the same
    /// templating as `ClientSampling.messages` and shown to the
    /// user in the desktop modal / CLI prompt / IDE notification.
    Human {
        prompt_template: String,
    },
}

fn default_max_iterations() -> u32 {
    10
}

/// One message in a `ClientSampling` node's prompt. Mirrors the
/// MCP spec's sampling message shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SamplingMessage {
    /// `"user"` or `"assistant"` per MCP spec.
    pub role: String,
    /// Plain text content. Templated via `{{var}}` substitution.
    pub content: String,
}

/// How a node creates its working branch relative to the parent
/// run's branch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchStrategy {
    /// Node writes to the parent run's branch directly. Default.
    #[default]
    Inherit,
    /// For each element of `input_key`'s array value, create a
    /// separate sandbox branch and run this node in parallel
    /// across them. The post-merge step's `merge_strategy`
    /// dictates how the parallel outputs combine.
    FanOutPerInput { input_key: String },
    /// Always create a fresh `BranchKind::Sandbox` for this node.
    NewSandbox,
}

/// How a node combines the outputs of its predecessor branches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MergeStrategy {
    /// No merge. Node sees its direct predecessor's output
    /// untouched. Default.
    #[default]
    None,
    /// Chain `synthesize_merge` across all predecessor branches
    /// pairwise (((A+B)+(C+D))+E for 5 inputs). The 2-way
    /// `synthesize_merge` engine method is the underlying
    /// primitive (`engine.rs:1556`); chaining is the v1 N-way
    /// contract per plan.
    SynthesizePairwise,
    /// Take the first predecessor branch that completes
    /// successfully; cancel the rest.
    FirstWins,
    /// Wait for ALL predecessor branches to complete (success or
    /// fail), then proceed without merging — node sees them as a
    /// keyed map of `{branch_name: output}`.
    Barrier,
}

/// What happens to the leaf-node branches at flow run completion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FinalMergeSpec {
    /// Merge policy enforced at the final merge. Same string
    /// values as `thinkingroot_core::types::branch::MergePolicy`
    /// (kept as String here so this crate doesn't import the
    /// branch types).
    pub policy: String,
    /// Target branch for the final merge. Default: `"main"`.
    #[serde(default = "default_main")]
    pub target: String,
}

fn default_main() -> String {
    "main".to_string()
}

/// One DAG edge — declares an ordering + optional data dependency
/// from one node to another.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeSpec {
    /// Producer node id.
    pub from: String,
    /// Consumer node id.
    pub to: String,
    /// Optional output key — references a named output on the
    /// producer node (used when the producer emits a structured
    /// outputs map). Default: the producer's primary output.
    #[serde(default)]
    pub output: Option<String>,
}

impl FlowDefinition {
    /// Load a flow definition from a YAML (`.yaml` / `.yml`) or
    /// TOML (`.toml`) file. Both formats produce the same
    /// `FlowDefinition` struct — `serde::Deserialize` is the
    /// single source of truth.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|e| FlowError::DefinitionIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::from_str_with_extension(&contents, path)
    }

    /// Parse a flow definition from a string, using the file
    /// extension at `path` to pick the parser. `path` is also
    /// used in error messages for diagnosability.
    pub fn from_str_with_extension(contents: &str, path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "yaml" | "yml" => Self::from_yaml(contents).map_err(|e| FlowError::DefinitionParse {
                path: Some(path.to_path_buf()),
                message: e.to_string(),
            }),
            "toml" => Self::from_toml(contents).map_err(|e| FlowError::DefinitionParse {
                path: Some(path.to_path_buf()),
                message: e.to_string(),
            }),
            other => Err(FlowError::UnsupportedExtension {
                path: path.to_path_buf(),
                extension: other.to_string(),
            }),
        }
    }

    /// Parse a YAML flow definition. Public so tests + bench
    /// harnesses can build definitions without touching disk.
    pub fn from_yaml(s: &str) -> std::result::Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(s)
    }

    /// Parse a TOML flow definition.
    pub fn from_toml(s: &str) -> std::result::Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to YAML — used by `flow_define` MCP tool when
    /// returning the canonical form to callers + by the storage
    /// layer when writing to disk.
    pub fn to_yaml(&self) -> std::result::Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    /// Serialize to TOML.
    pub fn to_toml(&self) -> std::result::Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Stable content hash over the canonical TOML serialisation.
    /// Used by C9's Witness storage so the same definition always
    /// produces the same content_blake3 regardless of whether the
    /// caller supplied YAML or TOML originally.
    pub fn content_hash(&self) -> std::result::Result<String, toml::ser::Error> {
        let toml = self.to_toml()?;
        Ok(blake3::hash(toml.as_bytes()).to_hex().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_definition() -> &'static str {
        r#"
id: smoke-test-v1
description: Single-node smoke test
nodes:
  scanner:
    type: deterministic
    function: search
"#
    }

    fn three_node_lit_review() -> &'static str {
        r#"
id: lit-review-v1
version: 1
description: Three-node lit review (scanner + summarizer + reviewer)
inputs:
  papers:
    type: array<string>
    description: Paper file paths to review
outputs:
  summary_branch:
    type: branch_ref
    source: summarizer
nodes:
  scanner:
    type: local_llm
    system: Extract claims from each paper.
    tools:
      - ingest_path
    max_iterations: 10
    branch_strategy:
      kind: fan_out_per_input
      input_key: papers
  summarizer:
    type: client_sampling
    messages:
      - role: user
        content: Summarize the scanner output.
    model_hints: []
    max_tokens: 2000
    merge_strategy:
      kind: synthesize_pairwise
  reviewer:
    type: human
    prompt_template: 'Approve the summary?'
edges:
  - from: scanner
    to: summarizer
  - from: summarizer
    to: reviewer
final_merge:
  policy: requires_proposal
  target: main
timeout_secs: 300
max_node_retries: 2
"#
    }

    #[test]
    fn parse_minimal_yaml_definition() {
        let def = FlowDefinition::from_yaml(minimal_definition()).expect("parse");
        assert_eq!(def.id, "smoke-test-v1");
        assert_eq!(def.version, 1); // default
        assert_eq!(def.max_node_retries, 1); // default
        assert_eq!(def.nodes.len(), 1);
        assert!(matches!(
            def.nodes.get("scanner").unwrap().node_type,
            NodeType::Deterministic { .. }
        ));
    }

    #[test]
    fn parse_full_yaml_lit_review_definition() {
        let def = FlowDefinition::from_yaml(three_node_lit_review()).expect("parse");
        assert_eq!(def.id, "lit-review-v1");
        assert_eq!(def.nodes.len(), 3);
        assert_eq!(def.edges.len(), 2);
        assert_eq!(def.timeout_secs, Some(300));
        assert_eq!(def.max_node_retries, 2);

        // scanner is LocalLlm with FanOutPerInput.
        let scanner = def.nodes.get("scanner").expect("scanner present");
        match &scanner.node_type {
            NodeType::LocalLlm {
                system,
                tools,
                max_iterations,
            } => {
                assert!(system.contains("Extract"));
                assert_eq!(tools, &vec!["ingest_path".to_string()]);
                assert_eq!(*max_iterations, 10);
            }
            other => panic!("scanner should be LocalLlm, got {other:?}"),
        }
        assert!(matches!(
            scanner.branch_strategy,
            BranchStrategy::FanOutPerInput { ref input_key } if input_key == "papers"
        ));

        // summarizer is ClientSampling with SynthesizePairwise merge.
        let summarizer = def.nodes.get("summarizer").expect("summarizer present");
        match &summarizer.node_type {
            NodeType::ClientSampling { max_tokens, .. } => assert_eq!(*max_tokens, 2000),
            other => panic!("summarizer should be ClientSampling, got {other:?}"),
        }
        assert!(matches!(
            summarizer.merge_strategy,
            MergeStrategy::SynthesizePairwise
        ));

        // reviewer is Human.
        let reviewer = def.nodes.get("reviewer").expect("reviewer present");
        assert!(matches!(reviewer.node_type, NodeType::Human { .. }));

        // Final merge config.
        let fm = def.final_merge.expect("final_merge present");
        assert_eq!(fm.policy, "requires_proposal");
        assert_eq!(fm.target, "main");
    }

    #[test]
    fn yaml_and_toml_round_trip_to_identical_struct() {
        let def_a = FlowDefinition::from_yaml(three_node_lit_review()).expect("parse YAML");
        let toml = def_a.to_toml().expect("serialize to TOML");
        let def_b = FlowDefinition::from_toml(&toml).expect("parse TOML");
        assert_eq!(def_a, def_b);
    }

    #[test]
    fn from_path_handles_unsupported_extension() {
        let result = FlowDefinition::from_str_with_extension(
            "id: x\nnodes:\n  a:\n    type: deterministic\n    function: f\n",
            std::path::Path::new("/tmp/flow.json"),
        );
        assert!(matches!(
            result,
            Err(FlowError::UnsupportedExtension { ref extension, .. })
                if extension == "json"
        ));
    }

    #[test]
    fn from_path_dispatches_yaml_extension() {
        let result = FlowDefinition::from_str_with_extension(
            minimal_definition(),
            std::path::Path::new("/tmp/flow.yml"),
        );
        assert!(result.is_ok(), "yml extension should dispatch YAML parser");
    }

    #[test]
    fn from_path_dispatches_toml_extension() {
        // Convert minimal YAML to TOML first.
        let def = FlowDefinition::from_yaml(minimal_definition()).expect("parse YAML");
        let toml = def.to_toml().expect("emit TOML");
        let result = FlowDefinition::from_str_with_extension(
            &toml,
            std::path::Path::new("/tmp/flow.toml"),
        );
        assert!(result.is_ok(), "toml extension should dispatch TOML parser");
    }

    #[test]
    fn content_hash_is_deterministic_across_calls() {
        let def = FlowDefinition::from_yaml(three_node_lit_review()).expect("parse");
        let h1 = def.content_hash().expect("hash 1");
        let h2 = def.content_hash().expect("hash 2");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // BLAKE3 hex is 64 chars
    }

    #[test]
    fn content_hash_changes_when_definition_changes() {
        let mut def = FlowDefinition::from_yaml(three_node_lit_review()).expect("parse");
        let h1 = def.content_hash().expect("hash 1");
        def.description = "different description".to_string();
        let h2 = def.content_hash().expect("hash 2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn flow_error_is_definition_error_classifies_correctly() {
        let unknown = FlowError::UnknownNode {
            node_id: "x".to_string(),
        };
        assert!(unknown.is_definition_error());
        assert!(!unknown.is_retryable());

        let node_failed = FlowError::NodeFailed {
            node_id: "n".to_string(),
            message: "oh no".to_string(),
        };
        assert!(!node_failed.is_definition_error());
        assert!(node_failed.is_retryable());

        let storage = FlowError::Storage("cozo write failed".to_string());
        assert!(!storage.is_definition_error());
        assert!(storage.is_retryable());
    }

    #[test]
    fn default_node_strategies_are_inherit_and_none() {
        let def = FlowDefinition::from_yaml(minimal_definition()).expect("parse");
        let scanner = def.nodes.get("scanner").unwrap();
        assert!(matches!(scanner.branch_strategy, BranchStrategy::Inherit));
        assert!(matches!(scanner.merge_strategy, MergeStrategy::None));
        assert!(!scanner.no_approval);
    }

    #[test]
    fn default_max_iterations_for_local_llm_is_ten() {
        let yaml = r#"
id: x
nodes:
  a:
    type: local_llm
    system: "test"
"#;
        let def = FlowDefinition::from_yaml(yaml).expect("parse");
        let a = def.nodes.get("a").unwrap();
        match &a.node_type {
            NodeType::LocalLlm { max_iterations, .. } => assert_eq!(*max_iterations, 10),
            other => panic!("expected LocalLlm, got {other:?}"),
        }
    }

    #[test]
    fn final_merge_target_defaults_to_main() {
        let yaml = r#"
id: x
nodes:
  a:
    type: deterministic
    function: f
final_merge:
  policy: requires_proposal
"#;
        let def = FlowDefinition::from_yaml(yaml).expect("parse");
        let fm = def.final_merge.expect("final_merge");
        assert_eq!(fm.target, "main");
    }
}
