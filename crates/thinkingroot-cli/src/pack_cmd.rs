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
//! Trust verification is wired through [`tr_verify::verify_v3_pack_with_revocation`]:
//! every `root install` checks the pack hash, DSSE signature, in-toto
//! subject digest, and revocation deny-list before extracting any
//! source files.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use thinkingroot_core::types::ContentHash;
use tr_format::{
    read_v3_pack, ClaimRecord, ManifestV3, V3Pack, V3PackBuilder, Version,
};
use tr_revocation::{CacheConfig, RevocationCache};
use tr_verify::{V3TamperedKind, V3Verdict, verify_v3_pack};

use crate::resolver::{HttpDirectUrlResolver, HttpRegistryResolver, LocalFsResolver, PackResolver};

// -----------------------------------------------------------------------------
// Exit codes for `root install` verdict refusals (Phase F design §2.3).
// -----------------------------------------------------------------------------

/// Pack is unsigned and policy required a signature.
pub const EXIT_UNSIGNED: i32 = 70;
/// Recomputed pack hash diverged from the manifest's declared
/// `pack_hash`, OR the DSSE signature failed to verify.
pub const EXIT_TAMPERED: i32 = 71;
/// Pack hash is on the registry's signed revocation deny-list.
pub const EXIT_REVOKED: i32 = 72;

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

/// Run `root pack`. v3-only: there is no `--format` option since
/// the v1 wire format was deleted in the legacy cleanup commits.
pub fn run_pack(
    workspace: &Path,
    out: Option<PathBuf>,
    name_override: Option<String>,
    version_override: Option<String>,
    license_override: Option<String>,
    description_override: Option<String>,
    sign_key_path: Option<&Path>,
    sign_keyless: bool,
) -> Result<()> {
    run_pack_v3(
        workspace,
        out,
        name_override,
        version_override,
        license_override,
        description_override,
        sign_key_path,
        sign_keyless,
    )
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
    sign_key_path: Option<&Path>,
    sign_keyless: bool,
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

    // 1. Manifest scaffolding from Pack.toml + CLI overrides.
    let mut manifest = build_manifest_v3(
        workspace,
        name_override,
        version_override,
        license_override,
        description_override,
    )?;
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

    // Capture identity fields up front since `builder` takes ownership
    // of the manifest below, and we need `name`/`version` for both the
    // output filename and the success log line.
    let pack_name = manifest.name.clone();
    let pack_version = manifest.version.clone();

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

    // 5. Seal the pack. If a signing key was supplied, drive
    //    `build_signed` so a Sigstore Bundle is appended as the 4th
    //    outer-tar entry. Otherwise emit unsigned — `root verify`
    //    reports the Unsigned verdict for these.
    let out_path = out.unwrap_or_else(|| {
        let owner_slug = pack_name.replace('/', "-");
        workspace.join(format!("{owner_slug}-{pack_version}.tr"))
    });
    let pack_filename = out_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("package.tr")
        .to_string();
    let bytes = if let Some(key_path) = sign_key_path {
        let key = load_signing_key(key_path)
            .with_context(|| format!("load signing key from {}", key_path.display()))?;
        builder
            .build_signed(&key, &pack_filename)
            .map_err(|e| anyhow!("build signed .tr: {e}"))?
    } else if sign_keyless {
        run_keyless_signing(builder, &pack_filename)?
    } else {
        builder.build().map_err(|e| anyhow!("build .tr: {e}"))?
    };
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&out_path, &bytes).with_context(|| format!("write {}", out_path.display()))?;

    let signed_label = if sign_key_path.is_some() {
        " signed"
    } else if sign_keyless {
        " signed (keyless)"
    } else {
        ""
    };
    println!(
        "  packed{} {} {} (tr/3 — {} files, {} source bytes, {} claims, {} pack bytes) -> {}",
        signed_label,
        pack_name,
        pack_version,
        packed_paths.len(),
        source_bytes_added,
        claim_count,
        bytes.len(),
        out_path.display()
    );
    Ok(())
}

/// Load an Ed25519 signing key from disk. Two file formats accepted:
/// - 32 raw bytes (the most compact form; `dd if=/dev/urandom bs=32
///   count=1 of=key.bin` produces one).
/// - 64 hex chars on a single line (with optional trailing newline) —
///   easier to inspect and to commit-checkable for test fixtures.
///
/// Returns a clear error when neither shape matches so the user can
/// regenerate without guessing.
fn load_signing_key(path: &Path) -> Result<ed25519_dalek::SigningKey> {
    use ed25519_dalek::SigningKey;
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        return Ok(SigningKey::from_bytes(&arr));
    }
    // Try hex with possible trailing newline.
    let trimmed = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("key file is neither 32 raw bytes nor UTF-8 hex"))?
        .trim();
    if trimmed.len() == 64 {
        let mut arr = [0u8; 32];
        for i in 0..32 {
            let byte_str = &trimmed[i * 2..i * 2 + 2];
            arr[i] = u8::from_str_radix(byte_str, 16)
                .map_err(|_| anyhow!("key file has invalid hex at byte {i}"))?;
        }
        return Ok(SigningKey::from_bytes(&arr));
    }
    Err(anyhow!(
        "signing key must be 32 raw bytes or 64 hex chars; got {} bytes",
        bytes.len()
    ))
}

/// Drive Sigstore-public-good keyless DSSE signing for `root pack
/// --sign-keyless`. Returns the outer `.tr` bytes (with `signature.sig`
/// as the 4th tar entry) ready for disk-write.
///
/// OIDC token sourcing:
///
/// 1. `$TR_OIDC_TOKEN` — preferred for headless / CI use. The CLI does
///    no further verification on the token; sigstore-rs's
///    `IdentityToken::try_from(&str)` enforces `aud == "sigstore"`,
///    Fulcio enforces issuer and challenge.
/// 2. Otherwise, [`tr_sigstore::live::browser_oidc_flow`] runs the
///    interactive PKCE redirect against Sigstore-public-good's OIDC
///    issuer (`oauth2.sigstore.dev/auth`).
///
/// We're called from the sync `run_pack_v3` body, which itself runs
/// inside `async_main`'s tokio multi-thread runtime. The signing
/// closure uses `tokio::task::block_in_place` + `Handle::current()
/// .block_on` to drive the async [`tr_sigstore::live::sign_canonical_bytes_keyless`]
/// without restructuring the surrounding sync code.
fn run_keyless_signing(
    builder: tr_format::V3PackBuilder,
    pack_filename: &str,
) -> Result<Vec<u8>> {
    use std::time::SystemTime;
    use tr_sigstore::live::{
        IdentityToken, SignKeylessOptions, browser_oidc_flow, sign_canonical_bytes_keyless,
    };

    // Step 1: obtain the OIDC id_token. Env var preferred; browser
    // flow as the fallback.
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
    // Round-trip back to the JWT string — the keyless signer parses
    // the JWT itself for the challenge claim and to construct the
    // `CoreIdToken` openidconnect type. Display impl on
    // `IdentityToken` returns the original token string.
    let jwt = token.to_string();

    // Step 2: build the signing closure. The closure receives the
    // canonical pack bytes (BLAKE3 input per spec §3.1) and returns a
    // `SigstoreBundle` ready to embed in the outer tar.
    let signer = move |canonical_bytes: &[u8],
                       _pack_hash: &str,
                       pack_filename: &str|
          -> std::result::Result<tr_sigstore::SigstoreBundle, anyhow::Error> {
        // From inside the multi-thread tokio runtime, `block_in_place`
        // tells tokio "this thread is going to do blocking work, move
        // other tasks off it"; then `Handle::current().block_on` runs
        // the async signer to completion synchronously. This works
        // because `async_main` always runs us on a multi-thread
        // runtime — `Builder::new_multi_thread()` in `main.rs`.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                sign_canonical_bytes_keyless(
                    canonical_bytes,
                    pack_filename,
                    &jwt,
                    SystemTime::now(),
                    SignKeylessOptions::default(),
                )
                .await
                .map_err(|e| anyhow!("keyless sign: {e}"))
            })
        })
    };

    builder
        .build_with_signer(signer, pack_filename)
        .map_err(|e| anyhow!("build keyless-signed .tr: {e}"))
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
pub fn run_verify(
    pack_path: &Path,
    allow_unsigned: bool,
    revocation_check: bool,
    registry_override: Option<String>,
) -> Result<i32> {
    let bytes = fs::read(pack_path)
        .with_context(|| format!("read {}", pack_path.display()))?;
    let pack = read_v3_pack(&bytes).map_err(|e| anyhow!("parse {}: {e}", pack_path.display()))?;

    // Two paths: with revocation (async, consults the cached deny-
    // list) and without (sync, fully offline). Both produce the same
    // V3Verdict shape; `Revoked` is only reachable from the async
    // path. The sync path is the right choice for air-gapped CI / for
    // verifying packs the deny-list can't speak about (private packs
    // signed with author keys outside any registry).
    let verdict = if revocation_check {
        let cache = build_revocation_cache(registry_override)
            .context("construct revocation cache")?;
        // `run_verify` is sync but called from async_main's tokio
        // runtime — `block_in_place` + `Handle::current().block_on`
        // drives the async revocation check synchronously. Same idiom
        // we use for `--sign-keyless` in run_keyless_signing.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                tr_verify::verify_v3_pack_with_revocation(&pack, &cache),
            )
        })
    } else {
        verify_v3_pack(&pack)
    };

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
            if revocation_check {
                println!("  revocation: not on deny-list");
            } else {
                println!("  revocation: skipped (--no-revocation-check)");
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
        V3Verdict::Revoked(details) => {
            let advisory = &details.advisory;
            eprintln!(
                "  revoked: {} {} — reason: {:?}",
                advisory.pack, advisory.version, advisory.reason
            );
            eprintln!("  revoked at: unix {}", advisory.revoked_at);
            eprintln!("  authority: {:?}", advisory.authority);
            eprintln!("  details: {}", advisory.details_url);
            Ok(EXIT_REVOKED)
        }
    }
}

/// Build a [`tr_revocation::RevocationCache`] using the same defaults
/// the v1 install path uses: production registry URL (overridable),
/// platform default cache directory, 60-min fresh TTL, 7-day stale
/// grace.
fn build_revocation_cache(
    registry_override: Option<String>,
) -> Result<tr_revocation::RevocationCache> {
    let registry_url_str = match registry_override {
        Some(s) => s,
        None => load_default_registry()?,
    };
    let registry_url: url::Url = registry_url_str
        .parse()
        .with_context(|| format!("parse registry URL `{registry_url_str}`"))?;

    let cache_dir = tr_revocation::default_cache_dir().ok_or_else(|| {
        anyhow!("could not determine platform cache directory for revocation snapshots")
    })?;

    let config = tr_revocation::CacheConfig::defaults_for(registry_url, cache_dir);
    Ok(tr_revocation::RevocationCache::new(config))
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

/// Resolve the v3 manifest by combining `Pack.toml` (if present) with
/// CLI overrides. `name` and `version` are required; `license` is
/// optional in v3 but warned-on-missing for distribution-friendly
/// packs.
fn build_manifest_v3(
    workspace: &Path,
    name_override: Option<String>,
    version_override: Option<String>,
    license_override: Option<String>,
    description_override: Option<String>,
) -> Result<ManifestV3> {
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
    let version = Version::parse(&version_str)
        .with_context(|| format!("parse version `{}` (must be semver)", version_str))?;

    let mut manifest = ManifestV3::new(name, version);
    if let Some(license) = license_override.or_else(|| from_file.as_ref().map(|p| p.license.clone()))
    {
        manifest.license = Some(license);
    }
    if let Some(d) = description_override.or_else(|| from_file.and_then(|p| p.description)) {
        manifest.description = Some(d);
    }
    // Validate eagerly so user gets the error before the slow walk.
    manifest
        .validate()
        .map_err(|e| anyhow!("invalid manifest: {e}"))?;
    Ok(manifest)
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
    let pack = read_v3_pack(&bytes).map_err(|e| anyhow!("read .tr: {e}"))?;
    let preview =
        tr_render::render_preview(&pack).map_err(|e| anyhow!("render preview: {e}"))?;

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
    let registry_url_for_cache = match (&install_ref, &registry_override) {
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
    let cache = build_default_revocation_cache(&registry_url_for_cache)?;
    install_with_revocation_cache(
        reference,
        target,
        registry_override,
        &cache,
        is_remote,
        allow_unsigned,
    )
    .await
}

/// Test-friendly entry point that takes a pre-built
/// [`RevocationCache`]. Production callers use [`run_install`]; tests
/// build a cache against a tmpdir so they don't touch `~/.cache`.
pub async fn install_with_revocation_cache(
    reference: &str,
    target: Option<PathBuf>,
    registry_override: Option<String>,
    cache: &RevocationCache,
    is_remote: bool,
    user_allow_unsigned: bool,
) -> Result<()> {
    let install_ref = parse_install_ref(reference)?;
    let resolver = build_resolver(install_ref, registry_override)?;
    let bytes = resolver.resolve().await?;
    install_from_bytes(&bytes, target, cache, is_remote, user_allow_unsigned).await
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

/// Build a [`RevocationCache`] using the same defaults as `root verify`:
/// caller-supplied registry URL, platform cache directory, 60-min fresh
/// TTL, 7-day stale grace.
fn build_default_revocation_cache(registry_url: &str) -> Result<RevocationCache> {
    let url = url::Url::parse(registry_url)
        .with_context(|| format!("parse registry url `{registry_url}`"))?;
    let cache_dir = tr_revocation::default_cache_dir()
        .ok_or_else(|| anyhow!("no platform cache directory available"))?;
    Ok(RevocationCache::new(CacheConfig::defaults_for(
        url, cache_dir,
    )))
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

/// Verify a `.tr` byte slice against the supplied revocation cache and,
/// on a `Verified` verdict, extract the source archive + manifest +
/// claims into the target directory's `.thinkingroot/`. Used by both
/// [`run_install`] (production) and
/// [`install_with_revocation_cache`] (tests).
///
/// `is_remote` flips the default-allow-unsigned behaviour: local
/// installs accept unsigned packs by default, remote installs (HTTPS
/// or registry) require either a signature or
/// `user_allow_unsigned == true`.
async fn install_from_bytes(
    bytes: &[u8],
    target: Option<PathBuf>,
    cache: &RevocationCache,
    is_remote: bool,
    user_allow_unsigned: bool,
) -> Result<()> {
    let pack = read_v3_pack(bytes).map_err(|e| anyhow!("read .tr: {e}"))?;
    let verdict =
        tr_verify::verify_v3_pack_with_revocation(&pack, cache).await;
    let allow_unsigned = user_allow_unsigned || !is_remote;
    enforce_v3_verdict(&pack, &verdict, allow_unsigned)?;
    extract_v3_pack_to_target(&pack, target)
}

/// Map a [`V3Verdict`] to either `Ok(())` (continue install) or an
/// [`InstallRefused`] error carrying the right exit code + message.
fn enforce_v3_verdict(pack: &V3Pack, verdict: &V3Verdict, allow_unsigned: bool) -> Result<()> {
    match verdict {
        V3Verdict::Verified {
            identity,
            rekor_log_index,
            signed_at,
        } => {
            tracing::info!(
                identity = ?identity,
                rekor_log_index = ?rekor_log_index,
                signed_at = %signed_at,
                "verified {} {}",
                pack.manifest.name,
                pack.manifest.version
            );
            Ok(())
        }
        V3Verdict::Unsigned => {
            if allow_unsigned {
                tracing::info!(
                    "installing unsigned pack {} {} (allow-unsigned)",
                    pack.manifest.name,
                    pack.manifest.version
                );
                Ok(())
            } else {
                Err(InstallRefused {
                    exit_code: EXIT_UNSIGNED,
                    message: format!(
                        "✗ refusing to install unsigned pack `{}` — pass --allow-unsigned to override",
                        pack.manifest.name
                    ),
                }
                .into())
            }
        }
        V3Verdict::Tampered(kind) => Err(InstallRefused {
            exit_code: EXIT_TAMPERED,
            message: format_v3_tampered(&pack.manifest, kind),
        }
        .into()),
        V3Verdict::Revoked(d) => Err(InstallRefused {
            exit_code: EXIT_REVOKED,
            message: format_revoked(d),
        }
        .into()),
    }
}

fn format_v3_tampered(manifest: &ManifestV3, kind: &V3TamperedKind) -> String {
    let detail = match kind {
        V3TamperedKind::PackHashMismatch {
            declared,
            recomputed,
        } => format!(
            "pack hash mismatch (manifest declares `{declared}`, recomputed `{recomputed}`)"
        ),
        V3TamperedKind::SignatureFailed { reason } => {
            format!("signature failed: {reason}")
        }
    };
    format!("✗ pack `{}` failed integrity check: {detail}", manifest.name)
}

fn format_revoked(d: &tr_verify::RevokedDetails) -> String {
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

/// Extract a verified v3 pack's source archive + manifest + claims
/// into `target`. The inner `source.tar.zst` is decompressed and walked,
/// emitting one file per entry under `<target>/.thinkingroot/sources/`.
/// `manifest.toml` and `claims.jsonl` land under `<target>/.thinkingroot/`
/// directly so consumers can re-parse them via `tr_format`.
fn extract_v3_pack_to_target(pack: &V3Pack, target: Option<PathBuf>) -> Result<()> {
    let manifest = &pack.manifest;
    let target_dir = match target {
        Some(t) => t,
        None => default_install_dir(manifest)?,
    };
    let engine_dir = target_dir.join(".thinkingroot");
    let sources_dir = engine_dir.join("sources");
    fs::create_dir_all(&sources_dir)
        .with_context(|| format!("create {}", sources_dir.display()))?;

    // Decompress the inner source.tar.zst and walk its entries.
    let decompressed = zstd::stream::decode_all(&pack.source_archive[..])
        .map_err(|e| anyhow!("decompress source.tar.zst: {e}"))?;
    let mut archive = tar::Archive::new(std::io::Cursor::new(decompressed));
    let mut count = 0usize;
    for entry in archive
        .entries()
        .map_err(|e| anyhow!("walk source archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| anyhow!("walk source archive: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| anyhow!("entry path: {e}"))?
            .into_owned();
        let dest = sources_dir.join(&path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut buf = Vec::with_capacity(entry.size() as usize);
        std::io::Read::read_to_end(&mut entry, &mut buf)
            .map_err(|e| anyhow!("read entry {}: {e}", path.display()))?;
        fs::write(&dest, &buf)
            .with_context(|| format!("write {}", dest.display()))?;
        count += 1;
    }

    // Drop the manifest + claims alongside the sources for downstream
    // tooling. Manifest is re-emitted as canonical TOML so the on-disk
    // copy is byte-identical to what the pack carried.
    fs::write(engine_dir.join("manifest.toml"), manifest.to_canonical_toml())
        .with_context(|| format!("write {}", engine_dir.join("manifest.toml").display()))?;
    fs::write(engine_dir.join("claims.jsonl"), &pack.claims_jsonl)
        .with_context(|| format!("write {}", engine_dir.join("claims.jsonl").display()))?;

    println!(
        "  installed {} {} ({} source files, {} claims) -> {}",
        manifest.name,
        manifest.version,
        count,
        manifest.claim_count.unwrap_or(0),
        target_dir.display()
    );
    Ok(())
}

/// Default install directory: `~/.thinkingroot/packs/<name>/<version>/`.
/// `<name>` is `owner/slug`, which becomes a two-level subpath on disk.
fn default_install_dir(manifest: &ManifestV3) -> Result<PathBuf> {
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

    /// Build a fake v3-shape compiled workspace with a real CozoDB +
    /// source-byte store. Returns the workspace root.
    ///
    /// The shape this produces matches what `root compile` leaves
    /// behind:
    /// - `.thinkingroot/graph.db` — CozoDB with one source + one claim
    ///   carrying a non-zero `(byte_start, byte_end)` triple (W1
    ///   contract).
    /// - `.thinkingroot/rooting/sources/...` — durable source-byte
    ///   store entry the v3 pack writer reads from.
    /// - `Pack.toml` at workspace root.
    fn fake_v3_workspace(dir: &Path) -> PathBuf {
        use thinkingroot_core::types::{ContentHash, SourceSpan, SourceType, WorkspaceId};
        use thinkingroot_core::{Claim, ClaimType, Source};
        use thinkingroot_graph::graph::GraphStore;
        use thinkingroot_rooting::{FileSystemSourceStore, SourceByteStore};

        let workspace = dir.to_path_buf();
        let engine = workspace.join(".thinkingroot");
        fs::create_dir_all(&engine).unwrap();

        let graph = GraphStore::init(&engine).unwrap();
        let source_store = FileSystemSourceStore::new(&engine).unwrap();

        // Synthetic source — a small Rust file. The `file://` URI is
        // what the workspace_relative_pack_path mapping consumes.
        let source_text = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let abs_path = workspace.join("src").join("lib.rs");
        fs::create_dir_all(abs_path.parent().unwrap()).unwrap();
        fs::write(&abs_path, source_text).unwrap();
        let uri = format!("file://{}", abs_path.display());
        let content_hash = ContentHash::from_bytes(source_text.as_bytes());
        let source = Source::new(uri.clone(), SourceType::File).with_hash(content_hash.clone());
        graph.insert_source(&source).unwrap();
        source_store
            .put(source.id, &content_hash, source_text.as_bytes())
            .unwrap();

        // One claim with byte ranges into the source. The byte slice
        // `[3, 35)` covers `add(a: i32, b: i32) -> i32` — the
        // function signature.
        let claim = Claim::new(
            "add takes two i32 and returns their sum",
            ClaimType::Definition,
            source.id,
            WorkspaceId::new(),
        )
        .with_span(SourceSpan::bytes(3, 35));
        graph.insert_claim(&claim).unwrap();

        fs::write(
            workspace.join("Pack.toml"),
            r#"[pack]
name = "alice/v3-e2e"
version = "1.0.0"
license = "MIT"
description = "v3 lifecycle round-trip test."
"#,
        )
        .unwrap();
        workspace
    }

    #[tokio::test]
    async fn v3_pack_lifecycle_round_trips_byte_ranges() {
        use tr_format::read_v3_pack;

        let tmp = tempdir().unwrap();
        let workspace = fake_v3_workspace(tmp.path());

        // 1. Run pack — same code path as `root pack --format=tr/3`.
        let out_tr = workspace.join("alice-v3-e2e-1.0.0.tr");
        run_pack(
            &workspace,
            Some(out_tr.clone()),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        assert!(out_tr.exists(), "v3 pack file not produced");

        // 2. Read back via the v3 reader (same code path as
        //    `root verify` parses with).
        let bytes = fs::read(&out_tr).unwrap();
        let pack = read_v3_pack(&bytes).expect("v3 pack must parse");

        // 3. Manifest invariants.
        assert_eq!(pack.manifest.name, "alice/v3-e2e");
        assert_eq!(pack.manifest.format_version, "tr/3");
        assert!(
            pack.manifest.pack_hash.starts_with("blake3:"),
            "pack_hash must be set: {:?}",
            pack.manifest.pack_hash
        );
        assert!(pack.manifest.source_hash.starts_with("blake3:"));
        assert!(pack.manifest.claims_hash.starts_with("blake3:"));
        assert_eq!(pack.manifest.source_files, Some(1));
        assert_eq!(pack.manifest.claim_count, Some(1));
        assert_eq!(pack.manifest.license.as_deref(), Some("MIT"));
        assert_eq!(
            pack.manifest.description.as_deref(),
            Some("v3 lifecycle round-trip test.")
        );
        assert!(pack.signature.is_none(), "unsigned pack today");

        // 4. Pack-hash chain. The verifier's first-line defence —
        //    catches any tampering between sign-time and verify-time.
        assert_eq!(
            pack.recompute_pack_hash(),
            pack.manifest.pack_hash,
            "recipe-recomputed hash must match manifest's declared hash"
        );

        // 5. Claims body sanity. JSONL with stable field order, byte
        //    ranges populated.
        let claims_text =
            std::str::from_utf8(&pack.claims_jsonl).expect("claims.jsonl is UTF-8");
        let lines: Vec<&str> = claims_text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "exactly one claim emitted");
        let line: serde_json::Value =
            serde_json::from_str(lines[0]).expect("claims.jsonl line parses");
        assert_eq!(line["start"], 3);
        assert_eq!(line["end"], 35);
        assert_eq!(
            line["stmt"],
            "add takes two i32 and returns their sum"
        );
        assert!(
            line["file"].as_str().unwrap().ends_with("src/lib.rs"),
            "file should resolve to a workspace-relative path"
        );

        // 6. Verify (unsigned path) — confirms the pack itself is
        //    well-formed even before W3.5 wires Fulcio signing.
        use tr_verify::{V3Verdict, verify_v3_pack};
        let verdict = verify_v3_pack(&pack);
        assert!(
            matches!(verdict, V3Verdict::Unsigned),
            "unsigned pack should report Unsigned verdict, got {verdict:?}"
        );
    }

    #[tokio::test]
    async fn v3_pack_signed_round_trips_through_verify() {
        use tr_format::read_v3_pack;
        use tr_verify::{V3Verdict, verify_v3_pack};

        let tmp = tempdir().unwrap();
        let workspace = fake_v3_workspace(tmp.path());

        // Write a deterministic Ed25519 key as 64 hex chars.
        let key_path = tmp.path().join("signing.key");
        let hex_key: String = (0..32).map(|i| format!("{:02x}", i + 1)).collect();
        fs::write(&key_path, &hex_key).unwrap();

        let out_tr = workspace.join("alice-v3-e2e-1.0.0.tr");
        run_pack(
            &workspace,
            Some(out_tr.clone()),
            None,
            None,
            None,
            None,
            Some(&key_path),
            false,
        )
        .unwrap();

        // Read back: the pack now carries a signature.sig.
        let bytes = fs::read(&out_tr).unwrap();
        let pack = read_v3_pack(&bytes).expect("signed pack must parse");
        assert!(pack.signature.is_some(), "signed pack must carry signature.sig");

        // Library-level verify: should be Verified (self-signed).
        let verdict = verify_v3_pack(&pack);
        match verdict {
            V3Verdict::Verified {
                identity,
                rekor_log_index,
                ..
            } => {
                assert!(identity.is_none(), "self-signed has no Sigstore identity");
                assert!(rekor_log_index.is_none(), "self-signed has no Rekor entry");
            }
            other => panic!("expected Verified, got {other:?}"),
        }

        // CLI exit-code path: signed pack returns 0 without --allow-unsigned.
        let code = run_verify(&out_tr, false, false, None).unwrap();
        assert_eq!(code, 0, "signed pack should exit 0");
    }

    #[tokio::test]
    async fn signing_key_loader_accepts_raw_and_hex() {
        let tmp = tempdir().unwrap();
        // Raw 32 bytes.
        let raw_path = tmp.path().join("raw.key");
        let raw: Vec<u8> = (0u8..32u8).collect();
        fs::write(&raw_path, &raw).unwrap();
        let raw_key = load_signing_key(&raw_path).expect("raw bytes accepted");
        // Hex form of same bytes.
        let hex_path = tmp.path().join("hex.key");
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        fs::write(&hex_path, &hex).unwrap();
        let hex_key = load_signing_key(&hex_path).expect("hex accepted");
        assert_eq!(
            raw_key.verifying_key().to_bytes(),
            hex_key.verifying_key().to_bytes(),
            "raw and hex of same bytes must produce the same key"
        );

        // Wrong length is a clean error.
        let bad_path = tmp.path().join("bad.key");
        fs::write(&bad_path, b"short").unwrap();
        assert!(load_signing_key(&bad_path).is_err());
    }

    #[tokio::test]
    async fn v3_pack_then_verify_via_run_verify_cli_path() {
        // Variant that goes through the `root verify` CLI exit-code
        // surface end-to-end.
        let tmp = tempdir().unwrap();
        let workspace = fake_v3_workspace(tmp.path());

        let out_tr = workspace.join("alice-v3-e2e-1.0.0.tr");
        run_pack(
            &workspace,
            Some(out_tr.clone()),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();

        // Without --allow-unsigned, exit code is EXIT_UNSIGNED = 70.
        let code = run_verify(&out_tr, false, false, None).unwrap();
        assert_eq!(code, EXIT_UNSIGNED);

        // With --allow-unsigned, exit code is 0.
        let code = run_verify(&out_tr, true, false, None).unwrap();
        assert_eq!(code, 0);
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


    // -------------------------------------------------------------------------
    // V3 install policy + extraction integration tests.
    //
    // The verdict semantics themselves are covered by
    // `crates/tr-verify/src/v3.rs`. These tests confirm that each
    // V3Verdict variant maps to the right InstallRefused exit code and
    // that the extraction step does NOT run on refusal.
    // -------------------------------------------------------------------------

    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use std::time::Duration;
    use tr_revocation::{
        Advisory, Authority, CacheConfig, PinnedKey, Reason, RevocationCache, Snapshot,
    };

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Pre-seed a `RevocationCache` with a signed snapshot listing
    /// `revoked_hashes` so `load_or_refresh` returns it on the
    /// fresh-cache path without any HTTP round-trip.
    fn fresh_revocation_cache(
        cache_dir: PathBuf,
        revocation_key: &SigningKey,
        revoked_hashes: &[&str],
    ) -> RevocationCache {
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

        RevocationCache::new(CacheConfig {
            registry_url: url::Url::parse("https://hub.example/").unwrap(),
            cache_dir,
            fresh_ttl: Duration::from_secs(60 * 60),
            stale_grace: Duration::from_secs(7 * 24 * 60 * 60),
            trusted_keys: vec![PinnedKey {
                key_id: "rev-test".into(),
                ed25519_public: revocation_key.verifying_key().to_bytes(),
            }],
            max_snapshot_bytes: 50 * 1024 * 1024,
        })
    }

    fn build_unsigned_v3_pack(name: &str) -> Vec<u8> {
        let manifest = ManifestV3::new(name, Version::parse("0.1.0").unwrap());
        let mut b = V3PackBuilder::new(manifest);
        b.add_source_file("a.md", b"alpha\n").unwrap();
        b.add_claim(ClaimRecord::new(
            "c-1",
            "alpha is the first letter",
            vec!["alpha".into()],
            "a.md",
            0,
            5,
        ));
        b.build().unwrap()
    }

    fn build_signed_v3_pack(name: &str, key: &SigningKey) -> Vec<u8> {
        let manifest = ManifestV3::new(name, Version::parse("0.1.0").unwrap());
        let mut b = V3PackBuilder::new(manifest);
        b.add_source_file("a.md", b"alpha\n").unwrap();
        b.add_claim(ClaimRecord::new(
            "c-1",
            "alpha is the first letter",
            vec!["alpha".into()],
            "a.md",
            0,
            5,
        ));
        let pack_filename = format!("{}-0.1.0.tr", name.replace('/', "-"));
        b.build_signed(key, &pack_filename).unwrap()
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
    async fn install_succeeds_for_signed_v3_pack() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(20);
        let cache = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

        let signing = signing_key(21);
        let pack_bytes = build_signed_v3_pack("alice/thesis", &signing);
        let pack_path = write_pack_to_disk(tmp.path(), "alice-thesis.tr", &pack_bytes);

        let target = tmp.path().join("install");
        install_with_revocation_cache(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &cache,
            true, // is_remote
            false,
        )
        .await
        .expect("signed v3 install should succeed");

        assert!(target.join(".thinkingroot/manifest.toml").exists());
        assert!(target.join(".thinkingroot/claims.jsonl").exists());
        assert!(target.join(".thinkingroot/sources/a.md").exists());
    }

    #[tokio::test]
    async fn install_refuses_unsigned_remote_pack_with_exit_unsigned() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(22);
        let cache = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

        let pack_bytes = build_unsigned_v3_pack("alice/demo");
        let pack_path = write_pack_to_disk(tmp.path(), "alice-demo.tr", &pack_bytes);

        let target = tmp.path().join("install");
        let err = install_with_revocation_cache(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &cache,
            true, // is_remote
            false, // user_allow_unsigned = false
        )
        .await
        .expect_err("unsigned remote pack should be refused");

        assert_eq!(refuse_exit_code(&err), EXIT_UNSIGNED);
        assert!(!target.exists(), "target dir should not exist on refusal");
    }

    #[tokio::test]
    async fn install_accepts_unsigned_local_pack_by_default() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(23);
        let cache = fresh_revocation_cache(tmp.path().join("rev"), &rev_key, &[]);

        let pack_bytes = build_unsigned_v3_pack("alice/local");
        let pack_path = write_pack_to_disk(tmp.path(), "alice-local.tr", &pack_bytes);

        let target = tmp.path().join("install");
        install_with_revocation_cache(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &cache,
            false, // is_remote = false
            false,
        )
        .await
        .expect("unsigned local pack should be accepted by default");

        assert!(target.join(".thinkingroot/manifest.toml").exists());
    }

    #[tokio::test]
    async fn install_refuses_revoked_pack_with_exit_revoked() {
        let tmp = tempdir().unwrap();
        let rev_key = signing_key(24);

        let signing = signing_key(25);
        let pack_bytes = build_signed_v3_pack("alice/bad", &signing);
        let pack_path = write_pack_to_disk(tmp.path(), "alice-bad.tr", &pack_bytes);
        let pack = read_v3_pack(&pack_bytes).unwrap();

        // Pack-hash format is `blake3:<hex>` — strip the prefix when
        // building the revocation snapshot since `fresh_revocation_cache`
        // re-adds the prefix.
        let bare_hash = pack.manifest.pack_hash.strip_prefix("blake3:").unwrap();
        let cache = fresh_revocation_cache(
            tmp.path().join("rev"),
            &rev_key,
            &[bare_hash],
        );

        let target = tmp.path().join("install");
        let err = install_with_revocation_cache(
            pack_path.to_str().unwrap(),
            Some(target.clone()),
            None,
            &cache,
            true,
            false,
        )
        .await
        .expect_err("revoked pack should be refused");

        assert_eq!(refuse_exit_code(&err), EXIT_REVOKED);
        assert!(!target.exists());
    }

    // Tamper detection is comprehensively covered by
    // `crates/tr-verify/src/v3.rs` tests (`tampered_claims_jsonl_detected_before_signature_check`,
    // `tampered_manifest_pack_hash_field_detected`, etc.). The CLI
    // path delegates to those — no duplication here.
}
