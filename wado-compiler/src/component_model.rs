//! Component Model support for code generation
//!
//! This module provides:
//! - WASI import registry: collects WASI imports from effect definitions in lib/wasi/*.wado
//! - Component Model ABI: type conversion and support checking for CM codegen

use std::collections::{BTreeMap, BTreeSet};

use crate::hashmap::{IndexMap, IndexSet};

use wasm_encoder::ValType;

use crate::ast::{CmImport, GenericType, Type};
use crate::tir::{TypeId, TypeTable};

/// A variant case with both CM and Wado names.
#[derive(Debug, Clone)]
pub struct CmVariantCase {
    /// Component Model name (e.g., `"DNS-error"`).
    pub cm_name: String,
    /// Wado source name (e.g., `"DnsError"`).
    pub wado_name: String,
    /// Payload type, if any.
    pub payload: Option<Type>,
}

/// Extract the CM name from a `#[cm("...")]` attribute.
///
/// Handles two formats:
/// - Type-level: `#[cm("wasi:pkg/iface@ver#cm-name")]` → takes the fragment after `#`
/// - Case-level: `#[cm("cm-name")]` → uses the whole arg directly
///
/// Strip a `AsyncCall<T>` wrapper from the declared return type of an `async`
/// interface method, recovering the CM-level result type `T`.
///
/// In WIT, `async func foo(...) -> T` lowers the CM-level result as `T`, but
/// the Wado-facing interface declaration exposes `AsyncCall<T>` so user code can
/// defer `wait()`-ing until after any stream parameter rendezvous
/// completes. The CM binding synthesiser needs the raw `T` when computing
/// outptr layout.
///
/// For non-async methods this is a no-op. For async methods whose return
/// type is missing or not `AsyncCall<_>` we return the original type —
/// defensive in case hand-written interface declarations predate the
/// `AsyncCall<T>` convention (in which case the compiler's earlier
/// expectations still apply).
pub fn unwrap_async_call_if_async(is_async: bool, declared: &Option<Type>) -> Option<Type> {
    if !is_async {
        return declared.clone();
    }
    match declared {
        Some(Type::Generic(generic)) if generic.name == "AsyncCall" && generic.args.len() == 1 => {
            let inner = &generic.args[0];
            if is_unit_type(inner) {
                return None;
            }
            Some(inner.clone())
        }
        other => other.clone(),
    }
}

/// Returns true if `ty` is the Wado unit type. Recognises both surface
/// syntaxes that the parser may emit: the empty tuple `[]` and the
/// named form `()`.
pub fn is_unit_type(ty: &Type) -> bool {
    match ty {
        Type::Tuple(elems) => elems.is_empty(),
        Type::Named(named) => matches!(named.name.as_str(), "()" | "Unit" | "unit"),
        _ => false,
    }
}

/// Preserves acronym casing from the WIT source (e.g., `DNS-timeout`, `TLS-protocol-error`).
/// Panics if no `#[cm]` attribute is present — all CM names must be metadata-driven.
fn cm_attr_cm_name(attrs: &[crate::ast::Attribute], wado_name: &str) -> String {
    attrs
        .iter()
        .find(|a| a.name == "cm")
        .and_then(|a| a.args.first())
        .map(|arg| {
            let s = arg.as_str();
            // Type-level attrs contain a `#` separator; case-level attrs do not
            if let Some(fragment) = s.split('#').nth(1) {
                fragment.to_owned()
            } else {
                s.to_owned()
            }
        })
        .unwrap_or_else(|| panic!("missing #[cm] attribute for CM name: {wado_name}"))
}

/// Extract CM parameter names from a `#[cm_params("param-a", "param-b")]` attribute.
fn extract_cm_params_attr(attrs: &[crate::ast::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .find(|a| a.name == "cm_params")
        .map(|a| a.args.iter().map(|arg| arg.as_str().to_string()).collect())
        .unwrap_or_default()
}

/// Information about a WASI function from an interface method
#[derive(Debug, Clone)]
pub struct WasiFunctionInfo {
    /// Effect name (e.g., "Stdout")
    pub interface_name: String,
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
    /// Parameter names and types: (`wado_name`, `cm_name`, type)
    pub params: Vec<(String, String, Type)>,
    /// Return type
    pub return_type: Option<Type>,
}

impl WasiFunctionInfo {
    /// Build the local alias name for Component Model imports.
    ///
    /// Format: `wasi:{package}/{interface_name}::{method_name}`
    /// Example: `wasi:cli/Stdout::write_via_stream`
    pub fn local_alias_name(&self) -> String {
        build_local_alias_name(&self.package, &self.interface_name, &self.method_name)
    }

    /// Whether `canon lower` requires the Memory canonical option.
    ///
    /// True when any parameter or return type involves linear memory
    /// (strings, lists, streams, options, results, tuples), or when async.
    pub fn needs_memory(&self) -> bool {
        if self.is_async {
            return true;
        }
        if self
            .params
            .iter()
            .any(|(_, _, ty)| Self::type_requires_memory(ty))
        {
            return true;
        }
        self.return_type
            .as_ref()
            .is_some_and(Self::return_type_requires_memory)
    }

    /// Whether `canon lower` requires the Realloc canonical option.
    ///
    /// Same conditions as `needs_memory` — they are always needed together.
    pub fn needs_realloc(&self) -> bool {
        self.needs_memory()
    }

    /// Like `needs_memory`, but also checks WASI variant return types.
    ///
    /// WASI variant types (e.g., `Method` with `Other(String)`) have string
    /// payloads that require memory for `canon lower`.
    pub fn needs_memory_with_registry(&self, registry: &WasiRegistry) -> bool {
        if self.needs_memory() {
            return true;
        }
        // Check if return type is a WASI variant whose payload contains a string
        if let Some(rt) = &self.return_type
            && Self::named_type_payload_requires_memory(rt, registry)
        {
            return true;
        }
        // Check if any param is a WASI variant whose payload contains a string
        self.params
            .iter()
            .any(|(_, _, ty)| Self::named_type_payload_requires_memory(ty, registry))
    }

    /// Check if a named type is a WASI variant/struct whose payload requires memory.
    /// Uses the reference's own `source_interface` when present (stdlib-
    /// populated), otherwise falls back to the unique `wasi:*` source.
    fn named_type_payload_requires_memory(ty: &Type, registry: &WasiRegistry) -> bool {
        if let Type::Named(named) = ty {
            let source = named
                .source_interface
                .as_deref()
                .map(str::to_string)
                .or_else(|| {
                    registry
                        .find_wasi_variant_source(&named.name)
                        .map(str::to_string)
                });
            if let Some(src) = &source {
                if let Some(cases) = registry.get_variant_cases_by_source(src, &named.name) {
                    return cases.iter().any(|case| {
                        case.payload
                            .as_ref()
                            .is_some_and(Self::type_requires_memory)
                    });
                }
                // WASI struct (record) types always need memory since they have
                // multiple fields and exceed MAX_FLAT_RESULTS (1) in canon lower.
                if registry
                    .get_struct_fields_by_source(src, &named.name)
                    .is_some()
                {
                    return true;
                }
            }
        }
        false
    }

    /// Check if a parameter type requires Memory + Realloc in canon lower.
    fn type_requires_memory(ty: &Type) -> bool {
        match ty {
            Type::Generic(g) => matches!(g.name.as_str(), "Stream" | "Array"),
            Type::Named(named) => named.name == "String",
            _ => false,
        }
    }

    /// Whether this async function has a `Stream<T>` or `Future<T>` parameter.
    ///
    /// Streaming async functions cannot be waited on inside the adapter because
    /// the caller must write to the stream before the subtask completes.
    /// The adapter returns the raw subtask handle (i32) for these functions.
    pub fn has_streaming_param(&self) -> bool {
        self.params.iter().any(|(_, _, ty)| {
            matches!(ty, Type::Generic(g) if matches!(g.name.as_str(), "Stream" | "Future"))
        })
    }

    /// Whether this function returns a Future<T> or a tuple containing Future<T>.
    pub fn return_type_has_future(&self) -> bool {
        fn has_future(ty: &Type) -> bool {
            match ty {
                Type::Generic(g) if g.name == "Future" => true,
                Type::Generic(g) => g.args.iter().any(has_future),
                Type::Tuple(elems) => elems.iter().any(has_future),
                _ => false,
            }
        }
        self.return_type.as_ref().is_some_and(has_future)
    }

    /// Check if a return type requires Memory + Realloc in canon lower.
    fn return_type_requires_memory(ty: &Type) -> bool {
        match ty {
            Type::Named(named) => named.name == "String",
            Type::Generic(generic) => matches!(
                generic.name.as_str(),
                "Array" | "Option" | "Result" | "Tuple" | "Stream" | "Future"
            ),
            Type::Tuple(elems) => !elems.is_empty(),
            _ => false,
        }
    }
}

/// Build a local alias name for a WASI function.
///
/// Format: `wasi:{package}/{interface_name}::{method_name}`
/// Example: `wasi:cli/Stdout::write_via_stream`
///
/// This naming scheme:
/// - Uses `wasi:` prefix for clarity
/// - Includes package for uniqueness across packages
/// - Uses Wado effect/method names (not WIT interface/function names)
/// - Uses `::` as method separator (Wado convention)
pub fn build_local_alias_name(package: &str, interface_name: &str, method_name: &str) -> String {
    format!("wasi:{package}/{interface_name}::{method_name}")
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
#[derive(Debug, Clone, Default)]
pub struct WasiRegistry {
    /// `Effect::method` -> function info
    effect_to_func: IndexMap<String, WasiFunctionInfo>,

    /// Interface path -> list of functions
    /// Using `BTreeMap` for deterministic ordering
    interfaces: BTreeMap<String, Vec<WasiFunctionInfo>>,

    /// Local alias -> (`interface_path`, `wasi_func_name`)
    /// Key format: `wasi:{package}/{interface_name}::{method_name`}
    /// e.g., "`wasi:cli/Stdout::write_via_stream`"
    local_aliases: IndexMap<String, (String, String)>,

    /// Track which WASI function names are used to detect collisions
    used_names: BTreeSet<String>,

    /// Newtypes collected from WASI modules (e.g., Instant -> u64).
    /// Key: `(source_interface, wado_name)` where `source_interface` is the
    /// `#[cm("...")]` fragment before the `#` (e.g.
    /// `"wasi:clocks/types@0.3.0-rc-2026-03-15"`). Keying by interface makes
    /// same-named types from distinct interfaces structurally distinct.
    newtypes: IndexMap<(String, String), Type>,

    /// Resource types collected from WASI modules (e.g., `TerminalInput`).
    /// Key: `(source_interface, wado_name)`. Value: CM kebab-case name.
    resources: IndexMap<(String, String), String>,

    /// Flags types collected from WASI modules (e.g., `PathFlags`, `OpenFlags`).
    /// Key: `(source_interface, wado_name)`. Value:
    /// `(cm_name, member names in kebab-case)`.
    flags: IndexMap<(String, String), (String, Vec<String>)>,

    /// Enum types collected from WASI modules (e.g., `ErrorCode`, `IpAddressFamily`).
    /// Key: `(source_interface, wado_name)`. Value:
    /// `(cm_name, variant names in kebab-case)`.
    enums: IndexMap<(String, String), (String, Vec<String>)>,

    /// Variant types collected from WASI modules (e.g., `HeaderError`).
    /// Key: `(source_interface, wado_name)`. Value: `(cm_name, cases)`.
    variants: IndexMap<(String, String), (String, Vec<CmVariantCase>)>,

    /// Struct types collected from WASI modules (e.g., `DnsErrorPayload`).
    /// Key: `(source_interface, wado_name)`. Value:
    /// `(cm_name, fields with CM names, fields with Wado names)`.
    /// Fields: Vec<(`cm_field_name`, `field_type`)>
    /// Wado fields: Vec<(`wado_field_name`, `cm_field_name`, `field_type`)>
    structs: IndexMap<(String, String), (String, Vec<(String, Type)>, Vec<(String, String, Type)>)>,
}

/// Insert into a `(source_interface, name)`-keyed map, panicking on duplicate
/// registration. Two registrations of the same `(interface, name)` pair
/// indicate a stdlib bug (the same declaration emitted twice) — we want a
/// loud failure instead of a silent overwrite.
fn register_unique<V>(
    map: &mut IndexMap<(String, String), V>,
    kind: &str,
    source_interface: String,
    name: String,
    value: V,
) {
    let key = (source_interface, name);
    assert!(
        !map.contains_key(&key),
        "WasiRegistry: duplicate {kind} registration for `{}` in interface `{}`. \
         Each (interface, name) pair must be registered exactly once.",
        key.1,
        key.0,
    );
    map.insert(key, value);
}

/// Return the value for `name` in a `(source_interface, name)`-keyed map
/// when exactly one interface defines it. Returns `None` for zero or multiple
/// matches — ambiguous same-named types must be disambiguated via an
/// interface- or package-scoped accessor.
fn find_unique_in<'a, V>(map: &'a IndexMap<(String, String), V>, name: &str) -> Option<&'a V> {
    let mut found: Option<&V> = None;
    for ((_, n), v) in map {
        if n != name {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(v);
    }
    found
}

/// Return true if any interface registers `name` in a
/// `(source_interface, name)`-keyed map.
fn has_any_in<V>(map: &IndexMap<(String, String), V>, name: &str) -> bool {
    map.keys().any(|(_, n)| n == name)
}

/// Return the value for `name` in a `(source_interface, name)`-keyed map when
/// exactly one interface whose source starts with `prefix` defines it. Returns
/// `None` for zero or multiple matches under the prefix. No fallback to the
/// bare-name scan: a caller that passes a prefix is explicitly requesting
/// prefix-scoped resolution and must not be silently redirected to a type
/// defined in a different namespace.
fn find_unique_with_prefix<'a, V>(
    map: &'a IndexMap<(String, String), V>,
    prefix: &str,
    name: &str,
) -> Option<&'a V> {
    let mut found: Option<&V> = None;
    for ((src, n), v) in map {
        if n != name || !src.starts_with(prefix) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(v);
    }
    found
}

/// Return the `source_interface` when `name` has exactly one registrant under
/// `prefix`.
fn find_unique_source_with_prefix<'a, V>(
    map: &'a IndexMap<(String, String), V>,
    prefix: &str,
    name: &str,
) -> Option<&'a str> {
    let mut found: Option<&str> = None;
    for ((src, n), _) in map {
        if n != name || !src.starts_with(prefix) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(src.as_str());
    }
    found
}

/// Return the `source_interface` when `name` has exactly one registrant,
/// ignoring empty source strings.
fn find_unique_source_in<'a, V>(
    map: &'a IndexMap<(String, String), V>,
    name: &str,
) -> Option<&'a str> {
    let mut found: Option<&str> = None;
    for ((src, n), _) in map {
        if n != name {
            continue;
        }
        if found.is_some() {
            return None;
        }
        if !src.is_empty() {
            found = Some(src.as_str());
        }
    }
    found
}

/// Walk a parsed stdlib module and return `name -> source_interface` for
/// every top-level declaration that carries a `#[cm("...")]` attribute.
///
/// Resolve a `Type::Named` reference through the newtype-alias map, scanning
/// by name. When the name is ambiguous (multiple interfaces define a type
/// with the same name), the original type is returned unchanged so the
/// caller can fall back to source-aware resolution. Other type shapes are
/// returned as-is.
fn resolve_type(ty: &Type, aliases: &IndexMap<(String, String), Type>) -> Type {
    match ty {
        Type::Named(named) => {
            if let Some(resolved) = find_unique_in(aliases, &named.name) {
                resolved.clone()
            } else {
                ty.clone()
            }
        }
        _ => ty.clone(),
    }
}

/// Captured `pub interface Foo { ... }` declarations across all parsed
/// modules. Used by `WorldRegistry::register` (via lookup callbacks) to
/// expand `export Foo;` into per-method exports and to resolve `import Foo;`
/// to its CM FQ.
#[derive(Default)]
struct InterfaceDeclTable {
    by_name: IndexMap<String, InterfaceDeclEntry>,
}

struct InterfaceDeclEntry {
    cm_interface_fq: String,
    methods: Vec<InterfaceDeclMethod>,
}

struct InterfaceDeclMethod {
    name: String,
    is_async: bool,
    params: Vec<(String, Type)>,
    return_type: Option<Type>,
}

impl InterfaceDeclTable {
    fn import_cm_fq(&self, name: &str) -> Option<String> {
        self.by_name.get(name).map(|e| e.cm_interface_fq.clone())
    }

    fn export_lookup(
        &self,
        name: &str,
        newtypes: &IndexMap<(String, String), Type>,
    ) -> Option<crate::world_registry::InterfaceExportLookup> {
        let entry = self.by_name.get(name)?;
        let methods = entry
            .methods
            .iter()
            .map(|m| crate::world_registry::InterfaceExportMethod {
                name: m.name.clone(),
                is_async: m.is_async,
                params: m
                    .params
                    .iter()
                    .map(|(n, ty)| (n.clone(), resolve_type(ty, newtypes)))
                    .collect(),
                return_type: m
                    .return_type
                    .as_ref()
                    .map(|ty| unwrap_async_call_if_async(m.is_async, &Some(ty.clone())))
                    .unwrap_or(None)
                    .or_else(|| m.return_type.clone()),
            })
            .collect();
        Some(crate::world_registry::InterfaceExportLookup {
            cm_interface_fq: entry.cm_interface_fq.clone(),
            methods,
        })
    }
}

fn collect_interface_decls(modules: &[(&'static str, crate::ast::Module)]) -> InterfaceDeclTable {
    use crate::ast::Item;

    let mut table = InterfaceDeclTable::default();
    for (_, module) in modules {
        for item in &module.items {
            let Item::Interface(iface) = item else {
                continue;
            };
            // The interface's CM FQ is the full `#[cm("...")]` argument on the
            // interface declaration itself (e.g.
            // `"wasi:http/handler@0.3.0-rc-2026-03-15"`). Skip anonymous
            // interfaces without a CM attribute — they are not boundary-
            // visible and cannot back a world export.
            let Some(cm_fq) = iface
                .attrs
                .iter()
                .find(|a| a.name == "cm")
                .and_then(|a| a.args.first())
                .map(|arg| arg.as_str().to_string())
            else {
                continue;
            };

            let methods: Vec<InterfaceDeclMethod> = iface
                .methods
                .iter()
                .map(|m| InterfaceDeclMethod {
                    name: m.name.clone(),
                    is_async: m.is_async,
                    params: m
                        .params
                        .iter()
                        .map(|p| (p.name.clone(), p.ty.clone()))
                        .collect(),
                    return_type: m.return_type.clone(),
                })
                .collect();

            // Same-name interfaces declared in multiple modules cannot be
            // distinguished at world-registration time (`export Foo;` /
            // `import Foo;` both look up by local name). Mirror
            // `WorldRegistry::register`'s policy: keep the first registrant,
            // log a warning, and ignore the rest. A duplicate here is a
            // stdlib bug or a user-input collision; silent overwriting would
            // make the earlier interface invisible to any world that
            // references it.
            if table.by_name.contains_key(&iface.name) {
                eprintln!(
                    "InterfaceDeclTable: duplicate interface `{}` (also at {cm_fq:?}). \
                     Keeping the first registrant.",
                    iface.name,
                );
                continue;
            }

            table.by_name.insert(
                iface.name.clone(),
                InterfaceDeclEntry {
                    cm_interface_fq: cm_fq,
                    methods,
                },
            );
        }
    }
    table
}

/// The returned map is keyed by the Wado-side identifier (e.g. `Response`)
/// and points at the interface prefix before the `#` fragment (e.g.
/// `"wasi:http/types@0.3.0-rc-2026-03-15"`). Effects, structs, variants,
/// enums, flags, resources, and newtypes are all included.
fn collect_cm_definitions(module: &crate::ast::Module) -> IndexMap<String, String> {
    use crate::ast::Item;
    let mut out: IndexMap<String, String> = IndexMap::default();
    for item in &module.items {
        let (name, attrs) = match item {
            Item::Newtype(a) => (a.name.clone(), a.attrs.as_slice()),
            Item::Resource(r) => (r.name.clone(), r.attrs.as_slice()),
            Item::Struct(s) => (s.name.clone(), s.attrs.as_slice()),
            Item::Flags(f) => (f.name.clone(), f.attributes.as_deref().unwrap_or(&[])),
            Item::Enum(e) => (e.name.clone(), e.attrs.as_slice()),
            Item::Variant(v) => (v.name.clone(), v.attrs.as_slice()),
            Item::Interface(e) => (e.name.clone(), e.attrs.as_slice()),
            _ => continue,
        };
        let source = WasiRegistry::cm_source_interface(attrs);
        if source.is_empty() {
            continue;
        }
        out.insert(name, source);
    }
    out
}

/// Build the `name -> source_interface` map that applies inside `module_path`.
///
/// Entries come from three sources, in order of precedence (later wins if
/// there were duplicates, but stdlib currently never introduces them):
///   1. Types declared in this module (`collect_cm_definitions`).
///   2. Named imports: `use { X } from "other/path.wado"` contributes `X` →
///      whatever interface `other/path.wado` declares it in.
///   3. `use { X as Y } from ...` contributes `Y` with the same target.
///
/// Names the lookup cannot resolve (primitives, generics, `String`, unknown
/// references) are left out; downstream consumers treat a missing key as
/// `source_interface = None`.
fn build_local_name_resolver(
    module_path: &str,
    module: &crate::ast::Module,
    defs_by_module: &IndexMap<&'static str, IndexMap<String, String>>,
) -> IndexMap<String, String> {
    use crate::ast::{Item, UseItem};
    let mut local: IndexMap<String, String> = IndexMap::default();
    if let Some(defs) = defs_by_module.get(module_path) {
        for (name, source) in defs {
            local.insert(name.clone(), source.clone());
        }
    }
    for item in &module.items {
        if let Item::Use(use_decl) = item {
            let Some(other_defs) = resolve_use_source(&use_decl.source, defs_by_module) else {
                continue;
            };
            for use_item in &use_decl.items {
                match use_item {
                    UseItem::Simple { name, alias, .. } => {
                        let Some(source) = other_defs.get(name) else {
                            continue;
                        };
                        let bind = alias.clone().unwrap_or_else(|| name.clone());
                        local.insert(bind, source.clone());
                    }
                    UseItem::InterfaceFunctions {
                        interface_name,
                        functions: _,
                    } => {
                        if let Some(source) = other_defs.get(interface_name) {
                            local.insert(interface_name.clone(), source.clone());
                        }
                    }
                    UseItem::Namespace { .. } | UseItem::Wildcard => {}
                }
            }
        }
    }
    local
}

/// Resolve a stdlib `use ... from "<source>"` source string to the
/// `collect_cm_definitions` entry for the targeted module.
///
/// Accepts both the flat package form (`"wasi:http"`) and the per-interface
/// form (`"wasi:http/types.wado"`). Flat imports search every interface under
/// the package for a matching name at use site (see
/// `build_local_name_resolver`).
fn resolve_use_source<'a>(
    source: &str,
    defs_by_module: &'a IndexMap<&'static str, IndexMap<String, String>>,
) -> Option<&'a IndexMap<String, String>> {
    defs_by_module.get(source).or_else(|| {
        // Flat package form: fold every interface under the same package into
        // a synthetic merged view. We build this lazily only when needed.
        // In stdlib, flat package .wado files are pure re-exports whose
        // definitions each still carry their own `#[cm]` — but `use "wasi:http"`
        // may be used to grab any of them. Rather than caching the merged map,
        // we return None and let the caller handle flat imports via per-item
        // resolution below. Returning None here means bindings like
        // `use { ErrorCode } from "wasi:http"` in stdlib are not resolved.
        // Stdlib does not currently rely on flat-package imports for type
        // references (verified by grep), so this is acceptable.
        let _ = source;
        None
    })
}

/// Walk every `Type` node reachable from `module` and set
/// `NamedType.source_interface` from `local_names` whenever the name is known.
///
/// References whose name is not in `local_names` are intentionally left as
/// `None` — those are primitives (`String`, `bool`, `i32`, ...), generic type
/// parameters, or names the stdlib never declares.
fn populate_named_type_sources(
    module: &mut crate::ast::Module,
    local_names: &IndexMap<String, String>,
) {
    use crate::ast::Item;
    for item in &mut module.items {
        match item {
            Item::Function(f) => {
                for param in &mut f.params {
                    walk_type(&mut param.ty, local_names);
                }
                if let Some(ret) = &mut f.return_type {
                    walk_type(ret, local_names);
                }
            }
            Item::Struct(s) => {
                for field in &mut s.fields {
                    walk_type(&mut field.ty, local_names);
                }
            }
            Item::Variant(v) => {
                for case in &mut v.cases {
                    if let Some(payload) = &mut case.payload {
                        walk_type(payload, local_names);
                    }
                }
            }
            Item::Newtype(a) => walk_type(&mut a.ty, local_names),
            Item::Interface(effect) => {
                for method in &mut effect.methods {
                    for param in &mut method.params {
                        walk_type(&mut param.ty, local_names);
                    }
                    if let Some(ret) = &mut method.return_type {
                        walk_type(ret, local_names);
                    }
                }
            }
            Item::Resource(r) => {
                for method in &mut r.methods {
                    for param in &mut method.params {
                        walk_type(&mut param.ty, local_names);
                    }
                    if let Some(ret) = &mut method.return_type {
                        walk_type(ret, local_names);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Recursively descend into a Wado `Type` and populate
/// `NamedType.source_interface` on every named leaf whose identifier appears
/// in `local_names`.
fn walk_type(ty: &mut crate::ast::Type, local_names: &IndexMap<String, String>) {
    use crate::ast::Type;
    match ty {
        Type::Named(n) => {
            if n.source_interface.is_none()
                && let Some(source) = local_names.get(&n.name)
            {
                n.source_interface = Some(source.clone());
            }
        }
        Type::Generic(g) => {
            for arg in &mut g.args {
                walk_type(arg, local_names);
            }
        }
        Type::NamespacedGeneric(g) => {
            for arg in &mut g.args {
                walk_type(arg, local_names);
            }
        }
        Type::Function(f) => {
            for p in &mut f.params {
                walk_type(p, local_names);
            }
            walk_type(&mut f.return_type, local_names);
        }
        Type::Tuple(elems) => {
            for e in elems {
                walk_type(e, local_names);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => walk_type(inner, local_names),
        _ => {}
    }
}

impl WasiRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the canonical source interface that owns `(kind, name)` — i.e.
    /// the `#[cm("...")]` fragment before the `#` (e.g.
    /// `"wasi:filesystem/types@0.3.0-rc-2026-03-15"`). `kind` is one of
    /// `"variants"`, `"enums"`, `"resources"`, `"structs"`, `"flags"`, or
    /// `"newtypes"`.
    ///
    /// Returns `Some` only when exactly one WASI interface declares the bare
    /// name. Same-named types from multiple interfaces (e.g. `ErrorCode` in
    /// `wasi:filesystem/types` vs `wasi:http/types`) return `None` — callers
    /// must then disambiguate with a source-interface-scoped accessor
    /// (e.g. `get_variant_cases_by_source`).
    pub fn bare_name_owner(&self, kind: &str, name: &str) -> Option<&str> {
        let candidates: Vec<&str> = match kind {
            "variants" => self
                .variants
                .keys()
                .filter_map(|(src, n)| (n == name && !src.is_empty()).then_some(src.as_str()))
                .collect(),
            "enums" => self
                .enums
                .keys()
                .filter_map(|(src, n)| (n == name && !src.is_empty()).then_some(src.as_str()))
                .collect(),
            "resources" => self
                .resources
                .keys()
                .filter_map(|(src, n)| (n == name && !src.is_empty()).then_some(src.as_str()))
                .collect(),
            "structs" => self
                .structs
                .keys()
                .filter_map(|(src, n)| (n == name && !src.is_empty()).then_some(src.as_str()))
                .collect(),
            "flags" => self
                .flags
                .keys()
                .filter_map(|(src, n)| (n == name && !src.is_empty()).then_some(src.as_str()))
                .collect(),
            "newtypes" => self
                .newtypes
                .keys()
                .filter_map(|(src, n)| (n == name && !src.is_empty()).then_some(src.as_str()))
                .collect(),
            _ => return None,
        };
        if candidates.len() == 1 {
            Some(candidates[0])
        } else {
            None
        }
    }

    /// Extract the source interface (the part before the `#` fragment,
    /// e.g. `"wasi:cli/stdout@0.3.0-rc-2026-03-15"`) from the
    /// first `#[cm("...")]` attribute on a stdlib item. Returns an
    /// empty string if no attribute is present (user-authored items
    /// don't carry this).
    fn cm_source_interface(attrs: &[crate::ast::Attribute]) -> String {
        attrs
            .iter()
            .find(|a| a.name == "cm")
            .and_then(|a| a.args.first())
            .and_then(|arg| arg.as_str().split('#').next())
            .unwrap_or("")
            .to_string()
    }

    /// Build the registry from the embedded stdlib
    ///
    /// Parses the embedded wasi:* modules and registers their interface methods.
    /// Also collects newtypes and world definitions.
    pub fn build_from_stdlib() -> &'static (Self, crate::world_registry::WorldRegistry) {
        use std::sync::OnceLock;

        static INSTANCE: OnceLock<(WasiRegistry, crate::world_registry::WorldRegistry)> =
            OnceLock::new();

        INSTANCE.get_or_init(Self::build_from_stdlib_inner)
    }

    fn build_from_stdlib_inner() -> (Self, crate::world_registry::WorldRegistry) {
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

        // Two-pass bootstrap so every stdlib `Type::Named` reference carries
        // an exact source interface.
        //
        // Pass 1: parse every stdlib module and record the declared type
        // names and their `#[cm(...)]` interface paths per module. This lets
        // cross-module `use { X } from "path"` be resolved in pass 2 even
        // when `path` is parsed after the importer.
        //
        // Pass 2: for each module, build a local name -> source_interface map
        // (local definitions + resolved `use` imports) and walk every `Type`
        // node to populate `NamedType.source_interface`. Register the
        // walked module afterwards.
        let mut modules: Vec<(&'static str, crate::ast::Module)> = Vec::new();
        let mut defs_by_module: IndexMap<&'static str, IndexMap<String, String>> =
            IndexMap::default();
        for (path, source) in stdlib::ALL_WASI_MODULES {
            let module = parse_module(source);
            defs_by_module.insert(*path, collect_cm_definitions(&module));
            modules.push((*path, module));
        }
        for (path, source) in [
            ("core:kiln/kiln_host.wado", stdlib::CORE_KILN_KILN_HOST),
            ("core:kiln/types.wado", stdlib::CORE_KILN_TYPES),
            ("core:kiln/worlds.wado", stdlib::CORE_KILN_WORLDS),
        ] {
            let module = parse_module(source);
            defs_by_module.insert(path, collect_cm_definitions(&module));
            modules.push((path, module));
        }

        let mut registry = Self::new();
        let mut world_registry = WorldRegistry::new();

        // Pass 1: resolve named-type sources and register types/interfaces/
        // resources for every module. World declarations are deferred to
        // pass 2 so that `export Foo;` in a world can be expanded against the
        // already-registered interface signatures, even when the interface
        // is declared in a different module than the world.
        let mut resolved_modules: Vec<(&'static str, crate::ast::Module)> =
            Vec::with_capacity(modules.len());
        for (path, mut module) in modules {
            let local_names = build_local_name_resolver(path, &module, &defs_by_module);
            populate_named_type_sources(&mut module, &local_names);
            registry.register_module_decls(&module);
            resolved_modules.push((path, module));
        }

        // Build interface lookup from accumulated `pub interface Foo`
        // declarations across every module. The world registrar uses this to
        // expand `export Foo;` interface refs into per-method exports.
        let interface_decls = collect_interface_decls(&resolved_modules);

        // Pass 2: register worlds.
        for (_, module) in &resolved_modules {
            registry.register_module_worlds(module, &mut world_registry, &interface_decls);
        }

        (registry, world_registry)
    }

    /// Register types, interfaces, and resources from a WASI module. World
    /// declarations are handled separately by [`Self::register_module_worlds`]
    /// so that interface exports can be expanded against the full set of
    /// interface declarations across modules.
    fn register_module_decls(&mut self, module: &crate::ast::Module) {
        use crate::ast::Item;

        // First, collect newtypes from this module
        for item in &module.items {
            if let Item::Newtype(alias) = item {
                let source_interface = Self::cm_source_interface(&alias.attrs);
                register_unique(
                    &mut self.newtypes,
                    "newtype",
                    source_interface,
                    alias.name.clone(),
                    alias.ty.clone(),
                );
            }
        }

        // Collect resource types from this module
        for item in &module.items {
            if let Item::Resource(resource) = item {
                // Use the #[cm] fragment as the CM name (preserves acronym casing like DNS, TLS)
                let cm_name = cm_attr_cm_name(&resource.attrs, &resource.name);
                // Extract source interface path from #[cm] attribute
                // Format: #[cm("wasi:cli/terminal-input@0.3.0-rc-2026-01-06#terminal-input")]
                let source_interface = Self::cm_source_interface(&resource.attrs);
                register_unique(
                    &mut self.resources,
                    "resource",
                    source_interface,
                    resource.name.clone(),
                    cm_name,
                );
            }
        }

        // Collect struct types from this module (e.g., DnsErrorPayload -> DNS-error-payload)
        for item in &module.items {
            if let Item::Struct(struct_def) = item {
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(&struct_def.attrs, &struct_def.name);
                let source_interface = Self::cm_source_interface(&struct_def.attrs);
                let fields: Vec<(String, Type)> = struct_def
                    .fields
                    .iter()
                    .map(|f| (cm_attr_cm_name(&f.attrs, &f.name), f.ty.clone()))
                    .collect();
                let wado_fields: Vec<(String, String, Type)> = struct_def
                    .fields
                    .iter()
                    .map(|f| {
                        (
                            f.name.clone(),
                            cm_attr_cm_name(&f.attrs, &f.name),
                            f.ty.clone(),
                        )
                    })
                    .collect();
                register_unique(
                    &mut self.structs,
                    "struct",
                    source_interface,
                    struct_def.name.clone(),
                    (cm_name, fields, wado_fields),
                );
            }
        }

        // Collect flags types from this module
        for item in &module.items {
            if let Item::Flags(flags_def) = item {
                let attrs = flags_def.attributes.as_deref().unwrap_or(&[]);
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(attrs, &flags_def.name);
                let source_interface = Self::cm_source_interface(attrs);
                // Use per-member #[cm] attr for CM name
                let member_names: Vec<String> = flags_def
                    .flags
                    .iter()
                    .map(|m| cm_attr_cm_name(&m.attrs, &m.name))
                    .collect();
                register_unique(
                    &mut self.flags,
                    "flags",
                    source_interface,
                    flags_def.name.clone(),
                    (cm_name, member_names),
                );
            }
        }

        // Collect enum types from this module
        for item in &module.items {
            if let Item::Enum(enum_def) = item {
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(&enum_def.attrs, &enum_def.name);
                // Use per-case #[cm] attr for CM name
                let variant_names: Vec<String> = enum_def
                    .cases
                    .iter()
                    .map(|c| cm_attr_cm_name(&c.attrs, &c.name))
                    .collect();

                // Extract interface path from #[cm] attribute if present
                // Format: #[cm("wasi:sockets/types@0.3.0-rc-2025-09-16#error-code")]
                let source_interface = Self::cm_source_interface(&enum_def.attrs);
                register_unique(
                    &mut self.enums,
                    "enum",
                    source_interface,
                    enum_def.name.clone(),
                    (cm_name, variant_names),
                );
            }
        }

        // Collect variant types from this module (e.g., HeaderError)
        for item in &module.items {
            if let Item::Variant(variant_def) = item {
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(&variant_def.attrs, &variant_def.name);
                let source_interface = Self::cm_source_interface(&variant_def.attrs);
                // Store both CM and Wado names for each case
                let cases: Vec<CmVariantCase> = variant_def
                    .cases
                    .iter()
                    .map(|c| CmVariantCase {
                        cm_name: cm_attr_cm_name(&c.attrs, &c.name),
                        wado_name: c.name.clone(),
                        payload: c.payload.clone(),
                    })
                    .collect();

                register_unique(
                    &mut self.variants,
                    "variant",
                    source_interface,
                    variant_def.name.clone(),
                    (cm_name, cases),
                );
            }
        }

        // Register interface methods with resolved types for params but NOT for return type
        // Return type must keep original names (e.g., Mark not u64) for newtype semantics
        for item in &module.items {
            if let Item::Interface(effect) = item {
                for method in &effect.methods {
                    if let Some(wasi) = method.attrs.first().and_then(|a| a.cm_import.as_ref()) {
                        // Extract CM param names from #[cm_params] attribute
                        let cm_param_names = extract_cm_params_attr(&method.attrs);
                        let params: Vec<(String, String, Type)> = method
                            .params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                let cm_name = cm_param_names
                                    .get(i)
                                    .unwrap_or_else(|| {
                                        panic!(
                                            "missing #[cm_params] for param '{}' in {}.{}",
                                            p.name, effect.name, method.name
                                        )
                                    })
                                    .clone();
                                (p.name.clone(), cm_name, resolve_type(&p.ty, &self.newtypes))
                            })
                            .collect();

                        // Keep original return type for newtype semantics
                        // The resolver will handle Mark -> newtype mapping.
                        //
                        // For `async fn foo(...) -> AsyncCall<T>` CM imports,
                        // strip the `AsyncCall<T>` wrapper at registration so
                        // the stored return type is the CM-ABI `T`. The
                        // resolver re-wraps it as `AsyncCall<T>` when
                        // answering Wado-level type queries (so user code
                        // sees the new API), and the CM binding synthesiser
                        // also re-wraps when constructing its adapter.
                        let return_type =
                            unwrap_async_call_if_async(method.is_async, &method.return_type);

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

        // Register resource methods with resolved types for params but NOT for return type
        for item in &module.items {
            if let Item::Resource(resource) = item {
                for method in &resource.methods {
                    if let Some(wasi) = method.attrs.first().and_then(|a| a.cm_import.as_ref()) {
                        // Extract CM param names from #[cm_params] attribute
                        let cm_param_names = extract_cm_params_attr(&method.attrs);
                        let params: Vec<(String, String, Type)> = method
                            .params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                let cm_name = cm_param_names
                                    .get(i)
                                    .unwrap_or_else(|| {
                                        panic!(
                                            "missing #[cm_params] for param '{}' in {}.{}",
                                            p.name, resource.name, method.name
                                        )
                                    })
                                    .clone();
                                (p.name.clone(), cm_name, resolve_type(&p.ty, &self.newtypes))
                            })
                            .collect();

                        // Keep original return type for newtype semantics.
                        // Resource methods declared as `async fn -> AsyncCall<T>`
                        // are unwrapped at registration so downstream code
                        // works with the CM-ABI `T`. See the effect branch.
                        let return_type =
                            unwrap_async_call_if_async(method.is_async, &method.return_type);

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
    }

    /// Register world declarations from a WASI module. `interface_decls`
    /// must already contain every `pub interface Foo` known to the
    /// compilation, so that interface exports (`export Foo;`) and interface
    /// imports (`import Foo;`) can be resolved to their CM FQ and method
    /// signatures.
    fn register_module_worlds(
        &self,
        module: &crate::ast::Module,
        world_registry: &mut crate::world_registry::WorldRegistry,
        interface_decls: &InterfaceDeclTable,
    ) {
        use crate::ast::Item;

        for item in &module.items {
            if let Item::World(world) = item {
                world_registry.register(
                    world,
                    |name| interface_decls.export_lookup(name, &self.newtypes),
                    |name| interface_decls.import_cm_fq(name),
                );
            }
        }
    }

    // -- Strict, source-aware lookups --------------------------------------
    //
    // Every stdlib `Type::Named` reference carries an exact
    // `source_interface` populated during bootstrap (see
    // `populate_named_type_sources`), so these accessors key directly on
    // `(interface, name)`. They never fall back to a bare-name scan — the
    // caller must supply the resolved source — which means a same-named
    // type in another namespace (e.g. `core:kiln/types::Response` vs.
    // `wasi:http/types::Response`) cannot shadow a lookup.
    //
    // Each accessor returns `Option` so the caller can distinguish "no
    // registration for that (interface, name) pair" from a specific wrong
    // interface. When the caller *knows* the registration must exist, use
    // `.expect("<context>")` to panic loudly if the invariant is violated.

    /// Newtype registered at `(interface, name)`, if any.
    pub fn get_newtype_by_source(&self, interface: &str, name: &str) -> Option<&Type> {
        self.newtypes
            .get(&(interface.to_string(), name.to_string()))
    }

    /// Resource registered at `(interface, name)`; returns the CM kebab name.
    pub fn get_resource_cm_name_by_source(&self, interface: &str, name: &str) -> Option<&str> {
        self.resources
            .get(&(interface.to_string(), name.to_string()))
            .map(String::as_str)
    }

    /// Struct registered at `(interface, name)`; returns the CM kebab name.
    pub fn get_struct_cm_name_by_source(&self, interface: &str, name: &str) -> Option<&str> {
        self.structs
            .get(&(interface.to_string(), name.to_string()))
            .map(|(cm_name, _, _)| cm_name.as_str())
    }

    /// Struct registered at `(interface, name)`; returns CM-kebab field
    /// `(name, type)` pairs.
    pub fn get_struct_fields_by_source(
        &self,
        interface: &str,
        name: &str,
    ) -> Option<&[(String, Type)]> {
        self.structs
            .get(&(interface.to_string(), name.to_string()))
            .map(|(_, fields, _)| fields.as_slice())
    }

    /// Struct registered at `(interface, name)`; returns field
    /// `(wado_name, cm_name, type)` triples.
    pub fn get_struct_fields_with_wado_names_by_source(
        &self,
        interface: &str,
        name: &str,
    ) -> Option<&[(String, String, Type)]> {
        self.structs
            .get(&(interface.to_string(), name.to_string()))
            .map(|(_, _, wado_fields)| wado_fields.as_slice())
    }

    /// Variant registered at `(interface, name)`; returns the CM kebab name.
    pub fn get_variant_cm_name_by_source(&self, interface: &str, name: &str) -> Option<&str> {
        self.variants
            .get(&(interface.to_string(), name.to_string()))
            .map(|(cm_name, _)| cm_name.as_str())
    }

    /// Variant registered at `(interface, name)`; returns case metadata.
    pub fn get_variant_cases_by_source(
        &self,
        interface: &str,
        name: &str,
    ) -> Option<&[CmVariantCase]> {
        self.variants
            .get(&(interface.to_string(), name.to_string()))
            .map(|(_, cases)| cases.as_slice())
    }

    /// Enum registered at `(interface, name)`; returns the CM kebab name.
    pub fn get_enum_cm_name_by_source(&self, interface: &str, name: &str) -> Option<&str> {
        self.enums
            .get(&(interface.to_string(), name.to_string()))
            .map(|(cm_name, _)| cm_name.as_str())
    }

    /// Enum registered at `(interface, name)`; returns CM-kebab variant names.
    pub fn get_enum_variants_by_source(&self, interface: &str, name: &str) -> Option<&[String]> {
        self.enums
            .get(&(interface.to_string(), name.to_string()))
            .map(|(_, variants)| variants.as_slice())
    }

    /// Flags registered at `(interface, name)`; returns the CM kebab name.
    pub fn get_flags_cm_name_by_source(&self, interface: &str, name: &str) -> Option<&str> {
        self.flags
            .get(&(interface.to_string(), name.to_string()))
            .map(|(cm_name, _)| cm_name.as_str())
    }

    /// Flags registered at `(interface, name)`; returns the member list.
    pub fn get_flags_members_by_source(&self, interface: &str, name: &str) -> Option<&[String]> {
        self.flags
            .get(&(interface.to_string(), name.to_string()))
            .map(|(_, members)| members.as_slice())
    }

    // -- "Find in wasi namespace" helpers for callers without AST context.
    //
    // A handful of synthesis paths operate on a Wado-side type *name* that
    // arrived via resolved symbol info (not an AST `Type::Named` node), so
    // they cannot consult `NamedType.source_interface`. These helpers walk
    // every `wasi:*` interface and return the unique match; an ambiguous
    // name (the same type declared by two different wasi packages) returns
    // `None`, which the caller must propagate. Kiln and other non-wasi
    // namespaces are intentionally excluded from this search.

    /// Find the source interface of a struct declared in `wasi:*` by its Wado
    /// name, when exactly one wasi interface registers the name.
    pub fn find_wasi_struct_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.structs, "wasi:", name)
    }

    /// Find the source interface of a struct declared in `core:kiln/*` by its
    /// Wado name, when exactly one kiln interface registers the name. Used by
    /// the CM lift/lower paths so records from `core:kiln/types` (e.g.
    /// `RawRequest`) can be resolved without leaking into the bare-name WASI
    /// lookups — those stay strictly scoped to `wasi:*` to preserve the
    /// `wasi:http/types::Response` vs. `core:kiln/types::Response`
    /// disambiguation.
    pub fn find_kiln_struct_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.structs, "core:kiln/", name)
    }

    /// Find the source interface of a variant declared in `core:kiln/*` by its
    /// Wado name, when exactly one kiln interface registers the name. Sibling
    /// to [`Self::find_kiln_struct_source`].
    pub fn find_kiln_variant_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.variants, "core:kiln/", name)
    }

    /// Find the source interface of an enum declared in `core:kiln/*`.
    pub fn find_kiln_enum_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.enums, "core:kiln/", name)
    }

    /// Find the source interface of a newtype declared in `wasi:*` by its
    /// Wado name, when exactly one wasi interface registers the name.
    pub fn find_wasi_newtype_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.newtypes, "wasi:", name)
    }

    /// Find the source interface of a resource declared in `wasi:*` by its
    /// Wado name, when exactly one wasi interface registers the name.
    pub fn find_wasi_resource_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.resources, "wasi:", name)
    }

    /// Find the source interface of a variant declared in `wasi:*` by its
    /// Wado name, when exactly one wasi interface registers the name.
    pub fn find_wasi_variant_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.variants, "wasi:", name)
    }

    /// Find the source interface of an enum declared in `wasi:*` by its
    /// Wado name, when exactly one wasi interface registers the name.
    pub fn find_wasi_enum_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.enums, "wasi:", name)
    }

    /// Find the source interface of a flags type declared in `wasi:*` by its
    /// Wado name, when exactly one wasi interface registers the name.
    pub fn find_wasi_flags_source(&self, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.flags, "wasi:", name)
    }

    /// Given a source-interface prefix (e.g. `"wasi:filesystem/types@"`) and
    /// a Wado type name, return the full registered source interface that
    /// matches in any type kind. Used by TIR → AST conversion to recover the
    /// exact versioned `source_interface` from a coarser `ModuleSource`.
    pub fn find_wasi_source_under_prefix(&self, prefix: &str, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(&self.structs, prefix, name)
            .or_else(|| find_unique_source_with_prefix(&self.variants, prefix, name))
            .or_else(|| find_unique_source_with_prefix(&self.enums, prefix, name))
            .or_else(|| find_unique_source_with_prefix(&self.flags, prefix, name))
            .or_else(|| find_unique_source_with_prefix(&self.resources, prefix, name))
            .or_else(|| find_unique_source_with_prefix(&self.newtypes, prefix, name))
    }

    /// Resolve the `wasi:*` source interface for a `NamedType` reference,
    /// optionally biased by the WASI package currently being synthesized.
    ///
    /// Resolution order:
    ///   1. The reference's own populated `source_interface` (stdlib bootstrap
    ///      always fills this). Returns `None` if it points outside `wasi:*`.
    ///   2. When the reference is unresolved (synthesized at lower time from
    ///      a TIR `ResolvedType`), search every wasi type kind. If a
    ///      `wasi_package_hint` is supplied, prefer an interface under
    ///      `wasi:{hint}/` — this disambiguates cross-package same-named
    ///      types such as `ErrorCode` defined independently in
    ///      `wasi:filesystem`, `wasi:http`, and `wasi:sockets`.
    ///   3. Otherwise fall through to the unique wasi-namespace registrant,
    ///      returning `None` if the name is ambiguous.
    pub fn resolve_wasi_source_for<'a>(
        &'a self,
        named: &'a crate::ast::NamedType,
        wasi_package_hint: Option<&str>,
    ) -> Option<&'a str> {
        if let Some(s) = named.source_interface.as_deref() {
            return if s.starts_with("wasi:") {
                Some(s)
            } else {
                None
            };
        }
        if let Some(pkg) = wasi_package_hint {
            let prefix = format!("wasi:{pkg}/");
            if let Some(s) = find_unique_source_with_prefix(&self.newtypes, &prefix, &named.name)
                .or_else(|| find_unique_source_with_prefix(&self.resources, &prefix, &named.name))
                .or_else(|| find_unique_source_with_prefix(&self.structs, &prefix, &named.name))
                .or_else(|| find_unique_source_with_prefix(&self.variants, &prefix, &named.name))
                .or_else(|| find_unique_source_with_prefix(&self.enums, &prefix, &named.name))
                .or_else(|| find_unique_source_with_prefix(&self.flags, &prefix, &named.name))
            {
                return Some(s);
            }
        }
        self.find_wasi_newtype_source(&named.name)
            .or_else(|| self.find_wasi_resource_source(&named.name))
            .or_else(|| self.find_wasi_struct_source(&named.name))
            .or_else(|| self.find_wasi_variant_source(&named.name))
            .or_else(|| self.find_wasi_enum_source(&named.name))
            .or_else(|| self.find_wasi_flags_source(&named.name))
    }

    /// Resolve a named type to its source interface across all CM namespaces
    /// (`wasi:*` and `core:kiln/*`). The Kiln-specific lookup is only
    /// consulted when the WASI lookup misses, preserving the strict
    /// cross-namespace scoping the rest of the binding pipeline relies on.
    ///
    /// Used by the flat-param lift path when the binding is for a
    /// `core:kiln/generator` world export and the parameter happens to be a
    /// `core:kiln/types` record such as `RawRequest`.
    pub fn resolve_cm_source_for<'a>(
        &'a self,
        named: &'a crate::ast::NamedType,
        wasi_package_hint: Option<&str>,
    ) -> Option<&'a str> {
        if let Some(s) = named.source_interface.as_deref() {
            return Some(s);
        }
        if let Some(s) = self.resolve_wasi_source_for(named, wasi_package_hint) {
            return Some(s);
        }
        self.find_kiln_struct_source(&named.name)
            .or_else(|| self.find_kiln_variant_source(&named.name))
            .or_else(|| self.find_kiln_enum_source(&named.name))
    }

    // -- Legacy unscoped lookups -------------------------------------------

    /// Get a newtype by name, when unambiguous across interfaces.
    pub fn get_newtype(&self, name: &str) -> Option<&Type> {
        find_unique_in(&self.newtypes, name)
    }

    /// Iterate all newtypes as `((source_interface, name), type)`.
    pub fn newtypes(&self) -> &IndexMap<(String, String), Type> {
        &self.newtypes
    }

    /// Check if a type name is a registered resource (in any interface).
    pub fn is_resource(&self, name: &str) -> bool {
        has_any_in(&self.resources, name)
    }

    /// Get the CM kebab-case name for a resource, when unambiguous.
    pub fn get_resource_cm_name(&self, name: &str) -> Option<&str> {
        find_unique_in(&self.resources, name).map(String::as_str)
    }

    /// Get the source interface path for a resource, when unambiguous.
    /// e.g., `TerminalInput` -> `"wasi:cli/terminal-input@0.3.0-rc-2026-01-06"`.
    pub fn get_resource_source_interface(&self, name: &str) -> Option<&str> {
        find_unique_source_in(&self.resources, name)
    }

    /// Check if a type name is a registered enum (in any interface).
    pub fn is_enum(&self, name: &str) -> bool {
        has_any_in(&self.enums, name)
    }

    /// Get the CM kebab-case name for an enum, when unambiguous.
    pub fn get_enum_cm_name(&self, name: &str) -> Option<&str> {
        find_unique_in(&self.enums, name).map(|(cm_name, _)| cm_name.as_str())
    }

    /// Get the CM enum variant names (in kebab-case), when unambiguous.
    pub fn get_enum_variants(&self, name: &str) -> Option<&[String]> {
        find_unique_in(&self.enums, name).map(|(_, variants)| variants.as_slice())
    }

    /// Get the CM enum variant names scoped to a specific source interface
    /// (e.g. `"wasi:cli/types@0.3.0-rc-2026-03-15"`). Disambiguates enums
    /// sharing a Wado name across interfaces. Falls back to the unique WASI
    /// cross-package registrant when the scoped interface does not define the
    /// name. The fallback is limited to the `wasi:` namespace.
    pub fn get_enum_variants_by_interface(
        &self,
        interface_path: &str,
        name: &str,
    ) -> Option<&[String]> {
        self.enums
            .get(&(interface_path.to_string(), name.to_string()))
            .or_else(|| find_unique_with_prefix(&self.enums, "wasi:", name))
            .map(|(_, variants)| variants.as_slice())
    }

    /// Get the CM enum name scoped to a specific source interface.
    pub fn get_enum_cm_name_by_interface(&self, interface_path: &str, name: &str) -> Option<&str> {
        self.enums
            .get(&(interface_path.to_string(), name.to_string()))
            .or_else(|| find_unique_with_prefix(&self.enums, "wasi:", name))
            .map(|(cm_name, _)| cm_name.as_str())
    }

    /// Check if a specific interface defines its own enum type (exact match, no fallback).
    pub fn has_enum_in_interface(&self, interface_path: &str, name: &str) -> bool {
        self.enums
            .contains_key(&(interface_path.to_string(), name.to_string()))
    }

    /// Check if a type name is a registered flags type (in any interface).
    pub fn is_flags(&self, name: &str) -> bool {
        has_any_in(&self.flags, name)
    }

    /// Get the CM kebab-case name for a flags type, when unambiguous.
    pub fn get_flags_cm_name(&self, name: &str) -> Option<&str> {
        find_unique_in(&self.flags, name).map(|(cm_name, _)| cm_name.as_str())
    }

    /// Get the CM member names (in kebab-case) for a flags type, when unambiguous.
    pub fn get_flags_members(&self, name: &str) -> Option<&[String]> {
        find_unique_in(&self.flags, name).map(|(_, members)| members.as_slice())
    }

    /// Check if a type name is a registered variant (in any interface).
    pub fn is_variant(&self, name: &str) -> bool {
        has_any_in(&self.variants, name)
    }

    /// Get the CM kebab-case name for a variant, when unambiguous.
    pub fn get_variant_cm_name(&self, name: &str) -> Option<&str> {
        find_unique_in(&self.variants, name).map(|(cm_name, _)| cm_name.as_str())
    }

    /// Get the variant cases, when unambiguous.
    pub fn get_variant_cases(&self, name: &str) -> Option<&[CmVariantCase]> {
        find_unique_in(&self.variants, name).map(|(_, cases)| cases.as_slice())
    }

    /// Get the variant cases scoped to a specific source interface. Falls back
    /// to the unique WASI cross-package registrant (scoped to `wasi:`) when
    /// the given interface does not define the name.
    pub fn get_variant_cases_by_interface(
        &self,
        interface_path: &str,
        name: &str,
    ) -> Option<&[CmVariantCase]> {
        self.variants
            .get(&(interface_path.to_string(), name.to_string()))
            .or_else(|| find_unique_with_prefix(&self.variants, "wasi:", name))
            .map(|(_, cases)| cases.as_slice())
    }

    /// Get the CM variant name scoped to a specific source interface. Falls
    /// back to the unique WASI cross-package registrant (scoped to `wasi:`).
    pub fn get_variant_cm_name_by_interface(
        &self,
        interface_path: &str,
        name: &str,
    ) -> Option<&str> {
        self.variants
            .get(&(interface_path.to_string(), name.to_string()))
            .or_else(|| find_unique_with_prefix(&self.variants, "wasi:", name))
            .map(|(cm_name, _)| cm_name.as_str())
    }

    /// Check if a type name is a registered struct (WIT record, in any interface).
    pub fn is_struct(&self, name: &str) -> bool {
        has_any_in(&self.structs, name)
    }

    /// Get the CM kebab-case name for a struct, when unambiguous.
    pub fn get_struct_cm_name(&self, name: &str) -> Option<&str> {
        find_unique_in(&self.structs, name).map(|(cm_name, _, _)| cm_name.as_str())
    }

    /// Get the fields of a struct (CM kebab-case field name, field type), when unambiguous.
    pub fn get_struct_fields(&self, name: &str) -> Option<&[(String, Type)]> {
        find_unique_in(&self.structs, name).map(|(_, fields, _)| fields.as_slice())
    }

    /// Get the source interface path for a struct, when unambiguous.
    pub fn get_struct_source_interface(&self, name: &str) -> Option<&str> {
        find_unique_source_in(&self.structs, name)
    }

    /// Find the interface name (e.g., `"types"`) for a WASI struct given its
    /// CM kebab name (e.g., `"directory-entry"`). Used by the component
    /// builder to alias types from WASI interface imports. Scoped to the
    /// `wasi:` namespace — non-WASI structs are never considered — and
    /// returns `Some` only when exactly one wasi interface declares the
    /// struct under that CM name (or a matching Wado name).
    pub fn find_interface_for_struct_cm_name(&self, cm_name: &str) -> Option<String> {
        let mut found: Option<String> = None;
        for ((source_path, wado_name), (struct_cm_name, _, _)) in &self.structs {
            if !source_path.starts_with("wasi:") {
                continue;
            }
            if struct_cm_name != cm_name && wado_name != cm_name {
                continue;
            }
            let wasi = CmImport::parse(source_path)?;
            if found.as_ref().is_some_and(|prev| prev != &wasi.interface) {
                return None; // ambiguous across interfaces — refuse to guess.
            }
            found = Some(wasi.interface);
        }
        found
    }

    /// Get the fields with Wado names, when unambiguous.
    pub fn get_struct_fields_with_wado_names(
        &self,
        name: &str,
    ) -> Option<&[(String, String, Type)]> {
        find_unique_in(&self.structs, name).map(|(_, _, wado_fields)| wado_fields.as_slice())
    }

    /// Iterate over all structs from a specific interface (matched by prefix).
    /// Returns (`wado_name`, `cm_name`, fields) in insertion order.
    pub fn structs_for_interface(
        &self,
        interface_prefix: &str,
    ) -> impl Iterator<Item = (&str, &str, &[(String, Type)])> {
        self.structs
            .iter()
            .filter_map(move |((source, name), (cm_name, fields, _))| {
                if source.starts_with(interface_prefix) {
                    Some((name.as_str(), cm_name.as_str(), fields.as_slice()))
                } else {
                    None
                }
            })
    }

    /// Iterate over variants from a specific interface (matched by prefix on
    /// the source interface). Returns (`wado_name`, `cm_name`, cases) in
    /// insertion order.
    pub fn variants_for_interface(
        &self,
        interface_prefix: &str,
    ) -> impl Iterator<Item = (&str, &str, &[CmVariantCase])> {
        self.variants
            .iter()
            .filter_map(move |((source, name), (cm_name, cases))| {
                if source.starts_with(interface_prefix) {
                    Some((name.as_str(), cm_name.as_str(), cases.as_slice()))
                } else {
                    None
                }
            })
    }

    /// Iterate over all resources from a specific interface (matched by prefix).
    /// Returns (`wado_name`, `cm_name`) in insertion order.
    pub fn resources_for_interface(
        &self,
        interface_prefix: &str,
    ) -> impl Iterator<Item = (&str, &str)> {
        self.resources
            .iter()
            .filter_map(move |((source, name), cm_name)| {
                if source.starts_with(interface_prefix) {
                    Some((name.as_str(), cm_name.as_str()))
                } else {
                    None
                }
            })
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
            && let Some(cm_name) = find_unique_in(&self.resources, &inner.name)
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
        // Build sets of known enum, variant, flags, and resource names
        // (bare name is enough here — the sets are only used as a membership check).
        let enums: IndexSet<&str> = self
            .enums
            .keys()
            .chain(self.variants.keys())
            .chain(self.flags.keys())
            .map(|(_, n)| n.as_str())
            .collect();
        let resources: IndexSet<&str> = self.resources.keys().map(|(_, n)| n.as_str()).collect();
        let structs: IndexSet<&str> = self.structs.keys().map(|(_, n)| n.as_str()).collect();

        // Check all parameter types
        for (_, _, ty) in &func.params {
            if !is_param_type_supported_with_types(ty, &enums, &resources, &structs) {
                return false;
            }
        }
        // Check return type if present - resolve newtypes first
        // Return types may contain newtypes like Mark, Duration, Instant
        // that need to be resolved to their underlying types for the support check
        if let Some(ret_ty) = &func.return_type {
            let resolved_ret = self.resolve_type(ret_ty);
            if !is_return_type_supported_with_types(&resolved_ret, &enums, &resources, &structs) {
                return false;
            }
        }
        true
    }

    /// Register a WASI function from an interface method
    ///
    /// # Arguments
    /// * `interface_name` - The interface name (e.g., "Stdout")
    /// * `method_name` - The method name (e.g., "`write_via_stream`")
    /// * `wasi` - The parsed WASI import metadata
    /// * `is_async` - Whether this is an async function
    /// * `params` - Parameter names and types
    /// * `return_type` - Return type (if any)
    pub fn register(
        &mut self,
        interface_name: &str,
        method_name: &str,
        wasi: &CmImport,
        is_async: bool,
        params: Vec<(String, String, Type)>,
        return_type: Option<Type>,
    ) {
        let interface_path = wasi.interface_path();

        // Get the WASI function name from the attribute, or derive from method name
        let wasi_func_name = wasi
            .function
            .clone()
            .unwrap_or_else(|| method_name.replace('_', "-"));

        // Resolve newtypes in params upfront
        // This ensures codegen doesn't need any type resolution logic
        let resolved_params: Vec<(String, String, Type)> = params
            .into_iter()
            .map(|(name, cm_name, ty)| (name, cm_name, self.resolve_type(&ty)))
            .collect();
        let func_info = WasiFunctionInfo {
            interface_name: interface_name.to_string(),
            method_name: method_name.to_string(),
            wasi_func_name: wasi_func_name.clone(),
            interface_path: interface_path.clone(),
            package: wasi.package.clone(),
            is_async,
            params: resolved_params,
            return_type,
        };

        // Generate the local alias name using utility function
        // Format: wasi:{package}/{interface_name}::{method_name}
        let local_name = func_info.local_alias_name();

        self.used_names.insert(local_name.clone());

        // Register in effect -> func map
        let qualified_name = format!("{interface_name}::{method_name}");
        self.effect_to_func
            .insert(qualified_name, func_info.clone());

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
            let wasi = CmImport::parse(path);

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
            if let Some(wasi) = crate::ast::CmImport::parse(path) {
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
            if let Some(wasi) = CmImport::parse(path)
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
            if let Some(wasi) = CmImport::parse(path)
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

    /// Resolve newtypes in a Type recursively
    ///
    /// This resolves newtypes like `Instant` -> `u64` throughout the type tree,
    /// including within generic type arguments.
    pub fn resolve_type(&self, ty: &Type) -> Type {
        match ty {
            Type::Named(named) => {
                if let Some(aliased_ty) = self.get_newtype(&named.name) {
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
                    id: generic.id,
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
                    is_mut: func_ty.is_mut,
                    params: resolved_params,
                    return_type: resolved_return,
                    effects: func_ty.effects.clone(),
                    effect_ids: func_ty.effect_ids.clone(),
                    stores: func_ty.stores.clone(),
                }))
            }
            // NamespacedGeneric types (like builtin::array<T>) are passed through
            Type::NamespacedGeneric(ng) => {
                let resolved_args: Vec<Type> =
                    ng.args.iter().map(|arg| self.resolve_type(arg)).collect();
                Type::NamespacedGeneric(crate::ast::NamespacedGenericType {
                    id: ng.id,
                    namespace: ng.namespace.clone(),
                    name: ng.name.clone(),
                    args: resolved_args,
                    span: ng.span,
                })
            }
            // TypePackSpread is only valid inside tuple types — pass through
            Type::TypePackSpread(..) => ty.clone(),
        }
    }
}

use wasm_encoder::{ComponentValType, InstanceType, PrimitiveValType, TypeBounds};

/// Helper for generating CM types within an [`InstanceType`] from registry metadata.
///
/// Tracks type indices and deduplicates types (borrow, list, result, etc.)
/// within a single instance type definition. Used by codegen to replace
/// hardcoded type indices with metadata-driven generation.
pub struct CmInstanceTypeGen {
    next_idx: u32,
    cache: IndexMap<String, u32>,
    /// Optional interface path hint for disambiguating types with the same name
    /// across different WASI interfaces (e.g., "wasi:http/types@..." to select
    /// HTTP's `ErrorCode` over filesystem's `ErrorCode`).
    interface_hint: Option<String>,
}

impl CmInstanceTypeGen {
    pub fn new(start_idx: u32) -> Self {
        Self {
            next_idx: start_idx,
            cache: IndexMap::default(),
            interface_hint: None,
        }
    }

    /// Create a new generator with an interface hint for type disambiguation.
    pub fn with_interface_hint(start_idx: u32, interface_hint: &str) -> Self {
        Self {
            next_idx: start_idx,
            cache: IndexMap::default(),
            interface_hint: Some(interface_hint.to_string()),
        }
    }

    /// Register a pre-existing type index for cache lookups
    pub fn register_existing(&mut self, key: &str, idx: u32) {
        self.cache.insert(key.to_string(), idx);
    }

    pub fn alloc_idx(&mut self) -> u32 {
        let idx = self.next_idx;
        self.next_idx += 1;
        idx
    }

    /// Returns the next type index that will be allocated.
    pub fn next_idx(&self) -> u32 {
        self.next_idx
    }

    /// Update the next type index (when external code has allocated indices in between calls).
    pub fn set_next_idx(&mut self, idx: u32) {
        self.next_idx = idx;
    }

    /// Compute a stable cache key for an AST type (ignoring spans)
    fn type_key(ty: &Type) -> String {
        match ty {
            Type::Named(n) => n.name.clone(),
            Type::Reference(inner) => format!("&{}", Self::type_key(inner)),
            Type::MutReference(inner) => format!("&mut {}", Self::type_key(inner)),
            Type::Generic(g) => {
                let args: Vec<String> = g.args.iter().map(Self::type_key).collect();
                format!("{}:{}", g.name, args.join(","))
            }
            Type::Tuple(elems) => {
                let args: Vec<String> = elems.iter().map(Self::type_key).collect();
                format!("[{}]", args.join(","))
            }
            _ => format!("{ty:?}"),
        }
    }

    /// Define a variant type and its named export, returning the exported type index.
    ///
    /// Handles payload types recursively via `ast_type_to_cm`.
    fn define_variant(
        &mut self,
        instance_type: &mut InstanceType,
        cm_name: &str,
        cases: &[CmVariantCase],
        wasi_registry: &WasiRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> u32 {
        let cache_key = format!("variant:{cm_name}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }

        // Build payload CM types first (before emitting the variant, to maintain type order)
        let payload_cm_types: Vec<Option<ComponentValType>> = cases
            .iter()
            .map(|case| {
                case.payload.as_ref().map(|ty| {
                    let resolved = wasi_registry.resolve_type(ty);
                    self.ast_type_to_cm(&resolved, instance_type, wasi_registry, resource_exports)
                })
            })
            .collect();

        let variant_cases: Vec<(&str, Option<ComponentValType>)> = cases
            .iter()
            .zip(payload_cm_types.iter())
            .map(|(case, payload_cm)| (case.cm_name.as_str(), *payload_cm))
            .collect();
        instance_type.ty().defined_type().variant(variant_cases);
        let variant_idx = self.alloc_idx();

        // Export to make it "named" (required by CM spec for records/variants)
        instance_type.export(
            cm_name,
            wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(variant_idx)),
        );
        let export_idx = self.alloc_idx();

        self.cache.insert(cache_key, export_idx);
        export_idx
    }

    /// Define a record (struct) type and its named export, returning the exported type index.
    fn define_record(
        &mut self,
        instance_type: &mut InstanceType,
        cm_name: &str,
        fields: &[(String, Type)],
        wasi_registry: &WasiRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> u32 {
        let cache_key = format!("record:{cm_name}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }

        // Build field CM types first
        let field_cm_types: Vec<(String, ComponentValType)> = fields
            .iter()
            .map(|(field_name, field_ty)| {
                let resolved = wasi_registry.resolve_type(field_ty);
                let cm_type =
                    self.ast_type_to_cm(&resolved, instance_type, wasi_registry, resource_exports);
                (field_name.clone(), cm_type)
            })
            .collect();

        let field_refs: Vec<(&str, ComponentValType)> = field_cm_types
            .iter()
            .map(|(n, t)| (n.as_str(), *t))
            .collect();
        instance_type.ty().defined_type().record(field_refs);
        let record_idx = self.alloc_idx();

        // Export the record type (required by CM spec)
        instance_type.export(
            cm_name,
            wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(record_idx)),
        );
        let export_idx = self.alloc_idx();

        self.cache.insert(cache_key, export_idx);
        export_idx
    }

    /// Define an option<T> type, returning the type index.
    fn define_option(
        &mut self,
        instance_type: &mut InstanceType,
        inner: ComponentValType,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("option:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        instance_type.ty().defined_type().option(inner);
        let idx = self.alloc_idx();
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a stream<T> type, returning the type index.
    fn define_stream(
        &mut self,
        instance_type: &mut InstanceType,
        elem: Option<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("stream:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        instance_type.ty().defined_type().stream(elem);
        let idx = self.alloc_idx();
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a future<T> type, returning the type index.
    fn define_future(
        &mut self,
        instance_type: &mut InstanceType,
        inner: Option<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("future:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        instance_type.ty().defined_type().future(inner);
        let idx = self.alloc_idx();
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a borrow type, returning the type index.
    fn define_borrow(
        &mut self,
        instance_type: &mut InstanceType,
        resource_export_idx: u32,
        resource_cm_name: &str,
    ) -> u32 {
        let cache_key = format!("borrow:{resource_cm_name}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        instance_type
            .ty()
            .defined_type()
            .borrow(resource_export_idx);
        let idx = self.alloc_idx();
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a list type, returning the type index.
    fn define_list(
        &mut self,
        instance_type: &mut InstanceType,
        elem_type: ComponentValType,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("list:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        instance_type.ty().defined_type().list(elem_type);
        let idx = self.alloc_idx();
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a tuple type, returning the type index.
    fn define_tuple(
        &mut self,
        instance_type: &mut InstanceType,
        elems: Vec<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("tuple:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        instance_type.ty().defined_type().tuple(elems);
        let idx = self.alloc_idx();
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a result type, returning the type index.
    fn define_result(
        &mut self,
        instance_type: &mut InstanceType,
        ok_type: Option<ComponentValType>,
        err_type: Option<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("result:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        instance_type.ty().defined_type().result(ok_type, err_type);
        let idx = self.alloc_idx();
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Convert a resolved Wado AST type to a CM [`ComponentValType`] within the instance type.
    ///
    /// Creates intermediate types as needed and caches them for deduplication.
    /// The `resource_exports` maps CM resource names (e.g., "fields") to their
    /// export indices within the instance type.
    pub fn ast_type_to_cm(
        &mut self,
        ty: &Type,
        instance_type: &mut InstanceType,
        wasi_registry: &WasiRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> ComponentValType {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "String" => ComponentValType::Primitive(PrimitiveValType::String),
                "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
                "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
                "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
                "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
                "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
                "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
                "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
                "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
                "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
                "char" => ComponentValType::Primitive(PrimitiveValType::Char),
                name => {
                    // Every branch here is emitting a WASI interface
                    // declaration. The type reference must have a resolved
                    // `wasi:*` source_interface from stdlib bootstrap, or
                    // fall back to the emitting interface (`interface_hint`)
                    // when the reference is synthesized, or to the unique
                    // `wasi:*` registrant as a last resort. If none match
                    // we panic — that would mean the caller handed us an
                    // unresolved reference to a type no interface declares.
                    let interface_hint_match: Option<String> =
                        self.interface_hint.as_deref().and_then(|h| {
                            let hit = wasi_registry
                                .get_resource_cm_name_by_source(h, name)
                                .is_some()
                                || wasi_registry.get_variant_cases_by_source(h, name).is_some()
                                || wasi_registry.get_struct_fields_by_source(h, name).is_some()
                                || wasi_registry.get_enum_variants_by_source(h, name).is_some()
                                || wasi_registry.get_flags_members_by_source(h, name).is_some();
                            hit.then(|| h.to_string())
                        });
                    let source_owned: String = named
                        .source_interface
                        .as_deref()
                        .filter(|s| s.starts_with("wasi:"))
                        .map(str::to_string)
                        .or(interface_hint_match)
                        .or_else(|| {
                            wasi_registry
                                .find_wasi_resource_source(name)
                                .or_else(|| wasi_registry.find_wasi_variant_source(name))
                                .or_else(|| wasi_registry.find_wasi_struct_source(name))
                                .or_else(|| wasi_registry.find_wasi_enum_source(name))
                                .or_else(|| wasi_registry.find_wasi_flags_source(name))
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| {
                            panic!(
                                "unresolved wasi named type reference `{name}` while emitting CM instance"
                            )
                        });
                    let source = source_owned.as_str();
                    if let Some(cm_name) =
                        wasi_registry.get_resource_cm_name_by_source(source, name)
                    {
                        let cache_key = format!("own:{cm_name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        let export_idx = resource_exports[cm_name];
                        instance_type.ty().defined_type().own(export_idx);
                        let idx = self.alloc_idx();
                        self.cache.insert(cache_key, idx);
                        ComponentValType::Type(idx)
                    } else if let Some(cases) = {
                        // Prefer the explicit `interface_hint` for cross-
                        // interface aliases (e.g. re-exported variants);
                        // otherwise use the resolved source.
                        let hint = self.interface_hint.clone();
                        let cases_opt = if let Some(h) = hint.as_deref() {
                            wasi_registry.get_variant_cases_by_interface(h, name)
                        } else {
                            wasi_registry.get_variant_cases_by_source(source, name)
                        };
                        cases_opt.map(<[CmVariantCase]>::to_vec)
                    } {
                        let cm_name = if let Some(hint) = &self.interface_hint {
                            wasi_registry
                                .get_variant_cm_name_by_interface(hint, name)
                                .expect("variant cm_name present when cases are")
                                .to_string()
                        } else {
                            wasi_registry
                                .get_variant_cm_name_by_source(source, name)
                                .expect("variant cm_name present when cases are")
                                .to_string()
                        };
                        let idx = self.define_variant(
                            instance_type,
                            &cm_name,
                            &cases,
                            wasi_registry,
                            resource_exports,
                        );
                        ComponentValType::Type(idx)
                    } else if let Some(fields) = wasi_registry
                        .get_struct_fields_by_source(source, name)
                        .map(<[(String, Type)]>::to_vec)
                    {
                        let cm_name = wasi_registry
                            .get_struct_cm_name_by_source(source, name)
                            .expect("struct cm_name present when fields are")
                            .to_string();
                        let idx = self.define_record(
                            instance_type,
                            &cm_name,
                            &fields,
                            wasi_registry,
                            resource_exports,
                        );
                        ComponentValType::Type(idx)
                    } else if let Some(variants) = wasi_registry
                        .get_enum_variants_by_source(source, name)
                        .map(<[String]>::to_vec)
                    {
                        let cache_key = format!("enum:{name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        instance_type
                            .ty()
                            .defined_type()
                            .enum_type(variants.iter().map(String::as_str));
                        let idx = self.alloc_idx();
                        self.cache.insert(cache_key.clone(), idx);

                        if let Some(cm_name) =
                            wasi_registry.get_enum_cm_name_by_source(source, name)
                        {
                            instance_type.export(
                                cm_name,
                                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(idx)),
                            );
                            let export_idx = self.alloc_idx();
                            self.cache.insert(cache_key, export_idx);
                            return ComponentValType::Type(export_idx);
                        }

                        ComponentValType::Type(idx)
                    } else if let Some(members) = wasi_registry
                        .get_flags_members_by_source(source, name)
                        .map(<[String]>::to_vec)
                    {
                        let cache_key = format!("flags:{name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        instance_type
                            .ty()
                            .defined_type()
                            .flags(members.iter().map(String::as_str));
                        let idx = self.alloc_idx();
                        self.cache.insert(cache_key, idx);
                        ComponentValType::Type(idx)
                    } else {
                        panic!("unsupported named type for CM instance: {name} (source={source})")
                    }
                }
            },
            Type::Reference(inner) | Type::MutReference(inner) => {
                if let Type::Named(n) = inner.as_ref()
                    && let Some(source) = n
                        .source_interface
                        .as_deref()
                        .filter(|s| s.starts_with("wasi:"))
                        .or_else(|| wasi_registry.find_wasi_resource_source(&n.name))
                    && let Some(cm_name) =
                        wasi_registry.get_resource_cm_name_by_source(source, &n.name)
                {
                    let export_idx = resource_exports[cm_name];
                    let idx = self.define_borrow(instance_type, export_idx, cm_name);
                    return ComponentValType::Type(idx);
                }
                panic!("unsupported reference type for CM instance: {ty:?}")
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "Array" => {
                    let elem_cm = self.ast_type_to_cm(
                        &generic.args[0],
                        instance_type,
                        wasi_registry,
                        resource_exports,
                    );
                    let key = Self::type_key(&generic.args[0]);
                    let idx = self.define_list(instance_type, elem_cm, &key);
                    ComponentValType::Type(idx)
                }
                "Result" => {
                    let is_ok_unit = matches!(&generic.args[0], Type::Tuple(t) if t.is_empty())
                        || matches!(&generic.args[0], Type::Named(n) if n.name == "()");
                    let is_err_unit = matches!(&generic.args[1], Type::Tuple(t) if t.is_empty())
                        || matches!(&generic.args[1], Type::Named(n) if n.name == "()");
                    let ok_type = if is_ok_unit {
                        None
                    } else {
                        Some(self.ast_type_to_cm(
                            &generic.args[0],
                            instance_type,
                            wasi_registry,
                            resource_exports,
                        ))
                    };
                    let err_type = if is_err_unit {
                        None
                    } else {
                        Some(self.ast_type_to_cm(
                            &generic.args[1],
                            instance_type,
                            wasi_registry,
                            resource_exports,
                        ))
                    };
                    let key = format!(
                        "{},{}",
                        Self::type_key(&generic.args[0]),
                        Self::type_key(&generic.args[1])
                    );
                    let idx = self.define_result(instance_type, ok_type, err_type, &key);
                    ComponentValType::Type(idx)
                }
                "Option" => {
                    let inner_cm = self.ast_type_to_cm(
                        &generic.args[0],
                        instance_type,
                        wasi_registry,
                        resource_exports,
                    );
                    let key = Self::type_key(&generic.args[0]);
                    let idx = self.define_option(instance_type, inner_cm, &key);
                    ComponentValType::Type(idx)
                }
                "Stream" => {
                    let (elem, key) = if generic.args.is_empty() {
                        (None, "unit".to_string())
                    } else {
                        let cm = self.ast_type_to_cm(
                            &generic.args[0],
                            instance_type,
                            wasi_registry,
                            resource_exports,
                        );
                        (Some(cm), Self::type_key(&generic.args[0]))
                    };
                    let idx = self.define_stream(instance_type, elem, &key);
                    ComponentValType::Type(idx)
                }
                "Future" => {
                    let (inner, key) = if generic.args.is_empty() {
                        (None, "unit".to_string())
                    } else {
                        let cm = self.ast_type_to_cm(
                            &generic.args[0],
                            instance_type,
                            wasi_registry,
                            resource_exports,
                        );
                        (Some(cm), Self::type_key(&generic.args[0]))
                    };
                    let idx = self.define_future(instance_type, inner, &key);
                    ComponentValType::Type(idx)
                }
                "AsyncCall" if generic.args.len() == 1 => {
                    // `AsyncCall<T>` is a Wado-level wrapper for CM async
                    // imports; the CM ABI-level type is the inner `T`.
                    // Transparently unwrap here so downstream code sees the
                    // CM signature.
                    self.ast_type_to_cm(
                        &generic.args[0],
                        instance_type,
                        wasi_registry,
                        resource_exports,
                    )
                }
                _ => panic!("unsupported generic type for CM instance: {}", generic.name),
            },
            Type::Tuple(elems) if elems.is_empty() => {
                panic!("unit type should be handled at Result level, not directly")
            }
            Type::Tuple(elems) => {
                let cm_elems: Vec<ComponentValType> = elems
                    .iter()
                    .map(|e| self.ast_type_to_cm(e, instance_type, wasi_registry, resource_exports))
                    .collect();
                let key = elems
                    .iter()
                    .map(Self::type_key)
                    .collect::<Vec<_>>()
                    .join(",");
                let idx = self.define_tuple(instance_type, cm_elems, &key);
                ComponentValType::Type(idx)
            }
            _ => panic!("unsupported type for CM instance: {ty:?}"),
        }
    }
}

/// Convert a pre-resolved AST type to Wasm `ValType`
///
/// This is a pure conversion function - newtypes must already be resolved
/// before calling this function. Use `WasiRegistry::resolve_type()` during
/// registration to ensure types are pre-resolved.
///
/// Note: This returns a SINGLE `ValType`. For compound types that lower to
/// multiple core values (like String → ptr+len), use `flatten_wasi_param_type` instead.
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
        Type::Reference(_) | Type::MutReference(_) => {
            // borrow<resource> or own<resource> - just an i32 handle
            ValType::I32
        }
        Type::Tuple(_) => ValType::I32,
        other => panic!("unsupported type variant in wasi_type_to_valtype: {other:?}"),
    }
}

/// Join two `ValType` options following CM union rules.
///
/// If both sides have the same type, use that. Otherwise, widen to i64.
/// If only one side is present, use that side's type.
fn join_val_types(a: Option<ValType>, b: Option<ValType>) -> ValType {
    match (a, b) {
        (Some(a), Some(b)) if a == b => a,
        (Some(_), Some(_)) => ValType::I64,
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => ValType::I32,
    }
}

/// Flatten a pre-resolved AST type into CM core-level `ValType`s.
///
/// Compound types like String and `Array<T>` are lowered to (ptr: i32, len: i32)
/// in the Component Model core ABI. This function pushes the appropriate number
/// of `ValType`s for each parameter.
pub fn flatten_wasi_param_type(ty: &Type, out: &mut Vec<ValType>, registry: &WasiRegistry) {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            // String is lowered to (ptr: i32, len: i32) in CM core ABI
            "String" => {
                out.push(ValType::I32); // ptr
                out.push(ValType::I32); // len
            }
            "i32" | "u32" | "bool" | "char" | "u8" | "i8" | "u16" | "i16" => {
                out.push(ValType::I32);
            }
            "i64" | "u64" => out.push(ValType::I64),
            "f32" => out.push(ValType::F32),
            "f64" => out.push(ValType::F64),
            // Unit type — no core values
            "()" => {}
            // Struct (record) types flatten to concatenation of field flat
            // types. Source is taken from the Named reference when present,
            // otherwise we fall back to the unique `wasi:*` registrant.
            name if named
                .source_interface
                .as_deref()
                .filter(|s| s.starts_with("wasi:"))
                .or_else(|| registry.find_wasi_struct_source(name))
                .and_then(|s| registry.get_struct_fields_by_source(s, name))
                .is_some() =>
            {
                let source = named
                    .source_interface
                    .as_deref()
                    .filter(|s| s.starts_with("wasi:"))
                    .or_else(|| registry.find_wasi_struct_source(name))
                    .expect("already matched above");
                let fields = registry
                    .get_struct_fields_by_source(source, name)
                    .expect("already matched above");
                for (_, field_ty) in fields {
                    flatten_wasi_param_type(field_ty, out, registry);
                }
            }
            // Variant types flatten to: discriminant i32 + union(max payload)
            name if named
                .source_interface
                .as_deref()
                .filter(|s| s.starts_with("wasi:"))
                .or_else(|| registry.find_wasi_variant_source(name))
                .and_then(|s| registry.get_variant_cases_by_source(s, name))
                .is_some() =>
            {
                out.push(ValType::I32); // discriminant
                let source = named
                    .source_interface
                    .as_deref()
                    .filter(|s| s.starts_with("wasi:"))
                    .or_else(|| registry.find_wasi_variant_source(name))
                    .expect("already matched above");
                if let Some(cases) = registry.get_variant_cases_by_source(source, name) {
                    let mut max_flat: Vec<ValType> = Vec::new();
                    for case in cases {
                        if let Some(payload_ty) = &case.payload {
                            let mut case_flat = Vec::new();
                            flatten_wasi_param_type(payload_ty, &mut case_flat, registry);
                            let len = case_flat.len().max(max_flat.len());
                            for i in 0..len {
                                let old = max_flat.get(i).copied();
                                let new = case_flat.get(i).copied();
                                if i < max_flat.len() {
                                    max_flat[i] = join_val_types(old, new);
                                } else {
                                    max_flat.push(join_val_types(old, new));
                                }
                            }
                        }
                    }
                    out.extend(max_flat);
                }
            }
            // Resource handles, enums, etc.
            _ => out.push(ValType::I32),
        },
        Type::Generic(generic) => match generic.name.as_str() {
            // list<T> is lowered to (ptr: i32, len: i32) in CM core ABI
            "Array" => {
                out.push(ValType::I32); // ptr
                out.push(ValType::I32); // len
            }
            // Stream and Future are single i32 handles
            "Stream" | "Future" => out.push(ValType::I32),
            // option<T> flattens to: discriminant i32 + flatten(T)
            "Option" if generic.args.len() == 1 => {
                out.push(ValType::I32); // discriminant
                flatten_wasi_param_type(&generic.args[0], out, registry);
            }
            // result<T, E> flattens to: discriminant i32 + union(flatten(T), flatten(E))
            "Result" if generic.args.len() == 2 => {
                out.push(ValType::I32); // discriminant
                let mut ok_flat = Vec::new();
                let mut err_flat = Vec::new();
                flatten_wasi_param_type(&generic.args[0], &mut ok_flat, registry);
                flatten_wasi_param_type(&generic.args[1], &mut err_flat, registry);
                let max_len = ok_flat.len().max(err_flat.len());
                for i in 0..max_len {
                    let ok_val = ok_flat.get(i).copied();
                    let err_val = err_flat.get(i).copied();
                    out.push(join_val_types(ok_val, err_val));
                }
            }
            _ => out.push(ValType::I32),
        },
        // borrow<resource> - i32 handle
        Type::Reference(_) | Type::MutReference(_) => out.push(ValType::I32),
        // Unit type - no core values
        Type::Tuple(elems) if elems.is_empty() => {}
        Type::Tuple(_) => out.push(ValType::I32),
        _ => out.push(ValType::I32),
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

/// Check if a parameter type is supported for Component Model generation
///
/// Type aliases (like Instant, Duration) should already be resolved to their
/// underlying types before this check.
/// The `enums` and `resources` sets contain known enum/resource type names.
fn is_param_type_supported_with_types(
    ty: &Type,
    enums: &IndexSet<&str>,
    resources: &IndexSet<&str>,
    structs: &IndexSet<&str>,
) -> bool {
    match ty {
        Type::Named(named) => {
            let name = named.name.as_str();
            // Check primitives and unit type
            // Unit type () is parsed as Named("()"), not Tuple([])
            // Resource types are passed as borrow<resource> in CM (i32 handle in core wasm)
            // Struct types (records) like Instant are also supported as params
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
                || structs.contains(name)
        }
        Type::Generic(generic) => {
            matches!(
                generic.name.as_str(),
                "Stream" | "Result" | "Future" | "Option" | "Array"
            )
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            // borrow<resource> - passed as i32 handle at CM boundary
            if let Type::Named(named) = inner.as_ref() {
                resources.contains(named.name.as_str())
            } else {
                false
            }
        }
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
    enums: &IndexSet<&str>,
    resources: &IndexSet<&str>,
    structs: &IndexSet<&str>,
) -> bool {
    match ty {
        Type::Named(named) => {
            let name = named.name.as_str();
            // Check primitives, enums, resources, structs, and unit type
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
                || structs.contains(name)
        }
        Type::Generic(generic) => {
            match generic.name.as_str() {
                "Stream" | "Future" => true,
                "AsyncCall" if generic.args.len() == 1 => {
                    // `AsyncCall<T>` is the Wado-level wrapper for async CM imports;
                    // the CM ABI return is the inner `T`. Support depends on `T`.
                    is_return_type_supported_with_types(&generic.args[0], enums, resources, structs)
                }
                "Result" => {
                    // Result<T, E> - both T and E must be supported
                    generic.args.iter().all(|arg| {
                        is_return_type_supported_with_types(arg, enums, resources, structs)
                    })
                }
                "Array" | "Option" => {
                    // Recursively check that inner types are supported primitives
                    generic
                        .args
                        .iter()
                        .all(|arg| is_primitive_type_supported_with_types(arg, enums, resources))
                }
                "Tuple" => {
                    // All tuple elements must be supported return types
                    generic.args.iter().all(|arg| {
                        is_return_type_supported_with_types(arg, enums, resources, structs)
                    })
                }
                _ => false,
            }
        }
        // Handle [...] tuple syntax
        Type::Tuple(elements) => {
            // Empty tuple () is the unit type, which is always supported
            if elements.is_empty() {
                return true;
            }
            elements
                .iter()
                .all(|el| is_return_type_supported_with_types(el, enums, resources, structs))
        }
        _ => false,
    }
}

/// Check if a type is a supported primitive type (for inner types of Array/Option/Tuple)
fn is_primitive_type_supported_with_types(
    ty: &Type,
    enums: &IndexSet<&str>,
    resources: &IndexSet<&str>,
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
    is_param_type_supported_with_types(
        ty,
        &IndexSet::default(),
        &IndexSet::default(),
        &IndexSet::default(),
    )
}

/// Check if a return type is supported (without enum/resource/struct knowledge)
pub fn is_return_type_supported(ty: &Type) -> bool {
    is_return_type_supported_with_types(
        ty,
        &IndexSet::default(),
        &IndexSet::default(),
        &IndexSet::default(),
    )
}

/// Check if all types in a WASI function are supported for Component Model generation
/// (without enum/resource knowledge - use `WasiRegistry::is_function_supported` instead)
pub fn is_wasi_function_supported(func: &WasiFunctionInfo) -> bool {
    // Check all parameter types (Result not allowed in params)
    for (_, _, ty) in &func.params {
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
    // Check if an AST type represents unit () — can be Tuple([]) or Named("()")
    let is_unit = |t: &Type| -> bool {
        matches!(t, Type::Tuple(e) if e.is_empty()) || matches!(t, Type::Named(n) if n.name == "()")
    };
    match ty {
        // Simple types are returned directly
        Type::Named(named) => matches!(
            named.name.as_str(),
            "String" // String (list<u8> in CM) requires outptr
        ),
        // Generic types that require outptr
        Type::Generic(generic) => match generic.name.as_str() {
            // Result<(), ()> returns a single i32 discriminant — no outptr needed
            "Result"
                if generic.args.len() == 2
                    && is_unit(&generic.args[0])
                    && is_unit(&generic.args[1]) =>
            {
                false
            }
            "Array" | "Option" | "Result" | "Tuple" => true,
            _ => false,
        },
        // Tuple types [...] require outptr (non-empty tuples only)
        Type::Tuple(elems) => !elems.is_empty(),
        _ => false,
    }
}

/// Check if a named WASI type is a variant with payload cases, requiring outptr return.
///
/// Types like `Method` (which has `Other(String)`) flatten to more than
/// `MAX_FLAT_RESULTS` core values, so they must be returned via an outptr.
/// Generic `cm_flat_types` doesn't know about WASI variant cases; this
/// registry-aware check fills that gap.
pub fn wasi_named_type_return_needs_outptr(ty: &Type, registry: &WasiRegistry) -> bool {
    if let Type::Named(named) = ty
        && let Some(source) = registry.resolve_wasi_source_for(named, None)
    {
        if let Some(cases) = registry.get_variant_cases_by_source(source, &named.name) {
            return cases.iter().any(|case| case.payload.is_some());
        }
        // WASI structs (records) always need outptr — they have multiple fields
        if registry
            .get_struct_fields_by_source(source, &named.name)
            .is_some()
        {
            return true;
        }
    }
    false
}

/// Registry-aware CM canonical ABI size for a type.
///
/// Resolves WASI structs (records), enums, flags, and variants through the registry
/// to compute their true CM layout size, instead of defaulting to 4 (i32 handle).
pub fn cm_size_with_registry(ty: &Type, registry: &WasiRegistry) -> u32 {
    match ty {
        Type::Named(named) => {
            let Some(source) = registry.resolve_cm_source_for(named, None) else {
                return crate::cm_abi::cm_size(ty);
            };
            if let Some(resolved) = registry.get_newtype_by_source(source, &named.name) {
                return cm_size_with_registry(resolved, registry);
            }
            if let Some(fields) = registry.get_struct_fields_by_source(source, &named.name) {
                let resolved_fields: Vec<Type> = fields
                    .iter()
                    .map(|(_, ty)| registry.resolve_type(ty))
                    .collect();
                let mut offset = 0u32;
                let mut max_align = 1u32;
                for field_ty in &resolved_fields {
                    let fa = cm_align_with_registry(field_ty, registry);
                    let fs = cm_size_with_registry(field_ty, registry);
                    offset = crate::cm_abi::align_to(offset, fa);
                    offset += fs;
                    max_align = max_align.max(fa);
                }
                return crate::cm_abi::align_to(offset, max_align);
            }
            if let Some(sa) = wasi_variant_cm_size_align(named, registry) {
                return sa.0;
            }
            if let Some(variants) = registry.get_enum_variants_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_enum_byte_size(variants.len());
            }
            if let Some(members) = registry.get_flags_members_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_flags_byte_size(members.len());
            }
            crate::cm_abi::cm_size(ty)
        }
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => {
                let inner = &g.args[0];
                let payload_align = cm_align_with_registry(inner, registry);
                let payload_size = cm_size_with_registry(inner, registry);
                let payload_offset = crate::cm_abi::align_to(1, payload_align);
                let overall_align = 1u32.max(payload_align);
                crate::cm_abi::align_to(payload_offset + payload_size, overall_align)
            }
            "Result" if g.args.len() == 2 => {
                let ok_size = cm_size_with_registry(&g.args[0], registry);
                let err_size = cm_size_with_registry(&g.args[1], registry);
                let payload_size = ok_size.max(err_size);
                let payload_align = cm_align_with_registry(&g.args[0], registry)
                    .max(cm_align_with_registry(&g.args[1], registry));
                let payload_offset = crate::cm_abi::align_to(1, payload_align);
                let overall_align = 1u32.max(payload_align);
                crate::cm_abi::align_to(payload_offset + payload_size, overall_align)
            }
            _ => crate::cm_abi::cm_size(ty),
        },
        _ => crate::cm_abi::cm_size(ty),
    }
}

/// Registry-aware CM canonical ABI alignment for a type.
pub fn cm_align_with_registry(ty: &Type, registry: &WasiRegistry) -> u32 {
    match ty {
        Type::Named(named) => {
            let Some(source) = registry.resolve_cm_source_for(named, None) else {
                return crate::cm_abi::cm_align(ty);
            };
            if let Some(resolved) = registry.get_newtype_by_source(source, &named.name) {
                return cm_align_with_registry(resolved, registry);
            }
            if let Some(fields) = registry.get_struct_fields_by_source(source, &named.name) {
                let mut max_align = 1u32;
                for (_, field_ty) in fields {
                    let resolved = registry.resolve_type(field_ty);
                    max_align = max_align.max(cm_align_with_registry(&resolved, registry));
                }
                return max_align;
            }
            if let Some(sa) = wasi_variant_cm_size_align(named, registry) {
                return sa.1;
            }
            if let Some(variants) = registry.get_enum_variants_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_enum_byte_size(variants.len());
            }
            if let Some(members) = registry.get_flags_members_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_flags_byte_align(members.len());
            }
            crate::cm_abi::cm_align(ty)
        }
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => 1u32.max(cm_align_with_registry(&g.args[0], registry)),
            "Result" if g.args.len() == 2 => 1u32
                .max(cm_align_with_registry(&g.args[0], registry))
                .max(cm_align_with_registry(&g.args[1], registry)),
            _ => crate::cm_abi::cm_align(ty),
        },
        _ => crate::cm_abi::cm_align(ty),
    }
}

/// Compute the CM canonical-ABI size and alignment for a WASI variant type.
///
/// Returns `None` if the type is not a known WASI variant with payload cases.
///
/// Layout (Canonical ABI):
/// - discriminant: 1 byte (u8) for variants with ≤ 256 cases
/// - payload: at `align_to(1, max_payload_align)`
/// - total: `align_to(payload_offset + max_payload_size, max_payload_align)`
pub fn wasi_variant_cm_size_align(
    named: &crate::ast::NamedType,
    registry: &WasiRegistry,
) -> Option<(u32, u32)> {
    wasi_variant_cm_size_align_scoped(named, registry, None)
}

/// Package-scoped variant of `wasi_variant_cm_size_align`.
///
/// When the reference's `source_interface` is populated (stdlib bootstrap),
/// that exact source is used. Otherwise the unique `wasi:*` registrant for
/// the name is used. `wasi_package` is kept as a parameter so that existing
/// callers can still supply it; it is currently unused here because source
/// resolution is already unambiguous.
pub fn wasi_variant_cm_size_align_scoped(
    named: &crate::ast::NamedType,
    registry: &WasiRegistry,
    _wasi_package: Option<&str>,
) -> Option<(u32, u32)> {
    let source = registry.resolve_wasi_source_for(named, None)?;
    let cases = registry.get_variant_cases_by_source(source, &named.name)?;
    if !cases.iter().any(|case| case.payload.is_some()) {
        return None; // no payload cases — not outptr
    }
    let mut max_payload_size = 0u32;
    let mut max_payload_align = 1u32;
    for case in cases {
        if let Some(ty) = &case.payload {
            max_payload_size =
                max_payload_size.max(cm_size_with_registry_scoped(ty, registry, None));
            max_payload_align =
                max_payload_align.max(cm_align_with_registry_scoped(ty, registry, None));
        }
    }
    let disc_size = 1u32; // u8 for n ≤ 256 cases
    let payload_offset = crate::cm_abi::align_to(disc_size, max_payload_align);
    let overall_align = max_payload_align; // max(disc_align=1, payload_align)
    let size = crate::cm_abi::align_to(payload_offset + max_payload_size, overall_align);
    Some((size, overall_align))
}

/// Package-scoped CM canonical ABI size for a type.
///
/// Like `cm_size_with_registry`, but uses `wasi_package` to disambiguate types
/// with the same name across different WASI packages.
pub fn cm_size_with_registry_scoped(
    ty: &Type,
    registry: &WasiRegistry,
    wasi_package: Option<&str>,
) -> u32 {
    match ty {
        Type::Named(named) => {
            let Some(source) = registry.resolve_cm_source_for(named, None) else {
                return crate::cm_abi::cm_size(ty);
            };
            if let Some(resolved) = registry.get_newtype_by_source(source, &named.name) {
                return cm_size_with_registry_scoped(resolved, registry, wasi_package);
            }
            if let Some(fields) = registry.get_struct_fields_by_source(source, &named.name) {
                let resolved_fields: Vec<Type> = fields
                    .iter()
                    .map(|(_, ty)| registry.resolve_type(ty))
                    .collect();
                let mut offset = 0u32;
                let mut max_align = 1u32;
                for field_ty in &resolved_fields {
                    let fa = cm_align_with_registry_scoped(field_ty, registry, wasi_package);
                    let fs = cm_size_with_registry_scoped(field_ty, registry, wasi_package);
                    offset = crate::cm_abi::align_to(offset, fa);
                    offset += fs;
                    max_align = max_align.max(fa);
                }
                return crate::cm_abi::align_to(offset, max_align);
            }
            if let Some(sa) = wasi_variant_cm_size_align_scoped(named, registry, wasi_package) {
                return sa.0;
            }
            if let Some(variants) = registry.get_enum_variants_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_enum_byte_size(variants.len());
            }
            if let Some(members) = registry.get_flags_members_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_flags_byte_size(members.len());
            }
            crate::cm_abi::cm_size(ty)
        }
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => {
                let inner = &g.args[0];
                let payload_align = cm_align_with_registry_scoped(inner, registry, wasi_package);
                let payload_size = cm_size_with_registry_scoped(inner, registry, wasi_package);
                let payload_offset = crate::cm_abi::align_to(1, payload_align);
                let overall_align = 1u32.max(payload_align);
                crate::cm_abi::align_to(payload_offset + payload_size, overall_align)
            }
            "Result" if g.args.len() == 2 => {
                let ok_size = cm_size_with_registry_scoped(&g.args[0], registry, wasi_package);
                let err_size = cm_size_with_registry_scoped(&g.args[1], registry, wasi_package);
                let payload_size = ok_size.max(err_size);
                let payload_align =
                    cm_align_with_registry_scoped(&g.args[0], registry, wasi_package).max(
                        cm_align_with_registry_scoped(&g.args[1], registry, wasi_package),
                    );
                let payload_offset = crate::cm_abi::align_to(1, payload_align);
                let overall_align = 1u32.max(payload_align);
                crate::cm_abi::align_to(payload_offset + payload_size, overall_align)
            }
            _ => crate::cm_abi::cm_size(ty),
        },
        _ => crate::cm_abi::cm_size(ty),
    }
}

/// Package-scoped CM canonical ABI alignment for a type.
pub fn cm_align_with_registry_scoped(
    ty: &Type,
    registry: &WasiRegistry,
    wasi_package: Option<&str>,
) -> u32 {
    match ty {
        Type::Named(named) => {
            let Some(source) = registry.resolve_cm_source_for(named, None) else {
                return crate::cm_abi::cm_align(ty);
            };
            if let Some(resolved) = registry.get_newtype_by_source(source, &named.name) {
                return cm_align_with_registry_scoped(resolved, registry, wasi_package);
            }
            if let Some(fields) = registry.get_struct_fields_by_source(source, &named.name) {
                let mut max_align = 1u32;
                for (_, field_ty) in fields {
                    let resolved = registry.resolve_type(field_ty);
                    max_align = max_align.max(cm_align_with_registry_scoped(
                        &resolved,
                        registry,
                        wasi_package,
                    ));
                }
                return max_align;
            }
            if let Some(sa) = wasi_variant_cm_size_align_scoped(named, registry, wasi_package) {
                return sa.1;
            }
            if let Some(variants) = registry.get_enum_variants_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_enum_byte_size(variants.len());
            }
            if let Some(members) = registry.get_flags_members_by_source(source, &named.name) {
                return crate::synthesis::cm_binding::cm_flags_byte_align(members.len());
            }
            crate::cm_abi::cm_align(ty)
        }
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => 1u32.max(cm_align_with_registry_scoped(
                &g.args[0],
                registry,
                wasi_package,
            )),
            "Result" if g.args.len() == 2 => 1u32
                .max(cm_align_with_registry_scoped(
                    &g.args[0],
                    registry,
                    wasi_package,
                ))
                .max(cm_align_with_registry_scoped(
                    &g.args[1],
                    registry,
                    wasi_package,
                )),
            _ => crate::cm_abi::cm_align(ty),
        },
        _ => crate::cm_abi::cm_align(ty),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Span;

    fn make_span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    fn make_stream_u8_type() -> Type {
        Type::Generic(crate::ast::GenericType {
            id: crate::ast::AstId::fresh(),
            name: "Stream".to_string(),
            args: vec![Type::Named(crate::ast::NamedType {
                id: crate::ast::AstId::fresh(),
                name: "u8".to_string(),
                span: make_span(),
                source_interface: None,
            })],
            span: make_span(),
        })
    }

    fn make_result_type() -> Type {
        Type::Generic(crate::ast::GenericType {
            id: crate::ast::AstId::fresh(),
            name: "Result".to_string(),
            args: vec![
                Type::Tuple(vec![]), // ()
                Type::Named(crate::ast::NamedType {
                    id: crate::ast::AstId::fresh(),
                    name: "ErrorCode".to_string(),
                    span: make_span(),
                    source_interface: None,
                }),
            ],
            span: make_span(),
        })
    }

    #[test]
    fn test_register_and_resolve() {
        let mut registry = WasiRegistry::new();

        let wasi = CmImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();

        registry.register(
            "Stdout",
            "write_via_stream",
            &wasi,
            true,
            vec![(
                "data".to_string(),
                "data".to_string(),
                make_stream_u8_type(),
            )],
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
            CmImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();
        registry.register(
            "Stdout",
            "write_via_stream",
            &stdout_wasi,
            true,
            vec![(
                "data".to_string(),
                "data".to_string(),
                make_stream_u8_type(),
            )],
            Some(make_result_type()),
        );

        // Register stderr - different interface, same function name
        let stderr_wasi =
            CmImport::parse("wasi:cli/stderr@0.3.0-rc-2025-09-16#write-via-stream").unwrap();
        registry.register(
            "Stderr",
            "write_via_stream",
            &stderr_wasi,
            true,
            vec![(
                "data".to_string(),
                "data".to_string(),
                make_stream_u8_type(),
            )],
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

        let wasi = CmImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();

        registry.register(
            "Stdout",
            "write_via_stream",
            &wasi,
            true,
            vec![(
                "data".to_string(),
                "data".to_string(),
                make_stream_u8_type(),
            )],
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
            interface_name: "Stdout".to_string(),
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

    #[test]
    fn test_array_and_option_type_support() {
        use crate::ast::{GenericType, NamedType};

        // Array<String> should be supported
        let array_string = Type::Generic(GenericType {
            id: crate::ast::AstId::fresh(),
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                id: crate::ast::AstId::fresh(),
                name: "String".to_string(),
                span: make_span(),
                source_interface: None,
            })],
            span: make_span(),
        });
        assert!(
            is_return_type_supported(&array_string),
            "Array<String> should be supported"
        );

        // Array<Tuple<String, String>> should be supported
        let tuple_ss = Type::Generic(GenericType {
            id: crate::ast::AstId::fresh(),
            name: "Tuple".to_string(),
            args: vec![
                Type::Named(NamedType {
                    id: crate::ast::AstId::fresh(),
                    name: "String".to_string(),
                    span: make_span(),
                    source_interface: None,
                }),
                Type::Named(NamedType {
                    id: crate::ast::AstId::fresh(),
                    name: "String".to_string(),
                    span: make_span(),
                    source_interface: None,
                }),
            ],
            span: make_span(),
        });
        let array_tuple = Type::Generic(GenericType {
            id: crate::ast::AstId::fresh(),
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
            id: crate::ast::AstId::fresh(),
            name: "Option".to_string(),
            args: vec![Type::Named(NamedType {
                id: crate::ast::AstId::fresh(),
                name: "String".to_string(),
                span: make_span(),
                source_interface: None,
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
        let resolved = registry.resolve("TcpSocket::create");
        assert!(
            resolved.is_some(),
            "TcpSocket::create should be resolved, got {resolved:?}"
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
