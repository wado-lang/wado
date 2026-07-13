//! Component Model support for code generation
//!
//! This module provides:
//! - WASI import registry: collects WASI imports from effect definitions in lib/wasi/*.wado
//! - Component Model ABI: type conversion and support checking for CM codegen

use std::collections::{BTreeMap, BTreeSet};

use crate::hashmap::{IndexMap, IndexSet};

use wasm_encoder::ValType;

use crate::ast::{Attribute, CmImport, GenericType, Type};
use crate::module_source::ModuleSource;
use crate::name::to_kebab;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::wir::{CmFuturePayload, CmPayloadType, CmScalarType};

/// Classify a future's type argument into the CM future payload category.
/// The single source of truth shared by the future-read synthesis path and the
/// WIR-build future-write / cancel / drop / new paths, so every operation on a
/// given `Future<T>` agrees on its component-level future type.
///
/// - Primitive scalar → `Scalar(s)`
/// - `Result<(), E>` (unit Ok type) → `Transmission(error-code package)`
/// - Anything else → `Trailers` (the HTTP trailers pattern)
pub fn classify_future_payload(type_table: &TypeTable, type_arg: TypeId) -> CmFuturePayload {
    match type_table.get(type_arg) {
        ResolvedType::Primitive(prim) => {
            if let Some(scalar) = primitive_to_cm_scalar(prim) {
                return CmFuturePayload::Scalar(scalar);
            }
        }
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == "Result" && type_args.len() >= 2 => {
            if matches!(type_table.get(type_args[0]), ResolvedType::Unit) {
                return CmFuturePayload::Transmission(error_code_source(type_table, type_args[1]));
            }
        }
        _ => {}
    }
    // A general value payload (`future<string>`, `future<list<u32>>`, …). Named
    // types (records / variants / enums / resources) currently return `None`,
    // so the WASI trailers / transmission shapes fall through to the legacy
    // `Trailers` classification unchanged.
    if let Some(payload) = cm_payload_type_from_type_id(type_table, type_arg) {
        return CmFuturePayload::Value(payload);
    }
    CmFuturePayload::Trailers
}

/// Classify a `stream<T>` element type into its CM stream payload. `u8` keeps
/// the default `U8` stream; scalar / structural elements use `Value`; anything
/// else (named records / unsupported) falls back to `U8` so existing behaviour
/// is unchanged.
pub fn classify_stream_payload(
    type_table: &TypeTable,
    element: TypeId,
) -> crate::wir::CmStreamPayload {
    use crate::wir::CmStreamPayload;
    if matches!(
        type_table.get(element),
        ResolvedType::Primitive(PrimitiveType::U8)
    ) {
        return CmStreamPayload::U8;
    }
    match cm_payload_type_from_type_id(type_table, element) {
        Some(payload) => CmStreamPayload::Value(payload),
        None => CmStreamPayload::U8,
    }
}

/// Build a self-contained [`CmPayloadType`] from a resolved type, for use as a
/// `future<T>` / `stream<T>` payload identity. Returns `None` for types not yet
/// supported as general payloads (named records / variants / enums / resources,
/// 128-bit and SIMD primitives), so callers fall back to legacy handling.
pub fn cm_payload_type_from_type_id(
    type_table: &TypeTable,
    type_id: TypeId,
) -> Option<CmPayloadType> {
    if let Some(inner) = type_table.as_option(type_id) {
        return Some(CmPayloadType::Option(Box::new(
            cm_payload_type_from_type_id(type_table, inner)?,
        )));
    }
    if let Some(inner) = type_table.as_list(type_id) {
        return Some(CmPayloadType::List(Box::new(cm_payload_type_from_type_id(
            type_table, inner,
        )?)));
    }
    if let Some(elems) = type_table.as_tuple(type_id) {
        let elems = elems
            .iter()
            .map(|&e| cm_payload_type_from_type_id(type_table, e))
            .collect::<Option<Vec<_>>>()?;
        return Some(CmPayloadType::Tuple(elems));
    }
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => primitive_to_cm_scalar(prim).map(CmPayloadType::Scalar),
        ResolvedType::Struct { name, .. } if name == "String" => Some(CmPayloadType::String),
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == "Result" && type_args.len() == 2 => {
            let arm = |id: TypeId| -> Option<Option<Box<CmPayloadType>>> {
                if matches!(type_table.get(id), ResolvedType::Unit) {
                    Some(None)
                } else {
                    Some(Some(Box::new(cm_payload_type_from_type_id(
                        type_table, id,
                    )?)))
                }
            };
            Some(CmPayloadType::Result(
                arm(type_args[0])?,
                arm(type_args[1])?,
            ))
        }
        // A user/dependency record: lower/lift it as a named CM record. WASI and
        // kiln records keep their own (registry-driven) paths, so they stay
        // `None` here and fall through to the legacy classification.
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } if !is_cm_owned_source(module_source) => Some(CmPayloadType::Named(to_kebab(name))),
        _ => None,
    }
}

/// Whether a type's module source already owns a CM lowering path (`wasi:*`
/// interfaces and the `core:kiln/*` generator surface). User, local, and
/// dependency records do not, so they route through the general `Named` payload.
/// Whether a type's module source already owns a CM lowering path: `wasi:*`
/// interfaces and the `core:kiln/*` generator surface (modules sourced as
/// `kiln/...`). Keep in sync with the AST mirror in `cm_payload_type_from_ast`,
/// which applies the equivalent `wasi:` / `core:kiln/` check on the resolved
/// source string.
fn is_cm_owned_source(ms: &ModuleSource) -> bool {
    match ms {
        ModuleSource::Wasi { .. } => true,
        // The `core:kiln` facade itself (`name == "kiln"`) plus its WIT-generated
        // submodules (`kiln/...`). `kilnfoo` is unrelated, so match exactly.
        ModuleSource::Core { name } => {
            name.as_str() == "kiln" || name.as_str().starts_with("kiln/")
        }
        _ => false,
    }
}

/// CM scalar for a primitive type's Wado name (`"u32"`, `"i8"`, `"f64"`, …).
fn cm_scalar_from_ast_name(name: &str) -> Option<CmScalarType> {
    Some(match name {
        "bool" => CmScalarType::Bool,
        "char" => CmScalarType::Char,
        "u8" => CmScalarType::U8,
        "u16" => CmScalarType::U16,
        "u32" => CmScalarType::U32,
        "u64" => CmScalarType::U64,
        "i8" => CmScalarType::S8,
        "i16" => CmScalarType::S16,
        "i32" => CmScalarType::S32,
        "i64" => CmScalarType::S64,
        "f32" => CmScalarType::F32,
        "f64" => CmScalarType::F64,
        _ => return None,
    })
}

/// AST-`Type` analogue of [`cm_payload_type_from_type_id`], for codegen (which
/// works off the export's raw Wado return type). Returns `None` for named
/// records / variants / enums and other unsupported shapes.
pub fn cm_payload_type_from_ast(
    ty: &crate::ast::Type,
    registry: &CmInterfaceRegistry,
) -> Option<crate::wir::CmPayloadType> {
    use crate::ast::Type;
    use crate::wir::CmPayloadType;
    let resolved = registry.resolve_type(ty);
    match &resolved {
        Type::Named(n) if n.name == "String" => Some(CmPayloadType::String),
        Type::Named(n) => {
            if let Some(scalar) = cm_scalar_from_ast_name(&n.name) {
                return Some(CmPayloadType::Scalar(scalar));
            }
            // A user/dependency record: a registered struct whose source is not
            // a CM-owned (`wasi:*` / `core:kiln/*`) interface. Mirrors the
            // `Named` arm of `cm_payload_type_from_type_id`.
            // Mirror of `is_cm_owned_source` on the resolved source string.
            let src = registry.resolve_cm_source_for(n, None)?;
            if src.starts_with("wasi:") || src.starts_with("core:kiln/") {
                return None;
            }
            let cm = registry.get_struct_cm_name_by_source(src, &n.name)?;
            Some(CmPayloadType::Named(cm.to_string()))
        }
        Type::Tuple(elems) => elems
            .iter()
            .map(|e| cm_payload_type_from_ast(e, registry))
            .collect::<Option<Vec<_>>>()
            .map(CmPayloadType::Tuple),
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => Some(CmPayloadType::Option(Box::new(
                cm_payload_type_from_ast(&g.args[0], registry)?,
            ))),
            "List" if g.args.len() == 1 => Some(CmPayloadType::List(Box::new(
                cm_payload_type_from_ast(&g.args[0], registry)?,
            ))),
            "Result" if g.args.len() == 2 => {
                let arm = |t: &Type| -> Option<Option<Box<CmPayloadType>>> {
                    if is_unit_type(t) {
                        Some(None)
                    } else {
                        Some(Some(Box::new(cm_payload_type_from_ast(t, registry)?)))
                    }
                };
                Some(CmPayloadType::Result(arm(&g.args[0])?, arm(&g.args[1])?))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Classify an AST-`Type` stream element (codegen's `task.return` resolver),
/// mirroring [`classify_stream_payload`] on resolved types.
pub fn classify_stream_payload_from_ast(
    ty: &crate::ast::Type,
    registry: &CmInterfaceRegistry,
) -> crate::wir::CmStreamPayload {
    use crate::ast::Type;
    use crate::wir::CmStreamPayload;
    let resolved = registry.resolve_type(ty);
    if let Type::Named(n) = &resolved
        && n.name == "u8"
    {
        return CmStreamPayload::U8;
    }
    match cm_payload_type_from_ast(ty, registry) {
        Some(payload) => CmStreamPayload::Value(payload),
        None => CmStreamPayload::U8,
    }
}

/// Classify an AST-`Type` future payload (codegen's `task.return` resolver),
/// mirroring [`classify_future_payload`] on resolved types.
pub fn classify_future_payload_from_ast(
    ty: &crate::ast::Type,
    registry: &CmInterfaceRegistry,
) -> CmFuturePayload {
    use crate::ast::Type;
    let resolved = registry.resolve_type(ty);
    if let Type::Named(n) = &resolved
        && let Some(scalar) = cm_scalar_from_ast_name(&n.name)
    {
        return CmFuturePayload::Scalar(scalar);
    }
    if let Some(payload) = cm_payload_type_from_ast(ty, registry) {
        return CmFuturePayload::Value(payload);
    }
    CmFuturePayload::Trailers
}

/// Map a primitive type to its CM scalar type, or `None` for non-CM-scalars.
pub fn primitive_to_cm_scalar(prim: &PrimitiveType) -> Option<CmScalarType> {
    Some(match prim {
        PrimitiveType::I8 => CmScalarType::S8,
        PrimitiveType::I16 => CmScalarType::S16,
        PrimitiveType::I32 => CmScalarType::S32,
        PrimitiveType::I64 => CmScalarType::S64,
        PrimitiveType::U8 => CmScalarType::U8,
        PrimitiveType::U16 => CmScalarType::U16,
        PrimitiveType::U32 => CmScalarType::U32,
        PrimitiveType::U64 => CmScalarType::U64,
        PrimitiveType::F32 => CmScalarType::F32,
        PrimitiveType::F64 => CmScalarType::F64,
        PrimitiveType::Bool => CmScalarType::Bool,
        PrimitiveType::Char => CmScalarType::Char,
        _ => return None,
    })
}

/// WASI package owning an `ErrorCode` type (e.g. `"http/types.wado"` → `"http"`).
/// Each package's error-code is a distinct CM type, so the transmission future
/// is parameterized by it. Falls back to `"cli"` for a non-WASI error type.
pub fn error_code_source(type_table: &TypeTable, error_type_id: TypeId) -> String {
    let module_source = match type_table.get(error_type_id) {
        ResolvedType::Enum { module_source, .. } | ResolvedType::Variant { module_source, .. } => {
            module_source
        }
        _ => return "cli".to_string(),
    };
    if let ModuleSource::Wasi { interface } = module_source {
        interface.split('/').next().unwrap_or("cli").to_string()
    } else {
        "cli".to_string()
    }
}

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
        .find_map(|a| match &a.cm_boundary {
            // Type-level CM imports use the function fragment (after `#`)
            // as the CM name. If the path has no function fragment, fall
            // back to the whole interface path to preserve the previous
            // behaviour.
            Some(crate::ast::CmBoundary::Import(cm)) => {
                Some(cm.function.clone().unwrap_or_else(|| cm.interface_path()))
            }
            // Case-level CM names (variant cases, fields, ...) carry just
            // the CM-side identifier.
            Some(crate::ast::CmBoundary::Name(s)) => Some(s.clone()),
            Some(crate::ast::CmBoundary::Canonical { .. }) | None => None,
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
pub struct CmFunctionInfo {
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

impl CmFunctionInfo {
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
    pub fn needs_memory_with_registry(&self, registry: &CmInterfaceRegistry) -> bool {
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
    fn named_type_payload_requires_memory(ty: &Type, registry: &CmInterfaceRegistry) -> bool {
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
            Type::Generic(g) => matches!(g.name.as_str(), "Stream" | "List"),
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
                "List" | "Option" | "Result" | "Stream" | "Future"
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
pub struct CmInterfaceInfo {
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
    pub functions: Vec<CmFunctionInfo>,
    /// Resource type exported by this interface (if any).
    /// Format: (Wado name, CM kebab-case name)
    /// e.g., ("`TerminalInput`", "terminal-input")
    pub resource_type: Option<(String, String)>,
}

impl CmInterfaceInfo {
    /// The codegen component-instance key `"{package}-{interface}"` (e.g.
    /// `"http-types"`). The single source of truth for this format: a bare
    /// interface name would collide across packages (`wasi:cli/types` vs
    /// `wasi:http/types`), so every register/lookup pair builds the key here.
    #[must_use]
    pub fn instance_key(&self) -> String {
        format!("{}-{}", self.package, self.interface)
    }
}

/// Registry of WASI imports for code generation
///
/// Collects information from effect definitions and provides:
/// - Resolution of effect calls (e.g., "`Stdout::write_via_stream`") to local names
/// - Iteration over interfaces for Component Model import generation
#[derive(Debug, Clone, Default)]
pub struct CmInterfaceRegistry {
    /// `Effect::method` -> function info
    effect_to_func: IndexMap<String, CmFunctionInfo>,

    /// Interface path -> list of functions
    /// Using `BTreeMap` for deterministic ordering
    interfaces: BTreeMap<String, Vec<CmFunctionInfo>>,

    /// Local alias -> (`interface_path`, `wasi_func_name`)
    /// Key format: `wasi:{package}/{interface_name}::{method_name`}
    /// e.g., "`wasi:cli/Stdout::write_via_stream`"
    local_aliases: IndexMap<String, (String, String)>,

    /// Track which WASI function names are used to detect collisions
    used_names: BTreeSet<String>,

    /// Newtypes collected from WASI modules (e.g., Instant -> u64).
    /// Key: `(source_interface, wado_name)` where `source_interface` is the
    /// `#[cm("...")]` fragment before the `#` (e.g.
    /// `"wasi:clocks/types@0.3.0"`). Keying by interface makes
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

    /// CM interface FQ -> `ModuleSource` for non-stdlib namespaces (`--lib`
    /// locals and component imports). WASI/core interfaces are absent; their
    /// source is derived from the FQ by `module_source_for_cm_interface`.
    cm_interface_module_sources: IndexMap<String, ModuleSource>,

    /// FQs of interfaces imported from a CM component dependency. The plan
    /// classifies these as [`crate::wir::ImportKind::Component`] for composition.
    component_interfaces: IndexSet<String>,
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
        "CmInterfaceRegistry: duplicate {kind} registration for `{}` in interface `{}`. \
         Each (interface, name) pair must be registered exactly once.",
        key.1,
        key.0,
    );
    map.insert(key, value);
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
/// Resolve a `Type::Named` reference through the newtype-alias map by the
/// reference's declaring interface (`source_interface`). A reference without a
/// source, or one that names no newtype, is returned unchanged. Other type
/// shapes are returned as-is.
fn resolve_type(ty: &Type, aliases: &IndexMap<(String, String), Type>) -> Type {
    match ty {
        Type::Named(named) => named
            .source_interface
            .as_deref()
            .and_then(|source| aliases.get(&(source.to_string(), named.name.clone())))
            .cloned()
            .unwrap_or_else(|| ty.clone()),
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
            // `"wasi:http/handler@0.3.0"`). Skip anonymous
            // interfaces without a CM attribute — they are not boundary-
            // visible and cannot back a world export.
            let Some(cm_fq) = iface.attrs.iter().find_map(Attribute::cm_identifier) else {
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
/// `"wasi:http/types@0.3.0"`). Effects, structs, variants,
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
        let source = CmInterfaceRegistry::cm_source_interface(attrs);
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

impl CmInterfaceRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the canonical source interface that owns `(kind, name)` — i.e.
    /// the `#[cm("...")]` fragment before the `#` (e.g.
    /// `"wasi:filesystem/types@0.3.0"`). `kind` is one of
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
    /// e.g. `"wasi:cli/stdout@0.3.0"`) from the
    /// first `#[cm("...")]` attribute on a stdlib item. Returns an
    /// empty string if no attribute is present (user-authored items
    /// don't carry this).
    fn cm_source_interface(attrs: &[crate::ast::Attribute]) -> String {
        attrs
            .iter()
            .find_map(|a| a.as_cm_import().map(crate::ast::CmImport::interface_path))
            .unwrap_or_default()
    }

    /// Build the registry from the embedded stdlib
    ///
    /// Parses the embedded wasi:* modules and registers their interface methods.
    /// Also collects newtypes and world definitions.
    /// The stdlib registries, shared via `Arc`. Built once (the parse + register
    /// bootstrap is expensive) and handed out as cheap `Arc` clones. A `--lib`
    /// compile augments its own clone with the package's local types via
    /// `Arc::make_mut`, so the shared stdlib copy is never mutated.
    pub fn build_from_stdlib() -> (
        std::sync::Arc<Self>,
        std::sync::Arc<crate::world_registry::WorldRegistry>,
    ) {
        use std::sync::{Arc, OnceLock};

        static INSTANCE: OnceLock<(
            Arc<CmInterfaceRegistry>,
            Arc<crate::world_registry::WorldRegistry>,
        )> = OnceLock::new();

        INSTANCE
            .get_or_init(|| {
                let (cm, world) = Self::build_from_stdlib_inner();
                (Arc::new(cm), Arc::new(world))
            })
            .clone()
    }

    fn build_from_stdlib_inner() -> (Self, crate::world_registry::WorldRegistry) {
        use crate::ast::Module as AstModule;
        use crate::lexer::lex;
        use crate::parser::Parser;
        use crate::stdlib;
        use crate::world_registry::WorldRegistry;

        fn parse_module(source: &str) -> AstModule {
            let r = lex(source);
            assert!(r.errors.is_empty(), "lexer error in stdlib: {:?}", r.errors);
            let mut parser = Parser::new(r.tokens);
            parser.parse_strict().expect("parser error in stdlib")
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
                    if let Some(wasi) = method.attrs.first().and_then(|a| a.as_cm_import()) {
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
                        // The elaborator will handle Mark -> newtype mapping.
                        //
                        // For `async fn foo(...) -> AsyncCall<T>` CM imports,
                        // strip the `AsyncCall<T>` wrapper at registration so
                        // the stored return type is the CM-ABI `T`. The
                        // elaborator re-wraps it as `AsyncCall<T>` when
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
                    if let Some(wasi) = method.attrs.first().and_then(|a| a.as_cm_import()) {
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

    /// Register a component dependency's binding module via the stdlib's
    /// [`Self::register_module_decls`] path, recording each interface FQ as a
    /// component import for [`crate::wir::ImportKind::Component`] classification.
    pub fn register_component_decls(
        &mut self,
        module: &crate::ast::Module,
        interface_fqs: &[String],
        module_source: &ModuleSource,
    ) {
        self.register_module_decls(module);
        for fq in interface_fqs {
            self.component_interfaces.insert(fq.clone());
            self.cm_interface_module_sources
                .insert(fq.clone(), module_source.clone());
        }
    }

    /// Whether `fq` names an interface imported from a CM component dependency.
    #[must_use]
    pub fn is_component_interface(&self, fq: &str) -> bool {
        self.component_interfaces.contains(fq)
    }

    /// Register a `--lib` entry module's own named types under the synthesized
    /// default-interface FQ.
    ///
    /// Unlike [`Self::register_module_decls`], library types carry no
    /// `#[cm(...)]` attribute — they are the package's own declarations, not WIT
    /// bindings — so CM names are derived by kebab-casing the Wado identifiers
    /// and the source interface is the library's default-interface FQ. After
    /// this runs, both the export type plan (`resolve_cm_export_type`) and the
    /// CM type emitter (`ast_type_to_cm`) resolve these types through the
    /// registry exactly like WASI types, with no library-specific path.
    pub fn register_lib_local_decls(
        &mut self,
        module: &crate::ast::Module,
        iface_fq: &str,
        entry_source: ModuleSource,
    ) {
        use crate::ast::Item;

        // Record the FQ -> entry ModuleSource so consumers resolve lib-local
        // types' module source (and detect lib-local provenance) without
        // prefix-sniffing the FQ.
        self.cm_interface_module_sources
            .insert(iface_fq.to_string(), entry_source);

        // Every locally-declared type resolves to this library's default
        // interface FQ. Stdlib modules get this from `populate_named_type_sources`
        // before registration; the `--lib` path must do the same so a stored
        // field / payload / newtype-base reference carries an exact
        // `source_interface` (a bare-name reference cannot be resolved later).
        let local_names: IndexMap<String, String> = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Newtype(a) => Some(a.name.clone()),
                Item::Struct(s) => Some(s.name.clone()),
                Item::Flags(f) => Some(f.name.clone()),
                Item::Enum(e) => Some(e.name.clone()),
                Item::Variant(v) => Some(v.name.clone()),
                Item::Resource(r) => Some(r.name.clone()),
                _ => None,
            })
            .map(|name| (name, iface_fq.to_string()))
            .collect();
        let with_sources = |ty: &Type| {
            let mut ty = ty.clone();
            walk_type(&mut ty, &local_names);
            ty
        };

        for item in &module.items {
            match item {
                Item::Newtype(alias) => register_unique(
                    &mut self.newtypes,
                    "newtype",
                    iface_fq.to_string(),
                    alias.name.clone(),
                    with_sources(&alias.ty),
                ),
                Item::Struct(struct_def) => {
                    let fields: Vec<(String, Type)> = struct_def
                        .fields
                        .iter()
                        .map(|f| (to_kebab(&f.name), with_sources(&f.ty)))
                        .collect();
                    let wado_fields: Vec<(String, String, Type)> = struct_def
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), to_kebab(&f.name), with_sources(&f.ty)))
                        .collect();
                    register_unique(
                        &mut self.structs,
                        "struct",
                        iface_fq.to_string(),
                        struct_def.name.clone(),
                        (to_kebab(&struct_def.name), fields, wado_fields),
                    );
                }
                Item::Flags(flags_def) => {
                    let members: Vec<String> =
                        flags_def.flags.iter().map(|m| to_kebab(&m.name)).collect();
                    register_unique(
                        &mut self.flags,
                        "flags",
                        iface_fq.to_string(),
                        flags_def.name.clone(),
                        (to_kebab(&flags_def.name), members),
                    );
                }
                Item::Enum(enum_def) => {
                    let variants: Vec<String> =
                        enum_def.cases.iter().map(|c| to_kebab(&c.name)).collect();
                    register_unique(
                        &mut self.enums,
                        "enum",
                        iface_fq.to_string(),
                        enum_def.name.clone(),
                        (to_kebab(&enum_def.name), variants),
                    );
                }
                Item::Variant(variant_def) => {
                    let cases: Vec<CmVariantCase> = variant_def
                        .cases
                        .iter()
                        .map(|c| CmVariantCase {
                            cm_name: to_kebab(&c.name),
                            wado_name: c.name.clone(),
                            payload: c.payload.as_ref().map(&with_sources),
                        })
                        .collect();
                    register_unique(
                        &mut self.variants,
                        "variant",
                        iface_fq.to_string(),
                        variant_def.name.clone(),
                        (to_kebab(&variant_def.name), cases),
                    );
                }
                _ => {}
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

    /// Reverse-lookup a struct's Wado name from its CM (kebab) name, when
    /// unambiguous across interfaces. Used by codegen to reconstruct the
    /// declaring `Type::Named` for a `CmPayloadType::Named(<cm-name>)` payload.
    pub fn find_struct_wado_name_by_cm(&self, cm_name: &str) -> Option<&str> {
        let mut hit = None;
        for ((_, wado_name), (cm, _, _)) in &self.structs {
            if cm == cm_name {
                if hit.is_some() {
                    return None;
                }
                hit = Some(wado_name.as_str());
            }
        }
        hit
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
    /// `OutputFile`) can be resolved without leaking into the bare-name WASI
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
    pub fn find_source_under_prefix(&self, prefix: &str, name: &str) -> Option<&str> {
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

    /// The `ModuleSource` a CM interface FQ was registered under — the entry
    /// module for a `--lib` local, or the dependency `ModuleSource::Wasm` for a
    /// component import. `None` for a WASI/core interface (whose source is
    /// derived from the FQ by naming convention) or an unknown FQ.
    pub fn cm_interface_module_source_of(&self, iface_fq: &str) -> Option<&ModuleSource> {
        self.cm_interface_module_sources.get(iface_fq)
    }

    /// Resolve a named type to its source interface across all CM namespaces
    /// (`wasi:*` and `core:kiln/*`). The Kiln-specific lookup is only
    /// consulted when the WASI lookup misses, preserving the strict
    /// cross-namespace scoping the rest of the binding pipeline relies on.
    ///
    /// Used by the flat-param lift path when the binding is for a
    /// `core:kiln/generator` world export and the parameter happens to be a
    /// `core:kiln/types` record such as `OutputFile`.
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
            // Lib-local types (`wado compile --lib`) are registered by name
            // under the package's default-interface FQ (neither `wasi:` nor
            // `core:`), so the prefix-scoped lookups above miss them. A bare
            // by-name scan resolves them when the name is unambiguous.
            .or_else(|| find_unique_source_in(&self.structs, &named.name))
            .or_else(|| find_unique_source_in(&self.variants, &named.name))
            .or_else(|| find_unique_source_in(&self.enums, &named.name))
            .or_else(|| find_unique_source_in(&self.flags, &named.name))
    }

    /// Resolve a named type to a source interface within `namespace_prefix`
    /// (e.g. `"core:kiln/"`, `"wasi:http/"`). Used to scope a world export's
    /// type to the world's own package, so a name that also exists in another
    /// package (e.g. `Response` in both `wasi:http` and `core:kiln`) resolves
    /// to the world's own. Returns `None` for an empty prefix or no unique match.
    pub fn resolve_cm_source_with_prefix<'a>(
        &'a self,
        named: &crate::ast::NamedType,
        namespace_prefix: &str,
    ) -> Option<&'a str> {
        if namespace_prefix.is_empty() {
            return None;
        }
        find_unique_source_with_prefix(&self.newtypes, namespace_prefix, &named.name)
            .or_else(|| {
                find_unique_source_with_prefix(&self.resources, namespace_prefix, &named.name)
            })
            .or_else(|| {
                find_unique_source_with_prefix(&self.structs, namespace_prefix, &named.name)
            })
            .or_else(|| {
                find_unique_source_with_prefix(&self.variants, namespace_prefix, &named.name)
            })
            .or_else(|| find_unique_source_with_prefix(&self.enums, namespace_prefix, &named.name))
            .or_else(|| find_unique_source_with_prefix(&self.flags, namespace_prefix, &named.name))
    }

    // -- Legacy unscoped lookups -------------------------------------------

    /// Resolve a newtype *reference* to its aliased base type by its declaring
    /// interface (`NamedType::source_interface`), so two modules that declare a
    /// same-named newtype resolve to their own base — never a bare-name guess.
    /// A reference that reaches CM emission carries a resolved source; one
    /// without a source is not a known newtype here.
    fn resolve_newtype_ref(&self, named: &crate::ast::NamedType) -> Option<&Type> {
        let source = named.source_interface.as_deref()?;
        self.get_newtype_by_source(source, &named.name)
    }

    /// The base type of a *local* newtype `name` — one declared in the compiled
    /// package rather than imported from a CM interface (`wasi:*`, kiln, or a
    /// registered component interface). Returns `None` for CM-imported newtypes
    /// and non-newtype names. The CM codegen emits a local newtype as a named
    /// type alias at the boundary (issue #1456) instead of erasing it to its
    /// base, so the compiled component's structural type matches `wado wit`.
    ///
    /// Returns the newtype's registration source and its base type. `source`
    /// is the reference's declaring interface (`NamedType::source_interface`);
    /// it keys the lookup exactly, so two modules that each declare a same-named
    /// local newtype over different base types never collide (the `(name,
    /// source)` discipline the rest of the registry uses). A reference without a
    /// source, or one naming a CM-imported newtype, is not a *local* newtype
    /// here. The returned source is echoed back so callers key the emitted alias
    /// by it and never double-emit the same boundary type.
    pub fn local_newtype_base(&self, source: Option<&str>, name: &str) -> Option<(&str, &Type)> {
        let source = source?;
        if self.is_cm_source(source) {
            return None;
        }
        self.newtypes
            .get_key_value(&(source.to_string(), name.to_string()))
            .map(|((s, _), ty)| (s.as_str(), ty))
    }

    /// Iterate all newtypes as `((source_interface, name), type)`.
    pub fn newtypes(&self) -> &IndexMap<(String, String), Type> {
        &self.newtypes
    }

    /// Check if a type name is a registered resource (in any interface).
    pub fn is_resource(&self, name: &str) -> bool {
        has_any_in(&self.resources, name)
    }

    /// Get the source interface path for a resource, when unambiguous.
    /// e.g., `TerminalInput` -> `"wasi:cli/terminal-input@0.3.0-rc-2026-01-06"`.
    pub fn get_resource_source_interface(&self, name: &str) -> Option<&str> {
        find_unique_source_in(&self.resources, name)
    }

    /// Source interface of the resource whose CM (kebab) name is `cm_name`.
    pub fn resource_source_by_cm_name(&self, cm_name: &str) -> Option<&str> {
        self.resources
            .iter()
            .find(|((_, _), cm)| cm.as_str() == cm_name)
            .map(|((src, _), _)| src.as_str())
    }

    /// Check if a type name is a registered enum (in any interface).
    pub fn is_enum(&self, name: &str) -> bool {
        has_any_in(&self.enums, name)
    }

    /// Get the CM enum variant names scoped to a specific source interface
    /// (e.g. `"wasi:cli/types@0.3.0"`). Disambiguates enums
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

    /// Check if a type name is a registered variant (in any interface).
    pub fn is_variant(&self, name: &str) -> bool {
        has_any_in(&self.variants, name)
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

    /// Whether a struct named `name` is registered under an interface whose CM
    /// source is exactly `source`. Unlike [`Self::get_struct_fields`], this keys
    /// on the struct's own module source, so a user record is never confused
    /// with a same-named WASI/dependency struct (which lives under a different
    /// source). A `--lib` entry record is registered under the package default
    /// interface, whose source [`register_lib_local_decls`] maps to the entry
    /// module; outside `--lib` the user record is registered nowhere, so this is
    /// `false` and the payload has no CM type to lower against.
    pub fn is_struct_registered_from(&self, source: &ModuleSource, name: &str) -> bool {
        self.structs.keys().any(|(fq, struct_name)| {
            struct_name == name && self.cm_interface_module_sources.get(fq) == Some(source)
        })
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
    ///
    /// The returned key is the package-qualified component-instance key
    /// `"{package}-{interface}"` (e.g. `"http-types"`), matching how codegen
    /// registers imported interface instances. A bare interface name would
    /// collide across packages (`wasi:cli/types` vs `wasi:http/types`, both
    /// interface `types`).
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
            let key = format!("{}-{}", wasi.package, wasi.interface);
            if found.as_ref().is_some_and(|prev| prev != &key) {
                return None; // ambiguous across interfaces — refuse to guess.
            }
            found = Some(key);
        }
        found
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

    /// Iterate over enums from a specific interface (matched by prefix).
    /// Returns (`wado_name`, `cm_name`, `cm_variant_names`) in insertion order.
    pub fn enums_for_interface(
        &self,
        interface_prefix: &str,
    ) -> impl Iterator<Item = (&str, &str, &[String])> {
        self.enums
            .iter()
            .filter_map(move |((source, name), (cm_name, variants))| {
                if source.starts_with(interface_prefix) {
                    Some((name.as_str(), cm_name.as_str(), variants.as_slice()))
                } else {
                    None
                }
            })
    }

    /// Iterate over flags from a specific interface (matched by prefix).
    /// Returns (`wado_name`, `cm_name`, `cm_member_names`) in insertion order.
    pub fn flags_for_interface(
        &self,
        interface_prefix: &str,
    ) -> impl Iterator<Item = (&str, &str, &[String])> {
        self.flags
            .iter()
            .filter_map(move |((source, name), (cm_name, members))| {
                if source.starts_with(interface_prefix) {
                    Some((name.as_str(), cm_name.as_str(), members.as_slice()))
                } else {
                    None
                }
            })
    }

    /// Iterate over newtypes from a specific interface (matched by prefix).
    /// Returns (`wado_name`, `base_type`) in insertion order.
    pub fn newtypes_for_interface(
        &self,
        interface_prefix: &str,
    ) -> impl Iterator<Item = (&str, &Type)> {
        self.newtypes
            .iter()
            .filter_map(move |((source, name), ty)| {
                if source.starts_with(interface_prefix) {
                    Some((name.as_str(), ty))
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
            && let Some(source) = inner.source_interface.as_deref()
            && let Some(cm_name) = self.get_resource_cm_name_by_source(source, &inner.name)
        {
            return Some((inner.name.clone(), cm_name.to_string()));
        }

        None
    }

    /// Check if a WASI function is supported for Component Model generation.
    ///
    /// This uses the registry's known enums and resources to determine if
    /// all types in the function signature are supported.
    pub fn is_function_supported(&self, func: &CmFunctionInfo) -> bool {
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
        let func_info = CmFunctionInfo {
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
    pub fn get_function(&self, name: &str) -> Option<&CmFunctionInfo> {
        self.effect_to_func.get(name)
    }

    /// Get all interfaces that need to be imported
    ///
    /// Returns interfaces in deterministic order (sorted by path)
    pub fn interfaces(&self) -> impl Iterator<Item = CmInterfaceInfo> + '_ {
        self.interfaces.iter().map(|(path, functions)| {
            // Parse the interface path to extract components
            let wasi = CmImport::parse(path);

            // Check if any function returns a resource type (Option<ResourceName>)
            let resource_type = functions
                .iter()
                .find_map(|func| self.get_resource_from_return_type(&func.return_type));

            CmInterfaceInfo {
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

    /// Whether `source` names an interface whose values follow the CM canonical
    /// ABI: a stdlib CM namespace (`wasi:`, `core:kiln/`) or a component import
    /// (whose package namespace is arbitrary, so tracked explicitly).
    pub fn is_cm_source(&self, source: &str) -> bool {
        source.starts_with("wasi:")
            || source.starts_with("core:kiln/")
            || self.component_interfaces.contains(source)
    }

    /// Flatten a CM type into its canonical-ABI core value sequence. Single
    /// source of truth for how a value-type maps to flat core params/results.
    pub fn cm_flatten(&self, ty: &Type) -> Vec<crate::cm_abi::CmValType> {
        let mut out = Vec::new();
        self.cm_flatten_into(ty, &mut out);
        out
    }

    fn cm_flatten_into(&self, ty: &Type, out: &mut Vec<crate::cm_abi::CmValType>) {
        use crate::cm_abi::CmValType;
        let resolved = self.resolve_type(ty);
        match &resolved {
            Type::Named(named) => match named.name.as_str() {
                "bool" | "u8" | "i8" | "u16" | "i16" | "i32" | "u32" | "char" => {
                    out.push(CmValType::I32);
                }
                "i64" | "u64" => out.push(CmValType::I64),
                "f32" => out.push(CmValType::F32),
                "f64" => out.push(CmValType::F64),
                "String" => {
                    out.push(CmValType::I32);
                    out.push(CmValType::I32);
                }
                "()" => {}
                name => {
                    if let Some(source) = self.resolve_cm_source_for(named, None) {
                        if let Some(fields) = self
                            .get_struct_fields_by_source(source, name)
                            .map(<[(String, Type)]>::to_vec)
                        {
                            for (_, field_ty) in &fields {
                                self.cm_flatten_into(field_ty, out);
                            }
                            return;
                        }
                        if let Some(cases) = self
                            .get_variant_cases_by_source(source, name)
                            .map(<[CmVariantCase]>::to_vec)
                        {
                            out.push(CmValType::I32);
                            self.push_joined_payloads(cases.iter().map(|c| c.payload.clone()), out);
                            return;
                        }
                    }
                    out.push(CmValType::I32);
                }
            },
            Type::Generic(g) => match g.name.as_str() {
                "List" => {
                    out.push(CmValType::I32);
                    out.push(CmValType::I32);
                }
                "Stream" | "Future" | "Own" | "Borrow" => out.push(CmValType::I32),
                "Option" if g.args.len() == 1 => {
                    out.push(CmValType::I32);
                    self.cm_flatten_into(&g.args[0], out);
                }
                "Result" if g.args.len() == 2 => {
                    out.push(CmValType::I32);
                    self.push_joined_payloads(
                        [Some(g.args[0].clone()), Some(g.args[1].clone())].into_iter(),
                        out,
                    );
                }
                other => panic!("unsupported generic type for CM flattening: {other}"),
            },
            Type::Reference(_) | Type::MutReference(_) => out.push(CmValType::I32),
            Type::Tuple(elems) => {
                for elem in elems {
                    self.cm_flatten_into(elem, out);
                }
            }
            other => panic!("unsupported type for CM flattening: {other:?}"),
        }
    }

    /// Push the per-slot Canonical ABI `join` of optional payload types (variant
    /// cases or a result's ok/err); a `None` payload contributes no slots.
    fn push_joined_payloads(
        &self,
        payloads: impl Iterator<Item = Option<Type>>,
        out: &mut Vec<crate::cm_abi::CmValType>,
    ) {
        use crate::cm_abi::CmValType;
        let groups: Vec<Vec<CmValType>> = payloads
            .map(|p| p.map(|t| self.cm_flatten(&t)).unwrap_or_default())
            .collect();
        let max_len = groups.iter().map(Vec::len).max().unwrap_or(0);
        for i in 0..max_len {
            let mut acc: Option<CmValType> = None;
            for g in &groups {
                if let Some(v) = g.get(i).copied() {
                    acc = Some(CmValType::join(acc, Some(v)));
                }
            }
            out.push(acc.unwrap_or(CmValType::I32));
        }
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
        self.resolve_type_impl(ty, false)
    }

    /// Like [`Self::resolve_type`], but a *local* newtype (no `#[cm(...)]`
    /// source) is kept as its named reference instead of peeled to its base.
    /// The CM codegen then emits it as a named type alias (`type meters = f64`)
    /// so the compiled component's structural type matches `wado wit`
    /// (issue #1456). WASI/CM-imported newtypes still resolve through.
    pub fn resolve_type_preserving_local_newtypes(&self, ty: &Type) -> Type {
        self.resolve_type_impl(ty, true)
    }

    fn resolve_type_impl(&self, ty: &Type, preserve_local: bool) -> Type {
        match ty {
            Type::Named(named) => {
                if preserve_local
                    && self
                        .local_newtype_base(named.source_interface.as_deref(), &named.name)
                        .is_some()
                {
                    ty.clone()
                } else if let Some(aliased_ty) = self.resolve_newtype_ref(named) {
                    // Recursively resolve the aliased type
                    self.resolve_type_impl(aliased_ty, preserve_local)
                } else {
                    ty.clone()
                }
            }
            Type::Generic(generic) => {
                // Resolve type arguments recursively
                let resolved_args: Vec<Type> = generic
                    .args
                    .iter()
                    .map(|arg| self.resolve_type_impl(arg, preserve_local))
                    .collect();
                Type::Generic(GenericType {
                    id: generic.id,
                    name: generic.name.clone(),
                    args: resolved_args,
                    span: generic.span,
                })
            }
            Type::Tuple(types) => {
                let resolved: Vec<Type> = types
                    .iter()
                    .map(|t| self.resolve_type_impl(t, preserve_local))
                    .collect();
                Type::Tuple(resolved)
            }
            Type::Reference(inner) => {
                Type::Reference(Box::new(self.resolve_type_impl(inner, preserve_local)))
            }
            Type::MutReference(inner) => {
                Type::MutReference(Box::new(self.resolve_type_impl(inner, preserve_local)))
            }
            Type::Function(func_ty) => {
                // For function types, resolve params and return type
                let resolved_params: Vec<Type> = func_ty
                    .params
                    .iter()
                    .map(|t| self.resolve_type_impl(t, preserve_local))
                    .collect();
                let resolved_return = self.resolve_type_impl(&func_ty.return_type, preserve_local);
                Type::Function(Box::new(crate::ast::FunctionType {
                    is_mut: func_ty.is_mut,
                    params: resolved_params,
                    return_type: resolved_return,
                    effects: func_ty.effects.clone(),
                    effect_ids: func_ty.effect_ids.clone(),
                    stores: func_ty.stores.clone(),
                }))
            }
            // NamespacedGeneric types (like `ns::Type<T>`) are passed through
            Type::NamespacedGeneric(ng) => {
                let resolved_args: Vec<Type> = ng
                    .args
                    .iter()
                    .map(|arg| self.resolve_type_impl(arg, preserve_local))
                    .collect();
                Type::NamespacedGeneric(crate::ast::NamespacedGenericType {
                    id: ng.id,
                    namespace: ng.namespace.clone(),
                    name: ng.name.clone(),
                    args: resolved_args,
                    span: ng.span,
                })
            }
            // TypePackSpread is only valid inside tuple types — pass through
            Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => ty.clone(),
        }
    }
}

use wasm_encoder::{
    ComponentTypeRef, ComponentValType, InstanceType, PrimitiveValType, TypeBounds,
};

/// One Component Model defined-type shape, with its inner value types already
/// resolved. The [`CmTypeGen`] engine builds these and hands them to a
/// [`CmTypeSink`], which decides where they land.
pub enum CmDefined<'a> {
    Record(&'a [(&'a str, ComponentValType)]),
    Variant(&'a [(&'a str, Option<ComponentValType>)]),
    Enum(&'a [&'a str]),
    Flags(&'a [&'a str]),
    List(ComponentValType),
    Tuple(&'a [ComponentValType]),
    Option(ComponentValType),
    Result {
        ok: Option<ComponentValType>,
        err: Option<ComponentValType>,
    },
    Own(u32),
    Borrow(u32),
    Future(Option<ComponentValType>),
    Stream(Option<ComponentValType>),
    /// A type alias to a primitive (`type meters = f64`), used to preserve a
    /// local newtype at the CM boundary.
    Primitive(PrimitiveValType),
}

/// Emission target for the CM type engine. Decouples *what* type to build (the
/// engine's registry-driven recursion) from *where* it lands: an interface
/// [`InstanceType`] (WASI imports, exported interface types) via [`InstanceSink`],
/// or the component's top-level type space (bare `--lib` world exports) via the
/// codegen-side `TopLevelSink`. Object-safe so the engine holds `&mut dyn`.
pub trait CmTypeSink {
    /// Emit one defined type and return its index in this sink's type space.
    fn define(&mut self, defined: CmDefined<'_>) -> u32;
    /// Make defined type `idx` a named CM type `cm_name`; return the index to
    /// reference it by afterwards (an instance alias index, or `idx` itself for
    /// the top-level sink).
    fn name(&mut self, cm_name: &str, idx: u32) -> u32;
}

/// [`CmTypeSink`] that emits into an interface [`InstanceType`], sharing the
/// caller's type-index counter so the engine and any direct emissions stay in
/// lockstep. Reproduces the historical `CmInstanceTypeGen` behavior exactly.
pub struct InstanceSink<'a> {
    pub it: &'a mut InstanceType,
    pub next_idx: &'a mut u32,
}

impl InstanceSink<'_> {
    fn alloc(&mut self) -> u32 {
        let idx = *self.next_idx;
        *self.next_idx += 1;
        idx
    }
}

impl CmTypeSink for InstanceSink<'_> {
    fn define(&mut self, defined: CmDefined<'_>) -> u32 {
        emit_cm_defined(self.it.ty().defined_type(), defined);
        self.alloc()
    }

    fn name(&mut self, cm_name: &str, idx: u32) -> u32 {
        self.it
            .export(cm_name, ComponentTypeRef::Type(TypeBounds::Eq(idx)));
        self.alloc()
    }
}

/// Write one [`CmDefined`] shape into a defined-type encoder. Shared by every
/// [`CmTypeSink`] so the shape → wasm-encoder mapping lives in one place.
pub(crate) fn emit_cm_defined(
    enc: wasm_encoder::ComponentDefinedTypeEncoder<'_>,
    defined: CmDefined<'_>,
) {
    match defined {
        CmDefined::Record(fields) => enc.record(fields.iter().copied()),
        CmDefined::Variant(cases) => enc.variant(cases.iter().copied()),
        CmDefined::Enum(names) => enc.enum_type(names.iter().copied()),
        CmDefined::Flags(names) => enc.flags(names.iter().copied()),
        CmDefined::List(elem) => enc.list(elem),
        CmDefined::Tuple(elems) => enc.tuple(elems.iter().copied()),
        CmDefined::Option(inner) => enc.option(inner),
        CmDefined::Result { ok, err } => enc.result(ok, err),
        CmDefined::Own(resource) => enc.own(resource),
        CmDefined::Borrow(resource) => enc.borrow(resource),
        CmDefined::Future(payload) => enc.future(payload),
        CmDefined::Stream(payload) => enc.stream(payload),
        CmDefined::Primitive(prim) => enc.primitive(prim),
    }
}

/// Map a Wado primitive type name to its Component Model [`PrimitiveValType`].
///
/// Single source of truth for the Wado-primitive → CM-primitive table. Both the
/// plan builder (which only needs the recognized set) and codegen (which needs
/// the rendered valtype) delegate here, so the recognized and renderable sets
/// cannot diverge. Returns `None` for any non-primitive name.
pub fn wado_primitive_name_to_cm(name: &str) -> Option<PrimitiveValType> {
    let prim = match name {
        "i8" => PrimitiveValType::S8,
        "i16" => PrimitiveValType::S16,
        "i32" => PrimitiveValType::S32,
        "i64" => PrimitiveValType::S64,
        "u8" => PrimitiveValType::U8,
        "u16" => PrimitiveValType::U16,
        "u32" => PrimitiveValType::U32,
        "u64" => PrimitiveValType::U64,
        "f32" => PrimitiveValType::F32,
        "f64" => PrimitiveValType::F64,
        "bool" => PrimitiveValType::Bool,
        "char" => PrimitiveValType::Char,
        "String" => PrimitiveValType::String,
        _ => return None,
    };
    Some(prim)
}

/// Registry-driven recursive builder for Component Model types.
///
/// Given a Wado [`Type`] and the [`CmInterfaceRegistry`], it walks the type,
/// resolves named types to their CM metadata, and emits the corresponding CM
/// defined types through a [`CmTypeSink`] — an [`InstanceSink`] for interface
/// instance types, or the codegen top-level sink for bare world exports. It
/// owns only the dedup cache and the disambiguation hint; type-index allocation
/// lives in the sink, so one engine serves both targets.
pub struct CmTypeGen {
    cache: IndexMap<String, u32>,
    /// Optional interface path hint for disambiguating types with the same name
    /// across different WASI interfaces (e.g., "wasi:http/types@..." to select
    /// HTTP's `ErrorCode` over filesystem's `ErrorCode`).
    interface_hint: Option<String>,
}

impl Default for CmTypeGen {
    fn default() -> Self {
        Self::new()
    }
}

impl CmTypeGen {
    pub fn new() -> Self {
        Self {
            cache: IndexMap::default(),
            interface_hint: None,
        }
    }

    /// Create a new generator with an interface hint for type disambiguation.
    pub fn with_interface_hint(interface_hint: &str) -> Self {
        Self {
            cache: IndexMap::default(),
            interface_hint: Some(interface_hint.to_string()),
        }
    }

    /// Register a pre-existing type index for cache lookups
    pub fn register_existing(&mut self, key: &str, idx: u32) {
        self.cache.insert(key.to_string(), idx);
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
        sink: &mut dyn CmTypeSink,
        cm_name: &str,
        cases: &[CmVariantCase],
        cm_interface_registry: &CmInterfaceRegistry,
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
                    let resolved = cm_interface_registry.resolve_type_preserving_local_newtypes(ty);
                    self.ast_type_to_cm(sink, &resolved, cm_interface_registry, resource_exports)
                })
            })
            .collect();

        let variant_cases: Vec<(&str, Option<ComponentValType>)> = cases
            .iter()
            .zip(payload_cm_types.iter())
            .map(|(case, payload_cm)| (case.cm_name.as_str(), *payload_cm))
            .collect();
        let variant_idx = sink.define(CmDefined::Variant(&variant_cases));
        // Export to make it "named" (required by CM spec for records/variants)
        let export_idx = sink.name(cm_name, variant_idx);

        self.cache.insert(cache_key, export_idx);
        export_idx
    }

    /// Define a record (struct) type and its named export, returning the exported type index.
    fn define_record(
        &mut self,
        sink: &mut dyn CmTypeSink,
        cm_name: &str,
        fields: &[(String, Type)],
        cm_interface_registry: &CmInterfaceRegistry,
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
                let resolved =
                    cm_interface_registry.resolve_type_preserving_local_newtypes(field_ty);
                let cm_type =
                    self.ast_type_to_cm(sink, &resolved, cm_interface_registry, resource_exports);
                (field_name.clone(), cm_type)
            })
            .collect();

        let field_refs: Vec<(&str, ComponentValType)> = field_cm_types
            .iter()
            .map(|(n, t)| (n.as_str(), *t))
            .collect();
        let record_idx = sink.define(CmDefined::Record(&field_refs));
        // Export the record type (required by CM spec)
        let export_idx = sink.name(cm_name, record_idx);

        self.cache.insert(cache_key, export_idx);
        export_idx
    }

    /// Define an option<T> type, returning the type index.
    fn define_option(
        &mut self,
        sink: &mut dyn CmTypeSink,
        inner: ComponentValType,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("option:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::Option(inner));
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a stream<T> type, returning the type index.
    fn define_stream(
        &mut self,
        sink: &mut dyn CmTypeSink,
        elem: Option<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("stream:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::Stream(elem));
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a future<T> type, returning the type index.
    fn define_future(
        &mut self,
        sink: &mut dyn CmTypeSink,
        inner: Option<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("future:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::Future(inner));
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a borrow type, returning the type index.
    fn define_borrow(
        &mut self,
        sink: &mut dyn CmTypeSink,
        resource_export_idx: u32,
        resource_cm_name: &str,
    ) -> u32 {
        let cache_key = format!("borrow:{resource_cm_name}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::Borrow(resource_export_idx));
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a list type, returning the type index.
    fn define_list(
        &mut self,
        sink: &mut dyn CmTypeSink,
        elem_type: ComponentValType,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("list:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::List(elem_type));
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a tuple type, returning the type index.
    fn define_tuple(
        &mut self,
        sink: &mut dyn CmTypeSink,
        elems: Vec<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("tuple:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::Tuple(&elems));
        self.cache.insert(cache_key, idx);
        idx
    }

    /// Define a result type, returning the type index.
    fn define_result(
        &mut self,
        sink: &mut dyn CmTypeSink,
        ok_type: Option<ComponentValType>,
        err_type: Option<ComponentValType>,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("result:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::Result {
            ok: ok_type,
            err: err_type,
        });
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
        sink: &mut dyn CmTypeSink,
        ty: &Type,
        cm_interface_registry: &CmInterfaceRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> ComponentValType {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "String" => ComponentValType::Primitive(PrimitiveValType::String),
                "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
                "i8" => ComponentValType::Primitive(PrimitiveValType::S8),
                "i16" => ComponentValType::Primitive(PrimitiveValType::S16),
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
                    // A local newtype (declared in the compiled package, not
                    // imported from a CM interface) is preserved as a named CM
                    // alias (`type meters = f64`) so the compiled component's
                    // structural type matches `wado wit`, rather than erasing to
                    // its base (issue #1456).
                    let source = named.source_interface.as_deref();
                    if let Some((canonical_source, base)) = cm_interface_registry
                        .local_newtype_base(source, name)
                        .map(|(s, ty)| (s.to_string(), ty.clone()))
                    {
                        // Key the cache by the newtype's *canonical* source so
                        // same-named local newtypes from different modules stay
                        // distinct, and every reference to one newtype (whether
                        // or not it carries a source) dedups to a single alias.
                        let cache_key = format!("newtype:{canonical_source}:{name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        // Resolve the base through the registry first: a base that
                        // is itself an imported (WASI) newtype peels to its
                        // primitive, while a local-newtype base is kept and
                        // recursed into as a nested alias. Passing the raw base
                        // would reach the unsupported-name panic below for an
                        // imported-newtype base.
                        let base =
                            cm_interface_registry.resolve_type_preserving_local_newtypes(&base);
                        let base_val = self.ast_type_to_cm(
                            sink,
                            &base,
                            cm_interface_registry,
                            resource_exports,
                        );
                        // A primitive base has no defined-type index to alias, so
                        // define one; an aggregate base already has an index we
                        // name directly (`type meters = point`).
                        let base_idx = match base_val {
                            ComponentValType::Primitive(prim) => {
                                sink.define(CmDefined::Primitive(prim))
                            }
                            ComponentValType::Type(idx) => idx,
                        };
                        let named_idx = sink.name(&to_kebab(name), base_idx);
                        self.cache.insert(cache_key, named_idx);
                        return ComponentValType::Type(named_idx);
                    }
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
                            let hit = cm_interface_registry
                                .get_resource_cm_name_by_source(h, name)
                                .is_some()
                                || cm_interface_registry
                                    .get_variant_cases_by_source(h, name)
                                    .is_some()
                                || cm_interface_registry
                                    .get_struct_fields_by_source(h, name)
                                    .is_some()
                                || cm_interface_registry
                                    .get_enum_variants_by_source(h, name)
                                    .is_some()
                                || cm_interface_registry
                                    .get_flags_members_by_source(h, name)
                                    .is_some();
                            hit.then(|| h.to_string())
                        });
                    let source_owned: String = named
                        .source_interface
                        .as_deref()
                        .filter(|s| cm_interface_registry.is_cm_source(s))
                        .map(str::to_string)
                        .or(interface_hint_match)
                        .or_else(|| {
                            cm_interface_registry
                                .find_wasi_resource_source(name)
                                .or_else(|| cm_interface_registry.find_wasi_variant_source(name))
                                .or_else(|| cm_interface_registry.find_wasi_struct_source(name))
                                .or_else(|| cm_interface_registry.find_wasi_enum_source(name))
                                .or_else(|| cm_interface_registry.find_wasi_flags_source(name))
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| {
                            panic!(
                                "unresolved CM named type reference `{name}` while emitting CM instance"
                            )
                        });
                    let source = source_owned.as_str();
                    if let Some(cm_name) =
                        cm_interface_registry.get_resource_cm_name_by_source(source, name)
                    {
                        let cache_key = format!("own:{cm_name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        let export_idx = resource_exports[cm_name];
                        let idx = sink.define(CmDefined::Own(export_idx));
                        self.cache.insert(cache_key, idx);
                        ComponentValType::Type(idx)
                    } else if let Some(cases) = {
                        // Prefer the explicit `interface_hint` for cross-
                        // interface aliases (e.g. re-exported variants), but fall
                        // back to the resolved source when the hinted interface
                        // does not own the variant. A kiln generator's export
                        // interface is hinted, yet its `generate` returns shared
                        // `core:kiln/types` variants (e.g. `Error`) sourced
                        // elsewhere.
                        let cases_opt = self
                            .interface_hint
                            .as_deref()
                            .and_then(|h| {
                                cm_interface_registry.get_variant_cases_by_interface(h, name)
                            })
                            .or_else(|| {
                                cm_interface_registry.get_variant_cases_by_source(source, name)
                            });
                        cases_opt.map(<[CmVariantCase]>::to_vec)
                    } {
                        let cm_name = self
                            .interface_hint
                            .as_deref()
                            .and_then(|h| {
                                cm_interface_registry.get_variant_cm_name_by_interface(h, name)
                            })
                            .or_else(|| {
                                cm_interface_registry.get_variant_cm_name_by_source(source, name)
                            })
                            .expect("variant cm_name present when cases are")
                            .to_string();
                        let idx = self.define_variant(
                            sink,
                            &cm_name,
                            &cases,
                            cm_interface_registry,
                            resource_exports,
                        );
                        ComponentValType::Type(idx)
                    } else if let Some(fields) = cm_interface_registry
                        .get_struct_fields_by_source(source, name)
                        .map(<[(String, Type)]>::to_vec)
                    {
                        let cm_name = cm_interface_registry
                            .get_struct_cm_name_by_source(source, name)
                            .expect("struct cm_name present when fields are")
                            .to_string();
                        let idx = self.define_record(
                            sink,
                            &cm_name,
                            &fields,
                            cm_interface_registry,
                            resource_exports,
                        );
                        ComponentValType::Type(idx)
                    } else if let Some(variants) = cm_interface_registry
                        .get_enum_variants_by_source(source, name)
                        .map(<[String]>::to_vec)
                    {
                        let cache_key = format!("enum:{name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        let variant_refs: Vec<&str> = variants.iter().map(String::as_str).collect();
                        let idx = sink.define(CmDefined::Enum(&variant_refs));
                        self.cache.insert(cache_key.clone(), idx);

                        if let Some(cm_name) =
                            cm_interface_registry.get_enum_cm_name_by_source(source, name)
                        {
                            let export_idx = sink.name(cm_name, idx);
                            self.cache.insert(cache_key, export_idx);
                            return ComponentValType::Type(export_idx);
                        }

                        ComponentValType::Type(idx)
                    } else if let Some(members) = cm_interface_registry
                        .get_flags_members_by_source(source, name)
                        .map(<[String]>::to_vec)
                    {
                        let cache_key = format!("flags:{name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        let member_refs: Vec<&str> = members.iter().map(String::as_str).collect();
                        let idx = sink.define(CmDefined::Flags(&member_refs));
                        self.cache.insert(cache_key.clone(), idx);

                        // Export to make it a named type (as for record/variant/
                        // enum) so an exported interface instance can reference it.
                        if let Some(cm_name) =
                            cm_interface_registry.get_flags_cm_name_by_source(source, name)
                        {
                            let export_idx = sink.name(cm_name, idx);
                            self.cache.insert(cache_key, export_idx);
                            return ComponentValType::Type(export_idx);
                        }

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
                        .or_else(|| cm_interface_registry.find_wasi_resource_source(&n.name))
                    && let Some(cm_name) =
                        cm_interface_registry.get_resource_cm_name_by_source(source, &n.name)
                {
                    let export_idx = resource_exports[cm_name];
                    let idx = self.define_borrow(sink, export_idx, cm_name);
                    return ComponentValType::Type(idx);
                }
                panic!("unsupported reference type for CM instance: {ty:?}")
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "List" => {
                    let elem_cm = self.ast_type_to_cm(
                        sink,
                        &generic.args[0],
                        cm_interface_registry,
                        resource_exports,
                    );
                    let key = Self::type_key(&generic.args[0]);
                    let idx = self.define_list(sink, elem_cm, &key);
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
                            sink,
                            &generic.args[0],
                            cm_interface_registry,
                            resource_exports,
                        ))
                    };
                    let err_type = if is_err_unit {
                        None
                    } else {
                        Some(self.ast_type_to_cm(
                            sink,
                            &generic.args[1],
                            cm_interface_registry,
                            resource_exports,
                        ))
                    };
                    let key = format!(
                        "{},{}",
                        Self::type_key(&generic.args[0]),
                        Self::type_key(&generic.args[1])
                    );
                    let idx = self.define_result(sink, ok_type, err_type, &key);
                    ComponentValType::Type(idx)
                }
                "Option" => {
                    let inner_cm = self.ast_type_to_cm(
                        sink,
                        &generic.args[0],
                        cm_interface_registry,
                        resource_exports,
                    );
                    let key = Self::type_key(&generic.args[0]);
                    let idx = self.define_option(sink, inner_cm, &key);
                    ComponentValType::Type(idx)
                }
                "Stream" => {
                    let (elem, key) = if generic.args.is_empty() {
                        (None, "unit".to_string())
                    } else {
                        let cm = self.ast_type_to_cm(
                            sink,
                            &generic.args[0],
                            cm_interface_registry,
                            resource_exports,
                        );
                        (Some(cm), Self::type_key(&generic.args[0]))
                    };
                    let idx = self.define_stream(sink, elem, &key);
                    ComponentValType::Type(idx)
                }
                "Future" => {
                    let (inner, key) = if generic.args.is_empty() {
                        (None, "unit".to_string())
                    } else {
                        let cm = self.ast_type_to_cm(
                            sink,
                            &generic.args[0],
                            cm_interface_registry,
                            resource_exports,
                        );
                        (Some(cm), Self::type_key(&generic.args[0]))
                    };
                    let idx = self.define_future(sink, inner, &key);
                    ComponentValType::Type(idx)
                }
                "AsyncCall" if generic.args.len() == 1 => {
                    // `AsyncCall<T>` is a Wado-level wrapper for CM async
                    // imports; the CM ABI-level type is the inner `T`.
                    // Transparently unwrap here so downstream code sees the
                    // CM signature.
                    self.ast_type_to_cm(
                        sink,
                        &generic.args[0],
                        cm_interface_registry,
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
                    .map(|e| self.ast_type_to_cm(sink, e, cm_interface_registry, resource_exports))
                    .collect();
                let key = elems
                    .iter()
                    .map(Self::type_key)
                    .collect::<Vec<_>>()
                    .join(",");
                let idx = self.define_tuple(sink, cm_elems, &key);
                ComponentValType::Type(idx)
            }
            _ => panic!("unsupported type for CM instance: {ty:?}"),
        }
    }
}

/// Convert a pre-resolved AST type to Wasm `ValType`
///
/// This is a pure conversion function - newtypes must already be resolved
/// before calling this function. Use `CmInterfaceRegistry::resolve_type()` during
/// registration to ensure types are pre-resolved.
///
/// Note: This returns a SINGLE `ValType`. For compound types that lower to
/// multiple core values (like String → ptr+len), use `flatten_cm_param_type` instead.
pub fn cm_type_to_valtype(ty: &Type) -> ValType {
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
            // List<T> is represented as a GC array reference (handled as i32 in WASI context)
            "List" => ValType::I32,
            // Option<T> is represented as i32 discriminant
            "Option" => ValType::I32,
            other => panic!("unknown generic type in cm_type_to_valtype: {other}"),
        },
        Type::Reference(_) | Type::MutReference(_) => {
            // borrow<resource> or own<resource> - just an i32 handle
            ValType::I32
        }
        Type::Tuple(_) => ValType::I32,
        other => panic!("unsupported type variant in cm_type_to_valtype: {other:?}"),
    }
}
/// Flatten a pre-resolved AST type into CM core-level `ValType`s.
///
/// Adapter over [`CmInterfaceRegistry::cm_flatten`] yielding `wasm_encoder::ValType`s.
pub fn flatten_cm_param_type(ty: &Type, out: &mut Vec<ValType>, registry: &CmInterfaceRegistry) {
    out.extend(
        registry
            .cm_flatten(ty)
            .into_iter()
            .map(cm_val_type_to_val_type),
    );
}

pub fn cm_val_type_to_val_type(v: crate::cm_abi::CmValType) -> ValType {
    match v {
        crate::cm_abi::CmValType::I32 => ValType::I32,
        crate::cm_abi::CmValType::I64 => ValType::I64,
        crate::cm_abi::CmValType::F32 => ValType::F32,
        crate::cm_abi::CmValType::F64 => ValType::F64,
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
                "i8" | "i16"
                    | "i32"
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
                "Stream" | "Result" | "Future" | "Option" | "List"
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
        Type::Tuple(elems) => elems
            .iter()
            .all(|e| is_param_type_supported_with_types(e, enums, resources, structs)),
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
                "i8" | "i16"
                    | "i32"
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
                "List" | "Option" => {
                    // A list/option element is any supported value type (e.g.
                    // `list<list<u8>>`, `list<[field-name, field-value]>`).
                    generic.args.iter().all(|arg| {
                        is_return_type_supported_with_types(arg, enums, resources, structs)
                    })
                }
                _ => false,
            }
        }
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
/// (without enum/resource knowledge - use `CmInterfaceRegistry::is_function_supported` instead)
pub fn is_cm_function_supported(func: &CmFunctionInfo) -> bool {
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

/// Canonical ABI maximum flat results before a return must use an outptr.
pub const MAX_FLAT_RESULTS: usize = 1;

/// Whether a return type must be returned via an outptr rather than as flat
/// core results. Single source of truth shared by the import-binding synthesizer
/// and the core functype builder, so the two never disagree on a signature.
pub fn cm_return_needs_outptr(ty: &Type, registry: &CmInterfaceRegistry) -> bool {
    registry.cm_flatten(ty).len() > MAX_FLAT_RESULTS
        || cm_named_type_return_needs_outptr(ty, registry)
}

/// Whether a named CM type is a record or payload-bearing variant (returned via
/// an outptr). Resolves the source through the registry so WASI, `--lib`, and
/// component-imported types are all recognised.
pub fn cm_named_type_return_needs_outptr(ty: &Type, registry: &CmInterfaceRegistry) -> bool {
    if let Type::Named(named) = ty
        && let Some(source) = registry.resolve_cm_source_for(named, None)
    {
        if let Some(cases) = registry.get_variant_cases_by_source(source, &named.name) {
            return cases.iter().any(|case| case.payload.is_some());
        }
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
pub fn cm_size_with_registry(ty: &Type, registry: &CmInterfaceRegistry) -> u32 {
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
            if let Some(sa) = cm_variant_size_align(named, registry) {
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
        Type::Tuple(elems) if !elems.is_empty() => {
            crate::cm_abi::layout_tuple_with_registry(elems, registry).size
        }
        _ => crate::cm_abi::cm_size(ty),
    }
}

/// Registry-aware CM canonical ABI alignment for a type.
pub fn cm_align_with_registry(ty: &Type, registry: &CmInterfaceRegistry) -> u32 {
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
            if let Some(sa) = cm_variant_size_align(named, registry) {
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
        Type::Tuple(elems) if !elems.is_empty() => {
            crate::cm_abi::layout_tuple_with_registry(elems, registry).align
        }
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
pub fn cm_variant_size_align(
    named: &crate::ast::NamedType,
    registry: &CmInterfaceRegistry,
) -> Option<(u32, u32)> {
    cm_variant_size_align_scoped(named, registry, None)
}

/// Package-scoped variant of `cm_variant_size_align`.
///
/// When the reference's `source_interface` is populated (stdlib bootstrap or a
/// component import) that exact source is used; otherwise the name is resolved
/// through the registry, biased by `wasi_package`.
pub fn cm_variant_size_align_scoped(
    named: &crate::ast::NamedType,
    registry: &CmInterfaceRegistry,
    wasi_package: Option<&str>,
) -> Option<(u32, u32)> {
    let source = registry.resolve_cm_source_for(named, wasi_package)?;
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
    registry: &CmInterfaceRegistry,
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
            if let Some(sa) = cm_variant_size_align_scoped(named, registry, wasi_package) {
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
        Type::Tuple(elems) if !elems.is_empty() => {
            crate::cm_abi::layout_tuple_with_registry_scoped(elems, registry, wasi_package).size
        }
        _ => crate::cm_abi::cm_size(ty),
    }
}

/// Package-scoped CM canonical ABI alignment for a type.
pub fn cm_align_with_registry_scoped(
    ty: &Type,
    registry: &CmInterfaceRegistry,
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
            if let Some(sa) = cm_variant_size_align_scoped(named, registry, wasi_package) {
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
        Type::Tuple(elems) if !elems.is_empty() => {
            crate::cm_abi::layout_tuple_with_registry_scoped(elems, registry, wasi_package).align
        }
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
        let mut registry = CmInterfaceRegistry::new();

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
        let mut registry = CmInterfaceRegistry::new();

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
        let mut registry = CmInterfaceRegistry::new();

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
        let func_info = CmFunctionInfo {
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

        // List<String> should be supported
        let array_string = Type::Generic(GenericType {
            id: crate::ast::AstId::fresh(),
            name: "List".to_string(),
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
            "List<String> should be supported"
        );

        // List<[String, String]> should be supported
        let tuple_ss = Type::Tuple(vec![
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
        ]);
        let array_tuple = Type::Generic(GenericType {
            id: crate::ast::AstId::fresh(),
            name: "List".to_string(),
            args: vec![tuple_ss],
            span: make_span(),
        });
        assert!(
            is_return_type_supported(&array_tuple),
            "List<[String, String]> should be supported"
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
        let (registry, _) = CmInterfaceRegistry::build_from_stdlib();

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
        let (registry, _) = CmInterfaceRegistry::build_from_stdlib();

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

    fn named(name: &str, source: Option<&str>) -> Type {
        Type::Named(crate::ast::NamedType {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            span: make_span(),
            source_interface: source.map(str::to_string),
        })
    }

    #[test]
    fn local_newtype_base_is_keyed_per_source() {
        // Two modules each declare a local newtype `Id` over a *different* base.
        // A bare-name lookup would collide; the (source, name) key must not.
        let mut registry = CmInterfaceRegistry::new();
        registry
            .newtypes
            .insert(("pkg:a/a@1".into(), "Id".into()), named("f64", None));
        registry
            .newtypes
            .insert(("pkg:b/b@1".into(), "Id".into()), named("i32", None));

        assert!(matches!(
            registry.local_newtype_base(Some("pkg:a/a@1"), "Id"),
            Some(("pkg:a/a@1", Type::Named(n))) if n.name == "f64"
        ));
        assert!(matches!(
            registry.local_newtype_base(Some("pkg:b/b@1"), "Id"),
            Some(("pkg:b/b@1", Type::Named(n))) if n.name == "i32"
        ));
        // A source-less reference to an ambiguous name resolves to nothing
        // rather than picking one arbitrarily.
        assert!(registry.local_newtype_base(None, "Id").is_none());

        // A CM-imported (wasi:) newtype is never treated as local.
        registry.newtypes.insert(
            ("wasi:clocks/types@0.3.0".into(), "Temp".into()),
            named("u64", None),
        );
        assert!(
            registry
                .local_newtype_base(Some("wasi:clocks/types@0.3.0"), "Temp")
                .is_none()
        );
        assert!(registry.local_newtype_base(None, "Temp").is_none());
    }

    #[test]
    fn resolve_preserving_keeps_local_but_peels_imported_newtype_base() {
        // A local newtype `Celsius = Temp` whose base `Temp` is an imported
        // (wasi:) newtype `= u64`. Preserving resolution keeps `Celsius` but
        // peels `Temp` to `u64`, so the alias branch never recurses into an
        // unhandled imported-newtype name (the #1456 review's ICE).
        let mut registry = CmInterfaceRegistry::new();
        registry.newtypes.insert(
            ("wasi:clocks/types@0.3.0".into(), "Temp".into()),
            named("u64", None),
        );
        registry.newtypes.insert(
            ("pkg:app/app@1".into(), "Celsius".into()),
            named("Temp", None),
        );

        let celsius = named("Celsius", Some("pkg:app/app@1"));
        assert!(matches!(
            registry.resolve_type_preserving_local_newtypes(&celsius),
            Type::Named(n) if n.name == "Celsius"
        ));

        let temp = named("Temp", Some("wasi:clocks/types@0.3.0"));
        assert!(matches!(
            registry.resolve_type_preserving_local_newtypes(&temp),
            Type::Named(n) if n.name == "u64"
        ));
    }
}
