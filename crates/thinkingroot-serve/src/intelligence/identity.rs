// crates/thinkingroot-serve/src/intelligence/identity.rs
//
// Workspace identity assembler.
//
// Produces a small, structured snapshot the chat synthesizer injects as a
// `<system-reminder>` context block — modelled after Claude Code's
// `getUserContext()` / `getSystemContext()` (gitStatus + claudeMd +
// currentDate). Without this, the LLM has no anchor for *which* workspace
// it's answering about and tends to default to generic answers.
//
// Pure-ish: the project-doc lookup reads at most one file from disk; counts
// and source kinds come from the in-memory cache via
// [`crate::engine::WorkspaceChatSnapshot`]. Safe to call once per request.

use std::path::{Path, PathBuf};

use thinkingroot_core::config::ChatConfig;

use crate::engine::WorkspaceChatSnapshot;

/// Soft cap on bytes lifted from the workspace's project doc. Matches the
/// Claude Code CLAUDE.md cap so the prompt overhead stays bounded even on
/// very large repos.
pub const PROJECT_DOC_MAX_BYTES: usize = 4096;

/// Filenames the auto-discovery walks when the user has not pinned an
/// explicit `project_doc_path` in `[chat]`. Order is intentional: the most
/// agent-targeted files come first so a repo with both `CLAUDE.md` and
/// `README.md` ends up using the agent-tuned variant.
const AUTO_DISCOVERY_FILES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    ".thinkingroot/INTRO.md",
    "README.md",
    "readme.md",
];

/// Stable, structured workspace identity — the data the synthesizer
/// embeds in the `<system-reminder>` context block. Fields are public
/// strings so the rendering code stays a pure formatter.
#[derive(Debug, Clone)]
pub struct WorkspaceIdentity {
    pub name: String,
    pub mounted_at: PathBuf,
    pub claim_count: usize,
    /// `(kind_label, count)` — already sorted descending by the snapshot.
    pub source_kinds: Vec<(String, usize)>,
    /// Auto-discovered project documentation, capped at
    /// [`PROJECT_DOC_MAX_BYTES`]. `None` when discovery is disabled or
    /// no recognised file exists.
    pub project_doc: Option<ProjectDoc>,
}

/// First few KB of a workspace's project README / agent guide.
#[derive(Debug, Clone)]
pub struct ProjectDoc {
    /// Filename relative to the workspace root, used as a label inside
    /// the `<system-reminder>` block so the LLM can cite it.
    pub label: String,
    pub content: String,
    /// `true` when the underlying file was longer than
    /// [`PROJECT_DOC_MAX_BYTES`] and we sliced.
    pub truncated: bool,
}

/// Build a `WorkspaceIdentity` from a cache snapshot.
///
/// `chat_config` controls project-doc discovery: when
/// `include_project_doc` is `false`, `project_doc` is always `None`; when
/// `project_doc_path` is set, it overrides the auto-discovery list.
pub fn build_workspace_identity(
    snapshot: &WorkspaceChatSnapshot,
    chat_config: &ChatConfig,
) -> WorkspaceIdentity {
    let project_doc = if chat_config.include_project_doc {
        load_project_doc(&snapshot.root_path, chat_config.project_doc_path.as_deref())
    } else {
        None
    };

    WorkspaceIdentity {
        name: snapshot.name.clone(),
        mounted_at: snapshot.root_path.clone(),
        claim_count: snapshot.claim_count,
        source_kinds: snapshot.source_kinds.clone(),
        project_doc,
    }
}

fn load_project_doc(root: &Path, explicit: Option<&str>) -> Option<ProjectDoc> {
    if let Some(rel) = explicit {
        // Explicit path — refuse to climb out of the workspace via `..`
        // segments. If a user really wants something outside, they can
        // copy it in.
        let candidate = root.join(rel);
        if candidate.starts_with(root) {
            return read_capped(&candidate, rel);
        }
        return None;
    }

    for name in AUTO_DISCOVERY_FILES {
        let candidate = root.join(name);
        if let Some(doc) = read_capped(&candidate, name) {
            return Some(doc);
        }
    }
    None
}

fn read_capped(path: &Path, label: &str) -> Option<ProjectDoc> {
    let raw = std::fs::read(path).ok()?;
    if raw.is_empty() {
        return None;
    }
    let truncated = raw.len() > PROJECT_DOC_MAX_BYTES;
    let slice = if truncated {
        &raw[..PROJECT_DOC_MAX_BYTES]
    } else {
        &raw[..]
    };
    // Truncation must land on a UTF-8 boundary; nudge backwards if needed.
    let safe_end = utf8_safe_end(slice);
    let content = String::from_utf8_lossy(&slice[..safe_end]).into_owned();
    Some(ProjectDoc {
        label: label.to_string(),
        content,
        truncated,
    })
}

fn utf8_safe_end(bytes: &[u8]) -> usize {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    if end == 0 {
        bytes.len()
    } else {
        // If the byte before `end` starts a multi-byte sequence that
        // is now incomplete, walk it off too.
        let start_byte = bytes[end - 1];
        let needed = if start_byte & 0b1000_0000 == 0 {
            1
        } else if start_byte & 0b1110_0000 == 0b1100_0000 {
            2
        } else if start_byte & 0b1111_0000 == 0b1110_0000 {
            3
        } else if start_byte & 0b1111_1000 == 0b1111_0000 {
            4
        } else {
            // Continuation byte at the leading position — corrupt input,
            // give the lossy decoder a single byte to work with.
            return end;
        };
        let lead_pos = end - 1;
        let have = bytes.len() - lead_pos;
        if have < needed {
            lead_pos
        } else {
            end
        }
    }
}

/// Render the identity as the body of a `<system-reminder>` block.
///
/// Format mirrors Claude Code's `prependUserContext` output —
/// `# section_name\nkey: value` lines — so models RLHF-tuned on that
/// shape recognise it as ambient context rather than user request.
pub fn render_identity_block(identity: &WorkspaceIdentity, today: Option<&str>) -> String {
    let mut out = String::with_capacity(512);

    out.push_str("# workspace\n");
    out.push_str(&format!("name: {}\n", identity.name));
    out.push_str(&format!("mounted_at: {}\n", identity.mounted_at.display()));
    out.push_str(&format!("claims_indexed: {}\n", identity.claim_count));
    if !identity.source_kinds.is_empty() {
        let top: Vec<String> = identity
            .source_kinds
            .iter()
            .take(6)
            .map(|(k, n)| format!("{k}({n})"))
            .collect();
        out.push_str(&format!("sources: {}\n", top.join(", ")));
    }

    if let Some(today) = today {
        if !today.trim().is_empty() {
            out.push_str("\n# today\n");
            out.push_str(today);
            out.push('\n');
        }
    }

    if let Some(doc) = &identity.project_doc {
        out.push_str(&format!("\n# project_doc ({})\n", doc.label));
        out.push_str(doc.content.trim_end());
        out.push('\n');
        if doc.truncated {
            out.push_str(&format!(
                "\n[truncated to first {} bytes]\n",
                PROJECT_DOC_MAX_BYTES
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, root: &Path, claim_count: usize, kinds: &[(&str, usize)]) -> WorkspaceChatSnapshot {
        WorkspaceChatSnapshot {
            name: name.to_string(),
            root_path: root.to_path_buf(),
            config: thinkingroot_core::Config::default(),
            claim_count,
            source_kinds: kinds.iter().map(|(k, n)| (k.to_string(), *n)).collect(),
        }
    }

    #[test]
    fn identity_carries_snapshot_fields() {
        let dir = tempfile::tempdir().unwrap();
        let s = snap("acme", dir.path(), 1234, &[("rs", 800), ("md", 200)]);
        let cfg = ChatConfig::default();
        let id = build_workspace_identity(&s, &cfg);
        assert_eq!(id.name, "acme");
        assert_eq!(id.claim_count, 1234);
        assert_eq!(id.source_kinds.len(), 2);
    }

    #[test]
    fn auto_discovers_claude_md_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), b"# Project\nHello, agent.")
            .unwrap();
        let s = snap("acme", dir.path(), 0, &[]);
        let cfg = ChatConfig::default();
        let id = build_workspace_identity(&s, &cfg);
        let doc = id.project_doc.expect("expected CLAUDE.md to be found");
        assert_eq!(doc.label, "CLAUDE.md");
        assert!(doc.content.contains("Hello, agent."));
        assert!(!doc.truncated);
    }

    #[test]
    fn project_doc_lookup_prefers_claude_over_readme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), b"agent guide").unwrap();
        std::fs::write(dir.path().join("README.md"), b"public readme").unwrap();
        let s = snap("acme", dir.path(), 0, &[]);
        let id = build_workspace_identity(&s, &ChatConfig::default());
        assert_eq!(id.project_doc.unwrap().label, "CLAUDE.md");
    }

    #[test]
    fn explicit_project_doc_path_overrides_auto_discovery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), b"agent guide").unwrap();
        std::fs::write(dir.path().join("docs/INTRO.md"), b"explicit doc").unwrap();
        let s = snap("acme", dir.path(), 0, &[]);
        let cfg = ChatConfig {
            project_doc_path: Some("docs/INTRO.md".to_string()),
            ..ChatConfig::default()
        };
        let id = build_workspace_identity(&s, &cfg);
        let doc = id.project_doc.unwrap();
        assert_eq!(doc.label, "docs/INTRO.md");
        assert!(doc.content.contains("explicit doc"));
    }

    #[test]
    fn explicit_path_outside_workspace_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ChatConfig {
            project_doc_path: Some("../escape.md".to_string()),
            ..ChatConfig::default()
        };
        let s = snap("acme", dir.path(), 0, &[]);
        let id = build_workspace_identity(&s, &cfg);
        assert!(id.project_doc.is_none());
    }

    #[test]
    fn project_doc_disabled_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), b"agent guide").unwrap();
        let cfg = ChatConfig {
            include_project_doc: false,
            ..ChatConfig::default()
        };
        let s = snap("acme", dir.path(), 0, &[]);
        let id = build_workspace_identity(&s, &cfg);
        assert!(id.project_doc.is_none());
    }

    #[test]
    fn project_doc_truncates_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(PROJECT_DOC_MAX_BYTES * 2);
        std::fs::write(dir.path().join("CLAUDE.md"), big.as_bytes()).unwrap();
        let s = snap("acme", dir.path(), 0, &[]);
        let id = build_workspace_identity(&s, &ChatConfig::default());
        let doc = id.project_doc.unwrap();
        assert!(doc.truncated);
        assert!(doc.content.len() <= PROJECT_DOC_MAX_BYTES);
    }

    #[test]
    fn render_block_includes_workspace_section() {
        let dir = PathBuf::from("/tmp/acme");
        let id = WorkspaceIdentity {
            name: "acme".to_string(),
            mounted_at: dir,
            claim_count: 1253,
            source_kinds: vec![("rs".to_string(), 800), ("md".to_string(), 200)],
            project_doc: None,
        };
        let block = render_identity_block(&id, Some("2026-04-28"));
        assert!(block.contains("# workspace"));
        assert!(block.contains("name: acme"));
        assert!(block.contains("claims_indexed: 1253"));
        assert!(block.contains("rs(800)"));
        assert!(block.contains("md(200)"));
        assert!(block.contains("# today"));
        assert!(block.contains("2026-04-28"));
    }

    #[test]
    fn render_block_includes_project_doc_when_present() {
        let id = WorkspaceIdentity {
            name: "acme".to_string(),
            mounted_at: PathBuf::from("/tmp/acme"),
            claim_count: 100,
            source_kinds: vec![],
            project_doc: Some(ProjectDoc {
                label: "CLAUDE.md".to_string(),
                content: "# Project\nLine 1\nLine 2".to_string(),
                truncated: false,
            }),
        };
        let block = render_identity_block(&id, None);
        assert!(block.contains("# project_doc (CLAUDE.md)"));
        assert!(block.contains("Line 1"));
        assert!(!block.contains("[truncated"));
    }

    #[test]
    fn render_block_marks_truncated_doc() {
        let id = WorkspaceIdentity {
            name: "acme".to_string(),
            mounted_at: PathBuf::from("/tmp/acme"),
            claim_count: 0,
            source_kinds: vec![],
            project_doc: Some(ProjectDoc {
                label: "README.md".to_string(),
                content: "x".repeat(PROJECT_DOC_MAX_BYTES),
                truncated: true,
            }),
        };
        let block = render_identity_block(&id, None);
        assert!(block.contains("[truncated to first"));
    }
}
