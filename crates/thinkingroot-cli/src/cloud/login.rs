//! `root login` — paste your API token, validate against the cloud's
//! `/me` endpoint, persist server + token + cached identity.

use anyhow::{anyhow, Result};
use console::style;
use serde::Deserialize;

use super::{config, http};

#[derive(Debug, Deserialize)]
struct MeUser {
    id: String,
    handle: String,
    #[allow(dead_code)]
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MeResponse {
    user: MeUser,
}

pub async fn run(token: Option<String>, server: Option<String>) -> Result<()> {
    let server = server
        .or_else(|| config::load_or_default(None).ok().map(|c| c.server))
        .unwrap_or_else(|| "https://api.thinkingroot.dev".into());

    let token = match token {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => prompt_token()?,
    };
    if token.is_empty() {
        return Err(anyhow!("token required"));
    }

    println!(
        "{} authenticating against {}",
        style("→").cyan(),
        style(&server).dim()
    );
    let http = http::client()?;
    let me: MeResponse = http::get_json(
        &http,
        &format!("{}/me", server.trim_end_matches('/')),
        &token,
    )
    .await?;

    let mut cfg = config::load_or_default(None).unwrap_or_else(|_| config::Config::empty());
    cfg.token = Some(token);
    cfg.server = server.clone();
    cfg.handle = Some(me.user.handle.clone());
    cfg.user_id = Some(me.user.id.clone());
    config::save(&cfg)?;

    println!(
        "{} signed in as {} ({})",
        style("✓").green(),
        style(format!("@{}", me.user.handle)).bold(),
        style(&me.user.id).dim()
    );
    println!(
        "  config: {}",
        style(config::config_path()?.display()).dim()
    );
    Ok(())
}

fn prompt_token() -> Result<String> {
    println!(
        "{} create a token at {} → /settings/api-tokens, then paste it here.",
        style("?").cyan(),
        style("the hub").bold()
    );
    let t = rpassword::prompt_password("token: ")?;
    Ok(t.trim().to_string())
}
