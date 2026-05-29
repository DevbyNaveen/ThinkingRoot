//! `root prompt {edit,list,version}` — manage Compiled Prompt templates
//! over the engine REST API. Templates are versioned append-only; `edit`
//! writes a new version.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

use crate::function_cmd::{send_envelope, with_auth};

pub async fn edit(
    name: &str,
    file: Option<&Path>,
    workspace: &str,
    url: &str,
    api_key: Option<&str>,
) -> Result<()> {
    let template_text = match file {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("read template from {}", p.display()))?,
        None => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("read template body from stdin")?;
            s
        }
    };
    let client = reqwest::Client::new();
    let req = with_auth(
        client
            .put(format!("{url}/api/v1/ws/{workspace}/prompts"))
            .json(&serde_json::json!({ "name": name, "template_text": template_text })),
        api_key,
    );
    let data = send_envelope(req).await?;
    let version = data.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    println!("✓ wrote '{name}' version {version}");
    Ok(())
}

pub async fn list(workspace: &str, url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let req = with_auth(
        client.get(format!("{url}/api/v1/ws/{workspace}/prompts")),
        api_key,
    );
    let data = send_envelope(req).await?;
    let arr = data.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("No prompt templates in workspace '{workspace}'.");
    } else {
        for t in arr {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let version = t.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
            println!("{name}  (v{version})");
        }
    }
    Ok(())
}

pub async fn version(
    name: &str,
    workspace: &str,
    url: &str,
    api_key: Option<&str>,
) -> Result<()> {
    let client = reqwest::Client::new();
    let req = with_auth(
        client.get(format!("{url}/api/v1/ws/{workspace}/prompts/{name}/versions")),
        api_key,
    );
    let data = send_envelope(req).await?;
    let arr = data.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("No versions for prompt '{name}'.");
    } else {
        for t in arr {
            let version = t.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
            let vars = t
                .get("variables")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!("v{version}  vars: [{vars}]");
        }
    }
    Ok(())
}
