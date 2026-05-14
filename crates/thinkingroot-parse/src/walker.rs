use std::path::{Path, PathBuf};

use thinkingroot_core::config::ParserConfig;
use thinkingroot_core::{Error, Result};

/// Walk a directory tree and return all parseable files, respecting
/// `.gitignore` rules, the user's exclude patterns, and `.rootignore`
/// for ThinkingRoot-specific exclusions (e.g. files the user wants out
/// of the compiled cognition but doesn't want to add to `.gitignore`).
///
/// Precedence (highest to lowest): `.rootignore` → `.gitignore` →
/// `.git/info/exclude` → global git excludes → default `hidden(true)`.
/// Matches the `ignore` crate's standard precedence semantics for
/// custom ignore files (same model as `.dockerignore` / `.npmignore`).
pub fn walk(root: &Path, config: &ParserConfig) -> Result<Vec<PathBuf>> {
    let mut builder = ignore::WalkBuilder::new(root);

    builder
        .hidden(true) // skip hidden files by default
        .git_ignore(config.respect_gitignore)
        .git_global(config.respect_gitignore)
        .git_exclude(config.respect_gitignore)
        .add_custom_ignore_filename(".rootignore");

    // Add exclude patterns as overrides.
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pattern in &config.exclude_patterns {
        // Negate patterns: "!pattern" means exclude.
        overrides
            .add(&format!("!{pattern}"))
            .map_err(|e| Error::Config(format!("invalid exclude pattern '{pattern}': {e}")))?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| Error::Config(format!("failed to build overrides: {e}")))?;
    builder.overrides(overrides);

    let mut files = Vec::new();

    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // Permission errors on system directories (e.g. macOS ~/Library) are
                // expected when compiling a broad path like $HOME. Skip and continue.
                if let Some(io_err) = e.io_error()
                    && io_err.kind() == std::io::ErrorKind::PermissionDenied
                {
                    tracing::warn!("skipping inaccessible path: {e}");
                    continue;
                }
                tracing::debug!("walk error (skipping): {e}");
                continue;
            }
        };

        let path = entry.path();

        // Skip directories.
        if !path.is_file() {
            continue;
        }

        // Check file size limit.  metadata() can fail on macOS APFS clones,
        // some FUSE mounts, and racy unlinks.  Pre-fix the failure branch
        // silently *passed* the file through, which then got read in full
        // regardless of `max_file_size`.  Treat any metadata failure as a
        // skip with a warning so a 2 GB log file behind a flaky FUSE mount
        // can never blow the parser's budget.
        match path.metadata() {
            Ok(meta) if meta.len() > config.max_file_size => {
                tracing::debug!(
                    "skipping large file: {} ({} bytes)",
                    path.display(),
                    meta.len()
                );
                continue;
            }
            Ok(_) => {} // size OK
            Err(e) => {
                tracing::warn!("skipping {} (metadata failed: {e})", path.display(),);
                continue;
            }
        }

        // If include_extensions is set, filter by extension.
        if !config.include_extensions.is_empty() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !config
                .include_extensions
                .iter()
                .any(|e| e.to_lowercase() == ext)
            {
                continue;
            }
        }

        files.push(path.to_path_buf());
    }

    files.sort();
    tracing::info!("found {} files in {}", files.len(), root.display());
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn default_config() -> ParserConfig {
        ParserConfig {
            respect_gitignore: true,
            exclude_patterns: vec![],
            include_extensions: vec![],
            max_file_size: 10 * 1024 * 1024,
        }
    }

    #[test]
    fn rootignore_excludes_files_when_no_gitignore_present() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        write(root, "keep.txt", "kept");
        write(root, "secret.txt", "should be skipped");
        write(root, ".rootignore", "secret.txt\n");

        let files = walk(root, &default_config()).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"keep.txt".to_string()));
        assert!(
            !names.contains(&"secret.txt".to_string()),
            ".rootignore should exclude secret.txt — got {names:?}"
        );
    }

    #[test]
    fn rootignore_takes_precedence_over_gitignore_allow() {
        // .gitignore would otherwise NOT exclude `secret.env` (it isn't
        // listed there). The .rootignore is what does the work — this
        // pins the precedence: ThinkingRoot's user-cognition exclusion
        // is independent from git tracking.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        write(root, "keep.md", "kept");
        write(root, "secret.env", "DB_PASSWORD=hunter2");
        // an empty .gitignore — nothing excluded by git
        write(root, ".gitignore", "");
        write(root, ".rootignore", "*.env\n");

        let files = walk(root, &default_config()).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"keep.md".to_string()));
        assert!(
            !names.contains(&"secret.env".to_string()),
            ".rootignore should exclude secret.env even when .gitignore allows it — got {names:?}"
        );
    }

    #[test]
    fn rootignore_with_directory_pattern_excludes_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        write(root, "top.md", "kept");
        let personal = root.join("personal");
        fs::create_dir_all(&personal).unwrap();
        write(&personal, "diary.md", "private");
        write(&personal, "tax.md", "private");
        write(root, ".rootignore", "personal/\n");

        let files = walk(root, &default_config()).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"top.md".to_string()));
        assert!(
            !names.contains(&"diary.md".to_string())
                && !names.contains(&"tax.md".to_string()),
            "`personal/` directory pattern should exclude entire subtree — got {names:?}"
        );
    }

    #[test]
    fn walk_without_rootignore_still_works() {
        // Regression guard: the new custom-ignore wiring must not change
        // behaviour when no `.rootignore` is present.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "");
        write(root, "b.md", "");

        let files = walk(root, &default_config()).unwrap();
        assert_eq!(files.len(), 2);
    }
}
