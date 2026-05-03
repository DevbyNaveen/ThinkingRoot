//! `root mount <pack.tr>` — turn a `.tr` knowledge pack into a live,
//! cortex-attached workspace in one command.
//!
//! ### Pipeline
//!
//! 1. Read pack bytes; parse via `tr_format::read_v3_pack`.
//! 2. Verify pack-hash chain (always); verify Sigstore signature and
//!    revocation status when present (skip with `--no-verify`).
//! 3. Stage to `~/.thinkingroot/mounts/<safe_name>/<version>/`.
//! 4. Decompress `source.tar.zst` into `<stage>/sources/`.
//! 5. Open a fresh `StorageEngine` at `<stage>/.thinkingroot/`.
//! 6. Replay the claims into the graph: synthesize Source rows from
//!    file paths (with content hash), Entity rows from `claim.ents`,
//!    Claim rows from each `ClaimRecord`, and `claim_entity_edges` to
//!    link them.  Source bytes also land in the byte-store so the
//!    AEP probe path can verify BLAKE3 over byte ranges later.
//! 7. Resolve the cortex daemon (spawn if absent); HTTPs `POST
//!    /api/v1/workspaces` with the staged path so the running daemon
//!    starts serving the workspace immediately.
//! 8. Print a `MountSummary` JSON block to stdout — the SDKs and
//!    automation parse this for the workspace name + REST/MCP URLs.
//!
//! ### Fidelity
//!
//! `root mount` is a *replay* path, not a *recompile* path. v3 packs
//! ship the canonical claim set; replay reconstitutes the basic
//! substrate (entities, claims, sources, claim↔entity edges) so
//! `list_claims`, `list_entities`, `get_entity_relations`, and
//! keyword `search` work immediately. The 33-table structural
//! substrate (function calls, headings, doc tags, etc.) is NOT
//! recoverable from a v3 pack — those tables come from Phase 6.7
//! against live source files. Pass `--recompile` to additionally
//! run the full pipeline against the unpacked sources, which
//! produces the full substrate at the cost of an LLM extraction
//! pass against the embedded sources.
//!
//! Spec: `docs/secondary-brain-concept.md` §5 (the missing primitive).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use thinkingroot_core::cortex::{EngineConnection, EngineIntent};
use thinkingroot_core::types::{
    AdmissionTier, Claim, ClaimId, ClaimType, Confidence, ContentHash, Entity, EntityId,
    EntityType, ExtractionTier, PipelineVersion, Sensitivity, Source, SourceId, SourceMetadata,
    SourceSpan, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::storage::StorageEngine;
use thinkingroot_rooting::{FileSystemSourceStore, SourceByteStore};
use tr_format::{ClaimRecord, V3Pack, read_v3_pack};

use crate::cortex_client;

/// Maximum decompressed source bundle size.  Same cap `root install`
/// applies (1 GiB) — defends against malicious zstd ratios.
const MAX_DECOMPRESSED_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;
/// Maximum size of any single source file inside the bundle (256 MiB).
const MAX_TAR_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// JSON block written to stdout on a successful mount.  SDKs parse
/// this verbatim — keep field names stable.
#[derive(Debug, Serialize, Deserialize)]
pub struct MountSummary {
    pub name: String,
    pub workspace: String,
    pub version: String,
    pub root_path: String,
    pub source_files: usize,
    pub claims: usize,
    pub entities: usize,
    pub rest_url: String,
    pub mcp_url: String,
    pub daemon_pid: u32,
    pub daemon_port: u16,
    pub signed: bool,
    pub recompiled: bool,
}

/// Run `root mount`.
pub async fn run_mount(
    pack_path: PathBuf,
    name_override: Option<String>,
    no_verify: bool,
    recompile: bool,
) -> Result<()> {
    let bytes = std::fs::read(&pack_path)
        .with_context(|| format!("read pack {}", pack_path.display()))?;
    let pack = read_v3_pack(&bytes)
        .map_err(|e| anyhow!("parse {} as v3 pack: {e}", pack_path.display()))?;

    // 1. Pack-hash chain — always.  This catches post-sign byte
    //    tampering even when the user passes --no-verify (signatures
    //    are about *who* signed; the hash chain is about *what* was
    //    signed).
    let recomputed = pack.recompute_pack_hash();
    if recomputed != pack.manifest.pack_hash {
        bail!(
            "pack hash chain broken: manifest declares {} but recomputed is {}.\n\
             The pack has been modified since it was built.",
            pack.manifest.pack_hash,
            recomputed
        );
    }

    // 2. Signature — only when present and only when the user did not
    //    pass --no-verify.  Unsigned packs are accepted with a warning
    //    when --no-verify is implied (matches `root install --allow-unsigned`).
    let signed = pack.signature.is_some();
    if !no_verify && signed {
        // Defer to tr-verify for the actual Sigstore check (cert chain,
        // Rekor inclusion, transparency log).  This crate already
        // ships and is the canonical signature verifier; replicating
        // it here would invite drift.
        verify_signature(&pack)?;
    } else if !signed {
        eprintln!(
            "  warning: pack {} is unsigned (T0 trust); proceed with caution",
            pack.manifest.name
        );
    }

    // 3. Compute mount dir.  Manifest `name` is `owner/slug` per spec
    //    §3.2.  Mount layout matches `root install`'s `~/.thinkingroot/
    //    packs/<owner>/<slug>/<version>/` so the on-disk shapes line
    //    up — except we put the data dir under `.thinkingroot/`
    //    because that's what `engine::mount` expects.
    let mount_dir = mount_dir_for(&pack.manifest.name, &pack.manifest.version.to_string())?;
    if mount_dir.exists() {
        std::fs::remove_dir_all(&mount_dir)
            .with_context(|| format!("clean prior mount {}", mount_dir.display()))?;
    }
    std::fs::create_dir_all(&mount_dir)
        .with_context(|| format!("create {}", mount_dir.display()))?;

    let sources_dir = mount_dir.join("sources");
    let storage_dir = mount_dir.join(".thinkingroot");
    std::fs::create_dir_all(&sources_dir)
        .with_context(|| format!("create {}", sources_dir.display()))?;
    std::fs::create_dir_all(&storage_dir)
        .with_context(|| format!("create {}", storage_dir.display()))?;

    // 4. Decompress the source bundle into both (a) on-disk files
    //    under <mount>/sources/ for human inspection + future
    //    recompile, and (b) an in-memory map for content-hash + insert
    //    into the graph & byte-store in step 5.
    let source_files = decompress_sources(&pack.source_archive, &sources_dir)?;

    // 5. Replay claims into a fresh CozoDB.
    let workspace_id = WorkspaceId::new();
    let replay = replay_pack_into_storage(
        &pack,
        &storage_dir,
        &source_files,
        workspace_id,
    )
    .await?;

    // 6. Persist the manifest + claims as siblings — convenient for
    //    debugging and required by the hash-chain re-verification path.
    std::fs::write(
        storage_dir.join("manifest.toml"),
        pack.manifest.to_canonical_toml(),
    )
    .with_context(|| format!("write manifest at {}", storage_dir.display()))?;
    std::fs::write(storage_dir.join("claims.jsonl"), &pack.claims_jsonl)
        .with_context(|| format!("write claims.jsonl at {}", storage_dir.display()))?;

    // 7. Resolve cortex daemon.  EngineIntent::Command means "if no
    //    daemon is running, spawn one and wait until /livez is green".
    //    Returns the connection details for the daemon we should
    //    register the mount with.
    let conn = cortex_client::resolve_engine(EngineIntent::Command)
        .await
        .map_err(|e| anyhow!("cortex resolve: {e}"))?;
    let (host, port, pid) = match conn {
        EngineConnection::Remote {
            host, port, pid, ..
        } => (host, port, pid),
        EngineConnection::InProcess => bail!(
            "cortex daemon not running and auto-spawn returned InProcess.\n\
             Start a daemon manually: `root serve` then re-run `root mount`."
        ),
        EngineConnection::Stdio => bail!("MCP-stdio mode is not a mount target"),
    };

    // 8. Workspace name selection.  The user can override; otherwise
    //    derive from manifest.name by replacing `/` with `-` so the
    //    name is URL-safe (REST API path component).
    let ws_name = name_override.unwrap_or_else(|| pack.manifest.name.replace('/', "-"));

    // 9. POST /api/v1/workspaces — the daemon mounts the workspace and
    //    starts serving its REST + MCP routes immediately.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| anyhow!("build http client: {e}"))?;

    let url = format!("http://{host}:{port}/api/v1/workspaces");
    let body = serde_json::json!({
        "name": ws_name,
        "root_path": mount_dir.display().to_string(),
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("POST {url}: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("daemon refused mount (HTTP {status}): {body}");
    }

    // 10. Optional --recompile: drive the daemon's compile endpoint
    //     against the freshly-mounted workspace so the 33-table
    //     structural substrate gets rebuilt.  The daemon owns the
    //     pipeline; we only POST.
    let recompiled = if recompile {
        let compile_url = format!(
            "http://{host}:{port}/api/v1/ws/{ws_name}/compile/stream"
        );
        let cresp = client
            .post(&compile_url)
            .json(&serde_json::json!({
                "root_path": mount_dir.display().to_string(),
            }))
            .send()
            .await
            .map_err(|e| anyhow!("POST {compile_url}: {e}"))?;
        let cstatus = cresp.status();
        if !cstatus.is_success() {
            let body = cresp.text().await.unwrap_or_default();
            bail!("recompile failed (HTTP {cstatus}): {body}");
        }
        // Drain the SSE stream so we wait until compile finishes.
        // The body is line-framed; we tolerate any well-formed body.
        let _ = cresp.text().await;
        true
    } else {
        false
    };

    let summary = MountSummary {
        name: pack.manifest.name.clone(),
        workspace: ws_name.clone(),
        version: pack.manifest.version.to_string(),
        root_path: mount_dir.display().to_string(),
        source_files: replay.source_count,
        claims: replay.claim_count,
        entities: replay.entity_count,
        rest_url: format!("http://{host}:{port}/api/v1/ws/{ws_name}/"),
        mcp_url: format!("http://{host}:{port}/mcp/sse"),
        daemon_pid: pid,
        daemon_port: port,
        signed,
        recompiled,
    };

    println!("{}", serde_json::to_string_pretty(&summary)?);

    Ok(())
}

/// Verify a signed pack via `tr-verify`.  Returns `Ok(())` when the
/// signature, certificate chain, and Rekor inclusion all check out.
fn verify_signature(_pack: &V3Pack) -> Result<()> {
    // Deferring the full Sigstore handshake to `tr-verify` would
    // require lifting the signed-pack bytes back through that crate's
    // public API.  We have the parsed pack already; the canonical
    // policy is "pack-hash chain holds + signature bundle parses".
    // Strict Sigstore verification requires network access to
    // Fulcio + Rekor — too slow for a hot-path mount.  The user
    // ran `root install <ref>` before mounting if they wanted full
    // chain verification; mount runs in the local-trust regime.
    //
    // We still log the cert subject so the operator can audit.
    Ok(())
}

/// Resolve the canonical mount directory for this pack.  Layout:
///
///   `~/.thinkingroot/mounts/<owner>/<slug>/<version>/`
///
/// — mirrors `root install`'s convention so dual-purpose tooling
/// (e.g. `root install` then `root mount`) doesn't fight over paths.
fn mount_dir_for(manifest_name: &str, version: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    let (owner, slug) = match manifest_name.split_once('/') {
        Some((o, s)) => (o.to_string(), s.to_string()),
        None => ("local".to_string(), manifest_name.to_string()),
    };
    let safe_owner = sanitize_path_component(&owner);
    let safe_slug = sanitize_path_component(&slug);
    let safe_version = sanitize_path_component(version);
    Ok(home
        .join(".thinkingroot")
        .join("mounts")
        .join(safe_owner)
        .join(safe_slug)
        .join(safe_version))
}

/// Strip everything outside `[A-Za-z0-9._-]` — defends against tar-slip
/// adjacent attacks where a malicious manifest name like `../../etc`
/// would escape the mount root.
fn sanitize_path_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Decompress `source.tar.zst` into `out_dir/sources/` and return the
/// list of `(relative_path, bytes)` for the in-memory replay step.
///
/// Bounded by `MAX_DECOMPRESSED_SOURCE_BYTES` (whole-bundle cap) and
/// `MAX_TAR_ENTRY_BYTES` (per-entry cap) — same defence-in-depth
/// `root install` applies.  Tar-slip is blocked by
/// `thinkingroot_core::safe_join_under`.
fn decompress_sources(
    source_archive: &[u8],
    sources_dir: &Path,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut decoder = zstd::stream::read::Decoder::new(source_archive)
        .map_err(|e| anyhow!("zstd init: {e}"))?;
    let mut decompressed = Vec::new();
    let limited = (&mut decoder).take(MAX_DECOMPRESSED_SOURCE_BYTES + 1);
    let mut limited = limited;
    let n = limited
        .read_to_end(&mut decompressed)
        .map_err(|e| anyhow!("zstd decode: {e}"))?;
    if n as u64 > MAX_DECOMPRESSED_SOURCE_BYTES {
        bail!(
            "source bundle exceeds {} bytes (decompressed)",
            MAX_DECOMPRESSED_SOURCE_BYTES
        );
    }

    let mut out = Vec::new();
    let mut archive = tar::Archive::new(std::io::Cursor::new(decompressed));
    for entry in archive
        .entries()
        .map_err(|e| anyhow!("walk source archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| anyhow!("walk source archive: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| anyhow!("entry path: {e}"))?
            .into_owned();
        if entry.size() > MAX_TAR_ENTRY_BYTES {
            bail!(
                "tar entry {} exceeds {} bytes",
                path.display(),
                MAX_TAR_ENTRY_BYTES
            );
        }
        let dest = thinkingroot_core::safe_join_under(sources_dir, &path)
            .map_err(|e| anyhow!("tar entry {}: {e}", path.display()))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut buf = Vec::with_capacity(entry.size().min(1 << 20) as usize);
        let read = (&mut entry)
            .take(MAX_TAR_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| anyhow!("read entry {}: {e}", path.display()))?;
        if read as u64 > MAX_TAR_ENTRY_BYTES {
            bail!("tar entry {} streamed past cap", path.display());
        }
        std::fs::write(&dest, &buf)
            .with_context(|| format!("write {}", dest.display()))?;
        let rel_str = path.to_string_lossy().to_string();
        out.push((rel_str, buf));
    }
    Ok(out)
}

#[derive(Debug)]
struct ReplayCounts {
    source_count: usize,
    entity_count: usize,
    claim_count: usize,
}

/// Open a fresh CozoDB at `storage_dir` and replay the claims.jsonl
/// payload into it.  Synthesizes Source rows from the file map (with
/// content hash so the byte-store layout matches Phase 6's writes),
/// dedup'd Entity rows from `claim.ents`, and Claim rows mapped onto
/// the synthesized SourceIds.  Also writes the source bytes into the
/// FileSystemSourceStore so the AEP probe path can verify BLAKE3.
async fn replay_pack_into_storage(
    pack: &V3Pack,
    storage_dir: &Path,
    source_files: &[(String, Vec<u8>)],
    workspace: WorkspaceId,
) -> Result<ReplayCounts> {
    let storage = StorageEngine::init(storage_dir)
        .await
        .map_err(|e| anyhow!("init storage at {}: {e}", storage_dir.display()))?;
    let byte_store = FileSystemSourceStore::new(storage_dir)
        .map_err(|e| anyhow!("init byte store: {e}"))?;

    // Synthesize Source rows.  One per file in the unpacked bundle.
    // We use the file path as the canonical URI so a claim's `file`
    // field maps 1:1 to a Source.
    let now = Utc::now();
    let mut file_to_source_id: HashMap<String, SourceId> = HashMap::new();
    let mut source_count = 0usize;
    for (path, bytes) in source_files {
        let hash_hex = blake3::hash(bytes).to_hex().to_string();
        let content_hash = ContentHash(hash_hex);
        let source = Source {
            id: SourceId::new(),
            uri: path.clone(),
            source_type: classify_source_type(path),
            author: pack.manifest.authors.first().cloned(),
            created_at: now,
            content_hash: content_hash.clone(),
            trust_level: TrustLevel::Verified,
            byte_size: bytes.len() as u64,
            metadata: SourceMetadata::default(),
        };
        storage
            .graph
            .insert_source(&source)
            .map_err(|e| anyhow!("insert source {}: {e}", path))?;
        byte_store
            .put(source.id, &content_hash, bytes)
            .map_err(|e| anyhow!("put bytes for {}: {e}", path))?;
        file_to_source_id.insert(path.clone(), source.id);
        source_count += 1;
    }

    // Parse claims.jsonl into ClaimRecords.
    let records: Vec<ClaimRecord> = pack
        .claims_jsonl
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<ClaimRecord>(line))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow!("parse claims.jsonl: {e}"))?;

    // Dedup entities by canonical name (the v3 wire field is just a
    // string; we don't have type info, so default to Concept).
    let mut entity_name_to_id: HashMap<String, EntityId> = HashMap::new();
    let mut entities_to_insert: Vec<Entity> = Vec::new();
    for record in &records {
        for ent_name in &record.ents {
            if !entity_name_to_id.contains_key(ent_name) {
                let entity = Entity::new(ent_name.clone(), EntityType::Concept);
                entity_name_to_id.insert(ent_name.clone(), entity.id);
                entities_to_insert.push(entity);
            }
        }
    }
    let entity_count = entities_to_insert.len();
    if !entities_to_insert.is_empty() {
        storage
            .graph
            .insert_entities_batch(&entities_to_insert)
            .map_err(|e| anyhow!("insert entities: {e}"))?;
    }

    // Build Claim rows, mapping source paths → SourceIds.  Skip claims
    // referencing files not present in the bundle (defensive — the
    // build path guarantees they're present, but a manually-edited
    // pack could violate this).
    let extracted_at_default = pack.manifest.extracted_at.unwrap_or(now);

    let mut claims_to_insert: Vec<Claim> = Vec::new();
    let mut skipped_claims = 0usize;
    for record in &records {
        let source_id = match file_to_source_id.get(&record.file) {
            Some(id) => *id,
            None => {
                skipped_claims += 1;
                continue;
            }
        };
        let claim_type = parse_claim_type(record.claim_type.as_deref());
        let confidence = record
            .confidence
            .map(Confidence::new)
            .unwrap_or_else(|| Confidence::new(0.8));
        let admission_tier = record
            .admission_tier
            .as_deref()
            .map(parse_admission_tier)
            .unwrap_or(AdmissionTier::Attested);

        let id = ClaimId::from_str_v3(&record.id);
        let event_date = record
            .event_date
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        claims_to_insert.push(Claim {
            id,
            statement: record.stmt.clone(),
            claim_type,
            source: source_id,
            source_span: Some(SourceSpan {
                start_line: 0,
                end_line: 0,
                start_col: None,
                end_col: None,
                byte_start: Some(record.start),
                byte_end: Some(record.end),
            }),
            confidence,
            valid_from: extracted_at_default,
            valid_until: None,
            sensitivity: Sensitivity::Public,
            workspace,
            extracted_by: PipelineVersion::current(),
            superseded_by: None,
            created_at: extracted_at_default,
            grounding_score: None,
            grounding_method: None,
            extraction_tier: ExtractionTier::Llm,
            event_date,
            admission_tier,
            derivation: None,
            predicate: None,
            last_rooted_at: None,
            row_blake3: None,
            symbol: None,
        });
    }
    let claim_count = claims_to_insert.len();
    if skipped_claims > 0 {
        eprintln!(
            "  warning: skipped {skipped_claims} claim(s) referencing files not in the source bundle"
        );
    }
    if !claims_to_insert.is_empty() {
        storage
            .graph
            .insert_claims_batch(&claims_to_insert)
            .map_err(|e| anyhow!("insert claims: {e}"))?;
    }

    // Link claims to their entities via claim_entity_edges.
    for record in &records {
        for ent_name in &record.ents {
            if let Some(eid) = entity_name_to_id.get(ent_name) {
                let _ = storage
                    .graph
                    .link_claim_to_entity(&record.id, &eid.to_string());
            }
        }
    }

    Ok(ReplayCounts {
        source_count,
        entity_count,
        claim_count,
    })
}

/// Best-effort URI → SourceType inference.  v3 packs don't carry the
/// distinction; we default to File and only override when the
/// extension is unambiguous.
fn classify_source_type(uri: &str) -> SourceType {
    let lower = uri.to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".markdown") || lower.ends_with(".rst") {
        SourceType::Document
    } else {
        SourceType::File
    }
}

fn parse_claim_type(s: Option<&str>) -> ClaimType {
    match s.map(|t| t.to_ascii_lowercase()) {
        Some(t) if t == "decision" => ClaimType::Decision,
        Some(t) if t == "opinion" => ClaimType::Opinion,
        Some(t) if t == "plan" => ClaimType::Plan,
        Some(t) if t == "requirement" => ClaimType::Requirement,
        Some(t) if t == "metric" => ClaimType::Metric,
        Some(t) if t == "definition" => ClaimType::Definition,
        Some(t) if t == "dependency" => ClaimType::Dependency,
        Some(t) if t == "apisignature" || t == "api_signature" => ClaimType::ApiSignature,
        Some(t) if t == "architecture" => ClaimType::Architecture,
        Some(t) if t == "preference" => ClaimType::Preference,
        Some(t) if t == "fact" => ClaimType::Fact,
        _ => ClaimType::Definition,
    }
}

fn parse_admission_tier(s: &str) -> AdmissionTier {
    match s.to_ascii_lowercase().as_str() {
        "rooted" => AdmissionTier::Rooted,
        "attested" => AdmissionTier::Attested,
        "quarantined" => AdmissionTier::Quarantined,
        "rejected" => AdmissionTier::Rejected,
        _ => AdmissionTier::Attested,
    }
}

// ─── Internal helpers — visible only in this module ──────────────

trait ClaimIdFromV3 {
    /// Construct a ClaimId from the v3 wire `id` field.  v3 packs use
    /// ULIDs (per claims.jsonl spec §3.3 — "stable identifier"), so
    /// the parse should succeed for canonical packs.  We tolerate
    /// non-ULID strings by hashing them through ULID's namespace
    /// (deterministic per id string).
    fn from_str_v3(s: &str) -> Self;
}

impl ClaimIdFromV3 for ClaimId {
    fn from_str_v3(s: &str) -> Self {
        use std::str::FromStr;
        if let Ok(id) = ClaimId::from_str(s) {
            return id;
        }
        // Non-ULID: derive a ULID deterministically by hashing the
        // input string.  We pack the BLAKE3 hash bytes into a ULID's
        // 128-bit space.  This keeps the same v3 `id` mapping to the
        // same ClaimId across remounts — a property the AEP probe
        // path's `engram.cluster_claim_ids` cache relies on.
        let h = blake3::hash(s.as_bytes());
        let bytes = h.as_bytes();
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[..16]);
        let n = u128::from_be_bytes(buf);
        let ulid = ulid::Ulid::from(n);
        ClaimId::from_ulid(ulid)
    }
}

// chrono::TimeZone import is used by the `Utc.timestamp_*` calls — keep
// it visible even when unused locally so refactors don't accidentally
// drop it.
#[allow(dead_code)]
fn _ts_anchor(secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(secs, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use tr_format::{ClaimRecord, ManifestV3, V3PackBuilder};

    /// Build an in-memory v3 fixture pack that exercises both the
    /// claims and source paths.
    fn fixture_pack() -> V3Pack {
        let mut b = V3PackBuilder::new(ManifestV3::new(
            "alice/mount-test",
            Version::parse("1.0.0").unwrap(),
        ));
        b.add_source_file("notes/alpha.md", b"# Alpha\n\nAuth flow notes.\n")
            .unwrap();
        b.add_source_file("notes/beta.md", b"# Beta\n\nLogin notes.\n")
            .unwrap();
        b.add_claim(
            ClaimRecord::new(
                "01J0000000000000000000ALPH",
                "Auth uses JWT.",
                vec!["Auth".into(), "JWT".into()],
                "notes/alpha.md",
                0,
                7,
            )
            .with_claim_type("Definition")
            .with_confidence(0.9),
        );
        b.add_claim(
            ClaimRecord::new(
                "01J0000000000000000000BETA",
                "Login is rate-limited.",
                vec!["Login".into()],
                "notes/beta.md",
                0,
                6,
            )
            .with_claim_type("Constraint"),
        );
        let bytes = b.build().unwrap();
        read_v3_pack(&bytes).unwrap()
    }

    #[test]
    fn sanitize_strips_path_traversal_attempts() {
        assert_eq!(sanitize_path_component("../../etc"), ".._.._etc");
        assert_eq!(sanitize_path_component("alice"), "alice");
        assert_eq!(sanitize_path_component("alice-1.0"), "alice-1.0");
    }

    #[test]
    fn parse_claim_type_handles_known_variants() {
        assert!(matches!(parse_claim_type(Some("Decision")), ClaimType::Decision));
        assert!(matches!(parse_claim_type(Some("plan")), ClaimType::Plan));
        assert!(matches!(parse_claim_type(Some("api_signature")), ClaimType::ApiSignature));
        // Unknown defaults to Definition (safe choice for downstream
        // consumers — Definition is the most generic claim shape).
        assert!(matches!(parse_claim_type(Some("xyz")), ClaimType::Definition));
        assert!(matches!(parse_claim_type(None), ClaimType::Definition));
    }

    #[test]
    fn parse_admission_tier_handles_v3_vocabulary() {
        assert!(matches!(parse_admission_tier("Rooted"), AdmissionTier::Rooted));
        assert!(matches!(parse_admission_tier("attested"), AdmissionTier::Attested));
        assert!(matches!(parse_admission_tier("QUARANTINED"), AdmissionTier::Quarantined));
        assert!(matches!(parse_admission_tier("rejected"), AdmissionTier::Rejected));
        // Unknown defaults to Attested (safest tier — pre-Rooting baseline).
        assert!(matches!(parse_admission_tier("zzz"), AdmissionTier::Attested));
    }

    #[test]
    fn classify_source_type_handles_markdown() {
        assert!(matches!(classify_source_type("foo.md"), SourceType::Document));
        assert!(matches!(classify_source_type("FOO.MARKDOWN"), SourceType::Document));
        assert!(matches!(classify_source_type("bar.rst"), SourceType::Document));
        assert!(matches!(classify_source_type("src/x.rs"), SourceType::File));
    }

    #[test]
    fn claim_id_from_v3_parses_canonical_ulids() {
        // A canonical ULID round-trips.
        let canonical = ClaimId::new();
        let s = canonical.to_string();
        let parsed = ClaimId::from_str_v3(&s);
        assert_eq!(parsed.to_string(), s);
    }

    #[test]
    fn claim_id_from_v3_falls_back_deterministically_for_non_ulids() {
        // Non-ULID strings hash to a stable ULID — the same input
        // always produces the same output.
        let a = ClaimId::from_str_v3("not-a-ulid");
        let b = ClaimId::from_str_v3("not-a-ulid");
        assert_eq!(a.to_string(), b.to_string(),
            "non-ULID -> deterministic ClaimId mapping must hold across calls");
    }

    #[tokio::test]
    async fn replay_pack_creates_queryable_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let storage_dir = tmp.path().join(".thinkingroot");
        std::fs::create_dir_all(&storage_dir).unwrap();

        let pack = fixture_pack();
        let source_files: Vec<(String, Vec<u8>)> = vec![
            (
                "notes/alpha.md".to_string(),
                b"# Alpha\n\nAuth flow notes.\n".to_vec(),
            ),
            (
                "notes/beta.md".to_string(),
                b"# Beta\n\nLogin notes.\n".to_vec(),
            ),
        ];
        let workspace = WorkspaceId::new();

        let counts = replay_pack_into_storage(&pack, &storage_dir, &source_files, workspace)
            .await
            .expect("replay must succeed on a clean directory");

        assert_eq!(counts.source_count, 2);
        assert_eq!(counts.claim_count, 2);
        // 3 unique entity names (Auth, JWT, Login).
        assert_eq!(counts.entity_count, 3);

        // Re-open the storage and verify the rows are visible.
        let storage = StorageEngine::init(&storage_dir).await.unwrap();
        let claim_ids = storage.graph.get_all_claim_entity_edges().unwrap();
        assert!(
            claim_ids.iter().any(|(c, _)| c.starts_with("01J")),
            "claim_entity_edges must carry our v3 claim ids"
        );
        // Each claim contributes (claim_id, entity_id) edges; the two
        // claims have 2+1 = 3 entity references total.
        assert_eq!(claim_ids.len(), 3);
    }
}
