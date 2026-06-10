//! Brain-as-Code (final-plan §10 / 25b) — the `/brain` source-folder format.
//!
//! The DNA of a ThinkingRoot brain, git-versioned next to the app code and
//! MIT-licensable: human-editable prompts/functions/routes in a conventional
//! folder, the stateful analogue of a `supabase/` folder. This module is the
//! on-disk CONTRACT — a pure (de)serialiser between the folder and an in-memory
//! [`BrainCode`]. The CLI (`root brain pull/push`) populates it from / applies
//! it to a running engine over REST; this layer owns only the bytes-on-disk so
//! the round-trip is exact and unit-testable without a daemon.
//!
//! Layout:
//! ```text
//! brain/
//!   brain.toml          # manifest: name, version, base_brain
//!   prompts/<name>.md   # one Compiled Prompt per file
//!   functions/<name>.js # one Root Function per file
//!   routes.toml         # capability routes (input_class -> function)
//!   sources.txt         # source URIs, one per line
//! ```

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const BRAIN_DIR: &str = "brain";
const MANIFEST: &str = "brain.toml";
const PROMPTS_DIR: &str = "prompts";
const FUNCTIONS_DIR: &str = "functions";
const ROUTES: &str = "routes.toml";
const SOURCES: &str = "sources.txt";

/// `brain.toml` — identity + lineage of the brain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BrainManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// Optional lineage pointer — the pack/brain this one was forked from
    /// (renders the public Brain Tree). Empty = a root brain.
    #[serde(default)]
    pub base_brain: String,
}

/// One learned capability route: which function serves an input class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainRoute {
    pub input_class: String,
    pub function: String,
}

/// The full in-memory brain, mirrored 1:1 to the `/brain` folder.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrainCode {
    pub manifest: BrainManifest,
    /// `(name, markdown body)` per Compiled Prompt.
    pub prompts: Vec<(String, String)>,
    /// `(name, JS body)` per Root Function.
    pub functions: Vec<(String, String)>,
    pub routes: Vec<BrainRoute>,
    pub sources: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct RoutesFile {
    #[serde(default)]
    route: Vec<BrainRoute>,
}

/// Sanitise a prompt/function name into a safe single-segment filename stem.
/// Keeps alphanumerics, `-`, `_`, `.`; everything else becomes `_`. Prevents a
/// crafted name (`../etc`) from escaping the brain folder.
fn safe_stem(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "unnamed".to_string() } else { s }
}

/// Write a [`BrainCode`] to `<root>/brain/`, creating the layout.
pub fn write_brain(root: &Path, brain: &BrainCode) -> Result<()> {
    let dir = root.join(BRAIN_DIR);
    std::fs::create_dir_all(dir.join(PROMPTS_DIR)).context("create brain/prompts")?;
    std::fs::create_dir_all(dir.join(FUNCTIONS_DIR)).context("create brain/functions")?;

    let manifest = toml::to_string_pretty(&brain.manifest).context("serialise brain.toml")?;
    std::fs::write(dir.join(MANIFEST), manifest).context("write brain.toml")?;

    for (name, body) in &brain.prompts {
        let path = dir.join(PROMPTS_DIR).join(format!("{}.md", safe_stem(name)));
        std::fs::write(&path, body).with_context(|| format!("write prompt {name}"))?;
    }
    for (name, body) in &brain.functions {
        let path = dir.join(FUNCTIONS_DIR).join(format!("{}.js", safe_stem(name)));
        std::fs::write(&path, body).with_context(|| format!("write function {name}"))?;
    }

    let routes = toml::to_string_pretty(&RoutesFile { route: brain.routes.clone() })
        .context("serialise routes.toml")?;
    std::fs::write(dir.join(ROUTES), routes).context("write routes.toml")?;

    std::fs::write(dir.join(SOURCES), brain.sources.join("\n")).context("write sources.txt")?;
    Ok(())
}

/// Read a [`BrainCode`] back from `<root>/brain/`. Missing optional files
/// (routes/sources) are treated as empty; a missing manifest is an error
/// (an un-manifested folder isn't a brain).
pub fn read_brain(root: &Path) -> Result<BrainCode> {
    let dir = root.join(BRAIN_DIR);
    let manifest_raw = std::fs::read_to_string(dir.join(MANIFEST))
        .with_context(|| format!("read {BRAIN_DIR}/{MANIFEST} (is this a brain folder?)"))?;
    let manifest: BrainManifest =
        toml::from_str(&manifest_raw).context("parse brain.toml")?;

    let prompts = read_dir_files(&dir.join(PROMPTS_DIR), "md")?;
    let functions = read_dir_files(&dir.join(FUNCTIONS_DIR), "js")?;

    let routes = match std::fs::read_to_string(dir.join(ROUTES)) {
        Ok(raw) => toml::from_str::<RoutesFile>(&raw).context("parse routes.toml")?.route,
        Err(_) => Vec::new(),
    };
    let sources = match std::fs::read_to_string(dir.join(SOURCES)) {
        Ok(raw) => raw.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
        Err(_) => Vec::new(),
    };

    Ok(BrainCode { manifest, prompts, functions, routes, sources })
}

/// Read `(stem, body)` for every `*.ext` file in `dir`, sorted by name for a
/// deterministic order. Absent dir = empty.
fn read_dir_files(dir: &Path, ext: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };
    for entry in rd {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let body = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            out.push((stem, body));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Defensive field extraction — try several common key names for a body.
fn pick_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// `root brain pull` (§10 / 25b, slice 2a) — export the LIVE brain (prompts +
/// functions) from a running engine into `./brain/`. Talks to the daemon over
/// REST (the engine owns graph.db; the CLI never opens it directly). Lists each
/// kind, fetches each item's body per-name (the list may omit bodies), and
/// writes the folder via [`write_brain`].
pub async fn run_pull(
    conn: &thinkingroot_core::cortex::EngineConnection,
    root: &Path,
    _branch: Option<&str>,
) -> Result<()> {
    use crate::cortex_remote;
    let ws = cortex_remote::ensure_mounted_remote(conn, root)
        .await
        .context("mount workspace")?;

    // Prompts: list names, then fetch each body.
    let mut prompts = Vec::new();
    if let Ok(list) = cortex_remote::get_json(conn, &format!("/api/v1/ws/{ws}/prompts")).await {
        for item in list.as_array().cloned().unwrap_or_default() {
            let Some(name) = pick_str(&item, &["name"]) else { continue };
            let body = pick_str(&item, &["template_text", "text", "body"]);
            let body = match body {
                Some(b) => b,
                None => {
                    let one = cortex_remote::get_json(
                        conn,
                        &format!("/api/v1/ws/{ws}/prompts/{}", urlencode(&name)),
                    )
                    .await
                    .ok();
                    one.as_ref()
                        .and_then(|o| pick_str(o, &["template_text", "text", "body"]))
                        .unwrap_or_default()
                }
            };
            prompts.push((name, body));
        }
    }

    // Functions: list names, then fetch each body.
    let mut functions = Vec::new();
    if let Ok(list) = cortex_remote::get_json(conn, &format!("/api/v1/ws/{ws}/functions")).await {
        for item in list.as_array().cloned().unwrap_or_default() {
            let Some(name) = pick_str(&item, &["name"]) else { continue };
            let body = match pick_str(&item, &["body"]) {
                Some(b) => b,
                None => {
                    let one = cortex_remote::get_json(
                        conn,
                        &format!("/api/v1/ws/{ws}/functions/{}", urlencode(&name)),
                    )
                    .await
                    .ok();
                    one.as_ref()
                        .and_then(|o| pick_str(o, &["body"]))
                        .unwrap_or_default()
                }
            };
            functions.push((name, body));
        }
    }

    let brain = BrainCode {
        manifest: BrainManifest {
            name: ws.clone(),
            version: String::new(),
            base_brain: String::new(),
        },
        prompts,
        functions,
        routes: Vec::new(),
        sources: Vec::new(),
    };
    write_brain(root, &brain)?;
    println!(
        "pulled {} prompt(s), {} function(s) from '{ws}' -> {}/{BRAIN_DIR}/",
        brain.prompts.len(),
        brain.functions.len(),
        root.display()
    );
    Ok(())
}

/// `root brain push` (§10 / 25b, slice 2b) — apply a `./brain/` folder to a
/// running engine: deploy each prompt + function over REST. The inverse of
/// [`run_pull`]. Each deploy is a versioned PUT (the engine bumps the version),
/// so re-pushing is safe. Best-effort per item — one failure is reported but
/// does not abort the rest.
pub async fn run_push(
    conn: &thinkingroot_core::cortex::EngineConnection,
    root: &Path,
    _branch: Option<&str>,
) -> Result<()> {
    use crate::cortex_remote;
    let brain = read_brain(root).context("read ./brain (run `root brain pull` first?)")?;
    let ws = cortex_remote::ensure_mounted_remote(conn, root)
        .await
        .context("mount workspace")?;

    let mut ok_p = 0usize;
    let mut ok_f = 0usize;
    let mut failed = 0usize;

    for (name, body) in &brain.prompts {
        let payload = serde_json::json!({ "name": name, "template_text": body });
        match cortex_remote::put_json(conn, &format!("/api/v1/ws/{ws}/prompts"), &payload).await {
            Ok(_) => ok_p += 1,
            Err(e) => {
                failed += 1;
                eprintln!("  prompt '{name}' failed: {e}");
            }
        }
    }
    for (name, body) in &brain.functions {
        let payload = serde_json::json!({ "name": name, "body": body, "language": "js" });
        match cortex_remote::put_json(conn, &format!("/api/v1/ws/{ws}/functions"), &payload).await {
            Ok(_) => ok_f += 1,
            Err(e) => {
                failed += 1;
                eprintln!("  function '{name}' failed: {e}");
            }
        }
    }

    println!(
        "pushed {ok_p} prompt(s), {ok_f} function(s) to '{ws}'{}",
        if failed > 0 { format!(" ({failed} failed)") } else { String::new() }
    );
    if failed > 0 {
        anyhow::bail!("{failed} item(s) failed to deploy");
    }
    Ok(())
}

/// Minimal percent-encoding for a path segment (names are already
/// safe_stem-class but a space/`/` would break the URL).
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BrainCode {
        BrainCode {
            manifest: BrainManifest {
                name: "support-bot".into(),
                version: "0.1.0".into(),
                base_brain: "acme/base@1".into(),
            },
            prompts: vec![
                ("system".into(), "You are a support agent.".into()),
                ("triage".into(), "Classify the ticket.".into()),
            ],
            functions: vec![("greet".into(), "async (i,ctx)=>({hi:i.name})".into())],
            routes: vec![BrainRoute { input_class: "billing".into(), function: "greet".into() }],
            sources: vec!["file://docs/faq.md".into(), "https://x.example/policy".into()],
        }
    }

    #[test]
    fn brain_round_trips_through_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let brain = sample();
        write_brain(tmp.path(), &brain).unwrap();
        let back = read_brain(tmp.path()).unwrap();
        assert_eq!(back, brain, "folder round-trip must be exact");
    }

    #[test]
    fn layout_is_on_disk_as_specified() {
        let tmp = tempfile::tempdir().unwrap();
        write_brain(tmp.path(), &sample()).unwrap();
        let b = tmp.path().join("brain");
        assert!(b.join("brain.toml").exists());
        assert!(b.join("prompts/system.md").exists());
        assert!(b.join("functions/greet.js").exists());
        assert!(b.join("routes.toml").exists());
        assert!(b.join("sources.txt").exists());
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_brain(tmp.path()).is_err(), "no brain.toml → not a brain");
    }

    #[test]
    fn optional_files_default_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut brain = sample();
        brain.routes.clear();
        brain.sources.clear();
        write_brain(tmp.path(), &brain).unwrap();
        let back = read_brain(tmp.path()).unwrap();
        assert!(back.routes.is_empty());
        assert!(back.sources.is_empty());
    }

    #[test]
    fn pick_str_tries_keys_in_order() {
        let v = serde_json::json!({"template_text": "hi", "body": "x"});
        assert_eq!(pick_str(&v, &["template_text", "body"]).as_deref(), Some("hi"));
        assert_eq!(pick_str(&v, &["missing", "body"]).as_deref(), Some("x"));
        assert_eq!(pick_str(&v, &["nope"]), None);
        // Empty string is skipped (treated as absent).
        let e = serde_json::json!({"name": ""});
        assert_eq!(pick_str(&e, &["name"]), None);
    }

    #[test]
    fn urlencode_escapes_unsafe_chars() {
        assert_eq!(urlencode("simple-name_1.2"), "simple-name_1.2");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn unsafe_names_are_contained() {
        // The key property: no path separators survive, so the stem is a
        // single segment that can't escape the brain folder.
        let stem = safe_stem("../../etc/passwd");
        assert!(!stem.contains('/') && !stem.contains('\\'));
        // Exact parent/current-dir names are neutralised (would be traversal).
        assert_eq!(safe_stem(".."), "unnamed");
        assert_eq!(safe_stem("."), "unnamed");
        assert_eq!(safe_stem("normal-name_1.2"), "normal-name_1.2");
        assert_eq!(safe_stem(""), "unnamed");
    }
}
