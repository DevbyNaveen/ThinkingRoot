//! Path-traversal-safe joining and identifier validation primitives.
//!
//! Every place we accept untrusted path input — `.tr` pack tar
//! extraction, Tauri `fs::*` commands the webview can call, conversation
//! IDs from JS — has the same shape: a base directory we trust + a
//! string we don't, joined to produce a destination. Without explicit
//! containment checks `Path::join` happily admits `../../etc/passwd`
//! (collapses up the tree) and absolute paths like `/etc/passwd`
//! (silently discards the base). Both classes are exploited by zip-
//! slip / tar-slip attacks.
//!
//! This module exposes one safe-join helper plus one identifier
//! validator. They are deliberately small and free of cleverness so
//! security-review reads them quickly.

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Join `untrusted` onto `base` and assert the result canonically
/// stays *inside* `base`. Refuses parent-directory components, refuses
/// absolute path inputs, and refuses inputs whose canonicalised path
/// (after symlink resolution if the target exists) escapes `base`.
///
/// Use this at every boundary that accepts a filesystem path string
/// from outside the trust boundary: tar entry names, Tauri command
/// arguments, conversation IDs joined into directories, etc.
///
/// `base` does not need to exist; `untrusted` is checked component-by-
/// component before any I/O. If the joined path's parent already
/// exists, the parent is canonicalised and re-checked to defeat
/// symlink trickery (a tar entry whose parent directory is a symlink
/// out of the workspace).
pub fn safe_join_under(base: &Path, untrusted: impl AsRef<Path>) -> Result<PathBuf> {
    let untrusted = untrusted.as_ref();

    // Reject absolute inputs outright. `Path::join` discards the
    // base when the right-hand side is absolute, which would
    // silently drop us at the filesystem root.
    if untrusted.is_absolute() {
        return Err(Error::SecurityViolation(format!(
            "absolute path rejected: {}",
            untrusted.display()
        )));
    }

    // Walk the components and refuse anything that would escape the
    // base or that contains an OS-specific scary form (Prefix on
    // Windows, RootDir).
    for c in untrusted.components() {
        match c {
            Component::ParentDir => {
                return Err(Error::SecurityViolation(format!(
                    "parent-directory component rejected: {}",
                    untrusted.display()
                )));
            }
            Component::RootDir => {
                return Err(Error::SecurityViolation(format!(
                    "root-directory component rejected: {}",
                    untrusted.display()
                )));
            }
            Component::Prefix(_) => {
                return Err(Error::SecurityViolation(format!(
                    "drive-prefix component rejected: {}",
                    untrusted.display()
                )));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    let joined = base.join(untrusted);

    // Symlink defence: if any ancestor of the join target already
    // exists, canonicalise the deepest existing ancestor and assert
    // it stays inside `base`'s canonical form. Skip this when neither
    // side exists yet (first-run install on a fresh dir).
    let base_canon = match base.canonicalize() {
        Ok(c) => c,
        Err(_) => base.to_path_buf(),
    };
    let mut probe = joined.as_path();
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p,
            None => break,
        }
    }
    if probe.exists()
        && let Ok(probe_canon) = probe.canonicalize()
        && !probe_canon.starts_with(&base_canon)
    {
        return Err(Error::SecurityViolation(format!(
            "canonicalised path escapes base: {} not under {}",
            probe_canon.display(),
            base_canon.display()
        )));
    }

    Ok(joined)
}

/// Validate that `id` is a safe filesystem-name component — no path
/// separators, no parent-dir traversal, no leading dots, no NUL,
/// no control characters, and within a sane length cap.
///
/// Used for conversation IDs, branch names, workspace slugs, and any
/// other JS- or LLM-supplied string used directly as a filename.
/// Names that fail here would be rejected by `safe_join_under` later
/// anyway, but failing early at the boundary gives a cleaner error
/// message and prevents accidental `Path::new(id).file_name()` style
/// shortcuts elsewhere in the call graph.
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::SecurityViolation("identifier is empty".into()));
    }
    if id.len() > 255 {
        return Err(Error::SecurityViolation(format!(
            "identifier exceeds 255 chars: {} chars",
            id.len()
        )));
    }
    if id == "." || id == ".." {
        return Err(Error::SecurityViolation(format!(
            "identifier is path-traversal: `{}`",
            id
        )));
    }
    if id.starts_with('.') {
        return Err(Error::SecurityViolation(format!(
            "identifier starts with `.`: `{}`",
            id
        )));
    }
    for ch in id.chars() {
        match ch {
            '/' | '\\' | '\0' => {
                return Err(Error::SecurityViolation(format!(
                    "identifier contains path separator or NUL: `{}`",
                    id
                )));
            }
            c if c.is_control() => {
                return Err(Error::SecurityViolation(format!(
                    "identifier contains control character: `{}`",
                    id
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Returns `true` iff `host` is a literal loopback host: `127.0.0.0/8`,
/// `localhost`, or `::1`. Uses real IP-address parsing rather than
/// string-prefix matching, so `127.evil.com` is correctly classified
/// as non-loopback even though it starts with `127.`.
///
/// Use at every place HTTPS is enforced for non-loopback hosts:
/// `root install`, the desktop's pack download path, anywhere we
/// allow plain HTTP only for tests / on-host registries.
pub fn is_loopback_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" {
        return true;
    }
    // Strip surrounding brackets if any (IPv6 in URL form).
    let trimmed = lower
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&lower);
    if let Ok(addr) = trimmed.parse::<std::net::IpAddr>() {
        return addr.is_loopback();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn safe_join_accepts_normal_relative_path() {
        let base = std::env::temp_dir().join("safe-join-ok");
        fs::create_dir_all(&base).unwrap();
        let p = safe_join_under(&base, "sub/dir/file.txt").unwrap();
        assert!(p.starts_with(&base));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn safe_join_rejects_parent_dir_traversal() {
        let base = std::env::temp_dir().join("safe-join-parent");
        let err = safe_join_under(&base, "../etc/passwd").unwrap_err();
        assert!(matches!(err, Error::SecurityViolation(_)));
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        let base = std::env::temp_dir().join("safe-join-abs");
        let err = safe_join_under(&base, "/etc/passwd").unwrap_err();
        assert!(matches!(err, Error::SecurityViolation(_)));
    }

    #[test]
    fn safe_join_rejects_deeply_nested_parent_dirs() {
        let base = std::env::temp_dir().join("safe-join-deep");
        let err =
            safe_join_under(&base, "a/b/c/../../../../../../etc/passwd").unwrap_err();
        assert!(matches!(err, Error::SecurityViolation(_)));
    }

    #[test]
    fn safe_join_rejects_symlink_escape() {
        // Construct: base/link -> /tmp ; then ask to join "link/foo".
        // The component check passes (no `..`), but the canonicalised
        // probe must catch that base/link resolves outside base.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        fs::create_dir_all(&base).unwrap();
        let target = tmp.path().join("escape");
        fs::create_dir_all(&target).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&target, base.join("link")).unwrap();
            let err = safe_join_under(&base, "link/foo").unwrap_err();
            assert!(matches!(err, Error::SecurityViolation(_)));
        }
        #[cfg(not(unix))]
        {
            // Symlink creation requires elevated privileges on
            // Windows; the component-level check still applies, so
            // the OS-agnostic guarantee ("inputs with `..` rejected")
            // is exercised by other tests.
            let _ = (base, target);
        }
    }

    #[test]
    fn validate_id_accepts_normal_names() {
        validate_id("conversation-12345").unwrap();
        validate_id("alice_branch").unwrap();
        validate_id("01HW7XQE5K3KCYP3GAXEXAMPLE").unwrap();
    }

    #[test]
    fn validate_id_rejects_dotdot() {
        assert!(validate_id("..").is_err());
    }

    #[test]
    fn validate_id_rejects_path_separators() {
        assert!(validate_id("foo/bar").is_err());
        assert!(validate_id("foo\\bar").is_err());
    }

    #[test]
    fn validate_id_rejects_leading_dot() {
        // Hidden files surprise users + may collide with auxiliary
        // bookkeeping like `.first_boot_at`.
        assert!(validate_id(".hidden").is_err());
    }

    #[test]
    fn validate_id_rejects_control_chars() {
        assert!(validate_id("foo\nbar").is_err());
        assert!(validate_id("foo\0bar").is_err());
        assert!(validate_id("foo\u{7f}bar").is_err());
    }

    #[test]
    fn validate_id_rejects_oversize() {
        let s: String = "a".repeat(256);
        assert!(validate_id(&s).is_err());
    }

    #[test]
    fn loopback_classifies_real_loopback_ips() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.2"));
        assert!(is_loopback_host("127.255.255.255"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LocalHost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
    }

    #[test]
    fn loopback_rejects_non_loopback_lookalikes() {
        // `127.evil.com` is the canonical bypass for prefix-based
        // checks.  The new IP-parse-first implementation rejects it.
        assert!(!is_loopback_host("127.evil.com"));
        assert!(!is_loopback_host("evil.com"));
        assert!(!is_loopback_host("8.8.8.8"));
        assert!(!is_loopback_host("0.0.0.0")); // unspecified, NOT loopback
    }
}
