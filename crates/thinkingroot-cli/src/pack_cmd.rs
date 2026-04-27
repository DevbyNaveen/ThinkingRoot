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
use tr_format::{reader::read_file as read_tr, writer::PackBuilder, Manifest, Version};

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

/// Run `root install`. Extracts the `.tr` file at `tr_path` into
/// `<target>/.thinkingroot/`. Defaults `target` to
/// `~/.thinkingroot/packs/<owner>/<slug>/<version>/`.
pub fn run_install(tr_path: &Path, target: Option<PathBuf>) -> Result<()> {
    let pack = read_tr(tr_path)
        .map_err(|e| anyhow!("read {}: {e}", tr_path.display()))?;
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
            // Surface the manifest at the install target's root
            // (alongside `.thinkingroot/`), not inside the engine dir.
            // The engine itself doesn't read manifest.json — it's for
            // `root inspect` and external tooling.
            continue;
        }
        let bytes = pack
            .entry(path)
            .ok_or_else(|| anyhow!("pack reports `{}` but has no entry", path))?;
        let dest = engine_dir.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&dest, bytes)
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

    #[test]
    fn pack_then_install_round_trips_engine_output() {
        let src_tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(src_tmp.path());

        let out_tr = workspace.join("alice-demo-0.1.0.tr");
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None).unwrap();
        assert!(out_tr.exists(), ".tr file not produced");
        assert!(
            fs::metadata(&out_tr).unwrap().len() > 0,
            ".tr file is empty"
        );

        // Install into a fresh target.
        let install_tmp = tempdir().unwrap();
        let target = install_tmp.path().join("install-here");
        run_install(&out_tr, Some(target.clone())).unwrap();

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
        let pack = read_tr(&out_tr).unwrap();
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
        let pack = read_tr(&out_tr).unwrap();
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

    #[test]
    fn install_into_explicit_target_does_not_touch_home() {
        let src_tmp = tempdir().unwrap();
        let workspace = fake_engine_workspace(src_tmp.path());
        let out_tr = workspace.join("explicit.tr");
        run_pack(&workspace, Some(out_tr.clone()), None, None, None, None).unwrap();

        let dst_tmp = tempdir().unwrap();
        let target = dst_tmp.path().join("dst");
        run_install(&out_tr, Some(target.clone())).unwrap();
        assert!(target.join("manifest.json").exists());
        assert!(target.join(".thinkingroot/graph/triples.jsonl").exists());
    }

    #[test]
    fn install_rejects_corrupted_tr() {
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
        let err = run_install(&bad_tr, Some(target)).expect_err("must reject corrupted .tr");
        let msg = format!("{err}");
        assert!(msg.contains("read"), "got: {msg}");
    }
}
