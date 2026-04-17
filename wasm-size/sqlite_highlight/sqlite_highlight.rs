// Syntax highlight (Rust): tree-sitter + tree-sitter-sequel.
//
// Reads SQL from stdin and writes HTML-highlighted output to stdout.
// Size-comparable counterpart to sqlite_highlight.wado (Gale).

use std::io::{self, Read, Write};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.builtin",
    "keyword",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

fn render_html(source: &[u8], events: Vec<HighlightEvent>, out: &mut Vec<u8>) {
    for event in events {
        match event {
            HighlightEvent::Source { start, end } => {
                let text = std::str::from_utf8(&source[start..end]).unwrap_or("");
                for ch in text.chars() {
                    match ch {
                        '<' => out.extend_from_slice(b"&lt;"),
                        '>' => out.extend_from_slice(b"&gt;"),
                        '&' => out.extend_from_slice(b"&amp;"),
                        '"' => out.extend_from_slice(b"&quot;"),
                        _ => {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                }
            }
            HighlightEvent::HighlightStart(highlight) => {
                out.extend_from_slice(b"<span class=\"");
                for b in HIGHLIGHT_NAMES[highlight.0].bytes() {
                    out.push(if b == b'.' { b' ' } else { b });
                }
                out.extend_from_slice(b"\">");
            }
            HighlightEvent::HighlightEnd => {
                out.extend_from_slice(b"</span>");
            }
        }
    }
}

fn main() {
    let mut sql = Vec::new();
    io::stdin().read_to_end(&mut sql).expect("read stdin");

    let language = tree_sitter_sequel::LANGUAGE.into();
    let mut config = HighlightConfiguration::new(
        language,
        "sql",
        tree_sitter_sequel::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .expect("highlight config");
    config.configure(HIGHLIGHT_NAMES);

    let mut highlighter = Highlighter::new();
    let events: Vec<_> = highlighter
        .highlight(&config, &sql, None, |_| None)
        .expect("highlight")
        .map(|e| e.expect("event"))
        .collect();

    let mut out = Vec::with_capacity(sql.len() * 2);
    render_html(&sql, events, &mut out);
    io::stdout().write_all(&out).expect("write stdout");
}
