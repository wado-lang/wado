use std::fs;
use std::path::Path;

use serde_json::json;
use wado_lsp::{
    DefinitionResult, DocumentHighlight, HighlightKind, HoverResult, Position, ReferenceLocation,
};

use wado_compiler::kiln::InvocationIndex;

use crate::args::CliExit;
use crate::compiler_host::FilesystemCompilerHost;

struct PreparedQuery {
    uri: String,
    engine: wado_lsp::Engine,
    host: FilesystemCompilerHost,
}

type ManifestPair = (wado_manifest::Manifest, std::path::PathBuf);

async fn prepare_query(filename: &str) -> Result<PreparedQuery, CliExit> {
    let path = Path::new(filename);
    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;

    // Derive every entry-relative fact — host base, manifest root, the kiln
    // pipeline's entry, and the loader's `decl_file` (via `Uri::to_filename`
    // of `uri`) — from one canonical path and one manifest load, the way
    // `wado compile` does. Mixing a relative path for the host with the
    // canonical path for the pipeline would seed the host's dependency index
    // from a different manifest root than the pipeline uses.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let manifest_pair = crate::compile::load_nearest_manifest(&canonical);
    let host = entry_host(&canonical, manifest_pair.as_ref());
    let uri = format!("file://{}", canonical.display());

    let mut engine = wado_lsp::Engine::new();
    match run_generators_for(&canonical, &host, manifest_pair).await {
        Some(invocations) => engine.open_document_with_invocations(&uri, source, invocations),
        None => engine.open_document(&uri, source),
    }

    Ok(PreparedQuery { uri, engine, host })
}

/// Silent, dependency-seeded host based at `entry_file`'s directory — the
/// base `load_source` joins a generator's relative inputs against. Mirrors
/// how `wado compile` hosts an entry file, so generators resolve their inputs
/// identically on the query path. `manifest_pair` must be the one
/// [`run_generators_for`] is given, so the host's dependency index and the
/// pipeline share a single manifest root.
fn entry_host(entry_file: &Path, manifest_pair: Option<&ManifestPair>) -> FilesystemCompilerHost {
    let base = entry_file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    crate::compile::attach_manifest_deps(
        FilesystemCompilerHost::silent(base.clone()),
        manifest_pair,
        &base,
    )
}

/// Run any kiln generators declared in `entry_file` natively (wasmtime) and
/// return the redirect index they produced, so a position/diagnostics query
/// resolves generated symbols even when no on-disk artifact existed before.
///
/// Writes generator outputs to disk exactly as `wado compile` does — the
/// pipeline is cache-aware, so a valid cache is reused rather than rebuilt.
/// Returns `None` on any pipeline failure; the caller then opens the document
/// without an override, degrading to consume-only on-disk discovery.
async fn run_generators_for(
    entry_file: &Path,
    host: &FilesystemCompilerHost,
    manifest_pair: Option<ManifestPair>,
) -> Option<InvocationIndex> {
    match crate::compile::maybe_run_pipeline(entry_file, host, false, manifest_pair).await {
        Ok(outcome) => Some(outcome.invocations),
        Err(e) => {
            eprintln!(
                "warning: kiln generators could not run ({e}); \
                 falling back to on-disk generated output"
            );
            None
        }
    }
}

fn position_from_one_based(line: u32, column: u32) -> Position {
    Position {
        line: line.saturating_sub(1),
        character: column.saturating_sub(1),
    }
}

pub async fn run_diagnostics(filename: &str, json_output: bool) -> Result<(), CliExit> {
    let prepared = prepare_query(filename).await?;
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
    let prepared = prepare_query(filename).await?;
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
    let prepared = prepare_query(filename).await?;
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

/// Parse notation + resolve the base directory, host, and synthetic entry URI.
struct SymbolEnv {
    parsed: wado_compiler::symbol_notation::SymbolNotation,
    host: FilesystemCompilerHost,
    base_dir: std::path::PathBuf,
    uri: String,
}

fn symbol_env(notation: &str, base: &str) -> Result<SymbolEnv, CliExit> {
    let parsed = wado_compiler::symbol_notation::parse(notation)
        .map_err(|e| CliExit::error(format!("invalid symbol notation: {e}")))?;
    // Reject a module string that isn't a valid module path now, with a clean
    // error — otherwise `normalize_module_path` (here and in the loader) would
    // panic on it downstream.
    wado_compiler::name::try_normalize_module_path(&parsed.module)
        .map_err(|e| CliExit::error(format!("invalid module '{}': {e}", parsed.module)))?;
    let base_dir = fs::canonicalize(base).unwrap_or_else(|_| Path::new(base).to_path_buf());
    // Synthetic entry: never read from disk (opened with explicit text), but
    // its directory anchors relative-module resolution at `base`.
    let entry_path = base_dir.join("__wado_query__.wado");
    let manifest_pair = crate::compile::load_nearest_manifest(&entry_path);
    let host = crate::compile::attach_manifest_deps(
        FilesystemCompilerHost::silent(base_dir.clone()),
        manifest_pair.as_ref(),
        &base_dir,
    );
    let uri = format!("file://{}", entry_path.display());
    Ok(SymbolEnv {
        parsed,
        host,
        base_dir,
        uri,
    })
}

/// Open an engine on a synthetic entry that `use`s each module in `imports`,
/// optionally with a precomputed kiln redirect index (see [`symbol_invocations`]).
fn open_entry(
    uri: &str,
    imports: &[String],
    invocations: Option<InvocationIndex>,
) -> wado_lsp::Engine {
    let mut synthetic = String::new();
    for (i, module) in imports.iter().enumerate() {
        synthetic.push_str(&format!("use __wado_q{i} from \"{module}\";\n"));
    }
    let mut engine = wado_lsp::Engine::new();
    match invocations {
        Some(index) => engine.open_document_with_invocations(uri, synthetic, index),
        None => engine.open_document(uri, synthetic),
    }
    engine
}

/// Run the generators declared in the notation's target module and return a
/// redirect index keyed the way the loader sees that module when the synthetic
/// entry imports it.
///
/// The kiln `with` clause lives in the *target* module, not the synthetic
/// entry. When the synthetic `EntryPoint` imports `./main.wado`, the loader
/// keys the redirect's `decl_file` by the normalized module string
/// (`normalize_module_path("./main.wado")` → `main.wado`), not a filesystem
/// path — but [`run_generators_for`] keys it by the target's path. So re-key
/// every entry from the target's path to that module string before injection.
///
/// `None` when the module is not a local on-disk file (e.g. `core:` / `wasi:`
/// / a dependency name) or the pipeline could not run, so the query degrades
/// to consume-only.
async fn symbol_invocations(env: &SymbolEnv) -> Option<InvocationIndex> {
    // `module_key` is the loader's `decl_file` for the target when the
    // synthetic entry imports it (same `normalize_module_path` the loader
    // applies to the import string). `target_abs` is the clean on-disk path
    // the pipeline runs against — canonicalize so an embedded `./` from the
    // module string can't corrupt the generator's relative input resolution.
    // `canonicalize` failing also rules out non-file modules (`core:` /
    // `wasi:` / a dependency name), so the query stays consume-only for them.
    let module_key = wado_compiler::name::normalize_module_path(&env.parsed.module);
    let target_abs = env.base_dir.join(&module_key).canonicalize().ok()?;
    // The generator's relative inputs resolve against its declaring file's
    // directory, so run it with a host based there — not `env.host`, which is
    // based at `base_dir` to resolve the synthetic entry's imports. Host and
    // pipeline share one manifest load, anchored at the target.
    let manifest_pair = crate::compile::load_nearest_manifest(&target_abs);
    let pipeline_host = entry_host(&target_abs, manifest_pair.as_ref());
    let raw = run_generators_for(&target_abs, &pipeline_host, manifest_pair).await?;
    let target_decl = target_abs.to_string_lossy();
    let mut translated = InvocationIndex::new();
    for (decl, from, uri) in raw.entries() {
        let decl = if decl == target_decl {
            module_key.as_str()
        } else {
            decl
        };
        translated.insert(decl, from, uri);
    }
    Some(translated)
}

/// Build a single-module query context (the notation's module only). Used by
/// `definition` / `hover` / `document-highlight`.
async fn prepare_symbol_query(notation: &str, base: &str) -> Result<SymbolQuery, CliExit> {
    let env = symbol_env(notation, base)?;
    let invocations = symbol_invocations(&env).await;
    let engine = open_entry(
        &env.uri,
        std::slice::from_ref(&env.parsed.module),
        invocations,
    );
    Ok(SymbolQuery {
        parsed: env.parsed,
        engine,
        host: env.host,
        uri: env.uri,
    })
}

/// Build a workspace query context for `references`: the synthetic entry `use`s
/// the target module plus every `.wado` under `base`, so use→def edges from
/// sibling files are recorded. The combined load is all-or-nothing, so if it
/// fails (one file the analyzer can't load — e.g. a compile-time-codegen module
/// without a build cache), re-import only the target and the files that analyze
/// on their own, dropping the offender.
async fn prepare_references_query(notation: &str, base: &str) -> Result<SymbolQuery, CliExit> {
    let env = symbol_env(notation, base)?;
    let invocations = symbol_invocations(&env).await;
    let target = env.parsed.module.clone();
    let workspace = workspace_module_specs(&env.base_dir);

    let mut engine = open_entry(
        &env.uri,
        &imports_with_target(&target, &workspace),
        invocations.clone(),
    );
    if !engine.analyzes(&env.uri, &env.host).await {
        let mut loadable = Vec::new();
        for spec in &workspace {
            if *spec != target && file_analyzes(&env.base_dir, &env.host, spec).await {
                loadable.push(spec.clone());
            }
        }
        engine = open_entry(
            &env.uri,
            &imports_with_target(&target, &loadable),
            invocations,
        );
    }

    Ok(SymbolQuery {
        parsed: env.parsed,
        engine,
        host: env.host,
        uri: env.uri,
    })
}

/// `[target, ...extra]` with `target` first and any duplicate of it dropped.
fn imports_with_target(target: &str, extra: &[String]) -> Vec<String> {
    let mut imports = vec![target.to_string()];
    imports.extend(extra.iter().filter(|s| s.as_str() != target).cloned());
    imports
}

/// Whether `spec` analyzes on its own (imported into a throwaway entry).
async fn file_analyzes(base_dir: &Path, host: &FilesystemCompilerHost, spec: &str) -> bool {
    let uri = format!("file://{}", base_dir.join("__wado_probe__.wado").display());
    let mut engine = wado_lsp::Engine::new();
    engine.open_document(&uri, format!("use __p from \"{spec}\";\n"));
    engine.analyzes(&uri, host).await
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
    let q = prepare_symbol_query(notation, base).await?;
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
    let q = prepare_references_query(notation, base).await?;
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
    let q = prepare_symbol_query(notation, base).await?;
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
    let prepared = prepare_query(filename).await?;
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
    let prepared = prepare_query(filename).await?;
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
    let q = prepare_symbol_query(notation, base).await?;
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
