//! World registry for Component Model world definitions
//!
//! This module collects world definitions from lib/wasi/*.wado
//! and provides export signature information for code generation.
//!
//! Worlds are keyed by their fully-qualified name (e.g., "wasi:cli/command",
//! "wasi:http/service") derived from the `#[cm("...")]` attribute.

use crate::hashmap::IndexMap;

use crate::ast::{Type, WorldDecl, WorldExport, WorldImport};

/// Well-known world name for the test world.
///
/// When `--world test` is specified, the compiler treats test functions as the
/// component's exports and DCEs everything else. This is a synthetic world
/// that is not registered in the `WorldRegistry` (its exports are derived
/// dynamically from the entry module's `TirTest` declarations).
pub const TEST_WORLD: &str = "test";

/// Information about a world export function
#[derive(Debug, Clone)]
pub struct WorldExportInfo {
    /// Function name (e.g., "run")
    pub name: String,
    /// Whether this is an async function
    pub is_async: bool,
    /// Parameter types
    pub params: Vec<(String, Type)>,
    /// Return type (if any)
    pub return_type: Option<Type>,
}

impl WorldExportInfo {
    /// Create from a parsed `WorldExport`
    pub fn from_ast(export: &WorldExport) -> Self {
        let params = export
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();

        Self {
            name: export.name.clone(),
            is_async: export.is_async,
            params,
            return_type: export.return_type.clone(),
        }
    }

    /// Check if this export returns an HTTP response.
    ///
    /// Returns true if the return type is `Result<Response, ErrorCode>`.
    pub fn returns_http_response(&self) -> bool {
        let Some(return_type) = &self.return_type else {
            return false;
        };

        if let Type::Generic(generic) = return_type
            && generic.name == "Result"
            && generic.args.len() == 2
            && let Type::Named(ok_type) = &generic.args[0]
        {
            return ok_type.name == "Response";
        }
        false
    }
}

/// Information about a world's imported interface (a single `import Interface { ... }`
/// block in a Wado world declaration).
///
/// Mirrors the [`crate::ast::WorldImport`] AST node but lives outside the AST
/// graph so codegen can ask "does this world import `KilnHost`?" without
/// re-parsing the world declaration. The `interface_name` is the locally-bound
/// name (the one written in `import Foo { ... }`); resolving it to a CM
/// interface FQ requires the [`crate::component_model::WasiRegistry`] and is
/// done lazily by the consumer.
#[derive(Debug, Clone)]
pub struct WorldImportInfo {
    /// Imported interface name (e.g., `"Stdout"`, `"Types"`, `"KilnHost"`).
    pub interface_name: String,
    /// Functions / methods picked from the interface.
    pub functions: Vec<String>,
}

impl WorldImportInfo {
    /// Create from a parsed [`WorldImport`].
    pub fn from_ast(import: &WorldImport) -> Self {
        Self {
            interface_name: import.interface_name.clone(),
            functions: import.functions.clone(),
        }
    }
}

/// Information about a world definition
#[derive(Debug, Clone)]
pub struct WorldInfo {
    /// Fully-qualified world name (e.g., "wasi:cli/command")
    pub fq_name: String,
    /// Exported functions
    pub exports: Vec<WorldExportInfo>,
    /// Imported interfaces (one entry per `import Interface { ... }` block).
    pub imports: Vec<WorldImportInfo>,
}

impl WorldInfo {
    /// Check if this world has any async export.
    pub fn has_async_export(&self) -> bool {
        self.exports.iter().any(|e| e.is_async)
    }

    /// Check if this world has an HTTP handler export.
    ///
    /// Keyed on the world's fully-qualified name rather than the return-type
    /// shape so same-named types in other namespaces — notably
    /// `core:kiln/types::Response` under the `core:kiln/generator` world —
    /// cannot be mistaken for `wasi:http/types::Response` and route the
    /// generator through the HTTP codegen branch.
    pub fn has_http_handler_export(&self) -> bool {
        self.namespace_prefix() == "wasi:http/"
            && self
                .exports
                .iter()
                .any(WorldExportInfo::returns_http_response)
    }

    /// The CM package segment of this world's fully-qualified name.
    ///
    /// `wasi:http/service` → `"http"`, `core:kiln/generator` → `"kiln"`.
    /// Returns an empty slice when `fq_name` has no scheme/package
    /// structure (e.g. the built-in synthetic `test` world).
    ///
    /// This is a computed accessor, not a stored field — `fq_name` is the
    /// single source of truth. Use [`Self::namespace_prefix`] when the
    /// caller needs a `starts_with`-friendly form that also pins the
    /// scheme.
    pub fn package(&self) -> &str {
        fq_name_package(&self.fq_name)
    }

    /// Namespace prefix usable with `ModuleSource::starts_with` / type
    /// registry lookups that scope by `(name, namespace)`.
    ///
    /// `wasi:http/service` → `"wasi:http/"`, `core:kiln/generator` →
    /// `"core:kiln/"`. Returns an empty slice for bare-name worlds.
    pub fn namespace_prefix(&self) -> &str {
        fq_name_namespace_prefix(&self.fq_name)
    }

    /// True when this world has an `import {interface_name} { ... }` block.
    ///
    /// The check is by locally-bound interface name (the identifier written in
    /// the world declaration), which is unique within a world's imports.
    /// Codegen uses this to drive world-shape decisions from data — e.g.
    /// `imports_interface("KilnHost")` is true for the kiln generator world and
    /// any future world that imports the same effect, replacing string
    /// matches against `target_world == "core:kiln/generator"`.
    pub fn imports_interface(&self, interface_name: &str) -> bool {
        self.imports.iter().any(|i| i.interface_name == interface_name)
    }
}

/// Extract the package segment of a CM-style fully-qualified name
/// (`"wasi:http/service"` → `"http"`; `"core:kiln/generator"` →
/// `"kiln"`). Returns `""` when the name has no `scheme:package/...` shape.
///
/// Works for both world `fq_name`s (`wasi:http/service`) and interface FQs
/// (`wasi:http/types`) — the parsing only cares about the `scheme:pkg/...`
/// prefix and ignores whatever follows the `/`.
pub fn fq_name_package(fq_name: &str) -> &str {
    let Some((_, rest)) = fq_name.split_once(':') else {
        return "";
    };
    rest.split_once('/').map_or("", |(pkg, _)| pkg)
}

/// Extract the namespace prefix (`scheme:package/`) of a world
/// `fq_name`. Returns `""` when the name has no structure.
fn fq_name_namespace_prefix(fq_name: &str) -> &str {
    let after_slash = fq_name.find('/').map(|i| i + 1).unwrap_or(0);
    if after_slash == 0 || !fq_name[..after_slash].contains(':') {
        return "";
    }
    &fq_name[..after_slash]
}

/// Registry of world definitions for code generation
///
/// Collects world definitions from wasi/*.wado and provides:
/// - Export signature lookup for component generation
///
/// Worlds are keyed by fully-qualified name (e.g., "wasi:cli/command").
#[derive(Debug, Clone, Default)]
pub struct WorldRegistry {
    /// `fq_name` -> world info (e.g., "wasi:cli/command" -> `WorldInfo`)
    worlds: IndexMap<String, WorldInfo>,
}

/// Extract the fully-qualified world name (without version) from `#[cm("...")]` attribute.
///
/// For example, `#[cm("wasi:cli/command@0.3.0-rc-2026-01-06")]` returns `"wasi:cli/command"`.
fn fq_name_from_attrs(attrs: &[crate::ast::Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == "cm")
        .and_then(|a| a.args.first())
        .map(|arg| {
            let s = arg.as_str();
            // Strip version suffix (e.g., "@0.3.0-rc-2026-01-06")
            if let Some(at_pos) = s.find('@') {
                s[..at_pos].to_string()
            } else {
                s.to_string()
            }
        })
}

impl WorldRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a world from a parsed world declaration.
    ///
    /// The world is keyed by its fully-qualified name from the `#[cm("...")]` attribute.
    /// If no attribute is present, the `PascalCase` name is used as fallback.
    ///
    /// If another world is already registered under the same `fq_name`, the
    /// first registrant is kept and this registration is skipped (with a
    /// warning logged to stderr). Two distinct worlds sharing a fully-qualified
    /// name is a bug in the stdlib or user input — silently overwriting would
    /// make the earlier world invisible to the code generator.
    pub fn register(&mut self, world: &WorldDecl) {
        let fq_name = fq_name_from_attrs(&world.attrs).unwrap_or_else(|| world.name.clone());

        if self.worlds.contains_key(&fq_name) {
            eprintln!(
                "WorldRegistry: duplicate world `{fq_name}` (also declared as `{}`). \
                 Keeping the first registrant.",
                world.name,
            );
            return;
        }

        let exports = world
            .exports
            .iter()
            .map(WorldExportInfo::from_ast)
            .collect();
        let imports = world
            .imports
            .iter()
            .map(WorldImportInfo::from_ast)
            .collect();

        let info = WorldInfo {
            fq_name: fq_name.clone(),
            exports,
            imports,
        };

        self.worlds.insert(fq_name, info);
    }

    /// Get world info by fully-qualified name (e.g., "wasi:cli/command")
    pub fn get(&self, fq_name: &str) -> Option<&WorldInfo> {
        self.worlds.get(fq_name)
    }

    /// Get an export from a specific world
    pub fn get_export(&self, fq_name: &str, export_name: &str) -> Option<&WorldExportInfo> {
        self.worlds
            .get(fq_name)
            .and_then(|w| w.exports.iter().find(|e| e.name == export_name))
    }

    /// Check if a world is registered
    pub fn has_world(&self, fq_name: &str) -> bool {
        self.worlds.contains_key(fq_name)
    }

    /// Get the number of registered worlds
    pub fn len(&self) -> usize {
        self.worlds.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.worlds.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{self, Attribute};
    use crate::token::Span;

    fn make_span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = WorldRegistry::new();

        let world = WorldDecl {
            id: crate::ast::AstId::fresh(),
            name: "Command".to_string(),
            is_pub: false,
            attrs: vec![Attribute {
                name: "cm".to_string(),
                args: vec![ast::AttrArg::Str("wasi:cli/command@0.3.0".to_string())],
                cm_import: None,
                span: make_span(),
            }],
            imports: vec![],
            exports: vec![WorldExport {
                name: "run".to_string(),
                is_async: true,
                params: vec![],
                return_type: Some(Type::Generic(crate::ast::GenericType {
                    id: crate::ast::AstId::fresh(),
                    name: "Result".to_string(),
                    args: vec![Type::Tuple(vec![]), Type::Tuple(vec![])],
                    span: make_span(),
                })),
                span: make_span(),
            }],
            span: make_span(),
        };

        registry.register(&world);

        assert!(registry.has_world("wasi:cli/command"));
        assert!(!registry.has_world("Command"));
        assert!(!registry.has_world("Unknown"));

        let info = registry.get("wasi:cli/command").unwrap();
        assert_eq!(info.fq_name, "wasi:cli/command");
        assert_eq!(info.exports.len(), 1);

        let run_export = registry.get_export("wasi:cli/command", "run").unwrap();
        assert_eq!(run_export.name, "run");
        assert!(run_export.is_async);
        assert!(run_export.params.is_empty());
        assert!(run_export.return_type.is_some());
    }

    #[test]
    fn test_parse_cli_wado() {
        use crate::ast::Item;
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::stdlib::WASI_CLI_WORLDS;

        // Parse cli/worlds.wado (worlds moved to per-interface sub-files)
        let mut lexer = Lexer::new(WASI_CLI_WORLDS);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        let module = parser.parse().expect("parser error");

        // Build registry
        let mut registry = WorldRegistry::new();
        for item in &module.items {
            if let Item::World(world) = item {
                registry.register(world);
            }
        }

        // Verify Command world is registered by fq name
        assert!(
            registry.has_world("wasi:cli/command"),
            "wasi:cli/command world not found"
        );

        // Verify run export
        let run_export = registry
            .get_export("wasi:cli/command", "run")
            .expect("run export not found");
        assert!(run_export.is_async, "run should be async");
        assert!(run_export.params.is_empty(), "run should have no params");
    }

    #[test]
    fn test_kiln_generator_world_registered() {
        let (_registry, world_registry) = crate::component_model::WasiRegistry::build_from_stdlib();
        assert!(
            world_registry.has_world("core:kiln/generator"),
            "core:kiln/generator world should be registered"
        );
        let generate = world_registry
            .get_export("core:kiln/generator", "generate")
            .expect("generate export not found");
        assert_eq!(generate.params.len(), 1, "generate takes one parameter");
        assert!(
            generate.is_async,
            "generate is declared `async func` — the CM lift uses task.return \
             and the wasmtime runtime drives the call through Accessor"
        );
    }

    fn world_info(fq: &str) -> WorldInfo {
        WorldInfo {
            fq_name: fq.to_string(),
            exports: Vec::new(),
            imports: Vec::new(),
        }
    }

    #[test]
    fn test_fq_name_accessors_wasi() {
        let w = world_info("wasi:http/service");
        assert_eq!(w.package(), "http");
        assert_eq!(w.namespace_prefix(), "wasi:http/");
    }

    #[test]
    fn test_fq_name_accessors_kiln() {
        let w = world_info("core:kiln/generator");
        assert_eq!(w.package(), "kiln");
        assert_eq!(w.namespace_prefix(), "core:kiln/");
    }

    #[test]
    fn test_fq_name_accessors_cli() {
        let w = world_info("wasi:cli/command");
        assert_eq!(w.package(), "cli");
        assert_eq!(w.namespace_prefix(), "wasi:cli/");
    }

    #[test]
    fn test_fq_name_accessors_bare_name() {
        // Synthesized worlds without a `scheme:pkg/iface` shape (e.g. the
        // fallback when no `#[cm(...)]` is present) surface empty slices,
        // not partial parses.
        let w = world_info("test");
        assert_eq!(w.package(), "");
        assert_eq!(w.namespace_prefix(), "");
    }

    #[test]
    fn test_fq_name_accessors_no_scheme() {
        // `name/with-slash-but-no-scheme` has no scheme prefix, so
        // `namespace_prefix` returns `""` rather than a misleading
        // `"name/"`.
        let w = world_info("orphan/x");
        assert_eq!(w.package(), "");
        assert_eq!(w.namespace_prefix(), "");
    }

    #[test]
    fn test_imports_populated_from_stdlib() {
        // Real-world check: the kiln Generator world declares
        // `import KilnHost { ... }` in `lib/core/kiln/worlds.wado`, so the
        // populated registry must surface that import. The cli Command world
        // imports several effects (Stdout, Stdin, Environment, ...).
        let (_registry, world_registry) = crate::component_model::WasiRegistry::build_from_stdlib();

        let kiln = world_registry
            .get("core:kiln/generator")
            .expect("kiln world registered");
        assert!(
            kiln.imports_interface("KilnHost"),
            "kiln Generator imports KilnHost — expected by step-2c codegen gating"
        );
        assert!(!kiln.imports_interface("Stdout"));

        let cli = world_registry
            .get("wasi:cli/command")
            .expect("cli world registered");
        assert!(cli.imports_interface("Stdout"));
        assert!(!cli.imports_interface("KilnHost"));
    }
}
