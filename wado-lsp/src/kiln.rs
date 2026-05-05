//! Consume-only Kiln invocation index for LSP queries.
//!
//! The LSP runs without a Wasm runtime that can execute generator
//! components: native `wado-cli` provides one through wasmtime, but the
//! `wasm32-wasip2` LSP host (VS Code Wasm, browser playground) cannot.
//! Per WEP 2026-04-12 §"Transitional consume-only mode", the LSP still
//! redirects `use { ... } from "<schema>"` clauses to the on-disk
//! generator output as long as the recorded `<output_dir>/<primary>.kiln.json`
//! still matches the schema files on disk.
//!
//! [`prepare_invocations`] does exactly that: it parses the entry
//! source (in-memory, no extra I/O on the source itself), walks up to
//! the nearest `wado.toml` to find the manifest root, collects every
//! inline `with { generator: { ... } }` clause, and for each invocation
//! either:
//!
//! - registers `(decl_file, from) → kiln:/abs/path/to/entry.wado` in the
//!   returned [`InvocationIndex`] when the cache file is present and
//!   every recorded hash matches what is on disk, or
//! - emits [`Code::KilnStaleCache`] via `host.emit_diagnostic` and
//!   leaves the entry unregistered, so the import surfaces as a normal
//!   resolution error and the user sees a clear "re-run `wado compile`"
//!   hint.
//!
//! The helper performs no writes — `<primary>.kiln.json` and the
//! generated outputs are only read.

use std::path::{Path, PathBuf};

use wado_compiler::ast::{Item, Module};
use wado_compiler::kiln::metadata::{METADATA_VERSION, Metadata, metadata_filename};
use wado_compiler::kiln::{
    InvocationIndex, InvocationPath, collect_inline_invocations, content_hash,
    hash_options_canonical, hex_digest,
};
use wado_compiler::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};

/// Build a consume-only [`InvocationIndex`] for the entry document.
///
/// `entry_filename` must match the `filename` argument that
/// [`wado_compiler::annotate_with_invocations`] receives downstream;
/// otherwise the compiler-side redirect lookup misses. `Engine::*` query
/// methods feed the URI through `uri_to_filename` and pass that string
/// to both this helper and the annotator.
///
/// Returns an empty index when the entry has no inline `with` clauses,
/// when no enclosing `wado.toml` is found, or when the source cannot be
/// parsed (the regular compile pass surfaces the parse error
/// downstream — we don't need to report it twice).
pub fn prepare_invocations<H: CompilerHost>(
    entry_filename: &str,
    source: &str,
    host: &H,
) -> InvocationIndex {
    let entry_path = Path::new(entry_filename);
    let Some(manifest_root) = find_manifest_root(entry_path) else {
        return InvocationIndex::new();
    };

    let Ok(parsed) = wado_compiler::parse(source) else {
        return InvocationIndex::new();
    };

    let mut modules = wado_compiler::hashmap::IndexMap::default();
    modules.insert(entry_filename.to_string(), parsed.ast);
    let descriptors = wado_compiler::hashmap::IndexMap::default();
    let manifest_root_str = manifest_root.to_string_lossy();
    let invocations = match collect_inline_invocations(&modules, &descriptors, &manifest_root_str) {
        Ok(v) => v,
        // Inline-clause errors are surfaced by the regular
        // `annotate` pass (it re-runs the same collector). We
        // silently fall through here so we don't double-emit.
        Err(_) => return InvocationIndex::new(),
    };

    let entry_module = modules
        .get(entry_filename)
        .expect("entry module was just inserted");
    let mut index = InvocationIndex::new();
    for invocation in &invocations {
        let invocation_id = &invocation.decl_site.synthetic_id;
        match resolve_invocation(&manifest_root, invocation) {
            Ok((entry_uri, modified)) => {
                index.insert(
                    &invocation.decl_site.module,
                    invocation.from.as_str(),
                    &entry_uri,
                );
                for path in &modified {
                    let span = use_decl_span_for(entry_module, &invocation.from, entry_filename);
                    emit_modified(host, invocation_id, path, span);
                }
            }
            Err(reason) => {
                let span = use_decl_span_for(entry_module, &invocation.from, entry_filename);
                emit_stale(host, invocation_id, &reason, span);
            }
        }
    }
    index
}

/// Locate the [`UseDecl::source_span`] of the use clause whose normalized
/// `from` matches the given invocation path. Returns a synthetic
/// file-anchored span when no match is found — every invocation came
/// from a use clause, so a miss here is a defensive fallback only.
fn use_decl_span_for(module: &Module, from: &InvocationPath, filename: &str) -> DiagnosticSpan {
    for item in &module.items {
        if let Item::Use(use_decl) = item
            && InvocationPath::normalize(&use_decl.source).as_str() == from.as_str()
        {
            return DiagnosticSpan::from_span(&use_decl.source_span, Some(filename));
        }
    }
    DiagnosticSpan {
        file: filename.to_string(),
        line: 1,
        column: 1,
        end_line: Some(1),
        end_column: Some(1),
    }
}

/// Validate the cached metadata against on-disk state.
///
/// On hit returns the `kiln:/abs/path` URI of the generated entry
/// module plus the project-root-relative paths of any output files
/// whose bytes drifted from `metadata.outputs[].hash` (hit-but-modified
/// — the user hand-edited the generated `.wado`; honor the edit but
/// surface it via [`Code::KilnGeneratedModified`] at the call site).
/// On miss returns a human-readable reason for [`Code::KilnStaleCache`].
fn resolve_invocation(
    manifest_root: &Path,
    invocation: &wado_compiler::kiln::Invocation,
) -> Result<(String, Vec<String>), String> {
    let metadata_path = manifest_root
        .join(invocation.output_dir.as_str())
        .join(metadata_filename(invocation.from.as_str()));

    let metadata: Metadata = match std::fs::read_to_string(&metadata_path) {
        Ok(s) => match serde_json::from_str::<Metadata>(&s) {
            Ok(m) if m.version == METADATA_VERSION => m,
            Ok(_) => {
                return Err(format!(
                    "metadata at {} has an unsupported version",
                    metadata_path.display(),
                ));
            }
            Err(e) => {
                return Err(format!("failed to parse {}: {e}", metadata_path.display()));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("no cache at {}", metadata_path.display()));
        }
        Err(e) => {
            return Err(format!("cannot read {}: {e}", metadata_path.display()));
        }
    };

    if metadata.options_hash != hash_options_canonical(&invocation.options_canonical) {
        return Err("options changed since last generation".to_string());
    }

    if metadata.primary.path != invocation.from.as_str() {
        return Err("primary input path changed since last generation".to_string());
    }
    if !file_matches(
        manifest_root,
        &metadata.primary.path,
        &metadata.primary.hash,
    ) {
        return Err(format!("{} changed on disk", metadata.primary.path));
    }

    if metadata.inputs.len() != invocation.inputs.len() {
        return Err("inputs list changed since last generation".to_string());
    }
    for (declared, recorded) in invocation.inputs.iter().zip(&metadata.inputs) {
        if declared.as_str() != recorded.path {
            return Err("inputs list changed since last generation".to_string());
        }
        if !file_matches(manifest_root, &recorded.path, &recorded.hash) {
            return Err(format!("{} changed on disk", recorded.path));
        }
    }

    // `reads` are the transitive `host::read-file` pickups from the
    // last generator run (e.g. a `.proto` import or a `.g4` lexer
    // grammar). They're not in `invocation.inputs` so the user can't
    // see them at the call site, but a change to one still invalidates
    // the cache. CLI parity: see `wado_cli::kiln_driver::cache_matches`.
    for read in &metadata.reads {
        let normalized = InvocationPath::normalize(&read.path);
        if !file_matches(manifest_root, normalized.as_str(), &read.hash) {
            return Err(format!(
                "{} (read-file dependency) changed on disk",
                read.path
            ));
        }
    }

    let entry_output = metadata
        .outputs
        .iter()
        .find(|o| o.entry)
        .ok_or_else(|| "no entry output recorded in metadata".to_string())?;

    let abs_entry = manifest_root.join(&entry_output.path);
    if !abs_entry.is_file() {
        return Err(format!("{} missing on disk", entry_output.path));
    }

    let mut modified = Vec::new();
    for output in &metadata.outputs {
        if !file_matches(manifest_root, &output.path, &output.hash) {
            modified.push(output.path.clone());
        }
    }

    let canonical = std::fs::canonicalize(&abs_entry).unwrap_or(abs_entry);
    Ok((path_to_kiln_uri(&canonical), modified))
}

fn file_matches(manifest_root: &Path, path: &str, expected_hex: &str) -> bool {
    let Ok(bytes) = std::fs::read(manifest_root.join(path)) else {
        return false;
    };
    hex_digest(&content_hash(&bytes)) == expected_hex
}

/// Walk up from `entry_path`'s directory looking for the nearest
/// `wado.toml`. Returns the directory that contains it — the kiln
/// pipeline's `manifest_root`.
fn find_manifest_root(entry_path: &Path) -> Option<PathBuf> {
    let mut dir = entry_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if dir.as_os_str().is_empty() {
        dir = PathBuf::from(".");
    }
    loop {
        if dir.join("wado.toml").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Compose the `kiln:/abs/path` URI used by [`InvocationIndex`].
///
/// Mirrors `wado_cli::kiln_driver::path_to_kiln_uri`: the LSP and CLI
/// both produce these URIs independently; the compiler-side loader
/// strips the `kiln:` scheme and forwards the absolute path to
/// `CompilerHost::load_source`. Using the same scheme on both sides
/// means a cached entry written by `wado compile` and consumed by the
/// LSP resolves identically. See WEP 2026-04-12 §"URI scheme" for why
/// the scheme is `kiln:` and not `file:`.
fn path_to_kiln_uri(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/");
    if s.starts_with('/') {
        format!("kiln:{s}")
    } else {
        format!("kiln:/{s}")
    }
}

fn emit_stale<H: CompilerHost>(host: &H, invocation_id: &str, reason: &str, span: DiagnosticSpan) {
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::KilnStaleCache,
        message: format!(
            "kiln[{invocation_id}]: stale cache ({reason}); \
             re-run `wado compile` natively to refresh.",
        ),
        span: Some(span),
    });
}

fn emit_modified<H: CompilerHost>(host: &H, invocation_id: &str, path: &str, span: DiagnosticSpan) {
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::KilnGeneratedModified,
        message: format!(
            "kiln[{invocation_id}]: {path} has been modified after generation; \
             the on-disk content is honored, but `wado check` will fail. \
             Run `wado compile` (or delete the file) to regenerate.",
        ),
        span: Some(span),
    });
}
