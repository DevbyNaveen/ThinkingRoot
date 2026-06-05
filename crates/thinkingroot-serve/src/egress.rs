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
//! Root Functions reach the network via the host `op_tr_fetch` op (the
//! `tr_fetch` extension), which enforces TWO layers before any request:
//! 1. [`host_allowed_from_env`] — the project's `TR_OUTBOUND_ALLOWLIST`.
//! 2. [`vet_outbound_host`] — an SSRF guard that rejects loopback / private /
//!    link-local (incl. the `169.254.169.254` cloud-metadata IP) / ULA hosts,
//!    whether given as an IP literal OR a hostname that *resolves* to one, and
//!    the fetch client follows NO redirects (so an allowlisted host can't 302
//!    a request onto an internal IP). Any new outbound path MUST route through
//!    BOTH [`host_allowed`] and [`vet_outbound_host`].

use std::net::IpAddr;

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

/// True if `ip` is in a range an outbound Root-Function request must never be
/// allowed to reach: loopback, RFC-1918 private, link-local (169.254.0.0/16 —
/// the cloud-metadata range, incl. `169.254.169.254`), CGNAT shared space,
/// unspecified/broadcast/documentation, and IPv6 loopback/unspecified/ULA
/// (fc00::/7) / link-local (fe80::/10) — plus IPv4-mapped forms of any above.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                // CGNAT / shared address space 100.64.0.0/10.
                || (o[0] == 100 && (o[1] & 0xC0) == 0x40)
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&IpAddr::V4(mapped));
            }
            let s0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (s0 & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (s0 & 0xfe00) == 0xfc00 // unique-local fc00::/7
        }
    }
}

/// SSRF guard for an outbound host. Rejects internal-name targets and any host
/// that is — or DNS-resolves to — an internal/private IP (see [`is_blocked_ip`]).
/// This is the second layer after the allowlist: even an allowlisted hostname is
/// refused if it points at an internal address. Best-effort against DNS
/// rebinding (resolve-time view), but it closes the cloud-metadata / internal
/// reachability hole. Returns `Err(reason)` when the host must be blocked.
pub async fn vet_outbound_host(host: &str) -> Result<(), String> {
    let h = host.trim().trim_end_matches('.').to_lowercase();
    if h.is_empty() {
        return Err("empty host".into());
    }
    // Internal names that never have a legitimate outbound use.
    if h == "localhost"
        || h.ends_with(".localhost")
        || h == "metadata.google.internal"
        || h.ends_with(".internal")
    {
        return Err(format!("host '{host}' targets an internal/metadata endpoint"));
    }
    // Literal IP — check directly (no DNS).
    if let Ok(ip) = h.parse::<IpAddr>() {
        return if is_blocked_ip(&ip) {
            Err(format!("host '{host}' is an internal/private IP — blocked"))
        } else {
            Ok(())
        };
    }
    // Hostname — resolve and reject if ANY address is internal (DNS→internal).
    let addrs = tokio::net::lookup_host((h.as_str(), 0u16))
        .await
        .map_err(|e| format!("could not resolve host '{host}': {e}"))?;
    let mut resolved_any = false;
    for addr in addrs {
        resolved_any = true;
        if is_blocked_ip(&addr.ip()) {
            return Err(format!(
                "host '{host}' resolves to an internal/private IP — blocked"
            ));
        }
    }
    if !resolved_any {
        return Err(format!("host '{host}' did not resolve to any address"));
    }
    Ok(())
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

    #[test]
    fn blocks_internal_ip_literals() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254", // cloud metadata
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1", // CGNAT
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1", // IPv4-mapped loopback
        ] {
            assert!(
                is_blocked_ip(&ip.parse().unwrap()),
                "{ip} must be blocked"
            );
        }
    }

    #[test]
    fn allows_public_ip_literals() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(!is_blocked_ip(&ip.parse().unwrap()), "{ip} must be allowed");
        }
    }

    #[tokio::test]
    async fn vet_rejects_internal_literals_and_names() {
        assert!(vet_outbound_host("169.254.169.254").await.is_err());
        assert!(vet_outbound_host("127.0.0.1").await.is_err());
        assert!(vet_outbound_host("localhost").await.is_err());
        assert!(vet_outbound_host("metadata.google.internal").await.is_err());
        assert!(vet_outbound_host("foo.internal").await.is_err());
        // A public literal IP passes the vet (no DNS needed).
        assert!(vet_outbound_host("8.8.8.8").await.is_ok());
    }
}
