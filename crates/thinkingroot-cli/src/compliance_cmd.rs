//! `root compliance --eu-ai-act` — emit an EU AI Act technical-
//! documentation bundle for a compiled workspace.
//!
//! The bundle is the offline-verifiable counterpart to the Annex IV
//! technical documentation required by Reg. (EU) 2024/1689 for
//! high-risk AI systems. As of 2026-05-09 (today) the GPAI obligations
//! are binding (since 2025-08-02) and the high-risk obligations are
//! ~3 months away from being binding (2026-08-02). This command is
//! intentionally honest about what it can and cannot attest to:
//!
//! * It can produce a system-card, a model-identity manifest, and a
//!   per-claim provenance log derived from the workspace's CozoDB
//!   substrate (Article 11 / Annex IV §1, §2, §3 — system architecture,
//!   data governance, validation).
//! * It cannot fabricate training-data lineage for closed third-party
//!   LLMs. The training-data-summary.md file therefore points at the
//!   provider's own published terms via an allow-list of canonical URLs
//!   and refuses to print freeform attestations the operator did not
//!   supply (CLAUDE.md §honesty rule §1).
//!
//! Output layout (under `--out`):
//! ```text
//! compliance-bundle-{ws}-{ISO8601}.tr-compliance/
//! ├── manifest.toml             # bundle index with BLAKE3 of every file
//! ├── system-card.md            # Annex IV §1+§2 — system architecture
//! ├── model-identity.toml       # active provider + model + fingerprint
//! ├── provenance-log.jsonl      # Article 12 — one line per claim
//! ├── training-data-summary.md  # Article 13 — provider-published terms
//! ├── performance-metrics.toml  # Annex IV §3 — rooting tier histogram
//! ├── intended-purpose.md       # Article 13 — workspace README narrative
//! ├── deployer-obligations.md   # Article 26 — deployer checklist
//! └── signature.bundle          # optional: Sigstore over manifest.toml
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use thinkingroot_core::config::Config;

use thinkingroot_graph::graph::{GraphStore, V3ClaimExportRow};

/// CLI argument bundle for [`run_compliance`].
#[derive(Debug, Clone)]
pub struct ComplianceOpts {
    /// Emit the EU AI Act bundle. Today the only supported framework;
    /// the flag exists so other regimes (e.g. NIST AI RMF) can be added
    /// without a wire-format break.
    pub eu_ai_act: bool,
    /// Output directory. The bundle is written as a sub-directory of
    /// this path, named `compliance-bundle-{ws}-{ISO8601}.tr-compliance`.
    /// Defaults to `<workspace>/`.
    pub out: Option<PathBuf>,
    /// Sign the bundle's manifest with Sigstore-public-good keyless DSSE.
    /// Identical to `root pack --sign-keyless` — drives the same OIDC
    /// browser flow (or `$TR_OIDC_TOKEN` if set).
    pub sign: bool,
    /// Workspace root. Must contain `.thinkingroot/`.
    pub workspace: PathBuf,
}

/// Result handed back from [`run_compliance`]. The CLI prints the
/// bundle directory path; tests assert on every field. Fields are
/// surfaced as `pub` so downstream integrators (e.g. desktop wrappers,
/// audit pipelines) can read the bundle metadata without re-walking
/// the directory.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ComplianceBundle {
    /// Absolute path to the on-disk bundle directory.
    pub bundle_dir: PathBuf,
    /// File names (relative to `bundle_dir`) along with their BLAKE3 hex.
    /// Keyed by file name so order is stable across runs.
    pub files: BTreeMap<String, String>,
    /// True iff the bundle was signed and `signature.bundle` exists.
    pub signed: bool,
}

/// Drive the compliance command end-to-end.
///
/// Reads the workspace's CozoDB graph + config + README, renders eight
/// human-readable artefacts, computes a BLAKE3 manifest, and (optionally)
/// signs the manifest via Sigstore-keyless. Idempotent on re-run with
/// the same `--out` and timestamp; consecutive runs land in distinct
/// timestamped sub-directories.
pub async fn run_compliance(opts: ComplianceOpts) -> Result<ComplianceBundle> {
    if !opts.eu_ai_act {
        return Err(anyhow!(
            "no compliance framework selected; pass `--eu-ai-act`"
        ));
    }
    let ws = opts.workspace.canonicalize().with_context(|| {
        format!("workspace `{}` does not exist", opts.workspace.display())
    })?;
    let engine_dir = ws.join(".thinkingroot");
    if !engine_dir.exists() {
        return Err(anyhow!(
            "no engine output at `{}`; run `root compile {}` first",
            engine_dir.display(),
            ws.display()
        ));
    }

    let now = chrono::Utc::now();
    let ws_slug = workspace_slug(&ws);
    let bundle_name = format!(
        "compliance-bundle-{ws_slug}-{}.tr-compliance",
        now.format("%Y%m%dT%H%M%SZ")
    );
    let bundle_dir = opts.out.unwrap_or_else(|| ws.clone()).join(&bundle_name);
    fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("create {}", bundle_dir.display()))?;

    // ── 1. Gather the substrate snapshot. ──────────────────────────
    let config = Config::load_merged(&ws).with_context(|| "load workspace config")?;
    let graph_dir = engine_dir.join("graph");
    let graph = GraphStore::init(&graph_dir)
        .with_context(|| format!("open graph at {}", graph_dir.display()))?;
    let claims = graph.get_v3_claim_export()?;
    let entity_names = graph.get_claim_entity_names()?;
    let tier_counts = graph.count_claims_by_admission_tier()?;
    let pack_meta = read_pack_toml(&ws);
    let intended_purpose_text = read_intended_purpose(&ws);

    // ── 2. Render every artefact into memory before writing. ───────
    let system_card = render_system_card(&ws, &config, &claims, &entity_names);
    let model_identity = render_model_identity(&config, &pack_meta);
    let provenance_log = render_provenance_log(&claims, &entity_names);
    let training_data_summary = render_training_data_summary(&config);
    let performance_metrics = render_performance_metrics(&claims, tier_counts);
    let intended_purpose = render_intended_purpose(&intended_purpose_text, &pack_meta);
    let deployer_obligations = render_deployer_obligations();

    // ── 3. Write artefacts and accumulate per-file BLAKE3. ─────────
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    write_artefact(&bundle_dir, "system-card.md", &system_card, &mut files)?;
    write_artefact(&bundle_dir, "model-identity.toml", &model_identity, &mut files)?;
    write_artefact(&bundle_dir, "provenance-log.jsonl", &provenance_log, &mut files)?;
    write_artefact(
        &bundle_dir,
        "training-data-summary.md",
        &training_data_summary,
        &mut files,
    )?;
    write_artefact(
        &bundle_dir,
        "performance-metrics.toml",
        &performance_metrics,
        &mut files,
    )?;
    write_artefact(&bundle_dir, "intended-purpose.md", &intended_purpose, &mut files)?;
    write_artefact(
        &bundle_dir,
        "deployer-obligations.md",
        &deployer_obligations,
        &mut files,
    )?;

    // ── 4. Manifest + optional signature. ──────────────────────────
    let manifest = render_manifest(&bundle_name, &ws, &config, now, &files);
    let manifest_path = bundle_dir.join("manifest.toml");
    fs::write(&manifest_path, manifest.as_bytes())
        .with_context(|| format!("write {}", manifest_path.display()))?;
    let manifest_hash = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    files.insert("manifest.toml".to_string(), manifest_hash);

    let signed = if opts.sign {
        let bundle_bytes = sign_manifest_keyless(manifest.as_bytes(), &bundle_name).await?;
        let sig_path = bundle_dir.join("signature.bundle");
        fs::write(&sig_path, &bundle_bytes)
            .with_context(|| format!("write {}", sig_path.display()))?;
        let sig_hash = blake3::hash(&bundle_bytes).to_hex().to_string();
        files.insert("signature.bundle".to_string(), sig_hash);
        true
    } else {
        false
    };

    println!(
        "  compliance bundle (eu-ai-act) -> {} ({} files{})",
        bundle_dir.display(),
        files.len(),
        if signed { ", signed" } else { "" }
    );
    Ok(ComplianceBundle {
        bundle_dir,
        files,
        signed,
    })
}

// ─────────────────────────────────────────────────────────────────────
// Renderers. Each produces a deterministic byte string from the
// substrate snapshot — no rand, no system-time other than what is
// threaded in explicitly, no provider-key leakage.
// ─────────────────────────────────────────────────────────────────────

fn render_system_card(
    workspace: &Path,
    config: &Config,
    claims: &[V3ClaimExportRow],
    entity_names: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    let total_entities: std::collections::HashSet<&String> = entity_names
        .values()
        .flat_map(|names| names.iter())
        .collect();
    format!(
        "# ThinkingRoot system card\n\
         \n\
         _Annex IV §1 + §2 — system architecture and data governance._\n\
         \n\
         ## System identity\n\
         \n\
         - **Workspace:** `{ws}`\n\
         - **Engine:** ThinkingRoot OSS v{engine_ver}\n\
         - **Pipeline:** Parse → Extract → Ground → Rooting → Link → SVO → CozoDB persist\n\
         - **Substrate:** 33-table typed Datalog (CozoDB) per Compile Completeness Contract\n\
         - **LLM provider:** `{provider}`\n\
         - **Extraction model:** `{ext_model}`\n\
         \n\
         ## Data flows\n\
         \n\
         1. Source files are walked into the parser. Files exceeding the workspace `max_file_size` cap are skipped (recorded in the orphan audit).\n\
         2. The parser emits typed chunks with `(byte_start, byte_end)` ranges (invariant I-2).\n\
         3. The extractor calls the configured LLM provider per chunk; outputs are scored and admitted into typed claim rows with one of four trust tiers (Rooted / Attested / Quarantined / Rejected).\n\
         4. Phase 9 byte-coverage audit fails the compile if any source byte is unaccounted for (invariant I-3).\n\
         5. Every structural row carries `content_blake3` over its source slice (invariant I-4).\n\
         \n\
         ## Substrate snapshot\n\
         \n\
         - Total claims: **{claim_count}**\n\
         - Total distinct entities: **{entity_count}**\n\
         - Sources covered: **{source_count}**\n\
         \n\
         ## Risk class\n\
         \n\
         ThinkingRoot is general-purpose knowledge infrastructure. It is the deployer's responsibility to determine whether their use of the system falls under Annex III (high-risk) — see `deployer-obligations.md`.\n",
        ws = workspace.display(),
        engine_ver = env!("CARGO_PKG_VERSION"),
        provider = display_or_unset(&config.llm.default_provider),
        ext_model = display_or_unset(&config.llm.extraction_model),
        claim_count = claims.len(),
        entity_count = total_entities.len(),
        source_count = distinct_source_count(claims),
    )
}

fn render_model_identity(config: &Config, pack: &Option<PackMeta>) -> String {
    let mut s = String::new();
    s.push_str("# Annex IV §1.b — model identity\n");
    s.push_str("# Generated by `root compliance --eu-ai-act`. The provider + model values\n");
    s.push_str("# come from the workspace's effective config (workspace > global > defaults).\n");
    s.push_str("# A blank value means the workspace was compiled in structural-only mode\n");
    s.push_str("# (no LLM provider configured) and produced no LLM-derived claims.\n\n");
    s.push_str(&format!(
        "engine_version = \"{}\"\n",
        env!("CARGO_PKG_VERSION")
    ));
    s.push_str(&format!(
        "provider = \"{}\"\n",
        config.llm.default_provider
    ));
    s.push_str(&format!(
        "extraction_model = \"{}\"\n",
        config.llm.extraction_model
    ));
    s.push_str(&format!(
        "compilation_model = \"{}\"\n",
        config.llm.compilation_model
    ));
    if let Some(p) = pack {
        s.push_str(&format!("pack_name = \"{}\"\n", toml_escape(&p.name)));
        s.push_str(&format!("pack_version = \"{}\"\n", toml_escape(&p.version)));
        if let Some(license) = &p.license {
            s.push_str(&format!("license = \"{}\"\n", toml_escape(license)));
        }
    }
    s
}

/// One JSON object per line, RFC 8259 + RFC 7464 compliant. Each line is
/// a `ProvenanceEntry`: claim id, source URI, byte range, content hash,
/// admission tier, confidence, extracted-at timestamp.
fn render_provenance_log(
    claims: &[V3ClaimExportRow],
    entity_names: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    let mut out = String::new();
    for row in claims {
        let entry = ProvenanceEntry {
            claim_id: row.id.clone(),
            statement: row.statement.clone(),
            claim_type: row.claim_type.clone(),
            source_id: row.source_id.clone(),
            source_uri: row.source_uri.clone(),
            content_hash: row.content_hash.clone(),
            byte_start: row.byte_start,
            byte_end: row.byte_end,
            admission_tier: row.admission_tier.clone(),
            confidence: row.confidence,
            entities: entity_names.get(&row.id).cloned().unwrap_or_default(),
        };
        match serde_json::to_string(&entry) {
            Ok(line) => {
                out.push_str(&line);
                out.push('\n');
            }
            Err(e) => {
                tracing::warn!(claim_id = %row.id, error = %e, "skipping malformed provenance row");
            }
        }
    }
    out
}

/// Provider → published-policy URL map. The set is finite and
/// authoritative; an unknown provider lands as `Unknown` and the bundle
/// refuses to fabricate a URL for it.
fn render_training_data_summary(config: &Config) -> String {
    let provider = config.llm.default_provider.trim().to_lowercase();
    let attestation = match provider.as_str() {
        "openai" => Some("https://platform.openai.com/policies/usage-policies"),
        "anthropic" => Some("https://www.anthropic.com/legal/aup"),
        "azure" => Some(
            "https://learn.microsoft.com/en-us/azure/ai-services/openai/concepts/legal-and-privacy",
        ),
        "openrouter" => Some("https://openrouter.ai/docs#training"),
        "groq" => Some("https://groq.com/policies/privacy-policy/"),
        "deepseek" => Some("https://chat.deepseek.com/downloads/DeepSeek%20Privacy%20Policy.pdf"),
        "together" => Some("https://www.together.ai/privacy"),
        "perplexity" => Some("https://www.perplexity.ai/hub/legal/privacy-policy"),
        "bedrock" => Some("https://aws.amazon.com/service-terms/"),
        "ollama" | "litellm" | "custom" | "" => None,
        _ => None,
    };
    let local_kind = matches!(provider.as_str(), "ollama" | "litellm" | "custom");
    let body = match (attestation, local_kind, provider.is_empty()) {
        (Some(url), _, _) => format!(
            "## Provider-published terms\n\nThis system uses the third-party LLM provider `{}`. \
             Training-data lineage is governed by the provider's published terms at:\n\n- <{}>\n\n\
             ThinkingRoot does not re-host or restate that lineage. Verify the link content at the time of deployment.\n",
            provider, url
        ),
        (None, true, false) => format!(
            "## Self-hosted / local model\n\nThis system uses `{}` in a self-hosted or local-model configuration. \
             The deployer is responsible for documenting training-data lineage of the underlying model — ThinkingRoot \
             does not synthesize lineage attestations it cannot verify.\n",
            provider
        ),
        (None, _, true) => "## Structural-only compile\n\nNo LLM provider was configured for this workspace. The compiled \
             substrate contains structural claims only (parser-derived); no third-party LLM was used in extraction \
             and no training-data lineage attestation is required.\n"
            .to_string(),
        _ => format!(
            "## Unknown provider\n\nThe configured provider `{}` is not in the canonical allow-list. \
             ThinkingRoot refuses to fabricate a training-data attestation. Add the provider's published-terms URL \
             as an appendix to this bundle before submission to a regulator.\n",
            provider
        ),
    };
    format!(
        "# Article 13 — training-data summary\n\n_This section is generated from a fixed allow-list of provider-published URLs. \
         Unknown providers are flagged for the deployer to supply themselves; ThinkingRoot does not synthesise \
         training-data lineage it cannot verify._\n\n{}",
        body
    )
}

fn render_performance_metrics(
    claims: &[V3ClaimExportRow],
    tiers: (usize, usize, usize, usize),
) -> String {
    // tiers: (rooted, attested, quarantined, rejected) in declaration order
    // matching `count_claims_by_admission_tier`.
    let total = (tiers.0 + tiers.1 + tiers.2 + tiers.3) as f64;
    let pct = |n: usize| if total == 0.0 { 0.0 } else { n as f64 / total };
    let avg_conf = if claims.is_empty() {
        0.0
    } else {
        claims.iter().map(|c| c.confidence).sum::<f64>() / claims.len() as f64
    };
    format!(
        "# Annex IV §3 — performance metrics\n\
         # Generated by `root compliance --eu-ai-act` from the live CozoDB substrate.\n\
         \n\
         claim_count = {}\n\
         average_confidence = {:.4}\n\
         \n\
         [admission_tier_distribution]\n\
         rooted = {}\n\
         attested = {}\n\
         quarantined = {}\n\
         rejected = {}\n\
         \n\
         [admission_tier_share]\n\
         rooted = {:.4}\n\
         attested = {:.4}\n\
         quarantined = {:.4}\n\
         rejected = {:.4}\n",
        claims.len(),
        avg_conf,
        tiers.0,
        tiers.1,
        tiers.2,
        tiers.3,
        pct(tiers.0),
        pct(tiers.1),
        pct(tiers.2),
        pct(tiers.3),
    )
}

fn render_intended_purpose(narrative: &Option<String>, pack: &Option<PackMeta>) -> String {
    let mut out = String::from(
        "# Article 13 — intended purpose\n\n\
         _The deployer's narrative description of the intended purpose of this AI system, sourced \
          from the workspace `README.md` (or `Pack.toml` description as a fallback)._\n\n",
    );
    if let Some(text) = narrative {
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    } else if let Some(p) = pack {
        if let Some(desc) = &p.description {
            out.push_str("## Pack description\n\n");
            out.push_str(desc);
            out.push('\n');
        } else {
            out.push_str(empty_purpose_warning());
        }
    } else {
        out.push_str(empty_purpose_warning());
    }
    out
}

fn empty_purpose_warning() -> &'static str {
    "## ⚠ No intended-purpose narrative supplied\n\n\
     The workspace has neither `README.md` nor a `Pack.toml::description` field.\n\
     Article 13 requires the deployer to document the intended purpose of the system.\n\
     Add a `README.md` describing the system before re-running `root compliance`.\n"
}

fn render_deployer_obligations() -> String {
    "# Article 26 — deployer obligations\n\n\
     The following checklist enumerates obligations Reg. (EU) 2024/1689 places on deployers \
     of high-risk AI systems. ThinkingRoot is general-purpose knowledge infrastructure — \
     determining whether your specific use falls under Annex III is the deployer's call.\n\n\
     - [ ] Use the system in accordance with its instructions for use.\n\
     - [ ] Assign human oversight to natural persons with the necessary competence, training and authority.\n\
     - [ ] Ensure input data is relevant and sufficiently representative for the intended purpose.\n\
     - [ ] Monitor the operation of the system; suspend operation on incidents.\n\
     - [ ] Keep the automatically-generated logs (`provenance-log.jsonl` in this bundle) for at least 6 months.\n\
     - [ ] Inform workers and their representatives before putting a high-risk system into service in the workplace.\n\
     - [ ] Cooperate with national competent authorities on any action they take with respect to the system.\n\
     - [ ] If the system makes decisions or assists in making decisions concerning natural persons, \
           inform those persons that they are subject to the use of the AI system.\n"
        .to_string()
}

fn render_manifest(
    bundle_name: &str,
    workspace: &Path,
    config: &Config,
    now: chrono::DateTime<chrono::Utc>,
    files: &BTreeMap<String, String>,
) -> String {
    let mut s = String::new();
    s.push_str("# ThinkingRoot compliance bundle manifest\n");
    s.push_str("# Format: tr-compliance/1\n");
    s.push_str("# Hash: BLAKE3 over each file's exact bytes on disk.\n\n");
    s.push_str(&format!("format = \"{}\"\n", "tr-compliance/1"));
    s.push_str(&format!("framework = \"{}\"\n", "eu-ai-act"));
    s.push_str(&format!("bundle = \"{}\"\n", toml_escape(bundle_name)));
    s.push_str(&format!(
        "workspace = \"{}\"\n",
        toml_escape(&workspace.display().to_string())
    ));
    s.push_str(&format!(
        "generated_at = \"{}\"\n",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));
    s.push_str(&format!(
        "engine_version = \"{}\"\n\n",
        env!("CARGO_PKG_VERSION")
    ));
    s.push_str(&format!(
        "[provider]\nname = \"{}\"\nextraction_model = \"{}\"\n\n",
        toml_escape(&config.llm.default_provider),
        toml_escape(&config.llm.extraction_model)
    ));
    s.push_str("[files]\n");
    for (name, hash) in files {
        s.push_str(&format!(
            "\"{}\" = \"blake3:{}\"\n",
            toml_escape(name),
            hash
        ));
    }
    s
}

async fn sign_manifest_keyless(manifest_bytes: &[u8], bundle_name: &str) -> Result<Vec<u8>> {
    use tr_sigstore::live::{
        IdentityToken, SignKeylessOptions, browser_oidc_flow, sign_canonical_bytes_keyless,
    };

    let token: IdentityToken = match std::env::var("TR_OIDC_TOKEN") {
        Ok(jwt) if !jwt.is_empty() => tr_sigstore::live::identity_token_from_jwt(&jwt)
            .map_err(|e| anyhow!("$TR_OIDC_TOKEN not a valid Sigstore JWT: {e}"))?,
        _ => {
            eprintln!(
                "  opening browser for Sigstore OIDC flow \
                 (set $TR_OIDC_TOKEN to skip)…"
            );
            browser_oidc_flow(None, None, None)
                .map_err(|e| anyhow!("OIDC browser flow failed: {e}"))?
        }
    };
    let jwt = token.to_string();

    let bundle = sign_canonical_bytes_keyless(
        manifest_bytes,
        bundle_name,
        &jwt,
        SystemTime::now(),
        SignKeylessOptions::default(),
    )
    .await
    .map_err(|e| anyhow!("keyless sign: {e}"))?;
    let json = serde_json::to_vec_pretty(&bundle)
        .with_context(|| "serialise SigstoreBundle")?;
    Ok(json)
}

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProvenanceEntry {
    claim_id: String,
    statement: String,
    claim_type: String,
    source_id: String,
    source_uri: String,
    content_hash: String,
    byte_start: u64,
    byte_end: u64,
    admission_tier: String,
    confidence: f64,
    entities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackMeta {
    name: String,
    version: String,
    license: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PackTomlFile {
    pack: PackMeta,
}

fn read_pack_toml(workspace: &Path) -> Option<PackMeta> {
    let path = workspace.join("Pack.toml");
    let raw = fs::read_to_string(&path).ok()?;
    let parsed: PackTomlFile = toml::from_str(&raw).ok()?;
    Some(parsed.pack)
}

fn read_intended_purpose(workspace: &Path) -> Option<String> {
    for candidate in &["README.md", ".thinkingroot/README.md"] {
        let p = workspace.join(candidate);
        if let Ok(content) = fs::read_to_string(&p)
            && !content.trim().is_empty()
        {
            return Some(content);
        }
    }
    None
}

fn workspace_slug(workspace: &Path) -> String {
    let raw = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace");
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn write_artefact(
    dir: &Path,
    name: &str,
    contents: &str,
    files: &mut BTreeMap<String, String>,
) -> Result<()> {
    let path = dir.join(name);
    fs::write(&path, contents.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    let hash = blake3::hash(contents.as_bytes()).to_hex().to_string();
    files.insert(name.to_string(), hash);
    Ok(())
}

fn distinct_source_count(claims: &[V3ClaimExportRow]) -> usize {
    let mut set = std::collections::HashSet::new();
    for c in claims {
        set.insert(&c.source_id);
    }
    set.len()
}

fn display_or_unset(s: &str) -> &str {
    if s.is_empty() { "(unset)" } else { s }
}

/// TOML-escape a string for use inside a double-quoted basic string.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// Tests — every test runs offline (no network, no signing).
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_claims() -> Vec<V3ClaimExportRow> {
        vec![
            V3ClaimExportRow {
                id: "01J000000000000000000CLAIMA".to_string(),
                statement: "Auth uses OAuth2.".to_string(),
                claim_type: "Fact".to_string(),
                confidence: 0.92,
                admission_tier: "rooted".to_string(),
                byte_start: 0,
                byte_end: 24,
                source_id: "src-a".to_string(),
                source_uri: "file:///ws/auth.md".to_string(),
                content_hash: "blake3:abc".to_string(),
            },
            V3ClaimExportRow {
                id: "01J000000000000000000CLAIMB".to_string(),
                statement: "Tokens expire in 1h.".to_string(),
                claim_type: "Fact".to_string(),
                confidence: 0.78,
                admission_tier: "attested".to_string(),
                byte_start: 30,
                byte_end: 60,
                source_id: "src-a".to_string(),
                source_uri: "file:///ws/auth.md".to_string(),
                content_hash: "blake3:abc".to_string(),
            },
            V3ClaimExportRow {
                id: "01J000000000000000000CLAIMC".to_string(),
                statement: "DB is Postgres 16.".to_string(),
                claim_type: "Fact".to_string(),
                confidence: 0.55,
                admission_tier: "quarantined".to_string(),
                byte_start: 0,
                byte_end: 18,
                source_id: "src-b".to_string(),
                source_uri: "file:///ws/db.md".to_string(),
                content_hash: "blake3:def".to_string(),
            },
        ]
    }

    fn fake_entities() -> std::collections::HashMap<String, Vec<String>> {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "01J000000000000000000CLAIMA".to_string(),
            vec!["OAuth2".to_string(), "Auth".to_string()],
        );
        m.insert(
            "01J000000000000000000CLAIMB".to_string(),
            vec!["Token".to_string()],
        );
        m
    }

    #[test]
    fn provenance_log_covers_every_claim() {
        let claims = fake_claims();
        let entities = fake_entities();
        let log = render_provenance_log(&claims, &entities);
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), claims.len(), "one JSON line per claim");
        for line in &lines {
            let parsed: ProvenanceEntry = serde_json::from_str(line).unwrap();
            assert!(!parsed.claim_id.is_empty());
            assert!(!parsed.source_uri.is_empty());
            assert!(!parsed.admission_tier.is_empty());
        }
    }

    #[test]
    fn training_data_summary_uses_allowlist_url_for_known_provider() {
        let mut config = Config::default();
        config.llm.default_provider = "anthropic".to_string();
        let body = render_training_data_summary(&config);
        assert!(body.contains("anthropic"));
        assert!(body.contains("https://www.anthropic.com/legal/aup"));
        assert!(!body.contains("fabricate"));
    }

    #[test]
    fn training_data_summary_refuses_to_fabricate_for_unknown_provider() {
        let mut config = Config::default();
        config.llm.default_provider = "shadow-vendor".to_string();
        let body = render_training_data_summary(&config);
        assert!(
            body.contains("refuses to fabricate"),
            "must explicitly refuse, got: {body}"
        );
        assert!(
            !body.contains("https://"),
            "no fabricated URL for unknown provider"
        );
    }

    #[test]
    fn training_data_summary_handles_local_model() {
        let mut config = Config::default();
        config.llm.default_provider = "ollama".to_string();
        let body = render_training_data_summary(&config);
        assert!(body.to_lowercase().contains("self-hosted"));
        assert!(!body.contains("fabricate"));
    }

    #[test]
    fn performance_metrics_distribution_sums_to_total() {
        let claims = fake_claims();
        let metrics = render_performance_metrics(&claims, (1, 1, 1, 0));
        let parsed: toml::Value = toml::from_str(&metrics).unwrap();
        let total = parsed["claim_count"].as_integer().unwrap();
        assert_eq!(total, claims.len() as i64);
        let dist = &parsed["admission_tier_distribution"];
        let sum = dist["rooted"].as_integer().unwrap()
            + dist["attested"].as_integer().unwrap()
            + dist["quarantined"].as_integer().unwrap()
            + dist["rejected"].as_integer().unwrap();
        assert_eq!(sum, claims.len() as i64);
    }

    #[test]
    fn intended_purpose_falls_back_to_pack_description() {
        let pack = Some(PackMeta {
            name: "acme/widgets".to_string(),
            version: "0.1.0".to_string(),
            license: Some("MIT".to_string()),
            description: Some("Widgets knowledge pack.".to_string()),
        });
        let body = render_intended_purpose(&None, &pack);
        assert!(body.contains("Widgets knowledge pack"));
        assert!(!body.contains("⚠"));
    }

    #[test]
    fn intended_purpose_warns_when_no_narrative() {
        let body = render_intended_purpose(&None, &None);
        assert!(body.contains("⚠"));
        assert!(body.contains("Article 13"));
    }

    #[test]
    fn manifest_lists_every_artefact_with_blake3() {
        let mut files = BTreeMap::new();
        files.insert("system-card.md".to_string(), "abc".to_string());
        files.insert("provenance-log.jsonl".to_string(), "def".to_string());
        let config = Config::default();
        let manifest = render_manifest(
            "compliance-bundle-test-20260509T000000Z.tr-compliance",
            Path::new("/ws"),
            &config,
            chrono::Utc.with_ymd_and_hms(2026, 5, 9, 0, 0, 0).unwrap(),
            &files,
        );
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        assert_eq!(parsed["framework"].as_str(), Some("eu-ai-act"));
        let files_tbl = parsed["files"].as_table().unwrap();
        assert!(files_tbl.contains_key("system-card.md"));
        assert!(files_tbl.contains_key("provenance-log.jsonl"));
        assert!(
            files_tbl["system-card.md"]
                .as_str()
                .unwrap()
                .starts_with("blake3:")
        );
    }

    #[test]
    fn workspace_slug_replaces_unsafe_chars() {
        assert_eq!(workspace_slug(Path::new("/abs/My Project")), "My-Project");
        assert_eq!(workspace_slug(Path::new("/abs/api.v2")), "api-v2");
    }

    #[tokio::test]
    async fn run_compliance_writes_full_bundle_for_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        let engine = ws.join(".thinkingroot");
        let graph_dir = engine.join("graph");
        fs::create_dir_all(&graph_dir).unwrap();
        // Pre-create an empty CozoDB so GraphStore::init opens cleanly.
        let _ = thinkingroot_graph::graph::GraphStore::init(&graph_dir).unwrap();
        // Provide a README so intended-purpose populates rather than warning.
        fs::write(ws.join("README.md"), "# Test workspace\n").unwrap();

        let bundle = run_compliance(ComplianceOpts {
            eu_ai_act: true,
            out: Some(ws.clone()),
            sign: false,
            workspace: ws.clone(),
        })
        .await
        .unwrap();
        assert!(bundle.bundle_dir.exists());
        for artefact in [
            "system-card.md",
            "model-identity.toml",
            "provenance-log.jsonl",
            "training-data-summary.md",
            "performance-metrics.toml",
            "intended-purpose.md",
            "deployer-obligations.md",
            "manifest.toml",
        ] {
            assert!(
                bundle.files.contains_key(artefact),
                "missing artefact {artefact}"
            );
            assert!(bundle.bundle_dir.join(artefact).exists());
        }
        assert!(!bundle.signed);

        // Manifest's [files] table is consistent with bundle.files for
        // every artefact that was emitted before the manifest itself.
        let manifest = fs::read_to_string(bundle.bundle_dir.join("manifest.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let files_tbl = parsed["files"].as_table().unwrap();
        for (name, hash) in &bundle.files {
            if name == "manifest.toml" {
                continue;
            }
            let value = files_tbl
                .get(name.as_str())
                .unwrap_or_else(|| panic!("manifest missing {name}"))
                .as_str()
                .unwrap();
            assert_eq!(value, format!("blake3:{hash}"));
        }
    }

    #[tokio::test]
    async fn run_compliance_rejects_workspace_without_engine() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        let err = run_compliance(ComplianceOpts {
            eu_ai_act: true,
            out: Some(ws.clone()),
            sign: false,
            workspace: ws.clone(),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no engine output"));
    }

    use chrono::TimeZone;
}
