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
    InvocationIndex, InvocationPath, collect_inline_invocations, content_hash, generator_identity,
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

    if metadata.options_hash != hash_options_canonical(&invocation.options_canonical) {
        return Err("options changed since last generation".to_string());
    }

    let current_identity = generator_identity(&invocation.module);
    if metadata.generator != current_identity {
        return Err(format!(
            "generator changed since last generation ({} → {})",
            metadata.generator, current_identity,
        ));
    }

    // `generator_source_hash` is recorded by the CLI driver from the
    // generator package's compiled component closure. The LSP runs in
    // consume-only mode (no provider, no compile of the generator) and
    // cannot recompute the current hash, so it cannot detect the
    // "schema/inputs unchanged but generator source updated" case.
    // Per WEP 2026-04-12 §"Transitional consume-only mode" this is the
    // documented gap that `wado check` exists to plug in CI.

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

    if !metadata.outputs.iter().any(|o| o.entry) {
        return Err("no entry output recorded in metadata".to_string());
    }

    let mut modified = Vec::new();
    let mut entry_abs: Option<PathBuf> = None;
    for output in &metadata.outputs {
        let Some(abs) = safe_join(manifest_root, &output.path) else {
            return Err(format!(
                "output path {:?} is absolute, contains `..`, or escapes the workspace",
                output.path,
            ));
        };
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("{} missing on disk", output.path));
            }
            Err(e) => {
                return Err(format!("cannot read {}: {e}", output.path));
            }
        };
        if hex_digest(&content_hash(&bytes)) != output.hash {
            modified.push(output.path.clone());
        }
        if output.entry {
            entry_abs = Some(abs);
        }
    }

    let abs_entry = entry_abs.expect("entry output presence verified above");
    let canonical = std::fs::canonicalize(&abs_entry).unwrap_or(abs_entry);
    Ok((path_to_kiln_uri(&canonical), modified))
}

/// Hash the bytes of `manifest_root/path` and compare against
/// `expected_hex`. Returns `false` on any I/O failure (file missing,
/// permission denied), on an absolute / `..`-containing `path`
/// (refused by [`safe_join`]), and on any hash mismatch — every such
/// case is a cache miss as far as the caller is concerned.
fn file_matches(manifest_root: &Path, path: &str, expected_hex: &str) -> bool {
    let Some(abs) = safe_join(manifest_root, path) else {
        return false;
    };
    let Ok(bytes) = std::fs::read(abs) else {
        return false;
    };
    hex_digest(&content_hash(&bytes)) == expected_hex
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
