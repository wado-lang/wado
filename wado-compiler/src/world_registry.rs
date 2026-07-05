//! World registry for Component Model world definitions
//!
//! This module collects world definitions from lib/wasi/*.wado
//! and provides export signature information for code generation.
//!
//! Worlds are keyed by their fully-qualified name (e.g., "wasi:cli/command",
//! "wasi:http/service") derived from the `#[cm("...")]` attribute.

use crate::hashmap::IndexMap;

use crate::ast::{Type, WorldDecl, WorldExport, WorldExportFn, WorldImport};

/// Well-known world name for the test world.
///
/// When `--world test` is specified, the compiler treats test functions as the
/// component's exports and DCEs everything else. This is a synthetic world
/// that is not registered in the `WorldRegistry` (its exports are derived
/// dynamically from the entry module's `TirTest` declarations).
pub const TEST_WORLD: &str = "test";

/// Information about a world export function.
///
/// World exports take two AST shapes (`export Foo;` interface form and
/// `export fn name(...);` function form). Both are normalized into this
/// flat struct: an `export Foo;` is expanded by the registry into one
/// `WorldExportInfo` per interface method, with [`from_interface_fq`]
/// populated. Bare function exports leave `from_interface_fq` as `None`.
#[derive(Debug, Clone)]
pub struct WorldExportInfo {
    /// Function name (e.g., `"run"`, `"handle"`)
    pub name: String,
    /// Whether this is an async function
    pub is_async: bool,
    /// Parameter types
    pub params: Vec<(String, Type)>,
    /// Return type (if any)
    pub return_type: Option<Type>,
    /// CM FQ of the parent interface when this export was synthesized from
    /// an `export Foo;` interface reference (e.g.,
    /// `"wasi:http/handler@0.3.0"`). `None` for direct
    /// `export fn ...` declarations.
    pub from_interface_fq: Option<String>,
}

impl WorldExportInfo {
    /// Build a `WorldExportInfo` from a function-form AST export. Interface
    /// exports are not handled here; they are expanded at registration time
    /// using interface signatures provided to [`WorldRegistry::register`].
    fn from_function_ast(export: &WorldExportFn) -> Self {
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
            from_interface_fq: None,
        }
    }

    /// Whether this is a handler-instance export: an interface export
    /// (`from_interface_fq` populated) returning a non-unit `Result<_, _>` — the
    /// shape of `wasi:http/handler#handle`. The `from_interface_fq` gate excludes
    /// freestanding exports of the same shape (the kiln generator's `generate`).
    pub fn is_handler_instance_export(&self) -> bool {
        self.from_interface_fq.is_some() && returns_non_unit_result(self.return_type.as_ref())
    }
}

/// Resolved view of an interface's exported methods, supplied by the caller
/// when registering a world. Contains everything `WorldRegistry` needs to
/// expand `export Foo;` into per-method `WorldExportInfo` entries.
#[derive(Debug, Clone)]
pub struct InterfaceExportLookup {
    pub cm_interface_fq: String,
    pub methods: Vec<InterfaceExportMethod>,
}

#[derive(Debug, Clone)]
pub struct InterfaceExportMethod {
    pub name: String,
    pub is_async: bool,
    pub params: Vec<(String, Type)>,
    pub return_type: Option<Type>,
}

/// Information about a world's imported interface
/// (a single `import Foo;` line in a Wado world declaration).
///
/// World imports are bare interface references (WIT-faithful). The
/// `interface_name` is the locally-bound name; resolving it to a CM interface
/// FQ goes through the referenced `pub interface Foo` declaration's
/// `#[cm("...")]` attribute and is captured here as `cm_interface_fq` when
/// known.
#[derive(Debug, Clone)]
pub struct WorldImportInfo {
    /// Imported interface name (e.g., `"Stdout"`, `"KilnHost"`).
    pub interface_name: String,
    /// CM FQ name resolved from the referenced interface declaration, when
    /// known. `None` while the interface has not been registered.
    pub cm_interface_fq: Option<String>,
}

impl WorldImportInfo {
    /// Create from a parsed [`WorldImport`].
    pub fn from_ast(import: &WorldImport) -> Self {
        Self {
            interface_name: import.interface_name.clone(),
            cm_interface_fq: None,
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

    /// Whether this world exports the WASI HTTP handler — a handler-instance
    /// export whose interface is in the `http` package. Gates HTTP-specific
    /// behavior (importing `wasi:http/types`, the free-list allocator), so the
    /// package check keeps a non-HTTP handler-shaped export (e.g. a future
    /// `acme:widget/handler`) out of the HTTP path. The generic instance-export
    /// wrapping for all interface exports lives in
    /// `append_interface_instance_exports`.
    pub fn has_http_handler_export(&self) -> bool {
        self.exports.iter().any(|e| {
            e.is_handler_instance_export()
                && e.from_interface_fq.as_deref().map(fq_name_package) == Some("http")
        })
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
    /// any future world that imports the same interface, replacing string
    /// matches against `target_world == "core:kiln/generator"`.
    pub fn imports_interface(&self, interface_name: &str) -> bool {
        self.imports
            .iter()
            .any(|i| i.interface_name == interface_name)
    }
}

/// Whether `return_type` is a `Result<Ok, Err>` with at least one non-unit
/// arm — the "handler" return shape. `Result<(), ()>` (the CLI `Run` shape)
/// and any non-`Result` type return `false`.
fn returns_non_unit_result(return_type: Option<&Type>) -> bool {
    let Some(Type::Generic(generic)) = return_type else {
        return false;
    };
    generic.name == "Result"
        && generic.args.len() == 2
        && !(crate::component_model::is_unit_type(&generic.args[0])
            && crate::component_model::is_unit_type(&generic.args[1]))
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
        .find_map(|a| a.as_cm_import().map(crate::ast::CmImport::bare_path))
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
    /// `lookup_interface_export` resolves an `export Foo;` interface reference
    /// to the interface's CM FQ and methods. Interface exports for which the
    /// callback returns `None` are silently skipped (the world will be
    /// registered without that export); this happens when the referenced
    /// interface declaration is not yet known.
    ///
    /// `lookup_interface_import` is the analogous elaborator for `import Foo;`
    /// and only needs to surface the CM FQ (the methods are tree-shaken from
    /// usage). Returning `None` leaves `cm_interface_fq` unfilled.
    ///
    /// If another world is already registered under the same `fq_name`, the
    /// first registrant is kept and this registration is skipped (with a
    /// warning logged to stderr). Two distinct worlds sharing a fully-qualified
    /// name is a bug in the stdlib or user input — silently overwriting would
    /// make the earlier world invisible to the code generator.
    pub fn register(
        &mut self,
        world: &WorldDecl,
        lookup_interface_export: impl Fn(&str) -> Option<InterfaceExportLookup>,
        lookup_interface_import: impl Fn(&str) -> Option<String>,
    ) {
        let fq_name = fq_name_from_attrs(&world.attrs).unwrap_or_else(|| world.name.clone());

        if self.worlds.contains_key(&fq_name) {
            eprintln!(
                "WorldRegistry: duplicate world `{fq_name}` (also declared as `{}`). \
                 Keeping the first registrant.",
                world.name,
            );
            return;
        }

        let mut exports: Vec<WorldExportInfo> = Vec::new();
        for export in &world.exports {
            match export {
                WorldExport::Function(func) => {
                    exports.push(WorldExportInfo::from_function_ast(func));
                }
                WorldExport::Interface(iface) => {
                    let Some(lookup) = lookup_interface_export(&iface.interface_name) else {
                        // The two-pass stdlib bootstrap registers every
                        // `pub interface Foo` before any world, so a missing
                        // lookup at this point is an actual problem
                        // (mismatched name, missing `#[cm(...)]`, an
                        // out-of-tree world that names an undeclared
                        // interface). Warn so the failure is diagnosable;
                        // the world is still registered with the rest of
                        // its exports rather than aborting.
                        eprintln!(
                            "WorldRegistry: interface export `{}` in world `{fq_name}` \
                             references an unknown interface; skipping. \
                             Check for a missing `pub interface {}` or `#[cm(\"...\")]`.",
                            iface.interface_name, iface.interface_name,
                        );
                        continue;
                    };
                    for method in lookup.methods {
                        exports.push(WorldExportInfo {
                            name: method.name,
                            is_async: method.is_async,
                            params: method.params,
                            return_type: method.return_type,
                            from_interface_fq: Some(lookup.cm_interface_fq.clone()),
                        });
                    }
                }
            }
        }

        let imports: Vec<WorldImportInfo> = world
            .imports
            .iter()
            .map(|imp| WorldImportInfo {
                interface_name: imp.interface_name.clone(),
                cm_interface_fq: lookup_interface_import(&imp.interface_name),
            })
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

    /// Every export name across all registered worlds. A user function whose
    /// name matches one is a potential world entry point — liveness seeds it so
    /// a misdeclared entry (e.g. `fn run()` without `export`) survives reify
    /// gating and reaches the world-conformance check.
    pub fn all_export_names(&self) -> impl Iterator<Item = &str> {
        self.worlds
            .values()
            .flat_map(|w| w.exports.iter().map(|e| e.name.as_str()))
    }

    /// Check if a world is registered
    pub fn has_world(&self, fq_name: &str) -> bool {
        self.worlds.contains_key(fq_name)
    }

    /// Whether `world` is registered and imports `interface_name`. The single
    /// predicate behind kiln-generator detection (`imports_interface("KilnHost")`),
    /// so the frontend, `cm_binding`, and codegen all agree on what counts as a
    /// generator world instead of some string-matching `core:kiln/generator`.
    pub fn world_imports_interface(&self, world: &str, interface_name: &str) -> bool {
        self.get(world)
            .is_some_and(|w| w.imports_interface(interface_name))
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
            visibility: crate::ast::Visibility::Private,
            attrs: vec![Attribute {
                name: "cm".to_string(),
                args: vec![ast::AttrArg::Str("wasi:cli/command@0.3.0".to_string())],
                cm_boundary: Some(crate::ast::CmBoundary::Import(
                    crate::ast::CmImport::parse("wasi:cli/command@0.3.0").unwrap(),
                )),
                span: make_span(),
            }],
            imports: vec![],
            exports: vec![WorldExport::Function(crate::ast::WorldExportFn {
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
            })],
            span: make_span(),
        };

        registry.register(&world, |_| None, |_| None);

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
        assert!(run_export.from_interface_fq.is_none());
    }

    #[test]
    fn test_parse_cli_wado() {
        // Use the full stdlib bootstrap so that the `Run` interface is known
        // when registering the `wasi:cli/command` world. Without it,
        // `export Run;` would expand to zero methods (lookup callback returns
        // `None`).
        let (_registry, world_registry) =
            crate::component_model::CmInterfaceRegistry::build_from_stdlib();

        assert!(
            world_registry.has_world("wasi:cli/command"),
            "wasi:cli/command world not found"
        );

        // `export Run;` expands to the `run` method.
        let run_export = world_registry
            .get_export("wasi:cli/command", "run")
            .expect("run export not found");
        assert!(run_export.is_async, "run should be async");
        assert!(run_export.params.is_empty(), "run should have no params");
        assert_eq!(
            run_export.from_interface_fq.as_deref(),
            Some("wasi:cli/run@0.3.0"),
            "run should be tagged with its parent interface FQ"
        );
    }

    #[test]
    fn test_kiln_generator_world_registered() {
        let (_registry, world_registry) =
            crate::component_model::CmInterfaceRegistry::build_from_stdlib();
        assert!(
            world_registry.has_world("core:kiln/generator"),
            "core:kiln/generator world should be registered"
        );
        // Revision 3: the static world is an import-only template — each generator
        // synthesizes its own world with a typed `generate` (its `options`
        // shape is per-generator), so the static world carries no `generate`
        // export. It exists to hold the `KilnHost` import and anchor the
        // `core:kiln/generator` identity that detection keys on.
        let world = world_registry
            .get("core:kiln/generator")
            .expect("world registered");
        assert!(
            world.imports_interface("KilnHost"),
            "the generator world imports KilnHost"
        );
        assert!(
            world.exports.is_empty(),
            "the static generator world declares no exports"
        );
    }

    fn world_info(fq: &str) -> WorldInfo {
        WorldInfo {
            fq_name: fq.to_string(),
            exports: Vec::new(),
            imports: Vec::new(),
        }
    }

    /// Build a `Result<Ok, Err>` return type from two named-type names.
    fn result_return(ok: &str, err: &str) -> Type {
        Type::Generic(crate::ast::GenericType {
            id: crate::ast::AstId::fresh(),
            name: "Result".to_string(),
            args: vec![
                Type::Named(crate::ast::NamedType::new(
                    crate::ast::AstId::fresh(),
                    ok.to_string(),
                    make_span(),
                )),
                Type::Named(crate::ast::NamedType::new(
                    crate::ast::AstId::fresh(),
                    err.to_string(),
                    make_span(),
                )),
            ],
            span: make_span(),
        })
    }

    #[test]
    fn test_has_http_handler_export_from_stdlib() {
        // Characterization: the stdlib worlds keep their current classification
        // after the detection logic moves from namespace+`Response` sniffing to
        // the structural `from_interface_fq` + non-unit `Result` check.
        let (_registry, world_registry) =
            crate::component_model::CmInterfaceRegistry::build_from_stdlib();

        assert!(
            world_registry
                .get("wasi:http/service")
                .unwrap()
                .has_http_handler_export(),
            "wasi:http/service exports the Handler interface (handle -> Result<Response, _>)"
        );
        assert!(
            !world_registry
                .get("wasi:cli/command")
                .unwrap()
                .has_http_handler_export(),
            "wasi:cli/command's Run export returns Result<(), ()> — not a handler instance"
        );
        assert!(
            !world_registry
                .get("core:kiln/generator")
                .unwrap()
                .has_http_handler_export(),
            "kiln generate returns a non-unit Result but is a freestanding export \
             (from_interface_fq is None), so it is not a handler instance export"
        );
    }

    #[test]
    fn test_http_handler_detection_is_package_precise() {
        // `is_handler_instance_export` is the generic shape predicate (an
        // interface export returning a non-unit `Result`), detected with no
        // dependence on the `wasi:http/` namespace or the `Response` type name.
        // `has_http_handler_export` additionally requires the parent interface
        // to be in the `http` package, so it gates HTTP-specific behavior
        // precisely and does not misfire for a non-HTTP handler-shaped export.
        let http_handler = WorldExportInfo {
            name: "handle".to_string(),
            is_async: true,
            params: vec![],
            return_type: Some(result_return("Response", "ErrorCode")),
            from_interface_fq: Some("wasi:http/handler@0.3.0".to_string()),
        };
        let mut http_world = world_info("wasi:http/service");
        http_world.exports.push(http_handler);
        assert!(http_world.exports[0].is_handler_instance_export());
        assert!(
            http_world.has_http_handler_export(),
            "an http-package handler export gates the HTTP path"
        );

        // A third-party handler-shaped export has the same generic shape but is
        // NOT an HTTP handler — it must not be routed into the HTTP-types import
        // path.
        let mut widget_world = world_info("acme:widget/service");
        widget_world.exports.push(WorldExportInfo {
            name: "handle".to_string(),
            is_async: true,
            params: vec![],
            return_type: Some(result_return("Widget", "WidgetError")),
            from_interface_fq: Some("acme:widget/handler@1.0.0".to_string()),
        });
        assert!(
            widget_world.exports[0].is_handler_instance_export(),
            "the generic shape predicate is namespace-independent"
        );
        assert!(
            !widget_world.has_http_handler_export(),
            "a non-HTTP handler-shaped export is not an HTTP handler"
        );

        // The same return shape on a freestanding export (no parent interface)
        // is the kiln pattern — not a handler instance export at all.
        let mut freestanding = http_world.clone();
        freestanding.exports[0].from_interface_fq = None;
        assert!(!freestanding.exports[0].is_handler_instance_export());
        assert!(!freestanding.has_http_handler_export());

        // A unit `Result<(), ()>` interface export (the CLI Run shape) is not a
        // handler instance even with a parent interface FQ.
        let mut unit_export = world_info("wasi:http/command");
        unit_export.exports.push(WorldExportInfo {
            name: "run".to_string(),
            is_async: true,
            params: vec![],
            return_type: Some(result_return_unit()),
            from_interface_fq: Some("wasi:http/run@1.0.0".to_string()),
        });
        assert!(!unit_export.exports[0].is_handler_instance_export());
        assert!(!unit_export.has_http_handler_export());
    }

    /// `Result<(), ()>` built from empty tuples.
    fn result_return_unit() -> Type {
        Type::Generic(crate::ast::GenericType {
            id: crate::ast::AstId::fresh(),
            name: "Result".to_string(),
            args: vec![Type::Tuple(vec![]), Type::Tuple(vec![])],
            span: make_span(),
        })
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
        let (_registry, world_registry) =
            crate::component_model::CmInterfaceRegistry::build_from_stdlib();

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
