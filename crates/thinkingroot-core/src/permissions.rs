//! Phase D Wave 1 (2026-05-17) — identity-level permission rules
//! for the system-power tools (`file_read`, `file_write`,
//! `file_edit`, `glob`, `grep`, `shell_exec`, `clipboard_*`,
//! `open_in_default`, `trash`).
//!
//! ## Storage
//!
//! Persisted at `<dirs::config_dir()>/thinkingroot/permissions.toml`
//! with file mode `0600` on Unix via [`crate::safe_path::atomic_write`].
//! Sibling to `cortex.lock` and `credentials.toml` — explicitly
//! NOT inside `Config` because:
//!
//! 1. `Config::save` strips fields keyed by `api_key`; permission
//!    rule patterns containing literal `**` and dotted paths would
//!    pass through the strip but the design intent is wrong — a
//!    permission rule isn't a secret, it's an identity preference.
//! 2. `Config::load_merged` does per-workspace inheritance; a
//!    workspace-local "allow `~/Code/**`" should NOT silently
//!    become the default for every other workspace. Permissions
//!    are identity-scoped, not workspace-scoped.
//!
//! ## DEFAULT_DENY — hardcoded, never overridable
//!
//! [`DEFAULT_DENY`] is a compile-time const list of paths that
//! ThinkingRoot will refuse to access under any tool invocation,
//! regardless of user-authored rules. Inserting a user rule whose
//! pattern overlaps a DEFAULT_DENY pattern returns
//! [`PermissionError::ProtectedPath`]. This closes the "LLM talks
//! the user into approving `~/.ssh/**`" attack — even an
//! "allow_always" decision arrives at [`PermissionStore::insert_rule`]
//! and is refused before it can persist.
//!
//! At evaluation time, DEFAULT_DENY is checked FIRST. User rules
//! only matter for paths not matched by DEFAULT_DENY — so even if
//! a stale user rule somehow exists (e.g. file edited by hand),
//! the DEFAULT_DENY layer still fires.
//!
//! ## Canonicalisation invariant
//!
//! All paths fed into [`PermissionStore::evaluate_path`] MUST be
//! resolved via [`crate::safe_path::canonicalize_for_policy`]
//! BEFORE evaluation. Without that, an LLM-suggested path like
//! `./notes/id_rsa` (where `./notes` is a symlink to `~/.ssh`)
//! never matches the literal `~/.ssh/**` pattern. The
//! `PermissionsGate` wrapper in `thinkingroot-serve` enforces
//! this at the call site.

use std::path::Path;

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::safe_path::atomic_write;

/// Wire-format schema version for `permissions.toml`. Reader-bumped:
/// a binary on schema N refuses to parse a file with schema > N
/// and surfaces [`PermissionError::SchemaMismatch`] so future
/// fields don't get silently dropped.
pub const SCHEMA_VERSION: u32 = 1;

/// File mode used when persisting `permissions.toml` on Unix.
/// Owner read/write only; never readable by other users on a
/// shared host.
pub const FILE_MODE: u32 = 0o600;

/// Hardcoded paths and patterns that ThinkingRoot will refuse to
/// touch regardless of user rules.
///
/// **This list is intentionally broad.** The browser-profile entries
/// in particular cover Chrome/Firefox/Safari/Edge cookies + saved
/// passwords across all OSes; refusing to expose them by default
/// is the difference between "personal AI" and "credential
/// exfiltration tool the user signed up for."
///
/// The `~/` prefix is resolved against the running user's home
/// directory at evaluation time (via [`expand_tilde`]). The `**`
/// glob matches any number of path components including zero.
pub const DEFAULT_DENY: &[&str] = &[
    // SSH keys, GPG keyrings, AWS credentials — the classic
    // privilege-escalation surface.
    "~/.ssh/**",
    "~/.aws/**",
    "~/.gnupg/**",
    "~/.kube/**",
    "~/.docker/config.json",
    // ThinkingRoot's own credential file.
    "~/.config/thinkingroot/credentials*",
    "**/.config/thinkingroot/credentials*",
    // Generic dotenv + credentials patterns — covers .env, .env.local,
    // credentials.json, secrets.toml, etc.
    "**/.env",
    "**/.env.*",
    "**/credentials*",
    "**/secrets*",
    "**/*.pem",
    "**/*_rsa",
    "**/*_ed25519",
    "**/*.p12",
    // macOS browser profiles (cookies + saved passwords).
    "~/Library/Application Support/Google/Chrome/**",
    "~/Library/Application Support/Chromium/**",
    "~/Library/Application Support/Firefox/**",
    "~/Library/Application Support/com.apple.Safari/**",
    "~/Library/Containers/com.apple.Safari/**",
    "~/Library/Keychains/**",
    "~/Library/Cookies/**",
    // Linux browser profiles.
    "~/.config/google-chrome/**",
    "~/.config/chromium/**",
    "~/.mozilla/**",
    "~/.config/BraveSoftware/**",
    // Linux secrets services.
    "~/.local/share/keyrings/**",
    "~/.password-store/**",
];

/// Kind of rule.  `Path` rules match the canonical path that a
/// tool wants to access; `Command` rules match the shell command
/// being spawned by `shell_exec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Path,
    Command,
}

/// What the gate should do when the rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allow the tool call without prompting the user.
    Allow,
    /// Refuse the tool call. The agent receives a rejection
    /// with the matched rule pattern as the reason.
    Deny,
    /// Prompt the user for a one-shot or persistent decision.
    Ask,
}

/// A single user-authored permission rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub kind: RuleKind,
    pub pattern: String,
    pub decision: Decision,
    #[serde(default = "now_utc")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default = "default_created_by")]
    pub created_by: String,
}

fn now_utc() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn default_created_by() -> String {
    "user".to_string()
}

/// Typed errors returned by the permission store.
#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    /// On-disk schema is newer than this binary understands. The
    /// user has upgraded then downgraded ThinkingRoot, or the file
    /// was hand-edited.  We refuse to parse rather than silently
    /// dropping fields we don't recognise.
    #[error(
        "permissions.toml schema version mismatch: file is v{file}, this binary expects ≤ v{expected}"
    )]
    SchemaMismatch { file: u32, expected: u32 },

    /// The user (or the LLM via an `allow_always` request) tried
    /// to insert a rule whose pattern overlaps with a [`DEFAULT_DENY`]
    /// pattern. The hardcoded protection always wins.
    #[error(
        "path is protected by ThinkingRoot's hardcoded security policy: \
         pattern `{pattern}` conflicts with default-deny `{conflicts_with}`"
    )]
    ProtectedPath {
        pattern: String,
        conflicts_with: String,
    },

    /// The pattern string is not a valid glob.
    #[error("invalid glob pattern `{pattern}`: {reason}")]
    InvalidPattern { pattern: String, reason: String },

    /// IO error reading or writing `permissions.toml`.
    #[error("permissions.toml io error at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: crate::error::Error,
    },

    /// TOML parse error.
    #[error("permissions.toml parse error at `{path}`: {source}")]
    TomlParse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    /// TOML serialize error.
    #[error("permissions.toml serialize error: {source}")]
    TomlSerialize {
        #[source]
        source: toml::ser::Error,
    },
}

/// The persistent permission store. Loaded from disk at agent
/// startup and re-saved whenever a user decision adds a rule via
/// the `allow_always` / `deny_always` UI prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionStore {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for PermissionStore {
    fn default() -> Self {
        Self::empty()
    }
}

impl PermissionStore {
    /// An empty store with the current schema version.
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            rules: Vec::new(),
        }
    }

    /// Load from disk. Returns [`Self::empty`] when the file
    /// doesn't exist — first-run behaviour.
    pub fn load(path: &Path) -> Result<Self, PermissionError> {
        if !path.exists() {
            return Ok(Self::empty());
        }
        let bytes = std::fs::read_to_string(path).map_err(|e| PermissionError::Io {
            path: path.display().to_string(),
            source: crate::error::Error::io_path(path, e),
        })?;
        let store: Self = toml::from_str(&bytes).map_err(|e| PermissionError::TomlParse {
            path: path.display().to_string(),
            source: e,
        })?;
        if store.schema_version > SCHEMA_VERSION {
            return Err(PermissionError::SchemaMismatch {
                file: store.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        Ok(store)
    }

    /// Persist atomically with file mode `0600`. Creates parent
    /// directories as needed.
    pub fn save(&self, path: &Path) -> Result<(), PermissionError> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| PermissionError::TomlSerialize { source: e })?;
        atomic_write(path, text.as_bytes(), Some(FILE_MODE)).map_err(|e| PermissionError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        Ok(())
    }

    /// Insert a new rule. Returns [`PermissionError::ProtectedPath`]
    /// when the rule's pattern overlaps with [`DEFAULT_DENY`] — the
    /// hardcoded protection always wins and a user (or an LLM-
    /// influenced UI click) cannot override it.
    ///
    /// "Overlap" is determined by sampling a literal probe path
    /// from the user's pattern (the pattern's longest literal
    /// prefix, with `*` and `**` replaced by `x`) and evaluating
    /// the probe against the DEFAULT_DENY glob set. If DEFAULT_DENY
    /// matches the probe, we refuse the insert.  This is
    /// intentionally conservative: it catches the common attack
    /// (`allow ~/.ssh/foo`) without requiring full glob-overlap
    /// arithmetic.
    pub fn insert_rule(&mut self, rule: Rule) -> Result<(), PermissionError> {
        if rule.kind == RuleKind::Path {
            let probe = probe_path_from_pattern(&rule.pattern);
            if let Some(conflict) = default_deny_match(&probe)? {
                return Err(PermissionError::ProtectedPath {
                    pattern: rule.pattern.clone(),
                    conflicts_with: conflict,
                });
            }
        }
        // Validate the pattern compiles as a glob.
        match rule.kind {
            RuleKind::Path => {
                let expanded = expand_tilde(&rule.pattern);
                Glob::new(&expanded).map_err(|e| PermissionError::InvalidPattern {
                    pattern: rule.pattern.clone(),
                    reason: e.to_string(),
                })?;
            }
            RuleKind::Command => {
                Glob::new(&rule.pattern).map_err(|e| PermissionError::InvalidPattern {
                    pattern: rule.pattern.clone(),
                    reason: e.to_string(),
                })?;
            }
        }
        self.rules.push(rule);
        Ok(())
    }

    /// Evaluate a canonicalised path against the rule set.
    ///
    /// **Precondition:** the caller MUST have already resolved
    /// `canonical_path` via [`crate::safe_path::canonicalize_for_policy`].
    /// Passing a non-canonical path here is a programming error
    /// and bypasses the symlink-resolution invariant.
    ///
    /// Order of evaluation:
    /// 1. [`DEFAULT_DENY`] — match returns [`Decision::Deny`].
    /// 2. User rules in insertion order; first match wins.
    /// 3. No match returns [`Decision::Ask`] — surface a prompt.
    pub fn evaluate_path(&self, canonical_path: &Path) -> Decision {
        let s = canonical_path.to_string_lossy();
        if default_deny_match(&s).ok().flatten().is_some() {
            return Decision::Deny;
        }
        for rule in &self.rules {
            if rule.kind != RuleKind::Path {
                continue;
            }
            let expanded = expand_tilde(&rule.pattern);
            if let Ok(g) = Glob::new(&expanded)
                && g.compile_matcher().is_match(s.as_ref())
            {
                return rule.decision;
            }
        }
        Decision::Ask
    }

    /// Evaluate a shell command string against command-kind rules.
    /// Returns [`Decision::Ask`] when no rule matches.
    pub fn evaluate_command(&self, command: &str) -> Decision {
        for rule in &self.rules {
            if rule.kind != RuleKind::Command {
                continue;
            }
            if let Ok(g) = Glob::new(&rule.pattern)
                && g.compile_matcher().is_match(command)
            {
                return rule.decision;
            }
        }
        Decision::Ask
    }
}

/// Expand a leading `~/` or bare `~` to the running user's home
/// directory. Leaves everything else untouched (including embedded
/// `~` in middle-of-path positions, which are valid filename
/// characters on every OS).
fn expand_tilde(pattern: &str) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}/{}", home.display(), rest);
        }
    } else if pattern == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.display().to_string();
        }
    }
    pattern.to_string()
}

/// Build a probe path from a user-supplied glob pattern. Replaces
/// `**` with `x/x` and `*` with `x` so the resulting string is a
/// concrete-looking path under the same literal prefix as the
/// pattern. Used by [`PermissionStore::insert_rule`] to test
/// whether the user's pattern overlaps with DEFAULT_DENY.
fn probe_path_from_pattern(pattern: &str) -> String {
    let expanded = expand_tilde(pattern);
    let mut buf = String::with_capacity(expanded.len());
    let mut chars = expanded.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                // `**` consumes another `*`.
                if matches!(chars.peek(), Some('*')) {
                    chars.next();
                    buf.push_str("x/x");
                } else {
                    buf.push('x');
                }
            }
            '?' => buf.push('x'),
            other => buf.push(other),
        }
    }
    buf
}

/// Compile [`DEFAULT_DENY`] into a vec of (pattern, matcher) and
/// return the first matching pattern if any.
fn default_deny_match(path: &str) -> Result<Option<String>, PermissionError> {
    for raw in DEFAULT_DENY {
        let expanded = expand_tilde(raw);
        let matcher = compile_matcher(&expanded, raw)?;
        if matcher.is_match(path) {
            return Ok(Some((*raw).to_string()));
        }
    }
    Ok(None)
}

fn compile_matcher(expanded: &str, source: &str) -> Result<GlobMatcher, PermissionError> {
    let glob = Glob::new(expanded).map_err(|e| PermissionError::InvalidPattern {
        pattern: source.to_string(),
        reason: e.to_string(),
    })?;
    Ok(glob.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rule_allow_path(pattern: &str) -> Rule {
        Rule {
            kind: RuleKind::Path,
            pattern: pattern.to_string(),
            decision: Decision::Allow,
            created_at: chrono::Utc::now(),
            created_by: "test".to_string(),
        }
    }

    #[test]
    fn empty_store_evaluates_any_path_as_ask() {
        let store = PermissionStore::empty();
        let decision = store.evaluate_path(&PathBuf::from("/tmp/whatever.txt"));
        assert_eq!(decision, Decision::Ask);
    }

    #[test]
    fn default_deny_always_wins_for_ssh_paths() {
        let store = PermissionStore::empty();
        let home = dirs::home_dir().unwrap();
        let ssh_key = home.join(".ssh/id_rsa");
        let decision = store.evaluate_path(&ssh_key);
        assert_eq!(decision, Decision::Deny, "DEFAULT_DENY must match ~/.ssh/id_rsa");
    }

    #[test]
    fn default_deny_matches_dotenv_anywhere() {
        let store = PermissionStore::empty();
        let decision = store.evaluate_path(&PathBuf::from("/Users/me/projects/web/.env"));
        assert_eq!(decision, Decision::Deny);
        let decision2 = store.evaluate_path(&PathBuf::from("/Users/me/projects/web/.env.production"));
        assert_eq!(decision2, Decision::Deny);
    }

    #[test]
    fn insert_rule_rejects_user_allow_on_ssh_directory() {
        // The critical security invariant: even if the user (or
        // LLM via an "allow_always" UI click) tries to allow
        // ~/.ssh/specific_key, the insert must be refused because
        // the probe sample matches DEFAULT_DENY.
        let mut store = PermissionStore::empty();
        let r = Rule {
            kind: RuleKind::Path,
            pattern: "~/.ssh/id_rsa".to_string(),
            decision: Decision::Allow,
            created_at: chrono::Utc::now(),
            created_by: "user-attack".to_string(),
        };
        let err = store.insert_rule(r).unwrap_err();
        match err {
            PermissionError::ProtectedPath { pattern, conflicts_with } => {
                assert_eq!(pattern, "~/.ssh/id_rsa");
                assert!(conflicts_with.contains(".ssh"));
            }
            other => panic!("expected ProtectedPath, got {other:?}"),
        }
        assert!(store.rules.is_empty(), "rejected rule must not be persisted");
    }

    #[test]
    fn insert_rule_rejects_user_allow_on_ssh_wildcard() {
        // The same protection for the explicit wildcard form.
        let mut store = PermissionStore::empty();
        let r = rule_allow_path("~/.ssh/**");
        assert!(matches!(
            store.insert_rule(r),
            Err(PermissionError::ProtectedPath { .. })
        ));
    }

    #[test]
    fn insert_rule_rejects_user_allow_on_dotenv_anywhere() {
        let mut store = PermissionStore::empty();
        let r = rule_allow_path("**/.env");
        assert!(matches!(
            store.insert_rule(r),
            Err(PermissionError::ProtectedPath { .. })
        ));
    }

    #[test]
    fn insert_rule_accepts_legitimate_pattern() {
        let mut store = PermissionStore::empty();
        let r = rule_allow_path("~/Code/**");
        store.insert_rule(r).unwrap();
        assert_eq!(store.rules.len(), 1);
    }

    #[test]
    fn user_allow_rule_works_for_non_protected_path() {
        let mut store = PermissionStore::empty();
        store.insert_rule(rule_allow_path("~/Code/**")).unwrap();
        let home = dirs::home_dir().unwrap();
        let p = home.join("Code/myproj/src/main.rs");
        assert_eq!(store.evaluate_path(&p), Decision::Allow);
    }

    #[test]
    fn user_deny_rule_overrides_ask_default() {
        let mut store = PermissionStore::empty();
        let r = Rule {
            kind: RuleKind::Path,
            pattern: "~/private/**".to_string(),
            decision: Decision::Deny,
            created_at: chrono::Utc::now(),
            created_by: "test".to_string(),
        };
        store.insert_rule(r).unwrap();
        let home = dirs::home_dir().unwrap();
        let p = home.join("private/diary.md");
        assert_eq!(store.evaluate_path(&p), Decision::Deny);
    }

    #[test]
    fn save_and_load_round_trip_preserves_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("permissions.toml");
        let mut store = PermissionStore::empty();
        store.insert_rule(rule_allow_path("~/Code/**")).unwrap();
        store.save(&path).unwrap();

        let loaded = PermissionStore::load(&path).unwrap();
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.rules.len(), 1);
        assert_eq!(loaded.rules[0].pattern, "~/Code/**");
        assert_eq!(loaded.rules[0].decision, Decision::Allow);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_mode_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("permissions.toml");
        let store = PermissionStore::empty();
        store.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn schema_version_mismatch_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("permissions.toml");
        // Hand-craft a TOML with a future schema version.
        std::fs::write(&path, "schema_version = 999\n").unwrap();
        let err = PermissionStore::load(&path).unwrap_err();
        match err {
            PermissionError::SchemaMismatch { file, expected } => {
                assert_eq!(file, 999);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.toml");
        let store = PermissionStore::load(&path).unwrap();
        assert!(store.rules.is_empty());
        assert_eq!(store.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn evaluate_command_matches_glob() {
        let mut store = PermissionStore::empty();
        let r = Rule {
            kind: RuleKind::Command,
            pattern: "git *".to_string(),
            decision: Decision::Allow,
            created_at: chrono::Utc::now(),
            created_by: "test".to_string(),
        };
        store.insert_rule(r).unwrap();
        assert_eq!(store.evaluate_command("git status"), Decision::Allow);
        assert_eq!(store.evaluate_command("rm -rf /"), Decision::Ask);
    }

    #[test]
    fn invalid_glob_pattern_rejected_at_insert() {
        let mut store = PermissionStore::empty();
        let r = rule_allow_path("[unbalanced");
        assert!(matches!(
            store.insert_rule(r),
            Err(PermissionError::InvalidPattern { .. })
        ));
    }

    #[test]
    fn probe_path_replaces_glob_stars_with_x() {
        assert_eq!(probe_path_from_pattern("foo/**/bar"), "foo/x/x/bar");
        assert_eq!(probe_path_from_pattern("foo/*.rs"), "foo/x.rs");
        assert_eq!(probe_path_from_pattern("plain"), "plain");
    }

    #[test]
    fn default_deny_includes_at_least_15_patterns() {
        // Sanity check: if a future commit accidentally truncates
        // the const, this test catches it. The number is generous
        // to allow expansion without breaking.
        assert!(
            DEFAULT_DENY.len() >= 15,
            "DEFAULT_DENY shrunk to {} patterns — likely a regression",
            DEFAULT_DENY.len()
        );
    }
}
