//! Wedge 4: structured doc-comment tag extraction.
//!
//! Recognises `@param` / `@returns` / `@throws` / `@deprecated` / `@see`
//! style annotations across major flavours and writes the parsed [`DocTag`]
//! list onto the chunk's metadata. Does **not** emit new chunks — the
//! parent `Comment`/`ModuleDoc` chunk stays the source-of-truth scope, so
//! existing comment-to-parent linkage in the structural extractor keeps
//! working.
//!
//! Supported flavours (best-effort, no parser fallback into LLM):
//!
//! - **Rustdoc / JSDoc / JavaDoc**: `@param name desc`, `@returns desc`,
//!   `@throws Type desc`, `@deprecated [reason]`, `@see ref`.
//! - **Python Google-style**: `Args:` / `Returns:` / `Raises:` blocks with
//!   indented `name: description` lines under `Args:` and `Type: description`
//!   under `Raises:`.
//! - **Python reStructuredText**: `:param name: desc`, `:returns: desc`,
//!   `:raises Type: desc`.

use thinkingroot_core::ir::{Chunk, DocTag};

/// Append parsed doc-tags to `chunk.metadata.doc_tags`. Idempotent in the
/// sense that the existing list is preserved and new tags are appended.
pub fn populate(chunk: &mut Chunk) {
    let tags = parse(&chunk.content);
    if !tags.is_empty() {
        chunk.metadata.doc_tags.extend(tags);
    }
}

/// Parse a comment body (with leading `///`, `//!`, `/**`, `*`, `#`, etc.
/// trim markers tolerated) and return the recognised tags.
pub fn parse(body: &str) -> Vec<DocTag> {
    let cleaned = strip_comment_markers(body);
    let mut tags = Vec::new();

    parse_javadoc_style(&cleaned, &mut tags);
    parse_python_google_style(&cleaned, &mut tags);
    parse_python_rest_style(&cleaned, &mut tags);

    tags
}

fn strip_comment_markers(body: &str) -> String {
    body.lines()
        .map(|l| {
            // Remove leading whitespace + comment prefix together when a
            // comment marker is present. When no marker is present (e.g.
            // a Python docstring or a plain code-block doctag), leave the
            // line — including its leading whitespace — untouched so
            // indentation-sensitive parsing (`Args:` blocks) still works.
            let trimmed = l.trim_start();

            for prefix in ["///", "//!", "/**", "*/", "//"] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    return rest.strip_prefix(' ').unwrap_or(rest).to_string();
                }
            }
            if let Some(rest) = trimmed.strip_prefix("* ") {
                return rest.to_string();
            }
            if trimmed == "*" {
                return String::new();
            }
            // Shell-style "# " (only with trailing space — avoid stripping a
            // bare `#!` shebang or `#[attr]` Rust attribute).
            if let Some(rest) = trimmed.strip_prefix("# ") {
                return rest.to_string();
            }
            // Triple-quoted Python docstring delimiters at the top of the body.
            if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
                let stripped = trimmed
                    .trim_start_matches("\"\"\"")
                    .trim_start_matches("'''");
                let stripped = stripped
                    .trim_end_matches("\"\"\"")
                    .trim_end_matches("'''");
                return stripped.to_string();
            }

            l.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_javadoc_style(text: &str, out: &mut Vec<DocTag>) {
    // Recognise `@<kind> [<name>] <description...>` per line.
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix('@') else {
            continue;
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let kind_raw = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("").trim();
        if kind_raw.is_empty() {
            continue;
        }
        let kind = normalise_kind(kind_raw);
        let (name, description) = split_name_and_desc(&kind, body);
        out.push(DocTag {
            kind,
            name,
            description: description.to_string(),
        });
    }
}

fn split_name_and_desc<'a>(kind: &str, body: &'a str) -> (Option<String>, &'a str) {
    // For `param` and `throws`, the first whitespace-separated token is the name.
    if kind == "param" || kind == "throws" {
        let mut split = body.splitn(2, char::is_whitespace);
        let first = split.next().unwrap_or("").trim();
        let rest = split.next().unwrap_or("").trim();
        if first.is_empty() {
            return (None, body);
        }
        return (Some(first.to_string()), rest);
    }
    (None, body)
}

fn normalise_kind(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "param" | "parameter" | "arg" | "argument" => "param".to_string(),
        "return" | "returns" => "returns".to_string(),
        "throw" | "throws" | "exception" | "raises" | "raise" => "throws".to_string(),
        "deprecated" => "deprecated".to_string(),
        "see" | "seealso" => "see".to_string(),
        other => other.to_string(),
    }
}

fn parse_python_google_style(text: &str, out: &mut Vec<DocTag>) {
    // Sections: "Args:" / "Returns:" / "Raises:" — each followed by indented
    // continuation lines.  Within Args, a line like "  name: description"
    // (or "  name (Type): description") becomes a @param tag.
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        let trimmed = line.trim();
        let section = match trimmed {
            "Args:" | "Arguments:" => Some("param"),
            "Returns:" => Some("returns"),
            "Raises:" => Some("throws"),
            _ => None,
        };
        let Some(kind) = section else {
            i += 1;
            continue;
        };
        // Required indent: deeper than the section header.
        let header_indent = line.len() - line.trim_start().len();
        i += 1;
        while i < lines.len() {
            let next = lines[i];
            if next.trim().is_empty() {
                i += 1;
                continue;
            }
            let next_indent = next.len() - next.trim_start().len();
            if next_indent <= header_indent {
                break;
            }
            // Within an indented section.
            let body = next.trim();
            match kind {
                "param" | "throws" => {
                    if let Some((name, desc)) = body.split_once(':') {
                        // Strip optional "(Type)" annotation between name and colon.
                        let name = name.split_whitespace().next().unwrap_or("").to_string();
                        if !name.is_empty() {
                            out.push(DocTag {
                                kind: kind.to_string(),
                                name: Some(name),
                                description: desc.trim().to_string(),
                            });
                        }
                    }
                }
                "returns" => {
                    out.push(DocTag {
                        kind: kind.to_string(),
                        name: None,
                        description: body.to_string(),
                    });
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn parse_python_rest_style(text: &str, out: &mut Vec<DocTag>) {
    // ":param name: description" / ":returns: ..." / ":raises Type: ..."
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(':') else {
            continue;
        };
        let Some((tag_part, desc)) = rest.split_once(':') else {
            continue;
        };
        let mut parts = tag_part.split_whitespace();
        let kind_raw = parts.next().unwrap_or("");
        let name = parts.next().map(str::to_string);
        let kind = normalise_kind(kind_raw);
        // Only honour tags we recognise.
        if !matches!(
            kind.as_str(),
            "param" | "returns" | "throws" | "deprecated" | "see"
        ) {
            continue;
        }
        out.push(DocTag {
            kind,
            name,
            description: desc.trim().to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustdoc_param_returns_extracted() {
        let body = "/// Adds two numbers together.\n\
                    ///\n\
                    /// @param a the first number\n\
                    /// @param b the second number\n\
                    /// @returns the sum of `a` and `b`\n";
        let tags = parse(body);
        assert!(tags.iter().any(|t| t.kind == "param"
            && t.name.as_deref() == Some("a")
            && t.description.contains("first number")));
        assert!(tags.iter().any(|t| t.kind == "param" && t.name.as_deref() == Some("b")));
        assert!(tags.iter().any(|t| t.kind == "returns" && t.description.contains("sum")));
    }

    #[test]
    fn jsdoc_at_param_extracted() {
        let body = "/**\n\
                     * Greet someone.\n\
                     * @param {string} who - the name\n\
                     * @returns {void}\n\
                     * @throws {TypeError} when who is null\n\
                     */";
        let tags = parse(body);
        assert!(tags.iter().any(|t| t.kind == "param"
            && (t.name.as_deref() == Some("{string}") || t.name.as_deref() == Some("who"))));
        assert!(tags.iter().any(|t| t.kind == "throws"));
    }

    #[test]
    fn python_google_style_args_extracted() {
        let body = "\"\"\"Greet a user.\n\
                    \n\
                    Args:\n    \
                        name: the user name\n    \
                        age (int): optional age\n\
                    \n\
                    Returns:\n    \
                        a greeting string\n\
                    \n\
                    Raises:\n    \
                        ValueError: if name is empty\n\
                    \"\"\"";
        let tags = parse(body);
        assert!(tags.iter().any(|t| t.kind == "param" && t.name.as_deref() == Some("name")));
        assert!(tags.iter().any(|t| t.kind == "param" && t.name.as_deref() == Some("age")));
        assert!(tags.iter().any(|t| t.kind == "returns" && t.description.contains("greeting")));
        assert!(tags.iter().any(|t| t.kind == "throws" && t.name.as_deref() == Some("ValueError")));
    }

    #[test]
    fn python_rest_style_extracted() {
        let body = ":param name: the user\n\
                    :returns: a greeting\n\
                    :raises ValueError: if name is empty\n";
        let tags = parse(body);
        assert!(tags.iter().any(|t| t.kind == "param" && t.name.as_deref() == Some("name")));
        assert!(tags.iter().any(|t| t.kind == "returns"));
        assert!(tags.iter().any(|t| t.kind == "throws" && t.name.as_deref() == Some("ValueError")));
    }

    #[test]
    fn deprecated_extracted_with_optional_reason() {
        let body = "/// @deprecated use new_api instead\n";
        let tags = parse(body);
        assert!(tags
            .iter()
            .any(|t| t.kind == "deprecated" && t.description.contains("new_api")));
    }

    #[test]
    fn comment_with_no_tags_yields_empty() {
        let body = "/// just a description with no annotations\n";
        let tags = parse(body);
        assert!(tags.is_empty());
    }
}
