//! `root whoami` — print the logged-in identity, tier, and credits.

use anyhow::Result;
use console::style;

use thinkingroot_cloud_auth::config::{self, Config};

pub async fn run(server: Option<String>) -> Result<()> {
    let mut cfg = config::load()?.unwrap_or_else(Config::empty);
    if let Some(s) = server {
        cfg.server = s;
    }
    print!("{}", format_local_summary(&cfg));
    Ok(())
}

/// Pure formatter — reads only persisted fields; does NOT hit the
/// network. The future `--refresh` flag re-runs `/me` first.
pub fn format_local_summary(cfg: &Config) -> String {
    if !cfg.is_signed_in() {
        return format!(
            "{} not signed in. Run `root login` to start a session.\n",
            style("ⓘ").dim()
        );
    }
    let handle = cfg.handle.as_deref().unwrap_or("?");
    let tier = cfg.tier.as_deref().unwrap_or("free");
    let mut out = String::new();
    out.push_str(&format!(
        "{} signed in as {} ({} tier)\n",
        style("✓").green(),
        style(format!("@{handle}")).bold(),
        style(tier).cyan(),
    ));
    if let (Some(remaining), Some(total)) = (cfg.credits_remaining, cfg.credits_total) {
        out.push_str(&format!(
            "  credits: {} / {}",
            style(remaining.to_string()).cyan(),
            style(total.to_string()).dim(),
        ));
        if let Some(end) = cfg.credit_period_end {
            out.push_str(&format!(
                " · resets {}",
                style(end.format("%Y-%m-%d")).dim()
            ));
        }
        out.push('\n');
    }
    if let Some(expires) = cfg.token_expires_at {
        out.push_str(&format!(
            "  token expires: {}\n",
            style(expires.format("%Y-%m-%d")).dim()
        ));
    }
    if let Ok(p) = config::config_path() {
        out.push_str(&format!("  config: {}\n", style(p.display()).dim()));
    }
    out
}
