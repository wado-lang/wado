//! Consume-only Kiln invocation index for LSP queries.
//!
//! The LSP runs without a Wasm runtime that can execute generator
//! components: native `wado-cli` provides one through wasmtime, but the
//! `wasm32-wasip2` LSP host (VS Code Wasm, browser playground) cannot.
//! Per WEP 2026-04-12 §"Transitional consume-only mode", the LSP
//! redirects `use { ... } from "<schema>"` clauses to the on-disk
//! generator output recorded in `<output_dir>/<primary>.kiln.json`.
//!
//! Because the consume-only host cannot run the generator, it cannot
//! re-derive any of the hashes the metadata records, so validating them
//! would only make the redirect silently die and break every query over
//! a generated symbol. Consume-only therefore **trusts the on-disk
//! artifact**: if the metadata records an entry output that still exists
//! (and stays within the workspace), the redirect fires regardless of
//! schema / options / generator / reads / output-byte drift. Verifying
//! that drift is `wado check`'s job (native, runtime-backed).
//!
//! [`prepare_invocations`] does this: it parses the entry source
//! (in-memory, no extra I/O on the source itself), walks up to the
//! nearest `wado.toml` to find the manifest root, collects every inline
//! `with { generator: { ... } }` clause, and for each invocation either:
//!
//! - registers `(decl_file, from) → kiln:/abs/path/to/entry.wado` in the
//!   returned [`InvocationIndex`] when the metadata records an entry
//!   output that exists on disk, or
//! - emits [`Code::KilnStaleCache`] via `host.emit_diagnostic` and
//!   leaves the entry unregistered (no metadata, no entry recorded, or
//!   the output is missing / escapes the workspace), so the import
//!   surfaces as a normal resolution error and the user sees a clear
//!   "re-run `wado compile`" hint.
//!
//! The helper performs no writes — `<primary>.kiln.json` and the
//! generated outputs are only read.

use std::path::{Path, PathBuf};

use wado_compiler::ast::{Item, Module};
use wado_compiler::kiln::metadata::{METADATA_VERSION, Metadata, metadata_filename};
use wado_compiler::kiln::{InvocationIndex, InvocationPath, collect_inline_invocations};
use wado_compiler::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};

/// Build a consume-only [`InvocationIndex`] for the entry document.
///
/// `entry_filename` must match the `filename` argument the semantics
/// pipeline receives downstream; otherwise the compiler-side redirect
/// lookup misses. `Engine::*` query methods feed the URI through
/// `uri_to_filename` and pass that string to both this helper and the
/// downstream `wado_compiler::load` call.
///
/// Takes `&Module` (the parsed entry AST) instead of source bytes so
/// the LSP can share the parse result with the downstream load stage —
/// see `Engine::snapshot` for the shared-parse flow. **Contract**:
/// `entry_ast` must derive from the same bytes the caller subsequently
/// passes to `wado_compiler::load`; otherwise the spans this helper
/// emits via [`Code::KilnStaleCache`] will point at locations that
/// don't exist in the source the rest of the snapshot is built against.
///
/// Returns an empty index when the entry has no inline `with` clauses
/// or no enclosing `wado.toml` is found.
pub fn prepare_invocations<H: CompilerHost>(
    entry_filename: &str,
    entry_ast: &Module,
    host: &H,
) -> InvocationIndex {
    let entry_path = Path::new(entry_filename);
    let Some(manifest_root) = find_manifest_root(entry_path) else {
        return InvocationIndex::new();
    };

    let descriptors = wado_compiler::hashmap::IndexMap::default();
    let manifest_root_str = manifest_root.to_string_lossy();
    let invocations = match collect_inline_invocations(
        std::iter::once((entry_filename, entry_ast)),
        &descriptors,
        &manifest_root_str,
    ) {
        Ok(v) => v,
        // Inline-clause errors are surfaced by the regular
        // semantics pass (it re-runs the same collector). We
        // silently fall through here so we don't double-emit.
        Err(_) => return InvocationIndex::new(),
    };

    let mut index = InvocationIndex::new();
    for invocation in &invocations {
        let invocation_id = &invocation.decl_site.synthetic_id;
        match resolve_invocation(&manifest_root, invocation) {
            Ok(entry_uri) => {
                index.insert(
                    &invocation.decl_site.module,
                    invocation.from.as_str(),
                    &entry_uri,
                );
            }
            Err(reason) => {
                let span = use_decl_span_for(entry_ast, &invocation.from, entry_filename);
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

/// Resolve the on-disk generated entry module for an invocation.
///
/// Consume-only mode trusts the artifact: it does not validate any of
/// the recorded hashes (it cannot re-derive them without running the
/// generator). On success returns the `kiln:/abs/path` URI of the entry
/// module recorded in `<output_dir>/<primary>.kiln.json`. On miss —
/// no metadata, no entry output recorded, or an output that is missing
/// or escapes the workspace — returns a human-readable reason for
/// [`Code::KilnStaleCache`].
fn resolve_invocation(
    manifest_root: &Path,
    invocation: &wado_compiler::kiln::Invocation,
) -> Result<String, String> {
    let Some(output_dir_abs) = safe_join(manifest_root, invocation.output_dir.as_str()) else {
        return Err(format!(
            "output_dir {:?} is absolute or contains `..`",
            invocation.output_dir.as_str(),
        ));
    };
    let metadata_path = output_dir_abs.join(metadata_filename(invocation.from.as_str()));

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

    if !metadata.outputs.iter().any(|o| o.entry) {
        return Err("no entry output recorded in metadata".to_string());
    }

    // Every recorded output must still exist within the workspace: the entry
    // module typically imports its generated siblings by relative path, so a
    // missing sibling would fail the import with an opaque error. Refusing the
    // whole redirect here surfaces the actionable `re-run wado compile` hint
    // instead. This is an existence (and path-sandbox) check only — not the
    // freshness/hash validation consume-only deliberately skips.
    let mut entry_abs: Option<PathBuf> = None;
    for output in &metadata.outputs {
        let Some(abs) = safe_join(manifest_root, &output.path) else {
            return Err(format!(
                "output path {:?} is absolute, contains `..`, or escapes the workspace",
                output.path,
            ));
        };
        if !abs.exists() {
            return Err(format!("{} missing on disk", output.path));
        }
        if output.entry {
            entry_abs = Some(abs);
        }
    }

    let abs_entry = entry_abs.expect("entry output presence verified above");
    let canonical = std::fs::canonicalize(&abs_entry).unwrap_or(abs_entry);
    Ok(path_to_kiln_uri(&canonical))
}

/// Join `rel` onto `manifest_root` while refusing absolute paths,
/// `..` traversal, and symlink escapes out of the workspace. Defends
/// against a crafted inline `output_dir` or a tampered
/// `<primary>.kiln.json` making the LSP read or hash arbitrary files
/// outside the workspace.
///
/// Returns `None` for any path the caller should treat as a cache miss
/// (or, in the case of the `metadata_path` build, a hard validation
/// error). Callers do not need to repeat this check on the resulting
/// `PathBuf`.
///
/// Symlink check: when both `manifest_root` and the joined path
/// canonicalize successfully, the joined canonical path must remain
/// under the canonical root. When the joined path does not exist yet
/// (canonicalize fails) we keep the lexical join — the textual
/// `..`/absolute checks above already rule out the obvious escapes,
/// and a subsequent read on a non-existent file is harmless cache
/// miss. There is no TOCTOU defense beyond this; callers operate
/// read-only.
fn safe_join(manifest_root: &Path, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    for c in rel_path.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let joined = manifest_root.join(rel_path);
    if let (Ok(canon_root), Ok(canon_joined)) = (
        std::fs::canonicalize(manifest_root),
        std::fs::canonicalize(&joined),
    ) && !canon_joined.starts_with(&canon_root)
    {
        return None;
    }
    Some(joined)
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

/// Compose the `kiln:` redirect URI used by [`InvocationIndex`].
///
/// Thin wrapper over [`wado_compiler::loader::path_to_kiln_uri`] (the single
/// producer shared with the CLI), so cache entries written by `wado compile`
/// resolve identically in the LSP.
fn path_to_kiln_uri(path: &Path) -> String {
    wado_compiler::loader::path_to_kiln_uri(&path.display().to_string())
}

fn emit_stale<H: CompilerHost>(host: &H, invocation_id: &str, reason: &str, span: DiagnosticSpan) {
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::KilnStaleCache,
        message: format!(
            "kiln[{invocation_id}]: no generated output found ({reason}); \
             re-run `wado compile` natively to generate it.",
        ),
        span: Some(span),
    });
}
