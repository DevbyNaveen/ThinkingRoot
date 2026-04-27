//! Auth state introspection.
//!
//! Today the desktop has no sign-in flow — conversations live entirely
//! in the local workspace's `.thinkingroot/conversations/` directory. A
//! cloud-sync mirror is on the roadmap but the cloud has no
//! `conversations` service yet, so any HTTP call we wired here would
//! be a fabricated endpoint. We refuse to fake it.
//!
//! What we *can* honestly report is whether a token has been seeded
//! (via `tr login` from the publish CLI, which writes
//! `TR_CLOUD_TOKEN` into the same `desktop.toml` we read elsewhere)
//! and what the configured cloud base URL is. The UI uses this to
//! render an honest "Local only" / "Signed in" badge on the sidebar
//! and to gate the future sync hook when it lands.

use serde::Serialize;

use crate::config::AppConfig;

#[derive(Debug, Serialize, Clone)]
pub struct AuthState {
    /// True when both a base URL and a non-empty token are present.
    pub signed_in: bool,
    /// Base URL of the cloud the desktop would talk to. Empty when
    /// the user has not configured one.
    pub cloud_base_url: Option<String>,
    /// Cloud user handle from config, when present. We only read it —
    /// we do not currently verify it against the cloud.
    pub handle: Option<String>,
    /// Honest description of what's persisting where.
    pub storage: StorageSummary,
}

#[derive(Debug, Serialize, Clone)]
pub struct StorageSummary {
    /// Always `true` — every conversation is written to
    /// `<workspace>/.thinkingroot/conversations/`.
    pub local: bool,
    /// `false` until a cloud conversations service exists. The UI
    /// renders this as "Local only" rather than fabricating a sync
    /// status.
    pub cloud: bool,
}

#[tauri::command]
pub fn auth_state() -> Result<AuthState, String> {
    let cfg = AppConfig::load().map_err(|e| e.to_string())?;
    let token = cfg.env_or("TR_CLOUD_TOKEN").unwrap_or_default();
    let base = cfg.env_or("TR_CLOUD_API_BASE");
    let handle = cfg.env_or("TR_CLOUD_HANDLE");
    let signed_in = !token.is_empty() && base.as_ref().is_some_and(|b| !b.is_empty());
    Ok(AuthState {
        signed_in,
        cloud_base_url: base,
        handle,
        storage: StorageSummary {
            local: true,
            // Cloud sync is wired only when both a token *and* a real
            // conversations API exist. Today the API does not. Set
            // this to true only when the cloud ships the route — we
            // refuse to lie about it in the meantime.
            cloud: false,
        },
    })
}
