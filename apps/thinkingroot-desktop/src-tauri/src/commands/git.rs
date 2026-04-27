//! Git branch listing for the Brain sidebar — drives the "main +
//! all branches" GitHub-style list the user expects to see for the
//! active workspace.
//!
//! Shells out to the system `git` binary rather than depending on
//! `git2` because:
//!
//! - The user's git config (custom credentials, signing, includeIf
//!   directives) is honoured automatically — `git2` would need every
//!   field replicated.
//! - No new C dependency to compile against on every platform.
//! - Read-only operation; no concerns about the subprocess writing
//!   to the working tree.
//!
//! Output is parsed line-by-line; each line of `git branch -a` is
//! either `* <name>` (current branch), `  <name>`, or
//! `  remotes/<remote>/<name>`.

use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub kind: BranchKind,
    /// `true` for the currently checked-out branch.
    pub current: bool,
    /// For remote-tracking branches, the remote name (`origin`,
    /// `upstream`, …). `None` for local branches.
    pub remote: Option<String>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BranchKind {
    Local,
    Remote,
}

#[derive(Debug, Deserialize)]
pub struct GitBranchesArgs {
    pub path: String,
}

#[tauri::command]
pub fn git_branches(args: GitBranchesArgs) -> Result<Vec<BranchInfo>, String> {
    let path = PathBuf::from(&args.path);
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["branch", "-a", "--no-color"])
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !output.status.success() {
        // git prints "fatal: not a git repository" to stderr in that
        // case — pass it through verbatim so the UI shows the cause
        // rather than a generic exit-code error.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git exit {}", output.status)
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_branches(&stdout))
}

fn parse_branches(text: &str) -> Vec<BranchInfo> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        // A detached-HEAD line looks like "* (HEAD detached at abc123)" —
        // skip it; it's not a branch the user can switch to from a list.
        if line.contains("(HEAD detached") {
            continue;
        }
        let current = line.starts_with('*');
        // Skip the leading "* " or "  " marker (two chars) before the name.
        let rest = if line.len() >= 2 { &line[2..] } else { line };
        // Remote-tracking entries are sometimes printed as
        // `remotes/origin/HEAD -> origin/main` — keep the canonical
        // pointer name (`origin/HEAD`), drop the `-> origin/main`
        // arrow so we don't show two entries for the same branch.
        let name_part = rest.split(" -> ").next().unwrap_or(rest);
        if let Some(stripped) = name_part.strip_prefix("remotes/") {
            // Format: "<remote>/<branch>"
            if let Some((remote, branch)) = stripped.split_once('/') {
                out.push(BranchInfo {
                    name: branch.to_string(),
                    kind: BranchKind::Remote,
                    current: false,
                    remote: Some(remote.to_string()),
                });
            }
        } else {
            out.push(BranchInfo {
                name: name_part.to_string(),
                kind: BranchKind::Local,
                current,
                remote: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_marks_starred_branch_current() {
        let out = parse_branches("* main\n  feature/x\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "main");
        assert!(out[0].current);
        assert_eq!(out[1].name, "feature/x");
        assert!(!out[1].current);
    }

    #[test]
    fn parse_handles_remote_branches() {
        let out = parse_branches(
            "* main\n  remotes/origin/main\n  remotes/origin/feature/x\n",
        );
        let remote_main = out.iter().find(|b| b.kind == BranchKind::Remote && b.name == "main");
        assert!(remote_main.is_some());
        assert_eq!(
            remote_main.unwrap().remote.as_deref(),
            Some("origin"),
            "remote name parsed"
        );
    }

    #[test]
    fn parse_skips_head_arrow_line() {
        let out = parse_branches(
            "* main\n  remotes/origin/HEAD -> origin/main\n  remotes/origin/main\n",
        );
        // origin/HEAD line gets parsed into an entry named "HEAD" pointing
        // at remote `origin` — that's still a real entry; the canonicaliser
        // just stripped the arrow so we don't accidentally produce two.
        let head = out.iter().find(|b| b.name == "HEAD" && b.remote.as_deref() == Some("origin"));
        assert!(head.is_some());
    }

    #[test]
    fn parse_skips_detached_head() {
        let out = parse_branches("* (HEAD detached at abc1234)\n  main\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "main");
    }
}
