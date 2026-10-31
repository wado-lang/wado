//! Component Model support for code generation
//!
//! This module provides:
//! - WASI import registry: collects WASI imports from effect definitions in lib/wasi/*.wado
//! - Component Model ABI: type conversion and support checking for CM codegen

use std::collections::BTreeMap;

use crate::hashmap::{IndexMap, IndexSet};

use wasm_encoder::ValType;

use crate::ast;
use crate::ast::{
    AstId, Attribute, CmBoundary, CmImport, FunctionType, GenericType, InterfaceDecl, Item,
    NamedType, NamespacedGenericType, Type, UseItem, cm_function_path, cm_import_of,
    declares_unrestricted, world_import_of,
};
use crate::attribute::CM_PARAMS;
use crate::canonical::{CmDecl, CmFuturePayload, CmPayloadType, CmScalarType, CmStreamPayload};
use crate::cm_abi::{
    CmValType, cm_discriminant_byte_size, cm_flags_byte_align, cm_flags_byte_size,
    layout_fields_with_registry, layout_option_with_registry, layout_result_with_registry,
    layout_tuple_with_registry, layout_variant_with_registry, primitive_size_align,
};
use crate::cm_type_decls::CmTypeDecls;
use crate::defs::{DefId, DefTable};
use crate::module_source::{CmNamespace, ModuleSource};
use crate::name::{
    DeclName, DeclPath, RefKind, UNIT_TYPE_NAME, cm_binding_func_name, cm_import_alias,
    cm_lift_func_name, cm_release_args_func_name, cm_world_import_label, cm_wrap_async_func_name,
    ref_type_name, to_cm_name,
};
use crate::primitive::PrimitiveType;
use crate::resolve::Resolutions;
use crate::stdlib;
use crate::stdlib_snapshot::stdlib_bindings;
use crate::synthesis::cm_binding::types::cm_interface_module;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;
use crate::type_table::TypeTable;
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
            type_table.type_name(type_arg)
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

/// Why `key` cannot be a `map<K, V>` key, or `None` when it can. The Component
/// Model's `keytype` admits the integers, `bool`, `char`, and `string` — no
/// float, no aggregate.
pub fn map_key_rejection(type_table: &TypeTable, key: TypeId) -> Option<String> {
    let peeled = peel_newtypes(type_table, key);
    let admissible = match type_table.get(peeled) {
        ResolvedType::Primitive(prim) => matches!(
            prim,
            PrimitiveType::Bool
                | PrimitiveType::Char
                | PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::I64
                | PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32
                | PrimitiveType::U64
        ),
        ResolvedType::Struct { def, .. } => matches!(
            (
                def.decl(),
                type_table.compiler_item_def(CompilerItem::String),
            ),
            (Some(found), Some(string)) if found == string
        ),
        ResolvedType::Unit
        | ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Newtype { .. }
        | ResolvedType::Never
        | ResolvedType::Ref(_)
        | ResolvedType::MutRef(_)
        | ResolvedType::Function { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::AssocParam { .. }
        | ResolvedType::TypePack { .. }
        | ResolvedType::PackElement { .. }
        | ResolvedType::InferVar(_)
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::Unknown
        | ResolvedType::Error => false,
    };
    (!admissible).then(|| {
        format!(
            "`{}` is not a Component Model `map` key — a key is `bool`, \
             `char`, `String`, or an integer",
            type_table.type_name(key)
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
    let element = peel_newtypes(type_table, element);
    if is_u8_stream_element(type_table, element) {
        return CmStreamPayload::U8;
    }
    match cm_payload_type_from_type_id(type_table, element) {
        Some(payload) => CmStreamPayload::Value(payload),
        None => panic!(
            "`Stream<{}>` has no Component Model element type: a guest-created \
             stream of a WASI-owned type is not supported yet",
            type_table.type_name(element)
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
            )))
        }
        ResolvedType::Enum { def }
        | ResolvedType::Variant { def }
        | ResolvedType::Flags { def } => {
            Some(CmPayloadType::Named(CmDecl::new(type_table.defs(), *def)))
        }
        // Unlike the records above, a WASI-owned resource is included: its
        // component type is aliased from the defining interface, so `own<…>`
        // has one to point at.
        ResolvedType::Resource { def } => Some(CmPayloadType::Resource(CmDecl::new(
            type_table.defs(),
            *def,
        ))),
        _ => None,
    }
}

/// Whether a type's module source already owns a CM lowering path: `wasi:*`
/// interfaces and the WIT-generated `core:*` packages. User, local, and
/// dependency declarations do not, so they route through `Named`.
fn is_cm_owned_source(ms: &ModuleSource) -> bool {
    match ms {
        ModuleSource::Binding { .. } => true,
        ModuleSource::Core { name } => stdlib::is_wit_core_module(name),
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
    let resolved = registry.resolve_type(ty);
    let decls = registry.type_decls();
    match &resolved {
        Type::Named(n) if decls.is(n.id, CompilerItem::String) => Some(CmPayloadType::String),
        Type::Named(n) => {
            if let Some(scalar) = cm_scalar_from_ast_name(&n.name) {
                return Some(CmPayloadType::Scalar(scalar));
            }
            let src = registry.source_interface(n)?;
            let declares = |kinds: &[CmTypeKind]| {
                kinds
                    .iter()
                    .any(|&kind| registry.declares_kind(kind, &src, &n.name))
            };
            let declared = || {
                let def = registry.declared_def(n, type_table.defs())?;
                Some(CmDecl::new(type_table.defs(), def))
            };
            // Before the CM-owned bail below: a WASI resource counts.
            if declares(&[CmTypeKind::Resource]) {
                return Some(CmPayloadType::Resource(declared()?));
            }
            if src.starts_with("wasi:") || stdlib::is_wit_core_interface(&src) {
                return None;
            }
            if !declares(&[
                CmTypeKind::Struct,
                CmTypeKind::Variant,
                CmTypeKind::Enum,
                CmTypeKind::Flags,
            ]) {
                return None;
            }
            Some(CmPayloadType::Named(declared()?))
        }
        Type::Tuple(elems) => elems
            .iter()
            .map(|e| cm_payload_type_from_ast(type_table, e, registry))
            .collect::<Option<Vec<_>>>()
            .map(CmPayloadType::Tuple),
        Type::Generic(g) => match decls.item_at(g.id)? {
            CompilerItem::Option => Some(CmPayloadType::Option(Box::new(
                cm_payload_type_from_ast(type_table, &g.args[0], registry)?,
            ))),
            CompilerItem::List => Some(CmPayloadType::List(Box::new(cm_payload_type_from_ast(
                type_table, &g.args[0], registry,
            )?))),
            CompilerItem::Result => {
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
    let resolved = registry.resolve_type(ty);
    if let Type::Named(n) = &resolved
        && let Some(scalar) = cm_scalar_from_ast_name(&n.name)
    {
        return CmFuturePayload::Scalar(scalar);
    }
    if let Type::Generic(g) = &resolved
        && registry.type_decls().is(g.id, CompilerItem::Result)
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
    let source = registry.source_interface(n)?;
    if !source.starts_with("wasi:") {
        return None;
    }
    if ![CmTypeKind::Enum, CmTypeKind::Variant]
        .into_iter()
        .any(|kind| registry.declares_kind(kind, &source, &n.name))
    {
        return None;
    }
    let def = registry.declared_def(n, type_table.defs())?;
    Some(CmDecl::new(type_table.defs(), def))
}

/// `result<option<resource>, _>`.
fn is_trailers_payload_from_ast(resolved: &ast::Type, registry: &CmInterfaceRegistry) -> bool {
    let decls = registry.type_decls();
    let Type::Generic(g) = resolved else {
        return false;
    };
    if !decls.is(g.id, CompilerItem::Result) {
        return false;
    }
    let Type::Generic(ok) = &registry.resolve_type(&g.args[0]) else {
        return false;
    };
    if !decls.is(ok.id, CompilerItem::Option) {
        return false;
    }
    let Type::Named(inner) = &registry.resolve_type(&ok.args[0]) else {
        return false;
    };
    registry
        .source_interface(inner)
        .is_some_and(|source| registry.declares_kind(CmTypeKind::Resource, &source, &inner.name))
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
        // The Component Model's `defvaltype` stops at `f32` / `f64` and has
        // nothing 128 bits wide, so these cross no boundary.
        PrimitiveType::F16 | PrimitiveType::Bf16 | PrimitiveType::V128 => return None,
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
    Some(CmDecl::new(type_table.defs(), *def))
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
pub fn unwrap_async_call_if_async(
    is_async: bool,
    declared: &Option<Type>,
    decls: &CmTypeDecls,
) -> Option<Type> {
    if !is_async {
        return declared.clone();
    }
    match declared {
        Some(Type::Generic(generic)) if decls.is(generic.id, CompilerItem::AsyncCall) => {
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
/// type is a reference, so a copyable handle is a number: an `f64`, which jco
/// hands JavaScript as a plain number where a `u64` would be a `BigInt`.
fn extern_handle_type(span: Span) -> Type {
    Type::Named(NamedType::new(AstId::fresh(), "f64".to_string(), span))
}

/// Extract CM parameter names from a `#[cm_params("param-a", "param-b")]` attribute.
fn extract_cm_params_attr(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .find(|a| a.name == CM_PARAMS)
        .map(|a| a.args.iter().map(|arg| arg.as_str().to_string()).collect())
        .unwrap_or_default()
}

/// One entry per CM function name, keeping the first binding of each. A CM
/// interface exports a name once however many Wado names bind it, so anything
/// describing the interface walks this rather than the bindings.
pub fn one_per_cm_name<'a>(
    funcs: impl IntoIterator<Item = &'a CmFunctionInfo>,
) -> Vec<&'a CmFunctionInfo> {
    let mut seen = IndexSet::default();
    funcs
        .into_iter()
        .filter(|func| seen.insert(func.wasi_func_name.as_str()))
        .collect()
}

/// What [`CmInterfaceRegistry::value_params`] answers.
struct ValueParams {
    params: Vec<(String, String, Type)>,
    /// The closures among `params`, by index and declared type.
    callbacks: Vec<(usize, Type)>,
    /// See [`CmFunctionInfo::list_args`].
    list_args: Vec<usize>,
}

/// Information about a CM function from an interface method
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
    /// The indices of `params` declared `&ListView<E>`, recorded at the
    /// `List<E>` they cross as: the adapter passes the block the view lies in.
    pub list_args: Vec<usize>,
    /// The closures among `params`, by index and declared type. Each crosses as its `u32` key.
    pub callbacks: Vec<(usize, Type)>,
    /// Return type
    pub return_type: Option<Type>,
    /// Where a world function comes from; `None` for an interface function.
    pub world: Option<WorldFunctionSite>,
}

/// The dependency component exporting a world function, and the label the
/// program imports it under: a world function's name is the component's own,
/// so two dependencies may each export one of a name.
#[derive(Debug, Clone)]
pub struct WorldFunctionSite {
    pub component: ModuleSource,
    pub label: String,
}

impl CmFunctionInfo {
    /// The core function this import lowers to.
    pub fn local_alias_name(&self) -> String {
        cm_import_alias(&self.cm_identifier())
    }

    /// The name of this import's binding, which its call sites call.
    pub fn binding_name(&self) -> String {
        cm_binding_func_name(&self.cm_identifier())
    }

    /// The name of the lift `AsyncCall<T>` points to.
    pub fn lift_name(&self) -> String {
        cm_lift_func_name(&self.cm_identifier())
    }

    /// The name of the function `ArgBuffer` points to.
    pub fn release_args_name(&self) -> String {
        cm_release_args_func_name(&self.cm_identifier())
    }

    /// The name of the function a handler's result passes through to become
    /// this async import's `AsyncCall<T>`.
    pub fn wrap_async_name(&self) -> String {
        cm_wrap_async_func_name(&self.cm_identifier())
    }

    /// The Wado name that binds it: `Interface::operation`, or a world import's
    /// bare name.
    fn wado_name(&self) -> String {
        if self.is_world_function() {
            return self.method_name.clone();
        }
        DeclPath::method_of(&DeclName::new(&self.interface_name), &self.method_name).into_string()
    }

    /// Whether the component's world imports it directly, under no interface.
    #[must_use]
    pub fn is_world_function(&self) -> bool {
        self.world.is_some()
    }

    /// The CM function this binds, as [`CmImport::full_path`] spells it (a world
    /// function: the label the program imports it under). What identifies the
    /// function: its key in the registry and in `NirPackage::used_wasi_functions`.
    #[must_use]
    pub fn cm_identifier(&self) -> String {
        match &self.world {
            Some(site) => site.label.clone(),
            None => cm_function_path(&self.interface_path, &self.wasi_func_name),
        }
    }

    /// The declared closure type of parameter `i`, when it takes one.
    pub fn callback_at(&self, i: usize) -> Option<&Type> {
        self.callbacks
            .iter()
            .find_map(|(at, closure)| (*at == i).then_some(closure))
    }

    /// The CM parameter names and every type's [`CmInterfaceRegistry::type_key`],
    /// a closure's as declared rather than as its key.
    fn signature_key(
        &self,
        registry: &CmInterfaceRegistry,
    ) -> (bool, Vec<(&str, String)>, Option<String>) {
        let params = self
            .params
            .iter()
            .enumerate()
            .map(|(i, (_, cm_name, ty))| {
                let ty = self.callback_at(i).unwrap_or(ty);
                (cm_name.as_str(), registry.type_key(ty))
            })
            .collect();
        let ret = self.return_type.as_ref().map(|ty| registry.type_key(ty));
        (self.is_async, params, ret)
    }

    /// Whether `canon lower` requires the Memory canonical option: an async
    /// function, a parameter or result involving linear memory (strings,
    /// lists, streams, options, results, tuples), or a CM record or variant
    /// the registry knows. Realloc is required under exactly the same
    /// conditions.
    pub fn needs_memory_with_registry(&self, registry: &CmInterfaceRegistry) -> bool {
        let decls = registry.type_decls();
        if self.is_async
            || self
                .params
                .iter()
                .any(|(_, _, ty)| ty.any(&mut |ty| type_itself_requires_memory(ty, decls)))
            || self
                .return_type
                .as_ref()
                .is_some_and(|ty| return_type_requires_memory(ty, decls))
        {
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
        ty.any(&mut |ty| {
            type_itself_requires_memory(ty, registry.type_decls())
                || matches!(ty, Type::Named(named)
                    if Self::cm_named_requires_memory(named, registry, seen))
        })
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
        let Some(source) = registry.source_interface(named) else {
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
}

/// Whether `ty` is a string, a list or a stream, not looking inside it.
fn type_itself_requires_memory(ty: &Type, decls: &CmTypeDecls) -> bool {
    matches!(
        decls.item_of(ty),
        Some(
            CompilerItem::String
                | CompilerItem::List
                | CompilerItem::TreeMap
                | CompilerItem::Stream
        )
    )
}

/// Whether a return type requires Memory + Realloc in canon lower.
fn return_type_requires_memory(ty: &Type, decls: &CmTypeDecls) -> bool {
    match ty {
        Type::Tuple(elems) => !elems.is_empty(),
        _ => matches!(
            decls.item_of(ty),
            Some(
                CompilerItem::String
                    | CompilerItem::List
                    | CompilerItem::TreeMap
                    | CompilerItem::Option
                    | CompilerItem::Result
                    | CompilerItem::Stream
                    | CompilerItem::Future
            )
        ),
    }
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
    /// The resource a getter of this interface answers, if any.
    pub resource_type: Option<CmGetterResource>,
}

/// The resource a getter interface answers (`TerminalInput` from
/// `wasi:cli/terminal-stdin`), which another interface may declare.
#[derive(Debug, Clone)]
pub struct CmGetterResource {
    pub wado_name: String,
    pub cm_name: String,
    /// The interface declaring the resource.
    pub source: String,
}

/// Where a type declaration is registered: its interface, and the Wado name it
/// is registered under there, which an `export use` alias may have given it.
#[derive(Debug, Clone)]
struct CmRegistration {
    interface: String,
    name: String,
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
/// - The CM function each operation binds
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

    /// The declaration each type name the registry holds resolves to.
    type_decls: CmTypeDecls,

    /// Where each registered type declaration is registered, so a reference
    /// bound to its declaration answers its interface and the name the
    /// `(interface, name)`-keyed maps hold it under.
    registrations: IndexMap<AstId, CmRegistration>,

    /// Every bound CM function, by [`CmFunctionInfo::cm_identifier`].
    functions: IndexMap<String, CmFunctionInfo>,

    /// Interface path -> list of functions
    /// Using `BTreeMap` for deterministic ordering
    interfaces: BTreeMap<String, Vec<CmFunctionInfo>>,

    /// A `--lib` package's guest effect, by its declaration -> the interface
    /// minted for it.
    guest_effects: IndexMap<AstId, CmImport>,

    /// Newtypes collected from WASI modules (e.g., Instant -> u64).
    /// Key: `(source_interface, wado_name)` where `source_interface` is the
    /// `#[cm("...")]` fragment before the `#` (e.g.
    /// `"wasi:clocks/types@0.3.0"`). Keying by interface makes
    /// same-named types from distinct interfaces structurally distinct.
    newtypes: IndexMap<(String, String), Type>,

    /// The CM name each of [`Self::newtypes`] is declared under (`duration`).
    newtype_cm_names: IndexMap<(String, String), String>,

    /// Resource types collected from WASI modules (e.g., `TerminalInput`).
    /// Key: `(source_interface, wado_name)`. Value: CM kebab-case name.
    resources: IndexMap<(String, String), String>,

    /// Resources declared `linearity = "unrestricted"`, registered as `f64` newtypes.
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
    /// locals and component imports). WASI/core interfaces are absent: their
    /// module path is their FQ.
    cm_interface_module_sources: IndexMap<String, ModuleSource>,

    /// FQs of interfaces imported from a CM component dependency. The plan
    /// classifies these as [`crate::wir::ImportKind::Component`] for composition.
    component_interfaces: IndexSet<String>,

    /// Per component-dependency interface FQ, the host-leaf import FQs the
    /// dependency component itself declares (its WASI capabilities). Effect
    /// reconstruction unions these into the consumer when the interface is used;
    /// a purely-computational component maps to an empty set. Every exported
    /// interface of one component shares the same set (v1 component-level union).
    component_host_leaf_imports: IndexMap<String, Vec<String>>,

    /// World-level function imports (Phase 9), by the bodyless function
    /// declaring each -> its key in `functions`.
    world_imports: IndexMap<AstId, String>,
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
            Self::CmAttributed => to_cm_name(param),
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

/// The [`CmFunctionInfo::cm_identifier`] of the function `#[cm(cm_import)]` on
/// the Wado operation `operation` binds: the one the payload names, or else the
/// operation's own name in kebab case.
#[must_use]
pub fn cm_function_key(cm_import: &CmImport, operation: &str) -> String {
    cm_function_path(
        &cm_import.interface_path(),
        &cm_function_name(cm_import, operation),
    )
}

/// [`cm_function_key`] read off the `#[cm]` payload `cm_name`; `None` when it
/// names no CM import.
#[must_use]
pub fn cm_payload_function_key(cm_name: &str, operation: &str) -> Option<String> {
    Some(cm_function_key(&CmImport::parse(cm_name)?, operation))
}

fn cm_function_name(cm_import: &CmImport, operation: &str) -> String {
    cm_import
        .function
        .clone()
        .unwrap_or_else(|| to_cm_name(operation))
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

/// Spell `Self` as the enclosing resource's name in a receiver type. The site
/// keeps its id, which [`CmTypeDecls`] binds to the resource.
fn substitute_self_in_type(ty: &Type, resource_name: &str) -> Type {
    match ty {
        Type::Named(n) if n.name == "Self" => Type::Named(NamedType {
            name: resource_name.to_string(),
            ..n.clone()
        }),
        Type::Reference(inner) => {
            Type::Reference(Box::new(substitute_self_in_type(inner, resource_name)))
        }
        Type::MutReference(inner) => {
            Type::MutReference(Box::new(substitute_self_in_type(inner, resource_name)))
        }
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
        registry: &CmInterfaceRegistry,
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
                    .map(|(n, ty)| (n.clone(), registry.peel_cm_newtype(ty)))
                    .collect(),
                return_type: m
                    .return_type
                    .as_ref()
                    .map(|ty| {
                        unwrap_async_call_if_async(
                            m.is_async,
                            &Some(ty.clone()),
                            &registry.type_decls,
                        )
                    })
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

/// [`cm_definition`] of each item in `module`, keyed by name.
fn collect_cm_definitions(module: &ast::Module) -> IndexMap<String, String> {
    module
        .items
        .iter()
        .filter_map(cm_definition)
        .map(|(name, source)| (name.to_string(), source))
        .collect()
}

/// The interface each `#[cm(…)]` declaration among `items` binds, keyed by the
/// declaration.
pub fn cm_bound_defs<'i>(
    items: impl IntoIterator<Item = &'i Item>,
    defs: &DefTable,
) -> IndexMap<DefId, String> {
    items
        .into_iter()
        .filter_map(|item| {
            let (_, source) = cm_definition(item)?;
            Some((defs.def_at(item.id()), source))
        })
        .collect()
}

/// The name `item` declares and the interface its `#[cm(…)]` binds it to, or
/// `None` for an item binding none.
pub fn cm_definition(item: &Item) -> Option<(&str, String)> {
    let (name, attrs) = match item {
        Item::Newtype(a) => (&a.name, a.attrs.as_slice()),
        Item::Resource(r) => (&r.name, r.attrs.as_slice()),
        Item::Struct(s) => (&s.name, s.attrs.as_slice()),
        Item::Flags(f) => (&f.name, f.attributes.as_deref().unwrap_or(&[])),
        Item::Enum(e) => (&e.name, e.attrs.as_slice()),
        Item::Variant(v) => (&v.name, v.attrs.as_slice()),
        Item::Interface(e) => (&e.name, e.attrs.as_slice()),
        Item::Use(_)
        | Item::Function(_)
        | Item::TupleTypeDecl(_)
        | Item::BuiltinTypeDecl(_)
        | Item::Impl(_)
        | Item::Trait(_)
        | Item::World(_)
        | Item::Test(_)
        | Item::Global(_)
        | Item::Error(_) => return None,
    };
    let source = CmInterfaceRegistry::cm_source_interface(attrs);
    (!source.is_empty()).then_some((name.as_str(), source))
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
    let mut local: IndexMap<String, String> = IndexMap::default();
    if let Some(defs) = defs_by_module.get(module_path) {
        for (name, source) in defs {
            local.insert(name.clone(), source.clone());
        }
    }
    for item in &module.items {
        if let Item::Use(use_decl) = item {
            let Some(other_defs) = defs_by_module.get(use_decl.source.as_str()) else {
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
                    // `E::{op}` binds the operations, never `E`.
                    UseItem::InterfaceFunctions { .. }
                    | UseItem::Namespace { .. }
                    | UseItem::Wildcard => {}
                }
            }
        }
    }
    local
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

/// Why the program's own `#[cm]` declarations cannot be registered.
#[derive(Debug)]
pub enum UserCmError {
    /// A module declares into an interface another module owns, or binds an
    /// `Interface::method` another binding already took.
    Duplicate(String),
    /// A type crossing a binding belongs to no interface.
    UnboundType(String),
}

/// Names each type a program binding module writes by the declaration it
/// reaches, never by its spelling, and refuses one that binds no interface.
struct UserCmTypeBinder<'a> {
    resolutions: &'a Resolutions,
    /// The interface each `#[cm]` declaration of the program binds.
    bound: &'a IndexMap<DefId, String>,
}

impl UserCmTypeBinder<'_> {
    fn bind_module(&self, module: &mut ast::Module) -> Result<SourceInterfaceBatch, String> {
        let mut sources = SourceInterfaceBatch::default();
        for item in &mut module.items {
            self.bind_item(item, &mut sources)?;
        }
        Ok(sources)
    }

    fn bind_item(&self, item: &mut Item, sources: &mut SourceInterfaceBatch) -> Result<(), String> {
        let item_id = item.id();
        let declares_cm = || {
            let def = self.resolutions.defs().def_at(item_id);
            self.bound.contains_key(&def)
        };
        try_for_each_signed_type(item, &mut |site, ty| {
            let crosses = match site.signer {
                Signer::Declaration => declares_cm(),
                Signer::Function => world_import_of(site.attrs).is_some(),
                Signer::InterfaceOperation => cm_import_of(site.attrs).is_some(),
                Signer::ResourceMethod => cm_import_of(site.attrs).is_some() && declares_cm(),
            };
            self.bind_type(ty, site.label, crosses, sources)
        })
    }

    /// `crosses` says whether `ty` crosses the boundary in `label` or only sits
    /// in a signature the registry never reads.
    fn bind_type(
        &self,
        ty: &mut Type,
        label: &str,
        crosses: bool,
        sources: &mut SourceInterfaceBatch,
    ) -> Result<(), String> {
        let defs = self.resolutions.defs();
        bind_type_names(ty, self.resolutions, &mut |named, def| {
            if let Some(source) = self.bound.get(&def) {
                sources.insert(named.id, source.clone());
            } else if crosses && defs.module(def).is_program() {
                let declared = defs.name(def);
                let written = if named.name == declared {
                    format!("`{declared}`")
                } else {
                    format!("`{}` (`{declared}`)", named.name)
                };
                return Err(format!(
                    "{written} crosses the Component Model boundary in `{label}` but declares \
                     no interface; bind it with `#[cm(\"<interface>#<name>\")]`"
                ));
            }
            Ok(None)
        })
    }
}

/// What writes a type slot of an item.
#[derive(Debug, Clone, Copy)]
pub enum Signer {
    /// A field, a variant payload or a newtype's base.
    Declaration,
    Function,
    InterfaceOperation,
    ResourceMethod,
}

/// Where a type slot sits: what writes it, under which name and attributes.
pub struct TypeSlotSite<'a> {
    pub signer: Signer,
    pub label: &'a str,
    pub attrs: &'a [Attribute],
}

/// Visit each type `item` declares or signs, with the site writing it.
pub fn try_for_each_signed_type<E>(
    item: &mut Item,
    visit: &mut impl FnMut(&TypeSlotSite<'_>, &mut Type) -> Result<(), E>,
) -> Result<(), E> {
    match item {
        Item::Struct(s) => {
            let site = TypeSlotSite::declaration(&s.name, &s.attrs);
            s.fields
                .iter_mut()
                .try_for_each(|f| visit(&site, &mut f.ty))
        }
        Item::Variant(v) => {
            let site = TypeSlotSite::declaration(&v.name, &v.attrs);
            v.cases
                .iter_mut()
                .filter_map(|c| c.payload.as_mut())
                .try_for_each(|ty| visit(&site, ty))
        }
        Item::Newtype(a) => visit(&TypeSlotSite::declaration(&a.name, &a.attrs), &mut a.ty),
        Item::Function(f) => try_for_each_function_type(f, Signer::Function, visit),
        Item::Interface(decl) => try_for_each_operation_type(decl, visit),
        Item::Resource(r) => r
            .methods
            .iter_mut()
            .try_for_each(|m| try_for_each_function_type(m, Signer::ResourceMethod, visit)),
        Item::Use(_)
        | Item::Flags(_)
        | Item::Enum(_)
        | Item::TupleTypeDecl(_)
        | Item::BuiltinTypeDecl(_)
        | Item::Impl(_)
        | Item::Trait(_)
        | Item::World(_)
        | Item::Test(_)
        | Item::Global(_)
        | Item::Error(_) => Ok(()),
    }
}

/// Visit each type `item` declares or signs, as [`try_for_each_signed_type`]
/// does, without the copy of `item` a reader would otherwise take.
pub fn for_each_signed_type(item: &Item, visit: &mut impl FnMut(&Type)) {
    fn function(f: &ast::Function, visit: &mut impl FnMut(&Type)) {
        f.params
            .iter()
            .map(|p| &p.ty)
            .chain(&f.return_type)
            .for_each(visit);
    }
    match item {
        Item::Struct(s) => s.fields.iter().for_each(|f| visit(&f.ty)),
        Item::Variant(v) => v
            .cases
            .iter()
            .filter_map(|c| c.payload.as_ref())
            .for_each(visit),
        Item::Newtype(a) => visit(&a.ty),
        Item::Function(f) => function(f, visit),
        Item::Interface(decl) => decl.methods.iter().for_each(|m| function(m, visit)),
        Item::Resource(r) => r.methods.iter().for_each(|m| function(m, visit)),
        Item::Use(_)
        | Item::Flags(_)
        | Item::Enum(_)
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

/// Visit each parameter and result type of `decl`'s operations.
pub fn try_for_each_operation_type<E>(
    decl: &mut InterfaceDecl,
    visit: &mut impl FnMut(&TypeSlotSite<'_>, &mut Type) -> Result<(), E>,
) -> Result<(), E> {
    decl.methods
        .iter_mut()
        .try_for_each(|m| try_for_each_function_type(m, Signer::InterfaceOperation, visit))
}

fn try_for_each_function_type<E>(
    f: &mut ast::Function,
    signer: Signer,
    visit: &mut impl FnMut(&TypeSlotSite<'_>, &mut Type) -> Result<(), E>,
) -> Result<(), E> {
    let site = TypeSlotSite {
        signer,
        label: &f.name,
        attrs: &f.attrs,
    };
    f.params
        .iter_mut()
        .map(|p| &mut p.ty)
        .chain(f.return_type.as_mut())
        .try_for_each(|ty| visit(&site, ty))
}

impl<'a> TypeSlotSite<'a> {
    fn declaration(label: &'a str, attrs: &'a [Attribute]) -> Self {
        Self {
            signer: Signer::Declaration,
            label,
            attrs,
        }
    }
}

/// Rewrite every type name `ty` writes to its declaration's name, so no consumer
/// reads an alias's spelling. `leaf` sees each name with its declaration first,
/// and answers the name to write instead where the declaration is published
/// under another (`export use { T as U }`).
pub fn bind_type_names<E>(
    ty: &mut Type,
    resolutions: &Resolutions,
    leaf: &mut impl FnMut(&NamedType, DefId) -> Result<Option<String>, E>,
) -> Result<(), E> {
    let children: Vec<&mut Type> = match ty {
        Type::Named(named) => {
            let Some(def) = resolutions.declared_if_walked(named.id) else {
                return Ok(());
            };
            let defs = resolutions.defs();
            if !defs.kind(def).is_type() {
                return Ok(());
            }
            named.name = leaf(named, def)?.unwrap_or_else(|| defs.name(def).to_string());
            return Ok(());
        }
        Type::Generic(g) => {
            if let Some(def) = resolutions.declared_if_walked(g.id)
                && resolutions.defs().kind(def).is_type()
            {
                g.name = resolutions.defs().name(def).to_string();
            }
            g.args.iter_mut().collect()
        }
        Type::NamespacedGeneric(g) => g.args.iter_mut().collect(),
        Type::Function(f) => f
            .params
            .iter_mut()
            .chain(std::iter::once(&mut f.return_type))
            .collect(),
        Type::Tuple(elems) => elems.iter_mut().collect(),
        Type::Reference(inner) | Type::MutReference(inner) => vec![&mut **inner],
        Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => Vec::new(),
    };
    children
        .into_iter()
        .try_for_each(|ty| bind_type_names(ty, resolutions, leaf))
}

/// The source interface of every named type `module` signs whose name is in
/// `local_names`, the rest being primitives, type parameters or foreign names.
fn collect_named_type_sources(
    module: &ast::Module,
    local_names: &IndexMap<String, String>,
) -> SourceInterfaceBatch {
    let mut sources = SourceInterfaceBatch::default();
    for item in &module.items {
        for_each_signed_type(item, &mut |ty| {
            ty.for_each(&mut |ty| {
                if let Type::Named(n) = ty
                    && let Some(source) = local_names.get(&n.name)
                {
                    sources.entry(n.id).or_insert_with(|| source.clone());
                }
            });
        });
    }
    sources
}

impl CmInterfaceRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer what each type the program's own modules and component imports
    /// sign names, through this compile's resolution; the bundled bindings were
    /// answered once for every compile.
    pub fn bind_program_types(
        &mut self,
        modules: &IndexMap<ModuleSource, ast::Module>,
        resolutions: &Resolutions,
    ) {
        let signing = modules
            .iter()
            .filter(|(source, _)| !source.is_core() && !source.is_binding())
            .map(|(_, module)| module);
        self.type_decls.bind(modules.values(), signing, resolutions);
    }

    /// What each type name the registry holds names.
    #[must_use]
    pub fn type_decls(&self) -> &CmTypeDecls {
        &self.type_decls
    }

    /// Whether the resource `name` declared in `source` is unrestricted, and so
    /// crosses the boundary as the universal handle rather than a CM handle.
    #[must_use]
    pub fn is_unrestricted_resource(&self, source: &str, name: &str) -> bool {
        self.unrestricted_resources
            .contains_key(&(source.to_string(), name.to_string()))
    }

    /// The unrestricted resource `ty` is, or references.
    #[must_use]
    pub fn extern_handle(&self, ty: &Type) -> Option<Type> {
        let handle = self.deref_extern_handle(ty);
        let Type::Named(named) = &handle else {
            return None;
        };
        let source = self.source_interface(named)?;
        self.is_unrestricted_resource(&source, &named.name)
            .then_some(handle)
    }

    /// The CM type of a declared parameter: an extern-handle loses its
    /// reference and stays the resource; other newtypes resolve.
    fn cm_param_type(&self, ty: &Type) -> Type {
        self.extern_handle(ty)
            .unwrap_or_else(|| self.peel_cm_newtype(ty))
    }

    /// `ty` with a CM newtype it names replaced by the newtype's base. A
    /// reference with no interface, or naming no newtype, is returned
    /// unchanged, as are other type shapes.
    fn peel_cm_newtype(&self, ty: &Type) -> Type {
        let Type::Named(named) = ty else {
            return ty.clone();
        };
        self.source_interface(named)
            .and_then(|source| self.newtypes.get(&(source, named.name.clone())))
            .cloned()
            .unwrap_or_else(|| ty.clone())
    }

    /// `func`'s parameters as `(wado_name, cm_name, type)`. `fallback` names a
    /// parameter `#[cm_params]` leaves out, and `rewrite` maps a declared type
    /// before it is resolved.
    fn cm_params_of(
        &self,
        func: &ast::Function,
        fallback: impl Fn(&str) -> String,
        rewrite: impl Fn(&Type) -> Type,
    ) -> Vec<(String, String, Type)> {
        let cm_param_names = extract_cm_params_attr(&func.attrs);
        func.params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let cm_name = cm_param_names
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| fallback(&p.name));
                (p.name.clone(), cm_name, self.cm_param_type(&rewrite(&p.ty)))
            })
            .collect()
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
        self.source_interfaces.get(named.id).or_else(|| {
            self.interface_of_decl(self.type_decls.decl_at(named.id)?)
                .map(str::to_string)
        })
    }

    /// The declaration the type site `named` refers to.
    #[must_use]
    pub fn declared_def(&self, named: &NamedType, defs: &DefTable) -> Option<DefId> {
        Some(defs.def_at(self.type_decls.decl_at(named.id)?))
    }

    /// The CM interface the type declaration `decl` is registered under.
    #[must_use]
    pub fn interface_of_decl(&self, decl: AstId) -> Option<&str> {
        self.registrations
            .get(&decl)
            .map(|registration| registration.interface.as_str())
    }

    /// The Wado name the type declaration `decl` is registered under, which a
    /// type naming it must spell for the registry's maps to answer.
    #[must_use]
    pub fn registered_name_of(&self, decl: AstId) -> Option<&str> {
        self.registrations
            .get(&decl)
            .map(|registration| registration.name.as_str())
    }

    fn register_decl(&mut self, decl: AstId, interface: &str, name: &str) {
        self.registrations.insert(
            decl,
            CmRegistration {
                interface: interface.to_string(),
                name: name.to_string(),
            },
        );
    }

    /// A span-insensitive key for `ty`, which names a CM type by its declaring
    /// interface as well, so same-named types from two interfaces stay apart.
    fn type_key(&self, ty: &Type) -> String {
        let keys = |types: &[Type]| {
            types
                .iter()
                .map(|ty| self.type_key(ty))
                .collect::<Vec<_>>()
                .join(",")
        };
        match ty {
            Type::Named(n) => match self.source_interface(n) {
                Some(source) => format!("{source}#{}", n.name),
                None => n.name.clone(),
            },
            Type::Reference(inner) => ref_type_name(RefKind::Shared, &self.type_key(inner)),
            Type::MutReference(inner) => ref_type_name(RefKind::Mut, &self.type_key(inner)),
            Type::Generic(g) => format!("{}:{}", g.name, keys(&g.args)),
            Type::NamespacedGeneric(g) => {
                format!("{}::{}:{}", g.namespace, g.name, keys(&g.args))
            }
            Type::Tuple(elems) => format!("[{}]", keys(elems)),
            // The host sees a closure's arguments and result, never its effects.
            Type::Function(f) => {
                format!("fn({})->{}", keys(&f.params), self.type_key(&f.return_type))
            }
            _ => format!("{ty:?}"),
        }
    }

    /// Record the interface a reference site resolves to.
    pub fn set_source_interface(&self, site: AstId, interface: String) {
        self.source_interfaces.set(site, interface);
    }

    /// Merge a batch of site answers, first-writer-wins per site.
    pub fn extend_source_interfaces(&self, batch: SourceInterfaceBatch) {
        self.source_interfaces.extend(batch);
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
        // Two-pass bootstrap so every stdlib `Type::Named` carries an exact
        // source interface. Pass 1 records each module's declared type names and
        // `#[cm(…)]` paths, so pass 2 can resolve a cross-module `use` whose
        // target parses after the importer; pass 2 then builds each module's
        // local name → source_interface map and walks its `Type` nodes.
        let bindings = stdlib_bindings();
        let modules = bindings.modules.clone();
        let defs_by_module: IndexMap<&'static str, IndexMap<String, String>> = modules
            .iter()
            .map(|(path, module)| (*path, collect_cm_definitions(module)))
            .collect();

        let mut registry = Self::new();
        registry.type_decls = bindings.type_decls.clone();
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
            if let Err(msg) = registry.register_module_decls(&module, &CmDeclScope::Module) {
                panic!("the stdlib binds each `Interface::method` once: {msg}");
            }
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
    fn register_module_decls(
        &mut self,
        module: &ast::Module,
        scope: &CmDeclScope,
    ) -> Result<(), String> {
        for item in &module.items {
            let name = match item {
                Item::Resource(resource) => &resource.name,
                Item::Newtype(_)
                | Item::Struct(_)
                | Item::Flags(_)
                | Item::Enum(_)
                | Item::Variant(_) => item.type_decl().expect("a type item declares a type").name,
                _ => continue,
            };
            let source_interface = Self::cm_source_interface(item.attrs());
            if !source_interface.is_empty() {
                self.register_decl(item.id(), &source_interface, name);
            }
        }

        // First, collect newtypes from this module
        for item in &module.items {
            if let Item::Newtype(alias) = item {
                let source_interface = Self::cm_source_interface(&alias.attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
                self.newtype_cm_names.insert(
                    (source_interface.clone(), alias.name.clone()),
                    cm_attr_cm_name(&alias.attrs, &alias.name),
                );
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
                let source_interface = Self::cm_source_interface(&resource.attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
                // Use the #[cm] fragment as the CM name (preserves acronym casing like DNS, TLS)
                let cm_name = cm_attr_cm_name(&resource.attrs, &resource.name);
                // An unrestricted resource is erased at the boundary, which sees
                // the universal handle, a copyable `f64`, so it registers as a
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
                let source_interface = Self::cm_source_interface(&struct_def.attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(&struct_def.attrs, &struct_def.name);
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
                let source_interface = Self::cm_source_interface(attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(attrs, &flags_def.name);
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
                // Format: #[cm("wasi:sockets/types@0.3.0-rc-2025-09-16#error-code")]
                let source_interface = Self::cm_source_interface(&enum_def.attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(&enum_def.attrs, &enum_def.name);
                // Use per-case #[cm] attr for CM name
                let variant_names: Vec<String> = enum_def
                    .cases
                    .iter()
                    .map(|c| cm_attr_cm_name(&c.attrs, &c.name))
                    .collect();
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
                let source_interface = Self::cm_source_interface(&variant_def.attrs);
                if !scope.admits(&source_interface) {
                    continue;
                }
                // Use the #[cm] fragment as the CM name (preserves acronym casing)
                let cm_name = cm_attr_cm_name(&variant_def.attrs, &variant_def.name);
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

        self.register_interface_cm_methods(module, scope)?;

        // Register resource methods with resolved types for params but NOT for return type
        for item in &module.items {
            if let Item::Resource(resource) = item {
                let resource_source = Self::cm_source_interface(&resource.attrs);
                if !scope.admits(&resource_source) {
                    continue;
                }
                for method in &resource.methods {
                    if let Some(wasi) = cm_import_of(&method.attrs) {
                        let params = self.cm_params_of(
                            method,
                            |param| scope.param_fallback(param, &resource.name, &method.name),
                            |ty| substitute_self_in_type(ty, &resource.name),
                        );

                        // Keep original return type for newtype semantics.
                        // Resource methods declared as `async fn -> AsyncCall<T>`
                        // are unwrapped at registration so downstream code
                        // works with the CM-ABI `T`. See the effect branch.
                        let return_type = unwrap_async_call_if_async(
                            method.is_async,
                            &method.return_type,
                            &self.type_decls,
                        );

                        self.register(
                            &resource.name,
                            &method.name,
                            wasi,
                            method.is_async,
                            params,
                            return_type,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Register the `#[cm(…)]` declarations the program's own modules make, so
    /// a call to one lowers to that import instead of reaching WIR unresolved.
    pub fn register_user_cm_modules(
        &mut self,
        modules: &[(&ModuleSource, &ast::Module)],
        resolutions: &Resolutions,
    ) -> Result<(), UserCmError> {
        let bound_by_module: Vec<IndexMap<DefId, String>> = modules
            .iter()
            .map(|(_, module)| cm_bound_defs(&module.items, resolutions.defs()))
            .collect();
        let bound = bound_by_module
            .iter()
            .flatten()
            .map(|(def, source)| (*def, source.clone()))
            .collect();
        let binder = UserCmTypeBinder {
            resolutions,
            bound: &bound,
        };
        for ((module_source, module), module_bound) in modules.iter().zip(&bound_by_module) {
            let mut module = (*module).clone();
            let sources = binder
                .bind_module(&mut module)
                .map_err(UserCmError::UnboundType)?;
            self.register_user_cm_decls(module_bound.values(), module_source)
                .map_err(UserCmError::Duplicate)?;
            self.extend_source_interfaces(sources);
            self.register_module_decls(&module, &CmDeclScope::CmAttributed)
                .map_err(UserCmError::Duplicate)?;
        }
        Ok(())
    }

    /// Claim for `module_source` the interfaces its declarations bind. `Err`
    /// names an interface another module already declares.
    fn register_user_cm_decls<'s>(
        &mut self,
        interfaces: impl Iterator<Item = &'s String> + Clone,
        module_source: &ModuleSource,
    ) -> Result<(), String> {
        // A CM interface has one declaring module. Registering into one another
        // module already owns would overwrite that owner, leaving every type it
        // declares unresolvable, and collide in `register_unique`, which would
        // report user code as a stdlib bug.
        for source in interfaces.clone() {
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
        for source in interfaces {
            self.cm_interface_module_sources
                .entry(source.clone())
                .or_insert_with(|| module_source.clone());
        }
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
    fn register_interface_cm_methods(
        &mut self,
        module: &ast::Module,
        scope: &CmDeclScope,
    ) -> Result<(), String> {
        for item in &module.items {
            let Item::Interface(effect) = item else {
                continue;
            };
            for method in &effect.methods {
                let Some(wasi) = cm_import_of(&method.attrs) else {
                    continue;
                };
                let params = self.cm_params_of(
                    method,
                    |param| scope.param_fallback(param, &effect.name, &method.name),
                    Type::clone,
                );
                // Store the CM-ABI return type: an async import's `AsyncCall<T>`
                // wrapper is stripped here, and both the elaborator and the
                // binding synthesiser re-wrap it so user code still sees it.
                let return_type = unwrap_async_call_if_async(
                    method.is_async,
                    &method.return_type,
                    &self.type_decls,
                );
                self.register(
                    &effect.name,
                    &method.name,
                    wasi,
                    method.is_async,
                    params,
                    return_type,
                )?;
            }
        }
        Ok(())
    }

    /// Register a component dependency's binding module via the stdlib's
    /// `Self::register_module_decls` path, recording each interface FQ as a
    /// component import for [`crate::wir::ImportKind::Component`] classification.
    pub fn register_component_decls(
        &mut self,
        module: &ast::Module,
        interface_fqs: &[String],
        imported_fqs: &[String],
        host_leaf_imports: &[String],
        module_source: &ModuleSource,
    ) -> Result<(), String> {
        self.register_module_decls(module, &CmDeclScope::Module)?;
        self.register_world_imports(module, module_source)?;
        for fq in interface_fqs {
            self.component_interfaces.insert(fq.clone());
            self.cm_interface_module_sources
                .insert(fq.clone(), module_source.clone());
            if !host_leaf_imports.is_empty() {
                self.component_host_leaf_imports
                    .insert(fq.clone(), host_leaf_imports.to_vec());
            }
        }
        // The bindings declare an imported interface's types too, so each maps
        // back to them; the first component to import one declares it.
        for fq in imported_fqs {
            self.cm_interface_module_sources
                .entry(fq.clone())
                .or_insert_with(|| module_source.clone());
        }
        Ok(())
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

    /// Whether `fq` is a guest effect some component dependency imports: an
    /// imported interface outside WASI, which is the host's.
    #[must_use]
    pub fn is_dependency_guest_import(&self, fq: &str) -> bool {
        !fq.starts_with("wasi:") && self.is_dependency_import(fq)
    }

    /// Whether some component dependency imports `fq`.
    fn is_dependency_import(&self, fq: &str) -> bool {
        self.component_host_leaf_imports
            .values()
            .flatten()
            .any(|import| import == fq)
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
            .insert(iface_fq.to_string(), entry_source);

        // Every entry type is registered, not just the `pub` ones: a type an
        // `export fn` names crosses the boundary whether or not it is marked,
        // and nothing here computes that reachability.
        for item in &module.items {
            self.register_lib_local_item(item, iface_fq);
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
                (is_guest_effect(effect) && to_cm_name(&effect.name) == base.interface)
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
            let cm_import = CmImport {
                namespace: base.namespace.clone(),
                package: base.package.clone(),
                interface: to_cm_name(&effect.name),
                version: base.version.clone(),
                function: None,
            };
            self.guest_effects.insert(effect.id, cm_import.clone());
            for method in &effect.methods {
                let params: Vec<(String, String, Type)> = method
                    .params
                    .iter()
                    .map(|p| {
                        (
                            p.name.clone(),
                            to_cm_name(&p.name),
                            self.peel_cm_newtype(&p.ty),
                        )
                    })
                    .collect();
                let return_type = unwrap_async_call_if_async(
                    method.is_async,
                    &method.return_type,
                    &self.type_decls,
                );
                self.register(
                    &effect.name,
                    &method.name,
                    &cm_import,
                    method.is_async,
                    params,
                    return_type,
                )?;
            }
        }
        Ok(())
    }

    /// Register named type decls that reach the library's public surface from a
    /// module other than the entry: a local submodule (a facade re-exports them,
    /// and an `export fn` signature names them), or the stdlib for a newtype the
    /// surface names. Registered under the same lib-local interface FQ as the
    /// entry module's own types, so lift/lower resolves them uniformly. Each
    /// is registered under the name it carries, which may be an `export use`
    /// alias, and keeps its declaration, so resolution can locate a type the
    /// entry-FQ mapping cannot. A stdlib module signs nothing of its own, so the
    /// types these items sign are bound here.
    pub fn register_lib_local_items<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a (ModuleSource, Item)>,
        iface_fq: &str,
        resolutions: &Resolutions,
    ) {
        for (_, item) in items {
            assert!(
                item.type_decl().is_some(),
                "a library publishes only types beyond its entry module"
            );
            self.type_decls.bind_signed([item], resolutions);
            self.register_lib_local_item(item, iface_fq);
        }
    }

    /// `item`'s type references must already answer their source interface: the
    /// caller binds them by declaration.
    fn register_lib_local_item(&mut self, item: &Item, iface_fq: &str) {
        if let Some(decl) = item.type_decl() {
            self.register_decl(item.id(), iface_fq, decl.name);
        }
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
                    .map(|f| (to_cm_name(&f.name), f.ty.clone()))
                    .collect();
                let wado_fields: Vec<(String, String, Type)> = struct_def
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), to_cm_name(&f.name), f.ty.clone()))
                    .collect();
                register_unique(
                    &mut self.structs,
                    "struct",
                    iface_fq.to_string(),
                    struct_def.name.clone(),
                    (to_cm_name(&struct_def.name), fields, wado_fields),
                );
            }
            Item::Flags(flags_def) => {
                let members: Vec<String> = flags_def
                    .flags
                    .iter()
                    .map(|m| to_cm_name(&m.name))
                    .collect();
                register_unique(
                    &mut self.flags,
                    "flags",
                    iface_fq.to_string(),
                    flags_def.name.clone(),
                    (to_cm_name(&flags_def.name), members),
                );
            }
            Item::Enum(enum_def) => {
                let variants: Vec<String> =
                    enum_def.cases.iter().map(|c| to_cm_name(&c.name)).collect();
                register_unique(
                    &mut self.enums,
                    "enum",
                    iface_fq.to_string(),
                    enum_def.name.clone(),
                    (to_cm_name(&enum_def.name), variants),
                );
            }
            Item::Variant(variant_def) => {
                let cases: Vec<CmVariantCase> = variant_def
                    .cases
                    .iter()
                    .map(|c| CmVariantCase {
                        cm_name: to_cm_name(&c.name),
                        wado_name: c.name.clone(),
                        payload: c.payload.clone(),
                    })
                    .collect();
                register_unique(
                    &mut self.variants,
                    "variant",
                    iface_fq.to_string(),
                    variant_def.name.clone(),
                    (to_cm_name(&variant_def.name), cases),
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
        for item in &module.items {
            if let Item::World(world) = item {
                world_registry.register(
                    world,
                    |name| interface_decls.export_lookup(name, self),
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

    /// The CM name of the resource declaration `decl`, where one is registered.
    #[must_use]
    pub fn resource_cm_name_of(&self, decl: AstId) -> Option<&str> {
        let registration = self.registrations.get(&decl)?;
        self.get_resource_cm_name_by_source(&registration.interface, &registration.name)
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

    /// The CM resources `ty` references at any depth, inside the records and
    /// variants it names too, as `(declaring_interface, wado_name)`.
    fn resources_in_type(
        &self,
        ty: &Type,
        out: &mut IndexSet<(String, String)>,
        visited: &mut IndexSet<(String, String)>,
    ) {
        ty.for_each(&mut |ty| {
            let Type::Named(named) = ty else {
                return;
            };
            let Some(source) = self.source_interface(named) else {
                return;
            };
            let key = (source, named.name.clone());
            if !visited.insert(key.clone()) {
                return;
            }
            let (source, name) = &key;
            for member in self.member_types(source, name) {
                self.resources_in_type(member, out, visited);
            }
            if self.get_resource_cm_name_by_source(source, name).is_some() {
                out.insert(key);
            }
        });
    }

    /// The CM resources every signature of `funcs` references, as
    /// `(declaring_interface, wado_name)`.
    pub fn resources_in_signatures(&self, funcs: &[&CmFunctionInfo]) -> IndexSet<(String, String)> {
        let mut out = IndexSet::default();
        let mut visited = IndexSet::default();
        for func in funcs {
            for ty in func
                .return_type
                .iter()
                .chain(func.params.iter().map(|(_, _, ty)| ty))
            {
                self.resources_in_type(ty, &mut out, &mut visited);
            }
        }
        out
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
        // A types-only interface (`wasi:cli/types`) has no functions, so only
        // the types it declares name it.
        let bundled: IndexSet<&str> = self
            .interfaces
            .keys()
            .map(String::as_str)
            .chain(
                CmTypeKind::ALL
                    .into_iter()
                    .flat_map(|kind| self.kind_keys(kind).map(|(fq, _)| fq.as_str())),
            )
            .filter(|fq| cm_interface_module(fq).as_ref() == Some(&wanted))
            .collect();
        bundled.into_iter().collect()
    }

    /// A type `iface_fq` already declares, if any. A bundled `wasi:` / `core:`
    /// interface is absent from `cm_interface_module_sources` by design, so that
    /// map alone reports the whole stdlib as unowned; what the keyspaces hold is
    /// the fact itself.
    fn existing_cm_decl<'a>(&'a self, iface_fq: &'a str) -> Option<&'a str> {
        self.declared_type_names(iface_fq).next()
    }

    /// Resolve a newtype *reference* to its aliased base type by its declaring
    /// interface (`NamedType::source_interface`), so two modules that declare a
    /// same-named newtype resolve to their own base — never a bare-name guess.
    /// A source-less reference is a known newtype only as a prelude one.
    fn resolve_newtype_ref(&self, named: &NamedType) -> Option<&Type> {
        match self.source_interface(named) {
            Some(source) => self.get_newtype_by_source(&source, &DeclName::new(&named.name)),
            None => self.type_decls.newtype_base(named),
        }
    }

    /// The registration source and base type of a newtype the compiled package
    /// declares, which the boundary keeps as a named alias (issue #1456).
    pub fn local_newtype_base(&self, source: Option<&str>, name: &str) -> Option<(&str, &Type)> {
        let source = source?;
        if self.is_cm_source(source) || self.is_unrestricted_resource(source, name) {
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

    /// The CM name of the `ErrorCode` that `interface_path` declares itself, in
    /// either shape: a variant (`wasi:filesystem/types`, `wasi:sockets/types`)
    /// or an enum (`wasi:cli/types`). Exact: an interface that declares none
    /// answers `None` rather than borrowing another package's.
    pub fn own_error_code_cm_name(&self, interface_path: &str) -> Option<&str> {
        self.get_variant_cm_name_by_source(interface_path, ERROR_CODE_WADO_NAME)
            .or_else(|| self.get_enum_cm_name_by_source(interface_path, ERROR_CODE_WADO_NAME))
    }

    /// Whether `interface_fq` declares a function, which a types-only interface
    /// (`wasi:cli/types`) does not.
    pub fn declares_functions(&self, interface_fq: &str) -> bool {
        self.interfaces.contains_key(interface_fq)
    }

    /// The CM name and base of the newtype `interface_fq` declares as `name`.
    pub fn newtype(&self, interface_fq: &str, name: &str) -> Option<(&str, &Type)> {
        let key = (interface_fq.to_string(), name.to_string());
        Some((self.newtype_cm_names.get(&key)?, self.newtypes.get(&key)?))
    }

    /// The Wado name of every type `interface_fq` declares.
    pub fn declared_type_names<'a>(
        &'a self,
        interface_fq: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        CmTypeKind::ALL.into_iter().flat_map(move |kind| {
            self.kind_keys(kind)
                .filter(move |(fq, _)| fq == interface_fq)
                .map(|(_, name)| name.as_str())
        })
    }

    /// Every type declaration registered under `interface_fq`, with its
    /// registered name and a type site bound to it.
    pub fn declared_types<'a>(
        &'a self,
        interface_fq: &'a str,
    ) -> impl Iterator<Item = (&'a str, Type)> + 'a {
        self.registrations
            .iter()
            .filter(move |(_, registration)| registration.interface == interface_fq)
            .map(|(&decl, registration)| {
                let name = registration.name.as_str();
                let site =
                    self.type_decls
                        .decl_type(decl, name.to_string(), Vec::new(), Span::default());
                (name, site)
            })
    }

    /// A type site bound to the declaration `interface_fq` registers as `name`.
    pub fn declared_type(&self, interface_fq: &str, name: &str) -> Option<Type> {
        self.declared_types(interface_fq)
            .find(|(declared, _)| *declared == name)
            .map(|(_, site)| site)
    }

    /// The kind of the value type `interface_fq` declares as `name`, asked in
    /// the order [`CmTypeGen::ast_type_to_cm`] asks: a variant, a record, an
    /// enum, then flags. `None` for a resource or a newtype, which no instance
    /// spells as a type of its own.
    pub fn value_type_kind(&self, interface_fq: &str, name: &str) -> Option<CmTypeKind> {
        if self.declares_kind(CmTypeKind::Resource, interface_fq, name) {
            return None;
        }
        [
            CmTypeKind::Variant,
            CmTypeKind::Struct,
            CmTypeKind::Enum,
            CmTypeKind::Flags,
        ]
        .into_iter()
        .find(|&kind| self.declares_kind(kind, interface_fq, name))
    }

    /// The CM name of the value type `interface_fq` declares as `name`.
    pub fn value_type_cm_name(&self, interface_fq: &str, name: &str) -> Option<&str> {
        match self.value_type_kind(interface_fq, name)? {
            CmTypeKind::Variant => self.get_variant_cm_name_by_source(interface_fq, name),
            CmTypeKind::Struct => self.get_struct_cm_name_by_source(interface_fq, name),
            CmTypeKind::Enum => self.get_enum_cm_name_by_source(interface_fq, name),
            CmTypeKind::Flags => self.get_flags_cm_name_by_source(interface_fq, name),
            CmTypeKind::Resource | CmTypeKind::Newtype => unreachable!(),
        }
    }

    /// The types another interface declares that `interface`'s instance type
    /// reaches through `types`, as `(declaring_interface, wado_name)`: the
    /// resources it aliases and the value types it aliases rather than spells.
    /// A type `interface` declares itself is spelled there, so the walk goes on
    /// into its fields and payloads.
    pub fn foreign_types_in<'a>(
        &self,
        interface: &str,
        types: impl IntoIterator<Item = &'a Type>,
    ) -> IndexSet<(String, String)> {
        let mut out = IndexSet::default();
        let mut visited = IndexSet::default();
        for ty in types {
            self.foreign_types_in_type(interface, ty, &mut out, &mut visited);
        }
        out
    }

    fn foreign_types_in_type(
        &self,
        interface: &str,
        ty: &Type,
        out: &mut IndexSet<(String, String)>,
        visited: &mut IndexSet<String>,
    ) {
        self.resolve_type_preserving_local_newtypes(ty)
            .for_each(&mut |ty| {
                let Type::Named(named) = ty else {
                    return;
                };
                let Some(source) = self
                    .source_interface(named)
                    .filter(|source| self.is_cm_source(source))
                else {
                    return;
                };
                let name = &named.name;
                if source != interface {
                    if self.declares_kind(CmTypeKind::Resource, &source, name)
                        || self.value_type_kind(&source, name).is_some()
                    {
                        out.insert((source, name.clone()));
                    }
                    return;
                }
                if !visited.insert(name.clone()) {
                    return;
                }
                for member in self.member_types(&source, name) {
                    self.foreign_types_in_type(interface, member, out, visited);
                }
            });
    }

    /// The field types of the record, or the payload types of the variant,
    /// `interface_fq` declares as `name`; none for any other type.
    fn member_types(&self, interface_fq: &str, name: &str) -> Vec<&Type> {
        if let Some(fields) = self.get_struct_fields_by_source(interface_fq, name) {
            fields.iter().map(|(_, field)| field).collect()
        } else if let Some(cases) = self.get_variant_cases_by_source(interface_fq, name) {
            cases
                .iter()
                .filter_map(|case| case.payload.as_ref())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Whether `interface_fq` declares `name`, in any of the kinds a reference
    /// at the CM boundary can name.
    pub fn declares_wado_name(&self, interface_fq: &str, name: &str) -> bool {
        CmTypeKind::ALL
            .into_iter()
            .any(|kind| self.declares_kind(kind, interface_fq, name))
    }

    /// The interface declaring `decl`, and the CM name it exports `decl` under.
    pub fn declared_cm_type(&self, decl: &CmDecl) -> Option<(&str, &str)> {
        self.module_interfaces(decl.module())
            .into_iter()
            .find_map(|fq| Some((fq, self.cm_type_name(fq, decl.name())?)))
    }

    /// The CM name `interface_fq` exports its type `name` under.
    pub fn cm_type_name(&self, interface_fq: &str, name: &str) -> Option<&str> {
        // A newtype is peeled before it reaches the boundary, so these are the
        // kinds the ABI spells.
        self.get_resource_cm_name_by_source(interface_fq, name)
            .or_else(|| self.get_struct_cm_name_by_source(interface_fq, name))
            .or_else(|| self.get_variant_cm_name_by_source(interface_fq, name))
            .or_else(|| self.get_enum_cm_name_by_source(interface_fq, name))
            .or_else(|| self.get_flags_cm_name_by_source(interface_fq, name))
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
                // An extern-handle handle registers here to reach the `f64` every
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
    ) -> Option<CmGetterResource> {
        let ty = return_type.as_ref()?;

        // Check for Option<ResourceName> pattern
        if let Type::Generic(g) = ty
            && self.type_decls.is(g.id, CompilerItem::Option)
            && let Type::Named(inner) = &g.args[0]
            && let Some(source) = self.source_interface(inner)
            && let Some(cm_name) = self.get_resource_cm_name_by_source(&source, &inner.name)
        {
            return Some(CmGetterResource {
                wado_name: inner.name.clone(),
                cm_name: cm_name.to_string(),
                source,
            });
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
        let known = KnownTypes {
            decls: &self.type_decls,
            enums: self
                .enums
                .keys()
                .chain(self.variants.keys())
                .chain(self.flags.keys())
                .map(|(_, n)| n.as_str())
                .collect(),
            resources: self.resources.keys().map(|(_, n)| n.as_str()).collect(),
            structs: self.structs.keys().map(|(_, n)| n.as_str()).collect(),
        };
        // Parameter types at the boundary's view (an extern handle is an `f64`),
        // and the return type with its newtypes (`Mark`, `Duration`, `Instant`)
        // resolved to their bases.
        func.params
            .iter()
            .all(|(_, _, ty)| known.param_supported(&self.resolve_type(ty)))
            && func
                .return_type
                .as_ref()
                .is_none_or(|ty| known.return_supported(&self.resolve_type(ty)))
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
    ///
    /// `Err` when the CM function is already bound with another signature.
    pub fn register(
        &mut self,
        interface_name: &str,
        method_name: &str,
        wasi: &CmImport,
        is_async: bool,
        params: Vec<(String, String, Type)>,
        return_type: Option<Type>,
    ) -> Result<(), String> {
        let interface_path = wasi.interface_path();
        let ValueParams {
            params,
            callbacks,
            list_args,
        } = self.value_params(params);
        let func_info = CmFunctionInfo {
            interface_name: interface_name.to_string(),
            method_name: method_name.to_string(),
            wasi_func_name: cm_function_name(wasi, method_name),
            interface_path: interface_path.clone(),
            package: wasi.package.clone(),
            is_async,
            params,
            list_args,
            callbacks,
            return_type,
            world: None,
        };

        if self.bind(&func_info)? {
            self.interfaces
                .entry(interface_path)
                .or_default()
                .push(func_info);
        }
        Ok(())
    }

    /// `params` at their GC-level value types, closures as `u32` keys and a
    /// `&ListView<E>` as the `List<E>` it crosses as.
    fn value_params(&self, params: Vec<(String, String, Type)>) -> ValueParams {
        let mut callbacks = Vec::new();
        let mut list_args = Vec::new();
        let params = params
            .into_iter()
            .enumerate()
            .map(|(i, (name, cm_name, ty))| {
                if let Type::Reference(view) = &ty
                    && let Type::Generic(g) = view.as_ref()
                    && self.type_decls.is(g.id, CompilerItem::ListView)
                {
                    list_args.push(i);
                    let list =
                        self.type_decls
                            .item_type(CompilerItem::List, g.args.clone(), ty.span());
                    return (name, cm_name, self.value_type(&list));
                }
                if !matches!(ty, Type::Function(_)) {
                    return (name, cm_name, self.value_type(&ty));
                }
                let key = NamedType::new(AstId::fresh(), "u32".to_string(), ty.span());
                callbacks.push((i, ty));
                (name, cm_name, Type::Named(key))
            })
            .collect();
        ValueParams {
            params,
            callbacks,
            list_args,
        }
    }

    /// Binds `func_info`'s CM function; `Ok(false)` when it is already bound
    /// with the same signature, by any Wado name.
    fn bind(&mut self, func_info: &CmFunctionInfo) -> Result<bool, String> {
        let key = func_info.cm_identifier();
        if let Some(bound) = self.functions.get(&key) {
            // One import serves every binding of the function, so they must
            // agree on what crosses it.
            if func_info.signature_key(self) != bound.signature_key(self) {
                let (name, bound_name) = (func_info.wado_name(), bound.wado_name());
                let bindings = if name == bound_name {
                    format!("two `{name}`")
                } else {
                    format!("`{bound_name}` and `{name}`")
                };
                return Err(format!(
                    "{bindings} bind `{key}` with different signatures: make them agree"
                ));
            }
            return Ok(false);
        }
        self.functions.insert(key, func_info.clone());
        Ok(true)
    }

    /// Register the world-level function imports (Phase 9) of `component`'s
    /// binding `module`: each a bodyless free function carrying a `#[cm]`
    /// world-import boundary. Stored like an interface method so the adapter
    /// pipeline reuses the interface-method path.
    fn register_world_imports(
        &mut self,
        module: &ast::Module,
        component: &ModuleSource,
    ) -> Result<(), String> {
        for item in &module.items {
            let Item::Function(func) = item else {
                continue;
            };
            let Some(cm_func_name) = world_import_of(&func.attrs) else {
                continue;
            };
            let params = self.cm_params_of(
                func,
                |param| {
                    panic!(
                        "missing #[cm_params] for param '{param}' in world import {}",
                        func.name
                    )
                },
                Type::clone,
            );
            let ValueParams {
                params,
                callbacks,
                list_args,
            } = self.value_params(params);
            let namespace = component
                .wasm_canonical_namespace()
                .expect("a world function comes from a component asset");
            let func_info = CmFunctionInfo {
                // A world import sits above any interface, so it has no
                // package or interface path to name.
                interface_name: String::new(),
                method_name: func.name.clone(),
                wasi_func_name: cm_func_name.to_string(),
                interface_path: String::new(),
                package: String::new(),
                is_async: func.is_async,
                params,
                list_args,
                callbacks,
                // As for an interface method, the CM-ABI return type drops the
                // `AsyncCall<T>` wrapper user code keeps seeing.
                return_type: unwrap_async_call_if_async(
                    func.is_async,
                    &func.return_type,
                    &self.type_decls,
                ),
                world: Some(WorldFunctionSite {
                    component: component.clone(),
                    label: cm_world_import_label(&namespace, cm_func_name),
                }),
            };
            self.bind(&func_info)?;
            self.world_imports
                .insert(func.id, func_info.cm_identifier());
        }
        Ok(())
    }

    /// The key of the world-level function import (Phase 9) the function
    /// `decl` declares.
    #[must_use]
    pub fn world_import_key(&self, decl: AstId) -> Option<&str> {
        self.world_imports.get(&decl).map(String::as_str)
    }

    /// Every world-level function import (Phase 9).
    pub fn world_import_functions(&self) -> impl Iterator<Item = &CmFunctionInfo> + '_ {
        self.functions
            .values()
            .filter(|func| func.is_world_function())
    }

    /// The bound CM function whose [`CmFunctionInfo::cm_identifier`] is `key`.
    pub fn get_function(&self, key: &str) -> Option<&CmFunctionInfo> {
        self.functions.get(key)
    }

    /// The key of the function that the operation `operation` of the interface
    /// or resource declared at `owner` binds, from its `#[cm]` payload
    /// `cm_name`. A guest effect's operation carries none: its interface is the
    /// one minted for `owner`.
    #[must_use]
    pub fn operation_key(
        &self,
        owner: AstId,
        cm_name: Option<&str>,
        operation: &str,
    ) -> Option<String> {
        match cm_name {
            Some(cm_name) => cm_payload_function_key(cm_name, operation),
            None => Some(cm_function_key(self.guest_effects.get(&owner)?, operation)),
        }
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
    /// ABI: a bundled CM namespace (`wasi:`, a WIT-generated `core:*` package)
    /// or a component import (whose package namespace is arbitrary, so tracked
    /// explicitly).
    pub fn is_cm_source(&self, source: &str) -> bool {
        CmNamespace::split_specifier(source).is_some()
            || stdlib::is_wit_core_interface(source)
            || self.component_interfaces.contains(source)
            || self.is_dependency_import(source)
    }

    /// Flatten a CM type into its canonical-ABI core value sequence. Single
    /// source of truth for how a value-type maps to flat core params/results.
    pub fn cm_flatten(&self, ty: &Type) -> Vec<CmValType> {
        let mut out = Vec::new();
        self.cm_flatten_into(ty, &mut out);
        out
    }

    /// Whether a call taking `params` passes them through one buffer rather
    /// than as flat core values (`flatten_functype`).
    pub fn params_in_buffer<'t>(
        &self,
        params: impl IntoIterator<Item = &'t Type>,
        is_async: bool,
    ) -> bool {
        let flat_count = params.into_iter().map(|ty| self.cm_flatten(ty).len()).sum();
        flat_params_in_buffer(flat_count, is_async)
    }

    fn cm_flatten_into(&self, ty: &Type, out: &mut Vec<CmValType>) {
        let resolved = self.resolve_type(ty);
        match &resolved {
            Type::Named(named) if self.type_decls.is(named.id, CompilerItem::String) => {
                out.push(CmValType::I32);
                out.push(CmValType::I32);
            }
            Type::Named(named) => match named.name.as_str() {
                "bool" | "u8" | "i8" | "u16" | "i16" | "i32" | "u32" | "char" => {
                    out.push(CmValType::I32);
                }
                "i64" | "u64" => out.push(CmValType::I64),
                "f32" => out.push(CmValType::F32),
                "f64" => out.push(CmValType::F64),
                UNIT_TYPE_NAME => {}
                name => {
                    if let Some(source) = self.source_interface(named) {
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
            Type::Generic(g) => match self.type_decls.item_at(g.id) {
                // `map<K, V>` despecializes to `list<tuple<K, V>>`, so it
                // carries that type's `(ptr, count)` pair.
                Some(CompilerItem::List | CompilerItem::TreeMap) => {
                    out.push(CmValType::I32);
                    out.push(CmValType::I32);
                }
                Some(CompilerItem::Stream | CompilerItem::Future) => out.push(CmValType::I32),
                Some(CompilerItem::Option) => {
                    out.push(CmValType::I32);
                    self.cm_flatten_into(&g.args[0], out);
                }
                Some(CompilerItem::Result) => {
                    out.push(CmValType::I32);
                    self.push_joined_payloads(
                        [Some(g.args[0].clone()), Some(g.args[1].clone())].into_iter(),
                        out,
                    );
                }
                _ => panic!("unsupported generic type for CM flattening: {}", g.name),
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
            Type::Reference(inner) | Type::MutReference(inner)
                if !keep_handles && self.extern_handle(ty).is_some() =>
            {
                self.resolve_type_impl(inner, preserve_local, keep_handles)
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

use crate::compiler_item::CompilerItem;
use wasm_encoder::{
    Alias, ComponentOuterAliasKind, ComponentTypeRef, ComponentValType, InstanceType,
    PrimitiveValType, TypeBounds,
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
    /// 🗺️ `map<K, V>` — the `list<tuple<K, V>>` bytes under their own type
    /// constructor, so the intent survives into the WIT a consumer reads.
    Map {
        key: ComponentValType,
        value: ComponentValType,
    },
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
    /// Bring `outer_idx`, a type of the enclosing component, into this sink's
    /// type space; return the index to reference it by.
    fn alias_outer(&mut self, outer_idx: u32) -> u32;
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

    fn alias_outer(&mut self, _outer_idx: u32) -> u32 {
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

    fn alias_outer(&mut self, outer_idx: u32) -> u32 {
        self.it.alias(Alias::Outer {
            kind: ComponentOuterAliasKind::Type,
            count: 1,
            index: outer_idx,
        });
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
        CmDefined::Map { key, value } => enc.map(key, value),
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
        _ => return None,
    };
    Some(prim)
}

/// The CM primitive `named` is: a Wado primitive by its name, which no
/// declaration can take, or the `String` declaration.
pub fn cm_primitive_of(named: &NamedType, decls: &CmTypeDecls) -> Option<PrimitiveValType> {
    if decls.is(named.id, CompilerItem::String) {
        return Some(PrimitiveValType::String);
    }
    wado_primitive_name_to_cm(&named.name)
}

/// Registry-driven recursive builder for Component Model types.
///
/// Given a Wado [`Type`] and the [`CmInterfaceRegistry`], it walks the type,
/// resolves named types to their CM metadata, and emits the corresponding CM
/// defined types through a [`CmTypeSink`] — an [`InstanceSink`] for interface
/// instance types, or the codegen top-level sink for bare world exports.
/// Type-index allocation lives in the sink, so one engine serves both targets.
pub struct CmTypeGen {
    cache: IndexMap<String, u32>,
    /// The interface whose instance type the sink builds, so a caller can tell
    /// the types that instance exports itself from those another declares.
    emitting: Option<String>,
    /// The CM names this engine has exported into its sink, so a caller can ask
    /// what the walk put into the interface's namespace instead of predicting it.
    named_exports: IndexSet<String>,
    /// Named types the enclosing component already holds, by cache key, for
    /// the walk to alias on first reach rather than spell again.
    outer: IndexMap<String, u32>,
}

impl Default for CmTypeGen {
    fn default() -> Self {
        Self::new()
    }
}

fn declared_resource(resource_exports: &IndexMap<&str, u32>, cm_name: &str, source: &str) -> u32 {
    *resource_exports.get(cm_name).unwrap_or_else(|| {
        panic!(
            "resource `{cm_name}` from `{source}` is not among the resources this instance \
             declares ({}), so a handle to it has no type here",
            resource_exports
                .keys()
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

impl CmTypeGen {
    pub fn new() -> Self {
        Self {
            cache: IndexMap::default(),
            emitting: None,
            named_exports: IndexSet::default(),
            outer: IndexMap::default(),
        }
    }

    pub fn emitting_into(interface: &str) -> Self {
        Self {
            emitting: Some(interface.to_string()),
            ..Self::new()
        }
    }

    /// Let the walk reach the value type `name` that `owner` declares as `kind`
    /// through `outer_idx`, a type of the enclosing component. Aliasing keeps
    /// the one type its declaring interface exports, where a structural copy
    /// would be a type of its own.
    pub fn register_outer(&mut self, kind: CmTypeKind, owner: &str, name: &str, outer_idx: u32) {
        self.outer
            .insert(Self::named_key(kind, owner, name), outer_idx);
    }

    /// The type the walk already has under `key`, aliased in from the
    /// enclosing component on first reach when [`Self::register_outer`] put it
    /// there.
    fn existing(&mut self, sink: &mut dyn CmTypeSink, key: &str) -> Option<u32> {
        if let Some(&idx) = self.cache.get(key) {
            return Some(idx);
        }
        let idx = sink.alias_outer(*self.outer.get(key)?);
        self.cache.insert(key.to_string(), idx);
        Some(idx)
    }

    pub fn emitting(&self) -> Option<&str> {
        self.emitting.as_deref()
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

    /// A type name is unique only within the interface declaring it:
    /// `ErrorCode` is one type in `wasi:http/types` and another in
    /// `wasi:filesystem/types`.
    fn named_key(kind: CmTypeKind, owner: &str, name: &str) -> String {
        format!("{kind:?}:{owner}#{name}")
    }

    /// Register the `own` and `borrow` handles the sink already defines over
    /// resource `cm_name`, so the walk reuses them rather than minting its own.
    pub fn register_resource_handles(&mut self, cm_name: &str, own: u32, borrow: u32) {
        self.cache.insert(Self::own_key(cm_name), own);
        self.cache.insert(Self::borrow_key(cm_name), borrow);
    }

    fn own_key(resource_cm_name: &str) -> String {
        format!("own:{resource_cm_name}")
    }

    fn borrow_key(resource_cm_name: &str) -> String {
        format!("borrow:{resource_cm_name}")
    }

    /// Define a variant type and its named export, returning the exported type index.
    ///
    /// Handles payload types recursively via `ast_type_to_cm`.
    fn define_variant(
        &mut self,
        sink: &mut dyn CmTypeSink,
        owner: &str,
        name: &str,
        cm_name: &str,
        cases: &[CmVariantCase],
        cm_interface_registry: &CmInterfaceRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> u32 {
        let cache_key = Self::named_key(CmTypeKind::Variant, owner, name);
        if let Some(idx) = self.existing(sink, &cache_key) {
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

    /// Export `base` under `cm_name`: the named alias `owner`'s newtype `name`
    /// is (`type meters = f64`).
    pub fn define_alias(
        &mut self,
        sink: &mut dyn CmTypeSink,
        owner: &str,
        name: &str,
        cm_name: &str,
        base: &Type,
        cm_interface_registry: &CmInterfaceRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> ComponentValType {
        // The owner keys the cache so same-named newtypes stay distinct and
        // every reference dedups to one alias.
        let cache_key = Self::named_key(CmTypeKind::Newtype, owner, name);
        if let Some(&idx) = self.cache.get(&cache_key) {
            return ComponentValType::Type(idx);
        }
        // Peel the base first: an imported-newtype base resolves to its
        // primitive, avoiding the unsupported-name panic in `ast_type_to_cm`; a
        // local-newtype base recurses as a nested alias.
        let base = cm_interface_registry.resolve_type_preserving_local_newtypes(base);
        let base_idx =
            match self.ast_type_to_cm(sink, &base, cm_interface_registry, resource_exports) {
                ComponentValType::Primitive(prim) => sink.define(CmDefined::Primitive(prim)),
                ComponentValType::Type(idx) => idx,
            };
        let named_idx = self.export_named(sink, cm_name, base_idx);
        self.cache.insert(cache_key, named_idx);
        ComponentValType::Type(named_idx)
    }

    /// Define a record (struct) type and its named export, returning the exported type index.
    fn define_record(
        &mut self,
        sink: &mut dyn CmTypeSink,
        owner: &str,
        name: &str,
        cm_name: &str,
        fields: &[(String, Type)],
        cm_interface_registry: &CmInterfaceRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> u32 {
        let cache_key = Self::named_key(CmTypeKind::Struct, owner, name);
        if let Some(idx) = self.existing(sink, &cache_key) {
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
        let cache_key = Self::borrow_key(resource_cm_name);
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

    /// Define a map type, returning the type index.
    fn define_map(
        &mut self,
        sink: &mut dyn CmTypeSink,
        key: ComponentValType,
        value: ComponentValType,
        key_suffix: &str,
    ) -> u32 {
        let cache_key = format!("map:{key_suffix}");
        if let Some(&idx) = self.cache.get(&cache_key) {
            return idx;
        }
        let idx = sink.define(CmDefined::Map { key, value });
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
    /// The `resource_exports` maps CM resource names (e.g., "fields") to the
    /// resource's own type index, never to a handle over it.
    pub fn ast_type_to_cm(
        &mut self,
        sink: &mut dyn CmTypeSink,
        ty: &Type,
        cm_interface_registry: &CmInterfaceRegistry,
        resource_exports: &IndexMap<&str, u32>,
    ) -> ComponentValType {
        let decls = cm_interface_registry.type_decls();
        match ty {
            Type::Named(named) => {
                if let Some(prim) = cm_primitive_of(named, decls) {
                    return ComponentValType::Primitive(prim);
                }
                let name = named.name.as_str();
                // Preserve a local newtype as a named CM alias
                // (`type meters = f64`) so the structural type matches
                // `wado wit` instead of erasing to its base (issue #1456).
                let source = cm_interface_registry.source_interface(named);
                if let Some((canonical_source, base)) = cm_interface_registry
                    .local_newtype_base(source.as_deref(), name)
                    .map(|(s, ty)| (s.to_string(), ty.clone()))
                {
                    return self.define_alias(
                        sink,
                        &canonical_source,
                        name,
                        &to_cm_name(name),
                        &base,
                        cm_interface_registry,
                        resource_exports,
                    );
                }
                let source_owned: String = cm_interface_registry
                    .source_interface(named)
                    .unwrap_or_else(|| {
                        panic!(
                            "unresolved CM named type reference `{name}` while emitting CM instance"
                        )
                    });
                let source = source_owned.as_str();
                if let Some(cm_name) =
                    cm_interface_registry.get_resource_cm_name_by_source(source, name)
                {
                    let cache_key = Self::own_key(cm_name);
                    if let Some(&idx) = self.cache.get(&cache_key) {
                        return ComponentValType::Type(idx);
                    }
                    let export_idx = declared_resource(resource_exports, cm_name, source);
                    let idx = sink.define(CmDefined::Own(export_idx));
                    self.cache.insert(cache_key, idx);
                    ComponentValType::Type(idx)
                } else if let Some(cases) =
                    cm_interface_registry.get_variant_cases_by_source(source, name)
                {
                    let cases = cases.to_vec();
                    let cm_name = cm_interface_registry
                        .get_variant_cm_name_by_source(source, name)
                        .expect("variant cm_name present when cases are")
                        .to_string();
                    let idx = self.define_variant(
                        sink,
                        source,
                        name,
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
                        source,
                        name,
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
                    let cache_key = Self::named_key(CmTypeKind::Enum, source, name);
                    if let Some(idx) = self.existing(sink, &cache_key) {
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
                    let cache_key = Self::named_key(CmTypeKind::Flags, source, name);
                    if let Some(idx) = self.existing(sink, &cache_key) {
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
            Type::Reference(inner) | Type::MutReference(inner) => {
                if let Type::Named(n) = inner.as_ref()
                    && let Some(source) = cm_interface_registry.source_interface(n)
                    && let Some(cm_name) =
                        cm_interface_registry.get_resource_cm_name_by_source(&source, &n.name)
                {
                    let export_idx = declared_resource(resource_exports, cm_name, &source);
                    let idx = self.define_borrow(sink, export_idx, cm_name);
                    return ComponentValType::Type(idx);
                }
                panic!("unsupported reference type for CM instance: {ty:?}")
            }
            Type::Generic(generic) => match decls.item_at(generic.id) {
                Some(CompilerItem::List) => {
                    let elem_cm = self.ast_type_to_cm(
                        sink,
                        &generic.args[0],
                        cm_interface_registry,
                        resource_exports,
                    );
                    let key = cm_interface_registry.type_key(&generic.args[0]);
                    let idx = self.define_list(sink, elem_cm, &key);
                    ComponentValType::Type(idx)
                }
                Some(CompilerItem::TreeMap) => {
                    let key_cm = self.ast_type_to_cm(
                        sink,
                        &generic.args[0],
                        cm_interface_registry,
                        resource_exports,
                    );
                    let value_cm = self.ast_type_to_cm(
                        sink,
                        &generic.args[1],
                        cm_interface_registry,
                        resource_exports,
                    );
                    let key = format!(
                        "{},{}",
                        cm_interface_registry.type_key(&generic.args[0]),
                        cm_interface_registry.type_key(&generic.args[1])
                    );
                    let idx = self.define_map(sink, key_cm, value_cm, &key);
                    ComponentValType::Type(idx)
                }
                Some(CompilerItem::Result) => {
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
                        cm_interface_registry.type_key(&generic.args[0]),
                        cm_interface_registry.type_key(&generic.args[1])
                    );
                    let idx = self.define_result(sink, ok_type, err_type, &key);
                    ComponentValType::Type(idx)
                }
                Some(CompilerItem::Option) => {
                    let inner_cm = self.ast_type_to_cm(
                        sink,
                        &generic.args[0],
                        cm_interface_registry,
                        resource_exports,
                    );
                    let key = cm_interface_registry.type_key(&generic.args[0]);
                    let idx = self.define_option(sink, inner_cm, &key);
                    ComponentValType::Type(idx)
                }
                Some(CompilerItem::Stream) => {
                    let (elem, key) = if generic.args.is_empty() {
                        (None, "unit".to_string())
                    } else {
                        let cm = self.ast_type_to_cm(
                            sink,
                            &generic.args[0],
                            cm_interface_registry,
                            resource_exports,
                        );
                        (Some(cm), cm_interface_registry.type_key(&generic.args[0]))
                    };
                    let idx = self.define_stream(sink, elem, &key);
                    ComponentValType::Type(idx)
                }
                Some(CompilerItem::Future) => {
                    let (inner, key) = if generic.args.is_empty() {
                        (None, "unit".to_string())
                    } else {
                        let cm = self.ast_type_to_cm(
                            sink,
                            &generic.args[0],
                            cm_interface_registry,
                            resource_exports,
                        );
                        (Some(cm), cm_interface_registry.type_key(&generic.args[0]))
                    };
                    let idx = self.define_future(sink, inner, &key);
                    ComponentValType::Type(idx)
                }
                Some(CompilerItem::AsyncCall) => {
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
                let key = cm_interface_registry.type_key(ty);
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
        // A stream or future is a handle, a result or option a discriminant, a
        // list or map a GC reference, a reference a `borrow` or `own` handle:
        // each is an i32 in a WASI context.
        Type::Generic(_) | Type::Reference(_) | Type::MutReference(_) | Type::Tuple(_) => {
            ValType::I32
        }
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

/// The types a support check accepts by name: the compiler items, and the
/// enums, resources and records the registry knows.
struct KnownTypes<'a> {
    decls: &'a CmTypeDecls,
    enums: IndexSet<&'a str>,
    resources: IndexSet<&'a str>,
    structs: IndexSet<&'a str>,
}

impl KnownTypes<'_> {
    /// Whether a parameter type, its aliases already resolved, has a CM
    /// lowering. A parameter always carries a type, so `()` has none.
    fn param_supported(&self, ty: &Type) -> bool {
        match ty {
            Type::Named(named) => self.named_supported(named),
            Type::Generic(generic) => {
                matches!(
                    self.decls.item_at(generic.id),
                    Some(
                        CompilerItem::Stream
                            | CompilerItem::Result
                            | CompilerItem::Future
                            | CompilerItem::Option
                            | CompilerItem::List
                            | CompilerItem::TreeMap
                    )
                ) && !generic.args.iter().any(mentions_empty_tuple)
            }
            Type::Reference(inner) | Type::MutReference(inner) => {
                // borrow<resource> - passed as i32 handle at CM boundary
                if let Type::Named(named) = inner.as_ref() {
                    self.resources.contains(named.name.as_str())
                } else {
                    false
                }
            }
            Type::Tuple(elems) if !elems.is_empty() => {
                elems.iter().all(|e| self.param_supported(e))
            }
            _ => false,
        }
    }

    /// Whether `named` is a string, a CM primitive or one of the known enums,
    /// resources and records.
    fn named_supported(&self, named: &NamedType) -> bool {
        let name = named.name.as_str();
        self.decls.is(named.id, CompilerItem::String)
            || wado_primitive_name_to_cm(name).is_some()
            || self.enums.contains(name)
            || self.resources.contains(name)
            || self.structs.contains(name)
    }

    /// Whether a result type, its aliases already resolved, has a CM lifting.
    fn return_supported(&self, ty: &Type) -> bool {
        match ty {
            Type::Named(named) => ty.is_unit() || self.named_supported(named),
            Type::Generic(generic) => match self.decls.item_at(generic.id) {
                Some(CompilerItem::Stream | CompilerItem::Future) => {
                    !generic.args.iter().any(mentions_empty_tuple)
                }
                // `AsyncCall<T>` is the Wado-level wrapper for async CM imports;
                // the CM ABI return is the inner `T`. Support depends on `T`.
                Some(CompilerItem::AsyncCall) => self.return_supported(&generic.args[0]),
                // A `map` key's `keytype` restriction needs resolved types, so
                // the boundary representability check enforces it, not this
                // pass. A list/option element is any supported value type (e.g.
                // `list<list<u8>>`, `list<[field-name, field-value]>`).
                Some(
                    CompilerItem::Result
                    | CompilerItem::List
                    | CompilerItem::Option
                    | CompilerItem::TreeMap,
                ) => generic.args.iter().all(|arg| self.return_supported(arg)),
                _ => false,
            },
            Type::Tuple(elements) if !elements.is_empty() => {
                elements.iter().all(|el| self.return_supported(el))
            }
            _ => false,
        }
    }
}

/// Whether `ty` names the empty tuple anywhere. The shape predicates accept a
/// generic without judging its arguments, so `[]` would otherwise ride past.
fn mentions_empty_tuple(ty: &Type) -> bool {
    ty.any(&mut |ty| matches!(ty, Type::Tuple(elems) if elems.is_empty()))
}

/// Canonical ABI maximum flat results before a return must use an outptr.
pub const MAX_FLAT_RESULTS: usize = 1;

/// Canonical ABI maximum flat params a sync call passes as Wasm params. Past
/// it, the caller writes them to one buffer, laid out as a tuple of their
/// types, and passes its address (`flatten_functype`).
const MAX_FLAT_PARAMS: usize = 16;

/// [`MAX_FLAT_PARAMS`] for an async lowering, which takes a params buffer
/// sooner.
const MAX_FLAT_ASYNC_PARAMS: usize = 4;

/// [`CmInterfaceRegistry::params_in_buffer`] for params already flattened to
/// `flat_count` core values.
pub fn flat_params_in_buffer(flat_count: usize, is_async: bool) -> bool {
    flat_count
        > if is_async {
            MAX_FLAT_ASYNC_PARAMS
        } else {
            MAX_FLAT_PARAMS
        }
}

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
    let source = registry.source_interface(named)?;
    let cases = registry.get_variant_cases_by_source(&source, &named.name)?;
    let payloads = cases.iter().filter_map(|case| case.payload.as_ref());
    Some(layout_variant_with_registry(cases.len(), payloads, registry).size_align())
}

/// CM canonical ABI size and alignment. A named declaration resolves through
/// the registry; one it does not know is a 4-byte handle (an enum or resource).
pub fn cm_layout_with_registry(ty: &Type, registry: &CmInterfaceRegistry) -> (u32, u32) {
    const HANDLE: (u32, u32) = (4, 4);
    // `string` and `list<T>` are a `(ptr: i32, len: i32)`, and `map<K, V>`
    // despecializes to `list<tuple<K, V>>`.
    const PTR_LEN: (u32, u32) = (8, 4);
    let decls = registry.type_decls();
    match ty {
        Type::Named(named) if decls.is(named.id, CompilerItem::String) => PTR_LEN,
        Type::Named(named) => {
            if let Some(primitive) = primitive_size_align(&named.name) {
                return primitive;
            }
            let Some(source) = registry.source_interface(named) else {
                return registry
                    .type_decls()
                    .newtype_base(named)
                    .map_or(HANDLE, |base| cm_layout_with_registry(base, registry));
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
            HANDLE
        }
        Type::Generic(g) => match decls.item_at(g.id) {
            Some(CompilerItem::List | CompilerItem::TreeMap) => PTR_LEN,
            Some(CompilerItem::Option) => {
                layout_option_with_registry(&g.args[0], registry).size_align()
            }
            Some(CompilerItem::Result) => {
                layout_result_with_registry(&g.args[0], &g.args[1], registry).size_align()
            }
            // A stream or a future is a handle.
            _ => HANDLE,
        },
        Type::Tuple(elems) if elems.is_empty() => (0, 1),
        Type::Tuple(elems) => layout_tuple_with_registry(elems, registry).size_align(),
        Type::Reference(_) | Type::MutReference(_) => HANDLE,
        _ => (0, 1),
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
    use crate::analyze::Analyzer;
    use crate::ast::{AstId, GenericType, NamedType};
    use crate::compiler_host::{InMemoryCompilerHost, LogLevel};
    use crate::lexer::lex;
    use crate::logger::Logger;
    use crate::module_source::ModuleSourceInterner;
    use crate::parser;
    use crate::token::Span;
    use std::assert_matches;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    fn make_span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    /// One binding module, registered the way [`CmInterfaceRegistry::build_from_stdlib`]
    /// registers each of its own, its type sites bound to their declarations.
    fn registry_from(module_path: &'static str, source: &str) -> CmInterfaceRegistry {
        let lexed = lex(source);
        assert!(lexed.errors.is_empty(), "lexer error: {:?}", lexed.errors);
        let module = parser::Parser::new(lexed.tokens)
            .parse_strict()
            .expect("parser error");
        let interner = Rc::new(RefCell::new(ModuleSourceInterner::new()));
        let entry = interner.borrow_mut().local("./binding.wado");
        let modules: IndexMap<ModuleSource, ast::Module> =
            [(entry.clone(), module)].into_iter().collect();
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Off);
        let mut analyzer = Analyzer::new(&logger).with_interner(interner);
        let _ = analyzer.analyze_loaded_modules(&modules, &entry, IndexSet::default());
        let symbols = analyzer.into_symbols();
        let defs = Arc::new(DefTable::build(&modules, &symbols));
        let resolutions = Resolutions::build(&modules, &symbols, defs);
        let mut registry = CmInterfaceRegistry::new();
        registry
            .type_decls
            .bind(modules.values(), modules.values(), &resolutions);

        let module = &modules[&entry];
        let mut defs_by_module: IndexMap<&'static str, IndexMap<String, String>> =
            IndexMap::default();
        defs_by_module.insert(module_path, collect_cm_definitions(module));
        let local_names = build_local_name_resolver(module_path, module, &defs_by_module);
        registry.extend_source_interfaces(collect_named_type_sources(module, &local_names));
        registry
            .register_module_decls(module, &CmDeclScope::Module)
            .unwrap();
        registry
    }

    /// A type site naming the declaration `interface` registers as `name`.
    fn registered_type(registry: &CmInterfaceRegistry, interface: &str, name: &str) -> Type {
        let decl = registry
            .registrations
            .iter()
            .find(|(_, r)| r.interface == interface && r.name == name)
            .map(|(decl, _)| *decl)
            .unwrap_or_else(|| panic!("`{interface}` registers `{name}`"));
        registry
            .type_decls
            .decl_type(decl, name.to_string(), Vec::new(), make_span())
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

    /// An extern handle is a value, so a `&Handle` argument crosses as the handle,
    /// which the boundary sees as its `f64`.
    #[test]
    fn an_extern_handle_argument_is_peeled_outside_a_resource_method() {
        let registry = registry_from(
            "dom.wado",
            r#"
            #[cm("wado-lang:web/handle", linearity = "unrestricted")]
            pub resource Handle {
                #[cm("wado-lang:web/handle#sibling")]
                #[cm_params("self", "other")]
                fn sibling(&self, other: &Handle) -> Handle;
            }

            #[cm("wado-lang:web/global")]
            pub interface Dom {
                #[cm("wado-lang:web/global#adopt")]
                #[cm_params("node")]
                fn adopt(node: &Handle) -> Handle;
            }
            "#,
        );
        for (interface, method) in [("Handle", "sibling"), ("Dom", "adopt")] {
            for ty in param_types(&registry, interface, method) {
                assert_matches!(
                    &ty,
                    Type::Named(n) if n.name == "Handle",
                    "{interface}::{method} keeps a handle behind a reference"
                );
                assert_matches!(registry.resolve_type(&ty), Type::Named(n) if n.name == "f64");
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
        let status = registered_type(&registry, "wasi:demo/types@0.1.0", "Status");
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

        registry
            .register(
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
            )
            .unwrap();

        assert_eq!(
            bound_name(
                &registry,
                "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
            ),
            Some("Stdout::write_via_stream".to_string())
        );
    }

    /// The Wado name that binds the CM function `key`.
    fn bound_name(registry: &CmInterfaceRegistry, key: &str) -> Option<String> {
        registry.get_function(key).map(CmFunctionInfo::wado_name)
    }

    /// Whether `registry` binds an operation `interface::operation`.
    fn binds(registry: &CmInterfaceRegistry, interface: &str, operation: &str) -> bool {
        registry.interfaces().any(|i| {
            i.functions
                .iter()
                .any(|f| f.interface_name == interface && f.method_name == operation)
        })
    }

    #[test]
    fn test_same_named_interfaces_bind_their_own_functions() {
        let mut registry = CmInterfaceRegistry::new();
        for cm in ["example:demo/client#send", "wasi:http/client@0.3.0#send"] {
            registry
                .register(
                    "Client",
                    "send",
                    &CmImport::parse(cm).unwrap(),
                    false,
                    vec![],
                    None,
                )
                .unwrap();
        }
        let [demo, http] = ["example:demo/client#send", "wasi:http/client@0.3.0#send"]
            .map(|key| registry.get_function(key).unwrap());
        assert_eq!(demo.wado_name(), http.wado_name());
        assert_ne!(demo.binding_name(), http.binding_name());
    }

    #[test]
    fn test_no_collision_with_different_interfaces() {
        let mut registry = CmInterfaceRegistry::new();

        // Register stdout
        let stdout_wasi =
            CmImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();
        registry
            .register(
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
            )
            .unwrap();

        // Register stderr - different interface, same function name
        let stderr_wasi =
            CmImport::parse("wasi:cli/stderr@0.3.0-rc-2025-09-16#write-via-stream").unwrap();
        registry
            .register(
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
            )
            .unwrap();

        assert_eq!(
            bound_name(
                &registry,
                "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
            ),
            Some("Stdout::write_via_stream".to_string())
        );
        assert_eq!(
            bound_name(
                &registry,
                "wasi:cli/stderr@0.3.0-rc-2025-09-16#write-via-stream"
            ),
            Some("Stderr::write_via_stream".to_string())
        );
    }

    #[test]
    fn test_interfaces_iteration() {
        let mut registry = CmInterfaceRegistry::new();

        let wasi = CmImport::parse("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").unwrap();

        registry
            .register(
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
            )
            .unwrap();

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
    fn test_func_info_names_mint_from_its_cm_identifier() {
        let func_info = CmFunctionInfo {
            interface_name: "Stdout".to_string(),
            method_name: "write_via_stream".to_string(),
            wasi_func_name: "write-via-stream".to_string(),
            interface_path: "wasi:cli/stdout@0.3.0-rc-2025-09-16".to_string(),
            package: "cli".to_string(),
            is_async: true,
            params: vec![],
            list_args: vec![],
            callbacks: vec![],
            return_type: None,
            world: None,
        };
        assert_eq!(
            func_info.local_alias_name(),
            "$wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
        );
        assert_eq!(
            func_info.binding_name(),
            "$cm_binding__wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
        );
        assert_eq!(
            func_info.lift_name(),
            "$cm_lift__wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
        );
    }

    #[test]
    fn test_array_and_option_type_support() {
        let decls = stdlib_bindings().type_decls.clone();
        let item = |item, args| decls.item_type(item, args, make_span());
        let string = || item(CompilerItem::String, vec![]);
        let array_string = item(CompilerItem::List, vec![string()]);
        let array_tuple = item(
            CompilerItem::List,
            vec![Type::Tuple(vec![string(), string()])],
        );
        let option_string = item(CompilerItem::Option, vec![string()]);
        let known = KnownTypes {
            decls: &decls,
            enums: IndexSet::default(),
            resources: IndexSet::default(),
            structs: IndexSet::default(),
        };
        for ty in [array_string, array_tuple, option_string] {
            assert!(
                known.return_supported(&ty),
                "{} should be supported",
                render_ast_type(&ty)
            );
        }
    }

    #[test]
    fn test_random_functions_registered() {
        let (registry, _) = CmInterfaceRegistry::build_from_stdlib();

        for (interface, operation) in [
            ("Random", "get_random_u64"),
            ("Random", "get_random_bytes"),
            ("Insecure", "get_insecure_random_u64"),
            ("Insecure", "get_insecure_random_bytes"),
        ] {
            assert!(
                binds(&registry, interface, operation),
                "{interface}::{operation} should be registered"
            );
        }

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
        assert!(
            binds(&registry, "TcpSocket", "create"),
            "TcpSocket::create should be registered"
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

    /// One list drives every walk over the kinds, so a declaration of any kind
    /// answers all of them. A kind one walk misses resolves to nothing.
    // Spelled out rather than read from `ALL`, so a kind dropped from `ALL`
    // fails here; `register_of_kind` matches exhaustively, so a new kind will
    // not compile until it is listed.
    #[test]
    fn every_type_kind_answers_every_kind_walk() {
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

            assert!(registry.declares_wado_name(fq, "Probe"), "{kind:?}");
            assert_eq!(registry.existing_cm_decl(fq), Some("Probe"), "{kind:?}");
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
        let Type::Named(site) = &filesize else {
            unreachable!()
        };
        registry.set_source_interface(site.id, "wasi:filesystem/types@0.3.0".into());
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
    fn same_named_types_of_two_interfaces_are_two_cm_types() {
        let mut registry = CmInterfaceRegistry::new();
        for (fq, case) in [("wasi:a/types@1.0.0", "x"), ("wasi:b/types@1.0.0", "y")] {
            registry.enums.insert(
                (fq.into(), "ErrorCode".into()),
                ("error-code".into(), vec![case.into()]),
            );
        }
        let mut type_gen = CmTypeGen::new();
        let mut sink = CmNameSink::default();
        let mut lower = |fq: &str| {
            let named = NamedType::new(AstId::fresh(), "ErrorCode".into(), make_span());
            registry.set_source_interface(named.id, fq.into());
            type_gen.ast_type_to_cm(
                &mut sink,
                &Type::Named(named),
                &registry,
                &IndexMap::default(),
            )
        };
        let a = lower("wasi:a/types@1.0.0");
        let b = lower("wasi:b/types@1.0.0");
        assert_ne!(a, b);
    }

    /// A resource answers its CM name through its declaration's registration;
    /// a declaration registered as anything else answers none.
    #[test]
    fn a_resource_answers_its_cm_name_by_declaration() {
        let registry = registry_from(
            "wasi:demo",
            r#"
            #[cm("wasi:demo/types@0.1.0#request")]
            pub resource Request {}

            #[cm("wasi:demo/types@0.1.0#status")]
            pub enum Status {
                #[cm("ok")]
                Ok,
            }
            "#,
        );
        let decl_of = |name| {
            let Type::Named(site) = registered_type(&registry, "wasi:demo/types@0.1.0", name)
            else {
                unreachable!()
            };
            registry.type_decls.decl_at(site.id).unwrap()
        };
        assert_eq!(
            registry.resource_cm_name_of(decl_of("Request")),
            Some("request")
        );
        assert_eq!(registry.resource_cm_name_of(decl_of("Status")), None);
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
        let mut registry = (*CmInterfaceRegistry::build_from_stdlib().0).clone();
        let string = registry
            .type_decls()
            .item_type(CompilerItem::String, vec![], make_span());
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
            ("wrap".into(), vec![("s".into(), string)], vec![]),
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
