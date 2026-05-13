//! `root login` — opens a browser for sign-in; falls back to
//! `--token <T>` paste mode for headless / sandbox environments.

use anyhow::{Context, Result};
use console::style;
use tokio_util::sync::CancellationToken;

use thinkingroot_cloud_auth::auth_flow::{Surface, run_browser_login};
use thinkingroot_cloud_auth::{config, me};

/// Run the login flow.
///
/// - `token: Some(t)` → paste-mode (uses `t`, validates via `/me`,
///   persists). Prints a warning that the token will be in shell
///   history.
/// - `token: None` → browser-flow (opens browser, waits 60s for
///   callback, persists, prefetches credits).
/// - `server` overrides the configured server URL.
/// - `no_browser: true` → never opens a browser, only honours
///   `--token`. Surfaces an error if both are absent.
pub async fn run(
    token: Option<String>,
    server: Option<String>,
    no_browser: bool,
) -> Result<()> {
    let server = server
        .or_else(|| config::load().ok().flatten().map(|c| c.server))
        .unwrap_or_else(|| "https://api.thinkingroot.dev".into());

    match (token, no_browser) {
        (Some(t), _) => {
            eprintln!(
                "{} note: `--token` puts your token in shell history. \
                 Prefer the browser flow (omit `--token`) when possible.",
                style("⚠").yellow(),
            );
            run_paste_mode(t, server).await
        }
        (None, true) => {
            anyhow::bail!(
                "no token provided and --no-browser is set. \
                 Run `root login --token <T>` with a token from the hub."
            );
        }
        (None, false) => run_browser_mode(server).await,
    }
}

async fn run_paste_mode(token: String, server: String) -> Result<()> {
    println!(
        "{} authenticating against {}",
        style("→").cyan(),
        style(&server).dim()
    );
    let mut cfg = config::load()
        .context("load auth.json")?
        .unwrap_or_else(config::Config::empty);
    cfg.token = Some(token);
    cfg.server = server.clone();
    let me_resp = me::fetch_me(&cfg)
        .await
        .context("verify token via /me")?;
    cfg.user_id = Some(me_resp.user.id.clone());
    cfg.handle = Some(me_resp.user.handle.clone());
    cfg.tier = Some(me_resp.user.tier.clone());
    cfg.token_expires_at = Some(me_resp.token_expires_at);
    cfg.credit_period_end = Some(me_resp.credit_period_end);
    cfg.me_refreshed_at = Some(chrono::Utc::now());
    config::save(&cfg).context("save auth.json")?;
    print_success(&me_resp.user.handle, &me_resp.user.tier);
    Ok(())
}

async fn run_browser_mode(server: String) -> Result<()> {
    println!(
        "{} opening browser to sign in at {}",
        style("→").cyan(),
        style(&server).dim()
    );
    let cancel = CancellationToken::new();
    let cancel_for_ctrlc = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_for_ctrlc.cancel();
    });
    let outcome = run_browser_login(&server, Surface::Cli, cancel)
        .await
        .context("browser-flow login")?;
    print_success(&outcome.handle, &outcome.tier);
    if let Some(credits) = outcome.credits_remaining {
        println!(
            "  credits: {}",
            style(format!("{credits} remaining")).cyan()
        );
    }
    Ok(())
}

fn print_success(handle: &str, tier: &str) {
    println!(
        "{} signed in as {} ({} tier)",
        style("✓").green(),
        style(format!("@{handle}")).bold(),
        style(tier).cyan(),
    );
    if let Ok(p) = config::config_path() {
        println!("  config: {}", style(p.display()).dim());
    }
}
