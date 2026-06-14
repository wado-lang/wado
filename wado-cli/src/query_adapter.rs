use std::fs;
use std::path::Path;

use serde_json::json;
use wado_lsp::{
    DefinitionResult, DocumentHighlight, HighlightKind, HoverResult, Position, ReferenceLocation,
};

use crate::args::CliExit;
use crate::compiler_host::FilesystemCompilerHost;

struct PreparedQuery {
    uri: String,
    engine: wado_lsp::Engine,
    host: FilesystemCompilerHost,
}

fn prepare_query(filename: &str) -> Result<PreparedQuery, CliExit> {
    let path = Path::new(filename);
    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;

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

    Ok(PreparedQuery { uri, engine, host })
}

fn position_from_one_based(line: u32, column: u32) -> Position {
    Position {
        line: line.saturating_sub(1),
        character: column.saturating_sub(1),
    }
}

pub async fn run_diagnostics(filename: &str, json_output: bool) -> Result<(), CliExit> {
    let prepared = prepare_query(filename)?;
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
        return Err(CliExit::silent_failure(1));
    }
    Ok(())
}

pub async fn run_references(
    filename: &str,
    line: u32,
    column: u32,
    include_declaration: bool,
    json_output: bool,
) -> Result<(), CliExit> {
    let prepared = prepare_query(filename)?;
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
    Ok(())
}

pub async fn run_definition(
    filename: &str,
    line: u32,
    column: u32,
    json_output: bool,
) -> Result<(), CliExit> {
    let prepared = prepare_query(filename)?;
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
    Ok(())
}

/// A synthetic-entry analysis context for a symbol-notation query.
struct SymbolQuery {
    parsed: wado_compiler::symbol_notation::SymbolNotation,
    engine: wado_lsp::Engine,
    host: FilesystemCompilerHost,
    uri: String,
}

/// Parse the notation and build a synthetic entry that `use`s its module,
/// anchored at `base` so relative modules resolve from there (`core:` /
/// `wasi:` are location-independent).
///
/// When `load_workspace`, the entry also `use`s every `.wado` file under
/// `base`, so the resulting `Semantics` records use→def edges from the whole
/// workspace — required for `references` to span more than the target module's
/// own import graph.
fn prepare_symbol_query(
    notation: &str,
    base: &str,
    load_workspace: bool,
) -> Result<SymbolQuery, CliExit> {
    let parsed = wado_compiler::symbol_notation::parse(notation)
        .map_err(|e| CliExit::error(format!("invalid symbol notation: {e}")))?;

    let base_dir = fs::canonicalize(base).unwrap_or_else(|_| Path::new(base).to_path_buf());
    let host = FilesystemCompilerHost::silent(base_dir.clone());
    // Synthetic entry: never read from disk (opened with explicit text), but
    // its directory anchors relative-module resolution at `base`.
    let entry_path = base_dir.join("__wado_query__.wado");
    let uri = format!("file://{}", entry_path.display());

    let mut synthetic = format!("use __wado_query_ns from \"{}\";\n", parsed.module);
    if load_workspace {
        for (i, spec) in workspace_module_specs(&base_dir).into_iter().enumerate() {
            if spec == parsed.module {
                continue;
            }
            synthetic.push_str(&format!("use __wado_ws_{i} from \"{spec}\";\n"));
        }
    }

    let mut engine = wado_lsp::Engine::new();
    engine.open_document(&uri, synthetic);

    Ok(SymbolQuery {
        parsed,
        engine,
        host,
        uri,
    })
}

/// Relative module specs (`./sub/x.wado`) for every `.wado` file under `root`,
/// sorted. Skips VCS/build directories and the synthetic query entry.
fn workspace_module_specs(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if name.starts_with('.')
                    || matches!(name.as_ref(), "build" | "target" | "node_modules")
                {
                    continue;
                }
                walk(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "wado")
                && name != "__wado_query__.wado"
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(format!("./{}", rel.to_string_lossy().replace('\\', "/")));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Report a failed symbol resolution on stderr. For a missing symbol, list the
/// module's public symbols as suggestions.
fn report_symbol_error(
    parsed: &wado_compiler::symbol_notation::SymbolNotation,
    reason: &wado_lsp::SymbolQueryError,
) {
    match reason {
        wado_lsp::SymbolQueryError::NotFound { available } => {
            // Member miss (`Type::m`) lists the type's members; a free miss
            // lists the module's public symbols.
            let (subject, listing) = match &parsed.receiver {
                Some(r) => (
                    format!("no member '{}' on {}", parsed.member, r.type_name),
                    format!("members of {}", r.type_name),
                ),
                None => (
                    format!("no symbol '{}' in {}", parsed.member, parsed.module),
                    format!("public symbols in {}", parsed.module),
                ),
            };
            eprintln!("warning: {subject}");
            if available.is_empty() {
                eprintln!("note: none found");
            } else {
                eprintln!("note: {listing}:");
                for name in available {
                    eprintln!("  {name}");
                }
            }
        }
        other => eprintln!("warning: {other}"),
    }
}

/// Print a by-symbol query result, or — on error — print an empty result and
/// the failure reason (with suggestions). Shared by every `run_*_by_symbol`.
fn emit_symbol_result<T>(
    parsed: &wado_compiler::symbol_notation::SymbolNotation,
    result: Result<T, wado_lsp::SymbolQueryError>,
    on_ok: impl FnOnce(T),
    on_empty: impl FnOnce(),
) {
    match result {
        Ok(value) => on_ok(value),
        Err(reason) => {
            on_empty();
            report_symbol_error(parsed, &reason);
        }
    }
}

/// Resolve a symbol notation (`MODULE#SYMBOL`) to its definition location.
/// Mirrors [`run_definition`] output.
pub async fn run_definition_by_symbol(
    notation: &str,
    base: &str,
    public_only: bool,
    json_output: bool,
) -> Result<(), CliExit> {
    let q = prepare_symbol_query(notation, base, false)?;
    let result = q
        .engine
        .definition_by_symbol(&q.uri, &q.parsed, public_only, &q.host)
        .await;
    emit_symbol_result(
        &q.parsed,
        result,
        |def| {
            if json_output {
                print_definition_json(Some(&def));
            } else {
                print_definition_text(Some(&def));
            }
        },
        || {
            if json_output {
                print_definition_json(None);
            } else {
                print_definition_text(None);
            }
        },
    );
    Ok(())
}

/// Find all references to a symbol notation. Mirrors [`run_references`] output.
pub async fn run_references_by_symbol(
    notation: &str,
    base: &str,
    include_declaration: bool,
    public_only: bool,
    json_output: bool,
) -> Result<(), CliExit> {
    // References span the whole workspace, so load every `.wado` under `base`.
    let q = prepare_symbol_query(notation, base, true)?;
    let result = q
        .engine
        .references_by_symbol(&q.uri, &q.parsed, include_declaration, public_only, &q.host)
        .await;
    emit_symbol_result(
        &q.parsed,
        result,
        |refs| {
            if json_output {
                print_references_json(&refs);
            } else {
                print_references_text(&refs);
            }
        },
        || {
            if json_output {
                print_references_json(&[]);
            } else {
                print_references_text(&[]);
            }
        },
    );
    Ok(())
}

/// Highlight occurrences of a symbol notation within its defining module.
/// Mirrors [`run_document_highlight`] output, using the defining module's URI.
pub async fn run_document_highlight_by_symbol(
    notation: &str,
    base: &str,
    public_only: bool,
    json_output: bool,
) -> Result<(), CliExit> {
    let q = prepare_symbol_query(notation, base, false)?;
    let result = q
        .engine
        .document_highlight_by_symbol(&q.uri, &q.parsed, public_only, &q.host)
        .await;
    emit_symbol_result(
        &q.parsed,
        result,
        |(def_uri, highlights)| {
            if json_output {
                print_highlights_json(&highlights);
            } else {
                print_highlights_text(uri_to_display(&def_uri), &highlights);
            }
        },
        || {
            if json_output {
                print_highlights_json(&[]);
            } else {
                print_highlights_text("", &[]);
            }
        },
    );
    Ok(())
}

pub async fn run_document_highlight(
    filename: &str,
    line: u32,
    column: u32,
    json_output: bool,
) -> Result<(), CliExit> {
    let prepared = prepare_query(filename)?;
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
    Ok(())
}

pub async fn run_hover(
    filename: &str,
    line: u32,
    column: u32,
    public_only: bool,
    json_output: bool,
) -> Result<(), CliExit> {
    let prepared = prepare_query(filename)?;
    let position = position_from_one_based(line, column);
    let result = prepared
        .engine
        .hover_with(&prepared.uri, position, public_only, &prepared.host)
        .await;

    if json_output {
        print_hover_json(result.as_ref());
    } else {
        print_hover_text(result.as_ref());
    }

    warn_on_compile_errors(&prepared.host, result.is_none());
    Ok(())
}

/// Show the signature of a symbol named by notation. Mirrors [`run_hover`].
pub async fn run_hover_by_symbol(
    notation: &str,
    base: &str,
    public_only: bool,
    json_output: bool,
) -> Result<(), CliExit> {
    let q = prepare_symbol_query(notation, base, false)?;
    let result = q
        .engine
        .hover_by_symbol(&q.uri, &q.parsed, public_only, &q.host)
        .await;
    emit_symbol_result(
        &q.parsed,
        result,
        |hover| {
            if json_output {
                print_hover_json(Some(&hover));
            } else {
                print_hover_text(Some(&hover));
            }
        },
        || {
            if json_output {
                print_hover_json(None);
            } else {
                print_hover_text(None);
            }
        },
    );
    Ok(())
}

/// An empty position-based result is ambiguous (cursor on nothing vs.
/// symbol table never populated). Surface the underlying errors so the
/// user doesn't need to re-run `wado query diagnostics` to find out.
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

/// Strip the ```` ```wado ```` code fence from a hover value for terminal output.
fn hover_plaintext(value: &str) -> String {
    let v = value.trim();
    let v = v.strip_prefix("```wado").unwrap_or(v);
    let v = v.strip_prefix('\n').unwrap_or(v);
    let v = v.strip_suffix("```").unwrap_or(v);
    v.trim().to_string()
}

fn print_hover_text(result: Option<&HoverResult>) {
    match result {
        Some(h) => println!("{}", hover_plaintext(&h.contents.value)),
        None => println!("No hover."),
    }
}

fn print_hover_json(result: Option<&HoverResult>) {
    let value = match result {
        Some(h) => serde_json::to_value(h).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
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
