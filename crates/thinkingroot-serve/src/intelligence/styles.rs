// crates/thinkingroot-serve/src/intelligence/styles.rs
//
// Output styles — system-prompt fragments that override the default
// conversational tone for specific surfaces.
//
// Examples:
//   * `explanatory` — appends "include educational insights as you go"
//                     framing to the prompt; produces longer, teaching-
//                     oriented answers.
//   * `terse`       — clamps answers to the absolute minimum, no
//                     filler, no pleasantries, no preamble.
//   * `technical`   — heavy citation density, code-block default,
//                     more structure.
//
// Stored at `.thinkingroot/styles/<slug>.md` with the same frontmatter
// shape as skills (parsed via `skills::parse_skill`'s sibling). The
// active style for a workspace is set in `[chat]` config or per-call
// via the REST `style` field; the synthesizer appends the style's
// body to the conversational system prompt before sending to the LLM.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyle {
    pub name: String,
    pub description: String,
    /// System-prompt fragment appended to the resolved persona prompt
    /// (with one blank line separator). Goes into the `system` field
    /// of every chat / chat_with_tools call when this style is active.
    pub system_fragment: String,
    pub source_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum StyleLoadError {
    #[error("style file {path}: I/O: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("style file {path}: missing frontmatter")]
    MissingFrontmatter { path: PathBuf },
    #[error("style file {path}: missing required field '{field}'")]
    MissingField { path: PathBuf, field: &'static str },
    #[error("style file {path}: empty system_fragment body")]
    EmptyFragment { path: PathBuf },
    #[error("style registry: duplicate style name '{name}' (first at {first}, also at {second})")]
    DuplicateName {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
}

/// Parse one `.md` file into an [`OutputStyle`]. Pure — no I/O.
pub fn parse_style(path: PathBuf, raw: &str) -> Result<OutputStyle, StyleLoadError> {
    let (front, body) = split_frontmatter(raw)
        .ok_or_else(|| StyleLoadError::MissingFrontmatter { path: path.clone() })?;

    let name = front
        .get("name")
        .cloned()
        .ok_or_else(|| StyleLoadError::MissingField {
            path: path.clone(),
            field: "name",
        })?;
    let description =
        front
            .get("description")
            .cloned()
            .ok_or_else(|| StyleLoadError::MissingField {
                path: path.clone(),
                field: "description",
            })?;

    let system_fragment = body.trim_start_matches('\n').trim_end().to_string();
    if system_fragment.trim().is_empty() {
        return Err(StyleLoadError::EmptyFragment { path });
    }

    Ok(OutputStyle {
        name,
        description,
        system_fragment,
        source_path: path,
    })
}

/// Same frontmatter splitter as the skills loader. Duplicated rather
/// than shared because the two modules are at peer level and share
/// nothing else; the parser is 30 lines and we'd rather keep them
/// independent than introduce a "frontmatter" helper module that
/// would just be over-abstraction for two call sites.
fn split_frontmatter(text: &str) -> Option<(HashMap<String, String>, &str)> {
    let trimmed = text.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---\n") && !trimmed.starts_with("---\r\n") {
        return None;
    }
    let after_open = if trimmed.starts_with("---\r\n") {
        &trimmed[5..]
    } else {
        &trimmed[4..]
    };
    let (header, rest) = if let Some(end) = after_open.find("\n---\n") {
        (&after_open[..end], &after_open[end + 5..])
    } else if let Some(end) = after_open.find("\r\n---\r\n") {
        (&after_open[..end], &after_open[end + 7..])
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

#[derive(Debug, Default, Clone)]
pub struct OutputStyleRegistry {
    styles: Vec<OutputStyle>,
}

impl OutputStyleRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_styles(styles: Vec<OutputStyle>) -> Result<Self, StyleLoadError> {
        let mut seen: HashMap<String, PathBuf> = HashMap::new();
        for s in &styles {
            if let Some(first) = seen.get(&s.name) {
                return Err(StyleLoadError::DuplicateName {
                    name: s.name.clone(),
                    first: first.clone(),
                    second: s.source_path.clone(),
                });
            }
            seen.insert(s.name.clone(), s.source_path.clone());
        }
        Ok(Self { styles })
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self, StyleLoadError> {
        let mut styles: Vec<OutputStyle> = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(StyleLoadError::Io {
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
            let raw = fs::read_to_string(&path).map_err(|e| StyleLoadError::Io {
                path: path.clone(),
                source: e,
            })?;
            let style = parse_style(path, &raw)?;
            styles.push(style);
        }
        styles.sort_by(|a, b| a.name.cmp(&b.name));
        Self::from_styles(styles)
    }

    pub fn get(&self, name: &str) -> Option<&OutputStyle> {
        self.styles.iter().find(|s| s.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.styles.iter().map(|s| s.name.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.styles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &OutputStyle> {
        self.styles.iter()
    }
}

/// Compose the final system prompt by appending an output style's
/// fragment (when present) to the resolved persona prompt. Returns the
/// persona prompt unchanged when the style is `None` or empty.
///
/// Layout:
///
/// ```text
/// <persona prompt>
///
/// ## ACTIVE STYLE: <name>
/// <style fragment>
/// ```
///
/// The `## ACTIVE STYLE` header is what tells the model "anything
/// below is a layered override on top of the persona above" — same
/// pattern Claude Code uses for `output-style`.
pub fn compose_system_prompt(persona_prompt: &str, style: Option<&OutputStyle>) -> String {
    let Some(style) = style else {
        return persona_prompt.to_string();
    };
    if style.system_fragment.trim().is_empty() {
        return persona_prompt.to_string();
    }
    format!(
        "{}\n\n## ACTIVE STYLE: {}\n{}\n",
        persona_prompt.trim_end(),
        style.name,
        style.system_fragment.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_style(dir: &Path, slug: &str, contents: &str) -> PathBuf {
        let path = dir.join(format!("{slug}.md"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    const FIXTURE_TERSE: &str = "---\nname: terse\ndescription: Strict short answers\n---\n\nAnswer in one sentence. No filler.\n";

    #[test]
    fn parse_style_extracts_fields_and_body() {
        let s = parse_style(PathBuf::from("/tmp/terse.md"), FIXTURE_TERSE).unwrap();
        assert_eq!(s.name, "terse");
        assert_eq!(s.description, "Strict short answers");
        assert!(s.system_fragment.contains("Answer in one sentence"));
    }

    #[test]
    fn parse_style_rejects_missing_frontmatter() {
        let raw = "no frontmatter\nbody\n";
        let err = parse_style(PathBuf::from("/tmp/x.md"), raw).unwrap_err();
        assert!(matches!(err, StyleLoadError::MissingFrontmatter { .. }));
    }

    #[test]
    fn parse_style_rejects_missing_name() {
        let raw = "---\ndescription: A style\n---\nbody\n";
        let err = parse_style(PathBuf::from("/tmp/x.md"), raw).unwrap_err();
        match err {
            StyleLoadError::MissingField { field, .. } => assert_eq!(field, "name"),
            other => panic!("got {other}"),
        }
    }

    #[test]
    fn parse_style_rejects_empty_fragment() {
        let raw = "---\nname: x\ndescription: y\n---\n\n   \n";
        let err = parse_style(PathBuf::from("/tmp/x.md"), raw).unwrap_err();
        assert!(matches!(err, StyleLoadError::EmptyFragment { .. }));
    }

    #[test]
    fn load_from_dir_collects_all_md_styles_alphabetical() {
        let dir = tempdir().unwrap();
        write_style(
            dir.path(),
            "z",
            "---\nname: z\ndescription: Z\n---\n\nz body\n",
        );
        write_style(
            dir.path(),
            "a",
            "---\nname: a\ndescription: A\n---\n\na body\n",
        );
        let registry = OutputStyleRegistry::load_from_dir(dir.path()).unwrap();
        assert_eq!(registry.names(), vec!["a", "z"]);
    }

    #[test]
    fn load_from_dir_returns_empty_when_dir_missing() {
        let r = OutputStyleRegistry::load_from_dir(Path::new("/tmp/__no_styles_dir__")).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn from_styles_rejects_duplicate_names() {
        let s1 = OutputStyle {
            name: "dup".to_string(),
            description: "first".to_string(),
            system_fragment: "x".to_string(),
            source_path: PathBuf::from("/tmp/a.md"),
        };
        let s2 = OutputStyle {
            name: "dup".to_string(),
            description: "second".to_string(),
            system_fragment: "y".to_string(),
            source_path: PathBuf::from("/tmp/b.md"),
        };
        let err = OutputStyleRegistry::from_styles(vec![s1, s2]).unwrap_err();
        assert!(matches!(err, StyleLoadError::DuplicateName { .. }));
    }

    #[test]
    fn compose_system_prompt_returns_persona_when_style_is_none() {
        let composed = compose_system_prompt("persona text", None);
        assert_eq!(composed, "persona text");
    }

    #[test]
    fn compose_system_prompt_appends_style_fragment_with_header() {
        let style = OutputStyle {
            name: "terse".to_string(),
            description: "Short".to_string(),
            system_fragment: "Answer in one sentence.".to_string(),
            source_path: PathBuf::from("/tmp/x.md"),
        };
        let composed = compose_system_prompt("You are helpful.", Some(&style));
        assert!(composed.contains("You are helpful."));
        assert!(composed.contains("## ACTIVE STYLE: terse"));
        assert!(composed.contains("Answer in one sentence."));
        // Persona text first, style appended after.
        let persona_idx = composed.find("You are helpful.").unwrap();
        let style_idx = composed.find("## ACTIVE STYLE").unwrap();
        assert!(persona_idx < style_idx);
    }

    #[test]
    fn compose_system_prompt_skips_empty_fragment_style() {
        let style = OutputStyle {
            name: "empty".to_string(),
            description: "n/a".to_string(),
            system_fragment: "   ".to_string(),
            source_path: PathBuf::from("/tmp/x.md"),
        };
        let composed = compose_system_prompt("persona", Some(&style));
        assert_eq!(composed, "persona");
    }

    #[test]
    fn get_returns_none_for_unknown() {
        let r = OutputStyleRegistry::empty();
        assert!(r.get("nope").is_none());
    }
}
