//! `#![generated]` header emission and parsing.
//!
//! Every file a Kiln generator produces is stamped with a
//! `#![generated(by = "...", sources = [...])]` module attribute. The
//! compiler uses this for two things:
//!
//! 1. Tools (formatter, linter, VS Code) can show a subtle banner.
//! 2. The pipeline's stale-output GC deletes files inside an invocation's
//!    `output-dir` that bear `#![generated]` but were *not* produced by
//!    the current run. Hand-authored files in the same directory are
//!    untouched.
//!
//! The header is line-based and kept near the top of the file; parsing
//! here is intentionally tolerant (whitespace, trailing comma) but
//! emission is canonical.

use super::invocation::InvocationPath;

/// Metadata carried by a `#![generated(...)]` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedHeader {
    /// Generator identity string (e.g. `"ns:proto@1.0.0"`) or `"unknown"`.
    pub by: String,
    /// Source files the generator saw as inputs, in declaration order.
    pub sources: Vec<String>,
}

impl GeneratedHeader {
    /// Render the canonical form placed at the top of every generated file.
    ///
    /// Output format (exact):
    /// ```text
    /// #![generated(by = "<by>", sources = [<src1>, <src2>])]
    /// ```
    ///
    /// Followed by a single newline. Callers prepend this to the generator's
    /// body before writing to disk.
    #[must_use]
    pub fn emit(&self) -> String {
        let mut out = String::from("#![generated(by = \"");
        push_escaped(&mut out, &self.by);
        out.push_str("\", sources = [");
        for (i, src) in self.sources.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('"');
            push_escaped(&mut out, src);
            out.push('"');
        }
        out.push_str("])]\n");
        out
    }

    /// Emit with extra paths; convenience for callers that hold
    /// `InvocationPath` lists.
    #[must_use]
    pub fn emit_with_paths(by: &str, paths: &[InvocationPath]) -> String {
        Self {
            by: by.to_string(),
            sources: paths.iter().map(|p| p.as_str().to_string()).collect(),
        }
        .emit()
    }
}

/// Detect whether `source` begins with a `#![generated]` attribute.
///
/// Skips a leading UTF-8 BOM, `//!`/`//` line comments, and blank lines
/// before checking the attribute line — matching how
/// [`crate::parser`] tolerates leading trivia.
#[must_use]
pub fn has_generated_marker(source: &str) -> bool {
    parse_header(source).is_some()
}

/// Parse the `#![generated(...)]` header if present.
///
/// Returns `None` if the source does not open with such an attribute.
/// The parser is permissive — unknown keys are tolerated — so later fields
/// can be added without invalidating existing files.
#[must_use]
pub fn parse_header(source: &str) -> Option<GeneratedHeader> {
    let mut src = source.strip_prefix('\u{feff}').unwrap_or(source);
    loop {
        src = src.trim_start_matches([' ', '\t']);
        if let Some(rest) = src.strip_prefix("\r\n").or_else(|| src.strip_prefix('\n')) {
            src = rest;
            continue;
        }
        if src.starts_with("//") {
            let end = src.find('\n').unwrap_or(src.len());
            src = &src[end..];
            continue;
        }
        break;
    }

    let rest = src.strip_prefix("#![generated")?;
    if rest.starts_with(']') {
        return Some(GeneratedHeader {
            by: "unknown".to_string(),
            sources: Vec::new(),
        });
    }
    let rest = rest.strip_prefix('(')?;
    let close = rest.find(")]")?;
    let body = &rest[..close];

    let mut by = "unknown".to_string();
    let mut sources: Vec<String> = Vec::new();
    for part in split_top_level_commas(body) {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("by = ") {
            by = parse_quoted_string(val.trim())?;
        } else if let Some(val) = part.strip_prefix("sources = ") {
            sources = parse_string_array(val.trim())?;
        }
    }
    Some(GeneratedHeader { by, sources })
}

/// Split `s` on top-level commas, respecting brackets and quotes.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '[' | '(' if !in_string => depth += 1,
            ']' | ')' if !in_string => depth -= 1,
            ',' if depth == 0 && !in_string => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start <= s.len() {
        out.push(&s[start..]);
    }
    out
}

fn parse_quoted_string(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn parse_string_array(s: &str) -> Option<Vec<String>> {
    let inner = s.strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    for part in split_top_level_commas(inner) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(parse_quoted_string(part)?);
    }
    Some(out)
}

fn push_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_roundtrip_single_source() {
        let h = GeneratedHeader {
            by: "ns:proto@1.0.0".to_string(),
            sources: vec!["schema.proto".to_string()],
        };
        let text = h.emit();
        let parsed = parse_header(&text).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn emit_roundtrip_multiple_sources() {
        let h = GeneratedHeader {
            by: "ns:proto@1.0.0".to_string(),
            sources: vec![
                "a.proto".to_string(),
                "b.proto".to_string(),
                "c.proto".to_string(),
            ],
        };
        assert_eq!(parse_header(&h.emit()).unwrap(), h);
    }

    #[test]
    fn emit_escapes_quotes() {
        let h = GeneratedHeader {
            by: "weird\"name".to_string(),
            sources: vec!["with\"quote.proto".to_string()],
        };
        let text = h.emit();
        assert!(text.contains("\\\""));
        assert_eq!(parse_header(&text).unwrap(), h);
    }

    #[test]
    fn has_marker_true_for_generated() {
        let src = "#![generated(by = \"ns:p@1.0.0\", sources = [\"s.proto\"])]\n\npub fn x() {}";
        assert!(has_generated_marker(src));
    }

    #[test]
    fn has_marker_false_for_plain_file() {
        let src = "pub fn x() {}\n";
        assert!(!has_generated_marker(src));
    }

    #[test]
    fn has_marker_true_with_leading_bom_and_comments() {
        let src = "\u{feff}// copyright banner\n\n#![generated(by = \"x\", sources = [])]\n";
        assert!(has_generated_marker(src));
    }

    #[test]
    fn parse_tolerates_unknown_key() {
        let src =
            "#![generated(by = \"x\", sources = [\"s\"], future_field = \"irrelevant\")]\n";
        let h = parse_header(src).unwrap();
        assert_eq!(h.by, "x");
        assert_eq!(h.sources, vec!["s".to_string()]);
    }

    #[test]
    fn parse_none_when_marker_missing() {
        assert!(parse_header("").is_none());
        assert!(parse_header("pub fn x() {}").is_none());
        assert!(parse_header("#![other(attr)]").is_none());
    }

    #[test]
    fn emit_with_paths_uses_invocation_path_format() {
        let out =
            GeneratedHeader::emit_with_paths("gen", &[InvocationPath::normalize("./a/b.proto")]);
        assert!(out.contains("\"a/b.proto\""));
    }
}
