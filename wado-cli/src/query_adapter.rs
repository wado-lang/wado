use std::fs;
use std::path::Path;
use std::process;

use serde_json::json;

use crate::compiler_host::FilesystemCompilerHost;

/// Run diagnostics query and print results.
pub async fn run_diagnostics(filename: &str, json_output: bool) {
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

    let diagnostics = engine.diagnostics(&uri, &host).await;

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
        // Display as 1-based line/column for human readability
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
