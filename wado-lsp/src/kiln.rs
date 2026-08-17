//! Consume-only Kiln invocation index for LSP queries.
//!
//! The LSP runs without a Wasm runtime that can execute generator
//! components: native `wado-cli` provides one through wasmtime, but the
//! `wasm32-wasip2` LSP host (VS Code Wasm, browser playground) cannot.
//! Per WEP 2026-04-12 §"Consume-only mode", the LSP
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
/// Every file this reads or probes goes through the host; only path
/// normalisation ([`safe_join`], [`canonicalized`]) touches the filesystem, and
/// it degrades to the lexical form where there is none.
///
/// Returns an empty index when the entry has no inline `with` clauses
/// or no enclosing `wado.toml` is found.
pub async fn prepare_invocations<H: CompilerHost>(
    entry_filename: &str,
    entry_ast: &Module,
    host: &H,
) -> InvocationIndex {
    let entry_path = Path::new(entry_filename);
    let Some(manifest_root) = nearest_manifest_dir(entry_path, host).await else {
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
        // A malformed inline clause (e.g. a bare, non-`./` path) is reported
        // here: the semantics pass never re-runs this collector, so emitting
        // the diagnostics through the host is the only way they reach the
        // editor. No double-emit risk — this is the sole `collect` call on the
        // consume-only LSP path.
        Err(diags) => {
            for d in diags {
                host.emit_diagnostic(d);
            }
            return InvocationIndex::new();
        }
    };

    let mut index = InvocationIndex::new();
    for invocation in &invocations {
        let invocation_id = &invocation.decl_site.synthetic_id;
        match resolve_invocation(&manifest_root, invocation, host).await {
            Ok(entry_uri) => {
                // Key by the literal `from "<source>"` string: the loader looks
                // up redirects with the unresolved import path, while
                // `invocation.from` is resolved relative to the declaring file.
                index.insert(
                    &invocation.decl_site.module,
                    invocation.source.as_str(),
                    &entry_uri,
                );
            }
            Err(reason) => {
                let span = use_decl_span_for(entry_ast, &invocation.source, entry_filename);
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
async fn resolve_invocation<H: CompilerHost>(
    manifest_root: &Path,
    invocation: &wado_compiler::kiln::Invocation,
    host: &H,
) -> Result<String, String> {
    let Some(output_dir_abs) = safe_join(manifest_root, invocation.output_dir.as_str()) else {
        return Err(format!(
            "output_dir {:?} is absolute or contains `..`",
            invocation.output_dir.as_str(),
        ));
    };
    let metadata_path = output_dir_abs.join(metadata_filename(invocation.from.as_str()));

    let metadata_key = metadata_path.display().to_string();
    let bytes = match host.load_source(&metadata_key).await {
        Ok(bytes) => bytes,
        // An absent cache is the ordinary "not generated yet" case; one that is
        // there but will not read is a different thing to report. Presence is a
        // second question because a host may report both as the same
        // `SourceError` — `FilesystemCompilerHost` does.
        Err(_) if !host.source_exists(&metadata_key).await => {
            return Err(format!("no cache at {}", metadata_path.display()));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", metadata_path.display())),
    };
    let metadata: Metadata = serde_json::from_slice(&bytes)
        .map_err(|e| format!("failed to parse {}: {e}", metadata_path.display()))?;
    if metadata.version != METADATA_VERSION {
        return Err(format!(
            "metadata at {} has an unsupported version",
            metadata_path.display(),
        ));
    }

    if !metadata.outputs.iter().any(|o| o.entry) {
        return Err("no entry output recorded in metadata".to_string());
    }

    // The entry module imports its generated siblings by relative path, so a
    // missing one would fail the import opaquely; refusing the whole redirect
    // surfaces the actionable `re-run wado compile` hint instead. Presence and
    // path-sandbox only — not the hash validation consume-only skips, and not a
    // read: the loader is about to read these itself if the redirect fires.
    let mut entry_abs: Option<PathBuf> = None;
    for output in &metadata.outputs {
        let Some(abs) = safe_join(manifest_root, &output.path) else {
            return Err(format!(
                "output path {:?} is absolute, contains `..`, or escapes the workspace",
                output.path,
            ));
        };
        if !host.source_exists(&abs.display().to_string()).await {
            return Err(format!("{} missing on disk", output.path));
        }
        if output.entry {
            entry_abs = Some(abs);
        }
    }

    let abs_entry = entry_abs.expect("entry output presence verified above");
    Ok(path_to_kiln_uri(&canonicalized(abs_entry)))
}

/// The nearest ancestor of `start` whose `wado.toml` the host can load.
///
/// A relative `start` — every non-`file:` document URI — has no workspace to
/// anchor against and yields `None` rather than walking the process's cwd.
async fn nearest_manifest_dir<H: CompilerHost>(start: &Path, host: &H) -> Option<PathBuf> {
    if !start.is_absolute() {
        return None;
    }
    let mut dir = start.to_path_buf();
    while dir.pop() {
        let manifest = dir.join(wado_workspace::MANIFEST_FILENAME);
        if host.source_exists(&manifest.display().to_string()).await {
            return Some(dir);
        }
    }
    None
}

/// `path` with symlinks resolved, or `path` itself where that cannot be done.
/// Mirrors `wado-cli`'s kiln driver so both spell the same file the same way.
fn canonicalized(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
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
