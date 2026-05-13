//! `root whoami` — print the cloud identity associated with the saved
//! API token.

use anyhow::Result;
use console::style;
use serde::Deserialize;

use super::{http, load_or_default, require_token};

#[derive(Debug, Deserialize)]
struct MeUser {
    id: String,
    handle: String,
}

#[derive(Debug, Deserialize)]
struct MeResponse {
    user: MeUser,
}

pub async fn run(server_override: Option<String>) -> Result<()> {
    let cfg = load_or_default(server_override.as_deref())?;
    let token = require_token(&cfg)?;
    let http = http::client()?;
    let me: MeResponse = http::get_json(
        &http,
        &format!("{}/me", cfg.server.trim_end_matches('/')),
        token,
    )
    .await?;
    println!(
        "{} @{} ({})",
        style("•").cyan(),
        style(&me.user.handle).bold(),
        style(&me.user.id).dim()
    );
    println!("  server: {}", style(&cfg.server).dim());
    Ok(())
}
