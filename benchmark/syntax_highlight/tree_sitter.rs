// Tree-sitter syntax highlighting benchmark for SQL.
// Comparison baseline for Gale-generated syntax highlighter.

use std::time::Instant;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

// Standard tree-sitter highlight capture names
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

fn render_html(source: &[u8], events: Vec<HighlightEvent>) -> String {
    let mut html = String::with_capacity(source.len() * 2);
    for event in events {
        match event {
            HighlightEvent::Source { start, end } => {
                let text = std::str::from_utf8(&source[start..end]).unwrap_or("");
                for ch in text.chars() {
                    match ch {
                        '<' => html.push_str("&lt;"),
                        '>' => html.push_str("&gt;"),
                        '&' => html.push_str("&amp;"),
                        '"' => html.push_str("&quot;"),
                        _ => html.push(ch),
                    }
                }
            }
            HighlightEvent::HighlightStart(highlight) => {
                let name = HIGHLIGHT_NAMES[highlight.0];
                let class = name.replace('.', " ");
                html.push_str(&format!("<span class=\"{}\">", class));
            }
            HighlightEvent::HighlightEnd => {
                html.push_str("</span>");
            }
        }
    }
    html
}

fn main() {
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sqlite_parse/queries.sql"
    ))
    .expect("Failed to read queries.sql");

    let iterations = 100;

    println!(
        "syntax-highlight (tree-sitter): {} bytes, {} iterations",
        sql.len(),
        iterations
    );

    let language = tree_sitter_sequel::LANGUAGE.into();
    let mut config = HighlightConfiguration::new(
        language,
        "sql",
        tree_sitter_sequel::HIGHLIGHTS_QUERY,
        "",  // injections query
        "",  // locals query
    )
    .expect("Failed to create highlight configuration");
    config.configure(HIGHLIGHT_NAMES);

    let mut highlighter = Highlighter::new();

    // Warm up
    {
        let events: Vec<_> = highlighter
            .highlight(&config, sql.as_bytes(), None, |_| None)
            .expect("Highlight error")
            .map(|e| e.expect("Event error"))
            .collect();
        let html = render_html(sql.as_bytes(), events);
        assert!(html.len() > 0);
    }

    let start = Instant::now();
    for _ in 0..iterations {
        let events: Vec<_> = highlighter
            .highlight(&config, sql.as_bytes(), None, |_| None)
            .expect("Highlight error")
            .map(|e| e.expect("Event error"))
            .collect();
        let html = render_html(sql.as_bytes(), events);
        assert!(html.len() > 0);
    }
    let elapsed = start.elapsed();

    let elapsed_us = elapsed.as_micros();
    let per_iter_us = elapsed_us / iterations as u128;

    println!(
        "Elapsed: {}.{:03} ms ({} iterations)",
        elapsed.as_millis(),
        elapsed.as_micros() % 1000,
        iterations
    );
    println!(
        "Per iteration: {}.{:03} us",
        per_iter_us,
        (elapsed_us * 1000 / iterations as u128) % 1000
    );
}
