//! Operating-layer artifacts as graph nodes (M2).
//!
//! Prompts, Root Functions, flows, and MCP servers are first-class
//! *nodes* in the cognition graph, not just rows in their own stores.
//! This lets the capsule (M1) and the Console Brain Graph **retrieve,
//! route over, and visualise** the agent's operating layer the same way
//! they do facts.
//!
//! Design: the artifact **bodies stay in their own stores** (the source
//! of truth — `prompt_templates`, `root_functions`, on-disk flow YAML,
//! `mcp-servers.toml`); each write *syncs* a lightweight node + edges
//! here. Nodes reuse the existing `entities` relation (id
//! `"{kind}:{name}"`, `entity_type = kind`) and `entity_relations` for
//! edges (`prompt --uses--> function`, `function --calls--> mcp_server`,
//! `flow --node--> function`). On re-sync we replace this node's
//! outgoing artifact edges so a renamed reference can't leave a stale
//! edge behind.
//!
//! No synthetic witness is written: a witness must byte-anchor a source
//! span (`content_blake3 = BLAKE3(source[start..end])`, invariant
//! I-W8), which an artifact body is not. Versioning/provenance for
//! artifacts already lives in their own versioned stores.

use std::collections::BTreeMap;

use cozo::{DataValue, Num, ScriptMutability};
use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// Artifact node kinds. Stored verbatim in `entities.entity_type`.
pub const KIND_PROMPT: &str = "compiled_prompt";
pub const KIND_FUNCTION: &str = "root_function";
pub const KIND_FLOW: &str = "flow_def";
pub const KIND_MCP_SERVER: &str = "mcp_server";
pub const KIND_MCP_TOOL: &str = "mcp_tool";
/// A durable branch (topic/feature), synced as a node so the brain can
/// describe its own branch topology. Ephemeral `stream/*` branches are NOT
/// node-ified (high churn) — the engine filters them at the sync boundary.
pub const KIND_BRANCH: &str = "branch";

/// Branch lifecycle status carried in a branch node's encoded `description`.
pub const BRANCH_STATUS_ACTIVE: &str = "active";
pub const BRANCH_STATUS_MERGED: &str = "merged";
pub const BRANCH_STATUS_CLOSED: &str = "closed";

/// Typed view of a branch node decoded from its JSON-encoded `description`.
/// The branch **registry** stays the source of truth; this node is a synced
/// projection so the brain never claims a branch is `active` after it merged
/// (honesty rule). Status changes are applied by re-upserting the node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchNode {
    pub name: String,
    /// `active` | `merged` | `closed`.
    pub status: String,
    /// Parent branch this one forked from (`None` for roots / main).
    #[serde(default)]
    pub parent: Option<String>,
    /// Branch kind label (e.g. `topic`, `feature`) for display.
    #[serde(default)]
    pub kind: Option<String>,
    /// Fork time (epoch secs).
    #[serde(default)]
    pub created_at: f64,
    /// Merge time (epoch secs), set when status flips to `merged`.
    #[serde(default)]
    pub merged_at: Option<f64>,
}

/// A synced operating-layer node read back from the graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactNode {
    /// `"{kind}:{name}"`.
    pub id: String,
    pub name: String,
    /// One of the `KIND_*` constants.
    pub kind: String,
    pub description: String,
}

/// Build the deterministic node id for an artifact.
pub fn artifact_node_id(kind: &str, name: &str) -> String {
    format!("{kind}:{name}")
}

fn dv_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => other.to_string(),
    }
}

impl GraphStore {
    /// Sync one operating-layer artifact into the graph: upsert its
    /// `entities` node and replace its outgoing artifact edges with
    /// `edges` (`(relation_type, to_node_id)` pairs — build target ids
    /// with [`artifact_node_id`]). Idempotent; safe to call on every
    /// write. Best-effort by contract: callers wrap it in `let _ =`
    /// so a node-sync failure never fails the underlying artifact write.
    pub fn upsert_artifact_node(
        &self,
        kind: &str,
        name: &str,
        version: i64,
        description: &str,
        edges: &[(String, String)],
    ) -> Result<()> {
        let id = artifact_node_id(kind, name);
        let desc = if version > 0 {
            format!("{description} (v{version})")
        } else {
            description.to_string()
        };

        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.clone().into()));
        params.insert("name".into(), DataValue::Str(name.into()));
        params.insert("etype".into(), DataValue::Str(kind.into()));
        params.insert("desc".into(), DataValue::Str(desc.into()));
        self.query(
            r#"?[id, canonical_name, entity_type, description] <- [[$id, $name, $etype, $desc]]
            :put entities {id => canonical_name, entity_type, description}"#,
            params,
        )?;

        // Replace this node's outgoing edges. Artifact ids ("kind:name")
        // never collide with real entity ids, so clearing all from_id=id
        // edges only drops artifact edges this node previously declared.
        let mut rm = BTreeMap::new();
        rm.insert("fid".into(), DataValue::Str(id.clone().into()));
        self.raw_db()
            .run_script(
                "?[from_id, to_id, relation_type] := *entity_relations{from_id, to_id, relation_type}, from_id = $fid\n:rm entity_relations {from_id, to_id, relation_type}",
                rm,
                ScriptMutability::Mutable,
            )
            .map_err(|e| Error::GraphStorage(format!("artifact edge clear: {e}")))?;

        for (rel, to_id) in edges {
            let mut p = BTreeMap::new();
            p.insert("from_id".into(), DataValue::Str(id.clone().into()));
            p.insert("to_id".into(), DataValue::Str(to_id.clone().into()));
            p.insert("relation_type".into(), DataValue::Str(rel.clone().into()));
            p.insert("strength".into(), DataValue::Num(Num::Float(1.0)));
            self.query(
                r#"?[from_id, to_id, relation_type, strength] <- [[$from_id, $to_id, $relation_type, $strength]]
                :put entity_relations {from_id, to_id, relation_type => strength}"#,
                p,
            )?;
        }
        Ok(())
    }

    /// All artifact nodes of a kind, sorted by name.
    pub fn list_artifact_nodes(&self, kind: &str) -> Result<Vec<ArtifactNode>> {
        let mut params = BTreeMap::new();
        params.insert("k".into(), DataValue::Str(kind.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[id, canonical_name, description] := *entities{id, canonical_name, entity_type, description}, entity_type = $k",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("list_artifact_nodes: {e}")))?;
        let mut out: Vec<ArtifactNode> = rows
            .rows
            .iter()
            .map(|r| ArtifactNode {
                id: dv_str(&r[0]),
                name: dv_str(&r[1]),
                kind: kind.to_string(),
                description: dv_str(&r[2]),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Remove an artifact node and its outgoing edges (e.g. a deleted branch).
    /// No-op if the node doesn't exist.
    pub fn remove_artifact_node(&self, kind: &str, name: &str) -> Result<()> {
        let id = artifact_node_id(kind, name);
        let mut p = BTreeMap::new();
        p.insert("fid".into(), DataValue::Str(id.clone().into()));
        self.raw_db()
            .run_script(
                "?[from_id, to_id, relation_type] := *entity_relations{from_id, to_id, relation_type}, from_id = $fid\n:rm entity_relations {from_id, to_id, relation_type}",
                p,
                ScriptMutability::Mutable,
            )
            .map_err(|e| Error::GraphStorage(format!("remove_artifact_node edges: {e}")))?;
        let mut p2 = BTreeMap::new();
        p2.insert("id".into(), DataValue::Str(id.into()));
        self.raw_db()
            .run_script(
                "?[id, canonical_name, entity_type, description] := *entities{id, canonical_name, entity_type, description}, id = $id\n:rm entities {id}",
                p2,
                ScriptMutability::Mutable,
            )
            .map_err(|e| Error::GraphStorage(format!("remove_artifact_node entity: {e}")))?;
        Ok(())
    }

    /// Sync one **durable** branch as a graph node. Status/parent/timestamps
    /// are JSON-encoded into the node `description` (re-upsert on each change —
    /// no schema column needed); a `forked_from` edge points at the parent
    /// branch node. Idempotent; flipping status is just another upsert.
    pub fn upsert_branch_node(&self, branch: &BranchNode) -> Result<()> {
        let desc = serde_json::to_string(branch)
            .map_err(|e| Error::GraphStorage(format!("encode branch node: {e}")))?;
        let edges: Vec<(String, String)> = match &branch.parent {
            Some(p) if !p.is_empty() => {
                vec![("forked_from".into(), artifact_node_id(KIND_BRANCH, p))]
            }
            _ => Vec::new(),
        };
        // version 0 → no " (vN)" suffix appended to our JSON description.
        self.upsert_artifact_node(KIND_BRANCH, &branch.name, 0, &desc, &edges)
    }

    /// All branch nodes, decoded from their JSON descriptions, sorted by name.
    /// A node whose description doesn't decode (hand-written / legacy) is
    /// surfaced with a best-effort `active` status rather than dropped.
    pub fn list_branch_nodes(&self) -> Result<Vec<BranchNode>> {
        let nodes = self.list_artifact_nodes(KIND_BRANCH)?;
        Ok(nodes
            .into_iter()
            .map(|n| {
                serde_json::from_str::<BranchNode>(&n.description).unwrap_or(BranchNode {
                    name: n.name.clone(),
                    status: BRANCH_STATUS_ACTIVE.to_string(),
                    parent: None,
                    kind: None,
                    created_at: 0.0,
                    merged_at: None,
                })
            })
            .collect())
    }

    /// Outgoing artifact edges `(relation_type, to_node_id)` for one node.
    pub fn artifact_edges(&self, kind: &str, name: &str) -> Result<Vec<(String, String)>> {
        let id = artifact_node_id(kind, name);
        let mut params = BTreeMap::new();
        params.insert("fid".into(), DataValue::Str(id.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[relation_type, to_id] := *entity_relations{from_id, to_id, relation_type}, from_id = $fid",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("artifact_edges: {e}")))?;
        Ok(rows
            .rows
            .iter()
            .map(|r| (dv_str(&r[0]), dv_str(&r[1])))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> GraphStore {
        let db = cozo::DbInstance::new("mem", "", "").unwrap();
        let s = GraphStore::from_db_for_testing(db);
        s.init_for_testing().unwrap();
        s
    }

    #[test]
    fn upsert_lists_and_edges_round_trip() {
        let s = store();
        // A prompt that uses a function which calls an MCP tool.
        s.upsert_artifact_node(KIND_FUNCTION, "scaffold", 1, "scaffold a component", &[(
            "calls".into(),
            artifact_node_id(KIND_MCP_TOOL, "github::create_pr"),
        )])
        .unwrap();
        s.upsert_artifact_node(KIND_PROMPT, "sys", 2, "system prompt", &[(
            "uses".into(),
            artifact_node_id(KIND_FUNCTION, "scaffold"),
        )])
        .unwrap();

        let prompts = s.list_artifact_nodes(KIND_PROMPT).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].id, "compiled_prompt:sys");
        assert_eq!(prompts[0].kind, KIND_PROMPT);

        let fns = s.list_artifact_nodes(KIND_FUNCTION).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "scaffold");

        let prompt_edges = s.artifact_edges(KIND_PROMPT, "sys").unwrap();
        assert_eq!(prompt_edges, vec![("uses".to_string(), "root_function:scaffold".to_string())]);
        let fn_edges = s.artifact_edges(KIND_FUNCTION, "scaffold").unwrap();
        assert_eq!(fn_edges, vec![("calls".to_string(), "mcp_tool:github::create_pr".to_string())]);
    }

    #[test]
    fn re_upsert_replaces_stale_edges() {
        let s = store();
        s.upsert_artifact_node(KIND_PROMPT, "p", 1, "", &[("uses".into(), "root_function:old".into())])
            .unwrap();
        // New version references a different function — the old edge must go.
        s.upsert_artifact_node(KIND_PROMPT, "p", 2, "", &[("uses".into(), "root_function:new".into())])
            .unwrap();
        let edges = s.artifact_edges(KIND_PROMPT, "p").unwrap();
        assert_eq!(edges, vec![("uses".to_string(), "root_function:new".to_string())]);
        // Still exactly one prompt node (upsert, not duplicate).
        assert_eq!(s.list_artifact_nodes(KIND_PROMPT).unwrap().len(), 1);
    }

    #[test]
    fn branch_node_upsert_flip_and_lineage() {
        let s = store();
        // Fork a topic branch off main.
        s.upsert_branch_node(&BranchNode {
            name: "topic/auth".into(),
            status: BRANCH_STATUS_ACTIVE.into(),
            parent: Some("main".into()),
            kind: Some("topic".into()),
            created_at: 100.0,
            merged_at: None,
        })
        .unwrap();

        let nodes = s.list_branch_nodes().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "topic/auth");
        assert_eq!(nodes[0].status, BRANCH_STATUS_ACTIVE);
        assert_eq!(nodes[0].parent.as_deref(), Some("main"));
        // Lineage edge present.
        let edges = s.artifact_edges(KIND_BRANCH, "topic/auth").unwrap();
        assert_eq!(edges, vec![("forked_from".to_string(), "branch:main".to_string())]);

        // Flip to merged — re-upsert, NOT a duplicate; status + merged_at update.
        s.upsert_branch_node(&BranchNode {
            name: "topic/auth".into(),
            status: BRANCH_STATUS_MERGED.into(),
            parent: Some("main".into()),
            kind: Some("topic".into()),
            created_at: 100.0,
            merged_at: Some(200.0),
        })
        .unwrap();
        let nodes = s.list_branch_nodes().unwrap();
        assert_eq!(nodes.len(), 1, "re-upsert must not duplicate");
        assert_eq!(nodes[0].status, BRANCH_STATUS_MERGED, "never 'active' after merge");
        assert_eq!(nodes[0].merged_at, Some(200.0));

        // Remove on delete.
        s.remove_artifact_node(KIND_BRANCH, "topic/auth").unwrap();
        assert!(s.list_branch_nodes().unwrap().is_empty());
        assert!(s.artifact_edges(KIND_BRANCH, "topic/auth").unwrap().is_empty());
    }
}
