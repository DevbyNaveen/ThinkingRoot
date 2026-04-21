//! `root rooting` subcommand — inspect and maintain the Rooting gate from
//! the CLI.
//!
//! Subcommands:
//! - `report`: per-tier claim counts + the most recent trial failures.
//! - `verify <claim_id>`: full trial history + certificate details for one claim.
//! - `re-run [--all | --claim <id>]`: re-execute Rooting against current source
//!   bytes; catches drift without recompiling the whole pack.

use std::path::Path;

use console::style;
use thinkingroot_core::config::Config;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_rooting::{
    CandidateClaim, FileSystemSourceStore, Rooter, RootingConfig,
};

pub fn report(workspace_path: &Path) -> anyhow::Result<()> {
    let data_dir = resolve_data_dir(workspace_path)?;
    let graph_dir = data_dir.join("graph");
    let graph = GraphStore::init(&graph_dir)?;

    let (rooted, attested, quarantined, rejected) = graph.count_claims_by_admission_tier()?;
    let total = rooted + attested + quarantined + rejected;

    println!();
    println!("{}", style("Rooting Report").bold());
    println!("{}", style(format!("  workspace: {}", data_dir.display())).dim());
    println!();
    if total == 0 {
        println!(
            "  {}  workspace has no claims yet — run {} first",
            style("!").yellow(),
            style("root compile").bold()
        );
        return Ok(());
    }

    let pct = |n: usize| (n as f64 / total as f64) * 100.0;
    println!("{}", style("  Admission tiers").bold());
    println!(
        "    {:<13} {:>6} ({:>5.1}%)",
        style("Rooted").green(),
        rooted,
        pct(rooted)
    );
    println!(
        "    {:<13} {:>6} ({:>5.1}%)",
        style("Attested").white(),
        attested,
        pct(attested)
    );
    println!(
        "    {:<13} {:>6} ({:>5.1}%)",
        style("Quarantined").yellow(),
        quarantined,
        pct(quarantined)
    );
    println!(
        "    {:<13} {:>6} ({:>5.1}%)",
        style("Rejected").red(),
        rejected,
        pct(rejected)
    );
    println!();

    Ok(())
}

pub fn verify(workspace_path: &Path, claim_id: &str) -> anyhow::Result<()> {
    let data_dir = resolve_data_dir(workspace_path)?;
    let graph_dir = data_dir.join("graph");
    let graph = GraphStore::init(&graph_dir)?;

    println!();
    println!("{} {}", style("Rooting Verification").bold(), style(claim_id).dim());
    println!();

    let verdicts = graph.get_trial_verdicts_for_claim(claim_id)?;
    if verdicts.is_empty() {
        println!(
            "  {}  no trial verdicts found for this claim",
            style("!").yellow()
        );
        println!(
            "      (claim may pre-date Rooting or never went through Phase 6.5)",
        );
        return Ok(());
    }

    println!("{}", style(format!("  {} trial(s) on record", verdicts.len())).bold());
    for (i, v) in verdicts.iter().enumerate() {
        let (vid, trial_at, tier, prov, contra, pred, topo, temp, cert, reason, version) = v;
        let trial_time = chrono::DateTime::<chrono::Utc>::from_timestamp(*trial_at as i64, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "unknown".into());
        let tier_styled = match tier.as_str() {
            "rooted" => style(tier.as_str()).green().to_string(),
            "attested" => style(tier.as_str()).white().to_string(),
            "quarantined" => style(tier.as_str()).yellow().to_string(),
            "rejected" => style(tier.as_str()).red().to_string(),
            _ => tier.to_string(),
        };
        println!();
        println!("  #{}  {}   {}", i + 1, tier_styled, style(&trial_time).dim());
        println!("     trial_id    {}", style(vid).dim());
        println!("     rooter      {}", style(version).dim());
        let probe = |label: &str, score: f64| {
            if score < 0.0 {
                format!("{:<14} {}", label, style("skipped").dim())
            } else {
                format!("{:<14} {:.2}", label, score)
            }
        };
        println!("     {}", probe("provenance", *prov));
        println!("     {}", probe("contradiction", *contra));
        println!("     {}", probe("predicate", *pred));
        println!("     {}", probe("topology", *topo));
        println!("     {}", probe("temporal", *temp));
        if !cert.is_empty() {
            println!("     certificate  {}", style(cert).cyan());
        }
        if !reason.is_empty() {
            println!("     reason       {}", style(reason).yellow());
        }
    }
    println!();
    Ok(())
}

pub fn re_run(
    workspace_path: &Path,
    all: bool,
    claim_id: Option<&str>,
) -> anyhow::Result<()> {
    let data_dir = resolve_data_dir(workspace_path)?;
    let graph_dir = data_dir.join("graph");
    let graph = GraphStore::init(&graph_dir)?;
    let byte_store = FileSystemSourceStore::new(&data_dir)?;

    // Build the list of claim IDs to re-run.
    let ids: Vec<String> = if all {
        graph.get_all_claim_ids()?
    } else {
        vec![claim_id
            .expect("caller validated all-or-claim")
            .to_string()]
    };

    if ids.is_empty() {
        println!(
            "{}  no claims in workspace — nothing to re-run",
            style("!").yellow()
        );
        return Ok(());
    }

    println!();
    println!(
        "{} {}",
        style("Re-running Rooting").bold(),
        style(format!("{} claim(s)", ids.len())).dim()
    );

    // Load config for Rooting thresholds. Missing or malformed config falls
    // back to defaults — the re-run path should never fail because of config.
    let cfg = Config::load(workspace_path).unwrap_or_default();
    let rooting_cfg = RootingConfig {
        disabled: cfg.rooting.disabled,
        provenance_threshold: cfg.rooting.provenance_threshold,
        contradiction_floor: cfg.rooting.contradiction_floor,
        contribute_gate: cfg.rooting.contribute_gate.clone(),
    };
    if rooting_cfg.disabled {
        anyhow::bail!("rooting is disabled in workspace config — re-run aborted");
    }

    // Fetch full Claim structs and construct candidates.
    let mut claims: Vec<thinkingroot_core::Claim> = Vec::with_capacity(ids.len());
    for id in &ids {
        match graph.get_claim_by_id(id)? {
            Some(c) => claims.push(c),
            None => {
                tracing::warn!("claim {id} not found — skipping");
            }
        }
    }

    let candidates: Vec<CandidateClaim<'_>> = claims
        .iter()
        .map(|c| CandidateClaim {
            claim: c,
            predicate: c.predicate.as_ref(),
            derivation: c.derivation.as_ref(),
        })
        .collect();

    let rooter = Rooter::new(&graph, &byte_store, rooting_cfg);
    let output = rooter
        .root_batch(&candidates)
        .map_err(|e| anyhow::anyhow!("rooting re-run failed: {e}"))?;

    // Persist new verdicts + certificates. Update admission_tier +
    // last_rooted_at on the survivors so subsequent queries see the new tier
    // without another root re-run.
    thinkingroot_rooting::storage::insert_verdicts_batch(&graph, &output.verdicts)
        .map_err(|e| anyhow::anyhow!("persist verdicts: {e}"))?;
    thinkingroot_rooting::storage::insert_certificates_batch(&graph, &output.certificates)
        .map_err(|e| anyhow::anyhow!("persist certificates: {e}"))?;

    let mut promoted = 0usize;
    let mut unchanged = 0usize;
    let mut demoted = 0usize;
    for (idx, verdict) in output.verdicts.iter().enumerate() {
        let claim = &claims[idx];
        let old_tier = claim.admission_tier;
        let new_tier = verdict.admission_tier;
        let mut updated = claim.clone();
        updated.admission_tier = new_tier;
        updated.last_rooted_at = Some(verdict.trial_at);
        graph.insert_claim(&updated)?;
        match (old_tier, new_tier) {
            (a, b) if a == b => unchanged += 1,
            (thinkingroot_core::types::AdmissionTier::Attested, _)
            | (thinkingroot_core::types::AdmissionTier::Quarantined, thinkingroot_core::types::AdmissionTier::Rooted) => {
                promoted += 1
            }
            _ => demoted += 1,
        }
    }

    println!();
    println!(
        "  {}  rooted_now     {}",
        style("✓").green(),
        output.admitted_count
    );
    println!(
        "  {}  quarantined    {}",
        style("!").yellow(),
        output.quarantined_count
    );
    println!(
        "  {}  rejected       {}",
        style("✗").red(),
        output.rejected_count
    );
    println!();
    println!(
        "  {}  promoted: {}   {}  unchanged: {}   {}  demoted: {}",
        style("↑").green(),
        promoted,
        style("=").dim(),
        unchanged,
        style("↓").red(),
        demoted
    );
    println!();
    Ok(())
}

fn resolve_data_dir(workspace_path: &Path) -> anyhow::Result<std::path::PathBuf> {
    // Matches the convention used by other subcommands: `.thinkingroot/` is
    // the default data dir for a workspace. Branching is out of scope for
    // the Week 2 CLI — we always read from main.
    let dir = workspace_path.join(".thinkingroot");
    if !dir.exists() {
        anyhow::bail!(
            "no ThinkingRoot workspace found at {} — run `root compile` first",
            workspace_path.display()
        );
    }
    Ok(dir)
}
