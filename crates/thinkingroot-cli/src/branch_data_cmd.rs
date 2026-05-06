//! `root branch contribute-bulk` and `root branch redaction-set` —
//! CLI parity for the T0.7 connector bulk-contribute and T2.6 outbound
//! redaction policy surfaces. Both already exposed over REST + MCP;
//! these are the terminal-friendly bindings.
//!
//! Bulk contribute requires Principal::Connector (per
//! `.claude/rules/branch-system.md`), so the CLI takes
//! `--connector-id` + `--install-id` as required flags. The body
//! comes from a JSON file rather than positional args because the
//! claims array is meaningful to compose offline.

use std::path::Path;

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

#[derive(serde::Deserialize)]
struct BulkInputFile {
    /// Optional session id for turn attribution.
    #[serde(default)]
    session_id: Option<String>,
    /// When true, defer per-claim rooting to end-of-batch.
    #[serde(default)]
    backfill: bool,
    /// Workspace name override. When absent, the CLI computes one from
    /// the supplied --path.
    #[serde(default)]
    workspace: Option<String>,
    /// The claims being contributed. Must be `engine::AgentClaim`-shaped:
    ///   `{ statement, claim_type?, confidence?, entities? }`
    claims: Vec<serde_json::Value>,
}

/// `root branch contribute-bulk <branch> --connector-id <id>
/// --install-id <id> --idempotency-key <k> --file <path/to/claims.json>`
#[allow(clippy::too_many_arguments)]
pub async fn run_contribute_bulk(
    conn: &EngineConnection,
    root: &Path,
    branch: &str,
    connector_id: &str,
    install_id: &str,
    idempotency_key: &str,
    file: &Path,
) -> anyhow::Result<()> {
    let body_str = std::fs::read_to_string(file)
        .with_context(|| format!("read {}", file.display()))?;
    let parsed: BulkInputFile = serde_json::from_str(&body_str)
        .with_context(|| format!("parse {} as bulk-contribute input", file.display()))?;

    // The daemon needs a workspace name. Mount + reuse the resolved id.
    let ws_default = cortex_remote::ensure_mounted_remote(conn, root).await?;
    let workspace = parsed.workspace.unwrap_or(ws_default);

    let body = serde_json::json!({
        "workspace": workspace,
        "session_id": parsed.session_id,
        "connector_id": connector_id,
        "install_id": install_id,
        "idempotency_key": idempotency_key,
        "backfill": parsed.backfill,
        "claims": parsed.claims,
    });
    let path = format!("/api/v1/branches/{branch}/contribute-bulk");
    let data = cortex_remote::post_json(conn, &path, &body)
        .await
        .with_context(|| format!("contribute-bulk to {branch}"))?;

    let accepted = data.get("accepted_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let replayed = data.get("replayed").and_then(|v| v.as_bool()).unwrap_or(false);
    println!(
        "  {} {} claim(s) accepted on branch {}{}",
        style("✓").green().bold(),
        style(accepted).cyan().bold(),
        style(branch).white().bold(),
        if replayed {
            style(" (idempotent replay)").dim().to_string()
        } else {
            String::new()
        }
    );
    Ok(())
}

/// `root branch redaction-set <branch> --file <policy.json>`
/// or `root branch redaction-set <branch> --clear`
pub async fn run_redaction_set(
    conn: &EngineConnection,
    branch: &str,
    file: Option<&Path>,
    clear: bool,
) -> anyhow::Result<()> {
    let body = if clear {
        serde_json::json!({ "policy": null })
    } else {
        let f = file.ok_or_else(|| {
            anyhow::anyhow!("either --file <policy.json> or --clear is required")
        })?;
        let body_str = std::fs::read_to_string(f)
            .with_context(|| format!("read {}", f.display()))?;
        let policy: serde_json::Value = serde_json::from_str(&body_str)
            .with_context(|| format!("parse {} as RedactionPolicy", f.display()))?;
        serde_json::json!({ "policy": policy })
    };

    let path = format!("/api/v1/branches/{branch}/redaction");
    cortex_remote::post_json(conn, &path, &body)
        .await
        .with_context(|| format!("set redaction on {branch}"))?;

    if clear {
        println!(
            "  {} redaction policy cleared on {}",
            style("✓").green().bold(),
            style(branch).white().bold()
        );
    } else {
        println!(
            "  {} redaction policy applied to {}",
            style("✓").green().bold(),
            style(branch).white().bold()
        );
    }
    Ok(())
}
