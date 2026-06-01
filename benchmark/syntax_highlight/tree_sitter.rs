// Tree-sitter syntax highlighting benchmark for SQL.
// Comparison baseline for Gale-generated syntax highlighter.
//
// Reports highlighting throughput (MB/s). The iteration count auto-calibrates
// so the timed loop runs for about a second.

use std::time::Instant;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

const TARGET_NS: u128 = 1_000_000_000; // ~1s budget

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

fn report(label: &str, work_per_iter: f64, n: u64, elapsed_ns: u128, unit: &str) {
    let secs = elapsed_ns as f64 / 1e9;
    let rate = if secs > 0.0 {
        work_per_iter * n as f64 / secs
    } else {
        0.0
    };
    let per_ms = elapsed_ns as f64 / n as f64 / 1e6;
    let rbuf = if unit == "B" {
        if rate >= 1e9 {
            format!("{:.2} GB/s", rate / 1e9)
        } else if rate >= 1e6 {
            format!("{:.2} MB/s", rate / 1e6)
        } else if rate >= 1e3 {
            format!("{:.2} KB/s", rate / 1e3)
        } else {
            format!("{rate:.2} B/s")
        }
    } else if rate >= 1e9 {
        format!("{:.2} G {unit}/s", rate / 1e9)
    } else if rate >= 1e6 {
        format!("{:.2} M {unit}/s", rate / 1e6)
    } else if rate >= 1e3 {
        format!("{:.2} k {unit}/s", rate / 1e3)
    } else {
        format!("{rate:.2} {unit}/s")
    };
    println!("{label}: {rbuf}   ({per_ms:.3} ms/iter, {n} iter)");
}

// Calibrate `f` to run for about `TARGET_NS`, then report its throughput.
fn bench<T, F: FnMut() -> T>(label: &str, work_per_iter: f64, unit: &str, mut f: F) -> T {
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
    report(label, work_per_iter, iters, elapsed, unit);
    result
}

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
    let sql = std::fs::read_to_string("sqlite_parse/queries.sql")
        .or_else(|_| std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sqlite_parse/queries.sql"
        )))
        .expect("Failed to read queries.sql");

    let size = sql.len();

    println!("syntax-highlight (tree-sitter): {size} bytes");

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

    let last_len = bench("Throughput", size as f64, "B", || {
        let events: Vec<_> = highlighter
            .highlight(&config, sql.as_bytes(), None, |_| None)
            .expect("Highlight error")
            .map(|e| e.expect("Event error"))
            .collect();
        render_html(sql.as_bytes(), events).len()
    });

    assert!(last_len > 0);
}
