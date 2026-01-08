//! Embedded standard library sources
//!
//! The core library is bundled into the compiler binary using `include_str!`.
//! This ensures the compiler is self-contained and doesn't need to locate
//! the core library at runtime.

/// Embedded source for core::prelude
pub const CORE_PRELUDE: &str = include_str!("../core/prelude.wado");

/// Embedded source for core::cli
pub const CORE_CLI: &str = include_str!("../core/cli.wado");

/// Embedded source for core::filesystem
pub const CORE_FILESYSTEM: &str = include_str!("../core/filesystem.wado");

/// Get embedded core module source by path.
///
/// # Arguments
/// * `path` - Module path segments, e.g., `["core", "cli"]`
///
/// # Returns
/// The source code of the module if found, or `None` if not a core module.
pub fn get_core_module(path: &[String]) -> Option<&'static str> {
    if path.len() != 2 {
        return None;
    }

    if path[0] != "core" {
        return None;
    }

    match path[1].as_str() {
        "prelude" => Some(CORE_PRELUDE),
        "cli" => Some(CORE_CLI),
        "filesystem" => Some(CORE_FILESYSTEM),
        _ => None,
    }
}

/// Check if a module path refers to a core library module.
pub fn is_core_module(path: &[String]) -> bool {
    get_core_module(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_core_cli() {
        let path = vec!["core".to_string(), "cli".to_string()];
        let source = get_core_module(&path);
        assert!(source.is_some());
        assert!(source.unwrap().contains("println"));
    }

    #[test]
    fn test_get_core_prelude() {
        let path = vec!["core".to_string(), "prelude".to_string()];
        let source = get_core_module(&path);
        assert!(source.is_some());
        assert!(source.unwrap().contains("Stream"));
    }

    #[test]
    fn test_get_core_filesystem() {
        let path = vec!["core".to_string(), "filesystem".to_string()];
        let source = get_core_module(&path);
        assert!(source.is_some());
        assert!(source.unwrap().contains("Descriptor"));
    }

    #[test]
    fn test_unknown_module() {
        let path = vec!["core".to_string(), "unknown".to_string()];
        assert!(get_core_module(&path).is_none());
    }

    #[test]
    fn test_non_core_module() {
        let path = vec!["myapp".to_string(), "utils".to_string()];
        assert!(get_core_module(&path).is_none());
    }

    #[test]
    fn test_is_core_module() {
        let cli_path = vec!["core".to_string(), "cli".to_string()];
        assert!(is_core_module(&cli_path));

        let other_path = vec!["other".to_string(), "module".to_string()];
        assert!(!is_core_module(&other_path));
    }
}
