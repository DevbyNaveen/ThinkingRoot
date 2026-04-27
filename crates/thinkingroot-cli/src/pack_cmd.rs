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

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tr_format::{
    digest::blake3_hex, reader as tr_reader, writer::PackBuilder, Manifest, Version,
};

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

/// Run `root pack`. See module-level docs for behaviour.
pub fn run_pack(
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
pub async fn run_install(
    reference: &str,
    target: Option<PathBuf>,
    registry_override: Option<String>,
) -> Result<()> {
    let bytes = match parse_install_ref(reference)? {
        InstallRef::Local(path) => fs::read(&path)
            .with_context(|| format!("read {}", path.display()))?,
        InstallRef::DirectUrl(url) => fetch_direct_url(&url).await?,
        InstallRef::Registry {
            owner,
            slug,
            version,
        } => {
            let registry = match registry_override {
                Some(url) => url,
                None => load_default_registry()?,
            };
            fetch_via_registry(&registry, &owner, &slug, &version).await?
        }
    };
    install_from_bytes(&bytes, target)
}

/// A user-supplied install target — local file, direct URL, or
/// registry coordinate.
#[derive(Debug, PartialEq, Eq)]
enum InstallRef {
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
fn http_client() -> Result<reqwest::Client> {
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
fn refuse_insecure_http(url: &str) -> Result<()> {
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

/// Fetch a `.tr` directly from a URL (no registry resolution).
async fn fetch_direct_url(url: &str) -> Result<Vec<u8>> {
    refuse_insecure_http(url)?;
    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    let resp = resp
        .error_for_status()
        .with_context(|| format!("GET {}", url))?;
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("read body from {}", url))?;
    Ok(bytes.to_vec())
}

/// Fetch the discovery document, then fetch the pack's `.tr` bytes via
/// the advertised endpoint, verify the content hash, and return.
async fn fetch_via_registry(
    registry_url: &str,
    owner: &str,
    slug: &str,
    version: &str,
) -> Result<Vec<u8>> {
    refuse_insecure_http(registry_url)?;
    let registry_url = registry_url.trim_end_matches('/');
    let client = http_client()?;

    // 1. Discovery doc.
    let discovery_url = format!("{}/.well-known/tr-registry.json", registry_url);
    let disco: serde_json::Value = client
        .get(&discovery_url)
        .send()
        .await
        .with_context(|| format!("GET {}", discovery_url))?
        .error_for_status()
        .with_context(|| format!("GET {}", discovery_url))?
        .json()
        .await
        .with_context(|| format!("parse JSON from {}", discovery_url))?;

    let registry_fmt = disco["format_version"].as_str().unwrap_or("");
    if registry_fmt != "tr-registry/1" {
        return Err(anyhow!(
            "registry at {} advertises unsupported format_version `{}`",
            registry_url,
            registry_fmt
        ));
    }
    let advertised_tr_fmt = disco["tr_format"].as_str().unwrap_or("");
    if advertised_tr_fmt != tr_format::manifest::FORMAT_VERSION {
        return Err(anyhow!(
            "registry advertises tr_format `{}` but this client only handles `{}`",
            advertised_tr_fmt,
            tr_format::manifest::FORMAT_VERSION
        ));
    }
    let pattern = disco["endpoints"]["download"]
        .as_str()
        .ok_or_else(|| anyhow!("registry doc missing endpoints.download"))?;
    let max_bytes = disco["max_pack_bytes"]
        .as_u64()
        .unwrap_or(tr_reader::DEFAULT_SIZE_CAP);

    // 2. Build the download URL by template substitution.
    let download_path = pattern
        .replace("{owner}", owner)
        .replace("{slug}", slug)
        .replace("{version}", version);
    let download_url = format!("{}{}", registry_url, download_path);

    // 3. Fetch the bytes.
    let resp = client
        .get(&download_url)
        .send()
        .await
        .with_context(|| format!("GET {}", download_url))?
        .error_for_status()
        .with_context(|| format!("GET {}", download_url))?;

    if let Some(cl) = resp.content_length() {
        if cl > max_bytes {
            return Err(anyhow!(
                "registry advertised content-length {} exceeds max_pack_bytes {} for {}/{}",
                cl,
                max_bytes,
                owner,
                slug
            ));
        }
    }
    // Capture the registry-advertised hash before consuming the body —
    // this is independent verification on top of the manifest's own
    // canonical-bytes hash that `tr_format::reader` checks.
    let advertised_hash: Option<String> = resp
        .headers()
        .get("x-tr-content-hash")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("read body from {}", download_url))?;
    if bytes.len() as u64 > max_bytes {
        return Err(anyhow!(
            "registry returned {} bytes, exceeds max_pack_bytes {}",
            bytes.len(),
            max_bytes
        ));
    }

    // 4. Defense-in-depth hash check. If the registry put a hash in
    // the response header, verify the body matches before we even
    // hand it to `tr_format::reader`.
    if let Some(expected) = &advertised_hash {
        let actual = blake3_hex(&bytes);
        if &actual != expected {
            return Err(anyhow!(
                "content hash mismatch for {}/{}@{}: registry advertised `{}`, computed `{}`",
                owner,
                slug,
                version,
                expected,
                actual
            ));
        }
    }
    Ok(bytes.to_vec())
}

/// Extract a `.tr` byte slice into the target directory's
/// `.thinkingroot/`. Pulled out so registry-fetched and locally-loaded
/// bytes share one unpack path.
fn install_from_bytes(bytes: &[u8], target: Option<PathBuf>) -> Result<()> {
    let pack = tr_reader::read_bytes(bytes).map_err(|e| anyhow!("read .tr: {e}"))?;
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
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None).unwrap();
        assert!(out_tr.exists(), ".tr file not produced");
        assert!(
            fs::metadata(&out_tr).unwrap().len() > 0,
            ".tr file is empty"
        );

        // Install into a fresh target via the new string-keyed entry point.
        let install_tmp = tempdir().unwrap();
        let target = install_tmp.path().join("install-here");
        run_install(out_tr.to_str().unwrap(), Some(target.clone()), None)
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
        let err = run_pack(tmp.path(), None, None, None, None, None).unwrap_err();
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
        let err = run_pack(workspace, None, None, None, None, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("name"), "got: {msg}");
    }

    #[test]
    fn pack_path_set_matches_engine_files_minus_skips() {
        let tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(tmp.path());
        let out_tr = workspace.join("paths.tr");
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None).unwrap();
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
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None).unwrap();

        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("dst");
        run_install(out_tr.to_str().unwrap(), Some(target.clone()), None)
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
        run_pack(&workspace, Some(good_tr.clone()), None, None, None, None).unwrap();

        // Corrupt the file (flip a byte in the middle).
        let mut bytes = fs::read(&good_tr).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        let bad_tr = workspace.join("bad.tr");
        fs::write(&bad_tr, &bytes).unwrap();

        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("dst");
        let err = run_install(bad_tr.to_str().unwrap(), Some(target), None)
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
        run_pack(&workspace, Some(tr_out.clone()), None, None, None, None).unwrap();
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
        run_install(
            "alice/demo@0.1.0",
            Some(target.clone()),
            Some(registry_url.clone()),
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
        run_pack(&workspace, Some(tr_out.clone()), None, None, None, None).unwrap();
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
        )
        .await
        .expect_err("foreign registry format must be refused");
        server.abort();
        assert!(
            format!("{err}").contains("unsupported format_version"),
            "got: {err}"
        );
    }
}
