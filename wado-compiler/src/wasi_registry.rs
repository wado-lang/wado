//! WASI import registry for dynamic Component Model generation
//!
//! This module collects WASI imports from effect definitions in lib/wasi/*.wado
//! and provides resolution and iteration for code generation.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::ast::{Type, WasiImport};

/// Information about a WASI function from an effect method
#[derive(Debug, Clone)]
pub struct WasiFunctionInfo {
    /// Effect name (e.g., "Stdout")
    pub effect_name: String,
    /// Method name in Wado (e.g., "write_via_stream")
    pub method_name: String,
    /// WASI function name (e.g., "write-via-stream")
    pub wasi_func_name: String,
    /// Full WASI interface path (e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16")
    pub interface_path: String,
    /// WASI package (e.g., "cli")
    pub package: String,
    /// Whether this is an async function
    pub is_async: bool,
    /// Parameter types
    pub params: Vec<(String, Type)>,
    /// Return type
    pub return_type: Option<Type>,
}

impl WasiFunctionInfo {
    /// Build the local alias name for Component Model imports.
    ///
    /// Format: `wasi:{package}/{effect_name}::{method_name}`
    /// Example: `wasi:cli/Stdout::write_via_stream`
    pub fn local_alias_name(&self) -> String {
        build_local_alias_name(&self.package, &self.effect_name, &self.method_name)
    }

}

/// Build a local alias name for a WASI function.
///
/// Format: `wasi:{package}/{effect_name}::{method_name}`
/// Example: `wasi:cli/Stdout::write_via_stream`
///
/// This naming scheme:
/// - Uses `wasi:` prefix for clarity
/// - Includes package for uniqueness across packages
/// - Uses Wado effect/method names (not WIT interface/function names)
/// - Uses `::` as method separator (Wado convention)
pub fn build_local_alias_name(package: &str, effect_name: &str, method_name: &str) -> String {
    format!("wasi:{}/{}::{}", package, effect_name, method_name)
}

/// Information about a WASI interface (grouping functions by interface)
#[derive(Debug, Clone)]
pub struct WasiInterfaceInfo {
    /// Interface path (e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16")
    pub path: String,
    /// Namespace (e.g., "wasi")
    pub namespace: String,
    /// Package (e.g., "cli")
    pub package: String,
    /// Interface name (e.g., "stdout")
    pub interface: String,
    /// Version (e.g., "0.3.0-rc-2025-09-16")
    pub version: Option<String>,
    /// Functions in this interface
    pub functions: Vec<WasiFunctionInfo>,
}

/// Registry of WASI imports for code generation
///
/// Collects information from effect definitions and provides:
/// - Resolution of effect calls (e.g., "Stdout::write_via_stream") to local names
/// - Iteration over interfaces for Component Model import generation
#[derive(Debug, Default)]
pub struct WasiRegistry {
    /// Effect::method -> function info
    effect_to_func: HashMap<String, WasiFunctionInfo>,

    /// Interface path -> list of functions
    /// Using BTreeMap for deterministic ordering
    interfaces: BTreeMap<String, Vec<WasiFunctionInfo>>,

    /// Local alias -> (interface_path, wasi_func_name)
    /// Key format: wasi:{package}/{effect_name}::{method_name}
    /// e.g., "wasi:cli/Stdout::write_via_stream"
    local_aliases: HashMap<String, (String, String)>,

    /// Track which WASI function names are used to detect collisions
    used_names: BTreeSet<String>,
}

impl WasiRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a WASI function from an effect method
    ///
    /// # Arguments
    /// * `effect_name` - The effect name (e.g., "Stdout")
    /// * `method_name` - The method name (e.g., "write_via_stream")
    /// * `wasi` - The parsed WASI import metadata
    /// * `is_async` - Whether this is an async function
    /// * `params` - Parameter names and types
    /// * `return_type` - Return type (if any)
    pub fn register(
        &mut self,
        effect_name: &str,
        method_name: &str,
        wasi: &WasiImport,
        is_async: bool,
        params: Vec<(String, Type)>,
        return_type: Option<Type>,
    ) {
        let interface_path = wasi.interface_path();

        // Get the WASI function name from the attribute, or derive from method name
        let wasi_func_name = wasi
            .function
            .clone()
            .unwrap_or_else(|| method_name.replace('_', "-"));

        let func_info = WasiFunctionInfo {
            effect_name: effect_name.to_string(),
            method_name: method_name.to_string(),
            wasi_func_name: wasi_func_name.clone(),
            interface_path: interface_path.clone(),
            package: wasi.package.clone(),
            is_async,
            params,
            return_type,
        };

        // Generate the local alias name using utility function
        // Format: wasi:{package}/{effect_name}::{method_name}
        let local_name = func_info.local_alias_name();

        self.used_names.insert(local_name.clone());

        // Register in effect -> func map
        let qualified_name = format!("{}::{}", effect_name, method_name);
        self.effect_to_func
            .insert(qualified_name.clone(), func_info.clone());

        // Register in interface -> functions map
        self.interfaces
            .entry(interface_path.clone())
            .or_default()
            .push(func_info);

        // Register local alias: local_name -> (interface_path, wasi_func_name)
        self.local_aliases
            .insert(local_name, (interface_path, wasi_func_name));
    }

    /// Resolve an effect function call to its component-level local alias name
    ///
    /// # Arguments
    /// * `name` - The qualified effect call (e.g., "Stdout::write_via_stream")
    ///
    /// # Returns
    /// The component-level local function name (e.g., "wasi:cli/Stdout::write_via_stream")
    pub fn resolve(&self, name: &str) -> Option<String> {
        if !name.contains("::") {
            return None;
        }

        // Look up the function info in the registry
        let func_info = self.effect_to_func.get(name)?;

        // Find the local name for this function
        for (local_name, (interface_path, wasi_func_name)) in &self.local_aliases {
            if interface_path == &func_info.interface_path
                && wasi_func_name == &func_info.wasi_func_name
            {
                return Some(local_name.clone());
            }
        }

        // This shouldn't happen if registration is correct, but fallback to a generated name
        None
    }

    /// Get function info by qualified name
    pub fn get_function(&self, name: &str) -> Option<&WasiFunctionInfo> {
        self.effect_to_func.get(name)
    }

    /// Get all interfaces that need to be imported
    ///
    /// Returns interfaces in deterministic order (sorted by path)
    pub fn interfaces(&self) -> impl Iterator<Item = WasiInterfaceInfo> + '_ {
        self.interfaces.iter().map(|(path, functions)| {
            // Parse the interface path to extract components
            let wasi = WasiImport::parse(path);

            WasiInterfaceInfo {
                path: path.clone(),
                namespace: wasi
                    .as_ref()
                    .map(|w| w.namespace.clone())
                    .unwrap_or_default(),
                package: wasi.as_ref().map(|w| w.package.clone()).unwrap_or_default(),
                interface: wasi
                    .as_ref()
                    .map(|w| w.interface.clone())
                    .unwrap_or_default(),
                version: wasi.as_ref().and_then(|w| w.version.clone()),
                functions: functions.clone(),
            }
        })
    }

    /// Check if the registry has any WASI imports
    pub fn is_empty(&self) -> bool {
        self.interfaces.is_empty()
    }

    /// Check if a specific interface is in the registry (by interface name, e.g., "monotonic-clock")
    pub fn has_interface(&self, interface_name: &str) -> bool {
        self.interfaces.keys().any(|path| {
            if let Some(wasi) = crate::ast::WasiImport::parse(path) {
                wasi.interface == interface_name
            } else {
                false
            }
        })
    }

    /// Get the local name for a function in an interface
    pub fn get_local_name(&self, interface_path: &str, wasi_func_name: &str) -> Option<&String> {
        self.local_aliases
            .iter()
            .find(|(_, (path, func))| path == interface_path && func == wasi_func_name)
            .map(|(local_name, _)| local_name)
    }

    /// Get the WASI CLI version from registered imports
    ///
    /// Returns the version string from the first wasi:cli/* interface found.
    /// Returns None if no wasi:cli interfaces are registered.
    pub fn get_cli_version(&self) -> Option<&str> {
        for path in self.interfaces.keys() {
            if let Some(wasi) = WasiImport::parse(path)
                && wasi.namespace == "wasi"
                && wasi.package == "cli"
                && wasi.version.is_some()
            {
                // Return a reference to the version in the path string
                // The version starts after '@' in the path
                if let Some(at_pos) = path.find('@') {
                    return Some(&path[at_pos + 1..]);
                }
            }
        }
        None
    }

    /// Get the WASI version for a specific package (e.g., "cli", "clocks")
    ///
    /// Returns the version string from the first interface of that package.
    pub fn get_package_version(&self, package: &str) -> Option<&str> {
        for path in self.interfaces.keys() {
            if let Some(wasi) = WasiImport::parse(path)
                && wasi.namespace == "wasi"
                && wasi.package == package
                && let Some(at_pos) = path.find('@')
            {
                return Some(&path[at_pos + 1..]);
            }
        }
        None
    }

    /// Get the function info for stdout's write_via_stream
    pub fn get_stdout_write_via_stream(&self) -> Option<&WasiFunctionInfo> {
        self.effect_to_func.get("Stdout::write_via_stream")
    }

    /// Get the function info for stderr's write_via_stream
    pub fn get_stderr_write_via_stream(&self) -> Option<&WasiFunctionInfo> {
        self.effect_to_func.get("Stderr::write_via_stream")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Span;

    fn make_span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 1,
            column: 1,
        }
    }

    fn make_stream_u8_type() -> Type {
        Type::Generic(crate::ast::GenericType {
            name: "Stream".to_string(),
            args: vec![Type::Named(crate::ast::NamedType {
                name: "u8".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        })
    }

    fn make_result_type() -> Type {
        Type::Generic(crate::ast::GenericType {
            name: "Result".to_string(),
            args: vec![
                Type::Tuple(vec![]), // ()
                Type::Named(crate::ast::NamedType {
                    name: "ErrorCode".to_string(),
                    span: make_span(),
                }),
            ],
            span: make_span(),
        })
    }

    #[test]
    fn test_register_and_resolve() {
        let mut registry = WasiRegistry::new();

        let wasi =
            WasiImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();

        registry.register(
            "Stdout",
            "write_via_stream",
            &wasi,
            true,
            vec![("data".to_string(), make_stream_u8_type())],
            Some(make_result_type()),
        );

        // Local name uses wasi:{package}/{effect}::{method} format
        let resolved = registry.resolve("Stdout::write_via_stream");
        assert_eq!(
            resolved,
            Some("wasi:cli/Stdout::write_via_stream".to_string())
        );
    }

    #[test]
    fn test_no_collision_with_different_interfaces() {
        let mut registry = WasiRegistry::new();

        // Register stdout
        let stdout_wasi =
            WasiImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();
        registry.register(
            "Stdout",
            "write_via_stream",
            &stdout_wasi,
            true,
            vec![("data".to_string(), make_stream_u8_type())],
            Some(make_result_type()),
        );

        // Register stderr - different interface, same function name
        let stderr_wasi =
            WasiImport::parse("wasi:cli/stderr@0.3.0-rc-2025-09-16#write-via-stream").unwrap();
        registry.register(
            "Stderr",
            "write_via_stream",
            &stderr_wasi,
            true,
            vec![("data".to_string(), make_stream_u8_type())],
            Some(make_result_type()),
        );

        // Each gets its own unique name via wasi:{package}/{effect}::{method} pattern
        let stdout_resolved = registry.resolve("Stdout::write_via_stream");
        assert_eq!(
            stdout_resolved,
            Some("wasi:cli/Stdout::write_via_stream".to_string())
        );

        let stderr_resolved = registry.resolve("Stderr::write_via_stream");
        assert_eq!(
            stderr_resolved,
            Some("wasi:cli/Stderr::write_via_stream".to_string())
        );
    }

    #[test]
    fn test_interfaces_iteration() {
        let mut registry = WasiRegistry::new();

        let wasi =
            WasiImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();

        registry.register(
            "Stdout",
            "write_via_stream",
            &wasi,
            true,
            vec![("data".to_string(), make_stream_u8_type())],
            Some(make_result_type()),
        );

        let interfaces: Vec<_> = registry.interfaces().collect();
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].namespace, "wasi");
        assert_eq!(interfaces[0].package, "cli");
        assert_eq!(interfaces[0].interface, "stdout");
        assert_eq!(
            interfaces[0].version,
            Some("0.3.0-rc-2025-09-16".to_string())
        );
        assert_eq!(interfaces[0].functions.len(), 1);
    }

    #[test]
    fn test_build_local_alias_name() {
        // Test the utility function directly
        assert_eq!(
            build_local_alias_name("cli", "Stdout", "write_via_stream"),
            "wasi:cli/Stdout::write_via_stream"
        );
        assert_eq!(
            build_local_alias_name("clocks", "MonotonicClock", "now"),
            "wasi:clocks/MonotonicClock::now"
        );
    }

    #[test]
    fn test_func_info_local_alias_name() {
        let func_info = WasiFunctionInfo {
            effect_name: "Stdout".to_string(),
            method_name: "write_via_stream".to_string(),
            wasi_func_name: "write-via-stream".to_string(),
            interface_path: "wasi:cli/stdout@0.3.0-rc-2025-09-16".to_string(),
            package: "cli".to_string(),
            is_async: true,
            params: vec![],
            return_type: None,
        };
        assert_eq!(
            func_info.local_alias_name(),
            "wasi:cli/Stdout::write_via_stream"
        );
    }
}
