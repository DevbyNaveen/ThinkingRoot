//! Clean-room reimplementation. Inspired by openhuman/tokenjuice/classify.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.1 (2026-05-17) — pick the best rule for a `(command, args)` pair.
//!
//! Scoring is deterministic + total:
//!
//! 1. `argv0` exact match → +100
//! 2. each token in `argv_includes` found in args → +40
//! 3. (currently unused but reserved) command-string includes → +25
//! 4. rule `priority` → ×1
//!
//! Then ties break alphabetically by rule id (deterministic across
//! processes). The npm-install rule has `argv0=None` so it scores
//! lower than git/cargo/docker on their respective subcommands —
//! exactly what we want (an exact argv0 match should outrank a
//! cross-package-manager subcommand match).

use super::rules::{ALL_RULES, Rule};

pub(super) fn classify<'a>(
    command: &str,
    args: &[String],
    _stdout: &str,
    _stderr: &str,
) -> Option<&'a Rule> {
    let mut best: Option<(&Rule, i32)> = None;
    for rule in ALL_RULES.iter().copied() {
        let mut score: i32 = 0;
        // argv0 match
        if let Some(want) = rule.argv0 {
            if want.eq_ignore_ascii_case(command) {
                score += 100;
            } else {
                // A rule that pins argv0 but doesn't match it is
                // disqualified outright — score stays 0.
                continue;
            }
        }
        // argv-includes: every token must appear somewhere in args
        // for the rule to qualify.
        if !rule.argv_includes.is_empty() {
            let mut all_match = true;
            for want in rule.argv_includes {
                if !args.iter().any(|a| a == want) {
                    all_match = false;
                    break;
                }
            }
            if !all_match {
                continue;
            }
            score += 40 * rule.argv_includes.len() as i32;
        }
        score += rule.priority;
        // Choose by (score desc, id asc).
        let take = match best {
            None => true,
            Some((cur_rule, cur_score)) => {
                score > cur_score || (score == cur_score && rule.id < cur_rule.id)
            }
        };
        if take {
            best = Some((rule, score));
        }
    }
    best.map(|(r, _)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn git_status_classifies_as_git_status_rule() {
        let r = classify("git", &args(&["status"]), "", "").expect("must match a rule");
        assert_eq!(r.id, "git.status@v1");
    }

    #[test]
    fn git_diff_classifies_as_git_diff_rule() {
        let r = classify("git", &args(&["diff", "HEAD~1"]), "", "").unwrap();
        assert_eq!(r.id, "git.diff@v1");
    }

    #[test]
    fn cargo_test_classifies_as_cargo_test_rule() {
        let r = classify("cargo", &args(&["test", "--lib"]), "", "").unwrap();
        assert_eq!(r.id, "cargo.test@v1");
    }

    #[test]
    fn cargo_build_classifies_as_cargo_build_rule() {
        let r = classify("cargo", &args(&["build", "--release"]), "", "").unwrap();
        assert_eq!(r.id, "cargo.build@v1");
    }

    #[test]
    fn npm_install_matches_generic_install_rule() {
        // No argv0 lock — npm/pnpm/yarn share this rule.
        let r = classify("pnpm", &args(&["install"]), "", "").unwrap();
        assert_eq!(r.id, "npm.install@v1");
    }

    #[test]
    fn unknown_command_falls_back_to_generic_ansi_or_fallback() {
        let r = classify("totally-unknown", &[], "", "").unwrap();
        // Generic-ANSI outranks generic-fallback (10 > 1).
        assert_eq!(r.id, "generic.ansi@v1");
    }

    #[test]
    fn git_log_outranks_generic_rules() {
        let r = classify("git", &args(&["log"]), "", "").unwrap();
        assert_eq!(r.id, "git.log@v1");
    }

    #[test]
    fn docker_subcommand_picks_specific_rule() {
        assert_eq!(classify("docker", &args(&["ps"]), "", "").unwrap().id, "docker.ps@v1");
        assert_eq!(
            classify("docker", &args(&["logs", "container-name"]), "", "")
                .unwrap()
                .id,
            "docker.logs@v1"
        );
    }

    #[test]
    fn case_insensitive_argv0_match() {
        // Some shells alias to upper-case; the classifier should
        // still match.
        let r = classify("GIT", &args(&["status"]), "", "").unwrap();
        assert_eq!(r.id, "git.status@v1");
    }

    #[test]
    fn argv0_locked_rule_is_disqualified_on_wrong_command() {
        // cargo.test requires argv0 == "cargo"; passing "npm" with
        // "test" must NOT match cargo.test.
        let r = classify("npm", &args(&["test"]), "", "").unwrap();
        assert_ne!(r.id, "cargo.test@v1");
    }
}
