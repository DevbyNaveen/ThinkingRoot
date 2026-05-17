//! Clean-room reimplementation. Inspired by openhuman/tokenjuice/rules/builtin.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.1 (2026-05-17) — 10 hardcoded compaction rules.
//!
//! Rule shape is intentionally rigid: argv0 / argv-includes /
//! command-includes for classification, then an ordered
//! `&[Transform]` for the transform pipeline. No regex, no JSON,
//! no runtime config. Adding a rule is a single `&'static Rule`
//! literal at the bottom of this file.
//!
//! Versioning: every rule id carries `@vN`. Bumping a rule's
//! transforms bumps `@vN` so downstream surfaces logging
//! `compaction_rule = "git.status@v1"` keep their semantics pinned
//! against a frozen behaviour.

/// One transform step. The compactor applies these in order.
#[derive(Debug, Clone, Copy)]
pub enum Transform {
    /// Strip ANSI CSI escape sequences (colour, cursor moves). Cheap
    /// regex-free implementation in `reduce::strip_ansi`.
    StripAnsi,
    /// Collapse runs of consecutive identical lines into a single
    /// line. Preserves order; only adjacent duplicates collapse.
    DedupeAdjacent,
    /// Drop trailing blank lines and leading blank lines.
    TrimBlankEdges,
    /// Drop lines matching any of these substrings.
    SkipContaining(&'static [&'static str]),
    /// Keep ONLY lines matching any of these substrings.
    KeepContaining(&'static [&'static str]),
    /// If the line-count exceeds `total`, keep the first `head`
    /// lines + an elision marker `[... N more lines (M bytes) ...]`
    /// + the last `tail` lines. When `preserve_on_failure` triggers,
    /// the head + tail are each doubled.
    HeadTail {
        total: usize,
        head: usize,
        tail: usize,
    },
}

/// One classification + transform rule.
#[derive(Debug)]
pub struct Rule {
    /// Versioned id: `<family>.<name>@v<N>`, e.g. `"git.status@v1"`.
    pub id: &'static str,
    /// Match the argv[0] exactly. `None` defers to `argv0_any_of`.
    pub argv0: Option<&'static str>,
    /// Alternative argv[0] candidates (case-insensitive). Match if
    /// the command equals any entry. Used by package-manager-family
    /// rules (npm/pnpm/yarn share the install rule). Empty slice +
    /// `argv0: None` means "match any command" (generic rules).
    pub argv0_any_of: &'static [&'static str],
    /// Match when ALL listed tokens appear somewhere in args.
    /// Empty slice matches any args.
    pub argv_includes: &'static [&'static str],
    /// Higher priority wins ties.
    pub priority: i32,
    /// Transforms to apply in order.
    pub transforms: &'static [Transform],
    /// When `exit_code != 0`, double head + tail windows so the
    /// model sees more error context.
    pub preserve_on_failure: bool,
}

// ─── 10 rules ───────────────────────────────────────────────────────

const RULE_GIT_STATUS: Rule = Rule {
    id: "git.status@v1",
    argv0: Some("git"),
    argv0_any_of: &[],
    argv_includes: &["status"],
    priority: 100,
    transforms: &[
        Transform::StripAnsi,
        Transform::DedupeAdjacent,
        Transform::TrimBlankEdges,
        Transform::HeadTail {
            total: 80,
            head: 50,
            tail: 20,
        },
    ],
    preserve_on_failure: false,
};

const RULE_GIT_DIFF: Rule = Rule {
    id: "git.diff@v1",
    argv0: Some("git"),
    argv0_any_of: &[],
    // `git diff`, `git show`, `git log -p` all dump big diffs.
    // Matching on just `diff` would mis-match `git log --no-diff`;
    // we match the explicit subcommand only.
    argv_includes: &["diff"],
    priority: 100,
    transforms: &[
        Transform::StripAnsi,
        Transform::HeadTail {
            total: 250,
            head: 180,
            tail: 50,
        },
    ],
    preserve_on_failure: true,
};

const RULE_GIT_LOG: Rule = Rule {
    id: "git.log@v1",
    argv0: Some("git"),
    argv0_any_of: &[],
    argv_includes: &["log"],
    priority: 100,
    transforms: &[
        Transform::StripAnsi,
        Transform::HeadTail {
            total: 80,
            head: 60,
            tail: 10,
        },
    ],
    preserve_on_failure: false,
};

const RULE_NPM_INSTALL: Rule = Rule {
    id: "npm.install@v1",
    // Match npm / pnpm / yarn / bun via `argv0_any_of`. Pre-fix this
    // rule had `argv0: None + argv_includes: &["install"]`, which
    // also matched `cargo install`, `pip install`, `apt install`,
    // `brew install`, etc. — applying npm's spinner-strip transform
    // to cargo install's pretty-printed output truncated useful
    // diagnostic context. The OR-set keeps the family of package
    // managers that share npm's output pattern while disqualifying
    // unrelated `install` subcommands.
    argv0: None,
    argv0_any_of: &["npm", "pnpm", "yarn", "bun"],
    argv_includes: &["install"],
    priority: 80,
    transforms: &[
        Transform::StripAnsi,
        Transform::SkipContaining(&[
            "⠋",
            "⠙",
            "⠹",
            "⠸",
            "⠼",
            "⠴",
            "⠦",
            "⠧",
            "⠇",
            "⠏",
            "↑",
            "|",
            "/",
            "\\",
            "-",
            "[#",
            "Progress:",
            "Resolving:",
            "Fetching:",
            "Linking:",
        ]),
        Transform::DedupeAdjacent,
        Transform::HeadTail {
            total: 60,
            head: 30,
            tail: 20,
        },
    ],
    preserve_on_failure: true,
};

const RULE_CARGO_TEST: Rule = Rule {
    id: "cargo.test@v1",
    argv0: Some("cargo"),
    argv0_any_of: &[],
    argv_includes: &["test"],
    priority: 100,
    transforms: &[
        Transform::StripAnsi,
        // Keep only meaningful lines: test markers, results, failures.
        Transform::KeepContaining(&[
            "running ",
            "test ",
            "test result:",
            "FAILED",
            "failures:",
            "error[",
            "warning:",
            "thread '",
            "  left:",
            "  right:",
            "panicked at",
            "Compiling ",
            "Finished",
        ]),
        Transform::DedupeAdjacent,
        Transform::HeadTail {
            total: 200,
            head: 100,
            tail: 80,
        },
    ],
    preserve_on_failure: true,
};

const RULE_CARGO_BUILD: Rule = Rule {
    id: "cargo.build@v1",
    argv0: Some("cargo"),
    argv0_any_of: &[],
    argv_includes: &["build"],
    priority: 100,
    transforms: &[
        Transform::StripAnsi,
        Transform::SkipContaining(&["Compiling ", "Documenting ", "Checking ", "Updating "]),
        Transform::DedupeAdjacent,
        Transform::HeadTail {
            total: 120,
            head: 80,
            tail: 30,
        },
    ],
    preserve_on_failure: true,
};

const RULE_DOCKER_PS: Rule = Rule {
    id: "docker.ps@v1",
    argv0: Some("docker"),
    argv0_any_of: &[],
    argv_includes: &["ps"],
    priority: 100,
    transforms: &[
        Transform::StripAnsi,
        Transform::HeadTail {
            total: 40,
            head: 30,
            tail: 5,
        },
    ],
    preserve_on_failure: false,
};

const RULE_DOCKER_LOGS: Rule = Rule {
    id: "docker.logs@v1",
    argv0: Some("docker"),
    argv0_any_of: &[],
    argv_includes: &["logs"],
    priority: 100,
    transforms: &[
        Transform::StripAnsi,
        Transform::HeadTail {
            total: 250,
            head: 100,
            tail: 100,
        },
    ],
    preserve_on_failure: true,
};

const RULE_GENERIC_ANSI: Rule = Rule {
    id: "generic.ansi@v1",
    argv0: None,
    argv0_any_of: &[],
    argv_includes: &[],
    priority: 10,
    transforms: &[Transform::StripAnsi],
    preserve_on_failure: false,
};

const RULE_GENERIC_FALLBACK: Rule = Rule {
    id: "generic.fallback@v1",
    argv0: None,
    argv0_any_of: &[],
    argv_includes: &[],
    priority: 1,
    transforms: &[
        Transform::StripAnsi,
        Transform::HeadTail {
            total: 350,
            head: 250,
            tail: 80,
        },
    ],
    preserve_on_failure: false,
};

/// The full catalogue. `classify::classify` scans this in any order
/// (the comparison is total via priority + id), so list order
/// doesn't affect outcomes.
pub const ALL_RULES: &[&Rule] = &[
    &RULE_GIT_STATUS,
    &RULE_GIT_DIFF,
    &RULE_GIT_LOG,
    &RULE_NPM_INSTALL,
    &RULE_CARGO_TEST,
    &RULE_CARGO_BUILD,
    &RULE_DOCKER_PS,
    &RULE_DOCKER_LOGS,
    &RULE_GENERIC_ANSI,
    &RULE_GENERIC_FALLBACK,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_exactly_ten_rules() {
        assert_eq!(
            ALL_RULES.len(),
            10,
            "v1 ships exactly 10 rules; adding more is a separate ship"
        );
    }

    #[test]
    fn all_rule_ids_are_unique() {
        let mut ids: Vec<&str> = ALL_RULES.iter().map(|r| r.id).collect();
        ids.sort();
        let original_len = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            original_len,
            "rule ids must be unique — id collision would break the diagnostic field"
        );
    }

    #[test]
    fn every_rule_id_carries_version_suffix() {
        for r in ALL_RULES {
            assert!(
                r.id.contains("@v"),
                "rule {} missing @vN version suffix",
                r.id
            );
        }
    }

    #[test]
    fn priorities_separate_specific_from_generic() {
        // Generic rules MUST score below specific ones so an exact
        // git/npm/cargo match always wins over generic.ansi.
        let specific_min = ALL_RULES
            .iter()
            .filter(|r| r.argv0.is_some())
            .map(|r| r.priority)
            .min()
            .unwrap();
        let generic_max = ALL_RULES
            .iter()
            .filter(|r| r.argv0.is_none())
            .map(|r| r.priority)
            .max()
            .unwrap();
        assert!(
            generic_max < specific_min,
            "every specific rule must outrank every generic rule"
        );
    }
}
