use std::path::Path;
use std::process::Command;

use thinkingroot_core::ir::{Chunk, ChunkType, DocumentIR};
use thinkingroot_core::types::*;
use thinkingroot_core::{Error, Result};

/// Parse recent git history from a repository into DocumentIRs.
/// Each commit becomes a separate document with the commit message and diff as chunks.
pub fn parse_git_log(repo_path: &Path, max_commits: usize) -> Result<Vec<DocumentIR>> {
    // Check if this is a git repo.
    let status = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(repo_path)
        .output();

    match status {
        Ok(output) if output.status.success() => {}
        _ => return Ok(Vec::new()), // Not a git repo, skip silently.
    }

    let output = Command::new("git")
        .args([
            "log",
            &format!("-{max_commits}"),
            "--format=%H%n%an%n%ai%n%s%n%b%n---END---",
        ])
        .current_dir(repo_path)
        .output()
        .map_err(|e| Error::Parse {
            source_path: repo_path.to_path_buf(),
            message: format!("git log failed: {e}"),
        })?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let log_text = String::from_utf8_lossy(&output.stdout);
    let mut documents = Vec::new();

    for entry in log_text.split("---END---") {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        let lines: Vec<&str> = entry.lines().collect();
        if lines.len() < 4 {
            continue;
        }

        let sha = lines[0];
        let author = lines[1];
        let _date = lines[2];
        let subject = lines[3];
        let body = if lines.len() > 4 {
            lines[4..].join("\n").trim().to_string()
        } else {
            String::new()
        };

        let uri = format!("git://{}", sha);
        let source_id = SourceId::new();
        let mut doc = DocumentIR::new(source_id, uri, SourceType::GitCommit);
        doc.author = Some(author.to_string());
        doc.content_hash = ContentHash::from_bytes(sha.as_bytes());

        // Run diff stat first so we can embed it in the Prose chunk metadata.
        let changed_files = if let Ok(diff_output) = Command::new("git")
            .args(["diff", &format!("{sha}^..{sha}"), "--stat"])
            .current_dir(repo_path)
            .output()
        {
            if diff_output.status.success() {
                let diff_stat = String::from_utf8_lossy(&diff_output.stdout);
                parse_changed_files(&diff_stat)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Commit message as a Prose chunk, carrying author + changed files metadata.
        let message = if body.is_empty() {
            subject.to_string()
        } else {
            format!("{subject}\n\n{body}")
        };
        let mut prose_chunk = Chunk::new(&message, ChunkType::Prose, 1, 1);
        prose_chunk.metadata.commit_author = Some(author.to_string());
        prose_chunk.metadata.changed_files = changed_files;
        doc.add_chunk(prose_chunk);

        documents.push(doc);
    }

    tracing::info!(
        "parsed {} git commits from {}",
        documents.len(),
        repo_path.display()
    );
    Ok(documents)
}

/// Extract file paths from `git diff --stat` output.
/// Each file line looks like: " path/to/file.rs | 12 +++---"
/// Rename lines are expanded into two paths:
///   - Brace form:  "prefix{old => new}suffix" → "prefixoldsuffix", "prefixnewsuffix"
///   - Bare form:   "old.rs => new.rs"          → "old.rs", "new.rs"
///
/// The summary line ("N files changed, ...") has no " | " and is skipped.
fn parse_changed_files(diff_stat: &str) -> Vec<String> {
    let mut result = Vec::new();
    for line in diff_stat.lines() {
        let Some(pipe_pos) = line.find(" |") else {
            continue;
        };
        let path = line[..pipe_pos].trim();
        if path.is_empty() {
            continue;
        }
        // Handle git rename notation: "prefix{old => new}suffix"
        if let (Some(brace_start), Some(brace_end)) = (path.find('{'), path.find('}')) {
            if brace_start < brace_end {
                let prefix = &path[..brace_start];
                let suffix = &path[brace_end + 1..];
                let inner = &path[brace_start + 1..brace_end];
                if let Some((old_part, new_part)) = inner.split_once(" => ") {
                    let old_path = format!("{}{}{}", prefix, old_part.trim(), suffix);
                    let new_path = format!("{}{}{}", prefix, new_part.trim(), suffix);
                    if !old_path.is_empty() {
                        result.push(old_path);
                    }
                    if !new_path.is_empty() {
                        result.push(new_path);
                    }
                    continue;
                }
            }
        }
        // Handle bare-form rename: "old.rs => new.rs" or "src/a.rs => dst/b.rs"
        if let Some((old_part, new_part)) = path.split_once(" => ") {
            let old_path = old_part.trim().to_string();
            let new_path = new_part.trim().to_string();
            if !old_path.is_empty() {
                result.push(old_path);
            }
            if !new_path.is_empty() {
                result.push(new_path);
            }
            continue;
        }
        result.push(path.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_git_dir_returns_empty() {
        let tmp = std::env::temp_dir().join("thinkingroot-not-git");
        std::fs::create_dir_all(&tmp).ok();
        let docs = parse_git_log(&tmp, 10).unwrap();
        assert!(docs.is_empty());
    }

    #[test]
    fn parse_changed_files_extracts_paths() {
        let stat = " src/main.rs | 12 +++---\n crates/core/src/lib.rs |  4 ++\n 2 files changed, 16 insertions(+), 2 deletions(-)\n";
        let files = parse_changed_files(stat);
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(files.contains(&"crates/core/src/lib.rs".to_string()));
    }

    #[test]
    fn parse_changed_files_ignores_summary_line() {
        // The last line "N files changed..." has no pipe → must not be included
        let stat = "foo.rs | 1 +\n1 file changed, 1 insertion(+)\n";
        let files = parse_changed_files(stat);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "foo.rs");
    }

    #[test]
    fn parse_changed_files_empty_input() {
        assert!(parse_changed_files("").is_empty());
        assert!(parse_changed_files("\n\n").is_empty());
    }

    #[test]
    fn parse_changed_files_handles_rename() {
        let stat =
            " src/{old.rs => new.rs} | 3 ++-\n 1 file changed, 3 insertions(+), 0 deletions(-)\n";
        let files = parse_changed_files(stat);
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"src/old.rs".to_string()));
        assert!(files.contains(&"src/new.rs".to_string()));
    }

    #[test]
    fn parse_changed_files_handles_bare_rename() {
        // Pure rename — no common prefix, no braces
        let stat = " old.rs => new.rs | 0\n 1 file changed, 0 insertions(+), 0 deletions(-)\n";
        let files = parse_changed_files(stat);
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"old.rs".to_string()));
        assert!(files.contains(&"new.rs".to_string()));
    }

    #[test]
    fn parse_changed_files_handles_cross_dir_rename() {
        // Cross-directory rename — no braces
        let stat =
            " src/beta.rs => dst/gamma.rs | 0\n 1 file changed, 0 insertions(+), 0 deletions(-)\n";
        let files = parse_changed_files(stat);
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"src/beta.rs".to_string()));
        assert!(files.contains(&"dst/gamma.rs".to_string()));
    }
}
