//! Model catalogue fetcher with 1-hour cache in auth.json.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md`
//! §6.4. The catalogue is the source of truth for which managed models
//! the user's tier can target + their per-1k-token credit cost. Cached
//! in `Config.model_catalogue_cached` so `root provider set
//! thinkingroot-cloud --model …` (Task 9) can validate the model name
//! without a round-trip every invocation.

use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;

use crate::config::{self, Config, ModelCatalogue, ModelEntry};
use crate::error::CloudError;
use crate::http;

/// 1-hour TTL per spec §6.4. Catalogue churn is on the order of days
/// (Anthropic ships new model versions ~monthly); 1 hour balances
/// staleness vs network cost. Override is per-call via `force_refresh`.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

/// Fetch the model catalogue from the hub.
///
/// - `force_refresh: false` — return the cached catalogue if within
///   the 1-hour TTL; else fetch + persist.
/// - `force_refresh: true` — always fetch, always persist.
///
/// Requires a signed-in `Config` (`token` populated). When the user is
/// signed out, returns [`CloudError::NotLoggedIn`] **before** any
/// network attempt — honesty rule, the cloud can't answer for an
/// anonymous client and we don't want to leak the server URL probe.
pub async fn fetch_models(force_refresh: bool) -> Result<Vec<ModelEntry>, CloudError> {
    let cfg = config::load()?.unwrap_or_else(Config::empty);
    if !cfg.is_signed_in() {
        return Err(CloudError::NotLoggedIn);
    }

    if !force_refresh {
        if let Some(cached) = cfg.model_catalogue_cached.as_ref() {
            let age = Utc::now()
                .signed_duration_since(cached.fetched_at)
                .to_std()
                .unwrap_or(CACHE_TTL);
            if age < CACHE_TTL {
                return Ok(cached.models.clone());
            }
        }
    }

    let http = http::client()?;
    let url = format!("{}/v1/models", cfg.server.trim_end_matches('/'));
    let token = cfg.token.as_deref().ok_or(CloudError::NotLoggedIn)?;
    let resp: ModelsResponse = http::get_json(&http, &url, token).await?;
    let now = Utc::now();
    let catalogue = ModelCatalogue {
        fetched_at: now,
        models: resp.data,
    };
    let models = catalogue.models.clone();
    config::update(|c| {
        c.model_catalogue_cached = Some(catalogue.clone());
    })?;

    Ok(models)
}
