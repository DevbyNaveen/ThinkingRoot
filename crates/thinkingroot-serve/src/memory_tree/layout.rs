//! Clean-room reimplementation. Inspired by openhuman/tree_summarizer/
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.3 (2026-05-17) — directory layout conventions for the
//! markdown-tree export.
//!
//! Layout (deterministic; content-addressed, not time-keyed):
//!
//! ```
//! <output_dir>/
//! ├── index.md                          workspace summary + top-level
//! │                                     links
//! ├── sources/
//! │   └── <source_slug>/
//! │       ├── index.md                  source metadata + counts
//! │       ├── claims/
//! │       │   └── <claim_id_slug>.md
//! │       └── witnesses/
//! │           └── <witness_id_short>.md
//! └── topics/
//!     └── <branch_slug>/
//!         └── index.md
//! ```
//!
//! ## Filename safety
//!
//! Every path component routes through [`sanitize_for_fs`] —
//! replaces any byte outside `[A-Za-z0-9._-]` with `_`. Defends
//! against:
//!   - Path traversal (`../`, `..\\`)
//!   - Reserved Windows names (CON, PRN, NUL — by replacing the
//!     dot/colon they typically pair with, the components become
//!     ASCII-safe but not necessarily reserved-name-safe; callers
//!     on Windows should additionally prefix non-empty names).
//!   - Bytes outside ASCII (we accept lossy replacement for non-
//!     ASCII to keep filenames editor-friendly).

use std::path::{Path, PathBuf};

/// Replace every byte that isn't `[A-Za-z0-9._-]` with `_`.
/// Truncate to a sane max length (255 bytes is the EXT4/macOS
/// filename limit; 200 leaves headroom for `.md` suffixes and
/// shortcut indexes).
pub fn sanitize_for_fs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let safe = match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '_',
        };
        out.push(safe);
        if out.len() >= 200 {
            break;
        }
    }
    // Defence-in-depth: never let a component start with `.` to
    // avoid clashing with `.git`, `.thinkingroot`, etc., and never
    // emit empty (would silently collapse to the parent dir).
    if out.is_empty() {
        out.push_str("unnamed");
    } else if out.starts_with('.') {
        out = format!("_{out}");
    }
    out
}

/// Short hex-style id slug — used for witness filenames to keep
/// them legible. Takes the first 12 chars of a `WitnessId` hex
/// rendering. Collisions in 12-char prefix are astronomically
/// unlikely at v1 workspace sizes (<10⁶ witnesses).
pub fn short_id(id_hex: &str) -> String {
    let take = id_hex.len().min(12);
    sanitize_for_fs(&id_hex[..take])
}

/// `<output_dir>/index.md`
pub fn workspace_index(root: &Path) -> PathBuf {
    root.join("index.md")
}

/// `<output_dir>/sources/<slug>/index.md`
pub fn source_index(root: &Path, source_slug: &str) -> PathBuf {
    root.join("sources")
        .join(sanitize_for_fs(source_slug))
        .join("index.md")
}

/// `<output_dir>/sources/<source_slug>/claims/<claim_slug>.md`
pub fn claim_file(root: &Path, source_slug: &str, claim_slug: &str) -> PathBuf {
    root.join("sources")
        .join(sanitize_for_fs(source_slug))
        .join("claims")
        .join(format!("{}.md", sanitize_for_fs(claim_slug)))
}

/// `<output_dir>/sources/<source_slug>/witnesses/<witness_short>.md`
pub fn witness_file(root: &Path, source_slug: &str, witness_id_hex: &str) -> PathBuf {
    root.join("sources")
        .join(sanitize_for_fs(source_slug))
        .join("witnesses")
        .join(format!("{}.md", short_id(witness_id_hex)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_path_separators_and_special_chars() {
        // `../etc/passwd` first sanitises slashes → `.._etc_passwd`,
        // then the dotfile-guard prefixes the leading `.` with `_`
        // → `_.._etc_passwd`. The double guard is intentional — a
        // path-traversal payload should never produce a name an
        // editor might interpret specially.
        assert_eq!(sanitize_for_fs("../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_for_fs("file with spaces.md"), "file_with_spaces.md");
        assert_eq!(sanitize_for_fs("src/lib.rs"), "src_lib.rs");
    }

    #[test]
    fn sanitize_truncates_long_strings() {
        let s = "a".repeat(500);
        let out = sanitize_for_fs(&s);
        assert!(out.len() <= 200);
    }

    #[test]
    fn sanitize_avoids_dotfile_collision() {
        assert_eq!(sanitize_for_fs(".git"), "_.git");
        assert_eq!(sanitize_for_fs(".thinkingroot"), "_.thinkingroot");
    }

    #[test]
    fn sanitize_handles_empty_input() {
        assert_eq!(sanitize_for_fs(""), "unnamed");
    }

    #[test]
    fn short_id_produces_consistent_prefixes() {
        let hex = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(short_id(hex), "abcdef012345");
        // Idempotent
        assert_eq!(short_id(&short_id(hex)), "abcdef012345");
    }

    #[test]
    fn workspace_index_path_layout() {
        let root = PathBuf::from("/tmp/out");
        let p = workspace_index(&root);
        assert_eq!(p, PathBuf::from("/tmp/out/index.md"));
    }

    #[test]
    fn witness_file_layout_is_deterministic() {
        let root = PathBuf::from("/tmp/out");
        let a = witness_file(&root, "src/lib.rs", "abc123def456");
        let b = witness_file(&root, "src/lib.rs", "abc123def456");
        assert_eq!(a, b);
        assert!(a.to_str().unwrap().ends_with("src_lib.rs/witnesses/abc123def456.md"));
    }
}
