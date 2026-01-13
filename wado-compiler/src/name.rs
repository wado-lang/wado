//! Name mangling utilities for Wado compiler
//!
//! This module centralizes all naming/mangling logic for methods, effects, and other symbols.
//!
//! # Naming Conventions
//!
//! ## Method Names
//! - Simple: `{struct_name}::{method_name}` (e.g., `Point::sum`)
//! - Full: `{filename}/{struct_name}::{method_name}` (e.g., `./geometry.wado/Point::sum`)
//! - With trait: `{filename}/{struct_name}^{trait_name}::{method_name}` (e.g., `./geometry.wado/Point^Display::fmt`)
//!
//! ## Effect Operation Names
//! - Qualified: `{effect_name}::{operation_name}` (e.g., `Stdout::write_via_stream`)
//!
//! ## WASI Names
//! - Full: `wasi:{package}/{interface}::{function}` (e.g., `wasi:cli/stdout::write-via-stream`)
//!
//! ## Module-Qualified Names
//! - Function: `{module_path}::{function_name}` (e.g., `./utils.wado::helper`)
//! - Struct: `{module_path}::{struct_name}` (e.g., `./geometry.wado::Point`)
//!
//! # Module Path Canonicalization
//!
//! Module paths are canonicalized using URI path normalization (RFC 3986) to ensure:
//! - Same file imported via different paths resolves to same identity
//! - Always uses `/` separator (platform-agnostic, even on Windows)
//! - Resolves `.` and `..` segments
//!
//! Canonical paths are project-root-relative:
//! - For projects with `wado.toml`: relative to the directory containing `wado.toml`
//! - For standalone scripts: relative to the entry point's directory

use fluent_uri::UriRef;

/// Information about a method name for mangling/demangling.
///
/// The mangled format is:
/// - Without trait: `{filename}/{struct_name}::{method_name}`
/// - With trait: `{filename}/{struct_name}^{trait_name}::{method_name}`
///
/// Examples:
/// - `./geometry.wado/Point::sum`
/// - `./geometry.wado/Point^Display::fmt`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodNameInfo {
    /// The source filename (e.g., `./geometry.wado`)
    pub filename: String,
    /// The struct name (e.g., `Point`)
    pub struct_name: String,
    /// The trait name if this is a trait implementation (e.g., `Display`)
    pub trait_name: Option<String>,
    /// The method name (e.g., `sum`)
    pub method_name: String,
}

// =============================================================================
// Method Name Mangling
// =============================================================================

/// Build a mangled name for a struct method.
///
/// Format:
/// - Without trait: `{filename}/{struct_name}::{method_name}`
/// - With trait: `{filename}/{struct_name}^{trait_name}::{method_name}`
///
/// Examples:
/// - `./geometry.wado/Point::sum`
/// - `./geometry.wado/Point^Display::fmt`
pub fn build_method_mangled_name(info: &MethodNameInfo) -> String {
    match &info.trait_name {
        Some(trait_name) => format!(
            "{}/{}^{}::{}",
            info.filename, info.struct_name, trait_name, info.method_name
        ),
        None => format!(
            "{}/{}::{}",
            info.filename, info.struct_name, info.method_name
        ),
    }
}

/// Parse a mangled method name back into its components.
///
/// Returns `None` if the format is invalid.
///
/// Expected formats:
/// - `{filename}/{struct_name}::{method_name}`
/// - `{filename}/{struct_name}^{trait_name}::{method_name}`
pub fn parse_method_mangled_name(mangled: &str) -> Option<MethodNameInfo> {
    // Split at the last `/` to separate filename from the rest
    let slash_pos = mangled.rfind('/')?;
    let filename = &mangled[..slash_pos];
    let rest = &mangled[slash_pos + 1..];

    // Split at `::` to separate struct/trait from method name
    let double_colon_pos = rest.find("::")?;
    let struct_trait_part = &rest[..double_colon_pos];
    let method_name = &rest[double_colon_pos + 2..];

    // Check for trait separator `^`
    if let Some(caret_pos) = struct_trait_part.find('^') {
        let struct_name = &struct_trait_part[..caret_pos];
        let trait_name = &struct_trait_part[caret_pos + 1..];
        Some(MethodNameInfo {
            filename: filename.to_string(),
            struct_name: struct_name.to_string(),
            trait_name: Some(trait_name.to_string()),
            method_name: method_name.to_string(),
        })
    } else {
        Some(MethodNameInfo {
            filename: filename.to_string(),
            struct_name: struct_trait_part.to_string(),
            trait_name: None,
            method_name: method_name.to_string(),
        })
    }
}

/// Build a simple method mangled name without filename.
///
/// Format: `{struct_name}::{method_name}`
///
/// Example: `Point::sum`
pub fn build_simple_method_name(struct_name: &str, method_name: &str) -> String {
    format!("{}::{}", struct_name, method_name)
}

/// Transform a method name by replacing its struct prefix.
///
/// Given `Point::sum` and alias `OtherPoint`, returns `OtherPoint::sum`.
///
/// Returns `None` if the mangled name doesn't start with the expected struct prefix.
pub fn transform_method_alias(
    mangled_name: &str,
    original_struct: &str,
    alias_struct: &str,
) -> Option<String> {
    let prefix = format!("{}::", original_struct);
    mangled_name
        .strip_prefix(&prefix)
        .map(|method_name| build_simple_method_name(alias_struct, method_name))
}

// =============================================================================
// Effect Operation Names
// =============================================================================

/// Build a qualified effect operation name.
///
/// Format: `{effect_name}::{operation_name}`
///
/// Example: `Stdout::write_via_stream`
pub fn build_effect_operation_name(effect_name: &str, operation_name: &str) -> String {
    format!("{}::{}", effect_name, operation_name)
}

/// Parse a qualified effect operation name.
///
/// Given `Stdout::write_via_stream`, returns `Some(("Stdout", "write_via_stream"))`.
pub fn parse_effect_operation_name(qualified: &str) -> Option<(&str, &str)> {
    let pos = qualified.find("::")?;
    Some((&qualified[..pos], &qualified[pos + 2..]))
}

// =============================================================================
// WASI Names
// =============================================================================

/// Build a fully qualified WASI function name.
///
/// Format: `wasi:{package}/{interface}::{function}`
///
/// Example: `wasi:cli/stdout::write-via-stream`
pub fn build_wasi_qualified_name(package: &str, interface: &str, function: &str) -> String {
    format!("wasi:{}/{}::{}", package, interface, function)
}

/// Parse a fully qualified WASI function name.
///
/// Returns `Some((package, interface, function))` or `None` if invalid.
pub fn parse_wasi_qualified_name(qualified: &str) -> Option<(&str, &str, &str)> {
    let rest = qualified.strip_prefix("wasi:")?;
    let slash_pos = rest.find('/')?;
    let package = &rest[..slash_pos];
    let after_slash = &rest[slash_pos + 1..];

    let colon_pos = after_slash.find("::")?;
    let interface = &after_slash[..colon_pos];
    let function = &after_slash[colon_pos + 2..];

    Some((package, interface, function))
}

// =============================================================================
// Module-Qualified Names
// =============================================================================

/// Build a qualified name with module path.
///
/// Format: `{module_path}::{name}`
///
/// Examples:
/// - `./geometry.wado::Point`
/// - `core::internal::helper`
pub fn build_qualified_name(module_path: &[String], name: &str) -> String {
    if module_path.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", module_path.join("::"), name)
    }
}

/// Alias for `build_qualified_name` specifically for struct types.
///
/// Format: `{module_path}::{struct_name}`
///
/// Example: `./geometry.wado::Point`
pub fn build_qualified_struct_name(module_path: &[String], struct_name: &str) -> String {
    build_qualified_name(module_path, struct_name)
}

/// Build a qualified function name with module path.
///
/// Format: `{module_path}::{function_name}`
///
/// Example: `./utils.wado::helper`
pub fn build_qualified_function_name(module_path: &[String], function_name: &str) -> String {
    build_qualified_name(module_path, function_name)
}

/// Build a core::internal qualified name.
///
/// Format: `core::internal::{name}`
///
/// Example: `core::internal::log_stdout`
pub fn build_core_internal_name(name: &str) -> String {
    format!("core::internal::{}", name)
}

// =============================================================================
// Extraction Utilities
// =============================================================================

/// Extract the method name from a mangled name.
///
/// Given `Point::sum`, returns `Some("sum")`.
/// Given `./geometry.wado/Point::sum`, returns `Some("sum")`.
pub fn extract_method_name(mangled: &str) -> Option<&str> {
    mangled.rfind("::").map(|pos| &mangled[pos + 2..])
}

/// Extract the struct name from a simple mangled name.
///
/// Given `Point::sum`, returns `Some("Point")`.
/// Given `OtherPoint::sum`, returns `Some("OtherPoint")`.
///
/// Note: This only works for simple names without filename prefix.
pub fn extract_struct_name_simple(mangled: &str) -> Option<&str> {
    let double_colon_pos = mangled.find("::")?;
    Some(&mangled[..double_colon_pos])
}

/// Check if a mangled method name belongs to a specific struct.
///
/// Given `Point::sum` and struct `Point`, returns `true`.
/// Given `OtherPoint::sum` and struct `Point`, returns `false`.
pub fn is_method_of_struct(mangled_name: &str, struct_name: &str) -> bool {
    let prefix = format!("{}::", struct_name);
    mangled_name.starts_with(&prefix)
}

// =============================================================================
// Module Path Canonicalization
// =============================================================================

/// Validate that a module path is a valid URI reference.
///
/// Returns `Ok(())` if the path is valid, or `Err(message)` if invalid.
///
/// This should be called by the analyzer before attempting to load a module
/// to provide better error messages.
///
/// # Arguments
/// * `path` - The module path to validate
///
/// # Returns
/// * `Ok(())` - The path is valid
/// * `Err(String)` - The path is invalid, with an error message
pub fn validate_module_path(path: &str) -> Result<(), String> {
    // Special prefixes are always valid
    if path.starts_with("core:")
        || path.starts_with("wasi:")
        || path.starts_with("https://")
        || path.starts_with("http://")
    {
        return Ok(());
    }

    // Try to parse as a URI reference
    match UriRef::parse(path) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("invalid module path: {}", e)),
    }
}

/// Normalize a module path using URI path normalization (RFC 3986).
///
/// This function:
/// - Resolves `.` (current directory) segments
/// - Resolves `..` (parent directory) segments
/// - Removes duplicate slashes
/// - Always uses `/` separator (platform-agnostic)
///
/// The input should be a relative path like `./foo/bar.wado` or `../lib/utils.wado`.
///
/// # Panics
/// Panics if the path is not a valid URI reference.
///
/// Examples:
/// - `./geometry.wado` → `./geometry.wado`
/// - `./sub/../geometry.wado` → `./geometry.wado`
/// - `./sub/./nested/../file.wado` → `./sub/file.wado`
/// - `foo//bar.wado` → `foo/bar.wado`
pub fn normalize_module_path(path: &str) -> String {
    // Handle special module prefixes that shouldn't be normalized
    if path.starts_with("core:")
        || path.starts_with("wasi:")
        || path.starts_with("https://")
        || path.starts_with("http://")
    {
        return path.to_string();
    }

    // RFC 3986 normalize() only removes dot segments from absolute paths,
    // so we use our manual implementation for relative module paths.
    // We still use fluent-uri for validation and encoding normalization.
    let uri_ref =
        UriRef::parse(path).unwrap_or_else(|e| panic!("invalid module path '{}': {}", path, e));

    // Apply encoding normalization (percent-encoding, etc.)
    let normalized = uri_ref.normalize();
    // Then apply dot segment removal for relative paths
    remove_dot_segments(normalized.as_str())
}

/// Resolve a relative module path against a base module path.
///
/// This function resolves import paths relative to the importing module's path,
/// producing a canonical path from the project root.
///
/// Examples:
/// - base: `./main.wado`, relative: `./geometry.wado` → `./geometry.wado`
/// - base: `./sub/main.wado`, relative: `./utils.wado` → `./sub/utils.wado`
/// - base: `./sub/main.wado`, relative: `../lib.wado` → `./lib.wado`
/// - base: `./a/b/main.wado`, relative: `../../c.wado` → `./c.wado`
pub fn resolve_module_path(base: &str, relative: &str) -> String {
    // Handle special module prefixes - they don't need resolution
    if relative.starts_with("core:")
        || relative.starts_with("wasi:")
        || relative.starts_with("https://")
        || relative.starts_with("http://")
    {
        return relative.to_string();
    }

    // Get the directory of the base path
    let base_dir = get_parent_path(base);

    // Join the base directory with the relative path
    let joined = if base_dir.is_empty() {
        relative.to_string()
    } else if let Some(stripped) = relative.strip_prefix("./") {
        // ./foo from ./sub/ becomes ./sub/foo
        format!("{}/{}", base_dir, stripped)
    } else if relative.starts_with("../") {
        // ../foo from ./sub/ needs parent resolution
        format!("{}/{}", base_dir, relative)
    } else {
        // bare name like "foo.wado" - treat as relative to base dir
        format!("{}/{}", base_dir, relative)
    };

    // Normalize the result to resolve . and ..
    normalize_module_path(&joined)
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

    format!("./{}", name)
}

/// Convert a filesystem path to a canonical module path.
///
/// This function:
/// - Converts backslashes to forward slashes (Windows compatibility)
/// - Makes the path relative to project root (removes absolute prefix)
/// - Ensures the path starts with `./`
///
/// The `project_root` is the absolute path to the project root directory.
/// The `file_path` is the absolute path to the module file.
///
/// Example:
/// - project_root: `/home/user/project`
/// - file_path: `/home/user/project/src/lib.wado`
/// - result: `./src/lib.wado`
pub fn filesystem_to_module_path(project_root: &str, file_path: &str) -> Option<String> {
    // Normalize separators to forward slashes
    let root = project_root.replace('\\', "/");
    let path = file_path.replace('\\', "/");

    // Strip the project root prefix
    let relative = path.strip_prefix(&root)?;

    // Remove leading slash if present
    let relative = relative.strip_prefix('/').unwrap_or(relative);

    // Ensure it starts with ./
    if relative.starts_with("./") {
        Some(relative.to_string())
    } else {
        Some(format!("./{}", relative))
    }
}

/// Get the parent directory of a path.
///
/// Given `./sub/file.wado`, returns `./sub`.
/// Given `./file.wado`, returns `.`.
/// Given `file.wado`, returns empty string.
fn get_parent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(pos) => &path[..pos],
        None => "",
    }
}

/// Remove dot segments (`.` and `..`) from a path.
///
/// This implements RFC 3986 Section 5.2.4 for relative paths,
/// which fluent-uri's normalize() doesn't handle.
fn remove_dot_segments(path: &str) -> String {
    // Convert backslashes to forward slashes
    let path = path.replace('\\', "/");

    // Split into segments and process
    let mut segments: Vec<&str> = Vec::new();
    let has_leading_dot = path.starts_with("./");

    for segment in path.split('/') {
        match segment {
            "" | "." => {
                // Skip empty segments and current dir markers (except preserving leading ./)
            }
            ".." => {
                // Go up one level if possible
                if !segments.is_empty() && segments.last() != Some(&"..") {
                    segments.pop();
                } else {
                    segments.push("..");
                }
            }
            s => segments.push(s),
        }
    }

    // Reconstruct the path
    let result = segments.join("/");

    // Preserve leading ./ for relative paths
    if has_leading_dot && !result.starts_with("..") {
        format!("./{}", result)
    } else if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Method Name Tests
    // =========================================================================

    #[test]
    fn test_build_method_mangled_name_simple() {
        let info = MethodNameInfo {
            filename: "./geometry.wado".to_string(),
            struct_name: "Point".to_string(),
            trait_name: None,
            method_name: "sum".to_string(),
        };
        let mangled = build_method_mangled_name(&info);
        assert_eq!(mangled, "./geometry.wado/Point::sum");
    }

    #[test]
    fn test_build_method_mangled_name_with_trait() {
        let info = MethodNameInfo {
            filename: "./geometry.wado".to_string(),
            struct_name: "Point".to_string(),
            trait_name: Some("Display".to_string()),
            method_name: "fmt".to_string(),
        };
        let mangled = build_method_mangled_name(&info);
        assert_eq!(mangled, "./geometry.wado/Point^Display::fmt");
    }

    #[test]
    fn test_parse_method_mangled_name_simple() {
        let parsed = parse_method_mangled_name("./geometry.wado/Point::sum").unwrap();
        assert_eq!(parsed.filename, "./geometry.wado");
        assert_eq!(parsed.struct_name, "Point");
        assert_eq!(parsed.trait_name, None);
        assert_eq!(parsed.method_name, "sum");
    }

    #[test]
    fn test_parse_method_mangled_name_with_trait() {
        let parsed = parse_method_mangled_name("./geometry.wado/Point^Display::fmt").unwrap();
        assert_eq!(parsed.filename, "./geometry.wado");
        assert_eq!(parsed.struct_name, "Point");
        assert_eq!(parsed.trait_name, Some("Display".to_string()));
        assert_eq!(parsed.method_name, "fmt");
    }

    #[test]
    fn test_roundtrip_simple() {
        let original = MethodNameInfo {
            filename: "./geometry.wado".to_string(),
            struct_name: "Point".to_string(),
            trait_name: None,
            method_name: "magnitude".to_string(),
        };
        let mangled = build_method_mangled_name(&original);
        let parsed = parse_method_mangled_name(&mangled).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_roundtrip_with_trait() {
        let original = MethodNameInfo {
            filename: "./models/user.wado".to_string(),
            struct_name: "User".to_string(),
            trait_name: Some("Serialize".to_string()),
            method_name: "to_json".to_string(),
        };
        let mangled = build_method_mangled_name(&original);
        let parsed = parse_method_mangled_name(&mangled).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_parse_invalid_no_slash() {
        assert!(parse_method_mangled_name("Point::sum").is_none());
    }

    #[test]
    fn test_parse_invalid_no_double_colon() {
        assert!(parse_method_mangled_name("./geometry.wado/Point").is_none());
    }

    #[test]
    fn test_build_simple_method_name() {
        let name = build_simple_method_name("Point", "sum");
        assert_eq!(name, "Point::sum");
    }

    #[test]
    fn test_transform_method_alias() {
        let result = transform_method_alias("Point::sum", "Point", "OtherPoint");
        assert_eq!(result, Some("OtherPoint::sum".to_string()));
    }

    #[test]
    fn test_transform_method_alias_no_match() {
        let result = transform_method_alias("OtherPoint::sum", "Point", "AliasPoint");
        assert_eq!(result, None);
    }

    // =========================================================================
    // Effect Operation Tests
    // =========================================================================

    #[test]
    fn test_build_effect_operation_name() {
        let name = build_effect_operation_name("Stdout", "write_via_stream");
        assert_eq!(name, "Stdout::write_via_stream");
    }

    #[test]
    fn test_parse_effect_operation_name() {
        let (effect, op) = parse_effect_operation_name("Stdout::write_via_stream").unwrap();
        assert_eq!(effect, "Stdout");
        assert_eq!(op, "write_via_stream");
    }

    #[test]
    fn test_parse_effect_operation_name_invalid() {
        assert!(parse_effect_operation_name("no_double_colon").is_none());
    }

    // =========================================================================
    // WASI Name Tests
    // =========================================================================

    #[test]
    fn test_build_wasi_qualified_name() {
        let name = build_wasi_qualified_name("cli", "stdout", "write-via-stream");
        assert_eq!(name, "wasi:cli/stdout::write-via-stream");
    }

    #[test]
    fn test_parse_wasi_qualified_name() {
        let (package, interface, function) =
            parse_wasi_qualified_name("wasi:cli/stdout::write-via-stream").unwrap();
        assert_eq!(package, "cli");
        assert_eq!(interface, "stdout");
        assert_eq!(function, "write-via-stream");
    }

    #[test]
    fn test_parse_wasi_qualified_name_invalid() {
        assert!(parse_wasi_qualified_name("not-wasi-format").is_none());
        assert!(parse_wasi_qualified_name("wasi:no-slash").is_none());
    }

    // =========================================================================
    // Module-Qualified Name Tests
    // =========================================================================

    #[test]
    fn test_build_qualified_struct_name_empty_path() {
        let name = build_qualified_struct_name(&[], "Point");
        assert_eq!(name, "Point");
    }

    #[test]
    fn test_build_qualified_struct_name_with_path() {
        let path = vec!["./geometry.wado".to_string()];
        let name = build_qualified_struct_name(&path, "Point");
        assert_eq!(name, "./geometry.wado::Point");
    }

    #[test]
    fn test_build_qualified_function_name() {
        let path = vec!["./utils.wado".to_string()];
        let name = build_qualified_function_name(&path, "helper");
        assert_eq!(name, "./utils.wado::helper");
    }

    #[test]
    fn test_build_core_internal_name() {
        let name = build_core_internal_name("log_stdout");
        assert_eq!(name, "core::internal::log_stdout");
    }

    // =========================================================================
    // Extraction Tests
    // =========================================================================

    #[test]
    fn test_extract_method_name() {
        assert_eq!(extract_method_name("Point::sum"), Some("sum"));
        assert_eq!(
            extract_method_name("./geometry.wado/Point::sum"),
            Some("sum")
        );
        assert_eq!(extract_method_name("Point^Display::fmt"), Some("fmt"));
        assert_eq!(extract_method_name("no_double_colon"), None);
    }

    #[test]
    fn test_extract_struct_name_simple() {
        assert_eq!(extract_struct_name_simple("Point::sum"), Some("Point"));
        assert_eq!(
            extract_struct_name_simple("OtherPoint::sum"),
            Some("OtherPoint")
        );
        assert_eq!(extract_struct_name_simple("no_double_colon"), None);
    }

    #[test]
    fn test_is_method_of_struct() {
        assert!(is_method_of_struct("Point::sum", "Point"));
        assert!(is_method_of_struct("Point::product", "Point"));
        assert!(!is_method_of_struct("OtherPoint::sum", "Point"));
        assert!(!is_method_of_struct("Point", "Point"));
    }

    // =========================================================================
    // Module Path Canonicalization Tests
    // =========================================================================

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
    fn test_filesystem_to_module_path() {
        assert_eq!(
            filesystem_to_module_path("/home/user/project", "/home/user/project/src/lib.wado"),
            Some("./src/lib.wado".to_string())
        );
        assert_eq!(
            filesystem_to_module_path("/home/user/project", "/home/user/project/main.wado"),
            Some("./main.wado".to_string())
        );
    }

    #[test]
    fn test_filesystem_to_module_path_windows() {
        assert_eq!(
            filesystem_to_module_path(
                "C:\\Users\\dev\\project",
                "C:\\Users\\dev\\project\\src\\lib.wado"
            ),
            Some("./src/lib.wado".to_string())
        );
    }

    #[test]
    fn test_get_parent_path() {
        assert_eq!(get_parent_path("./sub/file.wado"), "./sub");
        assert_eq!(get_parent_path("./file.wado"), ".");
        assert_eq!(get_parent_path("file.wado"), "");
    }

    #[test]
    fn test_remove_dot_segments() {
        assert_eq!(remove_dot_segments("./a/b/../c.wado"), "./a/c.wado");
        assert_eq!(remove_dot_segments("./a/./b/c.wado"), "./a/b/c.wado");
        assert_eq!(remove_dot_segments("a//b/c.wado"), "a/b/c.wado");
    }

    // =========================================================================
    // Module Path Validation Tests
    // =========================================================================

    #[test]
    fn test_validate_module_path_valid() {
        assert!(validate_module_path("./geometry.wado").is_ok());
        assert!(validate_module_path("../lib.wado").is_ok());
        assert!(validate_module_path("core:cli").is_ok());
        assert!(validate_module_path("wasi:filesystem").is_ok());
        assert!(validate_module_path("https://example.com/lib.wado").is_ok());
        assert!(validate_module_path("http://localhost:8080/lib.wado").is_ok());
    }

    #[test]
    fn test_validate_module_path_invalid() {
        // Paths with invalid URI characters should fail
        // Note: Most printable characters are valid in URI references,
        // so we test with control characters or invalid sequences
        assert!(validate_module_path("./file with\x00null.wado").is_err());
    }
}
