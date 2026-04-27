//! `root status` — list recent cloud compile jobs for the logged-in
//! user.

use anyhow::{anyhow, Result};
use console::style;
use serde::Deserialize;

use super::{config, http};

#[derive(Debug, Deserialize)]
struct Job {
    id: String,
    status: String,
    owner_handle: String,
    pack_slug: String,
    created_at: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JobsResponse {
    #[serde(default)]
    jobs: Vec<Job>,
}

pub async fn run(limit: u32, server_override: Option<String>) -> Result<()> {
    let cfg = config::load_or_default(server_override.as_deref())?;
    let token = config::require_token(&cfg)?;
    let user_id = cfg
        .user_id
        .as_deref()
        .ok_or_else(|| anyhow!("missing user_id in config — run `root login` again"))?;

    let http = http::client()?;
    let url = format!(
        "{}/api/v1/users/{}/jobs?limit={}",
        cfg.server.trim_end_matches('/'),
        url_encode_path(user_id),
        limit
    );
    let resp: JobsResponse = http::get_json(&http, &url, token).await?;

    if resp.jobs.is_empty() {
        println!("{} no recent compile jobs", style("•").dim());
        return Ok(());
    }
    println!(
        "{:<24}  {:<12}  {:<28}  {}",
        style("Job").bold(),
        style("Status").bold(),
        style("Pack").bold(),
        style("Created").bold(),
    );
    for j in resp.jobs {
        let status_dot = match j.status.as_str() {
            "succeeded" => style("✓").green().to_string(),
            "failed" | "cancelled" => style("✗").red().to_string(),
            "running" | "claimed" => style("…").yellow().to_string(),
            _ => style("·").dim().to_string(),
        };
        println!(
            "{:<24}  {} {:<10}  {:<28}  {}",
            j.id,
            status_dot,
            j.status,
            format!("{}/{}", j.owner_handle, j.pack_slug),
            short_ts(&j.created_at),
        );
        if let Some(err) = j.error {
            if !err.is_empty() {
                println!("    {} {}", style("↳").dim(), style(err).red());
            }
        }
    }
    Ok(())
}

fn short_ts(s: &str) -> String {
    s.split('T').next().unwrap_or(s).to_string()
}

fn url_encode_path(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
