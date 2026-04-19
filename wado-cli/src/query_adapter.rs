use std::fs;
use std::path::Path;
use std::process;

use serde_json::json;
use wado_lsp::{DefinitionResult, DocumentHighlight, HighlightKind, Position, ReferenceLocation};

use crate::compiler_host::FilesystemCompilerHost;

struct PreparedQuery {
    uri: String,
    engine: wado_lsp::Engine,
    host: FilesystemCompilerHost,
}

fn prepare_query(filename: &str) -> PreparedQuery {
    let path = Path::new(filename);
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {e}", path.display());
            process::exit(1);
        }
    };

    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::silent(base_path);

    let uri = format!(
        "file://{}",
        path.canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
    );
    let mut engine = wado_lsp::Engine::new();
    engine.open_document(&uri, source);

    PreparedQuery { uri, engine, host }
}

fn position_from_one_based(line: u32, column: u32) -> Position {
    Position {
        line: line.saturating_sub(1),
        character: column.saturating_sub(1),
    }
}

/// Run diagnostics query and print results.
pub async fn run_diagnostics(filename: &str, json_output: bool) {
    let prepared = prepare_query(filename);
    let diagnostics = prepared
        .engine
        .diagnostics(&prepared.uri, &prepared.host)
        .await;

    if json_output {
        print_diagnostics_json(filename, &diagnostics);
    } else {
        print_diagnostics_text(filename, &diagnostics);
    }

    if diagnostics
        .iter()
        .any(|d| matches!(d.severity, wado_lsp::Severity::Error))
    {
        process::exit(1);
    }
}

/// Run references query and print results.
pub async fn run_references(
    filename: &str,
    line: u32,
    column: u32,
    include_declaration: bool,
    json_output: bool,
) {
    let prepared = prepare_query(filename);
    let position = position_from_one_based(line, column);
    let refs = prepared
        .engine
        .references(&prepared.uri, position, include_declaration, &prepared.host)
        .await;

    if json_output {
        print_references_json(&refs);
    } else {
        print_references_text(&refs);
    }

    warn_on_compile_errors(&prepared.host, refs.is_empty());
}

/// Run definition query and print results.
pub async fn run_definition(filename: &str, line: u32, column: u32, json_output: bool) {
    let prepared = prepare_query(filename);
    let position = position_from_one_based(line, column);
    let result = prepared
        .engine
        .definition(&prepared.uri, position, &prepared.host)
        .await;

    if json_output {
        print_definition_json(result.as_ref());
    } else {
        print_definition_text(result.as_ref());
    }

    warn_on_compile_errors(&prepared.host, result.is_none());
}

/// Run document-highlight query and print results.
pub async fn run_document_highlight(filename: &str, line: u32, column: u32, json_output: bool) {
    let prepared = prepare_query(filename);
    let position = position_from_one_based(line, column);
    let highlights = prepared
        .engine
        .document_highlight(&prepared.uri, position, &prepared.host)
        .await;

    if json_output {
        print_highlights_json(&highlights);
    } else {
        print_highlights_text(filename, &highlights);
    }

    warn_on_compile_errors(&prepared.host, highlights.is_empty());
}

/// When a position-based query returns no results, the cause is ambiguous —
/// the cursor might genuinely be on nothing, or the file might have failed to
/// compile far enough to populate the symbol table. Surface the compiler
/// errors on stderr so users can distinguish the two without re-running
/// `wado query diagnostics`.
fn warn_on_compile_errors(host: &FilesystemCompilerHost, result_was_empty: bool) {
    if !result_was_empty || !host.has_errors() {
        return;
    }
    eprintln!("warning: result may be incomplete due to compile errors:");
    for d in host.diagnostics() {
        if !matches!(d.severity, wado_compiler::Severity::Error) {
            continue;
        }
        let location = d
            .span
            .as_ref()
            .map(|s| format!("{}:{}:{}", s.file, s.line, s.column))
            .unwrap_or_else(|| "<unknown>".to_string());
        eprintln!("  {location}: {}", d.message);
    }
}

fn severity_str(s: wado_lsp::Severity) -> &'static str {
    match s {
        wado_lsp::Severity::Error => "error",
        wado_lsp::Severity::Warning => "warning",
        wado_lsp::Severity::Information => "information",
        wado_lsp::Severity::Hint => "hint",
    }
}

fn print_diagnostics_json(filename: &str, diagnostics: &[wado_lsp::Diagnostic]) {
    let json_diags: Vec<serde_json::Value> = diagnostics
        .iter()
        .map(|d| {
            json!({
                "file": filename,
                "range": {
                    "start": { "line": d.range.start.line, "character": d.range.start.character },
                    "end": { "line": d.range.end.line, "character": d.range.end.character },
                },
                "severity": severity_str(d.severity),
                "code": d.code,
                "message": d.message,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json_diags).unwrap());
}

fn print_diagnostics_text(filename: &str, diagnostics: &[wado_lsp::Diagnostic]) {
    if diagnostics.is_empty() {
        println!("No diagnostics.");
        return;
    }
    for d in diagnostics {
        let severity = match d.severity {
            wado_lsp::Severity::Error => "error",
            wado_lsp::Severity::Warning => "warning",
            wado_lsp::Severity::Information => "info",
            wado_lsp::Severity::Hint => "hint",
        };
        println!(
            "{}:{}:{}: {}: {} [{}]",
            filename,
            d.range.start.line + 1,
            d.range.start.character + 1,
            severity,
            d.message,
            d.code,
        );
    }
}

fn uri_to_display(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

fn print_references_json(refs: &[ReferenceLocation]) {
    let json_refs: Vec<serde_json::Value> = refs
        .iter()
        .map(|r| {
            json!({
                "uri": r.uri,
                "range": {
                    "start": { "line": r.range.start.line, "character": r.range.start.character },
                    "end": { "line": r.range.end.line, "character": r.range.end.character },
                },
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json_refs).unwrap());
}

fn print_references_text(refs: &[ReferenceLocation]) {
    if refs.is_empty() {
        println!("No references.");
        return;
    }
    for r in refs {
        println!(
            "{}:{}:{}",
            uri_to_display(&r.uri),
            r.range.start.line + 1,
            r.range.start.character + 1,
        );
    }
}

fn print_definition_json(result: Option<&DefinitionResult>) {
    let value = match result {
        Some(r) => json!({
            "uri": r.uri,
            "range": {
                "start": { "line": r.range.start.line, "character": r.range.start.character },
                "end": { "line": r.range.end.line, "character": r.range.end.character },
            },
        }),
        None => serde_json::Value::Null,
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}

fn print_definition_text(result: Option<&DefinitionResult>) {
    match result {
        Some(r) => println!(
            "{}:{}:{}",
            uri_to_display(&r.uri),
            r.range.start.line + 1,
            r.range.start.character + 1,
        ),
        None => println!("No definition."),
    }
}

fn highlight_kind_str(kind: HighlightKind) -> &'static str {
    match kind {
        HighlightKind::Text => "text",
        HighlightKind::Read => "read",
        HighlightKind::Write => "write",
    }
}

fn print_highlights_json(highlights: &[DocumentHighlight]) {
    let json_hl: Vec<serde_json::Value> = highlights
        .iter()
        .map(|h| {
            json!({
                "range": {
                    "start": { "line": h.range.start.line, "character": h.range.start.character },
                    "end": { "line": h.range.end.line, "character": h.range.end.character },
                },
                "kind": highlight_kind_str(h.kind),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json_hl).unwrap());
}

fn print_highlights_text(filename: &str, highlights: &[DocumentHighlight]) {
    if highlights.is_empty() {
        println!("No highlights.");
        return;
    }
    for h in highlights {
        println!(
            "{}:{}:{}: {}",
            filename,
            h.range.start.line + 1,
            h.range.start.character + 1,
            highlight_kind_str(h.kind),
        );
    }
}
