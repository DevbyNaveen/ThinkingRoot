//! `root pack` and `root install` — the offline halves of the `.tr`
//! distribution loop.
//!
//! ## Why this lives in OSS
//!
//! `.tr` is a portable knowledge pack: a `tar+zstd` container with a
//! manifest at root. The cloud monorepo (thinkingroot-cloud) used to be
//! the only producer (compile-worker → BlobStore) and the only consumer
//! (registry, hub agent runtime). Phase A relocated the
//! [`tr_format`] crate into this OSS repo so any third-party tool —
//! and the OSS engine itself — can read and write `.tr` files without
//! depending on the cloud.
//!
//! Phase B (this module) closes the loop in OSS so users can:
//!
//! 1. Compile a workspace with `root compile` (engine output lands in
//!    `<workspace>/.thinkingroot/`).
//! 2. **Package it** with `root pack` → produces a `.tr` file that any
//!    other OSS instance — or the cloud's registry — can ingest.
//! 3. **Install it** with `root install <file>` → extracts the `.tr`
//!    back to a target dir's `.thinkingroot/` so `root query` /
//!    `root serve` can mount it.
//!
//! The mapping from engine output to in-pack paths is identity: every
//! file under `.thinkingroot/` is preserved at the same relative path
//! inside the `.tr`. Three top-level entries are skipped because they
//! are local-only (`cache/`, `config.toml`, `fingerprints.json`) — they
//! either contain workstation paths, secrets, or rebuilt-from-source
//! caches that bloat the archive without adding value.
//!
//! Trust verification (signatures, revocation) is **out of scope** for
//! this module; it lives behind a follow-up `--require-tier` flag in a
//! later phase. The reader still verifies BLAKE3 of the manifest's
//! canonical bytes (that check is part of [`tr_format::reader`]
//! itself), so a corrupted or tampered `.tr` is rejected at install.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use thinkingroot_core::types::ContentHash;
use tr_format::{
    digest::blake3_hex, reader as tr_reader, read_v3_pack, writer::PackBuilder, ClaimRecord,
    Manifest, ManifestV3, TrustTier, V3PackBuilder, Version,
};
use tr_verify::{V3TamperedKind, V3Verdict, verify_v3_pack};
use tr_revocation::{CacheConfig, RevocationCache};
use tr_verify::{
    AuthorKeyStore, RevokedDetails, TamperedKind, Verdict, Verifier, VerifierConfig,
};

use crate::resolver::{HttpDirectUrlResolver, HttpRegistryResolver, LocalFsResolver, PackResolver};

// -----------------------------------------------------------------------------
// Exit codes for `root install` verdict refusals (Phase F design §2.3).
// -----------------------------------------------------------------------------

/// Pack is unsigned and policy required a signature.
pub const EXIT_UNSIGNED: i32 = 70;
/// Manifest hash mismatch, archive corruption, or signature payload
/// did not verify.
pub const EXIT_TAMPERED: i32 = 71;
/// Pack content hash is on the revocation deny-list.
pub const EXIT_REVOKED: i32 = 72;
/// T1 author key is not in the local trust store.
pub const EXIT_KEY_UNKNOWN: i32 = 73;
/// Revocation cache is past the stale-grace window.
pub const EXIT_STALE_CACHE: i32 = 74;
/// Trust tier is recognised but verification not yet implemented
/// (T2+ Sigstore — Step 4b).
pub const EXIT_UNSUPPORTED: i32 = 75;

/// Refusal returned by `run_install` when verification rejects the
/// pack. Carries an exit code so `main.rs` can `process::exit` with the
/// right number after printing the user-facing message.
#[derive(Debug)]
pub struct InstallRefused {
    /// One of the `EXIT_*` constants above.
    pub exit_code: i32,
    /// Pre-formatted user-facing message.
    pub message: String,
}

impl std::fmt::Display for InstallRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for InstallRefused {}

/// Directory entries inside `.thinkingroot/` that should never end up
/// in a `.tr` file.
///
/// - `cache/` is a recompute-from-source artefact and contains paths
///   that only mean something on the original machine.
/// - `config.toml` carries workspace-local overrides and may include
///   provider keys (when a user customises `root setup` per project).
/// - `fingerprints.json` is the incremental-compile mtime ledger; it
///   regenerates on the next compile and would invalidate immediately
///   on a different host anyway.
const SKIP_TOP_LEVEL: &[&str] = &["cache", "config.toml", "fingerprints.json"];

/// On-disk pack metadata. Looked for at `<workspace>/Pack.toml`.
///
/// CLI flags (`--name`, `--version`, `--license`) override individual
/// fields. The combination of `Pack.toml` + flags must yield all three
/// required fields — otherwise `root pack` errors out.
#[derive(Debug, Deserialize)]
struct PackTomlFile {
    pack: PackTomlInner,
}

#[derive(Debug, Deserialize)]
struct PackTomlInner {
    name: String,
    version: String,
    license: String,
    #[serde(default)]
    description: Option<String>,
}

/// Run `root pack`. Dispatches to the v1 (multi-directory tar+zstd) or
/// v3 (3-file `[manifest.toml, source.tar.zst, claims.jsonl]`) writer
/// based on `format`. See module-level docs for v1 behaviour and
/// `crates/tr-format/src/writer_v3.rs` for v3.
pub fn run_pack(
    workspace: &Path,
    out: Option<PathBuf>,
    name_override: Option<String>,
    version_override: Option<String>,
    license_override: Option<String>,
    description_override: Option<String>,
    format: &str,
) -> Result<()> {
    match format {
        "tr/1" => run_pack_v1(
            workspace,
            out,
            name_override,
            version_override,
            license_override,
            description_override,
        ),
        "tr/3" => run_pack_v3(
            workspace,
            out,
            name_override,
            version_override,
            license_override,
            description_override,
        ),
        other => Err(anyhow!(
            "unknown pack format `{other}`; supported: tr/1, tr/3"
        )),
    }
}

fn run_pack_v1(
    workspace: &Path,
    out: Option<PathBuf>,
    name_override: Option<String>,
    version_override: Option<String>,
    license_override: Option<String>,
    description_override: Option<String>,
) -> Result<()> {
    let engine_dir = workspace.join(".thinkingroot");
    if !engine_dir.exists() {
        return Err(anyhow!(
            "no engine output at `{}`; run `root compile {}` first",
            engine_dir.display(),
            workspace.display()
        ));
    }

    let manifest = build_manifest(
        workspace,
        name_override,
        version_override,
        license_override,
        description_override,
    )?;

    let mut pb = PackBuilder::new(manifest.clone());
    let added = add_engine_files(&mut pb, &engine_dir)
        .with_context(|| format!("walk {}", engine_dir.display()))?;
    if added == 0 {
        return Err(anyhow!(
            "engine output at `{}` is empty (no packable files); did you run `root compile`?",
            engine_dir.display()
        ));
    }

    let bytes = pb.build().map_err(|e| anyhow!("build .tr: {e}"))?;

    let out_path = out.unwrap_or_else(|| {
        let owner_slug = manifest.name.replace('/', "-");
        workspace.join(format!("{}-{}.tr", owner_slug, manifest.version))
    });
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
    }
    fs::write(&out_path, &bytes)
        .with_context(|| format!("write {}", out_path.display()))?;

    println!(
        "  packed {} {} ({} files, {} bytes) -> {}",
        manifest.name,
        manifest.version,
        added,
        bytes.len(),
        out_path.display()
    );
    Ok(())
}

/// Build a v3 `package.tr` from a compiled workspace.
///
/// The flow:
///
///  1. Open the workspace's CozoDB at `.thinkingroot/graph.db` and the
///     [`thinkingroot_rooting::FileSystemSourceStore`] at
///     `.thinkingroot/rooting/sources/`.
///  2. Walk every `(uri, content_hash)` source row. For each non-empty
///     hash, fetch the source bytes from the byte store and stage them
///     under a workspace-relative POSIX path inside the V3 builder's
///     source bundle.
///  3. Walk every claim row joined with its source — populates
///     `(file, byte_start, byte_end)` from the v3 byte-range columns
///     persisted by the W1 migration.
///  4. Walk every `claim_id → entity_name` edge to populate `ents`.
///  5. Hand off to `V3PackBuilder::build` which seals the manifest with
///     the three BLAKE3 hashes per spec §3.1.
fn run_pack_v3(
    workspace: &Path,
    out: Option<PathBuf>,
    name_override: Option<String>,
    version_override: Option<String>,
    license_override: Option<String>,
    description_override: Option<String>,
) -> Result<()> {
    use thinkingroot_rooting::{FileSystemSourceStore, SourceByteStore};

    let engine_dir = workspace.join(".thinkingroot");
    if !engine_dir.exists() {
        return Err(anyhow!(
            "no engine output at `{}`; run `root compile {}` first",
            engine_dir.display(),
            workspace.display()
        ));
    }

    // 1. Manifest scaffolding from Pack.toml + CLI overrides. The v1
    //    `build_manifest` already does the merge logic; we lift its
    //    fields onto a fresh ManifestV3 to avoid duplicating the
    //    Pack.toml plumbing.
    let v1_manifest = build_manifest(
        workspace,
        name_override,
        version_override,
        license_override,
        description_override,
    )?;
    let mut manifest = ManifestV3::new(&v1_manifest.name, v1_manifest.version.clone());
    if !v1_manifest.license.is_empty() {
        manifest.license = Some(v1_manifest.license.clone());
    }
    if !v1_manifest.description.is_empty() {
        manifest.description = Some(v1_manifest.description.clone());
    }
    manifest.authors = v1_manifest.authors.clone();
    manifest.extracted_at = Some(chrono::Utc::now());
    manifest.extractor = Some(format!(
        "thinkingroot/extract@{}",
        env!("CARGO_PKG_VERSION")
    ));

    // 2. Open the workspace stores. CozoDB hands us joined claim+source
    //    rows; FileSystemSourceStore hands us source bytes by hash.
    let graph = thinkingroot_graph::graph::GraphStore::init(&engine_dir)
        .with_context(|| format!("open graph at {}", engine_dir.display()))?;
    let source_store = FileSystemSourceStore::new(&engine_dir)
        .with_context(|| format!("open source store at {}", engine_dir.display()))?;

    let mut builder = V3PackBuilder::new(manifest);
    let workspace_abs = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());

    // 3. Stage source files. We dedupe by pack path because two source
    //    rows may legitimately share a URI (e.g. a re-extract that
    //    produced a fresh row before the prior was GC'd) — the v3 pack
    //    layout has at most one entry per path.
    let sources = graph.get_sources_with_hashes()?;
    let mut packed_paths: HashSet<String> = HashSet::new();
    let mut source_bytes_added = 0u64;
    for (uri, content_hash) in &sources {
        if content_hash.is_empty() {
            continue;
        }
        let hash = ContentHash(content_hash.clone());
        let bytes = match source_store
            .get(&hash)
            .map_err(|e| anyhow!("source store read for {}: {e}", hash.0))?
        {
            Some(b) => b,
            None => continue,
        };
        let pack_path = workspace_relative_pack_path(uri, &workspace_abs);
        if packed_paths.insert(pack_path.clone()) {
            source_bytes_added += bytes.bytes.len() as u64;
            builder
                .add_source_file(&pack_path, &bytes.bytes)
                .with_context(|| format!("stage source {pack_path}"))?;
        }
    }

    // 4. Build claim records. Skip any claim whose owning source isn't
    //    in the pack (synthetic agent contributions, GC'd sources) so
    //    every emitted claim has a resolvable `file` field.
    let claim_rows = graph.get_v3_claim_export()?;
    let entity_names = graph.get_claim_entity_names()?;
    let mut claim_count = 0usize;
    for row in &claim_rows {
        if row.content_hash.is_empty() {
            continue;
        }
        let pack_path = workspace_relative_pack_path(&row.source_uri, &workspace_abs);
        if !packed_paths.contains(&pack_path) {
            continue;
        }
        let ents = entity_names.get(&row.id).cloned().unwrap_or_default();
        let mut record = ClaimRecord::new(
            row.id.clone(),
            row.statement.clone(),
            ents,
            pack_path,
            row.byte_start,
            row.byte_end,
        );
        if !row.claim_type.is_empty() {
            record = record.with_claim_type(row.claim_type.clone());
        }
        record = record.with_confidence(row.confidence);
        if !row.admission_tier.is_empty() {
            record = record.with_admission_tier(row.admission_tier.clone());
        }
        builder.add_claim(record);
        claim_count += 1;
    }

    // 5. Seal the pack and write to disk.
    let bytes = builder.build().map_err(|e| anyhow!("build .tr: {e}"))?;
    let out_path = out.unwrap_or_else(|| {
        let owner_slug = v1_manifest.name.replace('/', "-");
        workspace.join(format!("{owner_slug}-{}.tr", v1_manifest.version))
    });
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&out_path, &bytes).with_context(|| format!("write {}", out_path.display()))?;

    println!(
        "  packed {} {} (tr/3 — {} files, {} source bytes, {} claims, {} pack bytes) -> {}",
        v1_manifest.name,
        v1_manifest.version,
        packed_paths.len(),
        source_bytes_added,
        claim_count,
        bytes.len(),
        out_path.display()
    );
    Ok(())
}

/// Run `root verify <pack>`. Reads the pack from disk, parses the
/// outer tar, and invokes the v3 verification pipeline. Returns a
/// process exit code per the v3 spec §10.3 mapping:
///
/// - `0` — Verified.
/// - `1` — Tampered (recomputed hash ≠ declared hash, or signature
///   mismatch).
/// - `2` — Unsigned + `--allow-unsigned` not passed.
/// - `4` — Unsupported (reserved; today's free-fn verify never returns
///   this — placeholder for the `sigstore-impl` follow-up which adds
///   trust-root validation for Fulcio-issued certs).
///
/// Revocation is intentionally not consulted yet — `tr_revocation` is
/// async and the CLI's verify path is sync. The follow-up
/// `sigstore-impl` work wires the cache through with the same
/// short-circuit semantics as the existing v1 `Verifier::verify`.
pub fn run_verify(pack_path: &Path, allow_unsigned: bool) -> Result<i32> {
    let bytes = fs::read(pack_path)
        .with_context(|| format!("read {}", pack_path.display()))?;
    let pack = read_v3_pack(&bytes).map_err(|e| anyhow!("parse {}: {e}", pack_path.display()))?;
    let verdict = verify_v3_pack(&pack);

    match &verdict {
        V3Verdict::Verified {
            identity,
            rekor_log_index,
            signed_at,
        } => {
            println!(
                "  verified {} {} ({} files, {} claims)",
                pack.manifest.name,
                pack.manifest.version,
                pack.manifest.source_files.unwrap_or(0),
                pack.manifest.claim_count.unwrap_or(0),
            );
            println!("  pack hash: {}", pack.manifest.pack_hash);
            println!("  signed at: {signed_at}");
            match identity {
                Some(id) => println!("  signer: {id}"),
                None => println!("  signer: self-signed (Ed25519 public key bundled)"),
            }
            if let Some(idx) = rekor_log_index {
                println!("  rekor log index: {idx}");
            } else {
                println!("  rekor: not witnessed (self-signed)");
            }
            Ok(0)
        }
        V3Verdict::Unsigned => {
            if allow_unsigned {
                println!(
                    "  unsigned {} {} (no signature.sig in pack — accepted via --allow-unsigned)",
                    pack.manifest.name, pack.manifest.version,
                );
                Ok(0)
            } else {
                eprintln!(
                    "  unsigned: {} has no signature.sig (use --allow-unsigned to accept)",
                    pack_path.display()
                );
                Ok(EXIT_UNSIGNED)
            }
        }
        V3Verdict::Tampered(kind) => {
            match kind {
                V3TamperedKind::PackHashMismatch {
                    declared,
                    recomputed,
                } => {
                    eprintln!(
                        "  tampered: pack hash mismatch — manifest declares {declared}, \
                         recomputed {recomputed}"
                    );
                }
                V3TamperedKind::SignatureFailed { reason } => {
                    eprintln!("  tampered: signature failed — {reason}");
                }
            }
            Ok(EXIT_TAMPERED)
        }
    }
}

/// Map a stored source URI back to the workspace-relative POSIX path
/// the v3 pack writer stages it under.
///
/// Cases handled:
/// - `file:///abs/path/to/file.rs` inside `workspace_abs` → relative.
/// - `file:///abs/elsewhere.rs` outside the workspace → strip
///   `file://` and the leading slash (path lives at top level).
/// - Other schemes (`git://`, `mcp://agent/...`) — pass through; the
///   v3 pack treats them as opaque path strings. Reader-side tooling
///   can decide how to resolve them.
fn workspace_relative_pack_path(uri: &str, workspace_abs: &Path) -> String {
    if let Some(stripped) = uri.strip_prefix("file://") {
        let abs = Path::new(stripped);
        if let Ok(rel) = abs.strip_prefix(workspace_abs) {
            return rel.to_string_lossy().replace('\\', "/");
        }
        // Outside workspace — emit as a top-level path with leading
        // slashes stripped so the v3 writer's safe-path check accepts
        // it.
        return stripped.trim_start_matches('/').to_string();
    }
    uri.to_string()
}

/// Resolve the manifest by combining `Pack.toml` (if present) with CLI
/// overrides. Errors if any of the three required fields (`name`,
/// `version`, `license`) is unresolved.
fn build_manifest(
    workspace: &Path,
    name_override: Option<String>,
    version_override: Option<String>,
    license_override: Option<String>,
    description_override: Option<String>,
) -> Result<Manifest> {
    let pack_toml_path = workspace.join("Pack.toml");
    let from_file: Option<PackTomlInner> = if pack_toml_path.exists() {
        let raw = fs::read_to_string(&pack_toml_path)
            .with_context(|| format!("read {}", pack_toml_path.display()))?;
        let parsed: PackTomlFile = toml::from_str(&raw)
            .with_context(|| format!("parse {}", pack_toml_path.display()))?;
        Some(parsed.pack)
    } else {
        None
    };

    let pick = |cli: Option<String>, file: Option<String>, key: &'static str| -> Result<String> {
        cli.or(file).ok_or_else(|| {
            anyhow!(
                "missing required field `{}` — set it in {}/Pack.toml or pass --{}",
                key,
                workspace.display(),
                key
            )
        })
    };

    let name = pick(
        name_override,
        from_file.as_ref().map(|p| p.name.clone()),
        "name",
    )?;
    let version_str = pick(
        version_override,
        from_file.as_ref().map(|p| p.version.clone()),
        "version",
    )?;
    let license = pick(
        license_override,
        from_file.as_ref().map(|p| p.license.clone()),
        "license",
    )?;

    let version = Version::parse(&version_str)
        .with_context(|| format!("parse version `{}` (must be semver)", version_str))?;

    let mut manifest = Manifest::new(name, version, license);
    if let Some(d) = description_override {
        manifest.description = d;
    } else if let Some(d) = from_file.and_then(|p| p.description) {
        manifest.description = d;
    }
    // Validate eagerly so user gets the error before the slow walk.
    manifest
        .validate()
        .map_err(|e| anyhow!("invalid manifest: {e}"))?;
    Ok(manifest)
}

/// Walk `engine_dir` recursively, adding every regular file (other than
/// the skip-list entries) to the [`PackBuilder`] under its
/// `.thinkingroot`-relative path. Returns the count of files added.
fn add_engine_files(pb: &mut PackBuilder, engine_dir: &Path) -> Result<usize> {
    let mut count = 0usize;
    walk_files(engine_dir, &mut |abs_path| -> Result<()> {
        let rel = abs_path
            .strip_prefix(engine_dir)
            .map_err(|e| anyhow!("strip_prefix {}: {e}", abs_path.display()))?;
        // Skip if the top-level component is on the skip list.
        let mut comps = rel.components();
        if let Some(top) = comps.next() {
            let top_str = top.as_os_str().to_string_lossy();
            if SKIP_TOP_LEVEL.contains(&top_str.as_ref()) {
                return Ok(());
            }
        }
        // Normalise to forward slashes for in-pack paths.
        let pack_path = rel
            .to_str()
            .ok_or_else(|| anyhow!("non-utf8 path {}", rel.display()))?
            .replace('\\', "/");
        let bytes = fs::read(abs_path)
            .with_context(|| format!("read {}", abs_path.display()))?;
        pb.put_file(&pack_path, &bytes)
            .map_err(|e| anyhow!("put `{}` into pack: {e}", pack_path))?;
        count += 1;
        Ok(())
    })?;
    Ok(count)
}

/// Walk `dir` recursively, calling `visit` on every regular file.
/// Symlinks are not followed; reparse points / FIFOs / sockets are
/// silently skipped.
fn walk_files<F>(dir: &Path, visit: &mut F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    let read = fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in read {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            // .tr packs are content-addressed; following symlinks would
            // make the hash depend on filesystem layout outside the
            // workspace. Skip.
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            walk_files(&path, visit)?;
        } else if ft.is_file() {
            visit(&path)?;
        }
    }
    Ok(())
}

/// Run `root install`. The `reference` argument accepts:
///
/// - A local path: `./pack.tr`, `/abs/path.tr` — read from disk.
/// - A direct URL: `https://example.com/pack.tr` — HTTPS GET (HTTP
///   accepted only for `127.0.0.1` / `localhost` to keep tests honest).
/// - A registry coordinate: `owner/slug@version` (or `@latest`) —
///   resolved via the discovery doc at the configured registry's
///   `/.well-known/tr-registry.json`.
///
/// The configured registry comes from `TR_REGISTRY_URL` env var,
/// otherwise `~/.config/thinkingroot/registry.toml` (key `default`),
/// otherwise the built-in default `https://thinkingroot.dev`.
/// `--registry <url>` (passed via `registry_override`) overrides all
/// of the above for one invocation.
/// Resolve a pack reference, parse its manifest, and print a human-
/// readable preview to stdout — no verification, no extraction. Used
/// by `root install --dry-run` and the desktop install sheet path
/// (which calls into `tr-render` directly via Tauri).
pub async fn run_install_dry_run(
    reference: &str,
    registry_override: Option<String>,
) -> Result<()> {
    let install_ref = parse_install_ref(reference)?;
    let resolver = build_resolver(install_ref, registry_override)?;
    let bytes = resolver.resolve().await?;
    let pack = tr_reader::read_bytes(&bytes).map_err(|e| anyhow!("read .tr: {e}"))?;
    let preview = tr_render::render_preview(&pack.manifest, &bytes)
        .map_err(|e| anyhow!("render preview: {e}"))?;

    println!("{}", preview.manifest_table);
    println!();
    println!("{}", preview.markdown);
    println!(
        "(dry-run — nothing extracted; pass without `--dry-run` to install)"
    );
    Ok(())
}

pub async fn run_install(
    reference: &str,
    target: Option<PathBuf>,
    registry_override: Option<String>,
    allow_unsigned: bool,
) -> Result<()> {
    let install_ref = parse_install_ref(reference)?;
    let registry_url_for_verifier = match (&install_ref, &registry_override) {
        (InstallRef::Registry { .. }, Some(url)) => url.clone(),
        (InstallRef::Registry { .. }, None) => load_default_registry()?,
        // Local + direct-URL installs do not have a single canonical
        // registry URL. Use the built-in default so the revocation
        // cache lives in a deterministic location across invocations.
        _ => BUILTIN_DEFAULT_REGISTRY.to_string(),
    };
    let is_remote = matches!(
        install_ref,
        InstallRef::DirectUrl(_) | InstallRef::Registry { .. }
    );
    let verifier =
        build_default_verifier(&registry_url_for_verifier, is_remote, allow_unsigned)?;
    install_with_verifier(reference, target, registry_override, &verifier).await
}

/// Test-friendly entry point that takes a pre-built [`Verifier`].
///
/// Production callers use [`run_install`], which constructs a default
/// verifier from the system cache directory and pinned keys. Tests
/// pass a fixture-backed verifier so they can exercise revoked /
/// tampered / unknown-key paths without touching the user's home
/// directory.
pub async fn install_with_verifier(
    reference: &str,
    target: Option<PathBuf>,
    registry_override: Option<String>,
    verifier: &Verifier,
) -> Result<()> {
    let install_ref = parse_install_ref(reference)?;
    let resolver = build_resolver(install_ref, registry_override)?;
    let bytes = resolver.resolve().await?;
    install_from_bytes_with_verifier(&bytes, target, verifier).await
}

/// Dispatch an [`InstallRef`] to the right [`PackResolver`].
///
/// The [`crate::resolver::PackResolver`] trait keeps the system open
/// to future backends (OCI, S3-mirror, IPFS) — adding a new resolver
/// means a new arm here, not a rewrite of `install_with_verifier`.
pub(crate) fn build_resolver(
    install_ref: InstallRef,
    registry_override: Option<String>,
) -> Result<Box<dyn PackResolver>> {
    Ok(match install_ref {
        InstallRef::Local(path) => Box::new(LocalFsResolver::new(path)),
        InstallRef::DirectUrl(url) => Box::new(HttpDirectUrlResolver::new(url)),
        InstallRef::Registry {
            owner,
            slug,
            version,
        } => {
            let registry = match registry_override {
                Some(url) => url,
                None => load_default_registry()?,
            };
            Box::new(HttpRegistryResolver::new(registry, owner, slug, version))
        }
    })
}

fn build_default_verifier(
    registry_url: &str,
    is_remote: bool,
    user_allow_unsigned: bool,
) -> Result<Verifier> {
    let url = url::Url::parse(registry_url)
        .with_context(|| format!("parse registry url `{registry_url}`"))?;
    let cache_dir = tr_revocation::default_cache_dir()
        .ok_or_else(|| anyhow!("no platform cache directory available"))?;
    let cache = Arc::new(RevocationCache::new(CacheConfig::defaults_for(
        url, cache_dir,
    )));

    // Local installs default to T0 (no signature required); remote
    // installs require T1 unless the user passes --allow-unsigned.
    let (require_min_tier, default_allow) = if is_remote {
        (TrustTier::T1, false)
    } else {
        (TrustTier::T0, true)
    };
    let allow_unsigned = user_allow_unsigned || default_allow;

    Ok(Verifier::new(VerifierConfig {
        revocation: cache,
        // v0.1 ships no preconfigured author keys; users opt in by
        // running `root key trust import <id>` once that subcommand
        // lands (Step 6 / Phase G CLI consolidation).
        author_keys: Arc::new(AuthorKeyStore::empty()),
        require_min_tier,
        allow_unsigned,
    }))
}

/// A user-supplied install target — local file, direct URL, or
/// registry coordinate.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InstallRef {
    Local(PathBuf),
    DirectUrl(String),
    Registry {
        owner: String,
        slug: String,
        version: String,
    },
}

/// Parse a CLI argument into an [`InstallRef`].
///
/// Disambiguation rules:
/// - Anything starting with `http://` / `https://` is a URL.
/// - Anything starting with `.`, `/`, `\`, or containing a path
///   separator before the first `@` is treated as a local path.
/// - Otherwise, attempt to parse `owner/slug@version`. Owner must be
///   non-empty and contain no `/`; slug must be non-empty and contain
///   no `/`; version must be non-empty.
/// - Fallback: treat as a local path.
fn parse_install_ref(s: &str) -> Result<InstallRef> {
    if s.is_empty() {
        return Err(anyhow!("empty install reference"));
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Ok(InstallRef::DirectUrl(s.to_string()));
    }
    if s.starts_with('.') || s.starts_with('/') || s.starts_with('\\') {
        return Ok(InstallRef::Local(PathBuf::from(s)));
    }
    if let Some((coord, version)) = s.split_once('@') {
        if let Some((owner, slug)) = coord.split_once('/') {
            if !owner.is_empty()
                && !slug.is_empty()
                && !owner.contains('/')
                && !slug.contains('/')
                && !version.is_empty()
            {
                return Ok(InstallRef::Registry {
                    owner: owner.to_string(),
                    slug: slug.to_string(),
                    version: version.to_string(),
                });
            }
        }
    }
    // Final fallback: local path. Preserves the prior single-arg
    // behaviour for inputs that don't match any URI form.
    Ok(InstallRef::Local(PathBuf::from(s)))
}

/// Hardcoded fallback when neither the env var nor the config file
/// resolves a registry URL. Picked to match the production hub's
/// public hostname.
const BUILTIN_DEFAULT_REGISTRY: &str = "https://thinkingroot.dev";

/// Resolve the default registry URL: env var > config file > built-in.
fn load_default_registry() -> Result<String> {
    if let Ok(url) = std::env::var("TR_REGISTRY_URL") {
        if !url.trim().is_empty() {
            return Ok(url.trim().to_string());
        }
    }
    if let Some(cfg_path) = registry_config_path() {
        if cfg_path.exists() {
            let raw = fs::read_to_string(&cfg_path)
                .with_context(|| format!("read {}", cfg_path.display()))?;
            #[derive(Deserialize)]
            struct RegistryConfig {
                default: String,
            }
            let parsed: RegistryConfig = toml::from_str(&raw)
                .with_context(|| format!("parse {}", cfg_path.display()))?;
            if !parsed.default.trim().is_empty() {
                return Ok(parsed.default.trim().to_string());
            }
        }
    }
    Ok(BUILTIN_DEFAULT_REGISTRY.to_string())
}

fn registry_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("thinkingroot").join("registry.toml"))
}

/// Build the shared HTTPS client used for both registry and direct-URL
/// fetches. Pinned timeouts and a stable user-agent.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("thinkingroot/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow!("build http client: {e}"))
}

/// Reject `http://` URLs except for loopback hosts. Plain HTTP outside
/// localhost is a downgrade attack vector — the registry serves
/// content-addressed bytes, but TLS still protects against a MITM
/// substituting a different (validly-hashed) pack.
pub(crate) fn refuse_insecure_http(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if !lower.starts_with("http://") {
        return Err(anyhow!("unsupported URL scheme: {}", url));
    }
    let after = &url["http://".len()..];
    let host_end = after
        .find(|c: char| c == '/' || c == ':' || c == '?')
        .unwrap_or(after.len());
    let host = &after[..host_end];
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower == "127.0.0.1"
        || host_lower.starts_with("127.")
        || host_lower == "::1"
        || host_lower == "[::1]"
    {
        return Ok(());
    }
    Err(anyhow!(
        "refusing http:// for non-loopback host `{}` — registries must use https",
        host
    ))
}

// `fetch_direct_url` and `fetch_via_registry` were moved into
// `crate::resolver::http` (Phase C, Step 7). Their bodies now live in
// `HttpDirectUrlResolver::resolve` and `HttpRegistryResolver::resolve`
// respectively. Dispatch happens through [`build_resolver`].

/// Verify a `.tr` byte slice against the supplied [`Verifier`] and,
/// on a `Verified` verdict, extract its contents into the target
/// directory's `.thinkingroot/`. Used by both [`run_install`]
/// (production) and [`install_with_verifier`] (tests).
async fn install_from_bytes_with_verifier(
    bytes: &[u8],
    target: Option<PathBuf>,
    verifier: &Verifier,
) -> Result<()> {
    let pack = tr_reader::read_bytes(bytes).map_err(|e| anyhow!("read .tr: {e}"))?;
    let verdict = verifier
        .verify(&pack)
        .await
        .map_err(|e| anyhow!("verifier: {e}"))?;
    enforce_verdict(&pack, verdict)?;
    extract_pack_to_target(&pack, target)
}

/// Map a [`Verdict`] to either `Ok(())` (continue install) or an
/// [`InstallRefused`] error carrying the right exit code + message.
fn enforce_verdict(pack: &tr_reader::Pack, verdict: Verdict) -> Result<()> {
    match verdict {
        Verdict::Verified(d) => {
            tracing::info!(
                tier = ?d.tier,
                author = ?d.author_id,
                rekor_log_index = ?d.sigstore_log_index,
                revocation_freshness_secs = d.revocation_freshness_secs,
                "verified {} {}",
                pack.manifest.name,
                pack.manifest.version
            );
            Ok(())
        }
        Verdict::Unsigned => Err(InstallRefused {
            exit_code: EXIT_UNSIGNED,
            message: format!(
                "✗ refusing to install unsigned pack `{}` — pass --allow-unsigned to override (https-only)",
                pack.manifest.name
            ),
        }
        .into()),
        Verdict::Tampered(kind) => Err(InstallRefused {
            exit_code: EXIT_TAMPERED,
            message: format_tampered(&pack.manifest, &kind),
        }
        .into()),
        Verdict::Revoked(d) => Err(InstallRefused {
            exit_code: EXIT_REVOKED,
            message: format_revoked(&d),
        }
        .into()),
        Verdict::KeyUnknown { key_id } => Err(InstallRefused {
            exit_code: EXIT_KEY_UNKNOWN,
            message: format!(
                "✗ pack `{}` is signed by author key `{key_id}` which is not in your trust store",
                pack.manifest.name
            ),
        }
        .into()),
        Verdict::StaleCache { age_days } => Err(InstallRefused {
            exit_code: EXIT_STALE_CACHE,
            message: format!(
                "✗ revocation cache is {age_days} days old — refusing. Run `root revoked refresh` while online."
            ),
        }
        .into()),
        Verdict::Unsupported { tier, reason } => Err(InstallRefused {
            exit_code: EXIT_UNSUPPORTED,
            message: format!(
                "✗ this `root` build cannot verify {tier:?} packs yet ({reason}). Upgrade to a release that bundles the Sigstore trust root."
            ),
        }
        .into()),
    }
}

fn format_tampered(manifest: &Manifest, kind: &TamperedKind) -> String {
    let detail = match kind {
        TamperedKind::ManifestHashMismatch { expected, actual } => format!(
            "manifest body hash mismatch (expected `{expected}`, computed `{actual}`)"
        ),
        TamperedKind::ArchiveCorrupt(msg) => format!("archive corrupt: {msg}"),
        TamperedKind::SignaturePayloadMismatch => {
            "signature does not match the pack's contents".to_string()
        }
    };
    format!("✗ pack `{}` failed integrity check: {detail}", manifest.name)
}

fn format_revoked(d: &RevokedDetails) -> String {
    format!(
        "✗ pack `{}@{}` was revoked by {:?} on epoch {} (reason: {:?}) — see {}",
        d.advisory.pack,
        d.advisory.version,
        d.advisory.authority,
        d.advisory.revoked_at,
        d.advisory.reason,
        d.advisory.details_url,
    )
}

/// Extract a verified pack's payload + manifest into `target`. Pulled
/// out of [`install_from_bytes_with_verifier`] for clarity — the unpack
/// step never produces an [`InstallRefused`] (any failure here is an
/// I/O error that bubbles via `anyhow`).
fn extract_pack_to_target(pack: &tr_reader::Pack, target: Option<PathBuf>) -> Result<()> {
    let manifest = &pack.manifest;
    let target_dir = match target {
        Some(t) => t,
        None => default_install_dir(manifest)?,
    };
    let engine_dir = target_dir.join(".thinkingroot");
    fs::create_dir_all(&engine_dir)
        .with_context(|| format!("create {}", engine_dir.display()))?;

    let mut count = 0usize;
    for path in pack.paths() {
        if path == "manifest.json" {
            continue;
        }
        let entry_bytes = pack
            .entry(path)
            .ok_or_else(|| anyhow!("pack reports `{}` but has no entry", path))?;
        let dest = engine_dir.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&dest, entry_bytes)
            .with_context(|| format!("write {}", dest.display()))?;
        count += 1;
    }

    let manifest_path = target_dir.join("manifest.json");
    let manifest_json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| anyhow!("serialize manifest: {e}"))?;
    fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    println!(
        "  installed {} {} ({} files) -> {}",
        manifest.name,
        manifest.version,
        count,
        target_dir.display()
    );
    Ok(())
}

/// Default install directory: `~/.thinkingroot/packs/<name>/<version>/`.
/// `<name>` is `owner/slug`, which becomes a two-level subpath on disk.
fn default_install_dir(manifest: &Manifest) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory available"))?;
    let (owner, slug) = manifest
        .owner_and_slug()
        .map_err(|e| anyhow!("manifest name: {e}"))?;
    Ok(home
        .join(".thinkingroot")
        .join("packs")
        .join(owner)
        .join(slug)
        .join(manifest.version.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::tempdir;

    /// Set up a fake compiled workspace in `dir`. Returns the workspace
    /// root (parent of `.thinkingroot/`).
    fn fake_engine_workspace(dir: &Path) -> PathBuf {
        let workspace = dir.to_path_buf();
        let engine = workspace.join(".thinkingroot");
        // Files that should END UP in the .tr.
        fs::create_dir_all(engine.join("artifacts/entities")).unwrap();
        fs::write(
            engine.join("artifacts/entities/foo.md"),
            b"# Foo\nA fact.\n",
        )
        .unwrap();
        fs::create_dir_all(engine.join("graph")).unwrap();
        fs::write(engine.join("graph/triples.jsonl"), b"{\"s\":1}\n").unwrap();
        fs::create_dir_all(engine.join("rooting/sources")).unwrap();
        fs::write(
            engine.join("rooting/sources/abc123.src"),
            b"source content\n",
        )
        .unwrap();
        fs::write(engine.join("vectors.bin"), b"\x00\x01\x02\x03").unwrap();
        // Files that should be SKIPPED.
        fs::create_dir_all(engine.join("cache/extraction")).unwrap();
        fs::write(engine.join("cache/extraction/foo.json"), b"{}").unwrap();
        fs::write(engine.join("config.toml"), b"key=\"secret\"").unwrap();
        fs::write(engine.join("fingerprints.json"), b"[]").unwrap();
        // Pack.toml so we don't need CLI overrides.
        fs::write(
            workspace.join("Pack.toml"),
            r#"[pack]
name = "alice/demo"
version = "0.1.0"
license = "MIT"
description = "Round-trip test pack."
"#,
        )
        .unwrap();
        workspace
    }

    #[tokio::test]
    async fn pack_then_install_round_trips_engine_output() {
        let src_tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(src_tmp.path());

        let out_tr = workspace.join("alice-demo-0.1.0.tr");
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None, "tr/1").unwrap();
        assert!(out_tr.exists(), ".tr file not produced");
        assert!(
            fs::metadata(&out_tr).unwrap().len() > 0,
            ".tr file is empty"
        );

        // Install into a fresh target via the new string-keyed entry point.
        let install_tmp = tempdir().unwrap();
        let target = install_tmp.path().join("install-here");
        // T0 unsigned local pack — `allow_unsigned: true` is the local
        // default but pass explicitly for clarity.
        run_install(out_tr.to_str().unwrap(), Some(target.clone()), None, true)
            .await
            .unwrap();

        // Manifest landed at root.
        let manifest_path = target.join("manifest.json");
        assert!(manifest_path.exists(), "manifest.json not extracted");
        let manifest_blob = fs::read(&manifest_path).unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&manifest_blob).expect("manifest parses");
        assert_eq!(manifest.name, "alice/demo");
        assert_eq!(manifest.version, Version::parse("0.1.0").unwrap());
        assert_eq!(manifest.license, "MIT");
        assert_eq!(manifest.description, "Round-trip test pack.");

        // Engine files round-trip with the same content.
        let engine = target.join(".thinkingroot");
        let foo = engine.join("artifacts/entities/foo.md");
        assert!(foo.exists(), "artifact missing after install");
        assert_eq!(fs::read(&foo).unwrap(), b"# Foo\nA fact.\n");
        assert_eq!(
            fs::read(engine.join("graph/triples.jsonl")).unwrap(),
            b"{\"s\":1}\n"
        );
        assert_eq!(
            fs::read(engine.join("rooting/sources/abc123.src")).unwrap(),
            b"source content\n"
        );
        assert_eq!(fs::read(engine.join("vectors.bin")).unwrap(), b"\x00\x01\x02\x03");

        // Skipped files did NOT land.
        assert!(
            !engine.join("cache").exists(),
            "cache/ was packed (should be skipped)"
        );
        assert!(
            !engine.join("config.toml").exists(),
            "config.toml was packed (should be skipped)"
        );
        assert!(
            !engine.join("fingerprints.json").exists(),
            "fingerprints.json was packed (should be skipped)"
        );
    }

    #[test]
    fn pack_uses_cli_overrides_over_pack_toml() {
        let tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(tmp.path());
        let out_tr = workspace.join("override.tr");
        run_pack(
            &workspace,
            Some(out_tr.clone()),
            Some("bob/forked".to_string()),
            Some("2.5.0".to_string()),
            Some("Apache-2.0".to_string()),
            Some("Bob's fork.".to_string()),
            "tr/1",
        )
        .unwrap();
        let pack = tr_reader::read_file(&out_tr).unwrap();
        assert_eq!(pack.manifest.name, "bob/forked");
        assert_eq!(pack.manifest.version, Version::parse("2.5.0").unwrap());
        assert_eq!(pack.manifest.license, "Apache-2.0");
        assert_eq!(pack.manifest.description, "Bob's fork.");
    }

    #[test]
    fn pack_errors_when_engine_dir_absent() {
        let tmp = tempdir().unwrap();
        let err = run_pack(tmp.path(), None, None, None, None, None, "tr/1").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no engine output"), "got: {msg}");
    }

    #[test]
    fn pack_errors_when_required_fields_missing() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path();
        // Engine dir present, no Pack.toml, no overrides → error mentions `name`.
        fs::create_dir_all(workspace.join(".thinkingroot/artifacts")).unwrap();
        fs::write(workspace.join(".thinkingroot/artifacts/x.md"), b"x").unwrap();
        let err = run_pack(workspace, None, None, None, None, None, "tr/1").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("name"), "got: {msg}");
    }

    #[test]
    fn pack_path_set_matches_engine_files_minus_skips() {
        let tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(tmp.path());
        let out_tr = workspace.join("paths.tr");
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None, "tr/1").unwrap();
        let pack = tr_reader::read_file(&out_tr).unwrap();
        let paths: HashSet<&str> = pack
            .paths()
            .filter(|p| *p != "manifest.json")
            .collect();
        let expected: HashSet<&str> = [
            "artifacts/entities/foo.md",
            "graph/triples.jsonl",
            "rooting/sources/abc123.src",
            "vectors.bin",
        ]
        .into_iter()
        .collect();
        assert_eq!(paths, expected);
    }

    #[tokio::test]
    async fn install_into_explicit_target_does_not_touch_home() {
        let src_tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(src_tmp.path());
        let out_tr = workspace.join("explicit.tr");
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None, "tr/1").unwrap();

        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("dst");
        run_install(out_tr.to_str().unwrap(), Some(target.clone()), None, true)
            .await
            .unwrap();
        assert!(target.join("manifest.json").exists());
        assert!(target.join(".thinkingroot/graph/triples.jsonl").exists());
    }

    #[tokio::test]
    async fn install_rejects_corrupted_tr() {
        let src_tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(src_tmp.path());
        let good_tr = workspace.join("good.tr");
        run_pack(&workspace, Some(good_tr.clone()), None, None, None, None, "tr/1").unwrap();

        // Corrupt the file deterministically: zero out a 64-byte run
        // in the middle of the zstd stream + truncate the trailing
        // checksum. Flipping a single byte was probabilistically
        // recoverable by zstd's error correction.
        let mut bytes = fs::read(&good_tr).unwrap();
        let mid = bytes.len() / 2;
        let end = (mid + 64).min(bytes.len());
        for b in &mut bytes[mid..end] {
            *b = 0;
        }
        bytes.truncate(bytes.len().saturating_sub(8));
        let bad_tr = workspace.join("bad.tr");
        fs::write(&bad_tr, &bytes).unwrap();

        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("dst");
        let err = run_install(bad_tr.to_str().unwrap(), Some(target), None, true)
            .await
            .expect_err("must reject corrupted .tr");
        let msg = format!("{err}");
        assert!(msg.contains("read"), "got: {msg}");
    }

    // ── InstallRef parser ────────────────────────────────────────────

    #[test]
    fn parse_local_path_dot_slash() {
        assert_eq!(
            parse_install_ref("./alice-demo-0.1.0.tr").unwrap(),
            InstallRef::Local(PathBuf::from("./alice-demo-0.1.0.tr"))
        );
    }

    #[test]
    fn parse_local_path_absolute() {
        assert_eq!(
            parse_install_ref("/abs/path.tr").unwrap(),
            InstallRef::Local(PathBuf::from("/abs/path.tr"))
        );
    }

    #[test]
    fn parse_https_url() {
        assert_eq!(
            parse_install_ref("https://example.com/p.tr").unwrap(),
            InstallRef::DirectUrl("https://example.com/p.tr".to_string())
        );
    }

    #[test]
    fn parse_registry_coord() {
        assert_eq!(
            parse_install_ref("alice/cool-pack@1.2.3").unwrap(),
            InstallRef::Registry {
                owner: "alice".to_string(),
                slug: "cool-pack".to_string(),
                version: "1.2.3".to_string(),
            }
        );
    }

    #[test]
    fn parse_registry_coord_with_latest() {
        assert_eq!(
            parse_install_ref("alice/cool@latest").unwrap(),
            InstallRef::Registry {
                owner: "alice".to_string(),
                slug: "cool".to_string(),
                version: "latest".to_string(),
            }
        );
    }

    #[test]
    fn parse_path_containing_at_is_local() {
        // `./pack@v1.tr` has both a path-shape prefix and an `@`, but
        // the leading `.` wins — it's a path.
        assert_eq!(
            parse_install_ref("./pack@v1.tr").unwrap(),
            InstallRef::Local(PathBuf::from("./pack@v1.tr"))
        );
    }

    #[test]
    fn parse_empty_errors() {
        assert!(parse_install_ref("").is_err());
    }

    // ── Insecure-HTTP guard ──────────────────────────────────────────

    #[test]
    fn refuse_insecure_http_allows_https() {
        assert!(refuse_insecure_http("https://example.com/x").is_ok());
    }

    #[test]
    fn refuse_insecure_http_allows_loopback() {
        assert!(refuse_insecure_http("http://127.0.0.1:8080/x").is_ok());
        assert!(refuse_insecure_http("http://localhost/x").is_ok());
    }

    #[test]
    fn refuse_insecure_http_blocks_remote_http() {
        let err = refuse_insecure_http("http://example.com/x").unwrap_err();
        assert!(format!("{err}").contains("https"), "got: {err}");
    }

    // ── Live registry round-trip ─────────────────────────────────────

    /// Spin up an in-process axum server that mimics the cloud
    /// registry's discovery + download endpoints, then drive
    /// `run_install("alice/demo@1.0.0", ...)` against it. Verifies
    /// hash-header check, discovery-doc parsing, and the unpacked
    /// files match what `root pack` produced.
    #[tokio::test]
    async fn install_via_registry_round_trip() {
        use axum::{extract::State, http::header, response::IntoResponse, routing::get, Router};
        use std::sync::Arc;

        // 1. Build a real .tr from a fake engine workspace.
        let src_tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(src_tmp.path());
        let tr_out = workspace.join("alice-demo-0.1.0.tr");
        run_pack(&workspace, Some(tr_out.clone()), None, None, None, None, "tr/1").unwrap();
        let tr_bytes = fs::read(&tr_out).unwrap();
        let advertised_hash = blake3_hex(&tr_bytes);

        // 2. Spin up the test registry.
        struct AppState {
            tr_bytes: Vec<u8>,
            hash: String,
        }
        let state = Arc::new(AppState {
            tr_bytes: tr_bytes.clone(),
            hash: advertised_hash.clone(),
        });

        async fn discovery(State(_): State<Arc<AppState>>) -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "format_version": "tr-registry/1",
                "tr_format": tr_format::manifest::FORMAT_VERSION,
                "endpoints": {
                    "download": "/api/v1/packs/{owner}/{slug}/versions/{version}/download"
                },
                "max_pack_bytes": 134_217_728_u64,
                "supported_compressions": ["zstd"]
            }))
        }
        async fn download(State(s): State<Arc<AppState>>) -> impl IntoResponse {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/zstd"),
            );
            headers.insert(
                axum::http::HeaderName::from_static("x-tr-content-hash"),
                axum::http::HeaderValue::from_str(&s.hash).unwrap(),
            );
            (axum::http::StatusCode::OK, headers, s.tr_bytes.clone())
        }

        let app = Router::new()
            .route("/.well-known/tr-registry.json", get(discovery))
            .route(
                "/api/v1/packs/{owner}/{slug}/versions/{version}/download",
                get(download),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        let registry_url = format!("http://{}", bound);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // 3. Drive `root install` against the live test registry.
        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("installed");
        // The fixture pack is T0 (run_pack ships unsigned by default) and
        // we're installing remotely — pass --allow-unsigned to flip the
        // remote-default policy.
        run_install(
            "alice/demo@0.1.0",
            Some(target.clone()),
            Some(registry_url.clone()),
            true,
        )
        .await
        .unwrap();
        server.abort();

        // 4. Verify install layout — same shape as the local-file path.
        assert!(target.join("manifest.json").exists());
        assert!(target.join(".thinkingroot/graph/triples.jsonl").exists());
        assert_eq!(
            fs::read(target.join(".thinkingroot/artifacts/entities/foo.md")).unwrap(),
            b"# Foo\nA fact.\n"
        );
    }

    /// Hash mismatch must abort install — defense in depth on top of
    /// the manifest's own canonical-bytes check inside `tr_format`.
    #[tokio::test]
    async fn install_via_registry_rejects_hash_mismatch() {
        use axum::{extract::State, http::header, response::IntoResponse, routing::get, Router};
        use std::sync::Arc;

        let src_tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(src_tmp.path());
        let tr_out = workspace.join("alice-demo-0.1.0.tr");
        run_pack(&workspace, Some(tr_out.clone()), None, None, None, None, "tr/1").unwrap();
        let tr_bytes = fs::read(&tr_out).unwrap();

        struct AppState {
            tr_bytes: Vec<u8>,
        }
        let state = Arc::new(AppState { tr_bytes });

        async fn discovery() -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "format_version": "tr-registry/1",
                "tr_format": tr_format::manifest::FORMAT_VERSION,
                "endpoints": {
                    "download": "/api/v1/packs/{owner}/{slug}/versions/{version}/download"
                },
                "max_pack_bytes": 134_217_728_u64,
                "supported_compressions": ["zstd"]
            }))
        }
        async fn download(State(s): State<Arc<AppState>>) -> impl IntoResponse {
            // Lie about the hash — substitute `0`s.
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/zstd"),
            );
            headers.insert(
                axum::http::HeaderName::from_static("x-tr-content-hash"),
                axum::http::HeaderValue::from_static(
                    "0000000000000000000000000000000000000000000000000000000000000000",
                ),
            );
            (axum::http::StatusCode::OK, headers, s.tr_bytes.clone())
        }

        let app = Router::new()
            .route("/.well-known/tr-registry.json", get(discovery))
            .route(
                "/api/v1/packs/{owner}/{slug}/versions/{version}/download",
                get(download),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        let registry_url = format!("http://{}", bound);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("installed");
        let err = run_install(
            "alice/demo@0.1.0",
            Some(target),
            Some(registry_url),
            true,
        )
        .await
        .expect_err("hash mismatch must abort install");
        server.abort();
        assert!(
            format!("{err}").contains("hash mismatch"),
            "expected hash-mismatch error, got: {err}"
        );
    }

    /// Discovery doc with a foreign `format_version` must be refused.
    #[tokio::test]
    async fn install_via_registry_refuses_unknown_registry_format() {
        use axum::{response::IntoResponse, routing::get, Router};

        async fn discovery() -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "format_version": "tr-registry/99",
                "tr_format": "tr/1",
                "endpoints": {
                    "download": "/api/v1/packs/{owner}/{slug}/versions/{version}/download"
                }
            }))
        }
        let app = Router::new().route("/.well-known/tr-registry.json", get(discovery));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        let registry_url = format!("http://{}", bound);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("dst");
        let err = run_install(
            "alice/demo@0.1.0",
            Some(target),
            Some(registry_url),
            true,
        )
        .await
        .expect_err("foreign registry format must be refused");
        server.abort();
        assert!(
            format!("{err}").contains("unsupported format_version"),
            "got: {err}"
        );
    }

    // -------------------------------------------------------------------------
    // Trust-verification integration (Phase F Step 5)
    //
    // These tests exercise the policy + extraction layer in
    // `install_with_verifier`. The verdict semantics themselves are
    // already covered by `crates/tr-verify/tests/verify_smoke.rs`; these
    // tests confirm that each verdict maps to the right `InstallRefused`
    // exit code and that the file extraction step does NOT run on
    // refusal.
    // -------------------------------------------------------------------------

    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::Arc;
    use std::time::Duration;
    use tr_format::TrustTier;
    use tr_revocation::{
        Advisory, Authority, CacheConfig, PinnedKey, Reason, RevocationCache, Snapshot,
    };
    use tr_verify::{AuthorKeyStore, TrustedAuthorKey, Verifier, VerifierConfig};

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn fresh_revocation_cache(
        cache_dir: PathBuf,
        revocation_key: &SigningKey,
        revoked_hashes: &[&str],
    ) -> Arc<RevocationCache> {
        let mut snap = Snapshot {
            schema_version: "1.0.0".into(),
            generated_at: 1_745_100_000,
            generated_by: "hub.example".into(),
            full_list: true,
            entries: revoked_hashes
                .iter()
                .map(|h| Advisory {
                    content_hash: format!("blake3:{h}"),
                    pack: "alice/thesis".into(),
                    version: "0.1.0".into(),
                    reason: Reason::Malware,
                    revoked_at: 1_745_099_000,
                    authority: Authority::HubScanner,
                    details_url: "https://example/advisory".into(),
                })
                .collect(),
            signature: String::new(),
            signing_key_id: "rev-test".into(),
            next_poll_hint_sec: 3_600,
        };
        let payload = snap.canonical_bytes_for_signing().unwrap();
        let sig = revocation_key.sign(&payload);
        snap.signature = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(
            cache_dir.join("snapshot.json"),
            serde_json::to_vec(&snap).unwrap(),
        )
        .unwrap();
        fs::write(
            cache_dir.join("snapshot.fetched_at"),
            unix_now().to_string(),
        )
        .unwrap();

        Arc::new(RevocationCache::new(CacheConfig {
            registry_url: url::Url::parse("https://hub.example/").unwrap(),
            cache_dir,
            fresh_ttl: Duration::from_secs(60 * 60),
            stale_grace: Duration::from_secs(7 * 24 * 60 * 60),
            trusted_keys: vec![PinnedKey {
                key_id: "rev-test".into(),
                ed25519_public: revocation_key.verifying_key().to_bytes(),
            }],
            max_snapshot_bytes: 50 * 1024 * 1024,
        }))
    }

    fn build_unsigned_pack(name: &str, tier: TrustTier) -> Vec<u8> {
        let mut manifest = Manifest::new(name, Version::parse("0.1.0").unwrap(), "Apache-2.0");
        manifest.trust_tier = tier;
        let mut pb = PackBuilder::new(manifest);
        pb.put_text("artifacts/card.md", "# Hello").unwrap();
        pb.build().unwrap()
    }

    fn build_t1_signed_pack(name: &str, key: &SigningKey, key_id: &str) -> Vec<u8> {
        let mut manifest = Manifest::new(name, Version::parse("0.1.0").unwrap(), "Apache-2.0");
        manifest.trust_tier = TrustTier::T1;
        manifest.authors = vec![key_id.to_string()];

        let canonical = manifest.canonical_bytes_for_hashing().unwrap();
        let signature = key.sign(&canonical);

        let mut pb = PackBuilder::new(manifest).keep_generated_at();
        pb.put_file("signatures/author.sig", &signature.to_bytes())
            .unwrap();
        pb.put_text("artifacts/card.md", "# T1 demo").unwrap();
        pb.build().unwrap()
    }

    fn write_pack_to_disk(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn refuse_exit_code(err: &anyhow::Error) -> i32 {
        err.downcast_ref::<InstallRefused>()
            .map(|r| r.exit_code)
            .unwrap_or_else(|| panic!("expected InstallRefused, got: {err}"))
    }

    #[tokio::test]
    async fn install_succeeds_for_t1_pack_with_trusted_author_key() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(20);
        let cache = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

        let author = signing_key(21);
        let pack_bytes = build_t1_signed_pack("alice/thesis", &author, "alice");
        let pack_path = write_pack_to_disk(tmp.path(), "alice-thesis.tr", &pack_bytes);

        let store = Arc::new(AuthorKeyStore::with_keys([TrustedAuthorKey {
            key_id: "alice".into(),
            ed25519_public: author.verifying_key().to_bytes(),
        }]));
        let verifier = Verifier::new(VerifierConfig {
            revocation: cache,
            author_keys: store,
            require_min_tier: TrustTier::T1,
            allow_unsigned: false,
        });

        let target = tmp.path().join("install");
        install_with_verifier(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &verifier,
        )
        .await
        .expect("T1 install with trusted key should succeed");

        assert!(target.join("manifest.json").exists());
        assert!(target.join(".thinkingroot/artifacts/card.md").exists());
    }

    #[tokio::test]
    async fn install_refuses_unsigned_pack_with_exit_unsigned() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(22);
        let cache = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

        let pack_bytes = build_unsigned_pack("alice/demo", TrustTier::T0);
        let pack_path = write_pack_to_disk(tmp.path(), "alice-demo.tr", &pack_bytes);

        let verifier = Verifier::new(VerifierConfig {
            revocation: cache,
            author_keys: Arc::new(AuthorKeyStore::empty()),
            require_min_tier: TrustTier::T1,
            allow_unsigned: false,
        });

        let target = tmp.path().join("install");
        let err = install_with_verifier(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &verifier,
        )
        .await
        .expect_err("unsigned pack should be refused");

        assert_eq!(refuse_exit_code(&err), EXIT_UNSIGNED);
        // Refusal must NOT extract files.
        assert!(!target.exists(), "target dir should not exist on refusal");
    }

    #[tokio::test]
    async fn install_refuses_revoked_pack_with_exit_revoked() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(23);

        let pack_bytes = build_unsigned_pack("alice/bad", TrustTier::T0);
        let pack_path = write_pack_to_disk(tmp.path(), "alice-bad.tr", &pack_bytes);
        let pack = tr_reader::read_bytes(&pack_bytes).unwrap();

        let cache = fresh_revocation_cache(
            tmp.path().join("rev"),
            &rev_key,
            &[&pack.content_bytes_hash],
        );
        let verifier = Verifier::new(VerifierConfig {
            revocation: cache,
            author_keys: Arc::new(AuthorKeyStore::empty()),
            require_min_tier: TrustTier::T0,
            allow_unsigned: true,
        });

        let target = tmp.path().join("install");
        let err = install_with_verifier(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &verifier,
        )
        .await
        .expect_err("revoked pack should be refused");

        assert_eq!(refuse_exit_code(&err), EXIT_REVOKED);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn install_refuses_t1_with_unknown_author_key() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(24);
        let cache = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

        let author = signing_key(25);
        let pack_bytes = build_t1_signed_pack("alice/thesis", &author, "alice-unknown");
        let pack_path = write_pack_to_disk(tmp.path(), "alice-unknown.tr", &pack_bytes);

        let verifier = Verifier::new(VerifierConfig {
            revocation: cache,
            author_keys: Arc::new(AuthorKeyStore::empty()),
            require_min_tier: TrustTier::T1,
            allow_unsigned: false,
        });

        let target = tmp.path().join("install");
        let err = install_with_verifier(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &verifier,
        )
        .await
        .expect_err("unknown-key T1 pack should be refused");

        assert_eq!(refuse_exit_code(&err), EXIT_KEY_UNKNOWN);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn install_refuses_t2_pack_with_exit_unsupported() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(26);
        let cache = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

        let pack_bytes = build_unsigned_pack("alice/sigstore-demo", TrustTier::T2);
        let pack_path = write_pack_to_disk(tmp.path(), "alice-sigstore.tr", &pack_bytes);

        let verifier = Verifier::new(VerifierConfig {
            revocation: cache,
            author_keys: Arc::new(AuthorKeyStore::empty()),
            require_min_tier: TrustTier::T1,
            allow_unsigned: false,
        });

        let target = tmp.path().join("install");
        let err = install_with_verifier(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &verifier,
        )
        .await
        .expect_err("T2 pack should be refused until Step 4b");

        assert_eq!(refuse_exit_code(&err), EXIT_UNSUPPORTED);
        assert!(!target.exists());
    }
}
