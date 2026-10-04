// Highlight throughput of tree-sitter-highlight over the same page as the Gale
// arm: tree-sitter-html, with JavaScript, CSS and JSON injected where the page
// carries them.
//
// Reports highlighting throughput (MB/s). The iteration count auto-calibrates
// so the timed loop runs for about a second.

use std::time::Instant;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

const TARGET_NS: u128 = 1_000_000_000; // ~1s budget

const INPUT: &str = include_str!("input.html");

// The stock HTML injections cover `<script>` and `<style>` bodies only. These
// add the regions the Gale arm highlights too, except `style="..."`: the CSS
// grammar parses a stylesheet, not a declaration list.
const EXTRA_INJECTIONS: &str = r#"
((script_element
  (start_tag
    (attribute
      (attribute_name) @_attr
      (quoted_attribute_value (attribute_value) @_type)))
  (raw_text) @injection.content)
 (#eq? @_attr "type")
 (#match? @_type "json")
 (#set! injection.language "json"))

((attribute
  (attribute_name) @_attr
  (quoted_attribute_value (attribute_value) @injection.content))
 (#match? @_attr "^on")
 (#set! injection.language "javascript"))
"#;

fn next_iters(n: u64, elapsed_ns: u128, target_ns: u128) -> u64 {
    let e = if elapsed_ns == 0 { 1 } else { elapsed_ns };
    let mut est = (n as u128) * target_ns / e;
    let hi = n as u128 * 100;
    if est > hi {
        est = hi;
    }
    if est > 1_000_000_000 {
        est = 1_000_000_000;
    }
    if est < 1 {
        est = 1;
    }
    est as u64
}

fn report(label: &str, bytes_per_iter: f64, n: u64, elapsed_ns: u128) {
    let secs = elapsed_ns as f64 / 1e9;
    let rate = if secs > 0.0 { bytes_per_iter * n as f64 / secs } else { 0.0 };
    let per_ms = elapsed_ns as f64 / n as f64 / 1e6;
    let rbuf = if rate >= 1e9 {
        format!("{:.2} GB/s", rate / 1e9)
    } else if rate >= 1e6 {
        format!("{:.2} MB/s", rate / 1e6)
    } else if rate >= 1e3 {
        format!("{:.2} KB/s", rate / 1e3)
    } else {
        format!("{rate:.2} B/s")
    };
    println!("{label}: {rbuf}   ({per_ms:.3} ms/iter, {n} iter)");
}

// Calibrate `f` to run for about `TARGET_NS`, then report its throughput.
fn bench<T, F: FnMut() -> T>(label: &str, bytes_per_iter: f64, mut f: F) -> T {
    let mut result = f(); // warmup
    let mut iters: u64 = 1;
    let elapsed: u128;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            result = f();
        }
        let e = start.elapsed().as_nanos();
        if e >= TARGET_NS {
            elapsed = e;
            break;
        }
        let nx = next_iters(iters, e, TARGET_NS);
        if nx <= iters {
            elapsed = e;
            break;
        }
        iters = nx;
    }
    report(label, bytes_per_iter, iters, elapsed);
    result
}

const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "escape",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.special",
    "string.special.key",
    "tag",
    "type",
    "variable",
    "variable.builtin",
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
                let class = HIGHLIGHT_NAMES[highlight.0].replace('.', " ");
                html.push_str(&format!("<span class=\"{class}\">"));
            }
            HighlightEvent::HighlightEnd => html.push_str("</span>"),
        }
    }
    html
}

fn configuration(
    language: tree_sitter::Language,
    name: &str,
    highlights: &str,
    injections: &str,
) -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(language, name, highlights, injections, "")
        .expect("Failed to create highlight configuration");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn main() {
    let size = INPUT.len();
    println!("gale-highlight-html (tree-sitter): {size} bytes");

    let html_injections = format!("{}{EXTRA_INJECTIONS}", tree_sitter_html::INJECTIONS_QUERY);
    let html = configuration(
        tree_sitter_html::LANGUAGE.into(),
        "html",
        tree_sitter_html::HIGHLIGHTS_QUERY,
        &html_injections,
    );
    let javascript = configuration(
        tree_sitter_javascript::LANGUAGE.into(),
        "javascript",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::INJECTIONS_QUERY,
    );
    let css = configuration(tree_sitter_css::LANGUAGE.into(), "css", tree_sitter_css::HIGHLIGHTS_QUERY, "");
    let json = configuration(tree_sitter_json::LANGUAGE.into(), "json", tree_sitter_json::HIGHLIGHTS_QUERY, "");
    let mut highlighter = Highlighter::new();
    let mut render = || {
        let injected = |name: &str| match name {
            "javascript" => Some(&javascript),
            "css" => Some(&css),
            "json" => Some(&json),
            _ => None,
        };
        let events: Vec<_> = highlighter
            .highlight(&html, INPUT.as_bytes(), None, injected)
            .expect("Highlight error")
            .map(|e| e.expect("Event error"))
            .collect();
        render_html(INPUT.as_bytes(), events)
    };
    let out = render();
    // Injected, not left as text: a JavaScript keyword and a CSS property.
    assert!(out.contains("<span class=\"keyword\">class</span>"));
    assert!(out.contains("<span class=\"property\">margin</span>"));
    bench("Throughput", size as f64, || render().len());
}
