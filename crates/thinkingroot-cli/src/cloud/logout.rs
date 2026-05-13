//! `root logout` — wipe auth.json.

use anyhow::Result;
use console::style;

use thinkingroot_cloud_auth::config;

pub async fn run() -> Result<()> {
    let signed_in_before = config::load()?.is_some_and(|c| c.is_signed_in());
    config::clear()?;
    if signed_in_before {
        println!("{} signed out", style("✓").green());
    } else {
        println!("{} no active session — nothing to clear", style("ⓘ").dim());
    }
    Ok(())
}
