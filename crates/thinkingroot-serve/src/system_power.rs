//! Phase D Wave 1 (2026-05-17) — 10 system-power tool impl
//! functions. Each function is the pure-logic core invoked by a
//! thin MCP handler in `mcp/tools.rs`.
//!
//! All permission and sandbox enforcement happens BEFORE these
//! functions are called — via the `PermissionsGate` in the agent's
//! gate chain (`crates/thinkingroot-serve/src/intelligence/permissions_gate.rs`)
//! and the `thinkingroot-sandbox` crate. The functions here do not
//! second-guess the gate: if a path made it to `file_read` the gate
//! has already approved it.
//!
//! External MCP clients that call these tools directly (without
//! going through the agent loop) are responsible for their own
//! permission UX — `mcp/tools.rs::handle_call` does NOT auto-route
//! through PermissionsGate because external clients (Claude Code,
//! Cursor, Codex) have their own approval surfaces and we don't
//! want to double-prompt the user.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thinkingroot_sandbox::{Sandbox, SandboxBackend, SandboxError, SandboxPolicy};

// ─── Output types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct FileReadOutcome {
    pub path: String,
    pub content: String,
    pub byte_size: usize,
    pub line_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileWriteOutcome {
    pub path: String,
    pub bytes_written: usize,
    pub created: bool,
    pub parent_dirs_created: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EditOp {
    /// 1-based inclusive line range to replace. `start_line == 0`
    /// means "insert before line 1" and `end_line == 0` means
    /// "delete nothing, just insert". `replacement` may contain
    /// newlines or be empty (a pure delete).
    pub start_line: u32,
    pub end_line: u32,
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEditOutcome {
    pub path: String,
    pub edits_applied: usize,
    pub new_byte_size: usize,
    pub new_line_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobOutcome {
    pub pattern: String,
    pub base: String,
    pub matches: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: u32,
    pub line: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrepOutcome {
    pub pattern: String,
    pub base: String,
    pub matches: Vec<GrepMatch>,
    pub files_scanned: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShellOutcome {
    pub command: String,
    pub args: Vec<String>,
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub truncated_stdout: bool,
    pub truncated_stderr: bool,
    pub sandbox_backend: String,
    /// Phase E.1 (2026-05-17) — TokenJuice compaction diagnostic.
    /// Versioned rule id (e.g. `"git.status@v1"`, `"cargo.test@v1"`)
    /// or `"passthrough@v1"` when no rule fired. Surfaces in the
    /// JSON response so the LLM + the user can both see whether
    /// output was compacted and by which rule.
    pub compaction_rule: String,
    /// Total bytes of raw `(stdout + stderr)` before compaction.
    pub original_bytes: usize,
    /// Total bytes of `(stdout + stderr)` after compaction. Equal to
    /// `original_bytes` when compaction was skipped.
    pub compacted_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipboardReadOutcome {
    pub content: String,
    pub byte_size: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipboardWriteOutcome {
    pub bytes_written: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenInDefaultOutcome {
    pub path_or_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrashOutcome {
    pub trashed: usize,
    pub trashed_paths: Vec<String>,
    pub failed: Vec<TrashFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrashFailure {
    pub path: String,
    pub error: String,
}

// ─── Caps + defaults ───────────────────────────────────────────────

/// Maximum file size readable by `file_read` in a single call.
/// 5 MiB is enough for typical source code, markdown, and config
/// files; binary blobs or large datasets blow the LLM's context
/// window and should be paginated by the caller.
pub const FILE_READ_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Maximum glob results returned in a single call. Larger searches
/// fall back to a `truncated: true` flag so the caller knows to
/// narrow the pattern.
pub const GLOB_MAX_RESULTS: usize = 1000;

/// Maximum grep matches returned in a single call.
pub const GREP_MAX_MATCHES: usize = 500;

// ─── Error type ────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SystemPowerError {
    #[error("io error on `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("file too large: {byte_size} bytes exceeds limit {limit}")]
    FileTooLarge { byte_size: u64, limit: usize },
    #[error("invalid utf-8 in `{path}`: file contains binary content; use a binary-aware tool")]
    NotUtf8 { path: String },
    #[error("invalid edit op for `{path}`: {reason}")]
    InvalidEdit { path: String, reason: String },
    #[error("invalid glob pattern `{pattern}`: {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("invalid regex pattern `{pattern}`: {source}")]
    InvalidRegex {
        pattern: String,
        #[source]
        source: regex::Error,
    },
    #[error("clipboard error: {0}")]
    Clipboard(String),
    #[error("opener error: {0}")]
    Opener(String),
    #[error("trash error on `{path}`: {source}")]
    Trash {
        path: String,
        #[source]
        source: trash::Error,
    },
    #[error("sandbox error: {0}")]
    Sandbox(#[from] SandboxError),
}

// ─── 1. file_read ───────────────────────────────────────────────────

pub async fn file_read(path: &Path) -> Result<FileReadOutcome, SystemPowerError> {
    let path_str = path.display().to_string();

    // Probe size first so we never load a 10 GB log into memory.
    let metadata = tokio::fs::metadata(path).await.map_err(|e| SystemPowerError::Io {
        path: path_str.clone(),
        source: e,
    })?;
    let byte_size = metadata.len();
    if byte_size as usize > FILE_READ_MAX_BYTES {
        return Err(SystemPowerError::FileTooLarge {
            byte_size,
            limit: FILE_READ_MAX_BYTES,
        });
    }

    let bytes = tokio::fs::read(path).await.map_err(|e| SystemPowerError::Io {
        path: path_str.clone(),
        source: e,
    })?;
    let content = String::from_utf8(bytes).map_err(|_| SystemPowerError::NotUtf8 {
        path: path_str.clone(),
    })?;
    let line_count = content.lines().count();

    Ok(FileReadOutcome {
        path: path_str,
        byte_size: content.len(),
        line_count,
        truncated: false,
        content,
    })
}

// ─── 2. file_write ──────────────────────────────────────────────────

pub async fn file_write(
    path: &Path,
    content: &str,
    create_dirs: bool,
) -> Result<FileWriteOutcome, SystemPowerError> {
    let path_str = path.display().to_string();
    let existed = tokio::fs::try_exists(path).await.unwrap_or(false);

    let mut parent_dirs_created = false;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && create_dirs
        && !tokio::fs::try_exists(parent).await.unwrap_or(false)
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| SystemPowerError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        parent_dirs_created = true;
    }

    // Atomic write via tempfile + rename. Use a per-PID temp suffix
    // so two concurrent writers can't clobber each other's tmp file.
    let tmp = path.with_file_name(format!(
        "{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    tokio::fs::write(&tmp, content)
        .await
        .map_err(|e| SystemPowerError::Io {
            path: tmp.display().to_string(),
            source: e,
        })?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| {
            // Best-effort cleanup.
            let _ = std::fs::remove_file(&tmp);
            SystemPowerError::Io {
                path: path_str.clone(),
                source: e,
            }
        })?;

    Ok(FileWriteOutcome {
        path: path_str,
        bytes_written: content.len(),
        created: !existed,
        parent_dirs_created,
    })
}

// ─── 3. file_edit ───────────────────────────────────────────────────

pub async fn file_edit(
    path: &Path,
    edits: &[EditOp],
) -> Result<FileEditOutcome, SystemPowerError> {
    let path_str = path.display().to_string();
    let original = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| SystemPowerError::Io {
            path: path_str.clone(),
            source: e,
        })?;
    let lines: Vec<&str> = original.lines().collect();
    let line_count = lines.len() as u32;

    // Validate every edit before mutating.  Constraints:
    //   * start_line >= 1 OR (start_line == 0 && end_line == 0 → pure prepend, treated as insert before line 1)
    //   * end_line >= start_line OR end_line == 0 (pure insert at start_line - 1 boundary)
    //   * end_line <= line_count (can't replace past EOF)
    //   * No two edits overlap by line range.
    let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(edits.len());
    for (i, e) in edits.iter().enumerate() {
        if e.start_line > line_count && e.start_line > 0 {
            return Err(SystemPowerError::InvalidEdit {
                path: path_str,
                reason: format!(
                    "edit {i}: start_line {} > line_count {} (off-EOF)",
                    e.start_line, line_count
                ),
            });
        }
        if e.end_line > line_count {
            return Err(SystemPowerError::InvalidEdit {
                path: path_str,
                reason: format!(
                    "edit {i}: end_line {} > line_count {}",
                    e.end_line, line_count
                ),
            });
        }
        if e.end_line > 0 && e.end_line < e.start_line {
            return Err(SystemPowerError::InvalidEdit {
                path: path_str,
                reason: format!(
                    "edit {i}: end_line {} < start_line {}",
                    e.end_line, e.start_line
                ),
            });
        }
        let normalized = (e.start_line.max(1), if e.end_line == 0 { e.start_line.max(1) - 1 } else { e.end_line });
        for prev in &ranges {
            // Overlap iff a.start <= b.end AND b.start <= a.end (with our
            // edge-case for empty ranges treated as point inserts).
            if normalized.0 <= prev.1 && prev.0 <= normalized.1 {
                return Err(SystemPowerError::InvalidEdit {
                    path: path_str,
                    reason: format!(
                        "edit {i} overlaps previous edit at lines {}-{}",
                        prev.0, prev.1
                    ),
                });
            }
        }
        ranges.push(normalized);
    }

    // Apply edits in reverse-start-line order so earlier line
    // numbers stay valid as we mutate later positions.
    let mut sorted: Vec<&EditOp> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.start_line));

    let mut result_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    for e in &sorted {
        let start = e.start_line.saturating_sub(1) as usize;
        let end_exclusive = if e.end_line == 0 {
            start
        } else {
            e.end_line as usize
        };
        let replacement_lines: Vec<String> = if e.replacement.is_empty() {
            Vec::new()
        } else {
            e.replacement.lines().map(|s| s.to_string()).collect()
        };
        result_lines.splice(start..end_exclusive, replacement_lines);
    }

    let new_content = result_lines.join("\n");
    // Preserve final newline if the original had one.
    let new_content = if original.ends_with('\n') {
        format!("{new_content}\n")
    } else {
        new_content
    };
    let new_line_count = new_content.lines().count();

    // Atomic write via tempfile + rename (same pattern as file_write).
    let tmp = path.with_file_name(format!(
        "{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    tokio::fs::write(&tmp, &new_content)
        .await
        .map_err(|e| SystemPowerError::Io {
            path: tmp.display().to_string(),
            source: e,
        })?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            SystemPowerError::Io {
                path: path_str.clone(),
                source: e,
            }
        })?;

    Ok(FileEditOutcome {
        path: path_str,
        edits_applied: edits.len(),
        new_byte_size: new_content.len(),
        new_line_count,
    })
}

// ─── 4. glob ────────────────────────────────────────────────────────

pub async fn glob_search(
    pattern: &str,
    base: &Path,
) -> Result<GlobOutcome, SystemPowerError> {
    let glob = globset::Glob::new(pattern).map_err(|e| SystemPowerError::InvalidGlob {
        pattern: pattern.to_string(),
        source: e,
    })?;
    let matcher = glob.compile_matcher();
    let base_owned = base.to_path_buf();
    let pattern_owned = pattern.to_string();

    let (matches, truncated) = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let mut truncated = false;
        for entry in ignore::Walk::new(&base_owned).flatten() {
            let path = entry.path();
            if !matcher.is_match(path) {
                continue;
            }
            // Canonicalise to defeat symlink-cover-name attacks: the
            // LLM should NOT see paths under symlink names like
            // `notes -> ~/.ssh` that would let a later file_read
            // bypass the literal `~/.ssh/**` deny rule.
            let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            out.push(resolved.display().to_string());
            if out.len() >= GLOB_MAX_RESULTS {
                truncated = true;
                break;
            }
        }
        (out, truncated)
    })
    .await
    .unwrap_or_else(|_| (Vec::new(), false));

    Ok(GlobOutcome {
        pattern: pattern_owned,
        base: base.display().to_string(),
        matches,
        truncated,
    })
}

// ─── 5. grep ────────────────────────────────────────────────────────

pub async fn grep_search(
    pattern: &str,
    base: &Path,
    regex_mode: bool,
    case_sensitive: bool,
) -> Result<GrepOutcome, SystemPowerError> {
    let regex = if regex_mode {
        let mut builder = regex::RegexBuilder::new(pattern);
        builder.case_insensitive(!case_sensitive);
        builder
            .build()
            .map_err(|e| SystemPowerError::InvalidRegex {
                pattern: pattern.to_string(),
                source: e,
            })?
    } else {
        let escaped = regex::escape(pattern);
        let mut builder = regex::RegexBuilder::new(&escaped);
        builder.case_insensitive(!case_sensitive);
        builder
            .build()
            .map_err(|e| SystemPowerError::InvalidRegex {
                pattern: pattern.to_string(),
                source: e,
            })?
    };
    let base_owned = base.to_path_buf();
    let pattern_owned = pattern.to_string();

    let (matches, files_scanned, truncated) =
        tokio::task::spawn_blocking(move || -> (Vec<GrepMatch>, usize, bool) {
            let mut matches = Vec::new();
            let mut files_scanned = 0usize;
            let mut truncated = false;
            'walk: for entry in ignore::Walk::new(&base_owned).flatten() {
                if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    continue;
                }
                files_scanned += 1;
                let content = match std::fs::read_to_string(entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue, // binary or unreadable
                };
                for (i, line) in content.lines().enumerate() {
                    if regex.is_match(line) {
                        let resolved = std::fs::canonicalize(entry.path())
                            .unwrap_or_else(|_| entry.path().to_path_buf());
                        matches.push(GrepMatch {
                            path: resolved.display().to_string(),
                            line_number: (i + 1) as u32,
                            // Trim very long lines so a 10MB minified
                            // file doesn't blow up the LLM context.
                            line: if line.len() > 400 {
                                format!("{}…", &line[..400])
                            } else {
                                line.to_string()
                            },
                        });
                        if matches.len() >= GREP_MAX_MATCHES {
                            truncated = true;
                            break 'walk;
                        }
                    }
                }
            }
            (matches, files_scanned, truncated)
        })
        .await
        .unwrap_or_else(|_| (Vec::new(), 0, false));

    Ok(GrepOutcome {
        pattern: pattern_owned,
        base: base.display().to_string(),
        matches,
        files_scanned,
        truncated,
    })
}

// ─── 6. shell_exec ──────────────────────────────────────────────────

pub async fn shell_exec(
    sandbox: Arc<dyn Sandbox>,
    command: &str,
    args: &[String],
    policy: &SandboxPolicy,
) -> Result<ShellOutcome, SystemPowerError> {
    let output = sandbox.spawn(command, args, policy).await?;
    let backend = match output.backend {
        SandboxBackend::MacosSandboxExec => "macos_sandbox_exec",
        SandboxBackend::LinuxBubblewrap => "linux_bubblewrap",
        SandboxBackend::LinuxDegraded => "linux_degraded",
    };
    // Phase E.1 (2026-05-17) — TokenJuice compaction. Runs after the
    // sandbox spawn so the raw exit_code is available for
    // `preserve_on_failure` rules; the compacted strings are what
    // the LLM sees in `ShellOutcome.{stdout,stderr}` — the raw
    // bytes are never surfaced beyond this point.
    let compaction = crate::tokenjuice::compact_shell_output(
        command,
        args,
        &output.stdout,
        &output.stderr,
        output.exit_code,
    );
    Ok(ShellOutcome {
        command: command.to_string(),
        args: args.to_vec(),
        exit_code: output.exit_code,
        signal: output.signal,
        stdout: compaction.stdout,
        stderr: compaction.stderr,
        duration_ms: output.duration_ms,
        truncated_stdout: output.truncated_stdout,
        truncated_stderr: output.truncated_stderr,
        sandbox_backend: backend.to_string(),
        compaction_rule: compaction.rule_id,
        original_bytes: compaction.original_bytes,
        compacted_bytes: compaction.compacted_bytes,
    })
}

// ─── 7. clipboard_read ─────────────────────────────────────────────

pub fn clipboard_read() -> Result<ClipboardReadOutcome, SystemPowerError> {
    let mut cb = arboard::Clipboard::new().map_err(|e| SystemPowerError::Clipboard(e.to_string()))?;
    let content = cb
        .get_text()
        .map_err(|e| SystemPowerError::Clipboard(e.to_string()))?;
    let byte_size = content.len();
    Ok(ClipboardReadOutcome { content, byte_size })
}

// ─── 8. clipboard_write ────────────────────────────────────────────

pub fn clipboard_write(content: &str) -> Result<ClipboardWriteOutcome, SystemPowerError> {
    let mut cb = arboard::Clipboard::new().map_err(|e| SystemPowerError::Clipboard(e.to_string()))?;
    cb.set_text(content.to_string())
        .map_err(|e| SystemPowerError::Clipboard(e.to_string()))?;
    Ok(ClipboardWriteOutcome {
        bytes_written: content.len(),
    })
}

// ─── 9. open_in_default ────────────────────────────────────────────

pub fn open_in_default(path_or_url: &str) -> Result<OpenInDefaultOutcome, SystemPowerError> {
    opener::open(path_or_url).map_err(|e| SystemPowerError::Opener(e.to_string()))?;
    Ok(OpenInDefaultOutcome {
        path_or_url: path_or_url.to_string(),
    })
}

// ─── 10. trash ─────────────────────────────────────────────────────

pub fn trash_paths(paths: &[String]) -> TrashOutcome {
    let mut trashed = 0usize;
    let mut trashed_paths = Vec::new();
    let mut failed = Vec::new();
    for p in paths {
        match trash::delete(p) {
            Ok(_) => {
                trashed += 1;
                trashed_paths.push(p.clone());
            }
            Err(e) => failed.push(TrashFailure {
                path: p.clone(),
                error: e.to_string(),
            }),
        }
    }
    TrashOutcome {
        trashed,
        trashed_paths,
        failed,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_read_reads_text_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("hello.md");
        tokio::fs::write(&f, "line one\nline two\n").await.unwrap();
        let out = file_read(&f).await.unwrap();
        assert_eq!(out.content, "line one\nline two\n");
        assert_eq!(out.line_count, 2);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn file_read_rejects_files_over_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("big.bin");
        // 6 MiB > cap.
        let buf = vec![b'x'; 6 * 1024 * 1024];
        tokio::fs::write(&f, &buf).await.unwrap();
        let err = file_read(&f).await.unwrap_err();
        assert!(matches!(err, SystemPowerError::FileTooLarge { .. }));
    }

    #[tokio::test]
    async fn file_read_rejects_non_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("binary.bin");
        // Invalid UTF-8 bytes.
        tokio::fs::write(&f, [0xC3, 0x28]).await.unwrap();
        let err = file_read(&f).await.unwrap_err();
        assert!(matches!(err, SystemPowerError::NotUtf8 { .. }));
    }

    #[tokio::test]
    async fn file_write_creates_then_overwrites_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        let r1 = file_write(&f, "first", false).await.unwrap();
        assert!(r1.created);
        assert_eq!(r1.bytes_written, 5);
        let r2 = file_write(&f, "second", false).await.unwrap();
        assert!(!r2.created);
        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "second");
    }

    #[tokio::test]
    async fn file_write_creates_parent_dirs_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("nested/dirs/a.txt");
        let r = file_write(&f, "hi", true).await.unwrap();
        assert!(r.created);
        assert!(r.parent_dirs_created);
        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "hi");
    }

    #[tokio::test]
    async fn file_edit_replaces_a_line_range() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        tokio::fs::write(&f, "a\nb\nc\nd\n").await.unwrap();
        let edits = vec![EditOp {
            start_line: 2,
            end_line: 3,
            replacement: "B\nC".to_string(),
        }];
        let out = file_edit(&f, &edits).await.unwrap();
        assert_eq!(out.edits_applied, 1);
        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "a\nB\nC\nd\n");
    }

    #[tokio::test]
    async fn file_edit_rejects_overlapping_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        tokio::fs::write(&f, "1\n2\n3\n4\n5\n").await.unwrap();
        let edits = vec![
            EditOp { start_line: 1, end_line: 3, replacement: "X".into() },
            EditOp { start_line: 2, end_line: 4, replacement: "Y".into() },
        ];
        let err = file_edit(&f, &edits).await.unwrap_err();
        assert!(matches!(err, SystemPowerError::InvalidEdit { .. }));
    }

    #[tokio::test]
    async fn file_edit_applies_multiple_non_overlapping_in_reverse_order() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        tokio::fs::write(&f, "1\n2\n3\n4\n5\n").await.unwrap();
        let edits = vec![
            EditOp { start_line: 1, end_line: 1, replacement: "ONE".into() },
            EditOp { start_line: 5, end_line: 5, replacement: "FIVE".into() },
        ];
        file_edit(&f, &edits).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&f).await.unwrap(),
            "ONE\n2\n3\n4\nFIVE\n"
        );
    }

    #[tokio::test]
    async fn glob_finds_files_under_base() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        let b = tmp.path().join("sub/b.rs");
        let c = tmp.path().join("c.txt");
        tokio::fs::create_dir_all(tmp.path().join("sub")).await.unwrap();
        tokio::fs::write(&a, "").await.unwrap();
        tokio::fs::write(&b, "").await.unwrap();
        tokio::fs::write(&c, "").await.unwrap();

        let out = glob_search("**/*.rs", tmp.path()).await.unwrap();
        assert_eq!(out.matches.len(), 2);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn grep_finds_literal_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        tokio::fs::write(&f, "hello world\ngoodbye world\n").await.unwrap();
        let out = grep_search("world", tmp.path(), false, true).await.unwrap();
        assert_eq!(out.matches.len(), 2);
        assert_eq!(out.files_scanned, 1);
    }

    #[tokio::test]
    async fn grep_finds_regex_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        tokio::fs::write(&f, "version 1.2.3\nversion 9.8.7\n").await.unwrap();
        let out = grep_search(r"version \d+\.\d+\.\d+", tmp.path(), true, true)
            .await
            .unwrap();
        assert_eq!(out.matches.len(), 2);
    }

    #[tokio::test]
    async fn grep_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        tokio::fs::write(&f, "Hello\nhello\nHELLO\n").await.unwrap();
        let out = grep_search("hello", tmp.path(), false, false).await.unwrap();
        assert_eq!(out.matches.len(), 3);
    }

    #[tokio::test]
    async fn shell_exec_runs_echo_under_sandbox() {
        let sandbox = Arc::from(thinkingroot_sandbox::default_sandbox());
        let policy = SandboxPolicy::deny_default();
        // Pick a binary present on macOS and Linux.
        #[cfg(target_os = "macos")]
        let cmd = "/bin/echo";
        #[cfg(target_os = "linux")]
        let cmd = "/bin/echo";
        #[cfg(target_os = "windows")]
        let cmd = "echo";

        let result = shell_exec(sandbox, cmd, &["hi".to_string()], &policy).await;
        #[cfg(target_os = "windows")]
        {
            // Windows backend returns UnsupportedPlatform.
            assert!(matches!(result, Err(SystemPowerError::Sandbox(SandboxError::UnsupportedPlatform { .. }))));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let out = result.unwrap();
            assert_eq!(out.exit_code, 0);
            assert!(out.stdout.starts_with("hi"));
        }
    }

    #[test]
    fn trash_returns_failure_for_nonexistent_path() {
        let res = trash_paths(&["/this/path/does/not/exist/9f1e8b6a".to_string()]);
        assert_eq!(res.trashed, 0);
        assert_eq!(res.failed.len(), 1);
    }

    // Note: clipboard tests are intentionally NOT included as
    // automated test gates here — `arboard` requires a display
    // server on Linux (X11 or Wayland) and exhibits flaky
    // initialisation in headless CI. Tests for these are covered
    // by the manual-verification step in the plan and by the
    // permission_gate tests which exercise the path through the
    // gate without touching the OS clipboard.
}
