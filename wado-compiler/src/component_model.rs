//! Component Model support for code generation
//!
//! This module provides:
//! - WASI import registry: collects WASI imports from effect definitions in lib/wasi/*.wado
//! - Component Model ABI: type conversion and support checking for CM codegen

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use heck::ToKebabCase;
use wasm_encoder::ValType;

use crate::ast::{GenericType, Type, WasiImport};
use crate::tir::{TypeId, TypeTable};

/// Convert a name to kebab-case
fn to_kebab_case(name: &str) -> String {
    name.to_kebab_case()
}

/// Information about a WASI function from an effect method
#[derive(Debug, Clone)]
pub struct WasiFunctionInfo {
    /// Effect name (e.g., "Stdout")
    pub effect_name: String,
    /// Method name in Wado (e.g., "`write_via_stream`")
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
    /// Component Model call convention (derived from return type)
    pub call_convention: CmCallConvention,
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
    format!("wasi:{package}/{effect_name}::{method_name}")
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
    /// Resource type exported by this interface (if any).
    /// Format: (Wado name, CM kebab-case name)
    /// e.g., ("`TerminalInput`", "terminal-input")
    pub resource_type: Option<(String, String)>,
}

/// Registry of WASI imports for code generation
///
/// Collects information from effect definitions and provides:
/// - Resolution of effect calls (e.g., "`Stdout::write_via_stream`") to local names
/// - Iteration over interfaces for Component Model import generation
#[derive(Debug, Default)]
pub struct WasiRegistry {
    /// `Effect::method` -> function info
    effect_to_func: HashMap<String, WasiFunctionInfo>,

    /// Interface path -> list of functions
    /// Using `BTreeMap` for deterministic ordering
    interfaces: BTreeMap<String, Vec<WasiFunctionInfo>>,

    /// Local alias -> (`interface_path`, `wasi_func_name`)
    /// Key format: `wasi:{package}/{effect_name}::{method_name`}
    /// e.g., "`wasi:cli/Stdout::write_via_stream`"
    local_aliases: HashMap<String, (String, String)>,

    /// Track which WASI function names are used to detect collisions
    used_names: BTreeSet<String>,

    /// Type aliases collected from WASI modules (e.g., Instant -> u64)
    type_aliases: HashMap<String, Type>,

    /// Resource types collected from WASI modules (e.g., `TerminalInput`, `TerminalOutput`)
    /// Maps resource name -> CM resource name (kebab-case)
    resources: HashMap<String, String>,

    /// Enum types collected from WASI modules (e.g., `ErrorCode`, `IpAddressFamily`)
    /// Maps Wado enum name -> (CM enum name kebab-case, variant names in kebab-case)
    enums: HashMap<String, (String, Vec<String>)>,
}

impl WasiRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the registry from the embedded stdlib
    ///
    /// Parses the embedded wasi:* modules and registers their effect methods.
    /// Also collects type aliases and world definitions.
    pub fn build_from_stdlib() -> (Self, crate::world_registry::WorldRegistry) {
        use crate::ast::Module as AstModule;
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::stdlib;
        use crate::world_registry::WorldRegistry;

        fn parse_module(source: &str) -> AstModule {
            let mut lexer = Lexer::new(source);
            let tokens = lexer.tokenize().expect("lexer error in stdlib");
            let mut parser = Parser::new(tokens);
            parser.parse().expect("parser error in stdlib")
        }

        let mut registry = Self::new();
        let mut world_registry = WorldRegistry::new();

        // Parse and register wasi:cli
        let wasi_cli = parse_module(stdlib::WASI_CLI);
        registry.register_module(&wasi_cli, &mut world_registry);

        // Parse and register wasi:clocks
        let wasi_clocks = parse_module(stdlib::WASI_CLOCKS);
        registry.register_module(&wasi_clocks, &mut world_registry);

        // Parse and register wasi:random
        let wasi_random = parse_module(stdlib::WASI_RANDOM);
        registry.register_module(&wasi_random, &mut world_registry);

        // Parse and register wasi:sockets
        let wasi_sockets = parse_module(stdlib::WASI_SOCKETS);
        registry.register_module(&wasi_sockets, &mut world_registry);

        // Parse and register wasi:http (for world definitions only)
        // Note: HTTP functions with resource types (Request, Response) are not yet
        // fully supported for Component Model lowering. We register the module
        // to get the world definitions, but skip function registration.
        let wasi_http = parse_module(stdlib::WASI_HTTP);
        registry.register_world_definitions(&wasi_http, &mut world_registry);

        // Note: wasi:filesystem uses `flags` syntax which isn't supported yet
        // TODO: Add wasi:filesystem registration when `flags` parsing is implemented

        (registry, world_registry)
    }

    /// Register effects and types from a WASI module
    fn register_module(
        &mut self,
        module: &crate::ast::Module,
        world_registry: &mut crate::world_registry::WorldRegistry,
    ) {
        use crate::ast::Item;

        // First, collect type aliases from this module
        for item in &module.items {
            if let Item::Type(alias) = item {
                self.type_aliases
                    .insert(alias.name.clone(), alias.ty.clone());
            }
        }

        // Collect resource types from this module
        for item in &module.items {
            if let Item::Resource(resource) = item {
                // Convert PascalCase to kebab-case for CM
                let cm_name = to_kebab_case(&resource.name);
                self.resources.insert(resource.name.clone(), cm_name);
            }
        }

        // Collect enum types from this module
        // Use interface path from #[wasi] attribute as key to distinguish same-named enums
        for item in &module.items {
            if let Item::Enum(enum_def) = item {
                // Convert PascalCase to kebab-case for CM
                let cm_name = to_kebab_case(&enum_def.name);
                // Also collect variant names in kebab-case
                let variant_names: Vec<String> = enum_def
                    .cases
                    .iter()
                    .map(|c| to_kebab_case(&c.name))
                    .collect();

                // Extract interface path from #[wasi] attribute if present
                // Format: #[wasi("wasi:sockets/types@0.3.0-rc-2025-09-16#error-code")]
                let interface_path = enum_def
                    .attrs
                    .iter()
                    .find(|a| a.name == "wasi")
                    .and_then(|a| a.args.first())
                    .and_then(|s| s.split('#').next())
                    .map(std::string::ToString::to_string);

                // Store by Wado name for backward compatibility
                // but also store by interface path for interface-specific lookups
                self.enums.insert(
                    enum_def.name.clone(),
                    (cm_name.clone(), variant_names.clone()),
                );

                // Also store by interface path + name for disambiguation
                if let Some(path) = interface_path {
                    let full_key = format!("{path}#{}", enum_def.name);
                    self.enums.insert(full_key, (cm_name, variant_names));
                }
            }
        }

        // Helper closure to resolve types through aliases
        let resolve_type = |ty: &Type, aliases: &HashMap<String, Type>| -> Type {
            match ty {
                Type::Named(named) => {
                    if let Some(resolved) = aliases.get(&named.name) {
                        resolved.clone()
                    } else {
                        ty.clone()
                    }
                }
                _ => ty.clone(),
            }
        };

        // Register effect methods with resolved types
        for item in &module.items {
            if let Item::Effect(effect) = item {
                for method in &effect.methods {
                    if let Some(wasi) = method.attrs.first().and_then(|a| a.wasi_import.as_ref()) {
                        let params: Vec<(String, Type)> = method
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), resolve_type(&p.ty, &self.type_aliases)))
                            .collect();

                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|ty| resolve_type(ty, &self.type_aliases));

                        self.register(
                            &effect.name,
                            &method.name,
                            wasi,
                            method.is_async,
                            params,
                            return_type,
                        );
                    }
                }
            }
        }

        // Register resource methods with resolved types
        for item in &module.items {
            if let Item::Resource(resource) = item {
                for method in &resource.methods {
                    if let Some(wasi) = method.attrs.first().and_then(|a| a.wasi_import.as_ref()) {
                        let params: Vec<(String, Type)> = method
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), resolve_type(&p.ty, &self.type_aliases)))
                            .collect();

                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|ty| resolve_type(ty, &self.type_aliases));

                        self.register(
                            &resource.name,
                            &method.name,
                            wasi,
                            method.is_async,
                            params,
                            return_type,
                        );
                    }
                }
            }
        }

        // Register world definitions
        for item in &module.items {
            if let Item::World(world) = item {
                world_registry.register(world);
            }
        }
    }

    /// Register only world definitions from a WASI module (no effects)
    ///
    /// Use this for modules with resource types that aren't fully supported
    /// for Component Model lowering yet. This registers the world definitions
    /// so they can be used for targeting (e.g., --world Service), but skips
    /// effect/function registration.
    fn register_world_definitions(
        &mut self,
        module: &crate::ast::Module,
        world_registry: &mut crate::world_registry::WorldRegistry,
    ) {
        use crate::ast::Item;

        // Collect resource types (needed for type checking)
        for item in &module.items {
            if let Item::Resource(resource) = item {
                let cm_name = to_kebab_case(&resource.name);
                self.resources.insert(resource.name.clone(), cm_name);
            }
        }

        // Register world definitions only
        for item in &module.items {
            if let Item::World(world) = item {
                world_registry.register(world);
            }
        }
    }

    /// Get a type alias by name
    pub fn get_type_alias(&self, name: &str) -> Option<&Type> {
        self.type_aliases.get(name)
    }

    /// Get all type aliases
    pub fn type_aliases(&self) -> &HashMap<String, Type> {
        &self.type_aliases
    }

    /// Check if a type name is a registered resource
    pub fn is_resource(&self, name: &str) -> bool {
        self.resources.contains_key(name)
    }

    /// Get the CM kebab-case name for a resource
    pub fn get_resource_cm_name(&self, name: &str) -> Option<&str> {
        self.resources.get(name).map(String::as_str)
    }

    /// Check if a type name is a registered enum
    pub fn is_enum(&self, name: &str) -> bool {
        self.enums.contains_key(name)
    }

    /// Get the CM kebab-case name for an enum
    pub fn get_enum_cm_name(&self, name: &str) -> Option<&str> {
        self.enums.get(name).map(|(cm_name, _)| cm_name.as_str())
    }

    /// Get the CM enum variant names (in kebab-case)
    pub fn get_enum_variants(&self, name: &str) -> Option<&[String]> {
        self.enums
            .get(name)
            .map(|(_, variants)| variants.as_slice())
    }

    /// Get the CM enum variant names by interface path + name
    /// This is used to disambiguate enums with the same Wado name but different interfaces
    /// (e.g., wasi:cli/types#ErrorCode vs wasi:sockets/types#ErrorCode)
    pub fn get_enum_variants_by_interface(
        &self,
        interface_path: &str,
        name: &str,
    ) -> Option<&[String]> {
        let full_key = format!("{interface_path}#{name}");
        self.enums
            .get(&full_key)
            .or_else(|| self.enums.get(name))
            .map(|(_, variants)| variants.as_slice())
    }

    /// Get the CM enum name by interface path + name
    pub fn get_enum_cm_name_by_interface(&self, interface_path: &str, name: &str) -> Option<&str> {
        let full_key = format!("{interface_path}#{name}");
        self.enums
            .get(&full_key)
            .or_else(|| self.enums.get(name))
            .map(|(cm_name, _)| cm_name.as_str())
    }

    /// Get the resource type from a return type (if it's Option<ResourceName>)
    /// Returns (Wado name, CM name) if the return type references a resource
    pub fn get_resource_from_return_type(
        &self,
        return_type: &Option<Type>,
    ) -> Option<(String, String)> {
        let ty = return_type.as_ref()?;

        // Check for Option<ResourceName> pattern
        if let Type::Generic(g) = ty
            && g.name == "Option"
            && g.args.len() == 1
            && let Type::Named(inner) = &g.args[0]
            && let Some(cm_name) = self.resources.get(&inner.name)
        {
            return Some((inner.name.clone(), cm_name.clone()));
        }

        None
    }

    /// Check if a WASI function is supported for Component Model generation.
    ///
    /// This uses the registry's known enums and resources to determine if
    /// all types in the function signature are supported.
    pub fn is_function_supported(&self, func: &WasiFunctionInfo) -> bool {
        // Build sets of known enum and resource names
        let enums: HashSet<&str> = self.enums.keys().map(String::as_str).collect();
        let resources: HashSet<&str> = self.resources.keys().map(String::as_str).collect();

        // Check all parameter types
        for (_, ty) in &func.params {
            if !is_param_type_supported_with_types(ty, &enums, &resources) {
                return false;
            }
        }
        // Check return type if present
        if let Some(ret_ty) = &func.return_type
            && !is_return_type_supported_with_types(ret_ty, &enums, &resources)
        {
            return false;
        }
        true
    }

    /// Register a WASI function from an effect method
    ///
    /// # Arguments
    /// * `effect_name` - The effect name (e.g., "Stdout")
    /// * `method_name` - The method name (e.g., "`write_via_stream`")
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

        // Resolve type aliases in params and return type upfront
        // This ensures codegen doesn't need any type resolution logic
        let resolved_params: Vec<(String, Type)> = params
            .into_iter()
            .map(|(name, ty)| (name, self.resolve_type(&ty)))
            .collect();
        let resolved_return_type = return_type.map(|ty| self.resolve_type(&ty));

        // Derive CM call convention from return type, params, and async flag
        let call_convention = CmCallConvention::from_return_type(&resolved_return_type)
            .with_params(&resolved_params)
            .with_async(is_async);

        let func_info = WasiFunctionInfo {
            effect_name: effect_name.to_string(),
            method_name: method_name.to_string(),
            wasi_func_name: wasi_func_name.clone(),
            interface_path: interface_path.clone(),
            package: wasi.package.clone(),
            is_async,
            params: resolved_params,
            return_type: resolved_return_type,
            call_convention,
        };

        // Generate the local alias name using utility function
        // Format: wasi:{package}/{effect_name}::{method_name}
        let local_name = func_info.local_alias_name();

        self.used_names.insert(local_name.clone());

        // Register in effect -> func map
        let qualified_name = format!("{effect_name}::{method_name}");
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
    /// * `name` - The qualified effect call (e.g., "`Stdout::write_via_stream`")
    ///
    /// # Returns
    /// The component-level local function name (e.g., "`wasi:cli/Stdout::write_via_stream`")
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

            // Check if any function returns a resource type (Option<ResourceName>)
            let resource_type = functions
                .iter()
                .find_map(|func| self.get_resource_from_return_type(&func.return_type));

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
                resource_type,
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

    /// Get all registered WASI function names
    ///
    /// Returns an iterator over function names in `Effect::method` format
    /// (e.g., "`Stdout::write_via_stream`", "`MonotonicClock::now`").
    ///
    /// Used by the optimizer to populate `used_wasi_functions` in O0 mode.
    pub fn all_function_names(&self) -> impl Iterator<Item = &str> {
        self.effect_to_func.keys().map(std::string::String::as_str)
    }

    /// Get standard WASI function names (excluding effects that require explicit usage)
    ///
    /// Some effects are not included by default because:
    /// - Exit: May not be supported by all runtimes
    /// - Timezone: May not be available in all runtimes (wasi:clocks/timezone)
    /// - Terminal*: May not be available in non-terminal environments
    /// - `TcpSocket`, `UdpSocket`, `IpNameLookup`: Network interfaces require explicit usage
    ///
    /// These effects are only included when explicitly used in the program.
    pub fn standard_function_names(&self) -> impl Iterator<Item = &str> {
        self.all_function_names().filter(|name| {
            !name.starts_with("Exit::")
                && !name.starts_with("Timezone::")
                && !name.starts_with("TerminalStdin::")
                && !name.starts_with("TerminalStdout::")
                && !name.starts_with("TerminalStderr::")
                && !name.starts_with("TcpSocket::")
                && !name.starts_with("UdpSocket::")
                && !name.starts_with("IpNameLookup::")
        })
    }

    // ============================================================================
    // Type Conversion (AST types to Wasm types)
    // ============================================================================

    /// Resolve type aliases in a Type recursively
    ///
    /// This resolves type aliases like `Instant` -> `u64` throughout the type tree,
    /// including within generic type arguments.
    pub fn resolve_type(&self, ty: &Type) -> Type {
        match ty {
            Type::Named(named) => {
                if let Some(aliased_ty) = self.get_type_alias(&named.name) {
                    // Recursively resolve the aliased type
                    self.resolve_type(aliased_ty)
                } else {
                    ty.clone()
                }
            }
            Type::Generic(generic) => {
                // Resolve type arguments recursively
                let resolved_args: Vec<Type> = generic
                    .args
                    .iter()
                    .map(|arg| self.resolve_type(arg))
                    .collect();
                Type::Generic(GenericType {
                    name: generic.name.clone(),
                    args: resolved_args,
                    span: generic.span,
                })
            }
            Type::Tuple(types) => {
                let resolved: Vec<Type> = types.iter().map(|t| self.resolve_type(t)).collect();
                Type::Tuple(resolved)
            }
            Type::Reference(inner) => Type::Reference(Box::new(self.resolve_type(inner))),
            Type::MutReference(inner) => Type::MutReference(Box::new(self.resolve_type(inner))),
            Type::Function(func_ty) => {
                // For function types, resolve params and return type
                let resolved_params: Vec<Type> = func_ty
                    .params
                    .iter()
                    .map(|t| self.resolve_type(t))
                    .collect();
                let resolved_return = self.resolve_type(&func_ty.return_type);
                Type::Function(Box::new(crate::ast::FunctionType {
                    params: resolved_params,
                    return_type: resolved_return,
                    effects: func_ty.effects.clone(),
                }))
            }
            // NamespacedGeneric types (like builtin::array<T>) are passed through
            Type::NamespacedGeneric(ng) => {
                let resolved_args: Vec<Type> =
                    ng.args.iter().map(|arg| self.resolve_type(arg)).collect();
                Type::NamespacedGeneric(crate::ast::NamespacedGenericType {
                    namespace: ng.namespace.clone(),
                    name: ng.name.clone(),
                    args: resolved_args,
                    span: ng.span,
                })
            }
        }
    }
}

// ============================================================================
// Type Conversion (AST Type to Wasm ValType)
// ============================================================================

/// Convert a pre-resolved AST type to Wasm `ValType`
///
/// This is a pure conversion function - type aliases must already be resolved
/// before calling this function. Use `WasiRegistry::resolve_type()` during
/// registration to ensure types are pre-resolved.
pub fn wasi_type_to_valtype(ty: &Type) -> ValType {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" | "bool" | "char" | "u8" | "i8" | "u16" | "i16" => ValType::I32,
            "i64" | "u64" => ValType::I64,
            "f32" => ValType::F32,
            "f64" => ValType::F64,
            // For WASI contexts, unknown named types (struct types like Datetime, etc.)
            // are passed as i32 handles/pointers
            _ => ValType::I32,
        },
        Type::Generic(generic) => match generic.name.as_str() {
            // Stream<T> is represented as i32 handle
            "Stream" => ValType::I32,
            // Result<T, E> is represented as i32 discriminant
            "Result" => ValType::I32,
            // Future<T> is represented as i32 handle
            "Future" => ValType::I32,
            // Tuple types map to i32 for simplicity (struct pointer)
            "Tuple" => ValType::I32,
            // Array<T> is represented as a GC array reference (handled as i32 in WASI context)
            "Array" => ValType::I32,
            // Option<T> is represented as i32 discriminant
            "Option" => ValType::I32,
            other => panic!("unknown generic type in wasi_type_to_valtype: {other}"),
        },
        Type::Tuple(_) => ValType::I32,
        other => panic!("unsupported type variant in wasi_type_to_valtype: {other:?}"),
    }
}

/// Convert a resolved `TypeId` to Wasm `ValType`
///
/// Works with primitive `TypeIds` from `TypeTable` constants.
/// For builtin functions, this handles parameter and return types.
pub fn type_id_to_valtype(type_id: TypeId) -> ValType {
    match type_id {
        TypeTable::I8 | TypeTable::I16 | TypeTable::I32 => ValType::I32,
        TypeTable::U8 | TypeTable::U16 | TypeTable::U32 => ValType::I32,
        TypeTable::BOOL | TypeTable::CHAR => ValType::I32,
        TypeTable::I64 | TypeTable::U64 => ValType::I64,
        TypeTable::I128 | TypeTable::U128 => {
            // 128-bit integers are handled specially in wide arithmetic
            // Default to i64 for now (caller should handle specially)
            ValType::I64
        }
        TypeTable::F32 => ValType::F32,
        TypeTable::F64 => ValType::F64,
        TypeTable::UNIT | TypeTable::NEVER => {
            // Unit/Never have no representation - caller should handle
            ValType::I32
        }
        _ => {
            // Other types (structs, arrays, etc.) are GC references
            // represented as i32 in core wasm contexts
            ValType::I32
        }
    }
}

// ============================================================================
// Type Support Checking (for Component Model generation)
// ============================================================================

/// Check if a parameter type is supported for Component Model generation
///
/// Type aliases (like Instant, Duration) should already be resolved to their
/// underlying types before this check.
/// The `enums` and `resources` sets contain known enum/resource type names.
fn is_param_type_supported_with_types(
    ty: &Type,
    enums: &HashSet<&str>,
    resources: &HashSet<&str>,
) -> bool {
    match ty {
        Type::Named(named) => {
            let name = named.name.as_str();
            // Check primitives and unit type
            // Unit type () is parsed as Named("()"), not Tuple([])
            // Resource types are passed as borrow<resource> in CM (i32 handle in core wasm)
            matches!(
                name,
                "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "char"
                    | "String"
                    | "()"
            ) || enums.contains(name)
                || resources.contains(name)
        }
        Type::Generic(generic) => matches!(generic.name.as_str(), "Stream" | "Result"),
        _ => false,
    }
}

/// Check if a return type is supported for Component Model generation
///
/// Type aliases (like Instant, Duration) should already be resolved to their
/// underlying types before this check.
/// The `enums` and `resources` sets contain known enum/resource type names.
fn is_return_type_supported_with_types(
    ty: &Type,
    enums: &HashSet<&str>,
    resources: &HashSet<&str>,
) -> bool {
    match ty {
        Type::Named(named) => {
            let name = named.name.as_str();
            // Check primitives, enums, resources, and unit type
            // Unit type () is parsed as Named("()"), not Tuple([])
            matches!(
                name,
                "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "char"
                    | "String"
                    | "()"
            ) || enums.contains(name)
                || resources.contains(name)
        }
        Type::Generic(generic) => match generic.name.as_str() {
            "Stream" => true,
            "Result" => {
                // Result<T, E> - both T and E must be supported
                generic
                    .args
                    .iter()
                    .all(|arg| is_return_type_supported_with_types(arg, enums, resources))
            }
            "Array" | "Option" => {
                // Recursively check that inner types are supported primitives
                generic
                    .args
                    .iter()
                    .all(|arg| is_primitive_type_supported_with_types(arg, enums, resources))
            }
            "Tuple" => {
                // All tuple elements must be supported primitives (Tuple<...> syntax)
                generic
                    .args
                    .iter()
                    .all(|arg| is_primitive_type_supported_with_types(arg, enums, resources))
            }
            _ => false,
        },
        // Handle [...] tuple syntax
        Type::Tuple(elements) => {
            // Empty tuple () is the unit type, which is always supported
            if elements.is_empty() {
                return true;
            }
            elements
                .iter()
                .all(|el| is_primitive_type_supported_with_types(el, enums, resources))
        }
        _ => false,
    }
}

/// Check if a type is a supported primitive type (for inner types of Array/Option/Tuple)
fn is_primitive_type_supported_with_types(
    ty: &Type,
    enums: &HashSet<&str>,
    resources: &HashSet<&str>,
) -> bool {
    match ty {
        Type::Named(named) => {
            let name = named.name.as_str();
            // Unit type () is parsed as Named("()"), not Tuple([])
            matches!(
                name,
                "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "char"
                    | "String"
                    | "()"
            ) || enums.contains(name)
                || resources.contains(name)
        }
        // Handle Tuple<...> syntax
        Type::Generic(generic) if generic.name == "Tuple" => {
            // Tuples are allowed if all elements are primitives
            generic
                .args
                .iter()
                .all(|arg| is_primitive_type_supported_with_types(arg, enums, resources))
        }
        // Handle [...] tuple syntax
        Type::Tuple(elements) => elements
            .iter()
            .all(|el| is_primitive_type_supported_with_types(el, enums, resources)),
        _ => false,
    }
}

/// Check if a parameter type is supported (without enum/resource knowledge)
pub fn is_param_type_supported(ty: &Type) -> bool {
    is_param_type_supported_with_types(ty, &HashSet::new(), &HashSet::new())
}

/// Check if a return type is supported (without enum/resource knowledge)
pub fn is_return_type_supported(ty: &Type) -> bool {
    is_return_type_supported_with_types(ty, &HashSet::new(), &HashSet::new())
}

/// Check if a type is a supported primitive type (for inner types of Array/Option/Tuple)
#[allow(dead_code)]
fn is_primitive_type_supported(ty: &Type) -> bool {
    is_primitive_type_supported_with_types(ty, &HashSet::new(), &HashSet::new())
}

/// Check if all types in a WASI function are supported for Component Model generation
/// (without enum/resource knowledge - use `WasiRegistry::is_function_supported` instead)
pub fn is_wasi_function_supported(func: &WasiFunctionInfo) -> bool {
    // Check all parameter types (Result not allowed in params)
    for (_, ty) in &func.params {
        if !is_param_type_supported(ty) {
            return false;
        }
    }
    // Check return type if present (Result allowed)
    if let Some(ret_ty) = &func.return_type
        && !is_return_type_supported(ret_ty)
    {
        return false;
    }
    true
}

/// Check if a return type requires an outptr parameter in Component Model ABI
///
/// Complex types (list, string, option, result) are returned via linear memory
/// rather than as direct return values. The function signature changes to:
/// - Add an outptr: i32 parameter
/// - Return nothing (result is written to outptr)
pub fn return_type_requires_outptr(ty: &Type) -> bool {
    match ty {
        // Simple types are returned directly
        Type::Named(named) => matches!(
            named.name.as_str(),
            "String" // String (list<u8> in CM) requires outptr
        ),
        // Generic types that require outptr
        Type::Generic(generic) => matches!(
            generic.name.as_str(),
            "Array" | "Option" | "Result" | "Tuple"
        ),
        // Tuple types [...] require outptr (non-empty tuples only)
        Type::Tuple(elems) => !elems.is_empty(),
        _ => false,
    }
}

// ============================================================================
// Component Model Call Convention
// ============================================================================

/// Primitive type for CM tuple return handling
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmPrimitiveType {
    I32,
    I64,
    U32,
    U64,
    F32,
    F64,
}

impl CmPrimitiveType {
    /// Size in bytes
    pub fn size(&self) -> u32 {
        match self {
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }

    /// Alignment in bytes
    pub fn align(&self) -> u32 {
        self.size()
    }
}

/// Component Model ABI call convention
///
/// Describes how to call a CM function and handle its return value.
/// This struct is derived from the function's return type and captures
/// all the information codegen needs without knowing WASI-specific details.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // These are independent properties, not state
pub struct CmCallConvention {
    /// Whether this is an async function (needs subtask handling)
    pub is_async: bool,
    /// Whether `canon lower` requires Memory canonical option
    pub needs_memory: bool,
    /// Whether `canon lower` requires Realloc canonical option
    pub needs_realloc: bool,
    /// If Some, allocate outptr before call: (`size_bytes`, `align_bytes`)
    pub outptr_alloc: Option<(u32, u32)>,
    /// Conversion function to call after the call (if any)
    /// Full path like "`core/internal/cm_list_string_to_array`"
    pub result_converter: Option<String>,
    /// For tuple returns: element types for struct creation
    pub tuple_return: Option<Vec<CmPrimitiveType>>,
    /// For option<own<resource>>: true if needs boxing to Option<i32>
    pub option_resource_return: bool,
    /// For result<T, E> returns: `Some((ok_is_resource`, `err_is_enum`))
    /// `ok_is_resource`: if true, Ok payload is a resource handle (i32)
    /// `err_is_enum`: if true, Err payload is an enum (i32)
    pub result_return: Option<(bool, bool)>,
}

impl CmCallConvention {
    /// Derive call convention from a function's return type
    ///
    /// This analyzes the type and determines all CM ABI requirements,
    /// so codegen doesn't need to know about specific WASI types.
    pub fn from_return_type(return_type: &Option<Type>) -> Self {
        let Some(ty) = return_type else {
            return Self::default();
        };

        match ty {
            // Primitives: direct return, no special handling
            Type::Named(named) => match named.name.as_str() {
                "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" => {
                    Self::default()
                }
                // String return type needs memory/realloc for CM lowering
                // (string is lowered as list<u8> which requires memory allocation)
                "String" => Self {
                    needs_memory: true,
                    needs_realloc: true,
                    outptr_alloc: Some((8, 4)), // ptr + len
                    ..Self::default()
                },
                // Other named types (resources, enums) - use default
                _ => Self::default(),
            },

            // Generic types
            Type::Generic(generic) => match generic.name.as_str() {
                // list<string> -> needs outptr (8 bytes: ptr + count) + converter
                "Array" if generic.args.len() == 1 => Self::for_list_return(&generic.args[0]),

                // option<T> -> depends on T
                "Option" if generic.args.len() == 1 => Self::for_option_return(&generic.args[0]),

                // Tuple<T, U, ...> -> needs outptr + tuple struct creation
                "Tuple" if !generic.args.is_empty() => Self::for_tuple_return(&generic.args),

                // Result<T, E> -> needs outptr
                "Result" if generic.args.len() == 2 => {
                    Self::for_result_return(&generic.args[0], &generic.args[1])
                }

                // Stream<T>, Future<T> - handle returns
                "Stream" | "Future" => Self::default(),

                _ => Self::default(),
            },

            // [T, U, ...] tuple syntax
            Type::Tuple(elems) if !elems.is_empty() => Self::for_tuple_return(elems),

            _ => Self::default(),
        }
    }

    /// Convention for list<T> return types
    fn for_list_return(element_type: &Type) -> Self {
        // list<T> uses outptr (8 bytes: ptr + count, align 4)
        // Needs memory + realloc for lowering
        let converter = match element_type {
            Type::Named(named) if named.name == "String" => {
                Some("core/internal/cm_list_string_to_array".to_string())
            }
            Type::Named(named) if named.name == "u8" => {
                Some("core/internal/cm_list_u8_to_array".to_string())
            }
            // list<tuple<string, string>> for Environment::get_environment
            Type::Tuple(elems) if elems.len() == 2 => {
                if Self::is_string_type(&elems[0]) && Self::is_string_type(&elems[1]) {
                    Some("core/internal/cm_list_tuple_string_string_to_array".to_string())
                } else {
                    None
                }
            }
            // Tuple<String, String> syntax
            Type::Generic(g) if g.name == "Tuple" && g.args.len() == 2 => {
                if Self::is_string_type(&g.args[0]) && Self::is_string_type(&g.args[1]) {
                    Some("core/internal/cm_list_tuple_string_string_to_array".to_string())
                } else {
                    None
                }
            }
            _ => None,
        };

        Self {
            is_async: false,
            needs_memory: true,
            needs_realloc: true,
            outptr_alloc: Some((8, 4)), // ptr + count
            result_converter: converter,
            tuple_return: None,
            option_resource_return: false,
            result_return: None,
        }
    }

    /// Convention for option<T> return types
    fn for_option_return(inner_type: &Type) -> Self {
        match inner_type {
            // option<string> -> outptr (12 bytes: discriminant + ptr + len)
            Type::Named(named) if named.name == "String" => Self {
                is_async: false,
                needs_memory: true,
                needs_realloc: true,
                outptr_alloc: Some((12, 4)),
                result_converter: Some("core/internal/cm_option_string_to_option".to_string()),
                tuple_return: None,
                option_resource_return: false,
                result_return: None,
            },

            // option<own<resource>> -> outptr (4 bytes: 0=none, non-zero=handle)
            // This covers TerminalInput, TerminalOutput, etc.
            Type::Generic(g) if g.name == "Own" => Self {
                is_async: false,
                needs_memory: true,
                needs_realloc: true,
                outptr_alloc: Some((4, 4)),
                result_converter: None,
                tuple_return: None,
                option_resource_return: true,
                result_return: None,
            },

            // option<primitive> - simple case, but still needs outptr
            Type::Named(named) => {
                let prim_size = match named.name.as_str() {
                    "i32" | "u32" | "f32" | "bool" | "char" => 4,
                    "i64" | "u64" | "f64" => 8,
                    // Unknown type - assume resource handle
                    _ => {
                        return Self {
                            is_async: false,
                            needs_memory: true,
                            needs_realloc: true,
                            outptr_alloc: Some((4, 4)),
                            result_converter: None,
                            tuple_return: None,
                            option_resource_return: true,
                            result_return: None,
                        };
                    }
                };
                // Discriminant (1 byte padded to align) + value
                let align = prim_size;
                let size = align + prim_size; // discriminant + padding + value
                Self {
                    is_async: false,
                    needs_memory: true,
                    needs_realloc: true,
                    outptr_alloc: Some((size, align)),
                    result_converter: None,
                    tuple_return: None,
                    option_resource_return: false,
                    result_return: None,
                }
            }

            _ => Self::default(),
        }
    }

    /// Convention for tuple<T, U, ...> return types
    fn for_tuple_return(elements: &[Type]) -> Self {
        let mut primitives = Vec::new();
        let mut total_size: u32 = 0;
        let mut max_align: u32 = 1;

        for elem in elements {
            if let Some(prim) = Self::type_to_primitive(elem) {
                // Align current offset
                let align = prim.align();
                if !total_size.is_multiple_of(align) {
                    total_size += align - (total_size % align);
                }
                total_size += prim.size();
                max_align = max_align.max(align);
                primitives.push(prim);
            } else {
                // Non-primitive in tuple - not supported yet
                return Self::default();
            }
        }

        // Final size must be aligned to max alignment
        if !total_size.is_multiple_of(max_align) {
            total_size += max_align - (total_size % max_align);
        }

        Self {
            is_async: false,
            needs_memory: true,
            needs_realloc: true,
            outptr_alloc: Some((total_size, max_align)),
            result_converter: None,
            tuple_return: Some(primitives),
            option_resource_return: false,
            result_return: None,
        }
    }

    /// Convention for result<T, E> return types
    fn for_result_return(ok_type: &Type, err_type: &Type) -> Self {
        // Check if ok_type is a resource (named type that's not a primitive)
        let ok_is_resource = matches!(ok_type, Type::Named(named) if !matches!(
            named.name.as_str(),
            "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" | "String"
        ));

        // Check if err_type is an enum (named type that's not a primitive)
        let err_is_enum = matches!(err_type, Type::Named(named) if !matches!(
            named.name.as_str(),
            "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" | "String"
        ));

        // Result layout in CM: discriminant (i32) + max(ok_size, err_size)
        // For Result<Resource, Enum>: discriminant (4) + payload (4) = 8 bytes
        Self {
            is_async: false,
            needs_memory: true,
            needs_realloc: true,
            outptr_alloc: Some((8, 4)), // discriminant + payload
            result_converter: None,
            tuple_return: None,
            option_resource_return: false,
            result_return: Some((ok_is_resource, err_is_enum)),
        }
    }

    /// Set the `is_async` flag
    pub fn with_async(mut self, is_async: bool) -> Self {
        self.is_async = is_async;
        // Async functions always need Memory + Realloc for continuation handling
        if is_async {
            self.needs_memory = true;
            self.needs_realloc = true;
        }
        self
    }

    /// Update convention based on parameter types.
    ///
    /// Some parameter types (like Stream<T>) require Memory + Realloc
    /// even if the return type doesn't require it.
    pub fn with_params(mut self, params: &[(String, Type)]) -> Self {
        for (_, ty) in params {
            if Self::type_requires_memory(ty) {
                self.needs_memory = true;
                self.needs_realloc = true;
                break;
            }
        }
        self
    }

    /// Check if a type requires Memory + Realloc in canon lower
    fn type_requires_memory(ty: &Type) -> bool {
        match ty {
            Type::Generic(g) => matches!(g.name.as_str(), "Stream" | "Array"),
            Type::Named(named) => named.name == "String",
            _ => false,
        }
    }

    /// Check if a type is String
    fn is_string_type(ty: &Type) -> bool {
        matches!(ty, Type::Named(named) if named.name == "String")
    }

    /// Convert AST Type to `CmPrimitiveType`
    fn type_to_primitive(ty: &Type) -> Option<CmPrimitiveType> {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i32" => Some(CmPrimitiveType::I32),
                "i64" => Some(CmPrimitiveType::I64),
                "u32" => Some(CmPrimitiveType::U32),
                "u64" => Some(CmPrimitiveType::U64),
                "f32" => Some(CmPrimitiveType::F32),
                "f64" => Some(CmPrimitiveType::F64),
                _ => None,
            },
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Span;

    fn make_span() -> Span {
        Span::new(0, 0, 1, 1)
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
            call_convention: CmCallConvention::default(),
        };
        assert_eq!(
            func_info.local_alias_name(),
            "wasi:cli/Stdout::write_via_stream"
        );
    }

    #[test]
    fn test_cm_call_convention_from_return_type() {
        // Test primitives - no special handling
        let conv = CmCallConvention::from_return_type(&Some(Type::Named(crate::ast::NamedType {
            name: "i32".to_string(),
            span: make_span(),
        })));
        assert!(conv.outptr_alloc.is_none());
        assert!(conv.result_converter.is_none());

        // Test list<string>
        let conv =
            CmCallConvention::from_return_type(&Some(Type::Generic(crate::ast::GenericType {
                name: "Array".to_string(),
                args: vec![Type::Named(crate::ast::NamedType {
                    name: "String".to_string(),
                    span: make_span(),
                })],
                span: make_span(),
            })));
        assert_eq!(conv.outptr_alloc, Some((8, 4)));
        assert_eq!(
            conv.result_converter,
            Some("core/internal/cm_list_string_to_array".to_string())
        );
        assert!(conv.needs_memory);
        assert!(conv.needs_realloc);

        // Test tuple<u64, u64>
        let conv = CmCallConvention::from_return_type(&Some(Type::Tuple(vec![
            Type::Named(crate::ast::NamedType {
                name: "u64".to_string(),
                span: make_span(),
            }),
            Type::Named(crate::ast::NamedType {
                name: "u64".to_string(),
                span: make_span(),
            }),
        ])));
        assert_eq!(conv.outptr_alloc, Some((16, 8)));
        assert!(conv.result_converter.is_none());
        assert_eq!(
            conv.tuple_return,
            Some(vec![CmPrimitiveType::U64, CmPrimitiveType::U64])
        );
    }

    #[test]
    fn test_array_and_option_type_support() {
        use crate::ast::{GenericType, NamedType};

        // Array<String> should be supported
        let array_string = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert!(
            is_return_type_supported(&array_string),
            "Array<String> should be supported"
        );

        // Array<Tuple<String, String>> should be supported
        let tuple_ss = Type::Generic(GenericType {
            name: "Tuple".to_string(),
            args: vec![
                Type::Named(NamedType {
                    name: "String".to_string(),
                    span: make_span(),
                }),
                Type::Named(NamedType {
                    name: "String".to_string(),
                    span: make_span(),
                }),
            ],
            span: make_span(),
        });
        let array_tuple = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![tuple_ss],
            span: make_span(),
        });
        assert!(
            is_return_type_supported(&array_tuple),
            "Array<Tuple<String, String>> should be supported"
        );

        // Option<String> should be supported
        let option_string = Type::Generic(GenericType {
            name: "Option".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert!(
            is_return_type_supported(&option_string),
            "Option<String> should be supported"
        );
    }

    #[test]
    fn test_random_functions_registered() {
        let (registry, _) = WasiRegistry::build_from_stdlib();

        // Check that Random functions are registered
        assert!(
            registry.resolve("Random::get_random_u64").is_some(),
            "Random::get_random_u64 should be resolved"
        );
        assert!(
            registry.resolve("Random::get_random_bytes").is_some(),
            "Random::get_random_bytes should be resolved"
        );
        assert!(
            registry
                .resolve("Insecure::get_insecure_random_u64")
                .is_some(),
            "Insecure::get_insecure_random_u64 should be resolved"
        );
        assert!(
            registry
                .resolve("Insecure::get_insecure_random_bytes")
                .is_some(),
            "Insecure::get_insecure_random_bytes should be resolved"
        );

        // Check that the random interface is included
        let interfaces: Vec<_> = registry.interfaces().collect();
        let random_interface = interfaces
            .iter()
            .find(|i| i.interface == "random" && i.package == "random");
        assert!(
            random_interface.is_some(),
            "wasi:random/random interface should be registered"
        );
    }

    #[test]
    fn test_sockets_resource_methods_registered() {
        let (registry, _) = WasiRegistry::build_from_stdlib();

        // Check that TcpSocket resource methods are registered
        let resolved = registry.resolve("TcpSocket::static_tcp_socket_create");
        assert!(
            resolved.is_some(),
            "TcpSocket::static_tcp_socket_create should be resolved, got {:?}",
            resolved
        );

        // Check that the sockets types interface is included
        let interfaces: Vec<_> = registry.interfaces().collect();
        let sockets_interface = interfaces
            .iter()
            .find(|i| i.interface == "types" && i.package == "sockets");
        assert!(
            sockets_interface.is_some(),
            "wasi:sockets/types interface should be registered"
        );
    }
}
