//! Module paths and import resolution. A module path is a filesystem
//! representation, not a URI: normalized lexically ([`crate::path::normalize`]),
//! never percent-encoded, and project-root-relative.

use crate::kiln::InvocationIndex;
use crate::module_source::{CmNamespace, ModuleSource, ModuleSourceInterner};
use crate::path::{is_cwd_relative, normalize, relative_path};
use crate::remote::{is_remote, resolve_url};

/// Validate that a string can name a module.
///
/// A module path is a filesystem representation, not a URI, so URI-unsafe
/// characters such as spaces are valid; only content that cannot name a file
/// (a NUL byte) is rejected. Special prefixes (`core:` / `wasi:` /
/// `http(s)://`) are opaque identifiers and always valid. The analyzer calls
/// this before loading to surface a clean error message.
///
/// Returns `Ok(())` if the path is valid, or `Err(message)` if invalid.
pub fn validate_module_path(path: &str) -> Result<(), String> {
    if has_special_prefix(path) {
        return Ok(());
    }

    // A bare module path is a filesystem path, not a URI: URI-unsafe
    // characters such as spaces are valid here. Reject only content that
    // cannot name a file.
    if path.contains('\0') {
        return Err("module path contains a NUL byte".to_string());
    }
    Ok(())
}

/// `true` for an opaque module identifier that is not a filesystem path and
/// must never be normalized: a reserved namespace (`core:`, `wasi:`) or
/// a remote URI (`http://` / `https://`).
fn has_special_prefix(path: &str) -> bool {
    path.starts_with("core:") || CmNamespace::split_specifier(path).is_some() || is_remote(path)
}

/// Normalize a module path lexically via [`crate::path::normalize`] — resolve
/// `.`/`..`, collapse duplicate slashes, unify separators to `/`. Never
/// percent-encoded, so a space survives intact. Special prefixes (`core:`,
/// `wasi:`, `http://`, `https://`) are opaque and returned verbatim. Infallible:
/// every filesystem path normalizes.
pub fn normalize_module_path(path: &str) -> String {
    if has_special_prefix(path) {
        return path.to_string();
    }
    normalize(path)
}

/// [`normalize_module_path`] gated by [`validate_module_path`]. Normalization
/// itself cannot fail, but callers accepting untrusted input (e.g. a CLI
/// `--symbol` notation) use this to reject a string that cannot name a module
/// with a clean error instead of letting it flow downstream.
pub fn try_normalize_module_path(path: &str) -> Result<String, String> {
    validate_module_path(path)?;
    Ok(normalize_module_path(path))
}

/// Resolve an import against the importing module's path: base `./sub/main.wado`
/// with `../lib.wado` gives `./lib.wado`. A URI base keeps its scheme and
/// authority, and a remote one resolves as a URL reference does.
pub fn resolve_module_path(base: &str, relative: &str) -> String {
    if has_special_prefix(relative) {
        return relative.to_string();
    }
    if is_remote(base) {
        return resolve_url(base, relative);
    }
    let (origin, base_path) = split_uri_origin(base);
    let joined = match base_path.rfind('/') {
        Some(pos) => format!("{}/{relative}", &base_path[..pos]),
        None if origin.contains("//") => format!("/{relative}"),
        None => relative.to_string(),
    };
    format!("{origin}{}", normalize(&joined))
}

/// Split a URI's `scheme:` and `//authority` off its path. A one-letter scheme is
/// a Windows drive, which stays with the path.
fn split_uri_origin(path: &str) -> (&str, &str) {
    let Some(colon) = path.find(':') else {
        return ("", path);
    };
    let scheme = &path[..colon];
    let is_scheme = scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    if !is_scheme {
        return ("", path);
    }
    let after_scheme = &path[colon + 1..];
    let origin_len = match after_scheme.strip_prefix("//") {
        Some(authority) => colon + 3 + authority.find('/').unwrap_or(authority.len()),
        None => colon + 1,
    };
    path.split_at(origin_len)
}

/// Canonicalize a resolved local module identity to the unique minimal form for
/// its physical file, relative to `entry_dir`. Composing relative steps from the
/// importer is not canonical — a path climbing above `entry_dir` and re-entering
/// spells the same file two ways — so re-anchoring is what lets the loader intern
/// each file once. Empty `entry_dir` just normalizes.
#[must_use]
fn canonical_local_path(entry_dir: &str, resolved: &str) -> String {
    if entry_dir.is_empty() {
        return normalize_module_path(resolved);
    }
    // `relative_path` normalizes both arguments, so the anchored join needs no
    // separate normalize pass.
    relative_path(entry_dir, &format!("{entry_dir}/{resolved}"))
}

/// The entry directory of an entry [`ModuleSource`], for use as the
/// [`canonical_local_path`] anchor. `EntryPoint`'s filename's parent; empty
/// for any other source or a parentless filename (both disable
/// canonicalization, falling back to plain relative composition).
#[must_use]
pub fn entry_dir_of(entry_module: Option<&ModuleSource>) -> String {
    match entry_module {
        Some(ModuleSource::EntryPoint { filename, .. }) => module_parent_dir(filename).to_string(),
        None
        | Some(
            ModuleSource::Core { .. }
            | ModuleSource::Binding { .. }
            | ModuleSource::Local { .. }
            | ModuleSource::Dependency { .. }
            | ModuleSource::Remote { .. }
            | ModuleSource::Redirected { .. }
            | ModuleSource::Wasm { .. }
            | ModuleSource::Effect(_),
        ) => String::new(),
    }
}

/// The parent directory of a module path: everything before the last `/`, or
/// empty if none. Every [`canonical_local_path`] anchor comes from here.
#[must_use]
pub fn module_parent_dir(path: &str) -> &str {
    path.rfind('/').map_or("", |pos| &path[..pos])
}

/// The loader identity of `import_source` imported from the local module
/// `from_path`, or from the entry module where `None`, anchored at `entry_dir`.
#[must_use]
pub fn resolve_local_identity(
    entry_dir: &str,
    from_path: Option<&str>,
    import_source: &str,
) -> String {
    let resolved = match from_path {
        Some(from_path) => resolve_module_path(from_path, import_source),
        None => normalize_module_path(import_source),
    };
    canonical_local_path(entry_dir, &resolved)
}

/// The file a declaration in `source` is written in, as a [`InvocationIndex`]
/// keys it and a diagnostic names it. Empty for a source that holds no path.
#[must_use]
pub fn decl_file_of(source: &ModuleSource) -> &str {
    match source {
        ModuleSource::Local { path, .. } | ModuleSource::Dependency { path, .. } => path.as_str(),
        ModuleSource::EntryPoint { filename, .. } => filename.as_str(),
        ModuleSource::Redirected { uri, .. } => uri.as_str(),
        ModuleSource::Remote { url, .. } => url.as_str(),
        ModuleSource::Core { .. }
        | ModuleSource::Binding { .. }
        | ModuleSource::Wasm { .. }
        | ModuleSource::Effect(_) => "",
    }
}

/// Whether `segment`, written before a `:`, is a package namespace or `lib`:
/// a manifest name, never a URL scheme's `//`.
#[must_use]
pub fn is_package_namespace(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Split a reference to a file a package exports, `"<package>/<file>"`, where
/// `<package>` is a coordinate (`ns:pkg[@ver]`) or a `lib:` nickname. Neither
/// carries a `/`, so the first one starts the file.
#[must_use]
pub fn split_exported_file(specifier: &str) -> Option<(&str, &str)> {
    let (package, file) = specifier.split_once('/')?;
    let (namespace, name) = package.split_once(':')?;
    (is_package_namespace(namespace) && !name.is_empty()).then_some((package, file))
}

/// Resolve an import source, a Kiln invocation's redirect first. The one
/// resolver the loader, analysis and the elaborator share.
pub fn resolve_import_with_invocations(
    interner: &mut ModuleSourceInterner,
    from_module: &ModuleSource,
    import_source: &str,
    entry_module: Option<&ModuleSource>,
    invocations: &InvocationIndex,
) -> ModuleSource {
    if !invocations.is_empty() {
        let decl_file = decl_file_of(from_module);
        if let Some(entry_uri) = invocations.redirect(decl_file, import_source) {
            return interner.redirected(entry_uri, from_module);
        }
    }
    resolve_import_with_entry(interner, from_module, import_source, entry_module)
}

pub fn resolve_import_with_entry(
    interner: &mut ModuleSourceInterner,
    from_module: &ModuleSource,
    import_source: &str,
    entry_module: Option<&ModuleSource>,
) -> ModuleSource {
    // Handle special prefixes
    if let Some(name) = import_source.strip_prefix("core:") {
        return interner.core(name);
    }
    if let Some((namespace, interface)) = CmNamespace::split_specifier(import_source) {
        return interner.binding(namespace, interface);
    }
    if is_remote(import_source) {
        return interner.remote(import_source);
    }

    let relative = is_cwd_relative(import_source);
    // A relative import inherits the importer's package root.
    if relative {
        match from_module {
            ModuleSource::Dependency { pkg, path, .. } => {
                let resolved = resolve_module_path(path, import_source);
                let pkg = pkg.to_string();
                return interner.dependency_module(&pkg, &resolved);
            }
            ModuleSource::Remote { pkg, url } => {
                let resolved = resolve_module_path(url, import_source);
                let pkg = pkg.to_string();
                return interner.remote_module(&pkg, &resolved);
            }
            ModuleSource::Redirected { uri, .. } => {
                let resolved = resolve_module_path(uri, import_source);
                return interner.redirected(&resolved, from_module);
            }
            ModuleSource::Core { .. }
            | ModuleSource::Binding { .. }
            | ModuleSource::Local { .. }
            | ModuleSource::EntryPoint { .. }
            | ModuleSource::Wasm { .. }
            | ModuleSource::Effect(_) => {}
        }
    }

    // Dependency name (`use { … } from "router"` / `from "ns:pkg"`), or a file
    // one exports (`from "ns:pkg/src/x.wado"`): resolve against the
    // `[dependencies]` of the importer's own package, never the consumer's on
    // a dependency's behalf. A path dependency is Wado source; a registry
    // dependency is a prebuilt component imported across the CM boundary.
    if !relative {
        if let Some((package, file)) = split_exported_file(import_source) {
            if let Some(Ok(module)) = interner.resolve_exported_file(from_module, package, file) {
                return module;
            }
        } else {
            if let Some(dep) = interner.resolve_dependency(from_module, import_source) {
                return dep;
            }
            if let Some(component) =
                interner.resolve_component_dependency(from_module, import_source)
            {
                return component;
            }
        }
    }

    let importer = match from_module {
        ModuleSource::Local { path, .. } if relative => Some(Some(path.as_str())),
        ModuleSource::EntryPoint { .. } => Some(None),
        ModuleSource::Local { .. }
        | ModuleSource::Core { .. }
        | ModuleSource::Binding { .. }
        | ModuleSource::Dependency { .. }
        | ModuleSource::Remote { .. }
        | ModuleSource::Redirected { .. }
        | ModuleSource::Wasm { .. }
        | ModuleSource::Effect(_) => None,
    };
    if let Some(importer) = importer {
        let resolved = resolve_local_identity(&entry_dir_of(entry_module), importer, import_source);
        // A path back to the entry, from any module the entry itself included,
        // names the entry, so it keeps a single type identity.
        if let Some(entry) = entry_module {
            let entry_canonical = match entry {
                ModuleSource::EntryPoint { filename, .. } => canonicalize_entry_point(filename),
                ModuleSource::Core { .. }
                | ModuleSource::Binding { .. }
                | ModuleSource::Local { .. }
                | ModuleSource::Dependency { .. }
                | ModuleSource::Remote { .. }
                | ModuleSource::Redirected { .. }
                | ModuleSource::Wasm { .. }
                | ModuleSource::Effect(_) => entry.to_string(),
            };
            if resolved == entry_canonical {
                return entry.clone();
            }
        }
        return interner.local(&resolved);
    }

    // Fallback: normalize and return as Local path (bare/other imports).
    interner.local(&normalize_module_path(import_source))
}

/// Get the canonical name for an entry point file.
///
/// The entry point file gets a canonical name based on its filename,
/// prefixed with `./` to indicate it's in the project root.
///
/// Example: `main.wado` → `./main.wado`
pub fn canonicalize_entry_point(filename: &str) -> String {
    // Extract just the filename if a path is provided
    let name = filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .rsplit('\\')
        .next()
        .unwrap_or(filename);

    format!("./{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::DependencyIndex;
    use std::assert_matches;

    #[test]
    fn bare_dep_resolves_only_for_consumer_not_inside_dependency() {
        let mut interner = ModuleSourceInterner::new();
        let mut index = DependencyIndex::default();
        index
            .resolved
            .insert("logger".to_string(), "../logger/src/lib.wado".to_string());
        for lib in ["../logger/src/lib.wado", "../greet/src/lib.wado"] {
            index
                .packages
                .insert(lib.to_string(), DependencyIndex::default());
        }
        interner.set_dependencies(index);
        let entry = interner.entry_point("main.wado");
        // From the consuming project, a bare name binds to its dependency.
        let from_entry = resolve_import_with_entry(&mut interner, &entry, "logger", None);
        assert_matches!(from_entry, ModuleSource::Dependency { .. });
        // From inside a dependency, the same bare name must NOT bind to the
        // consumer's deps — it falls through to the local fallback.
        let from_dep = interner.dependency("../greet/src/lib.wado");
        let resolved = resolve_import_with_entry(&mut interner, &from_dep, "logger", None);
        let logger_dep = interner.dependency("../logger/src/lib.wado");
        assert_ne!(resolved, logger_dep);
    }

    #[test]
    fn test_normalize_simple_path() {
        assert_eq!(normalize_module_path("./geometry.wado"), "./geometry.wado");
        assert_eq!(normalize_module_path("./sub/file.wado"), "./sub/file.wado");
    }

    #[test]
    fn test_normalize_dot_segments() {
        assert_eq!(
            normalize_module_path("./sub/../geometry.wado"),
            "./geometry.wado"
        );
        assert_eq!(
            normalize_module_path("./sub/./file.wado"),
            "./sub/file.wado"
        );
        assert_eq!(normalize_module_path("./a/b/../c/./d.wado"), "./a/c/d.wado");
    }

    #[test]
    fn test_normalize_special_prefixes() {
        // Special prefixes should not be modified
        assert_eq!(normalize_module_path("core:cli"), "core:cli");
        assert_eq!(normalize_module_path("wasi:filesystem"), "wasi:filesystem");
        assert_eq!(
            normalize_module_path("https://example.com/lib.wado"),
            "https://example.com/lib.wado"
        );
    }

    #[test]
    fn test_resolve_same_directory() {
        assert_eq!(
            resolve_module_path("./main.wado", "./geometry.wado"),
            "./geometry.wado"
        );
    }

    #[test]
    fn test_resolve_subdirectory() {
        assert_eq!(
            resolve_module_path("./sub/main.wado", "./utils.wado"),
            "./sub/utils.wado"
        );
        assert_eq!(
            resolve_module_path("./a/b/main.wado", "./file.wado"),
            "./a/b/file.wado"
        );
    }

    #[test]
    fn test_resolve_parent_directory() {
        assert_eq!(
            resolve_module_path("./sub/main.wado", "../lib.wado"),
            "./lib.wado"
        );
        assert_eq!(
            resolve_module_path("./a/b/main.wado", "../../c.wado"),
            "./c.wado"
        );
    }

    #[test]
    fn test_resolve_absolute_base_with_space() {
        // The Kiln harvest uses the entry's absolute filesystem path as the
        // resolve base; a directory with a space must not panic and must keep
        // the absolute root (regression for #1417).
        assert_eq!(
            resolve_module_path("/abs/My Project/main.wado", "./eval.wado"),
            "/abs/My Project/eval.wado"
        );
        assert_eq!(
            resolve_module_path("/abs/My Project/sub/main.wado", "../eval.wado"),
            "/abs/My Project/eval.wado"
        );
    }

    #[test]
    fn test_resolve_against_a_uri_or_root() {
        assert_eq!(
            resolve_module_path("file:///abs/gen/x.wado", "./h.wado"),
            "file:///abs/gen/h.wado"
        );
        assert_eq!(
            resolve_module_path("https://host/a/b.wado", "../c.wado"),
            "https://host/c.wado"
        );
        assert_eq!(
            resolve_module_path("kiln:/abs/gen/x.wado", "../h.wado"),
            "kiln:/abs/h.wado"
        );
        assert_eq!(
            resolve_module_path("core:json/value.wado", "./parse.wado"),
            "core:json/parse.wado"
        );
        assert_eq!(
            resolve_module_path("https://host:8080", "./c.wado"),
            "https://host:8080/c.wado"
        );
        assert_eq!(resolve_module_path("/x.wado", "./c.wado"), "/c.wado");
        assert_eq!(
            resolve_module_path("C:/a/x.wado", "./c.wado"),
            "C:/a/c.wado"
        );
    }

    #[test]
    fn test_resolve_special_prefixes() {
        // Special prefixes should pass through unchanged
        assert_eq!(
            resolve_module_path("./sub/main.wado", "core:cli"),
            "core:cli"
        );
        assert_eq!(
            resolve_module_path("./sub/main.wado", "wasi:filesystem"),
            "wasi:filesystem"
        );
    }

    #[test]
    fn test_canonicalize_entry_point() {
        assert_eq!(canonicalize_entry_point("main.wado"), "./main.wado");
        assert_eq!(
            canonicalize_entry_point("/absolute/path/main.wado"),
            "./main.wado"
        );
        assert_eq!(
            canonicalize_entry_point("C:\\Windows\\path\\main.wado"),
            "./main.wado"
        );
    }

    #[test]
    fn test_module_parent_dir() {
        assert_eq!(module_parent_dir("./sub/file.wado"), "./sub");
        assert_eq!(module_parent_dir("./file.wado"), ".");
        assert_eq!(module_parent_dir("file.wado"), "");
    }

    #[test]
    fn test_normalize_module_path_filesystem_literal() {
        // A module path is a filesystem string: URI-unsafe characters such as
        // spaces are preserved, never percent-encoded, and never panic
        // (regression for #1417).
        assert_eq!(normalize_module_path("./a b.wado"), "./a b.wado");
        assert_eq!(
            normalize_module_path("/home/user/My Project/eval.wado"),
            "/home/user/My Project/eval.wado"
        );
        // A literal `%` is part of the filename, not an escape sequence.
        assert_eq!(normalize_module_path("./a%20b.wado"), "./a%20b.wado");
        // Absolute paths keep their root through dot-segment resolution.
        assert_eq!(normalize_module_path("/abs/a/../b.wado"), "/abs/b.wado");
    }

    #[test]
    fn test_validate_module_path_valid() {
        assert!(validate_module_path("./geometry.wado").is_ok());
        assert!(validate_module_path("../lib.wado").is_ok());
        assert!(validate_module_path("core:cli").is_ok());
        assert!(validate_module_path("wasi:filesystem").is_ok());
        assert!(validate_module_path("https://example.com/lib.wado").is_ok());
        assert!(validate_module_path("http://localhost:8080/lib.wado").is_ok());
        // A space is valid in a filesystem path (regression for #1417).
        assert!(validate_module_path("./a b.wado").is_ok());
        assert!(validate_module_path("/home/user/My Project/eval.wado").is_ok());
    }

    #[test]
    fn test_validate_module_path_invalid() {
        // Only content that cannot name a file is rejected (a NUL byte).
        assert!(validate_module_path("./file with\x00null.wado").is_err());
    }
}
