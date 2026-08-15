//! Shared helpers for translating between compiler positions/symbols and LSP
//! types. Used by go-to-definition, find-references, and document highlight.
//!
//! URI handling lives in [`crate::uri`]; position-encoding-aware span
//! conversion lives in [`crate::text`]. This module only retains the
//! `ModuleSource → URI` helpers that read the compiler's per-module
//! metadata.

use wado_compiler::module_source::ModuleSource;
use wado_compiler::symbol::Symbol;

use crate::uri::Uri;

/// Resolve the URI of a module relative to the requesting document's URI.
///
/// `request_uri` provides the base directory used to anchor `./relative.wado`
/// imports. Returns the request URI itself when the module is the entry point.
pub(crate) fn module_uri(
    entry: &ModuleSource,
    module: &ModuleSource,
    request_uri: &str,
) -> Option<String> {
    if module == entry {
        return Some(request_uri.to_string());
    }
    match module {
        ModuleSource::EntryPoint { filename } => Some(filename_to_uri(filename)),
        ModuleSource::Local { path } | ModuleSource::Dependency { path } => {
            Some(resolve_local_uri(path, request_uri))
        }
        ModuleSource::Core { name } => Some(format!("core:{name}")),
        ModuleSource::Wasi { interface } => Some(format!("wasi:{interface}")),
        ModuleSource::Remote { url } => Some(url.to_string()),
        // Kiln-redirected modules already carry a fully-qualified URI;
        // hand it to the LSP client unchanged.
        ModuleSource::Redirected { uri } => Some(uri.to_string()),
        // Wasm assets (`.wat`/`.wasm` imported via `with { type: ... }`)
        // expose their canonical path; opening these in the editor isn't
        // useful (binary `.wasm`) and stdlib `.wat` paths can't be served
        // until `workspace/textDocumentContent` lands. Emit the path so
        // CLI consumers can still see the reference site.
        ModuleSource::Wasm { path, .. } => Some(path.to_string()),
    }
}

/// Wrap an absolute path as a `file:` URI, percent-encoding on the way out —
/// the paths reaching here are decoded, from `Uri::to_filename` or from the
/// compiler. A string that is already a URI is left alone, as is a relative
/// path or a non-`file:` scheme.
fn filename_to_uri(filename: &str) -> String {
    if filename.starts_with("file://") {
        filename.to_string()
    } else if filename.starts_with('/') {
        format!("file://{}", crate::uri::percent_encode_path(filename))
    } else {
        filename.to_string()
    }
}

fn resolve_local_uri(module_path: &str, request_uri: &str) -> String {
    if module_path.starts_with('/') || module_path.starts_with("file://") {
        return filename_to_uri(&normalize_dot_segments(module_path));
    }
    // String-based parent extraction. `Uri::to_filename` is a passthrough
    // for non-`file:` schemes, so for `kiln:/abs/path/foo.wado` we get
    // `kiln:/abs/path/foo.wado` back and `rsplit_once('/')` yields the
    // sibling-import-aware base `kiln:/abs/path`. For schemes with no
    // path component (`core:cli`, `wasi:filesystem/types.wado` *with*
    // a path, `untitled:1`), `rsplit_once` either returns the right
    // thing or yields an empty `base_dir` and we fall back to the bare
    // module path.
    let request_path = Uri::new(request_uri).to_filename();
    let base_dir = request_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    // When the request path is rooted at "/" (e.g. "/test.wado"), rsplit_once
    // yields an empty base_dir, so preserve the leading slash explicitly.
    let joined = if base_dir.is_empty() {
        if request_path.starts_with('/') {
            format!("/{module_path}")
        } else {
            module_path.to_string()
        }
    } else {
        format!("{base_dir}/{module_path}")
    };
    filename_to_uri(&normalize_dot_segments(&joined))
}

/// Collapse `.` and `..` segments lexically.
///
/// Clients key open documents by URI string, so `file:///a/b/../lib/x.wado`
/// names a different document than the canonical `file:///a/lib/x.wado` the
/// same file gets when opened any other way.
///
/// Lexical only — symlinks are not resolved, the wasm target has no
/// filesystem. A `..` that escapes its own root is kept rather than dropped,
/// which would silently retarget the URI.
fn normalize_dot_segments(path: &str) -> String {
    if !path.contains("./") && !path.ends_with("/.") && !path.ends_with("/..") {
        return path.to_string();
    }
    let (prefix, rest) = split_scheme(path);
    let rooted = rest.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." if matches!(out.last(), Some(&last) if last != "..") => {
                out.pop();
            }
            // An unmatched `..` is only droppable when the path is rooted:
            // `/..` is `/`, but `../x` names a real sibling directory.
            ".." if rooted => {}
            _ => out.push(segment),
        }
    }
    let body = out.join("/");
    if rooted {
        format!("{prefix}/{body}")
    } else {
        format!("{prefix}{body}")
    }
}

/// Split `path` into its scheme prefix and the body `..` may rewrite, so a
/// `..` can never eat the scheme. A bare filesystem path has no prefix.
fn split_scheme(path: &str) -> (&str, &str) {
    if let Some(rest) = path.strip_prefix("file://") {
        return ("file://", rest);
    }
    match path.find(':') {
        // Only a real scheme, never a stray colon inside a filename: the
        // schemes reaching here are the compiler's own.
        Some(colon) if colon > 0 && !path[..colon].contains('/') => path.split_at(colon + 1),
        _ => ("", path),
    }
}

/// URI of the module defining `symbol`, relative to the requesting document.
pub(crate) fn symbol_uri(
    entry: &ModuleSource,
    symbol: &Symbol,
    request_uri: &str,
) -> Option<String> {
    module_uri(entry, symbol.module_source(), request_uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_import_off_kiln_uri_preserves_parent_directory() {
        // A kiln-redirected module's siblings anchor under its directory,
        // not at the scheme root.
        let resolved = resolve_local_uri("./sibling.wado", "kiln:/abs/path/foo.wado");
        assert_eq!(resolved, "kiln:/abs/path/sibling.wado");
    }

    #[test]
    fn relative_import_off_wasi_uri_preserves_parent_directory() {
        // wasi: URIs that include a path segment
        // (`wasi:filesystem/preopens.wado`) should anchor sibling
        // imports under that segment.
        let resolved = resolve_local_uri("./types.wado", "wasi:filesystem/preopens.wado");
        assert_eq!(resolved, "wasi:filesystem/types.wado");
    }

    #[test]
    fn relative_import_off_file_uri_anchors_at_request_dir() {
        let resolved = resolve_local_uri("./other.wado", "file:///home/user/foo.wado");
        assert_eq!(resolved, "file:///home/user/other.wado");
    }

    #[test]
    fn absolute_module_path_is_passed_through() {
        let resolved = resolve_local_uri("/abs/x.wado", "file:///home/user/foo.wado");
        assert_eq!(resolved, "file:///abs/x.wado");
    }

    #[test]
    fn parent_relative_import_collapses_dot_dot() {
        let resolved = resolve_local_uri("../lib/x.wado", "file:///home/user/foo.wado");
        assert_eq!(resolved, "file:///home/lib/x.wado");
    }

    #[test]
    fn repeated_parent_segments_collapse() {
        let resolved = resolve_local_uri("../../lib/x.wado", "file:///a/b/c/foo.wado");
        assert_eq!(resolved, "file:///a/lib/x.wado");
    }

    #[test]
    fn interior_dot_segments_collapse() {
        let resolved = resolve_local_uri("./sub/./x.wado", "file:///home/user/foo.wado");
        assert_eq!(resolved, "file:///home/user/sub/x.wado");
    }

    #[test]
    fn parent_escape_of_root_is_clamped() {
        // `/..` is `/`.
        let resolved = resolve_local_uri("../../../x.wado", "file:///a/foo.wado");
        assert_eq!(resolved, "file:///x.wado");
    }

    #[test]
    fn parent_relative_import_off_kiln_uri_collapses_too() {
        let resolved = resolve_local_uri("../gen/x.wado", "kiln:/abs/path/foo.wado");
        assert_eq!(resolved, "kiln:/abs/gen/x.wado");
    }

    #[test]
    fn sibling_of_an_encoded_request_uri_stays_encoded() {
        // The base directory we join against is decoded, so the join has to
        // be re-encoded on the way back out.
        let resolved = resolve_local_uri("./other.wado", "file:///home/user/my%20project/foo.wado");
        assert_eq!(resolved, "file:///home/user/my%20project/other.wado");
    }

    #[test]
    fn non_ascii_path_segments_are_re_encoded() {
        let resolved = resolve_local_uri("./types.wado", "file:///home/%E3%81%82/foo.wado");
        assert_eq!(resolved, "file:///home/%E3%81%82/types.wado");
    }

    #[test]
    fn a_literal_percent_survives_the_round_trip() {
        // A bare `%` would be decoded a second time by the client.
        let resolved = resolve_local_uri("./x.wado", "file:///home/100%25/foo.wado");
        assert_eq!(resolved, "file:///home/100%25/x.wado");
        assert_eq!(
            Uri::new(&resolved).to_filename(),
            "/home/100%/x.wado",
            "the emitted URI must decode back to the real path",
        );
    }

    #[test]
    fn path_separators_and_sub_delims_are_not_escaped() {
        let resolved = resolve_local_uri("./a+b,c.wado", "file:///home/user/foo.wado");
        assert_eq!(resolved, "file:///home/user/a+b,c.wado");
    }

    #[test]
    fn parent_segments_never_consume_the_scheme() {
        let resolved = resolve_local_uri("../../../x.wado", "kiln:/a/foo.wado");
        assert_eq!(resolved, "kiln:/x.wado");
    }

    #[test]
    fn colon_in_a_filename_is_not_mistaken_for_a_scheme() {
        let resolved = resolve_local_uri("../x.wado", "file:///a/we:ird/foo.wado");
        assert_eq!(resolved, "file:///a/x.wado");
    }

    #[test]
    fn absolute_module_path_with_dot_dot_is_normalized() {
        let resolved = resolve_local_uri("/abs/sub/../x.wado", "file:///home/user/foo.wado");
        assert_eq!(resolved, "file:///abs/x.wado");
    }
}
