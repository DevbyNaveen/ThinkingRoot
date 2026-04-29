//! `root publish` — read `tr-pack.toml`, tar+zstd the workspace, upload
//! the source archive to the compile-worker, enqueue a compile job,
//! and (by default) poll until the job terminates.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use console::style;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::{config, http};

const MANIFEST_NAME: &str = "tr-pack.toml";
const ALWAYS_EXCLUDED: &[&str] = &[".git", ".thinkingroot", "target", "node_modules"];

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub pack: PackSection,
    #[serde(default)]
    pub publish: Option<PublishSection>,
}

#[derive(Debug, Deserialize)]
pub struct PackSection {
    pub owner: String,
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_license")]
    pub license: String,
    #[serde(default = "default_visibility")]
    pub visibility: String,
}

fn default_license() -> String {
    "Apache-2.0".into()
}
fn default_visibility() -> String {
    "public".into()
}

#[derive(Debug, Deserialize, Default)]
pub struct PublishSection {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Serialize)]
struct UpsertPackBody {
    owner_handle: String,
    slug: String,
    description: String,
    license: String,
    visibility: String,
}

#[derive(Debug, Deserialize)]
struct UpsertPackResp {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    owner_handle: String,
    #[allow(dead_code)]
    slug: String,
}

#[derive(Debug, Deserialize)]
struct UploadResp {
    url: String,
    #[allow(dead_code)]
    content_hash: String,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct EnqueueBody {
    user_id: String,
    owner_handle: String,
    pack_slug: String,
    source_archive_url: String,
}

#[derive(Debug, Deserialize, Clone)]
struct Job {
    id: String,
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    attempts: Option<u32>,
}

pub async fn run(
    path: PathBuf,
    wait: bool,
    timeout_secs: u64,
    server_override: Option<String>,
) -> Result<()> {
    let cfg = config::load_or_default(server_override.as_deref())?;
    let token = config::require_token(&cfg)?;
    let user_id = cfg
        .user_id
        .as_deref()
        .ok_or_else(|| anyhow!("missing user_id in config — run `root login` again"))?;

    let manifest = load_manifest(&path)?;
    println!(
        "{} publishing {}",
        style("→").cyan(),
        style(format!("{}/{}", manifest.pack.owner, manifest.pack.slug)).bold()
    );

    let http = http::client()?;
    let server = cfg.server.trim_end_matches('/');

    // 1. Ensure the pack exists (registry upsert).
    let _: UpsertPackResp = http::post_json(
        &http,
        &format!("{server}/api/v1/packs"),
        token,
        &UpsertPackBody {
            owner_handle: manifest.pack.owner.clone(),
            slug: manifest.pack.slug.clone(),
            description: manifest.pack.description.clone(),
            license: manifest.pack.license.clone(),
            visibility: manifest.pack.visibility.clone(),
        },
    )
    .await
    .context("registry upsert_pack")?;
    println!("  {} registered pack metadata", style("✓").green());

    // 2. Tar+zstd the workspace.
    let exclude = collect_excludes(manifest.publish.as_ref());
    let tarball = build_tarball(&path, &exclude)?;
    println!(
        "  {} packaged {} bytes ({} files compressed)",
        style("✓").green(),
        tarball.size_bytes,
        tarball.file_count
    );

    // 3. Upload to compile-worker /api/v1/uploads.
    let upload: UploadResp = http::post_bytes(
        &http,
        &format!("{server}/api/v1/uploads"),
        token,
        "application/zstd",
        tarball.bytes,
    )
    .await
    .context("compile-worker upload")?;
    println!(
        "  {} uploaded → {} ({} bytes)",
        style("✓").green(),
        style(short_url(&upload.url)).dim(),
        upload.size_bytes
    );

    // 4. Enqueue compile job.
    let job: Job = http::post_json(
        &http,
        &format!("{server}/api/v1/jobs"),
        token,
        &EnqueueBody {
            user_id: user_id.to_string(),
            owner_handle: manifest.pack.owner.clone(),
            pack_slug: manifest.pack.slug.clone(),
            source_archive_url: upload.url.clone(),
        },
    )
    .await
    .context("compile-worker enqueue")?;
    println!(
        "  {} enqueued job {}",
        style("✓").green(),
        style(&job.id).cyan()
    );

    if !wait {
        println!("  {} `root status` to see progress", style("→").dim());
        return Ok(());
    }

    poll_job(&http, server, token, &job.id, timeout_secs).await
}

async fn poll_job(
    http: &reqwest::Client,
    server: &str,
    token: &str,
    job_id: &str,
    timeout_secs: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut last_status = String::new();
    loop {
        if Instant::now() > deadline {
            return Err(anyhow!(
                "timed out after {timeout_secs}s waiting for job {job_id}"
            ));
        }
        let job: Job = super::http::get_json(
            http,
            &format!("{server}/api/v1/jobs/{}", url_encode(job_id)),
            token,
        )
        .await?;
        if job.status != last_status {
            print!(
                "  {} status: {}",
                style("…").yellow(),
                style(&job.status).bold()
            );
            if let Some(a) = job.attempts {
                print!(" (attempt {a})");
            }
            println!();
            std::io::stdout().flush().ok();
            last_status = job.status.clone();
        }
        match job.status.as_str() {
            "succeeded" => {
                println!(
                    "  {} {}",
                    style("✓").green(),
                    style("compile succeeded").bold()
                );
                return Ok(());
            }
            "failed" | "cancelled" => {
                let err = job.error.unwrap_or_else(|| "(no error message)".into());
                return Err(anyhow!("job {} {}: {err}", job_id, job.status));
            }
            _ => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

struct Tarball {
    bytes: Vec<u8>,
    size_bytes: u64,
    file_count: u32,
}

fn collect_excludes(p: Option<&PublishSection>) -> Vec<String> {
    let mut out: Vec<String> = ALWAYS_EXCLUDED.iter().map(|s| s.to_string()).collect();
    if let Some(p) = p {
        for e in &p.exclude {
            if !e.is_empty() {
                out.push(e.clone());
            }
        }
    }
    out
}

fn is_excluded(rel: &Path, excludes: &[String]) -> bool {
    let s = rel.to_string_lossy();
    excludes.iter().any(|e| {
        // Exclude if the exclude string equals or is a prefix of any
        // path component (e.g. `target` matches both `target` and
        // `target/release/foo`). Intentionally simple — full glob
        // support can come later.
        rel.components()
            .any(|c| c.as_os_str().to_string_lossy() == *e)
            || s.starts_with(&format!("{e}/"))
            || AsRef::<str>::as_ref(&s) == e.as_str()
    })
}

fn build_tarball(root: &Path, excludes: &[String]) -> Result<Tarball> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", root.display()))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut file_count: u32 = 0;
    {
        let enc = zstd::stream::write::Encoder::new(&mut buf, 19)
            .context("zstd init")?
            .auto_finish();
        let mut tar = tar::Builder::new(enc);
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
                !is_excluded(rel, excludes)
            })
        {
            let entry = entry.with_context(|| "walk")?;
            let path = entry.path();
            if path == root {
                continue;
            }
            let rel = path
                .strip_prefix(&root)
                .with_context(|| format!("strip_prefix {}", path.display()))?;
            if entry.file_type().is_dir() {
                continue;
            }
            if entry.file_type().is_symlink() {
                // Skip symlinks — adding them faithfully needs more
                // work and they're not load-bearing for pack source.
                continue;
            }
            tar.append_path_with_name(path, rel)
                .with_context(|| format!("tar {}", rel.display()))?;
            file_count += 1;
        }
        tar.finish().context("tar finish")?;
    }
    let size_bytes = buf.len() as u64;
    Ok(Tarball {
        bytes: buf,
        size_bytes,
        file_count,
    })
}

pub fn load_manifest(path: &Path) -> Result<Manifest> {
    let p = path.join(MANIFEST_NAME);
    if !p.exists() {
        return Err(anyhow!(
            "no `{MANIFEST_NAME}` found in {} — run `root init` first",
            path.display()
        ));
    }
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let manifest: Manifest =
        toml::from_str(&text).with_context(|| format!("parse {}", p.display()))?;
    if manifest.pack.owner.is_empty() || manifest.pack.slug.is_empty() {
        return Err(anyhow!("`pack.owner` and `pack.slug` must be set"));
    }
    Ok(manifest)
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn short_url(u: &str) -> String {
    if u.len() <= 60 {
        u.to_string()
    } else {
        format!("{}…{}", &u[..28], &u[u.len() - 24..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn excludes_match_path_components() {
        let ex = vec![".git".to_string(), "target".to_string()];
        assert!(is_excluded(Path::new(".git"), &ex));
        assert!(is_excluded(Path::new(".git/HEAD"), &ex));
        assert!(is_excluded(Path::new("target/release/x"), &ex));
        assert!(!is_excluded(Path::new("src/main.rs"), &ex));
        assert!(!is_excluded(Path::new("targetless.txt"), &ex));
    }

    #[test]
    fn build_tarball_skips_excluded() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("README.md"), b"hello").unwrap();
        fs::create_dir_all(dir.path().join("target/foo")).unwrap();
        fs::write(dir.path().join("target/foo/big.bin"), b"junk").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), b"fn main() {}").unwrap();

        let tb = build_tarball(
            dir.path(),
            &["target".to_string(), ".thinkingroot".to_string()],
        )
        .unwrap();
        assert_eq!(tb.file_count, 2, "README.md + src/lib.rs only");
        assert!(tb.size_bytes > 0);
    }

    #[test]
    fn load_manifest_rejects_missing_fields() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("tr-pack.toml"),
            "[pack]\nowner = \"\"\nslug = \"x\"\n",
        )
        .unwrap();
        let err = load_manifest(dir.path()).unwrap_err();
        assert!(err.to_string().contains("owner"));
    }

    #[test]
    fn load_manifest_parses_minimal_valid() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("tr-pack.toml"),
            "[pack]\nowner = \"alice\"\nslug = \"demo\"\n",
        )
        .unwrap();
        let m = load_manifest(dir.path()).unwrap();
        assert_eq!(m.pack.owner, "alice");
        assert_eq!(m.pack.slug, "demo");
        assert_eq!(m.pack.license, "Apache-2.0");
        assert_eq!(m.pack.visibility, "public");
    }
}
