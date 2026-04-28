// crates/thinkingroot-serve/src/intelligence/skills.rs
//
// Skills — markdown files with frontmatter that teach the agent how
// to do specific tasks well in this workspace.
//
// File format (`.thinkingroot/skills/<slug>.md`):
//
// ```
// ---
// name: refactor-rust
// description: Use when refactoring Rust code in this codebase
// ---
//
// # Refactoring Rust code in this workspace
//
// Step 1: read the relevant module's CLAUDE.md.
// Step 2: …
// ```
//
// The frontmatter MUST carry `name` and `description`. Body is free
// markdown. The agent is told about the skill catalogue via the
// `list_skills` and `use_skill` tools (registered in
// `builtin_tools.rs`); when the model picks `use_skill { name }` the
// handler returns the skill body for the model to follow.
//
// We hand-roll a 2-key frontmatter parser instead of pulling in a YAML
// dep — every skill we ship has exactly `name` and `description`, the
// format is stable, and a 30-line parser is honest in a way a 5 k-LoC
// YAML library is not.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// One loaded skill — frontmatter fields plus the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source_path: PathBuf,
}

/// Result of parsing a single `.md` file. Sites that want to load a
/// directory of skills go through [`SkillRegistry::load_from_dir`].
#[derive(Debug, thiserror::Error)]
pub enum SkillLoadError {
    #[error("skill file {path}: I/O: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("skill file {path}: missing frontmatter")]
    MissingFrontmatter { path: PathBuf },
    #[error("skill file {path}: missing required field '{field}' in frontmatter")]
    MissingField { path: PathBuf, field: &'static str },
    #[error("skill file {path}: empty body")]
    EmptyBody { path: PathBuf },
    #[error("skill registry: duplicate skill name '{name}' (first at {first}, also at {second})")]
    DuplicateName {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
}

/// Parse the contents of a `.md` file into a [`Skill`]. Pure — no I/O.
/// Provided for callers that already have the raw bytes (e.g. embedded
/// in-process skills shipped with the engine).
pub fn parse_skill(path: PathBuf, raw: &str) -> Result<Skill, SkillLoadError> {
    let (front, body) = split_frontmatter(raw)
        .ok_or_else(|| SkillLoadError::MissingFrontmatter { path: path.clone() })?;

    let name = front
        .get("name")
        .cloned()
        .ok_or_else(|| SkillLoadError::MissingField {
            path: path.clone(),
            field: "name",
        })?;
    let description =
        front
            .get("description")
            .cloned()
            .ok_or_else(|| SkillLoadError::MissingField {
                path: path.clone(),
                field: "description",
            })?;

    let body = body.trim_start_matches('\n').to_string();
    if body.trim().is_empty() {
        return Err(SkillLoadError::EmptyBody { path });
    }

    Ok(Skill {
        name,
        description,
        body,
        source_path: path,
    })
}

/// Strip a `---\n…\n---\n` YAML-style frontmatter block off the front
/// of `text`. Returns `(parsed_map, remaining_body)` if the block is
/// present, `None` otherwise. Only `key: value` lines are recognised
/// — nested structures, lists, and quoted multi-line strings are out
/// of scope (skills don't need them).
fn split_frontmatter(text: &str) -> Option<(HashMap<String, String>, &str)> {
    let trimmed = text.trim_start_matches('\u{feff}'); // strip BOM if present
    if !trimmed.starts_with("---\n") && !trimmed.starts_with("---\r\n") {
        return None;
    }
    let after_open = if trimmed.starts_with("---\r\n") {
        &trimmed[5..]
    } else {
        &trimmed[4..]
    };

    // Look for the closing fence. Accept either CRLF or LF.
    let (header, rest) = if let Some(end) = after_open.find("\n---\n") {
        (&after_open[..end], &after_open[end + 5..])
    } else if let Some(end) = after_open.find("\r\n---\r\n") {
        (&after_open[..end], &after_open[end + 7..])
    } else if after_open.ends_with("\n---") {
        (
            &after_open[..after_open.len() - 4],
            &after_open[after_open.len()..],
        )
    } else {
        return None;
    };

    let mut map: HashMap<String, String> = HashMap::new();
    for line in header.lines() {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (k, v) = match trimmed.split_once(':') {
            Some(x) => x,
            None => continue,
        };
        let key = k.trim().to_string();
        let value = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if !key.is_empty() {
            map.insert(key, value);
        }
    }

    Some((map, rest))
}

/// Catalogue of skills. Construct via
/// [`SkillRegistry::load_from_dir`] (production) or
/// [`SkillRegistry::from_skills`] (tests, embedded skills).
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_skills(skills: Vec<Skill>) -> Result<Self, SkillLoadError> {
        let mut seen: HashMap<String, PathBuf> = HashMap::new();
        for s in &skills {
            if let Some(first) = seen.get(&s.name) {
                return Err(SkillLoadError::DuplicateName {
                    name: s.name.clone(),
                    first: first.clone(),
                    second: s.source_path.clone(),
                });
            }
            seen.insert(s.name.clone(), s.source_path.clone());
        }
        Ok(Self { skills })
    }

    /// Scan `dir` for `*.md` skills (non-recursive) and return a
    /// registry. Missing dir is not an error — the caller may simply
    /// have no skills configured.
    pub fn load_from_dir(dir: &Path) -> Result<Self, SkillLoadError> {
        let mut skills: Vec<Skill> = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(SkillLoadError::Io {
                    path: dir.to_path_buf(),
                    source: e,
                });
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let raw = fs::read_to_string(&path).map_err(|e| SkillLoadError::Io {
                path: path.clone(),
                source: e,
            })?;
            let skill = parse_skill(path, &raw)?;
            skills.push(skill);
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self::from_skills(skills)
    }

    pub fn names(&self) -> Vec<&str> {
        self.skills.iter().map(|s| s.name.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter()
    }

    /// Render the skill catalogue as a system-prompt fragment so the
    /// LLM knows what it can ask for. Format mirrors the manifest
    /// Claude Code shows agents: one line per skill, "name — desc".
    /// Empty registry returns empty string so callers can splice
    /// unconditionally.
    pub fn manifest_for_prompt(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("## AVAILABLE SKILLS\n");
        out.push_str(
            "Call `use_skill` with `name = <skill>` to load the full instructions for one.\n\n",
        );
        for s in &self.skills {
            out.push_str(&format!("- `{}` — {}\n", s.name, s.description));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_skill(dir: &Path, slug: &str, contents: &str) -> PathBuf {
        let path = dir.join(format!("{slug}.md"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    const FIXTURE_BASIC: &str = "---\nname: refactor-rust\ndescription: When refactoring Rust\n---\n\n# Refactor Rust\n\nStep 1...\n";

    #[test]
    fn parse_skill_extracts_frontmatter_and_body() {
        let s = parse_skill(PathBuf::from("/tmp/x.md"), FIXTURE_BASIC).unwrap();
        assert_eq!(s.name, "refactor-rust");
        assert_eq!(s.description, "When refactoring Rust");
        assert!(s.body.starts_with("# Refactor Rust"));
        assert!(s.body.contains("Step 1..."));
    }

    #[test]
    fn parse_skill_strips_quoted_values() {
        let raw = "---\nname: \"refactor-rust\"\ndescription: 'When refactoring'\n---\n\nbody\n";
        let s = parse_skill(PathBuf::from("/tmp/x.md"), raw).unwrap();
        assert_eq!(s.name, "refactor-rust");
        assert_eq!(s.description, "When refactoring");
    }

    #[test]
    fn parse_skill_rejects_missing_frontmatter() {
        let raw = "# No frontmatter here\nbody\n";
        let err = parse_skill(PathBuf::from("/tmp/x.md"), raw).unwrap_err();
        assert!(matches!(err, SkillLoadError::MissingFrontmatter { .. }));
    }

    #[test]
    fn parse_skill_rejects_missing_name() {
        let raw = "---\ndescription: Just a desc\n---\n\nbody\n";
        let err = parse_skill(PathBuf::from("/tmp/x.md"), raw).unwrap_err();
        match err {
            SkillLoadError::MissingField { field, .. } => assert_eq!(field, "name"),
            other => panic!("expected MissingField(name), got {other}"),
        }
    }

    #[test]
    fn parse_skill_rejects_missing_description() {
        let raw = "---\nname: x\n---\n\nbody\n";
        let err = parse_skill(PathBuf::from("/tmp/x.md"), raw).unwrap_err();
        match err {
            SkillLoadError::MissingField { field, .. } => assert_eq!(field, "description"),
            other => panic!("expected MissingField(description), got {other}"),
        }
    }

    #[test]
    fn parse_skill_rejects_empty_body() {
        let raw = "---\nname: x\ndescription: y\n---\n\n   \n";
        let err = parse_skill(PathBuf::from("/tmp/x.md"), raw).unwrap_err();
        assert!(matches!(err, SkillLoadError::EmptyBody { .. }));
    }

    #[test]
    fn load_from_dir_finds_all_md_skills_in_alphabetical_order() {
        let dir = tempdir().unwrap();
        write_skill(
            dir.path(),
            "z-skill",
            "---\nname: z-skill\ndescription: Z\n---\n\nzbody\n",
        );
        write_skill(
            dir.path(),
            "a-skill",
            "---\nname: a-skill\ndescription: A\n---\n\nabody\n",
        );
        // Non-md file ignored.
        std::fs::write(dir.path().join("notes.txt"), "should be skipped").unwrap();

        let registry = SkillRegistry::load_from_dir(dir.path()).unwrap();
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.names(), vec!["a-skill", "z-skill"]);
    }

    #[test]
    fn load_from_dir_returns_empty_when_dir_missing() {
        let registry = SkillRegistry::load_from_dir(Path::new("/tmp/__no_such_dir__")).unwrap();
        assert!(registry.is_empty());
    }

    #[test]
    fn load_from_dir_propagates_parse_errors() {
        let dir = tempdir().unwrap();
        write_skill(dir.path(), "broken", "no frontmatter here\n");
        let err = SkillRegistry::load_from_dir(dir.path()).unwrap_err();
        assert!(matches!(err, SkillLoadError::MissingFrontmatter { .. }));
    }

    #[test]
    fn from_skills_rejects_duplicate_names() {
        let s1 = Skill {
            name: "dup".to_string(),
            description: "first".to_string(),
            body: "b1".to_string(),
            source_path: PathBuf::from("/tmp/a.md"),
        };
        let s2 = Skill {
            name: "dup".to_string(),
            description: "second".to_string(),
            body: "b2".to_string(),
            source_path: PathBuf::from("/tmp/b.md"),
        };
        let err = SkillRegistry::from_skills(vec![s1, s2]).unwrap_err();
        assert!(matches!(err, SkillLoadError::DuplicateName { name, .. } if name == "dup"));
    }

    #[test]
    fn manifest_for_prompt_renders_one_line_per_skill() {
        let registry = SkillRegistry::from_skills(vec![
            Skill {
                name: "refactor-rust".to_string(),
                description: "When refactoring Rust".to_string(),
                body: "...".to_string(),
                source_path: PathBuf::from("/tmp/r.md"),
            },
            Skill {
                name: "explain-architecture".to_string(),
                description: "When the user asks how X works".to_string(),
                body: "...".to_string(),
                source_path: PathBuf::from("/tmp/e.md"),
            },
        ])
        .unwrap();
        let manifest = registry.manifest_for_prompt();
        assert!(manifest.contains("AVAILABLE SKILLS"));
        assert!(manifest.contains("`refactor-rust` — When refactoring Rust"));
        assert!(manifest.contains("`explain-architecture` — When the user asks how X works"));
        assert!(manifest.contains("use_skill"));
    }

    #[test]
    fn manifest_for_prompt_empty_registry_returns_empty_string() {
        let registry = SkillRegistry::empty();
        assert_eq!(registry.manifest_for_prompt(), "");
    }

    #[test]
    fn get_returns_none_for_unknown_skill() {
        let registry = SkillRegistry::empty();
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn handles_crlf_line_endings() {
        let raw = "---\r\nname: x\r\ndescription: y\r\n---\r\n\r\nbody line\r\n";
        let s = parse_skill(PathBuf::from("/tmp/x.md"), raw).unwrap();
        assert_eq!(s.name, "x");
        assert_eq!(s.description, "y");
        assert!(s.body.contains("body line"));
    }

    #[test]
    fn handles_bom_prefix() {
        let raw =
            "\u{feff}---\nname: x\ndescription: y\n---\n\nbody\n";
        let s = parse_skill(PathBuf::from("/tmp/x.md"), raw).unwrap();
        assert_eq!(s.name, "x");
    }
}
