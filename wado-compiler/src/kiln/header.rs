//! `#![generated(by = …, sources = […])]` header emission and parsing. Every
//! file a Kiln generator produces is stamped with it, so tools can show a banner
//! and the pipeline's stale-output GC can delete a stamped file the current run
//! did not produce, leaving hand-authored neighbours alone. Parsing reads the
//! header as the lexer does, so any spelling of it is accepted; emission is
//! canonical.

use std::iter::Peekable;

use super::invocation::InvocationPath;
use crate::escape::{quoted, string_value};
use crate::lexer::lex;
use crate::token::TokenKind;

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
        let sources: Vec<String> = self.sources.iter().map(|s| quoted(s)).collect();
        format!(
            "#![generated(by = {}, sources = [{}])]\n",
            quoted(&self.by),
            sources.join(", ")
        )
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

/// Detect whether `source` begins with a `#![generated]` attribute, after a
/// UTF-8 BOM and comments.
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
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let lexed = lex(source);
    let mut tokens = lexed.tokens.iter().map(|token| &token.kind).peekable();
    let mut next_is = |kind: &TokenKind| tokens.next_if_eq(&kind).is_some();
    if !(next_is(&TokenKind::Hash) && next_is(&TokenKind::Not) && next_is(&TokenKind::LBracket)) {
        return None;
    }
    if tokens.next()? != &TokenKind::Ident("generated".to_string()) {
        return None;
    }

    let mut header = GeneratedHeader {
        by: "unknown".to_string(),
        sources: Vec::new(),
    };
    if tokens.next_if_eq(&&TokenKind::LParen).is_some() {
        while tokens.next_if_eq(&&TokenKind::RParen).is_none() {
            let TokenKind::Ident(key) = tokens.next()? else {
                return None;
            };
            if tokens.next()? != &TokenKind::Eq {
                return None;
            }
            match key.as_str() {
                "by" => header.by = string(tokens.next()?)?,
                "sources" => header.sources = string_list(&mut tokens)?,
                _ => skip_value(&mut tokens)?,
            }
            if tokens.next_if_eq(&&TokenKind::Comma).is_none()
                && tokens.peek() != Some(&&TokenKind::RParen)
            {
                return None;
            }
        }
    }
    (tokens.next()? == &TokenKind::RBracket).then_some(header)
}

fn string(kind: &TokenKind) -> Option<String> {
    let TokenKind::StringLit { raw, quote } = kind else {
        return None;
    };
    string_value(raw, *quote).ok()
}

/// `[s, …]`, a trailing comma allowed.
fn string_list<'t>(
    tokens: &mut Peekable<impl Iterator<Item = &'t TokenKind>>,
) -> Option<Vec<String>> {
    if tokens.next()? != &TokenKind::LBracket {
        return None;
    }
    let mut out = Vec::new();
    while tokens.next_if_eq(&&TokenKind::RBracket).is_none() {
        out.push(string(tokens.next()?)?);
        if tokens.next_if_eq(&&TokenKind::Comma).is_none()
            && tokens.peek() != Some(&&TokenKind::RBracket)
        {
            return None;
        }
    }
    Some(out)
}

/// Step over an unknown key's value, up to the comma or `)` that ends it.
fn skip_value<'t>(tokens: &mut Peekable<impl Iterator<Item = &'t TokenKind>>) -> Option<()> {
    let mut depth = 0u32;
    loop {
        match tokens.peek()? {
            TokenKind::Comma | TokenKind::RParen if depth == 0 => return Some(()),
            TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket => depth = depth.checked_sub(1)?,
            _ => {}
        }
        tokens.next();
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
        let src = "#![generated(by = \"x\", sources = [\"s\"], future_field = \"irrelevant\")]\n";
        let h = parse_header(src).unwrap();
        assert_eq!(h.by, "x");
        assert_eq!(h.sources, vec!["s".to_string()]);
    }

    /// A header is Wado source, so any string literal spells a value in it.
    #[test]
    fn parse_reads_every_string_literal_form() {
        let src = r##"#![generated(by = #"x\y"#, sources = ["a\u{62}", #"./c\d"#,])]"##;
        let h = parse_header(src).unwrap();
        assert_eq!(h.by, r"x\y");
        assert_eq!(h.sources, vec!["ab".to_string(), r"./c\d".to_string()]);
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
