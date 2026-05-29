//! Outbound egress allowlist enforcement (engine side).
//!
//! Policy is owned by the cloud control plane (the `project_allowlist_domains`
//! table) and injected into the engine container as the
//! `TR_OUTBOUND_ALLOWLIST` env var (a comma-separated domain list) by the
//! provisioner at spawn. The engine ENFORCES it — the gateway never sees
//! engine→external calls, so enforcement cannot live there.
//!
//! Semantics:
//! - **Var unset** ⇒ allow-all. This is local/desktop dev parity: no
//!   cloud policy means no restriction.
//! - **Var set (even empty)** ⇒ default-deny; only listed domains (and
//!   their subdomains) are reachable.
//!
//! Enforcement points wired today:
//! - [`crate::acquisition_tools`] `mcp_server_install` — refuses to mount
//!   an HTTP MCP server whose endpoint host isn't allowlisted.
//! - [`allowed_hosts_for_sandbox`] — feeds the allowlist into a
//!   `thinkingroot_sandbox::SandboxPolicy` for shell acquisition.
//!
//! Root Functions themselves run in a `deno_core` isolate built from
//! `RuntimeOptions::default()`, which exposes NO `fetch`/network — so
//! they are network-denied by construction in v1, independent of this
//! list. Adding `deno_fetch` later MUST route through [`host_allowed`].

const ENV_VAR: &str = "TR_OUTBOUND_ALLOWLIST";

/// The configured allowlist, or `None` when the env var is unset
/// (allow-all). An empty/whitespace var parses to `Some(vec![])`
/// (deny-all).
pub fn allowlist_from_env() -> Option<Vec<String>> {
    parse_allowlist(std::env::var(ENV_VAR).ok().as_deref())
}

/// Pure parse of the raw env value (testable without touching env).
pub fn parse_allowlist(raw: Option<&str>) -> Option<Vec<String>> {
    raw.map(|s| {
        s.split(',')
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect()
    })
}

/// Whether `host` is permitted by `allowlist`. `None` ⇒ allow-all.
/// A host matches an entry if it equals it or is a subdomain of it
/// (`api.x.com` matches entry `x.com`).
pub fn host_allowed(host: &str, allowlist: &Option<Vec<String>>) -> bool {
    let host = host.trim().to_lowercase();
    match allowlist {
        None => true,
        Some(domains) => domains.iter().any(|d| {
            host == *d || host.ends_with(&format!(".{d}"))
        }),
    }
}

/// Convenience: check a host against the process-env allowlist.
pub fn host_allowed_from_env(host: &str) -> bool {
    host_allowed(host, &allowlist_from_env())
}

/// The hosts to pass into a `SandboxPolicy.allowed_hosts` for shell
/// acquisition. Empty vec when allow-all (the sandbox treats `*` /
/// empty per its own policy); the explicit domain list otherwise.
pub fn allowed_hosts_for_sandbox() -> Vec<String> {
    allowlist_from_env().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_allow_all() {
        let allow = parse_allowlist(None);
        assert!(host_allowed("anything.example.com", &allow));
    }

    #[test]
    fn set_is_default_deny() {
        let allow = parse_allowlist(Some("api.stripe.com, api.openai.com"));
        assert!(host_allowed("api.stripe.com", &allow));
        assert!(host_allowed("api.openai.com", &allow));
        // Subdomain of an allowed entry.
        assert!(host_allowed("eu.api.stripe.com", &allow));
        // Not listed.
        assert!(!host_allowed("evil.com", &allow));
        // A parent domain of an entry is NOT implied.
        assert!(!host_allowed("stripe.com", &allow));
    }

    #[test]
    fn empty_var_denies_everything() {
        let allow = parse_allowlist(Some("   "));
        assert_eq!(allow, Some(vec![]));
        assert!(!host_allowed("api.stripe.com", &allow));
    }
}
