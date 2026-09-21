//! Component Model support for code generation
//!
//! This module provides:
//! - WASI import registry: collects WASI imports from effect definitions in lib/wasi/*.wado
//! - Component Model ABI: type conversion and support checking for CM codegen

use std::collections::{BTreeMap, BTreeSet};

use crate::hashmap::{IndexMap, IndexSet};

use wasm_encoder::ValType;

use crate::ast;
use crate::ast::{
    AstId, Attribute, CmBoundary, CmImport, FunctionType, GenericType, InterfaceDecl, Item,
    NamedType, NamespacedGenericType, Type, cm_import_of, declares_unrestricted,
};
use crate::attribute::CM_PARAMS;
use crate::canonical::{CmDecl, CmFuturePayload, CmPayloadType, CmScalarType, CmStreamPayload};
use crate::cm_abi::{
    CmValType, cm_discriminant_byte_size, cm_flags_byte_align, cm_flags_byte_size,
    layout_fields_with_registry, layout_option_with_registry, layout_result_with_registry,
    layout_tuple_with_registry, layout_variant_with_registry, plain_size_align,
};
use crate::defs::DefId;
use crate::module_source::{CmNamespace, ModuleSource};
use crate::name::{DeclName, DeclPath, to_kebab};
use crate::synthesis::cm_binding::types::cm_interface_module;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::token::Span;
use crate::unparse::unparse_type_into;
use crate::world_registry::{InterfaceExportLookup, InterfaceExportMethod, WorldRegistry};

/// The declaration `interface_fq` names as `wado_name`, or `None` where it
/// declares none. The boundary step §9 of the declaration-identity WEP reserves.
pub fn cm_decl_in_interface(
    type_table: &TypeTable,
    registry: &CmInterfaceRegistry,
    interface_fq: &str,
    wado_name: &str,
) -> Option<DefId> {
    if let Some(source) = registry.cm_interface_module_source_of(interface_fq) {
        return type_table.cm_decl_in(wado_name, source);
    }
    // A bundled `wasi:` / `core:` interface records no module, and its FQ is its
    // own module path.
    let (namespace, module) = cm_interface_module(interface_fq)?;
    type_table.cm_decl_in_module_named(wado_name, &module, namespace)
}

/// The one classifier, so every operation on a given `Future<T>` — read, write,
/// cancel, drop, new — agrees on its component-level future type.
///
/// # Panics
/// On a payload it cannot classify. Ask [`future_payload_rejection`] first, for
/// a diagnostic instead.
pub fn classify_future_payload(type_table: &TypeTable, type_arg: TypeId) -> CmFuturePayload {
    try_classify_future_payload(type_table, type_arg).unwrap_or_else(|| {
        panic!(
            "`Future<{}>` has no Component Model payload type",
            type_table.base_type_name(type_arg)
        )
    })
}

pub fn future_payload_rejection(type_table: &TypeTable, payload: TypeId) -> Option<String> {
    try_classify_future_payload(type_table, payload)
        .is_none()
        .then(|| {
            format!(
                "`{}` has no Component Model representation as a `future` payload",
                type_table.type_name(payload)
            )
        })
}

/// The registry-driven `Record` stream path needs no [`CmPayloadType`].
pub fn is_cm_record_stream_element(type_table: &TypeTable, element: TypeId) -> bool {
    matches!(
        type_table.get(peel_newtypes(type_table, element)),
        ResolvedType::Struct { def, .. }
            if def.decl().is_some_and(|d| is_cm_owned_source(type_table.def_module(d)))
    )
}

/// The byte stream, which the hand-written `core:rt` adapters bind. A newtype
/// over `u8` is one: the payload classification peels before it decides.
pub fn is_u8_stream_element(type_table: &TypeTable, element: TypeId) -> bool {
    matches!(
        type_table.get(peel_newtypes(type_table, element)),
        ResolvedType::Primitive(PrimitiveType::U8)
    )
}

pub fn stream_payload_rejection(type_table: &TypeTable, element: TypeId) -> Option<String> {
    let element = peel_newtypes(type_table, element);
    if is_u8_stream_element(type_table, element) {
        return None;
    }
    cm_payload_type_from_type_id(type_table, element)
        .is_none()
        .then(|| {
            format!(
                "`{}` has no Component Model representation as a `stream` element",
                type_table.type_name(element)
            )
        })
}

/// A WIT alias has no representation of its own. The AST classifier's
/// `resolve_type` peels too, and the two must agree.
///
/// Newtypes only: `TypeTable::representation_head` also collapses `flags` to
/// `u32`, and a CM `flags` is its own type, one byte wide at ≤8 labels.
pub fn peel_newtypes(type_table: &TypeTable, type_id: TypeId) -> TypeId {
    type_table.reflect_structure_head(type_id)
}

fn try_classify_future_payload(
    type_table: &TypeTable,
    type_arg: TypeId,
) -> Option<CmFuturePayload> {
    let type_arg = peel_newtypes(type_table, type_arg);
    match type_table.get(type_arg) {
        ResolvedType::Primitive(prim) => {
            if let Some(scalar) = primitive_to_cm_scalar(prim) {
                return Some(CmFuturePayload::Scalar(scalar));
            }
        }
        ResolvedType::GenericInstance { type_args, .. }
            if type_table.is_result(type_arg) && type_args.len() >= 2 =>
        {
            if matches!(type_table.get(type_args[0]), ResolvedType::Unit)
                && let Some(decl) = wasi_error_code_decl(type_table, type_args[1])
            {
                return Some(CmFuturePayload::Transmission(decl));
            }
        }
        _ => {}
    }
    if let Some(payload) = cm_payload_type_from_type_id(type_table, type_arg) {
        return Some(CmFuturePayload::Value(payload));
    }
    is_trailers_payload(type_table, type_arg).then_some(CmFuturePayload::Trailers)
}

/// `result<option<trailers>, error-code>`, recognized by shape: codegen builds
/// this `future<T>` from the HTTP types interface, not from a [`CmPayloadType`].
fn is_trailers_payload(type_table: &TypeTable, type_arg: TypeId) -> bool {
    let ResolvedType::GenericInstance { type_args, .. } = type_table.get(type_arg) else {
        return false;
    };
    if !type_table.is_result(type_arg) || type_args.len() < 2 {
        return false;
    }
    let Some(inner) = type_table.as_option(type_args[0]) else {
        return false;
    };
    matches!(
        type_table.get(type_table.representation_head(inner)),
        ResolvedType::Resource { .. }
    )
}

/// A WASI-owned element (`stream<directory-entry>`) belongs to the
/// registry-driven `Record` path instead — select with
/// [`is_cm_record_stream_element`] before asking.
///
/// # Panics
/// On an element it cannot classify. Ask [`stream_payload_rejection`] first,
/// for a diagnostic instead.
pub fn classify_stream_payload(type_table: &TypeTable, element: TypeId) -> CmStreamPayload {
    use crate::canonical::CmStreamPayload;
    let element = peel_newtypes(type_table, element);
    if is_u8_stream_element(type_table, element) {
        return CmStreamPayload::U8;
    }
    match cm_payload_type_from_type_id(type_table, element) {
        Some(payload) => CmStreamPayload::Value(payload),
        None => panic!(
            "`Stream<{}>` has no Component Model element type: a guest-created \
             stream of a WASI-owned type is not supported yet",
            type_table.base_type_name(element)
        ),
    }
}

/// `None` for a type with no general payload: a CM-owned record, which takes
/// the registry-driven path, and 128-bit / SIMD primitives, which have no CM
/// scalar.
pub fn cm_payload_type_from_type_id(
    type_table: &TypeTable,
    type_id: TypeId,
) -> Option<CmPayloadType> {
    let type_id = peel_newtypes(type_table, type_id);
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
        ResolvedType::Struct { .. } if type_table.is_string(type_id) => Some(CmPayloadType::String),
        ResolvedType::GenericInstance { type_args, .. }
            if type_table.is_result(type_id) && type_args.len() == 2 =>
        {
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
        ResolvedType::Struct { def, .. }
            if !is_cm_owned_source(type_table.struct_head_module(*def)) =>
        {
            // An anonymous struct names no declaration, so it is no CM record.
            Some(CmPayloadType::Named(CmDecl::new(
                type_table.defs(),
                def.decl()?,
                &to_kebab(&type_table.struct_head_name(*def)),
            )))
        }
        ResolvedType::Enum { def }
        | ResolvedType::Variant { def }
        | ResolvedType::Flags { def }
            if !is_cm_owned_source(type_table.def_module(*def)) =>
        {
            Some(CmPayloadType::Named(CmDecl::new(
                type_table.defs(),
                *def,
                &to_kebab(type_table.def_name(*def)),
            )))
        }
        // Unlike the records above, a WASI-owned resource is included: its
        // component type is aliased from the defining interface, so `own<…>`
        // has one to point at.
        ResolvedType::Resource { def } => Some(CmPayloadType::Resource(CmDecl::new(
            type_table.defs(),
            *def,
            &to_kebab(type_table.def_name(*def)),
        ))),
        _ => None,
    }
}

/// Whether a type's module source already owns a CM lowering path: `wasi:*`
/// interfaces and the `core:kiln/*` generator surface. User, local, and
/// dependency declarations do not, so they route through `Named`.
fn is_cm_owned_source(ms: &ModuleSource) -> bool {
    match ms {
        ModuleSource::Binding { .. } => true,
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

/// AST-`Type` analogue of [`cm_payload_type_from_type_id`], for codegen, which
/// works off the export's raw Wado return type.
pub fn cm_payload_type_from_ast(
    type_table: &TypeTable,
    ty: &ast::Type,
    registry: &CmInterfaceRegistry,
) -> Option<CmPayloadType> {
    use crate::ast::Type;
    use crate::canonical::CmPayloadType;
    let resolved = registry.resolve_type(ty);
    match &resolved {
        Type::Named(n) if n.name == "String" => Some(CmPayloadType::String),
        Type::Named(n) => {
            if let Some(scalar) = cm_scalar_from_ast_name(&n.name) {
                return Some(CmPayloadType::Scalar(scalar));
            }
            let src = registry.resolve_cm_source_for(n)?;
            let declared = |cm: &str| {
                let def = cm_decl_in_interface(type_table, registry, &src, &n.name)?;
                Some(CmDecl::new(type_table.defs(), def, cm))
            };
            // Before the CM-owned bail below: a WASI resource counts.
            if let Some(cm) = registry
                .get_resource_cm_name_by_source(&src, &n.name)
                .map(str::to_string)
            {
                return Some(CmPayloadType::Resource(declared(&cm)?));
            }
            if src.starts_with("wasi:") || src.starts_with("core:kiln/") {
                return None;
            }
            let cm = registry
                .get_struct_cm_name_by_source(&src, &n.name)
                .or_else(|| registry.get_variant_cm_name_by_source(&src, &n.name))
                .or_else(|| registry.get_enum_cm_name_by_source(&src, &n.name))
                .or_else(|| registry.get_flags_cm_name_by_source(&src, &n.name))?
                .to_string();
            Some(CmPayloadType::Named(declared(&cm)?))
        }
        Type::Tuple(elems) => elems
            .iter()
            .map(|e| cm_payload_type_from_ast(type_table, e, registry))
            .collect::<Option<Vec<_>>>()
            .map(CmPayloadType::Tuple),
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => Some(CmPayloadType::Option(Box::new(
                cm_payload_type_from_ast(type_table, &g.args[0], registry)?,
            ))),
            "List" if g.args.len() == 1 => Some(CmPayloadType::List(Box::new(
                cm_payload_type_from_ast(type_table, &g.args[0], registry)?,
            ))),
            "Result" if g.args.len() == 2 => {
                let arm = |t: &Type| -> Option<Option<Box<CmPayloadType>>> {
                    if t.is_unit() {
                        Some(None)
                    } else {
                        Some(Some(Box::new(cm_payload_type_from_ast(
                            type_table, t, registry,
                        )?)))
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
    type_table: &TypeTable,
    ty: &ast::Type,
    registry: &CmInterfaceRegistry,
) -> CmStreamPayload {
    use crate::ast::Type;
    use crate::canonical::CmStreamPayload;
    let resolved = registry.resolve_type(ty);
    if let Type::Named(n) = &resolved
        && n.name == "u8"
    {
        return CmStreamPayload::U8;
    }
    match cm_payload_type_from_ast(type_table, ty, registry) {
        Some(payload) => CmStreamPayload::Value(payload),
        None => panic!(
            "`Stream<{}>` has no Component Model element type",
            render_ast_type(ty)
        ),
    }
}

/// Classify an AST-`Type` future payload (codegen's `task.return` resolver),
/// mirroring [`classify_future_payload`] on resolved types. The two must agree:
/// one classifies the future the guest operates on, the other the future the
/// export's signature declares, and a disagreement builds a component whose
/// declared type is not the one the body reads.
pub fn classify_future_payload_from_ast(
    type_table: &TypeTable,
    ty: &ast::Type,
    registry: &CmInterfaceRegistry,
) -> CmFuturePayload {
    use crate::ast::Type;
    let resolved = registry.resolve_type(ty);
    if let Type::Named(n) = &resolved
        && let Some(scalar) = cm_scalar_from_ast_name(&n.name)
    {
        return CmFuturePayload::Scalar(scalar);
    }
    if let Type::Generic(g) = &resolved
        && g.name == "Result"
        && g.args.len() == 2
        && g.args[0].is_unit()
        && let Some(decl) = wasi_error_code_decl_from_ast(type_table, &g.args[1], registry)
    {
        return CmFuturePayload::Transmission(decl);
    }
    if let Some(payload) = cm_payload_type_from_ast(type_table, ty, registry) {
        return CmFuturePayload::Value(payload);
    }
    if is_trailers_payload_from_ast(&resolved, registry) {
        return CmFuturePayload::Trailers;
    }
    panic!(
        "`Future<{}>` has no Component Model payload type",
        render_ast_type(&resolved)
    );
}

fn wasi_error_code_decl_from_ast(
    type_table: &TypeTable,
    ty: &ast::Type,
    registry: &CmInterfaceRegistry,
) -> Option<CmDecl> {
    let ast::Type::Named(n) = &registry.resolve_type(ty) else {
        return None;
    };
    let source = registry.resolve_cm_source_for(n)?;
    if !source.starts_with("wasi:") {
        return None;
    }
    let cm_name = registry
        .get_enum_cm_name_by_source(&source, &n.name)
        .or_else(|| registry.get_variant_cm_name_by_source(&source, &n.name))?
        .to_string();
    let def = cm_decl_in_interface(type_table, registry, &source, &n.name)?;
    Some(CmDecl::new(type_table.defs(), def, &cm_name))
}

/// `result<option<resource>, _>`.
fn is_trailers_payload_from_ast(resolved: &ast::Type, registry: &CmInterfaceRegistry) -> bool {
    use crate::ast::Type;
    let Type::Generic(g) = resolved else {
        return false;
    };
    if g.name != "Result" || g.args.len() != 2 {
        return false;
    }
    let Type::Generic(ok) = &registry.resolve_type(&g.args[0]) else {
        return false;
    };
    if ok.name != "Option" || ok.args.len() != 1 {
        return false;
    }
    let Type::Named(inner) = &registry.resolve_type(&ok.args[0]) else {
        return false;
    };
    registry
        .resolve_cm_source_for(inner)
        .and_then(|source| {
            registry
                .get_resource_cm_name_by_source(&source, &inner.name)
                .map(str::to_string)
        })
        .is_some()
}

/// For a diagnostic or a panic message — the `Debug` form dumps spans and ids.
fn render_ast_type(ty: &ast::Type) -> String {
    let mut out = String::new();
    unparse_type_into(ty, &mut out);
    out
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

/// The declaration of the WASI `ErrorCode` a transmission future carries, or
/// `None` if the type is not a WASI error-code — an ordinary `result<_, E>`
/// payload, then, not a transmission future.
///
/// The declaration and not its package: `wasi:sockets/types` and
/// `wasi:sockets/ip-name-lookup` each declare one, and they are unrelated.
fn wasi_error_code_decl(type_table: &TypeTable, error_type_id: TypeId) -> Option<CmDecl> {
    let (ResolvedType::Enum { def } | ResolvedType::Variant { def }) =
        type_table.get(error_type_id)
    else {
        return None;
    };
    let ModuleSource::Binding {
        namespace: CmNamespace::Wasi,
        ..
    } = type_table.def_module(*def)
    else {
        return None;
    };
    Some(CmDecl::new(
        type_table.defs(),
        *def,
        &to_kebab(type_table.def_name(*def)),
    ))
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

/// Strip an `AsyncCall<T>` wrapper from an `async` interface method's declared
/// return type, recovering the CM-level result `T`. WIT lowers
/// `async func foo(…) -> T` with result `T`, but the Wado-facing declaration
/// exposes `AsyncCall<T>` so user code can defer `wait()`-ing — and the binding
/// synthesiser needs the raw `T` for outptr layout. Otherwise a no-op.
pub fn unwrap_async_call_if_async(is_async: bool, declared: &Option<Type>) -> Option<Type> {
    if !is_async {
        return declared.clone();
    }
    match declared {
        Some(Type::Generic(generic)) if generic.name == "AsyncCall" && generic.args.len() == 1 => {
            let inner = &generic.args[0];
            if inner.is_unit() {
                return None;
            }
            Some(inner.clone())
        }
        other => other.clone(),
    }
}

/// Preserves acronym casing from the WIT source (e.g., `DNS-timeout`, `TLS-protocol-error`).
/// Panics if no `#[cm]` attribute is present — all CM names must be metadata-driven.
fn cm_attr_cm_name(attrs: &[Attribute], wado_name: &str) -> String {
    attrs
        .iter()
        .find_map(|a| match &a.cm_boundary {
            // Type-level CM imports use the function fragment (after `#`)
            // as the CM name. If the path has no function fragment, fall
            // back to the whole interface path to preserve the previous
            // behaviour.
            Some(CmBoundary::Import(cm)) => {
                Some(cm.function.clone().unwrap_or_else(|| cm.interface_path()))
            }
            // Case-level CM names (variant cases, fields, ...) carry just
            // the CM-side identifier.
            Some(CmBoundary::Name(s)) => Some(s.clone()),
            Some(CmBoundary::Canonical { .. } | CmBoundary::WorldImport(_)) | None => None,
        })
        .unwrap_or_else(|| panic!("missing #[cm] attribute for CM name: {wado_name}"))
}

/// The CM type an unrestricted resource crosses the boundary as. No CM value
/// type is a reference, so a copyable handle is an integer.
fn extern_handle_type(span: Span) -> Type {
    Type::Named(NamedType::new(AstId::fresh(), "u32".to_string(), span))
}

/// Extract CM parameter names from a `#[cm_params("param-a", "param-b")]` attribute.
fn extract_cm_params_attr(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .find(|a| a.name == CM_PARAMS)
        .map(|a| a.args.iter().map(|arg| arg.as_str().to_string()).collect())
        .unwrap_or_default()
}

/// Information about a CM function from an interface method
#[derive(Debug, Clone)]
pub struct CmFunctionInfo {
    /// Binding namespace (e.g., "wasi", "web")
    pub namespace: String,
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
    /// Format: `{namespace}:{package}/{interface_name}::{method_name}`
    /// Example: `wasi:cli/Stdout::write_via_stream`
    pub fn local_alias_name(&self) -> String {
        build_local_alias_name(
            &self.namespace,
            &self.package,
            &self.interface_name,
            &self.method_name,
        )
    }

    /// This function's key in `NirPackage::used_wasi_functions` (e.g.
    /// `"Stdout::write_via_stream"`).
    #[must_use]
    pub fn used_key(&self) -> String {
        used_wasi_key(&self.interface_name, &self.method_name)
    }

    /// Whether `canon lower` requires the Memory canonical option.
    ///
    /// True when any parameter or return type involves linear memory
    /// (strings, lists, streams, options, results, tuples), or when async.
    fn needs_memory(&self) -> bool {
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

    /// Whether `canon lower` requires the Memory canonical option, counting the
    /// CM records and variants the registry knows. Realloc is required under
    /// exactly the same conditions.
    pub fn needs_memory_with_registry(&self, registry: &CmInterfaceRegistry) -> bool {
        if self.needs_memory() {
            return true;
        }
        let mut seen = IndexSet::default();
        if let Some(rt) = &self.return_type
            && Self::cm_type_requires_memory(rt, registry, &mut seen)
        {
            return true;
        }
        self.params
            .iter()
            .any(|(_, _, ty)| Self::cm_type_requires_memory(ty, registry, &mut seen))
    }

    /// Whether lowering `ty` needs the `memory` canonical option, counting the
    /// CM records and variants the registry knows, at any depth. A record
    /// reached through an `Option` needs memory exactly as one at the top does.
    fn cm_type_requires_memory(
        ty: &Type,
        registry: &CmInterfaceRegistry,
        seen: &mut IndexSet<String>,
    ) -> bool {
        match ty {
            Type::Named(named) => {
                named.name == "String" || Self::cm_named_requires_memory(named, registry, seen)
            }
            Type::Generic(g) => {
                matches!(g.name.as_str(), "Stream" | "List")
                    || g.args
                        .iter()
                        .any(|arg| Self::cm_type_requires_memory(arg, registry, seen))
            }
            Type::Tuple(elems) => elems
                .iter()
                .any(|elem| Self::cm_type_requires_memory(elem, registry, seen)),
            Type::Reference(inner) | Type::MutReference(inner) => {
                Self::cm_type_requires_memory(inner, registry, seen)
            }
            Type::NamespacedGeneric(_)
            | Type::Function(_)
            | Type::TypePackSpread(..)
            | Type::Infer(_)
            | Type::Error(_) => false,
        }
    }

    /// Whether the CM record or variant `named` refers to needs memory. Uses
    /// the reference's own `source_interface` when present (stdlib-populated),
    /// otherwise falls back to the unique `wasi:*` source. `seen` stops a
    /// variant that reaches itself through a payload.
    fn cm_named_requires_memory(
        named: &NamedType,
        registry: &CmInterfaceRegistry,
        seen: &mut IndexSet<String>,
    ) -> bool {
        let Some(source) = registry.source_interface(named).or_else(|| {
            registry
                .find_binding_source(CmTypeKind::Variant, &named.name)
                .map(str::to_string)
        }) else {
            return false;
        };
        // A record has several fields and exceeds MAX_FLAT_RESULTS (1) in canon
        // lower, so it always goes through memory.
        if registry
            .get_struct_fields_by_source(&source, &named.name)
            .is_some()
        {
            return true;
        }
        let Some(cases) = registry.get_variant_cases_by_source(&source, &named.name) else {
            return false;
        };
        if !seen.insert(format!("{source}#{}", named.name)) {
            return false;
        }
        cases.iter().any(|case| {
            case.payload
                .as_ref()
                .is_some_and(|payload| Self::cm_type_requires_memory(payload, registry, seen))
        })
    }

    /// Whether a parameter type requires Memory + Realloc in canon lower: a
    /// string, a list or a stream anywhere in it, `option<string>` included.
    fn type_requires_memory(ty: &Type) -> bool {
        match ty {
            Type::Generic(g) => {
                matches!(g.name.as_str(), "Stream" | "List")
                    || g.args.iter().any(Self::type_requires_memory)
            }
            Type::Named(named) => named.name == "String",
            Type::Tuple(elems) => elems.iter().any(Self::type_requires_memory),
            Type::Reference(inner) | Type::MutReference(inner) => Self::type_requires_memory(inner),
            Type::NamespacedGeneric(_)
            | Type::Function(_)
            | Type::TypePackSpread(..)
            | Type::Infer(_)
            | Type::Error(_) => false,
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

    /// Whether this function returns a `Future<T>`, or a tuple containing one.
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

/// Build a CM function's local alias name,
/// `{namespace}:{package}/{interface_name}::{method_name}` — e.g.
/// `wasi:cli/Stdout::write_via_stream`. The namespace and package keep it
/// unique across packages, and the last two segments are the Wado effect /
/// method names, not the WIT interface / function ones.
pub fn build_local_alias_name(
    namespace: &str,
    package: &str,
    interface_name: &str,
    method_name: &str,
) -> String {
    format!("{namespace}:{package}/{interface_name}::{method_name}")
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

/// The codegen component-instance key `"{package}-{interface}"` (e.g.
/// `"http-types"`). Package-qualified because a bare interface name collides
/// across packages (`wasi:cli/types` vs `wasi:http/types`). Both
/// [`CmInterfaceInfo`] and [`crate::ast::CmImport`] name the same instance, so
/// both build the key here.
#[must_use]
pub fn cm_instance_key(package: &str, interface: &str) -> String {
    format!("{package}-{interface}")
}

impl CmInterfaceInfo {
    /// This interface's [`cm_instance_key`].
    #[must_use]
    pub fn instance_key(&self) -> String {
        cm_instance_key(&self.package, &self.interface)
    }
}

/// Registry of WASI imports for code generation
///
/// Collects information from effect definitions and provides:
/// - Resolution of effect calls (e.g., "`Stdout::write_via_stream`") to local names
/// - Iteration over interfaces for Component Model import generation
#[derive(Debug, Clone, Default)]
pub struct CmInterfaceRegistry {
    /// The CM interface each type reference resolves to, keyed by the
    /// reference site.
    ///
    /// A resolved fact does not belong on the syntax node: there it would be a
    /// second answer beside `crate::resolve::Resolutions`, which keys the same
    /// `AstId`, free to disagree with it. Keyed here, one pass writes it and
    /// every consumer reads the same entry (WEP 2026-08-12).
    source_interfaces: SourceInterfaces,

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

    /// Resources declared `#[cm(..., linearity = "unrestricted")]`, registered as
    /// `u32` newtypes rather than in [`Self::resources`].
    /// Key: `(source_interface, wado_name)`. Value: CM kebab-case name.
    unrestricted_resources: IndexMap<(String, String), String>,

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

    /// Lib-local named type -> the module that actually defines it. A library
    /// spreads its public types across submodules, so the interface FQ (which
    /// maps to the entry module) cannot alone locate a submodule-defined type
    /// like `HeadingInfo`. Keyed by name — unique within a library's API.
    lib_local_type_sources: IndexMap<String, ModuleSource>,

    /// FQs of interfaces imported from a CM component dependency. The plan
    /// classifies these as [`crate::wir::ImportKind::Component`] for composition.
    component_interfaces: IndexSet<String>,

    /// Per component-dependency interface FQ, the host-leaf import FQs the
    /// dependency component itself declares (its WASI capabilities). Effect
    /// reconstruction unions these into the consumer when the interface is used;
    /// a purely-computational component maps to an empty set. Every exported
    /// interface of one component shares the same set (v1 component-level union).
    component_host_leaf_imports: IndexMap<String, Vec<String>>,

    /// World-level function imports (Phase 9), by bare name. Marks which
    /// `effect_to_func` entries are world-level (their `CmFunctionInfo` lives
    /// there under the bare name).
    world_import_functions: IndexSet<String>,

    /// World-level function import name → the dependency `ModuleSource` that
    /// exports it (the composition target, and the identity that distinguishes
    /// it from a same-named local function).
    world_import_sources: IndexMap<String, ModuleSource>,

    /// Reverse index for the `_by_module` accessors (see [`ModuleSourceIndex`]).
    module_index: std::sync::OnceLock<ModuleSourceIndex>,
}

/// Reverse index for the `ModuleSource`-bridged CM-name lookups: per type kind,
/// `(module identity, wado name) -> cm name`. Each registration is indexed
/// under two keys so a lookup is O(1) from either identity namespace — the
/// interface stem of the `#[cm(...)]` source (stdlib), and the exact
/// `ModuleSource` display of a `--lib`/component interface (whose FQ shares no
/// stem with its loader identity).
#[derive(Debug, Clone, Default)]
struct ModuleSourceIndex {
    resources: IndexMap<(String, String), String>,
    structs: IndexMap<(String, String), String>,
    variants: IndexMap<(String, String), String>,
    enums: IndexMap<(String, String), String>,
    flags: IndexMap<(String, String), String>,
}

/// What a `#[cm(…)]` binds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CmBindingKind {
    Type,
    Function,
}

/// Every `#[cm(…)]` in `module`, paired with what it binds. One walk answers
/// both who has bindings at all and which of them are functions: two walks drift,
/// and a module an incomplete one skips loses declarations where nothing looks.
fn cm_imports(module: &ast::Module) -> Vec<(CmBindingKind, &CmImport)> {
    use crate::ast::Item;

    fn binds(
        kind: CmBindingKind,
        attrs: &[Attribute],
    ) -> impl Iterator<Item = (CmBindingKind, &CmImport)> {
        attrs
            .iter()
            .filter_map(Attribute::as_cm_import)
            .map(move |import| (kind, import))
    }
    fn operations(methods: &[ast::Function]) -> impl Iterator<Item = (CmBindingKind, &CmImport)> {
        methods
            .iter()
            .flat_map(|m| binds(CmBindingKind::Function, &m.attrs))
    }

    let mut found = Vec::new();
    for item in &module.items {
        match item {
            Item::Newtype(decl) => found.extend(binds(CmBindingKind::Type, &decl.attrs)),
            Item::Struct(decl) => found.extend(binds(CmBindingKind::Type, &decl.attrs)),
            Item::Enum(decl) => found.extend(binds(CmBindingKind::Type, &decl.attrs)),
            Item::Variant(decl) => found.extend(binds(CmBindingKind::Type, &decl.attrs)),
            Item::Flags(decl) => {
                if let Some(attrs) = decl.attributes.as_deref() {
                    found.extend(binds(CmBindingKind::Type, attrs));
                }
            }
            Item::Resource(decl) => {
                found.extend(binds(CmBindingKind::Type, &decl.attrs));
                found.extend(operations(&decl.methods));
            }
            Item::Interface(decl) => found.extend(operations(&decl.methods)),
            Item::Function(decl) => found.extend(binds(CmBindingKind::Function, &decl.attrs)),
            Item::Use(_)
            | Item::TupleTypeDecl(_)
            | Item::BuiltinTypeDecl(_)
            | Item::Impl(_)
            | Item::Trait(_)
            | Item::World(_)
            | Item::Test(_)
            | Item::Global(_)
            | Item::Error(_) => {}
        }
    }
    found
}

/// Whether `module` declares anything a [`CmDeclScope::CmAttributed`] scope
/// would admit.
pub fn declares_cm_binding(module: &ast::Module) -> bool {
    !cm_imports(module).is_empty()
}

/// The kind of resource-associated function, encoded by the `#[cm]` fragment
/// prefix (`[method]` / `[static]` / `[constructor]`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResKind {
    Method,
    Static,
    Constructor,
}

/// Classify a CM function name as a resource method/static/constructor,
/// returning `(kind, resource_cm_name, member_name)`. Free functions yield
/// `None`.
pub fn parse_resource_func(cm_func_name: &str) -> Option<(ResKind, &str, &str)> {
    if let Some(resource) = cm_func_name.strip_prefix("[constructor]") {
        Some((ResKind::Constructor, resource, ""))
    } else if let Some(rest) = cm_func_name.strip_prefix("[method]") {
        let (resource, member) = rest.split_once('.')?;
        Some((ResKind::Method, resource, member))
    } else if let Some(rest) = cm_func_name.strip_prefix("[static]") {
        let (resource, member) = rest.split_once('.')?;
        Some((ResKind::Static, resource, member))
    } else {
        None
    }
}

/// Which declarations of a module enter the registry.
enum CmDeclScope {
    /// Everything the module declares: a bundled binding module is all CM, and
    /// it declares every `#[cm_params]`, so a gap there is a generator defect.
    Module,
    /// Only what carries `#[cm(…)]`. A user module's own declarations are not
    /// bindings, and a hand-written one may leave `#[cm_params]` out.
    CmAttributed,
}

impl CmDeclScope {
    /// Whether a declaration whose `#[cm(…)]` resolved to `source_interface`
    /// (empty when it carries none) enters the registry.
    fn admits(&self, source_interface: &str) -> bool {
        match self {
            Self::Module => true,
            Self::CmAttributed => !source_interface.is_empty(),
        }
    }

    /// The CM name for a parameter `#[cm_params]` did not name.
    fn param_fallback(&self, param: &str, interface: &str, method: &str) -> String {
        match self {
            Self::Module => {
                panic!("missing #[cm_params] for param '{param}' in {interface}.{method}")
            }
            Self::CmAttributed => to_kebab(param),
        }
    }
}

/// Insert into a `(source_interface, name)`-keyed map, panicking on a duplicate
/// registration. The same declaration registered twice is a stdlib bug, and a
/// loud failure beats a silent overwrite.
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

/// The key an interface operation takes in `NirPackage::used_wasi_functions`
/// (e.g. `"Stdout::write_via_stream"`).
#[must_use]
pub fn used_wasi_key(interface: &str, operation: &str) -> String {
    format!("{interface}::{operation}")
}

/// `emitting` itself, where it declares `wado_name` in a
/// `(source_interface, wado_name)`-keyed map. Borrowed from the map rather than
/// from the argument, so a caller may pass a short-lived interface name.
fn declaring_source<'a, V>(
    map: &'a IndexMap<(String, String), V>,
    emitting: Option<&str>,
    wado_name: &str,
) -> Option<&'a str> {
    let iface = emitting?;
    map.get_key_value(&(iface.to_string(), wado_name.to_string()))
        .map(|((source, _), _)| source.as_str())
}

/// The Wado name a `(source_interface, wado_name)`-keyed map of CM names holds
/// under `iface_fq` for `cm_name`.
fn wado_name_under_cm_name<'a>(
    map: &'a IndexMap<(String, String), String>,
    iface_fq: &str,
    cm_name: &str,
) -> Option<&'a str> {
    map.iter()
        .find(|((source, _), name)| source == iface_fq && name.as_str() == cm_name)
        .map(|((_, wado_name), _)| wado_name.as_str())
}

/// A keyspace of the registry, one per kind of type a CM reference can name.
/// Every search over the kinds walks [`Self::ALL`], so one added here reaches
/// all of them at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmTypeKind {
    Newtype,
    Resource,
    Struct,
    Variant,
    Enum,
    Flags,
}

impl CmTypeKind {
    /// The order a bare-name search asks the kinds in. Which kind answers first
    /// is the tiebreak [WEP: Declaration Identity] records as unowned.
    ///
    /// [WEP: Declaration Identity]: ../../docs/wep-2026-08-12-declaration-identity.md
    pub const ALL: [Self; 6] = [
        Self::Newtype,
        Self::Resource,
        Self::Struct,
        Self::Variant,
        Self::Enum,
        Self::Flags,
    ];
}

/// The first name a `(source_interface, name)` key set holds under `iface_fq`,
/// which says that some module already declares into it.
fn first_name_in_interface<'a>(
    mut keys: impl Iterator<Item = &'a (String, String)>,
    iface_fq: &str,
) -> Option<&'a str> {
    keys.find(|(source, _)| source == iface_fq)
        .map(|(_, name)| name.as_str())
}

/// Whether a registration under `iface_fq` spells its CM name `cm_name`, where
/// `of` reads that name out of the kind's value.
fn spells_cm_name<V>(
    map: &IndexMap<(String, String), V>,
    iface_fq: &str,
    cm_name: &str,
    of: impl Fn(&V) -> &str,
) -> bool {
    map.iter()
        .any(|((fq, _), value)| fq == iface_fq && of(value) == cm_name)
}

/// Return the `source_interface` when `name` has exactly one registrant under
/// `prefix`.
fn find_unique_source_with_prefix<'a>(
    keys: impl Iterator<Item = &'a (String, String)>,
    prefix: &str,
    name: &str,
) -> Option<&'a str> {
    find_unique_source_in_set(keys, name, &|src| src.starts_with(prefix))
}

/// The unique source interface registering `name` across every bundled CM
/// namespace. A name declared by two of them is ambiguous — `None` — never
/// silently the `wasi:` one.
fn find_unique_source_in_binding<'a>(
    keys: impl Iterator<Item = &'a (String, String)>,
    name: &str,
) -> Option<&'a str> {
    find_unique_source_in_set(keys, name, &|src| {
        CmNamespace::split_specifier(src).is_some()
    })
}

/// The unique source interface registering `name` among sources for which
/// `is_member` holds, or `None` when there is no or more than one match.
fn find_unique_source_in_set<'a>(
    keys: impl Iterator<Item = &'a (String, String)>,
    name: &str,
    is_member: &dyn Fn(&str) -> bool,
) -> Option<&'a str> {
    let mut found: Option<&str> = None;
    for (src, n) in keys {
        if n != name || !is_member(src) {
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
fn find_unique_source_in<'a>(
    keys: impl Iterator<Item = &'a (String, String)>,
    name: &str,
) -> Option<&'a str> {
    find_unique_source_in_set(keys, name, &|src| !src.is_empty())
}

/// A source string without a trailing `.wado` suffix or `@version` tag, so a
/// loader identity (`wasi:http/types.wado`) and its CM registration key
/// (`wasi:http/types@0.3.0`) reduce to the same stem.
fn interface_stem(source: &str) -> &str {
    let source = source.strip_suffix(".wado").unwrap_or(source);
    source.split_once('@').map_or(source, |(base, _)| base)
}

/// Look up a `ModuleSourceIndex` kind map by a coarse [`ModuleSource`] display
/// string. Tries the exact identity first (a `--lib`/component interface,
/// keyed by its `ModuleSource` display), then the interface stem (stdlib
/// `wasi:`/`core:` types, whose loader path stems match their registration
/// source stem). O(1) in both cases.
fn lookup_by_module<'a>(
    index: &'a IndexMap<(String, String), String>,
    module_source: &str,
    name: &str,
) -> Option<&'a str> {
    index
        .get(&(module_source.to_string(), name.to_string()))
        .or_else(|| index.get(&(interface_stem(module_source).to_string(), name.to_string())))
        .map(String::as_str)
}

/// Resolve a `Type::Named` reference through the newtype-alias map by its
/// declaring interface (`source_interface`). A source-less reference, or one
/// naming no newtype, is returned unchanged, as are other type shapes.
fn resolve_type(
    ty: &Type,
    aliases: &IndexMap<(String, String), Type>,
    sources: &SourceInterfaces,
) -> Type {
    match ty {
        Type::Named(named) => sources
            .get(named.id)
            .and_then(|source| aliases.get(&(source, named.name.clone())))
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        _ => ty.clone(),
    }
}

/// Replace `Self` with the enclosing resource's name (and its source interface)
/// in a receiver type. The `self` / `&self` / `&mut self` forms carry `Self`,
/// but the CM registry stores concrete types, so `&self` on `resource
/// Descriptor` must register as `&Descriptor` carrying `Descriptor`'s source
/// interface — the type the old `self: &Descriptor` spelling produced. Losing
/// the source interface would make [`CmInterfaceRegistry::resources_in_type`]
/// miss the borrow.
fn substitute_self_in_type(
    sources: &SourceInterfaces,
    ty: &Type,
    resource_name: &str,
    source_interface: &str,
) -> Type {
    match ty {
        Type::Named(n) if n.name == "Self" => {
            if !source_interface.is_empty() {
                sources.set(n.id, source_interface.to_string());
            }
            Type::Named(NamedType {
                name: resource_name.to_string(),
                ..n.clone()
            })
        }
        Type::Reference(inner) => Type::Reference(Box::new(substitute_self_in_type(
            sources,
            inner,
            resource_name,
            source_interface,
        ))),
        Type::MutReference(inner) => Type::MutReference(Box::new(substitute_self_in_type(
            sources,
            inner,
            resource_name,
            source_interface,
        ))),
        other => other.clone(),
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
        sources: &SourceInterfaces,
    ) -> Option<InterfaceExportLookup> {
        let entry = self.by_name.get(name)?;
        let methods = entry
            .methods
            .iter()
            .map(|m| InterfaceExportMethod {
                name: m.name.clone(),
                is_async: m.is_async,
                params: m
                    .params
                    .iter()
                    .map(|(n, ty)| (n.clone(), resolve_type(ty, newtypes, sources)))
                    .collect(),
                return_type: m
                    .return_type
                    .as_ref()
                    .map(|ty| unwrap_async_call_if_async(m.is_async, &Some(ty.clone())))
                    .unwrap_or(None)
                    .or_else(|| m.return_type.clone()),
            })
            .collect();
        Some(InterfaceExportLookup {
            cm_interface_fq: entry.cm_interface_fq.clone(),
            methods,
        })
    }
}

fn collect_interface_decls(modules: &[(&'static str, ast::Module)]) -> InterfaceDeclTable {
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
            // `import Foo;` both look up by local name).
            assert!(
                !table.by_name.contains_key(&iface.name),
                "InterfaceDeclTable: duplicate interface `{}` (also at {cm_fq:?}). \
                 Each interface name must be declared exactly once across the stdlib.",
                iface.name,
            );

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
fn collect_cm_definitions(module: &ast::Module) -> IndexMap<String, String> {
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

/// Build the `name -> source_interface` map that applies inside `module_path`,
/// from this module's own declarations plus its named imports (an
/// `X as Y` contributing `Y`). Later entries win, though stdlib introduces no
/// duplicates. A name the lookup cannot resolve — a primitive, a generic, an
/// unknown reference — is left out, and consumers read a missing key as
/// `source_interface = None`.
fn build_local_name_resolver(
    module_path: &str,
    module: &ast::Module,
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
                    UseItem::InterfaceFunctions { interface_name, .. } => {
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
        // Flat package form. Rather than caching a merged view of every
        // interface in the package, return None and let the caller resolve
        // per-item below — so `use { ErrorCode } from "wasi:http"` does not
        // resolve here. Stdlib does not rely on flat-package imports for type
        // references, so nothing needs it yet.
        let _ = source;
        None
    })
}

/// The CM interface each named-type reference site resolves to, keyed by the
/// reference's own [`crate::ast::AstId`].
pub type SourceInterfaceBatch = IndexMap<AstId, String>;

/// Which CM interface each named-type reference site resolves to.
///
/// Monotone and first-writer-wins: an already-answered site — a shared
/// `core:kiln/types` record — keeps its interface, which CM lift/lower needs to
/// find its fields. Interior-mutable because the registry is shared as
/// `Arc<CmInterfaceRegistry>` while synthesis mints new reference sites, and
/// `Sync` because the stdlib registry is a process-wide `OnceLock`.
#[derive(Debug, Default)]
pub struct SourceInterfaces(std::sync::RwLock<SourceInterfaceBatch>);

impl Clone for SourceInterfaces {
    fn clone(&self) -> Self {
        Self(std::sync::RwLock::new(self.read().clone()))
    }
}

impl SourceInterfaces {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, SourceInterfaceBatch> {
        self.0.read().expect("source-interface table not poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, SourceInterfaceBatch> {
        self.0.write().expect("source-interface table not poisoned")
    }

    #[must_use]
    pub fn get(&self, site: AstId) -> Option<String> {
        self.read().get(&site).cloned()
    }

    pub fn set(&self, site: AstId, interface: String) {
        self.write().entry(site).or_insert(interface);
    }

    pub fn extend(&self, batch: SourceInterfaceBatch) {
        let mut table = self.write();
        for (site, interface) in batch {
            table.entry(site).or_insert(interface);
        }
    }
}

/// Collect the source interface of every named-type reference reachable from
/// `module` whose identifier is in `local_names`.
///
/// References whose name is not in `local_names` are intentionally left
/// unanswered — those are primitives (`String`, `bool`, `i32`, ...), generic
/// type parameters, or names the stdlib never declares.
fn collect_named_type_sources(
    module: &ast::Module,
    local_names: &IndexMap<String, String>,
) -> SourceInterfaceBatch {
    let mut sources = SourceInterfaceBatch::default();
    for item in &module.items {
        collect_item_type_sources(&mut sources, item, local_names);
    }
    sources
}

/// Collect the source interfaces of every type reference `item` declares.
fn collect_item_type_sources(
    sources: &mut SourceInterfaceBatch,
    item: &Item,
    local_names: &IndexMap<String, String>,
) {
    use crate::ast::Item;
    match item {
        Item::Function(f) => {
            for param in &f.params {
                walk_type(sources, &param.ty, local_names);
            }
            if let Some(ret) = &f.return_type {
                walk_type(sources, ret, local_names);
            }
        }
        Item::Struct(s) => {
            for field in &s.fields {
                walk_type(sources, &field.ty, local_names);
            }
        }
        Item::Variant(v) => {
            for case in &v.cases {
                if let Some(payload) = &case.payload {
                    walk_type(sources, payload, local_names);
                }
            }
        }
        Item::Newtype(a) => walk_type(sources, &a.ty, local_names),
        Item::Interface(effect) => {
            for method in &effect.methods {
                for param in &method.params {
                    walk_type(sources, &param.ty, local_names);
                }
                if let Some(ret) = &method.return_type {
                    walk_type(sources, ret, local_names);
                }
            }
        }
        Item::Resource(r) => {
            for method in &r.methods {
                for param in &method.params {
                    walk_type(sources, &param.ty, local_names);
                }
                if let Some(ret) = &method.return_type {
                    walk_type(sources, ret, local_names);
                }
            }
        }
        _ => {}
    }
}

/// Recursively descend into a Wado `Type` and answer for every named leaf whose
/// identifier appears in `local_names`.
fn walk_type(
    sources: &mut SourceInterfaceBatch,
    ty: &ast::Type,
    local_names: &IndexMap<String, String>,
) {
    use crate::ast::Type;
    match ty {
        Type::Named(n) => {
            if let Some(source) = local_names.get(&n.name) {
                sources.entry(n.id).or_insert_with(|| source.clone());
            }
        }
        Type::Generic(g) => {
            for arg in &g.args {
                walk_type(sources, arg, local_names);
            }
        }
        Type::NamespacedGeneric(g) => {
            for arg in &g.args {
                walk_type(sources, arg, local_names);
            }
        }
        Type::Function(f) => {
            for p in &f.params {
                walk_type(sources, p, local_names);
            }
            walk_type(sources, &f.return_type, local_names);
        }
        Type::Tuple(elems) => {
            for e in elems {
                walk_type(sources, e, local_names);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            walk_type(sources, inner, local_names);
        }
        _ => {}
    }
}

/// `name -> iface_fq` for every top-level type decl in `items`, the input
/// `walk_type` needs to stamp `source_interface` on lib-local references.
fn local_type_names<'a>(
    items: impl Iterator<Item = &'a Item>,
    iface_fq: &str,
) -> IndexMap<String, String> {
    use crate::ast::Item;
    items
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
        .collect()
}

impl CmInterfaceRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the resource `name` declared in `source` is unrestricted, and so
    /// crosses the boundary as the universal handle rather than a CM handle.
    #[must_use]
    pub fn is_unrestricted_resource(&self, source: &str, name: &str) -> bool {
        self.unrestricted_resources
            .contains_key(&(source.to_string(), name.to_string()))
    }

    /// The CM type of a declared parameter: an extern-handle loses its
    /// reference, then newtypes resolve.
    fn cm_param_type(&self, ty: &Type) -> Type {
        resolve_type(
            &self.deref_extern_handle(ty),
            &self.newtypes,
            &self.source_interfaces,
        )
    }

    /// Drop the reference around an extern-handle: the handle is a value, so
    /// `&self` and a `&Handle` argument both cross as the handle itself.
    fn deref_extern_handle(&self, ty: &Type) -> Type {
        let (Type::Reference(inner) | Type::MutReference(inner)) = ty else {
            return ty.clone();
        };
        let Type::Named(named) = inner.as_ref() else {
            return ty.clone();
        };
        match self.source_interface(named) {
            Some(source) if self.is_unrestricted_resource(&source, &named.name) => {
                inner.as_ref().clone()
            }
            _ => ty.clone(),
        }
    }

    /// The CM interface the reference at `named` resolves to, or `None` when
    /// no pass has answered for that site.
    #[must_use]
    pub fn source_interface(&self, named: &NamedType) -> Option<String> {
        self.source_interfaces.get(named.id)
    }

    /// Record the interface a reference site resolves to.
    pub fn set_source_interface(&self, site: AstId, interface: String) {
        self.source_interfaces.set(site, interface);
    }

    /// Merge a batch of site answers, first-writer-wins per site.
    pub fn extend_source_interfaces(&self, batch: SourceInterfaceBatch) {
        self.source_interfaces.extend(batch);
    }

    /// The canonical source interface owning `(kind, name)` — the `#[cm("…")]`
    /// fragment before the `#`, e.g. `"wasi:filesystem/types@0.3.0"`. `Some`
    /// only when exactly one interface declares the bare name under `kind`.
    pub fn bare_name_owner(&self, kind: CmTypeKind, name: &str) -> Option<&str> {
        find_unique_source_in(self.kind_keys(kind), name)
    }

    /// Extract the source interface (the part before the `#` fragment,
    /// e.g. `"wasi:cli/stdout@0.3.0"`) from the
    /// first `#[cm("...")]` attribute on a stdlib item. Returns an
    /// empty string if no attribute is present (user-authored items
    /// don't carry this).
    fn cm_source_interface(attrs: &[Attribute]) -> String {
        cm_import_of(attrs)
            .map(CmImport::interface_path)
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
    pub fn build_from_stdlib() -> (std::sync::Arc<Self>, std::sync::Arc<WorldRegistry>) {
        use std::sync::{Arc, OnceLock};

        static INSTANCE: OnceLock<(Arc<CmInterfaceRegistry>, Arc<WorldRegistry>)> = OnceLock::new();

        INSTANCE
            .get_or_init(|| {
                let (cm, world) = Self::build_from_stdlib_inner();
                (Arc::new(cm), Arc::new(world))
            })
            .clone()
    }

    fn build_from_stdlib_inner() -> (Self, WorldRegistry) {
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

        // Two-pass bootstrap so every stdlib `Type::Named` carries an exact
        // source interface. Pass 1 records each module's declared type names and
        // `#[cm(…)]` paths, so pass 2 can resolve a cross-module `use` whose
        // target parses after the importer; pass 2 then builds each module's
        // local name → source_interface map and walks its `Type` nodes.
        let mut modules: Vec<(&'static str, ast::Module)> = Vec::new();
        let mut defs_by_module: IndexMap<&'static str, IndexMap<String, String>> =
            IndexMap::default();
        let kiln = [
            "core:kiln/kiln_host.wado",
            "core:kiln/types.wado",
            "core:kiln/worlds.wado",
        ]
        .map(|path| {
            (
                path,
                stdlib::get_stdlib_module(path).expect("core:kiln is bundled"),
            )
        });
        for (path, source) in stdlib::all_binding_modules().iter().copied().chain(kiln) {
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
        let mut resolved_modules: Vec<(&'static str, ast::Module)> =
            Vec::with_capacity(modules.len());
        for (path, module) in modules {
            let local_names = build_local_name_resolver(path, &module, &defs_by_module);
            let sources = collect_named_type_sources(&module, &local_names);
            registry.extend_source_interfaces(sources);
            registry.register_module_decls(&module, &CmDeclScope::Module);
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
    fn register_module_decls(&mut self, module: &ast::Module, scope: &CmDeclScope) {
        use crate::ast::Item;

        // First, collect newtypes from this module
        for item in &module.items {
            if let Item::Newtype(alias) = item {
                let source_interface = Self::cm_source_interface(&alias.attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
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
                let source_interface = Self::cm_source_interface(&resource.attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
                // An unrestricted resource is erased at the boundary, which sees
                // the universal handle, a copyable `u32`, so it registers as a
                // newtype and every `own`/`borrow` path passes it by.
                if declares_unrestricted(&resource.attrs) {
                    self.unrestricted_resources
                        .insert((source_interface.clone(), resource.name.clone()), cm_name);
                    register_unique(
                        &mut self.newtypes,
                        "newtype",
                        source_interface,
                        resource.name.clone(),
                        extern_handle_type(resource.span),
                    );
                    continue;
                }
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
                if !scope.admits(&source_interface) {
                    continue;
                }
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
                if !scope.admits(&source_interface) {
                    continue;
                }
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
                if !scope.admits(&source_interface) {
                    continue;
                }
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
                if !scope.admits(&source_interface) {
                    continue;
                }
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

        self.register_interface_cm_methods(module, scope);

        // World-level function imports (Phase 9): a bodyless free function
        // carrying a `#[cm]` world-import boundary.
        for item in &module.items {
            if let Item::Function(func) = item
                && let Some(cm_func_name) = func
                    .attrs
                    .iter()
                    .find_map(|a| a.cm_boundary.as_ref().and_then(|b| b.as_world_import()))
            {
                let cm_param_names = extract_cm_params_attr(&func.attrs);
                let params: Vec<(String, String, Type)> = func
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let cm_name = cm_param_names
                            .get(i)
                            .unwrap_or_else(|| {
                                panic!(
                                    "missing #[cm_params] for param '{}' in world import {}",
                                    p.name, func.name
                                )
                            })
                            .clone();
                        (p.name.clone(), cm_name, self.cm_param_type(&p.ty))
                    })
                    .collect();
                // As for an interface method, the CM-ABI return type drops the
                // `AsyncCall<T>` wrapper user code keeps seeing.
                let return_type = unwrap_async_call_if_async(func.is_async, &func.return_type);
                self.register_world_import(
                    &func.name,
                    cm_func_name,
                    func.is_async,
                    params,
                    return_type,
                );
            }
        }

        // Register resource methods with resolved types for params but NOT for return type
        for item in &module.items {
            if let Item::Resource(resource) = item {
                let resource_source = Self::cm_source_interface(&resource.attrs);
                if !scope.admits(&resource_source) {
                    continue;
                }
                for method in &resource.methods {
                    if let Some(wasi) = cm_import_of(&method.attrs) {
                        // Extract CM param names from #[cm_params] attribute
                        let cm_param_names = extract_cm_params_attr(&method.attrs);
                        let params: Vec<(String, String, Type)> = method
                            .params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                let cm_name = match cm_param_names.get(i) {
                                    Some(declared) => declared.clone(),
                                    None => {
                                        scope.param_fallback(&p.name, &resource.name, &method.name)
                                    }
                                };
                                let ty = substitute_self_in_type(
                                    &self.source_interfaces,
                                    &p.ty,
                                    &resource.name,
                                    &resource_source,
                                );
                                (p.name.clone(), cm_name, self.cm_param_type(&ty))
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

    /// Register the `#[cm(…)]` declarations a user module makes, so a call to
    /// one lowers to that import instead of reaching WIR unresolved. An item
    /// without `#[cm(…)]` stays out: it is the module's own, not a binding.
    /// `Err` names an interface another module already declares.
    pub fn register_user_cm_decls(
        &mut self,
        module: &ast::Module,
        module_source: &ModuleSource,
    ) -> Result<(), String> {
        // The resolver keys cross-module `use` by path, and a user binding
        // module resolves its own declarations, so one placeholder key serves.
        const SELF_PATH: &str = "";
        let definitions = collect_cm_definitions(module);
        // A CM interface has one declaring module. Registering into one another
        // module already owns would overwrite that owner, leaving every type it
        // declares unresolvable, and collide in `register_unique`, which would
        // report user code as a stdlib bug.
        for source in definitions.values() {
            if let Some(existing) = self.existing_cm_decl(source) {
                return Err(format!(
                    "`{module_source}` declares a Component Model type in interface \
                     `{source}`, which already declares `{existing}`. An interface has \
                     one declaring module: bind an interface of your own instead."
                ));
            }
        }
        // Every interface this module binds maps back to it, as a component
        // dependency's and a lib entry's do. The type-id lookup starts from the
        // interface a type names and asks which module declares it, so without
        // this a record the module has just registered answers "no TypeId".
        for source in definitions.values() {
            self.cm_interface_module_sources
                .entry(source.clone())
                .or_insert_with(|| module_source.clone());
        }
        let mut defs_by_module: IndexMap<&'static str, IndexMap<String, String>> =
            IndexMap::default();
        defs_by_module.insert(SELF_PATH, definitions);
        let local_names = build_local_name_resolver(SELF_PATH, module, &defs_by_module);
        self.extend_source_interfaces(collect_named_type_sources(module, &local_names));
        self.register_module_decls(module, &CmDeclScope::CmAttributed);
        Ok(())
    }

    /// Check that every `#[cm(…)]` function name `module` binds is one the
    /// interface it names can export. It runs once every module is registered,
    /// since the `interface` naming a resource's operations need not be the
    /// module declaring that resource.
    ///
    /// `Err` describes the first name that names nothing. Left unchecked, the
    /// component validator rejects the emitted binary, which reaches the user
    /// as an internal compiler error.
    pub fn validate_cm_function_names(&self, module: &ast::Module) -> Result<(), String> {
        for (kind, import) in cm_imports(module) {
            if kind != CmBindingKind::Function {
                continue;
            }
            let Some(cm_func) = import.function.as_deref() else {
                continue;
            };
            let Some((_, receiver, _)) = parse_resource_func(cm_func) else {
                continue;
            };
            let iface = import.interface_path();
            if wado_name_under_cm_name(&self.resources, &iface, receiver).is_some() {
                continue;
            }
            let head = format!(
                "`{cm_func}` binds an operation of the Component Model resource `{receiver}`"
            );
            let unrestricted =
                wado_name_under_cm_name(&self.unrestricted_resources, &iface, receiver);
            return Err(match unrestricted {
                Some(wado_name) => format!(
                    "{head}, but `{wado_name}` is declared unrestricted, so it crosses the \
                     boundary as a plain handle and declares no resource to carry \
                     operations. Bind a plain function taking the handle as a parameter."
                ),
                None => format!("{head}, which interface `{iface}` does not declare."),
            });
        }
        Ok(())
    }

    /// Register every `#[cm(…)]` operation an `interface` in `module` declares.
    /// A param keeps its resolved type while the return type keeps the names as
    /// written, so a newtype survives the round trip.
    fn register_interface_cm_methods(&mut self, module: &ast::Module, scope: &CmDeclScope) {
        for item in &module.items {
            let Item::Interface(effect) = item else {
                continue;
            };
            for method in &effect.methods {
                let Some(wasi) = cm_import_of(&method.attrs) else {
                    continue;
                };
                let cm_param_names = extract_cm_params_attr(&method.attrs);
                let params: Vec<(String, String, Type)> = method
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let cm_name = match cm_param_names.get(i) {
                            Some(declared) => declared.clone(),
                            None => scope.param_fallback(&p.name, &effect.name, &method.name),
                        };
                        (p.name.clone(), cm_name, self.cm_param_type(&p.ty))
                    })
                    .collect();
                // Store the CM-ABI return type: an async import's `AsyncCall<T>`
                // wrapper is stripped here, and both the elaborator and the
                // binding synthesiser re-wrap it so user code still sees it.
                let return_type = unwrap_async_call_if_async(method.is_async, &method.return_type);
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

    /// Register a component dependency's binding module via the stdlib's
    /// `Self::register_module_decls` path, recording each interface FQ as a
    /// component import for [`crate::wir::ImportKind::Component`] classification.
    pub fn register_component_decls(
        &mut self,
        module: &ast::Module,
        interface_fqs: &[String],
        world_func_names: &[String],
        host_leaf_imports: &[String],
        module_source: &ModuleSource,
    ) {
        self.register_module_decls(module, &CmDeclScope::Module);
        for fq in interface_fqs {
            self.component_interfaces.insert(fq.clone());
            self.cm_interface_module_sources
                .insert(fq.clone(), module_source.clone());
            if !host_leaf_imports.is_empty() {
                self.component_host_leaf_imports
                    .insert(fq.clone(), host_leaf_imports.to_vec());
            }
        }
        for name in world_func_names {
            self.world_import_sources
                .insert(name.clone(), module_source.clone());
        }
    }

    /// Whether `fq` names an interface imported from a CM component dependency.
    #[must_use]
    pub fn is_component_interface(&self, fq: &str) -> bool {
        self.component_interfaces.contains(fq)
    }

    /// The host-leaf import FQs the component owning `fq` declares — the raw
    /// capabilities effect reconstruction unions into a consumer that uses this
    /// interface. Empty for a purely-computational component or a non-component
    /// interface. Ambient filtering is the consumer's responsibility.
    #[must_use]
    pub fn host_leaf_imports_for(&self, fq: &str) -> &[String] {
        self.component_host_leaf_imports
            .get(fq)
            .map_or(&[], Vec::as_slice)
    }

    /// Register a `--lib` entry module's own named types under the synthesized
    /// default-interface FQ. These carry no `#[cm(…)]` — they are the package's
    /// declarations, not WIT bindings — so CM names come from kebab-casing the
    /// Wado identifiers. Afterwards the export type plan and the CM type emitter
    /// resolve them through the registry exactly like WASI types.
    pub fn register_lib_local_decls(
        &mut self,
        module: &ast::Module,
        iface_fq: &str,
        entry_source: ModuleSource,
    ) {
        // Record the FQ -> entry ModuleSource so consumers resolve lib-local
        // types' module source (and detect lib-local provenance) without
        // prefix-sniffing the FQ.
        self.cm_interface_module_sources
            .insert(iface_fq.to_string(), entry_source.clone());

        let local_names = local_type_names(module.items.iter(), iface_fq);
        for item in &module.items {
            self.register_lib_local_item(item, iface_fq, &entry_source, &local_names);
        }
    }

    /// Register a `--lib` package's guest effect interfaces as CM imports, so an
    /// effect left unhandled at the library boundary lowers to a component
    /// import instead of an unresolved-call ICE. Mints a CM identity for a user
    /// `interface X { … }`: FQ `ns:pkg/<effect>@ver`, operation names kebabed.
    /// Errors when an effect's kebab name collides with the library's export.
    pub fn register_lib_guest_effect_imports(
        &mut self,
        interfaces: &[&InterfaceDecl],
        lib_fq: &str,
    ) -> Result<(), String> {
        let Some(base) = CmImport::parse(lib_fq) else {
            return Ok(());
        };
        let is_guest_effect = |effect: &InterfaceDecl| {
            !effect
                .methods
                .iter()
                .any(|m| cm_import_of(&m.attrs).is_some())
        };
        let collisions: Vec<&str> = interfaces
            .iter()
            .filter_map(|effect| {
                (is_guest_effect(effect) && to_kebab(&effect.name) == base.interface)
                    .then_some(effect.name.as_str())
            })
            .collect();
        if !collisions.is_empty() {
            return Err(format!(
                "guest effect interface `{}` collides with the library's own interface name \
                 `{}`; rename the effect",
                collisions.join("`, `"),
                base.interface
            ));
        }
        for effect in interfaces {
            if !is_guest_effect(effect) {
                continue;
            }
            let interface = to_kebab(&effect.name);
            for method in &effect.methods {
                let cm_import = CmImport {
                    namespace: base.namespace.clone(),
                    package: base.package.clone(),
                    interface: interface.clone(),
                    version: base.version.clone(),
                    function: Some(to_kebab(&method.name)),
                };
                let params: Vec<(String, String, Type)> = method
                    .params
                    .iter()
                    .map(|p| {
                        (
                            p.name.clone(),
                            to_kebab(&p.name),
                            resolve_type(&p.ty, &self.newtypes, &self.source_interfaces),
                        )
                    })
                    .collect();
                let return_type = unwrap_async_call_if_async(method.is_async, &method.return_type);
                self.register(
                    &effect.name,
                    &method.name,
                    &cm_import,
                    method.is_async,
                    params,
                    return_type,
                );
            }
        }
        Ok(())
    }

    /// Register named type decls that reach the library's public surface from a
    /// non-entry local module (a facade re-exports them, and an `export fn`
    /// signature names them). Registered under the same lib-local interface FQ
    /// as the entry module's own types, so lift/lower resolves them uniformly.
    /// Each carries the module that defines it, so resolution can locate a
    /// submodule type the entry-FQ mapping cannot.
    pub fn register_lib_local_items(&mut self, items: &[(ModuleSource, Item)], iface_fq: &str) {
        let local_names = local_type_names(items.iter().map(|(_, item)| item), iface_fq);
        for (source, item) in items {
            self.register_lib_local_item(item, iface_fq, source, &local_names);
        }
    }

    fn register_lib_local_item(
        &mut self,
        item: &Item,
        iface_fq: &str,
        module_source: &ModuleSource,
        local_names: &IndexMap<String, String>,
    ) {
        use crate::ast::Item;
        if let Some(name) = match item {
            Item::Newtype(d) => Some(&d.name),
            Item::Struct(d) => Some(&d.name),
            Item::Flags(d) => Some(&d.name),
            Item::Enum(d) => Some(&d.name),
            Item::Variant(d) => Some(&d.name),
            _ => None,
        } {
            self.lib_local_type_sources
                .insert(name.clone(), module_source.clone());
        }

        // Answer the source interface of every field / payload / base
        // reference this decl names, as the stdlib path does before
        // registration, so later CM resolution never needs a bare-name guess.
        let mut sources = SourceInterfaceBatch::default();
        collect_item_type_sources(&mut sources, item, local_names);
        self.extend_source_interfaces(sources);

        match item {
            Item::Newtype(alias) => register_unique(
                &mut self.newtypes,
                "newtype",
                iface_fq.to_string(),
                alias.name.clone(),
                alias.ty.clone(),
            ),
            Item::Struct(struct_def) => {
                let fields: Vec<(String, Type)> = struct_def
                    .fields
                    .iter()
                    .map(|f| (to_kebab(&f.name), f.ty.clone()))
                    .collect();
                let wado_fields: Vec<(String, String, Type)> = struct_def
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), to_kebab(&f.name), f.ty.clone()))
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
                        payload: c.payload.clone(),
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

    /// Register world declarations from a WASI module. `interface_decls`
    /// must already contain every `pub interface Foo` known to the
    /// compilation, so that interface exports (`export Foo;`) and interface
    /// imports (`import Foo;`) can be resolved to their CM FQ and method
    /// signatures.
    fn register_module_worlds(
        &self,
        module: &ast::Module,
        world_registry: &mut WorldRegistry,
        interface_decls: &InterfaceDeclTable,
    ) {
        use crate::ast::Item;

        for item in &module.items {
            if let Item::World(world) = item {
                world_registry.register(
                    world,
                    |name| {
                        interface_decls.export_lookup(name, &self.newtypes, &self.source_interfaces)
                    },
                    |name| interface_decls.import_cm_fq(name),
                );
            }
        }
    }

    // -- Strict, source-aware lookups --------------------------------------
    //
    // Keyed on `(interface, name)`, never falling back to a bare-name scan, so
    // a same-named type in another namespace cannot shadow a lookup. The caller
    // supplies the resolved source, which bootstrap populated on every stdlib
    // `Type::Named`. `.expect("<context>")` where the registration must exist.

    /// Newtype registered at `(interface, name)`, if any.
    pub fn get_newtype_by_source(&self, interface: &str, name: &DeclName) -> Option<&Type> {
        let name = name.as_decl_str();
        self.newtypes
            .get(&(interface.to_string(), name.to_string()))
    }

    /// Resource registered at `(interface, name)`; returns the CM kebab name.
    pub fn get_resource_cm_name_by_source(&self, interface: &str, name: &str) -> Option<&str> {
        self.resources
            .get(&(interface.to_string(), name.to_string()))
            .map(String::as_str)
    }

    // -- `ModuleSource`-bridged lookups ------------------------------------
    //
    // A `ResolvedType`'s `module_source` names its interface in the loader's
    // identity namespace, which differs from the `#[cm(...)]` registration key.
    // These accessors bridge both namespaces through a lazily-built O(1) index.

    /// The reverse index, built on first use once the registry is fully
    /// populated (before it is shared and queried during synthesis).
    fn module_index(&self) -> &ModuleSourceIndex {
        self.module_index.get_or_init(|| {
            let mut idx = ModuleSourceIndex::default();
            let insert =
                |dst: &mut IndexMap<(String, String), String>, src: &str, name: &str, cm: &str| {
                    dst.insert(
                        (interface_stem(src).to_string(), name.to_string()),
                        cm.to_string(),
                    );
                    if let Some(ms) = self.cm_interface_module_sources.get(src) {
                        dst.insert((ms.to_string(), name.to_string()), cm.to_string());
                    }
                };
            for ((src, name), cm) in &self.resources {
                insert(&mut idx.resources, src, name, cm);
            }
            for ((src, name), (cm, _, _)) in &self.structs {
                insert(&mut idx.structs, src, name, cm);
            }
            for ((src, name), (cm, _)) in &self.variants {
                insert(&mut idx.variants, src, name, cm);
            }
            for ((src, name), (cm, _)) in &self.enums {
                insert(&mut idx.enums, src, name, cm);
            }
            for ((src, name), (cm, _)) in &self.flags {
                insert(&mut idx.flags, src, name, cm);
            }
            idx
        })
    }

    /// Resource CM name for `name` declared in the interface identified by the
    /// coarse `module_source` string.
    pub fn get_resource_cm_name_by_module(&self, module_source: &str, name: &str) -> Option<&str> {
        lookup_by_module(&self.module_index().resources, module_source, name)
    }

    /// Struct CM name for `name` declared in `module_source`'s interface.
    pub fn get_struct_cm_name_by_module(&self, module_source: &str, name: &str) -> Option<&str> {
        lookup_by_module(&self.module_index().structs, module_source, name)
    }

    /// Variant CM name for `name` declared in `module_source`'s interface.
    pub fn get_variant_cm_name_by_module(&self, module_source: &str, name: &str) -> Option<&str> {
        lookup_by_module(&self.module_index().variants, module_source, name)
    }

    /// Enum CM name for `name` declared in `module_source`'s interface.
    pub fn get_enum_cm_name_by_module(&self, module_source: &str, name: &str) -> Option<&str> {
        lookup_by_module(&self.module_index().enums, module_source, name)
    }

    /// Flags CM name for `name` declared in `module_source`'s interface.
    pub fn get_flags_cm_name_by_module(&self, module_source: &str, name: &str) -> Option<&str> {
        lookup_by_module(&self.module_index().flags, module_source, name)
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

    // -- "Find in a bundled CM namespace" helpers for callers without AST
    // context.
    //
    // A handful of synthesis paths operate on a Wado-side type *name* that
    // arrived via resolved symbol info (not an AST `Type::Named` node), so
    // they cannot consult `self.source_interface(NamedType)`. These helpers walk
    // every [`CmNamespace`] interface and return the unique match; an ambiguous
    // name (the same type declared by two different bundled packages) returns
    // `None`, which the caller must propagate. Kiln and other non-binding
    // namespaces are intentionally excluded from this search.

    /// The `(source_interface, name)` keys `kind` registers.
    fn kind_keys(&self, kind: CmTypeKind) -> Box<dyn Iterator<Item = &(String, String)> + '_> {
        match kind {
            CmTypeKind::Newtype => Box::new(self.newtypes.keys()),
            CmTypeKind::Resource => Box::new(self.resources.keys()),
            CmTypeKind::Struct => Box::new(self.structs.keys()),
            CmTypeKind::Variant => Box::new(self.variants.keys()),
            CmTypeKind::Enum => Box::new(self.enums.keys()),
            CmTypeKind::Flags => Box::new(self.flags.keys()),
        }
    }

    /// Whether `kind` registers `name` under `interface_fq`.
    fn declares_kind(&self, kind: CmTypeKind, interface_fq: &str, name: &str) -> bool {
        let key = (interface_fq.to_string(), name.to_string());
        match kind {
            CmTypeKind::Newtype => self.newtypes.contains_key(&key),
            CmTypeKind::Resource => self.resources.contains_key(&key),
            CmTypeKind::Struct => self.structs.contains_key(&key),
            CmTypeKind::Variant => self.variants.contains_key(&key),
            CmTypeKind::Enum => self.enums.contains_key(&key),
            CmTypeKind::Flags => self.flags.contains_key(&key),
        }
    }

    /// The unique source interface registering `name` across every kind, among
    /// those `is_member` admits. Two kinds disagreeing is an ambiguity, not a race.
    fn find_unique_source_across_kinds(
        &self,
        name: &str,
        is_member: &dyn Fn(&str) -> bool,
    ) -> Option<&str> {
        let mut found: Option<&str> = None;
        for kind in CmTypeKind::ALL {
            let Some(src) = find_unique_source_in_set(self.kind_keys(kind), name, is_member) else {
                continue;
            };
            match found {
                None => found = Some(src),
                Some(prev) if prev != src => return None,
                Some(_) => {}
            }
        }
        found
    }

    /// The source interface of a `kind` declaration in a bundled CM namespace,
    /// when exactly one such interface registers the name.
    pub fn find_binding_source(&self, kind: CmTypeKind, name: &str) -> Option<&str> {
        find_unique_source_in_binding(self.kind_keys(kind), name)
    }

    /// The source interface of a `kind` declaration in `core:kiln/*`, when
    /// exactly one kiln interface registers the name. Kept apart from
    /// [`Self::find_binding_source`], whose bare-name WASI lookups stay scoped
    /// to `wasi:*` so `wasi:http/types::Response` and
    /// `core:kiln/types::Response` remain distinct.
    pub fn find_kiln_source(&self, kind: CmTypeKind, name: &str) -> Option<&str> {
        find_unique_source_with_prefix(self.kind_keys(kind), "core:kiln/", name)
    }

    /// The interface declaring the resource `wado_name`, asking `emitting`
    /// first. A user module is free to name a resource what a bundled interface
    /// also names one, and the bundled index then has no unique answer, so an
    /// emitter that knows which interface it is describing must say so.
    pub fn resource_source_in(&self, emitting: Option<&str>, wado_name: &str) -> Option<&str> {
        declaring_source(&self.resources, emitting, wado_name)
            .or_else(|| find_unique_source_in(self.resources.keys(), wado_name))
            .or_else(|| self.find_binding_source(CmTypeKind::Resource, wado_name))
    }

    /// The CM resources `ty` references at any depth, as
    /// `(declaring_interface, wado_name)`, asking `emitting` first. One walker:
    /// a second one drifts, and the shape it forgets loses an import where
    /// nothing looks.
    pub fn resources_in_type(
        &self,
        ty: &Type,
        emitting: Option<&str>,
        out: &mut IndexSet<(String, String)>,
    ) {
        match ty {
            Type::Named(named) => {
                if let Some(source) = self.cm_source_of_named_type(named, emitting)
                    && self
                        .get_resource_cm_name_by_source(&source, &named.name)
                        .is_some()
                {
                    out.insert((source, named.name.clone()));
                }
            }
            Type::Generic(g) => {
                for arg in &g.args {
                    self.resources_in_type(arg, emitting, out);
                }
            }
            Type::NamespacedGeneric(g) => {
                for arg in &g.args {
                    self.resources_in_type(arg, emitting, out);
                }
            }
            Type::Tuple(elems) => {
                for elem in elems {
                    self.resources_in_type(elem, emitting, out);
                }
            }
            Type::Reference(inner) | Type::MutReference(inner) => {
                self.resources_in_type(inner, emitting, out);
            }
            Type::Function(_) | Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => {}
        }
    }

    /// The CM resources every signature of `funcs` references, as
    /// `(declaring_interface, wado_name)`, for the interface `emitting`.
    pub fn resources_in_signatures(
        &self,
        funcs: &[&CmFunctionInfo],
        emitting: Option<&str>,
    ) -> IndexSet<(String, String)> {
        let mut out = IndexSet::default();
        for func in funcs {
            if let Some(ret) = &func.return_type {
                self.resources_in_type(ret, emitting, &mut out);
            }
            for (_, _, ty) in &func.params {
                self.resources_in_type(ty, emitting, &mut out);
            }
        }
        out
    }

    /// The interface declaring the flags `wado_name`, asking `emitting` first.
    /// See [`Self::resource_source_in`].
    pub fn flags_source_in(&self, emitting: Option<&str>, wado_name: &str) -> Option<&str> {
        declaring_source(&self.flags, emitting, wado_name)
            .or_else(|| self.find_binding_source(CmTypeKind::Flags, wado_name))
    }

    /// The owning interface FQ of a component-imported named type, when exactly
    /// one composed dependency interface declares `name`. Mirrors the
    /// `find_binding_*` fallbacks for a type re-exported from a CM component
    /// dependency (whose FQ carries no `wasi:` / `core:kiln/` prefix). `None` if
    /// no or more than one dependency interface declares `name`, across all type
    /// kinds — so a struct in one dependency and an enum in another are
    /// ambiguous, not silently resolved to the first kind.
    pub fn find_component_type_source(&self, name: &str) -> Option<&str> {
        self.find_unique_source_across_kinds(name, &|s| self.component_interfaces.contains(s))
    }

    /// The `ModuleSource` a CM interface FQ was registered under — the entry
    /// module for a `--lib` local, or the dependency `ModuleSource::Wasm` for a
    /// component import. `None` for a WASI/core interface (whose source is
    /// derived from the FQ by naming convention) or an unknown FQ.
    pub fn cm_interface_module_source_of(&self, iface_fq: &str) -> Option<&ModuleSource> {
        self.cm_interface_module_sources.get(iface_fq)
    }

    /// The CM interfaces `module` registers. A component exports as many as it
    /// likes through the one binding module the loader synthesizes for it.
    fn module_interfaces(&self, module: &ModuleSource) -> Vec<&str> {
        let recorded: Vec<&str> = self
            .cm_interface_module_sources
            .iter()
            .filter(|(_, source)| *source == module)
            .map(|(fq, _)| fq.as_str())
            .collect();
        if !recorded.is_empty() {
            return recorded;
        }
        // A bundled `wasi:` / `core:` interface is absent from the map by
        // design, and its module path is its own interface FQ, so the two
        // populations are disjoint rather than a fallback.
        let (namespace, path) = match module {
            ModuleSource::Binding {
                namespace,
                interface,
            } => (Some(*namespace), interface.as_str()),
            ModuleSource::Core { name } => (None, name.as_str()),
            _ => return Vec::new(),
        };
        let stem = path.strip_suffix(".wado").unwrap_or(path);
        let wanted = (namespace, format!("{stem}.wado"));
        let bundled: IndexSet<&str> = self
            .interfaces
            .keys()
            .map(String::as_str)
            .chain(self.resources.keys().map(|(fq, _)| fq.as_str()))
            .filter(|fq| cm_interface_module(fq).as_ref() == Some(&wanted))
            .collect();
        bundled.into_iter().collect()
    }

    /// A type `iface_fq` already declares, if any. A bundled `wasi:` / `core:`
    /// interface is absent from `cm_interface_module_sources` by design, so that
    /// map alone reports the whole stdlib as unowned; what the keyspaces hold is
    /// the fact itself.
    fn existing_cm_decl(&self, iface_fq: &str) -> Option<&str> {
        CmTypeKind::ALL
            .into_iter()
            .find_map(|kind| first_name_in_interface(self.kind_keys(kind), iface_fq))
    }

    /// The module that defines a lib-local named type, when known. Locates a
    /// submodule-defined public type (e.g. `HeadingInfo` inside a facade
    /// library) that the interface-FQ mapping alone cannot.
    pub fn lib_local_type_source(&self, name: &str) -> Option<&ModuleSource> {
        self.lib_local_type_sources.get(name)
    }

    /// The source interface of a named type: its own where the reference carries
    /// one, else the bare-name search [WEP: Declaration Identity] calls a gap.
    ///
    /// [WEP: Declaration Identity]: ../../docs/wep-2026-08-12-declaration-identity.md
    pub fn resolve_cm_source_for(&self, named: &NamedType) -> Option<String> {
        if let Some(s) = self.source_interface(named) {
            return Some(s);
        }
        self.find_kiln_source(CmTypeKind::Struct, &named.name)
            .or_else(|| self.find_kiln_source(CmTypeKind::Variant, &named.name))
            .or_else(|| self.find_kiln_source(CmTypeKind::Enum, &named.name))
            // Lib-local types (`wado compile --lib`) are registered by name
            // under the package's default-interface FQ (neither `wasi:` nor
            // `core:`), so the prefix-scoped lookups above miss them. A bare
            // by-name scan resolves them when the name is unambiguous.
            .or_else(|| self.find_unique_source_across_kinds(&named.name, &|src| !src.is_empty()))
            .map(str::to_string)
    }

    /// Resolve a named type to a source interface within `namespace_prefix`
    /// (e.g. `"core:kiln/"`, `"wasi:http/"`). Used to scope a world export's
    /// type to the world's own package, so a name that also exists in another
    /// package (e.g. `Response` in both `wasi:http` and `core:kiln`) resolves
    /// to the world's own. Returns `None` for an empty prefix or no unique match.
    pub fn resolve_cm_source_with_prefix<'a>(
        &'a self,
        named: &NamedType,
        namespace_prefix: &str,
    ) -> Option<&'a str> {
        if namespace_prefix.is_empty() {
            return None;
        }
        CmTypeKind::ALL.into_iter().find_map(|kind| {
            find_unique_source_with_prefix(self.kind_keys(kind), namespace_prefix, &named.name)
        })
    }

    /// Resolve a newtype *reference* to its aliased base type by its declaring
    /// interface (`NamedType::source_interface`), so two modules that declare a
    /// same-named newtype resolve to their own base — never a bare-name guess.
    /// A source-less reference is not a known newtype here.
    fn resolve_newtype_ref(&self, named: &NamedType) -> Option<&Type> {
        let source = self.source_interface(named)?;
        self.get_newtype_by_source(&source, &DeclName::new(&named.name))
    }

    /// The registration source and base type of a *local* newtype — one
    /// declared in the compiled package, not imported from a CM interface.
    /// `None` for CM-imported or source-less references. The CM codegen emits a
    /// local newtype as a named alias at the boundary (issue #1456) rather than
    /// erasing it to its base; the returned source keys that alias so references
    /// to one newtype never double-emit.
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

    /// The CM interface a named type belongs to: its recorded source, else the
    /// emitting interface when that one declares the name, else the bundled
    /// binding that declares it, else a component dependency's re-export. The
    /// one chain codegen resolves a reference by.
    pub fn cm_source_of_named_type(
        &self,
        named: &NamedType,
        interface_hint: Option<&str>,
    ) -> Option<String> {
        let name = &named.name;
        let declared_at_hint = interface_hint
            .filter(|hint| self.declares_wado_name(hint, name))
            .map(str::to_string);
        self.source_interface(named)
            .filter(|source| self.is_cm_source(source))
            .or(declared_at_hint)
            .or_else(|| {
                self.find_unique_source_across_kinds(name, &|src| {
                    CmNamespace::split_specifier(src).is_some()
                })
                .map(str::to_string)
            })
            .or_else(|| self.find_component_type_source(name).map(str::to_string))
    }

    /// The CM name of the `ErrorCode` that `interface_path` declares itself, in
    /// either shape: a variant (`wasi:filesystem/types`, `wasi:sockets/types`)
    /// or an enum (`wasi:cli/types`). Exact: an interface that declares none
    /// answers `None` rather than borrowing another package's.
    pub fn own_error_code_cm_name(&self, interface_path: &str) -> Option<&str> {
        self.get_variant_cm_name_by_source(interface_path, ERROR_CODE_WADO_NAME)
            .or_else(|| self.get_enum_cm_name_by_source(interface_path, ERROR_CODE_WADO_NAME))
    }

    /// Whether `interface_path` declares its own `ErrorCode`. Codegen picks the
    /// interface-local error type over an alias to the shared CLI one on this.
    pub fn declares_own_error_code(&self, interface_path: &str) -> bool {
        self.own_error_code_cm_name(interface_path).is_some()
    }

    /// Keying on the declaration's own module source keeps a user type from
    /// being confused with a same-named WASI or dependency declaration.
    ///
    /// A `--lib` entry declaration is registered under the package default
    /// interface, whose source [`Self::register_lib_local_decls`] maps to the entry
    /// module; outside `--lib` it is registered nowhere, so this is `false` and
    /// the payload has no CM type to lower against.
    pub fn is_named_type_registered_from(&self, source: &ModuleSource, name: &str) -> bool {
        self.interface_declaring(source, name).is_some()
    }

    /// Whether `interface_fq` declares `name`, in any of the kinds a reference
    /// at the CM boundary can name.
    pub fn declares_wado_name(&self, interface_fq: &str, name: &str) -> bool {
        CmTypeKind::ALL
            .into_iter()
            .any(|kind| self.declares_kind(kind, interface_fq, name))
    }

    /// The interface declaring `name` among those `source` registers. Keying by
    /// the module is what keeps a bundled interface spelling `name` out of it.
    pub fn interface_declaring(&self, source: &ModuleSource, name: &str) -> Option<&str> {
        self.module_interfaces(source)
            .into_iter()
            .find(|fq| self.declares_wado_name(fq, name))
    }

    /// The interface exporting `cm_name` among those `module` registers, for a
    /// consumer holding a [`crate::canonical::CmDecl`] rather than a Wado name.
    pub fn interface_declaring_cm_name(
        &self,
        module: &ModuleSource,
        cm_name: &str,
    ) -> Option<&str> {
        self.module_interfaces(module)
            .into_iter()
            .find(|fq| self.exports_cm_name(fq, cm_name))
    }

    fn exports_cm_name(&self, iface_fq: &str, cm_name: &str) -> bool {
        // A newtype is peeled before it reaches the boundary, so the kinds
        // recording the name the ABI spells are the kinds that can be asked for.
        spells_cm_name(&self.resources, iface_fq, cm_name, |cm| cm)
            || spells_cm_name(&self.structs, iface_fq, cm_name, |(cm, ..)| cm)
            || spells_cm_name(&self.variants, iface_fq, cm_name, |(cm, _)| cm)
            || spells_cm_name(&self.enums, iface_fq, cm_name, |(cm, _)| cm)
            || spells_cm_name(&self.flags, iface_fq, cm_name, |(cm, _)| cm)
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
                // An extern-handle handle registers here to reach the `u32` every
                // boundary path lowers it to, but it names no CM type: the WIT
                // spells the handle inline, so no alias declares it.
                if source.starts_with(interface_prefix)
                    && !self.is_unrestricted_resource(source, name)
                {
                    Some((name.as_str(), ty))
                } else {
                    None
                }
            })
    }

    /// The resource named by a return type, if it is an `Option<Resource>`.
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
            && let Some(source) = self.source_interface(inner)
            && let Some(cm_name) = self.get_resource_cm_name_by_source(&source, &inner.name)
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

        // Check all parameter types, at the boundary's view: an extern handle is a `u32`.
        for (_, _, ty) in &func.params {
            let resolved = self.resolve_type(ty);
            if !is_param_type_supported_with_types(&resolved, &enums, &resources, &structs) {
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

        // Params carry their value types: newtypes peeled, extern handles kept,
        // so a binding's GC-level types match the caller's.
        let resolved_params: Vec<(String, String, Type)> = params
            .into_iter()
            .map(|(name, cm_name, ty)| (name, cm_name, self.value_type(&ty)))
            .collect();
        let func_info = CmFunctionInfo {
            namespace: wasi.namespace.clone(),
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

    /// Register a world-level function import (Phase 9), keyed by its bare name
    /// (empty interface). Stored like an interface method so the adapter
    /// pipeline reuses the interface-method path.
    pub fn register_world_import(
        &mut self,
        func_name: &str,
        cm_func_name: &str,
        is_async: bool,
        params: Vec<(String, String, Type)>,
        return_type: Option<Type>,
    ) {
        let resolved_params: Vec<(String, String, Type)> = params
            .into_iter()
            .map(|(name, cm_name, ty)| (name, cm_name, self.value_type(&ty)))
            .collect();
        let func_info = CmFunctionInfo {
            // A world import sits above any interface, so it has no namespace,
            // package or interface path to name: its alias is a bare key.
            namespace: String::new(),
            interface_name: String::new(),
            method_name: func_name.to_string(),
            wasi_func_name: cm_func_name.to_string(),
            interface_path: String::new(),
            package: String::new(),
            is_async,
            params: resolved_params,
            return_type,
        };
        let local_name = func_info.local_alias_name();
        self.used_names.insert(local_name.clone());
        self.local_aliases
            .insert(local_name, (String::new(), cm_func_name.to_string()));
        self.effect_to_func.insert(func_name.to_string(), func_info);
        self.world_import_functions.insert(func_name.to_string());
    }

    /// Whether `name` is a world-level function import (Phase 9).
    #[must_use]
    pub fn is_world_import_function(&self, name: &DeclPath) -> bool {
        let name = name.as_decl_str();
        self.world_import_functions.contains(name)
    }

    /// Every world-level function import, as `(bare_name, info)`.
    pub fn world_import_functions(&self) -> impl Iterator<Item = (&str, &CmFunctionInfo)> + '_ {
        self.world_import_functions
            .iter()
            .filter_map(|name| Some((name.as_str(), self.effect_to_func.get(name)?)))
    }

    /// The dependency `ModuleSource` exporting world-level function `name`.
    #[must_use]
    pub fn world_import_source(&self, name: &str) -> Option<&ModuleSource> {
        self.world_import_sources.get(name)
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
    /// Keyed in the declaration namespace — `Resource::method` as the WIT
    /// declares it. A mangled name carries the declaring module, which this
    /// registry never stores, so the key type says which one is wanted.
    pub fn get_function(&self, name: &DeclPath) -> Option<&CmFunctionInfo> {
        self.effect_to_func.get(name.as_decl_str())
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
            if let Some(wasi) = CmImport::parse(path) {
                wasi.interface == interface_name
            } else {
                false
            }
        })
    }

    /// Whether `source` names an interface whose values follow the CM canonical
    /// ABI: a bundled CM namespace (`wasi:`, `web:`, `core:kiln/`) or a
    /// component import (whose package namespace is arbitrary, so tracked
    /// explicitly).
    pub fn is_cm_source(&self, source: &str) -> bool {
        CmNamespace::split_specifier(source).is_some()
            || source.starts_with("core:kiln/")
            || self.component_interfaces.contains(source)
    }

    /// Flatten a CM type into its canonical-ABI core value sequence. Single
    /// source of truth for how a value-type maps to flat core params/results.
    pub fn cm_flatten(&self, ty: &Type) -> Vec<CmValType> {
        let mut out = Vec::new();
        self.cm_flatten_into(ty, &mut out);
        out
    }

    fn cm_flatten_into(&self, ty: &Type, out: &mut Vec<CmValType>) {
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
                    if let Some(source) = self.resolve_cm_source_for(named) {
                        if let Some(fields) = self
                            .get_struct_fields_by_source(&source, name)
                            .map(<[(String, Type)]>::to_vec)
                        {
                            for (_, field_ty) in &fields {
                                self.cm_flatten_into(field_ty, out);
                            }
                            return;
                        }
                        if let Some(cases) = self
                            .get_variant_cases_by_source(&source, name)
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
        out: &mut Vec<CmValType>,
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

    /// Every local name bound to one CM function. More than one when a user
    /// module binds an operation the stdlib already binds: each Wado name mints
    /// its own alias, and all of them must reach the same import.
    pub fn local_names_for(
        &self,
        interface_path: &str,
        wasi_func_name: &str,
    ) -> impl Iterator<Item = &String> {
        self.local_aliases
            .iter()
            .filter(move |(_, (path, func))| path == interface_path && func == wasi_func_name)
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
        self.resolve_type_impl(ty, false, false)
    }

    /// The type a value of `ty` has in the guest: newtypes peel to their base,
    /// but an extern-handle resource stays itself. Its handle is the resource's
    /// own representation, and `Option<Node>` is a GC type of its own, not
    /// `Option<u32>`; [`Self::resolve_type`] is the boundary's view.
    pub fn value_type(&self, ty: &Type) -> Type {
        self.resolve_type_impl(ty, false, true)
    }

    /// Like [`Self::resolve_type`], but a *local* newtype (no `#[cm(...)]`
    /// source) is kept as its named reference instead of peeled to its base.
    /// The CM codegen then emits it as a named type alias (`type meters = f64`)
    /// so the compiled component's structural type matches `wado wit`
    /// (issue #1456). WASI/CM-imported newtypes still resolve through.
    pub fn resolve_type_preserving_local_newtypes(&self, ty: &Type) -> Type {
        self.resolve_type_impl(ty, true, false)
    }

    fn resolve_type_impl(&self, ty: &Type, preserve_local: bool, keep_handles: bool) -> Type {
        match ty {
            Type::Named(named) => {
                let source = self.source_interface(named);
                let kept = (preserve_local
                    && self
                        .local_newtype_base(source.as_deref(), &named.name)
                        .is_some())
                    || (keep_handles
                        && source
                            .as_deref()
                            .is_some_and(|s| self.is_unrestricted_resource(s, &named.name)));
                if kept {
                    ty.clone()
                } else if let Some(aliased_ty) = self.resolve_newtype_ref(named) {
                    // Recursively resolve the aliased type
                    self.resolve_type_impl(aliased_ty, preserve_local, keep_handles)
                } else {
                    ty.clone()
                }
            }
            Type::Generic(generic) => {
                // Resolve type arguments recursively
                let resolved_args: Vec<Type> = generic
                    .args
                    .iter()
                    .map(|arg| self.resolve_type_impl(arg, preserve_local, keep_handles))
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
                    .map(|t| self.resolve_type_impl(t, preserve_local, keep_handles))
                    .collect();
                Type::Tuple(resolved)
            }
            Type::Reference(inner) => Type::Reference(Box::new(self.resolve_type_impl(
                inner,
                preserve_local,
                keep_handles,
            ))),
            Type::MutReference(inner) => Type::MutReference(Box::new(self.resolve_type_impl(
                inner,
                preserve_local,
                keep_handles,
            ))),
            Type::Function(func_ty) => {
                // For function types, resolve params and return type
                let resolved_params: Vec<Type> = func_ty
                    .params
                    .iter()
                    .map(|t| self.resolve_type_impl(t, preserve_local, keep_handles))
                    .collect();
                let resolved_return =
                    self.resolve_type_impl(&func_ty.return_type, preserve_local, keep_handles);
                Type::Function(Box::new(FunctionType {
                    is_mut: func_ty.is_mut,
                    params: resolved_params,
                    return_type: resolved_return,
                    effects: func_ty.effects.clone(),
                    effect_ids: func_ty.effect_ids.clone(),
                }))
            }
            // NamespacedGeneric types (like `ns::Type<T>`) are passed through
            Type::NamespacedGeneric(ng) => {
                let resolved_args: Vec<Type> = ng
                    .args
                    .iter()
                    .map(|arg| self.resolve_type_impl(arg, preserve_local, keep_handles))
                    .collect();
                Type::NamespacedGeneric(Box::new(NamespacedGenericType {
                    id: ng.id,
                    namespace: ng.namespace.clone(),
                    name: ng.name.clone(),
                    name_span: ng.name_span,
                    args: resolved_args,
                    span: ng.span,
                }))
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

/// [`CmTypeSink`] that records the names a type walk would export, emitting
/// nothing — so a caller can ask which names a signature puts into an
/// interface's namespace through the same walk codegen uses to put them there.
#[derive(Default)]
pub struct CmNameSink {
    names: Vec<String>,
    next_idx: u32,
}

impl CmNameSink {
    pub fn names(&self) -> &[String] {
        &self.names
    }

    fn alloc(&mut self) -> u32 {
        let idx = self.next_idx;
        self.next_idx += 1;
        idx
    }
}

impl CmTypeSink for CmNameSink {
    fn define(&mut self, _defined: CmDefined<'_>) -> u32 {
        self.alloc()
    }

    fn name(&mut self, cm_name: &str, _idx: u32) -> u32 {
        self.names.push(cm_name.to_string());
        self.alloc()
    }
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
    /// The CM names this engine has exported into its sink, so a caller can ask
    /// what the walk put into the interface's namespace instead of predicting it.
    named_exports: IndexSet<String>,
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
            named_exports: IndexSet::default(),
        }
    }

    /// Create a new generator with an interface hint for type disambiguation.
    pub fn with_interface_hint(interface_hint: &str) -> Self {
        Self {
            cache: IndexMap::default(),
            interface_hint: Some(interface_hint.to_string()),
            named_exports: IndexSet::default(),
        }
    }

    pub fn interface_hint(&self) -> Option<&str> {
        self.interface_hint.as_deref()
    }

    /// Whether this engine exported `cm_name` into its sink's namespace. An
    /// alias out of the built instance asks this rather than re-deriving from
    /// the registry what the walk decided to emit.
    pub fn exported(&self, cm_name: &str) -> bool {
        self.named_exports.contains(cm_name)
    }

    /// Export `idx` under `cm_name`, recording the name for [`Self::exported`].
    fn export_named(&mut self, sink: &mut dyn CmTypeSink, cm_name: &str, idx: u32) -> u32 {
        self.named_exports.insert(cm_name.to_string());
        sink.name(cm_name, idx)
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
        let export_idx = self.export_named(sink, cm_name, variant_idx);

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
        let export_idx = self.export_named(sink, cm_name, record_idx);

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
                    // Preserve a local newtype as a named CM alias
                    // (`type meters = f64`) so the structural type matches
                    // `wado wit` instead of erasing to its base (issue #1456).
                    let source = cm_interface_registry.source_interface(named);
                    if let Some((canonical_source, base)) = cm_interface_registry
                        .local_newtype_base(source.as_deref(), name)
                        .map(|(s, ty)| (s.to_string(), ty.clone()))
                    {
                        // Canonical source keys the cache so same-named locals
                        // stay distinct and every reference dedups to one alias.
                        let cache_key = format!("newtype:{canonical_source}:{name}");
                        if let Some(&idx) = self.cache.get(&cache_key) {
                            return ComponentValType::Type(idx);
                        }
                        // Peel the base first: an imported-newtype base resolves
                        // to its primitive, avoiding the unsupported-name panic
                        // below; a local-newtype base recurses as a nested alias.
                        let base =
                            cm_interface_registry.resolve_type_preserving_local_newtypes(&base);
                        let base_val = self.ast_type_to_cm(
                            sink,
                            &base,
                            cm_interface_registry,
                            resource_exports,
                        );
                        let base_idx = match base_val {
                            ComponentValType::Primitive(prim) => {
                                sink.define(CmDefined::Primitive(prim))
                            }
                            ComponentValType::Type(idx) => idx,
                        };
                        let named_idx = self.export_named(sink, &to_kebab(name), base_idx);
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
                    let source_owned: String = cm_interface_registry
                        .cm_source_of_named_type(named, self.interface_hint.as_deref())
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
                        let export_idx = *resource_exports.get(cm_name).unwrap_or_else(|| {
                            panic!(
                                "resource `{cm_name}` from `{source}` is not among the resources \
                                 this instance declares ({}), so a handle to it has no type here",
                                resource_exports
                                    .keys()
                                    .copied()
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        });
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
                                cm_interface_registry.get_variant_cases_by_source(h, name)
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
                                cm_interface_registry.get_variant_cm_name_by_source(h, name)
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
                            let export_idx = self.export_named(sink, cm_name, idx);
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
                            let export_idx = self.export_named(sink, cm_name, idx);
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
                    && let Some(source) = cm_interface_registry
                        .source_interface(n)
                        .filter(|s| s.starts_with("wasi:"))
                        .or_else(|| {
                            cm_interface_registry
                                .find_binding_source(CmTypeKind::Resource, &n.name)
                                .map(str::to_string)
                        })
                    && let Some(cm_name) =
                        cm_interface_registry.get_resource_cm_name_by_source(&source, &n.name)
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
                    let is_ok_unit = generic.args[0].is_unit();
                    let is_err_unit = generic.args[1].is_unit();
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
            Type::Tuple(elems) if elems.is_empty() => unreachable!("{EMPTY_TUPLE_AT_BOUNDARY}"),
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

pub fn cm_val_type_to_val_type(v: CmValType) -> ValType {
    match v {
        CmValType::I32 => ValType::I32,
        CmValType::I64 => ValType::I64,
        CmValType::F32 => ValType::F32,
        CmValType::F64 => ValType::F64,
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
            ) && !generic.args.iter().any(mentions_empty_tuple)
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            // borrow<resource> - passed as i32 handle at CM boundary
            if let Type::Named(named) = inner.as_ref() {
                resources.contains(named.name.as_str())
            } else {
                false
            }
        }
        Type::Tuple(elems) if !elems.is_empty() => elems
            .iter()
            .all(|e| is_param_type_supported_with_types(e, enums, resources, structs)),
        _ => false,
    }
}

/// Whether `ty` names the empty tuple anywhere, including under a generic. The
/// shape predicates ask this where they accept a generic without judging its
/// arguments, so a payload of `[]` cannot ride in past them.
fn mentions_empty_tuple(ty: &Type) -> bool {
    match ty {
        Type::Tuple(elems) => elems.is_empty() || elems.iter().any(mentions_empty_tuple),
        Type::Generic(generic) => generic.args.iter().any(mentions_empty_tuple),
        Type::Reference(inner) | Type::MutReference(inner) => mentions_empty_tuple(inner),
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
                "Stream" | "Future" => !generic.args.iter().any(mentions_empty_tuple),
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
        Type::Tuple(elements) if !elements.is_empty() => elements
            .iter()
            .all(|el| is_return_type_supported_with_types(el, enums, resources, structs)),
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

/// The Wado name every CM `error-code` is declared under. The registry is keyed
/// by it, so it is what reaches the declaration.
pub const ERROR_CODE_WADO_NAME: &str = "ErrorCode";

/// The Wado name of the HTTP fields resource a trailers future carries.
/// `Trailers` is a newtype over it, so a component type points at the resource.
pub const FIELDS_WADO_NAME: &str = "Fields";

/// The Wado name of the HTTP response resource a handler result carries.
pub const RESPONSE_WADO_NAME: &str = "Response";

/// The interface declaring the canonical `error-code` that WASI WIT writes as
/// the error arm of a bare `result<_, error-code>`.
pub const CANONICAL_ERROR_CODE_INTERFACE: &str = "wasi:cli/types";

/// What every boundary path says when it meets the empty tuple, which none of
/// them does.
pub const EMPTY_TUPLE_AT_BOUNDARY: &str = "the empty tuple `[]` has no Component Model \
     representation, and the boundary rejects one before synthesis";

/// Whether a return type must use an outptr rather than flat core results. The
/// flat count is the single rule: a type returns via the outptr iff it flattens
/// to more than `MAX_FLAT_RESULTS` core values, so a single-field record
/// (`{ n: u64 }` -> `[i64]`) returns flat. Shared by the import-binding
/// synthesizer and the core functype builder so the two never disagree.
pub fn cm_return_needs_outptr(ty: &Type, registry: &CmInterfaceRegistry) -> bool {
    registry.cm_flatten(ty).len() > MAX_FLAT_RESULTS
}

/// The CM canonical-ABI size and alignment of a registered variant, `None` for
/// a type that is not one.
fn cm_variant_size_align(named: &NamedType, registry: &CmInterfaceRegistry) -> Option<(u32, u32)> {
    let source = registry.resolve_cm_source_for(named)?;
    let cases = registry.get_variant_cases_by_source(&source, &named.name)?;
    let payloads = cases.iter().filter_map(|case| case.payload.as_ref());
    Some(layout_variant_with_registry(cases.len(), payloads, registry).size_align())
}

/// Registry-aware CM canonical ABI size and alignment. A named declaration
/// resolves through the registry rather than defaulting to the 4-byte handle.
pub fn cm_layout_with_registry(ty: &Type, registry: &CmInterfaceRegistry) -> (u32, u32) {
    let unregistered = || plain_size_align(ty);
    match ty {
        Type::Named(named) => {
            let Some(source) = registry.resolve_cm_source_for(named) else {
                return unregistered();
            };
            if let Some(resolved) =
                registry.get_newtype_by_source(&source, &DeclName::new(&named.name))
            {
                return cm_layout_with_registry(resolved, registry);
            }
            if let Some(fields) = registry.get_struct_fields_by_source(&source, &named.name) {
                let resolved_fields: Vec<Type> = fields
                    .iter()
                    .map(|(_, ty)| registry.resolve_type(ty))
                    .collect();
                return layout_fields_with_registry(resolved_fields.iter(), registry).size_align();
            }
            if let Some(sa) = cm_variant_size_align(named, registry) {
                return sa;
            }
            if let Some(variants) = registry.get_enum_variants_by_source(&source, &named.name) {
                let disc = cm_discriminant_byte_size(variants.len());
                return (disc, disc);
            }
            if let Some(members) = registry.get_flags_members_by_source(&source, &named.name) {
                return (
                    cm_flags_byte_size(members.len()),
                    cm_flags_byte_align(members.len()),
                );
            }
            unregistered()
        }
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => {
                layout_option_with_registry(&g.args[0], registry).size_align()
            }
            "Result" if g.args.len() == 2 => {
                layout_result_with_registry(&g.args[0], &g.args[1], registry).size_align()
            }
            _ => unregistered(),
        },
        Type::Tuple(elems) if !elems.is_empty() => {
            layout_tuple_with_registry(elems, registry).size_align()
        }
        _ => unregistered(),
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
    use crate::ast::{AstId, GenericType, NamedType};
    use crate::lexer::lex;
    use crate::parser;
    use crate::token::Span;
    use std::assert_matches;

    fn make_span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    /// One binding module, registered the way [`CmInterfaceRegistry::build_from_stdlib`]
    /// registers each of its own.
    fn registry_from(module_path: &'static str, source: &str) -> CmInterfaceRegistry {
        let lexed = lex(source);
        assert!(lexed.errors.is_empty(), "lexer error: {:?}", lexed.errors);
        let module = parser::Parser::new(lexed.tokens)
            .parse_strict()
            .expect("parser error");
        let mut defs_by_module: IndexMap<&'static str, IndexMap<String, String>> =
            IndexMap::default();
        defs_by_module.insert(module_path, collect_cm_definitions(&module));
        let local_names = build_local_name_resolver(module_path, &module, &defs_by_module);
        let mut registry = CmInterfaceRegistry::new();
        registry.extend_source_interfaces(collect_named_type_sources(&module, &local_names));
        registry.register_module_decls(&module, &CmDeclScope::Module);
        registry
    }

    /// The registered parameter types of `interface::method`.
    fn param_types(registry: &CmInterfaceRegistry, interface: &str, method: &str) -> Vec<Type> {
        registry
            .interfaces()
            .flat_map(|info| info.functions)
            .find(|f| f.interface_name == interface && f.method_name == method)
            .unwrap_or_else(|| panic!("{interface}::{method} was never registered"))
            .params
            .into_iter()
            .map(|(_, _, ty)| ty)
            .collect()
    }

    /// An extern-handle is a value, so `&Handle` crosses as the handle itself
    /// wherever it is written. A `Type::Reference` surviving here reaches
    /// `codegen::component`, which has no borrow type to lower it to and panics.
    #[test]
    fn an_extern_handle_argument_is_peeled_outside_a_resource_method() {
        let registry = registry_from(
            "web:dom",
            r#"
            #[cm("web:dom/handle", linearity = "unrestricted")]
            pub resource Handle {
                #[cm("web:dom/handle#sibling")]
                #[cm_params("self", "other")]
                fn sibling(&self, other: &Handle) -> Handle;
            }

            #[cm("web:dom/global")]
            pub interface Dom {
                #[cm("web:dom/global#adopt")]
                #[cm_params("node")]
                fn adopt(node: &Handle) -> Handle;
            }
            "#,
        );
        for (interface, method) in [("Handle", "sibling"), ("Dom", "adopt")] {
            for ty in param_types(&registry, interface, method) {
                assert_matches!(
                    ty,
                    Type::Named(n) if n.name == "u32",
                    "{interface}::{method} keeps a handle behind a reference"
                );
            }
        }
    }

    /// Codegen aliases an interface's `error-code` out of that interface's own
    /// imported instance, so the question must be answered by that interface
    /// alone: borrowing another one's answer emits an alias of an export the
    /// instance never declared, and the component only fails in the validator.
    #[test]
    fn an_error_code_is_read_from_the_declaring_interface_alone() {
        let registry = registry_from(
            "wasi:demo",
            r#"
            #[cm("wasi:demo/types@0.1.0#error-code")]
            pub variant ErrorCode {
                #[cm("other")]
                Other(String),
            }

            #[cm("wasi:demo/plain@0.1.0")]
            pub interface Plain {
                #[cm("wasi:demo/plain@0.1.0#tick")]
                fn tick() -> i32;
            }
            "#,
        );
        assert_eq!(
            registry.own_error_code_cm_name("wasi:demo/types@0.1.0"),
            Some("error-code")
        );
        assert_eq!(
            registry.own_error_code_cm_name("wasi:demo/plain@0.1.0"),
            None
        );
        assert!(!registry.declares_own_error_code("wasi:demo/plain@0.1.0"));
    }

    /// The bare-name fallbacks search every bundled namespace, and a
    /// cross-namespace collision is ambiguous rather than silently `wasi:`.
    #[test]
    fn a_bare_name_resolves_within_every_bundled_namespace() {
        let registry = registry_from(
            "web:dom",
            r#"
            #[cm("web:dom/node", linearity = "unrestricted")]
            pub resource Node {}

            #[cm("web:dom/types")]
            pub interface Types {}

            #[cm("web:dom/types")]
            pub struct Rect {
                #[cm("x")]
                pub x: u32,
            }
            "#,
        );
        assert_eq!(
            registry.find_binding_source(CmTypeKind::Newtype, "Node"),
            Some("web:dom/node")
        );
        assert_eq!(
            registry.find_binding_source(CmTypeKind::Struct, "Rect"),
            Some("web:dom/types")
        );
    }

    /// Sizing a payload-less variant as the 4-byte handle of an unregistered
    /// name moves every later field of an enclosing record.
    #[test]
    fn a_payload_less_variant_lays_out_as_its_discriminant() {
        let registry = registry_from(
            "wasi:demo",
            r#"
            #[cm("wasi:demo/types@0.1.0#status")]
            pub variant Status {
                #[cm("active")]
                Active,
                #[cm("inactive")]
                Inactive,
            }
            "#,
        );
        let status = Type::Named(NamedType {
            id: AstId::fresh(),
            name: "Status".to_string(),
            span: make_span(),
        });
        assert_eq!(cm_layout_with_registry(&status, &registry), (1, 1));
    }

    fn make_stream_u8_type() -> Type {
        Type::Generic(GenericType {
            id: AstId::fresh(),
            name: "Stream".to_string(),
            args: vec![Type::Named(NamedType {
                id: AstId::fresh(),
                name: "u8".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        })
    }

    fn make_result_type() -> Type {
        Type::Generic(GenericType {
            id: AstId::fresh(),
            name: "Result".to_string(),
            args: vec![
                Type::Tuple(vec![]), // ()
                Type::Named(NamedType {
                    id: AstId::fresh(),
                    name: "ErrorCode".to_string(),
                    span: make_span(),
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
            build_local_alias_name("wasi", "cli", "Stdout", "write_via_stream"),
            "wasi:cli/Stdout::write_via_stream"
        );
        assert_eq!(
            build_local_alias_name("wasi", "clocks", "MonotonicClock", "now"),
            "wasi:clocks/MonotonicClock::now"
        );
    }

    #[test]
    fn test_func_info_local_alias_name() {
        let func_info = CmFunctionInfo {
            namespace: "wasi".to_string(),
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
            id: AstId::fresh(),
            name: "List".to_string(),
            args: vec![Type::Named(NamedType {
                id: AstId::fresh(),
                name: "String".to_string(),
                span: make_span(),
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
                id: AstId::fresh(),
                name: "String".to_string(),
                span: make_span(),
            }),
            Type::Named(NamedType {
                id: AstId::fresh(),
                name: "String".to_string(),
                span: make_span(),
            }),
        ]);
        let array_tuple = Type::Generic(GenericType {
            id: AstId::fresh(),
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
            id: AstId::fresh(),
            name: "Option".to_string(),
            args: vec![Type::Named(NamedType {
                id: AstId::fresh(),
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

    fn named(name: &str) -> Type {
        Type::Named(NamedType {
            id: AstId::fresh(),
            name: name.to_string(),
            span: make_span(),
        })
    }

    /// A reference whose source interface the registry already answers.
    fn named_in(registry: &mut CmInterfaceRegistry, name: &str, source: &str) -> Type {
        let ty = named(name);
        let Type::Named(n) = &ty else { unreachable!() };
        registry.set_source_interface(n.id, source.to_string());
        ty
    }

    /// Register one declaration of `kind`, the smallest shape each map admits.
    fn register_of_kind(
        registry: &mut CmInterfaceRegistry,
        kind: CmTypeKind,
        source: &str,
        name: &str,
    ) {
        let key = (source.to_string(), name.to_string());
        let cm = name.to_lowercase();
        match kind {
            CmTypeKind::Newtype => {
                registry.newtypes.insert(key, named("u64"));
            }
            CmTypeKind::Resource => {
                registry.resources.insert(key, cm);
            }
            CmTypeKind::Struct => {
                registry.structs.insert(key, (cm, vec![], vec![]));
            }
            CmTypeKind::Variant => {
                registry.variants.insert(key, (cm, vec![]));
            }
            CmTypeKind::Enum => {
                registry.enums.insert(key, (cm, vec![]));
            }
            CmTypeKind::Flags => {
                registry.flags.insert(key, (cm, vec![]));
            }
        }
    }

    /// One list drives every search over the kinds, so a declaration of any
    /// kind answers all of them. A kind one search misses resolves to nothing.
    // Spelled out rather than read from `ALL`, so a kind dropped from `ALL`
    // fails here; `register_of_kind` matches exhaustively, so a new kind will
    // not compile until it is listed.
    #[test]
    fn every_type_kind_answers_every_kind_search() {
        let fq = "wasi:probe/types@0.3.0";
        for kind in [
            CmTypeKind::Newtype,
            CmTypeKind::Resource,
            CmTypeKind::Struct,
            CmTypeKind::Variant,
            CmTypeKind::Enum,
            CmTypeKind::Flags,
        ] {
            assert!(
                CmTypeKind::ALL.contains(&kind),
                "{kind:?} is absent from ALL"
            );
            let mut registry = CmInterfaceRegistry::new();
            register_of_kind(&mut registry, kind, fq, "Probe");
            registry.component_interfaces.insert(fq.to_string());
            let ty = named("Probe");
            let Type::Named(reference) = &ty else {
                unreachable!()
            };

            assert!(registry.declares_wado_name(fq, "Probe"), "{kind:?}");
            assert_eq!(registry.existing_cm_decl(fq), Some("Probe"), "{kind:?}");
            assert_eq!(
                registry.bare_name_owner(kind, "Probe"),
                Some(fq),
                "{kind:?}"
            );
            assert_eq!(
                registry.resolve_cm_source_for(reference).as_deref(),
                Some(fq),
                "{kind:?}"
            );
            assert_eq!(
                registry.resolve_cm_source_with_prefix(reference, "wasi:"),
                Some(fq),
                "{kind:?}"
            );
            assert_eq!(
                registry.find_component_type_source("Probe"),
                Some(fq),
                "{kind:?}"
            );
            assert_eq!(
                registry.cm_source_of_named_type(reference, None).as_deref(),
                Some(fq),
                "{kind:?}"
            );
            assert_eq!(
                registry.find_binding_source(kind, "Probe"),
                Some(fq),
                "{kind:?}"
            );
        }
    }

    /// A newtype reference carrying no source resolves by name, so its base
    /// type's width reaches the layout rather than the 4-byte handle default.
    #[test]
    fn a_bundled_newtype_sizes_as_its_base() {
        let mut registry = CmInterfaceRegistry::new();
        registry.newtypes.insert(
            ("wasi:filesystem/types@0.3.0".into(), "Filesize".into()),
            named("u64"),
        );

        let filesize = named("Filesize");
        assert_eq!(cm_layout_with_registry(&filesize, &registry), (8, 8));
        assert_eq!(
            layout_tuple_with_registry(&[filesize, named("u8")], &registry).size,
            16
        );
    }

    #[test]
    fn local_newtype_base_is_keyed_per_source() {
        // Two modules each declare a local newtype `Id` over a *different* base.
        // A bare-name lookup would collide; the (source, name) key must not.
        let mut registry = CmInterfaceRegistry::new();
        registry
            .newtypes
            .insert(("pkg:a/a@1".into(), "Id".into()), named("f64"));
        registry
            .newtypes
            .insert(("pkg:b/b@1".into(), "Id".into()), named("i32"));

        assert_matches!(
            registry.local_newtype_base(Some("pkg:a/a@1"), "Id"),
            Some(("pkg:a/a@1", Type::Named(n))) if n.name == "f64"
        );
        assert_matches!(
            registry.local_newtype_base(Some("pkg:b/b@1"), "Id"),
            Some(("pkg:b/b@1", Type::Named(n))) if n.name == "i32"
        );
        // A source-less reference to an ambiguous name resolves to nothing
        // rather than picking one arbitrarily.
        assert!(registry.local_newtype_base(None, "Id").is_none());

        // A CM-imported (wasi:) newtype is never treated as local.
        registry.newtypes.insert(
            ("wasi:clocks/types@0.3.0".into(), "Temp".into()),
            named("u64"),
        );
        assert!(
            registry
                .local_newtype_base(Some("wasi:clocks/types@0.3.0"), "Temp")
                .is_none()
        );
        assert!(registry.local_newtype_base(None, "Temp").is_none());
    }

    #[test]
    fn a_component_s_later_interface_declares_as_much_as_its_first() {
        use crate::module_source::{ModuleSourceInterner, WasmAssetKind};

        let mut interner = ModuleSourceInterner::new();
        let dep = interner.wasm("./brotli.wasm", WasmAssetKind::Wasm);
        let mut registry = CmInterfaceRegistry::new();

        // One component exporting two interfaces: every FQ it exports maps back
        // to the single binding module the loader synthesizes for it.
        for fq in ["acme:brotli/compress@1.0.0", "acme:brotli/decompress@1.0.0"] {
            registry
                .cm_interface_module_sources
                .insert(fq.into(), dep.clone());
        }
        registry.structs.insert(
            ("acme:brotli/compress@1.0.0".into(), "Level".into()),
            ("level".into(), Vec::new(), Vec::new()),
        );
        registry.structs.insert(
            ("acme:brotli/decompress@1.0.0".into(), "Window".into()),
            ("window".into(), Vec::new(), Vec::new()),
        );

        assert_eq!(
            registry.interface_declaring(&dep, "Level"),
            Some("acme:brotli/compress@1.0.0")
        );
        assert_eq!(
            registry.interface_declaring(&dep, "Window"),
            Some("acme:brotli/decompress@1.0.0")
        );
        assert_eq!(registry.interface_declaring(&dep, "Absent"), None);
    }

    #[test]
    fn resource_cm_name_by_module_bridges_loader_identity_to_registration_key() {
        use crate::module_source::ModuleSourceInterner;

        let mut interner = ModuleSourceInterner::new();
        let mut registry = CmInterfaceRegistry::new();

        // WASI: registered under the versioned `#[cm]` key; a `ResolvedType`
        // presents the loader path `wasi:http/types.wado`.
        registry.resources.insert(
            ("wasi:http/types@0.3.0".into(), "Request".into()),
            "request".into(),
        );

        // Component dependency: FQ registration key vs. `dep:` ModuleSource
        // share no stem, so only the exact bridge resolves it (drop-leak guard).
        let dep = interner.dependency("../foo/lib.wado");
        registry.resources.insert(
            ("acme:foo/types@1.0.0".into(), "Widget".into()),
            "widget".into(),
        );
        registry
            .cm_interface_module_sources
            .insert("acme:foo/types@1.0.0".into(), dep.clone());

        // A dependency path that itself contains `@`: the exact bridge must not
        // truncate at it (a stem match would).
        let versioned_dep = interner.dependency("../bar@2.0/lib.wado");
        registry.resources.insert(
            ("acme:bar/types@1.0.0".into(), "Gadget".into()),
            "gadget".into(),
        );
        registry
            .cm_interface_module_sources
            .insert("acme:bar/types@1.0.0".into(), versioned_dep.clone());

        // WASI: resolved from the loader `.wado` path via the interface stem.
        assert_eq!(
            registry.get_resource_cm_name_by_module("wasi:http/types.wado", "Request"),
            Some("request")
        );
        // Component: resolved from the exact dependency `ModuleSource` display.
        assert_eq!(
            registry.get_resource_cm_name_by_module(&dep.to_string(), "Widget"),
            Some("widget")
        );
        // `@` in the dependency path does not defeat the exact bridge.
        assert_eq!(
            registry.get_resource_cm_name_by_module(&versioned_dep.to_string(), "Gadget"),
            Some("gadget")
        );
        // A name that exists but under a different module resolves to nothing.
        assert_eq!(
            registry.get_resource_cm_name_by_module("wasi:http/types.wado", "Widget"),
            None
        );
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
            named("u64"),
        );
        registry
            .newtypes
            .insert(("pkg:app/app@1".into(), "Celsius".into()), named("Temp"));

        let celsius = named_in(&mut registry, "Celsius", "pkg:app/app@1");
        assert_matches!(
            registry.resolve_type_preserving_local_newtypes(&celsius),
            Type::Named(n) if n.name == "Celsius"
        );

        let temp = named_in(&mut registry, "Temp", "wasi:clocks/types@0.3.0");
        assert_matches!(
            registry.resolve_type_preserving_local_newtypes(&temp),
            Type::Named(n) if n.name == "u64"
        );
    }

    #[test]
    fn single_flat_value_record_returns_flat_not_outptr() {
        let mut registry = CmInterfaceRegistry::new();
        let iface = "pkg:app/app@1";
        registry.structs.insert(
            (iface.into(), "Single".into()),
            ("single".into(), vec![("n".into(), named("u64"))], vec![]),
        );
        registry.structs.insert(
            (iface.into(), "Point".into()),
            (
                "point".into(),
                vec![("x".into(), named("f64")), ("y".into(), named("f64"))],
                vec![],
            ),
        );
        registry.structs.insert(
            (iface.into(), "Wrap".into()),
            ("wrap".into(), vec![("s".into(), named("String"))], vec![]),
        );

        assert!(
            !cm_return_needs_outptr(&named_in(&mut registry, "Single", iface), &registry),
            "single core-value record must return flat"
        );
        assert!(
            cm_return_needs_outptr(&named_in(&mut registry, "Point", iface), &registry),
            "multi core-value record must use outptr"
        );
        assert!(
            cm_return_needs_outptr(&named_in(&mut registry, "Wrap", iface), &registry),
            "record spanning >1 core value must use outptr"
        );
    }
}
