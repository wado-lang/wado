//! Type-level helpers for CM binding synthesis.
//!
//! Houses utilities that translate between AST `Type`, TIR `TypeId`, and
//! Canonical ABI flat / size / alignment information. Shared by the lift,
//! lower, and adapter synthesis paths.

use std::cell::RefCell;

use crate::ast::{AstId, NamedType, Type};
use crate::cm_abi;
use crate::compiler_item::CompilerItem;
use crate::component_model::{CmInterfaceRegistry, cm_layout_with_registry};
use crate::hashmap::IndexMap;
use crate::module_source::{CmNamespace, ModuleSource, ModuleSourceInterner};
use crate::primitive::PrimitiveType;
use crate::tir::{
    ResolvedType, TemplateId, TirBinaryOp, TirExpr, TirExprKind, TirModule, TirParam, TirStruct,
    TirVariantDecl, TypeId,
};
use crate::type_table::TypeTable;

use crate::component_model::map_key_rejection;
use crate::component_model::{future_payload_rejection, stream_payload_rejection};
use crate::defs::{DefId, DefKind, DefTable};
use crate::name::{FqTraitName, FqTypeName, UNIT_TYPE_NAME};
use crate::synthesis::common::{
    binary, builtin_call, cast, f64_const, i32_const, i64_const, synth_span,
};
use crate::tir::StructDef;

/// Snapshot of the stdlib cases and declarations CM binding builds with,
/// resolved once through the `CompilerItem` registry. Which type a signature
/// names is the registry's [`CmTypeDecls`](crate::cm_type_decls::CmTypeDecls)
/// to answer.
#[derive(Clone, Debug)]
pub struct CmStdlibNames {
    /// `Option::Some` case name + zero-based index.
    pub some_name: String,
    pub some_index: u32,
    /// `Option::None` case name + zero-based index.
    pub none_name: String,
    pub none_index: u32,
    /// `Result::Ok` case name + zero-based index.
    pub ok_name: String,
    pub ok_index: u32,
    /// `Result::Err` case name + zero-based index.
    pub err_name: String,
    pub err_index: u32,
    /// The `IndexValue` trait the list adapters call through, as the
    /// declaration the registry records — never a spelling a user trait could
    /// share.
    pub index_value: FqTraitName,
    /// `List`'s head, likewise the declaration the registry records.
    pub array_fq: FqTypeName,
    /// `List::len`, the declaration a list adapter's length call instantiates.
    pub list_len: TemplateId,
    /// `List`'s `index_value`, likewise for the element read.
    pub list_index_value: TemplateId,
}

impl CmStdlibNames {
    /// Look up every name through the [`CompilerItems`] registry.
    /// Cheap (a handful of registry hits + clones). Each synthesis entry
    /// point builds the snapshot once per binding — the lower side threads
    /// it through [`LowerContext`] — mirroring the `from_compiler_items`
    /// constructor shape used by the other synthesis passes
    /// (`SerdeStdlibNames`, `FormatStdlibNames`, `TraitsStdlibNames`).
    pub fn from_type_table(type_table: &TypeTable) -> Self {
        let items = type_table.compiler_items();
        let (_, _, some_name, some_index) = items.require_variant_case(CompilerItem::OptionSome);
        let (_, _, none_name, none_index) = items.require_variant_case(CompilerItem::OptionNone);
        let (_, _, ok_name, ok_index) = items.require_variant_case(CompilerItem::ResultOk);
        let (_, _, err_name, err_index) = items.require_variant_case(CompilerItem::ResultErr);
        Self {
            some_name: some_name.to_string(),
            some_index,
            none_name: none_name.to_string(),
            none_index,
            ok_name: ok_name.to_string(),
            ok_index,
            err_name: err_name.to_string(),
            err_index,
            index_value: items.trait_fq(CompilerItem::IndexValue),
            array_fq: type_table.compiler_struct_fq_name(CompilerItem::List),
            list_len: items.require_template(CompilerItem::ListLen),
            list_index_value: items.require_template(CompilerItem::ListIndexValue),
        }
    }
}

/// Context for lifting CM values to GC types, providing access to
/// the WASI registry (for variant/enum case info) and type table (for `TypeIds`).
///
/// Shared between the memory-based lift in `lift.rs` and the
/// flat-parameter lift in `export_adapter.rs`. Both paths recurse
/// through helpers that take a `RefCell<TypeTable>` borrow, so this
/// struct is `Copy` to make it cheap to pass by value across the
/// recursion sites without propagating a borrow.
#[derive(Clone, Copy)]
pub struct LiftContext<'a> {
    pub cm_interface_registry: &'a CmInterfaceRegistry,
    pub type_table: &'a RefCell<TypeTable>,
    /// `ModuleSource` interner shared with the package, used to canonicalise a
    /// synthesised type's module identity so it matches the elaborator's
    /// registered `StructName`s pointer-equal. The lift chain may `borrow_mut`
    /// it, so no caller may hold a `RefMut` across a call into `synthesize_lift`;
    /// the lift path itself borrows only transiently.
    pub interner: &'a RefCell<ModuleSourceInterner>,
}

impl LiftContext<'_> {
    /// The declaration the CM named type `named` refers to.
    pub(super) fn cm_decl(&self, named: &NamedType) -> DefId {
        cm_decl_of(
            named,
            self.cm_interface_registry,
            self.type_table.borrow().defs(),
        )
    }

    /// [`cm_type_to_type_id`], except that a record or variant of an interface a
    /// module declares is interned here rather than required to be interned
    /// already: a lib-local one may not be yet.
    pub(super) fn cm_type_id(&self, ty: &Type, tt: &mut TypeTable) -> TypeId {
        let registry = self.cm_interface_registry;
        match ty {
            Type::Named(n)
                if registry
                    .source_interface(n)
                    .is_some_and(|src| registry.cm_interface_module_source_of(&src).is_some()) =>
            {
                let def = cm_decl_of(n, registry, tt.defs());
                match tt.defs().kind(def) {
                    DefKind::Struct => tt.make_struct(StructDef::Decl(def)),
                    DefKind::Variant => tt.make_variant(def),
                    _ => cm_type_to_type_id(ty, tt, registry),
                }
            }
            Type::Tuple(elems) if !elems.is_empty() => {
                let ids: Vec<TypeId> = elems.iter().map(|e| self.cm_held_type_id(e, tt)).collect();
                tt.make_tuple(ids)
            }
            Type::Generic(g) => match self.cm_interface_registry.type_decls().item_of(ty) {
                Some(CompilerItem::List) => {
                    let elem = self.cm_held_type_id(&g.args[0], tt);
                    tt.make_list(elem)
                }
                Some(CompilerItem::Option) => {
                    let inner = self.cm_held_type_id(&g.args[0], tt);
                    tt.make_option(inner)
                }
                Some(CompilerItem::Result) => {
                    let ok = self.cm_held_type_id(&g.args[0], tt);
                    let err = self.cm_held_type_id(&g.args[1], tt);
                    tt.make_result(ok, err)
                }
                _ => cm_type_to_type_id(ty, tt, self.cm_interface_registry),
            },
            _ => cm_type_to_type_id(ty, tt, self.cm_interface_registry),
        }
    }

    /// [`Self::cm_type_id`] for a type a container holds.
    pub(super) fn cm_held_type_id(&self, ty: &Type, tt: &mut TypeTable) -> TypeId {
        held_type(ty, tt, |t, tt| self.cm_type_id(t, tt))
    }
}

/// Context for lowering GC values to CM representations, providing access
/// to the WASI registry (for record/variant layout), the type table (for
/// `TypeId`s), and the stdlib-name snapshot.
///
/// Mirror of [`LiftContext`] for the lower-side synthesis paths in
/// `lower.rs`. Not `Copy`: it owns the [`CmStdlibNames`] snapshot, so it is
/// passed by reference through the recursion sites.
pub struct LowerContext<'a> {
    pub cm_interface_registry: &'a CmInterfaceRegistry,
    pub type_table: &'a RefCell<TypeTable>,
    /// Stdlib-name snapshot; built once per binding instead of per call.
    pub names: CmStdlibNames,
}

/// The declaration the CM named type `named` refers to. Every type a CM
/// signature names is bound to one.
fn cm_decl_of(named: &NamedType, registry: &CmInterfaceRegistry, defs: &DefTable) -> DefId {
    registry
        .declared_def(named, defs)
        .unwrap_or_else(|| panic!("CM type `{}` names no declaration", named.name))
}

/// Convert a CM signature `Type` to a `TypeId`. A named type is the `TypeId` its
/// declaration was interned under.
pub fn cm_type_to_type_id(
    ty: &Type,
    type_table: &mut TypeTable,
    registry: &CmInterfaceRegistry,
) -> TypeId {
    let item = registry.type_decls().item_of(ty);
    match ty {
        Type::Named(_) if item == Some(CompilerItem::String) => {
            type_table.make_compiler_struct(CompilerItem::String)
        }
        Type::Named(_) if item == Some(CompilerItem::ByteList) => type_table.make_byte_list(),
        Type::Named(named) => match named.name.as_str() {
            "i8" => TypeTable::I8,
            "i16" => TypeTable::I16,
            "i32" => TypeTable::I32,
            "i64" => TypeTable::I64,
            "u8" => TypeTable::U8,
            "u16" => TypeTable::U16,
            "u32" => TypeTable::U32,
            "u64" => TypeTable::U64,
            "f32" => TypeTable::F32,
            "f64" => TypeTable::F64,
            "bool" => TypeTable::BOOL,
            "char" => TypeTable::CHAR,
            UNIT_TYPE_NAME => TypeTable::UNIT,
            // Resource/enum/variant types - look up the already-resolved TypeId.
            _ => {
                let def = cm_decl_of(named, registry, type_table.defs());
                type_table
                    .find_named_type_by_def(def)
                    // Resources are bare i32 handles at the CM boundary and
                    // need no registered GC type. Anything else without a
                    // TypeId would miscompile (e.g. FieldAccess on an i32).
                    .unwrap_or_else(|| {
                        assert!(
                            type_table.defs().kind(def) == DefKind::Resource,
                            "CM type `{}` has no registered TypeId",
                            named.name
                        );
                        TypeTable::I32
                    })
            }
        },
        Type::Generic(g) => {
            let held = |i: usize, type_table: &mut TypeTable| {
                cm_held_type_to_type_id(&g.args[i], type_table, registry)
            };
            let bare = |i: usize, type_table: &mut TypeTable| {
                cm_type_to_type_id(&g.args[i], type_table, registry)
            };
            match item {
                Some(CompilerItem::List) => {
                    let elem = held(0, type_table);
                    type_table.make_list(elem)
                }
                Some(CompilerItem::TreeMap) => {
                    let key = bare(0, type_table);
                    let value = bare(1, type_table);
                    type_table.make_tree_map(key, value)
                }
                Some(CompilerItem::Option) => {
                    let inner = held(0, type_table);
                    type_table.make_option(inner)
                }
                Some(CompilerItem::Result) => {
                    let ok = held(0, type_table);
                    let err = held(1, type_table);
                    type_table.make_result(ok, err)
                }
                Some(CompilerItem::Stream) => {
                    let inner = bare(0, type_table);
                    type_table.make_stream(inner)
                }
                Some(CompilerItem::Future) => {
                    let inner = bare(0, type_table);
                    type_table.make_future(inner)
                }
                Some(CompilerItem::AsyncCall) => {
                    let inner = bare(0, type_table);
                    type_table.make_async_call(inner)
                }
                _ => panic!("unsupported generic type at CM boundary: {}", g.name),
            }
        }
        Type::Tuple(types) if types.is_empty() => TypeTable::UNIT,
        Type::Tuple(types) => {
            let resolved: Vec<TypeId> = types
                .iter()
                .map(|t| cm_held_type_to_type_id(t, type_table, registry))
                .collect();
            type_table.make_tuple(resolved)
        }
        Type::Reference(inner) | Type::MutReference(inner)
            if registry.extern_handle(ty).is_some() =>
        {
            cm_type_to_type_id(inner, type_table, registry)
        }
        // Borrowed resource handles are i32 at the CM boundary.
        Type::Reference(_) | Type::MutReference(_) => TypeTable::I32,
        other => panic!("unsupported type at CM boundary: {other:?}"),
    }
}

/// The same conversion for a type a container holds, where a `borrow<t>` is the
/// `&T` the call site passes rather than the `i32` handle a bare one lowers to.
pub fn cm_held_type_to_type_id(
    ty: &Type,
    type_table: &mut TypeTable,
    registry: &CmInterfaceRegistry,
) -> TypeId {
    held_type(ty, type_table, |t, tt| cm_type_to_type_id(t, tt, registry))
}

/// A container's element type, resolved by `element`. Only a bare `borrow<t>`
/// is the `i32` the boundary passes; held, it is the `&T` of the call site.
fn held_type(
    ty: &Type,
    type_table: &mut TypeTable,
    element: impl FnOnce(&Type, &mut TypeTable) -> TypeId,
) -> TypeId {
    match ty {
        Type::Reference(inner) => {
            let inner = element(inner, type_table);
            type_table.make_ref(inner)
        }
        Type::MutReference(inner) => {
            let inner = element(inner, type_table);
            type_table.make_mut_ref(inner)
        }
        _ => element(ty, type_table),
    }
}

/// The module a bundled CM interface FQ maps to, with its owning namespace —
/// `None` for a `core:` module, which carries none.
pub(crate) fn cm_interface_module(source_interface: &str) -> Option<(Option<CmNamespace>, String)> {
    if let Some((namespace, rest)) = CmNamespace::split_specifier(source_interface) {
        return Some((Some(namespace), interface_module_name(rest)));
    }
    source_interface
        .strip_prefix("core:")
        .map(|rest| (None, interface_module_name(rest)))
}

/// The `{package}/{interface}.wado` spelling of a version-stripped interface path.
fn interface_module_name(without_namespace: &str) -> String {
    let without_version = without_namespace
        .split('@')
        .next()
        .unwrap_or(without_namespace);
    match without_version.split_once('/') {
        Some((pkg, iface)) => format!("{pkg}/{}.wado", iface.replace('-', "_")),
        None => format!("{without_version}.wado"),
    }
}

/// Create an i32 addition expression.
pub(super) fn binary_add(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(TirBinaryOp::Add, left, right, TypeTable::I32)
}

pub(super) fn binary_ne(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(TirBinaryOp::NotEq, left, right, TypeTable::BOOL)
}

pub(crate) fn kebab_to_pascal(s: &str) -> String {
    use heck::ToUpperCamelCase;
    s.to_upper_camel_case()
}

pub(super) fn is_wasm_flat_type(type_id: TypeId) -> bool {
    matches!(
        type_id,
        TypeTable::I32 | TypeTable::I64 | TypeTable::F32 | TypeTable::F64
    )
}

/// Validate that `type_id` has a Component Model value representation, erroring
/// for one that has none in any world — an empty record, which the CM binary
/// format forbids, or a 128-bit / `v128` scalar — rather than emitting an
/// invalid component. Recurses through containers, and rejects a type revisiting
/// itself: WIT has no recursive types, and synthesis would inline one forever.
pub(super) fn check_cm_boundary_representable(
    type_id: TypeId,
    slot: Slot,
    boundary: Boundary,
    type_table: &TypeTable,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    visited: &mut Vec<TypeId>,
) -> Result<(), String> {
    let names = CmStdlibNames::from_type_table(type_table);
    check_cm_boundary_representable_inner(
        type_id,
        slot,
        boundary,
        type_table,
        tir_modules,
        &names,
        visited,
    )
}

/// Where a type sits: `()` fills only a slot the Component Model lets stay empty,
/// a function result, a `result` arm or a case payload.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    Value,
    Optional,
}

/// Which way a signature crosses: an import also carries a borrowed resource
/// handle.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Boundary {
    Export,
    Import,
}

fn check_cm_boundary_representable_inner(
    type_id: TypeId,
    slot: Slot,
    boundary: Boundary,
    type_table: &TypeTable,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    names: &CmStdlibNames,
    visited: &mut Vec<TypeId>,
) -> Result<(), String> {
    if visited.contains(&type_id) {
        // `visited` is the recursion path (pushed on entry, popped on exit),
        // so a revisit is a genuine cycle. WIT cannot express recursive
        // types, so the type has no CM representation.
        return Err(format!(
            "recursive type `{}` cannot cross the Component Model boundary \
             — WIT has no recursive types",
            type_table.type_name(type_id)
        ));
    }
    visited.push(type_id);

    // Container shapes resolve through the type-table accessors regardless of
    // their declaring module, keeping this free of source-prefix branching.
    let recurse = |tid, slot, visited: &mut Vec<TypeId>| {
        check_cm_boundary_representable_inner(
            tid,
            slot,
            boundary,
            type_table,
            tir_modules,
            names,
            visited,
        )
    };
    let result = (|visited: &mut Vec<TypeId>| {
        if let Some(inner) = type_table.as_option(type_id) {
            return recurse(inner, Slot::Value, visited);
        }
        if let Some(elem) = type_table.as_list(type_id) {
            return recurse(elem, Slot::Value, visited);
        }
        if let Some(elems) = type_table.as_tuple(type_id) {
            if elems.is_empty() {
                return Err(
                    "the empty tuple `[]` has no Component Model representation — a `tuple` \
                     carries at least one type, and `()` is the type that carries none"
                        .to_string(),
                );
            }
            for e in elems {
                recurse(e, Slot::Value, visited)?;
            }
            return Ok(());
        }
        // Exhaustive over `ResolvedType` on purpose — no wildcard. A wildcard
        // would silently classify an unhandled (or future) variant as
        // representable and let it fall through to the i32 lowering, which is
        // exactly the bug this check exists to prevent. New variants must be
        // classified explicitly.
        match type_table.get(type_id) {
            // No CM value representation in any world: `defvaltype` stops at
            // `f32`, and a half lowered as its `u16` would read as an integer.
            ResolvedType::Primitive(
                PrimitiveType::V128 | PrimitiveType::F16 | PrimitiveType::Bf16,
            ) => Err(format!(
                "`{}` has no Component Model value representation",
                type_table.type_name(type_id)
            )),
            ResolvedType::Unit if slot == Slot::Value => Err(
                "`()` has no Component Model representation here — it stands only where a \
                 type may be absent: a function result, a `Result` arm or a case payload"
                    .to_string(),
            ),
            ResolvedType::Enum { def }
            | ResolvedType::Flags { def }
            | ResolvedType::Variant { def }
                if declares_no_case(*def, tir_modules) =>
            {
                Err(format!(
                    "`{}` declares no case, and an empty enum, flags or variant has no \
                     Component Model representation — add at least one",
                    type_table.type_name(type_id)
                ))
            }
            // Scalars, plain discriminants, bitflags, and plain resource
            // handles lower to an i32 handle identically in every world.
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. } => Ok(()),
            // The handle is an i32, but its payload is lifted and lowered by
            // value, so it must be classifiable too — `()` is representable
            // yet has no payload type.
            ResolvedType::GenericInstance { def, type_args }
                if type_table.is_resource_def(*def) =>
            {
                let item = type_table.compiler_type_item(*def);
                let args = type_args.clone();
                for &a in &args {
                    recurse(a, Slot::Optional, visited)?;
                }
                if let Some(&payload) = args.first()
                    && let Some(reason) = match item {
                        Some(CompilerItem::Future | CompilerItem::FutureWritable) => {
                            future_payload_rejection(type_table, payload)
                        }
                        Some(CompilerItem::Stream | CompilerItem::StreamWritable) => {
                            stream_payload_rejection(type_table, payload)
                        }
                        _ => None,
                    }
                {
                    return Err(reason);
                }
                Ok(())
            }
            ResolvedType::Struct { def, type_args } => {
                if type_table.is_string(type_id) {
                    return Ok(());
                }
                let name = type_table.struct_head_name(*def);
                match struct_decl_of(*def, type_args, tir_modules) {
                    Some(decl) if decl.fields.is_empty() => Err(format!(
                        "record `{name}` has no fields; an empty record has no \
                         Component Model representation — add at least one field"
                    )),
                    Some(decl) => {
                        let field_tys: Vec<TypeId> =
                            decl.fields.iter().map(|f| f.type_id).collect();
                        for ft in field_tys {
                            recurse(ft, Slot::Value, visited)?;
                        }
                        Ok(())
                    }
                    // A struct with no TIR decl is a registry-only record whose
                    // fields are validated by the registry path; if such a type
                    // ever reached flat lowering without a decl, that path
                    // panics rather than silently mis-flattening it.
                    None => Ok(()),
                }
            }
            ResolvedType::Variant { def } => match variant_decl_of(*def, tir_modules) {
                Some(decl) => {
                    let payloads: Vec<TypeId> = decl.cases.iter().map(|c| c.payload).collect();
                    for p in payloads {
                        recurse(p, Slot::Optional, visited)?;
                    }
                    Ok(())
                }
                None => Ok(()),
            },
            ResolvedType::GenericInstance { def, type_args } => {
                // Option/List/Tuple were handled above by the `as_*` accessors;
                // `Result<T, E>` recurses into its arms. Any other generic
                // instance has no concrete CM lowering at this boundary (it
                // should have monomorphized to a named type), so reject it
                // rather than lowering it as an opaque i32.
                let item = type_table.compiler_type_item(*def);
                if item == Some(CompilerItem::Result) {
                    let args = type_args.clone();
                    for a in args {
                        recurse(a, Slot::Optional, visited)?;
                    }
                    Ok(())
                } else if let Some((key, value)) = type_table.as_tree_map(type_id) {
                    // `map<K, V>`: the key comes from the CM's `keytype`
                    // subset, the value from any representable valtype.
                    if let Some(reason) = map_key_rejection(type_table, key) {
                        return Err(reason);
                    }
                    recurse(value, Slot::Value, visited)
                } else {
                    Err(format!(
                        "generic type `{}` has no Component Model value representation",
                        type_table.type_name(type_id)
                    ))
                }
            }
            ResolvedType::Newtype { base_type, .. } => {
                let base = *base_type;
                recurse(base, slot, visited)
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner)
                if boundary == Boundary::Import =>
            {
                let inner = *inner;
                if type_table.is_resource_type(type_table.representation_head(inner)) {
                    recurse(inner, Slot::Value, visited)
                } else {
                    Err(format!(
                        "a reference crosses a Component Model import only as a borrowed \
                         resource handle, and `{}` refers to no resource",
                        type_table.type_name(type_id)
                    ))
                }
            }
            ResolvedType::Never
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
            | ResolvedType::Error => Err(format!(
                "type `{}` has no Component Model value representation",
                type_table.type_name(type_id)
            )),
        }
    })(visited);

    visited.pop();
    result
}

/// Map a core-value `TypeId` (`i32`/`i64`/`f32`/`f64`) to its `CmValType`.
/// Non-core `TypeIds` (which never appear as a flat slot) map to `I32`.
pub(super) fn cm_val_type_from_type_id(tid: TypeId) -> cm_abi::CmValType {
    if tid == TypeTable::I64 {
        cm_abi::CmValType::I64
    } else if tid == TypeTable::F32 {
        cm_abi::CmValType::F32
    } else if tid == TypeTable::F64 {
        cm_abi::CmValType::F64
    } else {
        cm_abi::CmValType::I32
    }
}

/// Byte size of a flat CM core value type.
fn cmval_size(v: cm_abi::CmValType) -> u32 {
    match v {
        cm_abi::CmValType::I32 | cm_abi::CmValType::F32 => 4,
        cm_abi::CmValType::I64 | cm_abi::CmValType::F64 => 8,
    }
}

/// Reinterpret a flat value to the same-size integer (`i32`/`i64`), returning
/// the new expression and its integer `CmValType`.
fn flat_to_int_bits(value: TirExpr, ty: cm_abi::CmValType) -> (TirExpr, cm_abi::CmValType) {
    match ty {
        cm_abi::CmValType::I32 | cm_abi::CmValType::I64 => (value, ty),
        cm_abi::CmValType::F32 => (
            builtin_call("i32_reinterpret_f32", vec![value], TypeTable::I32),
            cm_abi::CmValType::I32,
        ),
        cm_abi::CmValType::F64 => (
            builtin_call("i64_reinterpret_f64", vec![value], TypeTable::I64),
            cm_abi::CmValType::I64,
        ),
    }
}

/// Reinterpret a same-size integer flat value to the float class of `ty`
/// (no-op when `ty` is already integral).
fn flat_from_int_bits(value: TirExpr, ty: cm_abi::CmValType) -> TirExpr {
    match ty {
        cm_abi::CmValType::I32 | cm_abi::CmValType::I64 => value,
        cm_abi::CmValType::F32 => builtin_call("f32_reinterpret_i32", vec![value], TypeTable::F32),
        cm_abi::CmValType::F64 => builtin_call("f64_reinterpret_i64", vec![value], TypeTable::F64),
    }
}

/// Bit-preserving coercion of a flat CM value from its declared joined slot
/// type `have` to a case's natural type `want` — the Canonical ABI variant
/// flat-join, lift direction. The join always widens, so `have` is at least as
/// wide as `want`: a same-size/different-class pair reinterprets; a wider slot
/// is narrowed by taking its low bits (`i64`→`i32` wrap) before reinterpreting.
/// A *numeric* cast would corrupt `i32`↔`f32` / `i64`↔`f64` pairs.
pub(super) fn coerce_flat_lift(
    value: TirExpr,
    have: cm_abi::CmValType,
    want: cm_abi::CmValType,
) -> TirExpr {
    if have == want {
        return value;
    }
    let (as_int, _) = flat_to_int_bits(value, have);
    let sized = if cmval_size(have) > cmval_size(want) {
        cast(as_int, TypeTable::I32)
    } else {
        as_int
    };
    flat_from_int_bits(sized, want)
}

/// Lower direction: coerce a case's natural value `want` into its declared
/// joined slot type `have`. Inverse of [`coerce_flat_lift`]: reinterpret to the
/// integer class, zero-extend a narrower value into the wider slot, then
/// reinterpret to the slot's class.
pub(super) fn coerce_flat_lower(
    value: TirExpr,
    want: cm_abi::CmValType,
    have: cm_abi::CmValType,
) -> TirExpr {
    if have == want {
        return value;
    }
    let (as_int, _) = flat_to_int_bits(value, want);
    let sized = if cmval_size(have) > cmval_size(want) {
        cast(as_int, TypeTable::I64)
    } else {
        as_int
    };
    flat_from_int_bits(sized, have)
}

/// Compute the flat ABI parameter types for a CM function parameter.
///
/// Adapter mapping [`CmInterfaceRegistry::cm_flatten`] to `TypeId`s.
pub fn flatten_param_type(ty: &Type, cm_interface_registry: &CmInterfaceRegistry) -> Vec<TypeId> {
    let resolved = cm_interface_registry.resolve_type(ty);
    cm_interface_registry
        .cm_flatten(&resolved)
        .into_iter()
        .map(cm_val_type_to_type_id)
        .collect()
}

pub use crate::cm_abi::{OPTION_OR_RESULT_CASES, cm_discriminant_byte_size, cm_flags_byte_size};

/// Core-wasm load op for a CM discriminant of the given byte size.
/// Discriminants are unsigned, so 1/2-byte widths zero-extend.
pub(super) fn disc_load_op(byte_size: u32) -> &'static str {
    match byte_size {
        1 => "i32_load8_u",
        2 => "i32_load16_u",
        4 => "i32_load",
        other => panic!("invalid CM discriminant byte size: {other}"),
    }
}

/// Core-wasm store op for a CM discriminant of the given byte size.
pub(super) fn disc_store_op(byte_size: u32) -> &'static str {
    match byte_size {
        1 => "i32_store8",
        2 => "i32_store16",
        4 => "i32_store",
        other => panic!("invalid CM discriminant byte size: {other}"),
    }
}

/// The stores that write a string, list, or direct param's flat values into an
/// async call's params buffer, at offsets from its slot.
pub(super) fn cm_param_store_plan(
    ty: &Type,
    cm_interface_registry: &CmInterfaceRegistry,
) -> Vec<(u32, &'static str)> {
    let item = cm_interface_registry.type_decls().item_of(ty);
    match ty {
        Type::Named(_) if item == Some(CompilerItem::String) => {
            vec![(0, "i32_store"), (4, "i32_store")]
        }
        Type::Named(named) if named.name == "f32" => vec![(0, "f32_store")],
        Type::Named(named) if named.name == "f64" => vec![(0, "f64_store")],
        Type::Named(_) => vec![(0, scalar_store_op(ty, cm_interface_registry))],
        Type::Generic(_) if item == Some(CompilerItem::List) => {
            vec![(0, "i32_store"), (4, "i32_store")]
        }
        _ => vec![(0, "i32_store")],
    }
}

/// The CM size and alignment of `ty` as the i32 pair a `realloc` argument or a
/// buffer stride takes.
pub(super) fn cm_layout_i32(ty: &Type, registry: &CmInterfaceRegistry) -> (i32, i32) {
    let (size, align) = cm_layout_with_registry(ty, registry);
    (size as i32, align as i32)
}

/// The integer store for one flat value of `ty`, at the width its CM layout
/// gives it. An unrestricted handle stores its `f64` bits as the `u64` it is.
pub(super) fn scalar_store_op(
    ty: &Type,
    cm_interface_registry: &CmInterfaceRegistry,
) -> &'static str {
    if cm_interface_registry.extern_handle(ty).is_some() {
        return "i64_store";
    }
    match flatten_param_type(ty, cm_interface_registry)[..] {
        [TypeTable::F64] => "f64_store",
        [TypeTable::F32] => "f32_store",
        [TypeTable::I64] => "i64_store",
        [TypeTable::I32] => match cm_layout_with_registry(ty, cm_interface_registry).0 {
            1 => "i32_store8",
            2 => "i32_store16",
            4 => "i32_store",
            other => panic!("a one-`i32` CM type cannot be {other} bytes wide: {ty:?}"),
        },
        ref flat => panic!("`{ty:?}` flattens to {flat:?}, not one scalar"),
    }
}

/// The load that reads back a one-value CM type with no sized declaration, and
/// the type it yields. The inverse of [`scalar_store_op`].
pub(super) fn handle_load_op(
    ty: &Type,
    cm_interface_registry: &CmInterfaceRegistry,
) -> (&'static str, TypeId) {
    if cm_interface_registry.extern_handle(ty).is_some() {
        return ("i64_load", TypeTable::U64);
    }
    let [flat] = cm_interface_registry.cm_flatten(ty)[..] else {
        panic!("a handle is one flat value: {ty:?}");
    };
    (flat_load_op(flat), cm_val_type_to_type_id(flat))
}

/// The builtin loading one flat value of type `vt`.
pub(super) fn flat_load_op(vt: cm_abi::CmValType) -> &'static str {
    match vt {
        cm_abi::CmValType::I32 => "i32_load",
        cm_abi::CmValType::I64 => "i64_load",
        cm_abi::CmValType::F32 => "f32_load",
        cm_abi::CmValType::F64 => "f64_load",
    }
}

/// The builtin storing one flat value of type `vt`.
pub(super) fn flat_store_op(vt: cm_abi::CmValType) -> &'static str {
    match vt {
        cm_abi::CmValType::I32 => "i32_store",
        cm_abi::CmValType::I64 => "i64_store",
        cm_abi::CmValType::F32 => "f32_store",
        cm_abi::CmValType::F64 => "f64_store",
    }
}

/// Check whether a return type needs lifting from a flat i32 discriminant to a GC struct.
/// This is true for Result types where all payloads are empty (unit), so the raw call
/// returns just a discriminant on the stack without an outptr.
pub(super) fn needs_flat_result_lifting(ty: &Type, registry: &CmInterfaceRegistry) -> bool {
    registry.type_decls().names(ty, CompilerItem::Result)
}

/// Flatten a variant type: discriminant + union of all case payloads.
fn flatten_variant_type(
    variant_decl: &TirVariantDecl,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
    names: &CmStdlibNames,
) {
    out.push(cm_abi::CmValType::I32); // variant discriminant
    let mut union: Vec<cm_abi::CmValType> = Vec::new();
    for case in &variant_decl.cases {
        let mut case_flat = Vec::new();
        flat_types_from_type_id_inner(case.payload, &mut case_flat, tir_modules, type_table, names);
        union = cm_abi::join_flat_unions(&union, &case_flat);
    }
    out.extend(union);
}

/// Flatten a struct type: concatenation of all field flat types.
fn flatten_struct_type(
    struct_decl: &TirStruct,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
    names: &CmStdlibNames,
) {
    for field in &struct_decl.fields {
        flat_types_from_type_id_inner(field.type_id, out, tir_modules, type_table, names);
    }
}

/// Compute flat CM ABI types from a `TypeId`, resolving through the type table.
pub(super) fn flat_types_from_type_id(
    type_id: TypeId,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) -> Vec<cm_abi::CmValType> {
    let mut out = Vec::new();
    flat_types_from_type_id_into(type_id, &mut out, tir_modules, type_table);
    out
}

/// Append flat CM ABI types from a `TypeId` to `out`.
///
/// Thin wrapper that builds the [`CmStdlibNames`] snapshot once and delegates
/// to the recursive inner function, so recursion does not rebuild it per level.
pub(super) fn flat_types_from_type_id_into(
    type_id: TypeId,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) {
    let names = CmStdlibNames::from_type_table(type_table);
    flat_types_from_type_id_inner(type_id, out, tir_modules, type_table, &names);
}

fn flat_types_from_type_id_inner(
    type_id: TypeId,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
    names: &CmStdlibNames,
) {
    match type_table.get(type_id) {
        ResolvedType::Primitive(p) => match p {
            PrimitiveType::I8
            | PrimitiveType::U8
            | PrimitiveType::I16
            | PrimitiveType::U16
            | PrimitiveType::I32
            | PrimitiveType::U32
            | PrimitiveType::Bool
            | PrimitiveType::Char => out.push(cm_abi::CmValType::I32),
            PrimitiveType::I64 | PrimitiveType::U64 => out.push(cm_abi::CmValType::I64),
            PrimitiveType::F32 => out.push(cm_abi::CmValType::F32),
            PrimitiveType::F64 => out.push(cm_abi::CmValType::F64),
            PrimitiveType::V128 | PrimitiveType::F16 | PrimitiveType::Bf16 => {
                panic!("{} cannot appear at CM boundary", p.as_str())
            }
        },
        ResolvedType::Unit => {} // no flat values
        ResolvedType::Struct { def, type_args } => {
            if type_table.is_string(type_id) {
                out.push(cm_abi::CmValType::I32); // ptr
                out.push(cm_abi::CmValType::I32); // len
            } else if let Some(struct_decl) = struct_decl_of(*def, type_args, tir_modules) {
                flatten_struct_type(&struct_decl, out, tir_modules, type_table, names);
            } else {
                // A record with no TIR declaration has no known field layout;
                // flattening it as one i32 would emit a wrong-arity lowering for
                // a multi-field record. Fail loudly rather than corrupt the
                // component (the memory lowerer panics on the same condition).
                panic!(
                    "struct `{}` has no TIR declaration; cannot compute its flat CM types",
                    type_table.struct_head_name(*def)
                );
            }
        }
        _ if type_table.is_resource_type(type_id) => {
            out.push(if type_table.is_unrestricted_handle(type_id) {
                cm_abi::CmValType::F64
            } else {
                cm_abi::CmValType::I32
            });
        }
        ResolvedType::Enum { .. } => out.push(cm_abi::CmValType::I32),
        ResolvedType::Variant { def } => {
            if let Some(variant_decl) = variant_decl_of(*def, tir_modules) {
                flatten_variant_type(&variant_decl, out, tir_modules, type_table, names);
            } else {
                out.push(cm_abi::CmValType::I32);
            }
        }
        ResolvedType::GenericInstance { def, type_args } => {
            if type_table.is_tuple_def(*def) {
                for &elem in type_args {
                    flat_types_from_type_id_inner(elem, out, tir_modules, type_table, names);
                }
                return;
            }
            match (type_table.compiler_type_item(*def), type_args.as_slice()) {
                (Some(CompilerItem::Option), [inner]) => {
                    out.push(cm_abi::CmValType::I32); // discriminant
                    flat_types_from_type_id_inner(*inner, out, tir_modules, type_table, names);
                }
                (Some(CompilerItem::Result), [ok, err]) => {
                    out.push(cm_abi::CmValType::I32); // discriminant
                    let mut ok_flat = Vec::new();
                    let mut err_flat = Vec::new();
                    flat_types_from_type_id_inner(
                        *ok,
                        &mut ok_flat,
                        tir_modules,
                        type_table,
                        names,
                    );
                    flat_types_from_type_id_inner(
                        *err,
                        &mut err_flat,
                        tir_modules,
                        type_table,
                        names,
                    );
                    out.extend(cm_abi::join_flat_unions(&ok_flat, &err_flat));
                }
                // A `map<K, V>` despecializes to `list<tuple<K, V>>`, so it
                // carries that type's pair.
                (Some(CompilerItem::List | CompilerItem::TreeMap), _) => {
                    out.push(cm_abi::CmValType::I32); // ptr
                    out.push(cm_abi::CmValType::I32); // len
                }
                _ => out.push(cm_abi::CmValType::I32),
            }
        }
        ResolvedType::Newtype { base_type, .. }
        | ResolvedType::Ref(base_type)
        | ResolvedType::MutRef(base_type) => {
            flat_types_from_type_id_inner(*base_type, out, tir_modules, type_table, names);
        }
        ResolvedType::Flags { .. } => {
            // Flags are u32 at the CM ABI level
            out.push(cm_abi::CmValType::I32);
        }
        _ => {} // Never, Error, Unknown, etc.
    }
}

/// The variant declaration `def` names. A resolved type carries its
/// declaration, so the search is keyed by that: a same-named variant in
/// another module answers for nothing (WEP 2026-08-12).
pub(super) fn variant_decl_of(
    def: DefId,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
) -> Option<TirVariantDecl> {
    tir_modules
        .values()
        .flat_map(|module| &module.variants)
        .find(|variant| variant.def == def)
        .cloned()
}

/// Whether the enum, flags or variant `def` declares no case at all.
fn declares_no_case(def: DefId, tir_modules: &IndexMap<ModuleSource, TirModule>) -> bool {
    tir_modules.values().any(|module| {
        module
            .enums
            .iter()
            .any(|e| e.def == def && e.cases.is_empty())
            || module
                .flags
                .iter()
                .any(|f| f.def == def && f.members.is_empty())
            || module
                .variants
                .iter()
                .any(|v| v.def == def && v.cases.is_empty())
    })
}

/// The struct declaration `def` names, at `type_args` where the module holds
/// that instantiation and at the declaration otherwise. Keyed by identity, for
/// the reason [`variant_decl_of`] is.
pub(super) fn struct_decl_of(
    def: StructDef,
    type_args: &[TypeId],
    tir_modules: &IndexMap<ModuleSource, TirModule>,
) -> Option<TirStruct> {
    let mut declaration = None;
    for s in tir_modules.values().flat_map(|module| &module.structs) {
        if s.def != def {
            continue;
        }
        if s.type_args == type_args {
            return Some(s.clone());
        }
        if s.type_args.is_empty() {
            declaration.get_or_insert(s);
        }
    }
    declaration.cloned()
}

/// Create a `VariantTag` TIR expression (extracts i32 discriminant).
pub(super) fn variant_tag(expr: TirExpr) -> TirExpr {
    let _ = expr.type_id;
    TirExpr::new(
        TirExprKind::VariantTag {
            expr: Box::new(expr),
        },
        TypeTable::I32,
        synth_span(),
    )
}

/// Create a `VariantTest` TIR expression (tests if variant is a specific case).
pub(super) fn variant_test(expr: TirExpr, case_index: u32, case_name: &str) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantTest {
            expr: Box::new(expr),
            case_index,
            case_name: case_name.to_string(),
        },
        TypeTable::BOOL,
        synth_span(),
    )
}

/// Create a `VariantPayload` TIR expression (extracts payload from a variant case).
pub(super) fn variant_payload(expr: TirExpr, case_index: u32, payload_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantPayload {
            expr: Box::new(expr),
            case_index,
            payload_type,
        },
        payload_type,
        synth_span(),
    )
}

/// Create a `FieldAccess` TIR expression (accesses a struct field).
pub(super) fn field_access(
    expr: TirExpr,
    field_name: &str,
    field_index: u32,
    field_type: TypeId,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::FieldAccess {
            expr: Box::new(expr),
            field_name: field_name.to_string(),
            field_index,
        },
        field_type,
        synth_span(),
    )
}

pub(super) fn cm_val_type_to_type_id(vt: cm_abi::CmValType) -> TypeId {
    match vt {
        cm_abi::CmValType::I32 => TypeTable::I32,
        cm_abi::CmValType::I64 => TypeTable::I64,
        cm_abi::CmValType::F32 => TypeTable::F32,
        cm_abi::CmValType::F64 => TypeTable::F64,
    }
}

/// Create a zero constant for a given CM value type.
pub(super) fn cm_zero(vt: cm_abi::CmValType) -> TirExpr {
    match vt {
        cm_abi::CmValType::I32 => i32_const(0),
        cm_abi::CmValType::I64 => i64_const(0),
        cm_abi::CmValType::F32 => TirExpr::new(
            TirExprKind::FloatLiteral {
                value: 0.0,
                repr: "0.0".to_string(),
            },
            TypeTable::F32,
            synth_span(),
        ),
        cm_abi::CmValType::F64 => f64_const(0.0),
    }
}

/// The compiler items a CM signature names, which the registry tells apart by
/// declaration.
const CM_ITEMS: [CompilerItem; 10] = [
    CompilerItem::String,
    CompilerItem::ByteList,
    CompilerItem::List,
    CompilerItem::ListView,
    CompilerItem::TreeMap,
    CompilerItem::Option,
    CompilerItem::Result,
    CompilerItem::Stream,
    CompilerItem::Future,
    CompilerItem::AsyncCall,
];

/// The compiler item among [`CM_ITEMS`] that declares `type_id`.
pub(super) fn cm_item_of_type(type_table: &TypeTable, type_id: TypeId) -> Option<CompilerItem> {
    let def = type_table.nominal_def(type_id)?;
    CM_ITEMS
        .into_iter()
        .find(|&item| type_table.is_compiler_item(def, item))
}

/// Reconstruct a minimal AST `Type` from a TIR `TypeId`, for callers that need
/// to re-enter the AST-shaped match arms. Only the top-level name and immediate
/// type args are filled in; deeper structure is looked up lazily. A compiler
/// item names its declaration, and another named type gets its
/// `source_interface` where the registry knows it, since the lift / lower
/// helpers key by `(source_interface, name)`.
pub(super) fn type_id_to_ast_type(
    type_id: TypeId,
    type_table: &TypeTable,
    cm_interface_registry: &CmInterfaceRegistry,
) -> Type {
    let span = synth_span();
    let resolved = type_table.get(type_id);
    let to_ast = |tid: &TypeId| type_id_to_ast_type(*tid, type_table, cm_interface_registry);
    if let Some(item) = cm_item_of_type(type_table, type_id) {
        let args = match resolved {
            ResolvedType::GenericInstance { type_args, .. } => {
                type_args.iter().map(to_ast).collect()
            }
            _ => Vec::new(),
        };
        return cm_interface_registry
            .type_decls()
            .item_type(item, args, span);
    }
    // The type names its declaration, so the registry answers its interface
    // from the declaration and no by-name search can offer another module's
    // `ErrorCode` and lift a record as its enum. It is spelled as the registry
    // holds it: a library publishes an adopted `FileType` as its `Kind`.
    let declared = |def: DefId, args: Vec<Type>| {
        let decl = type_table.defs().ast_id(def);
        let name = cm_interface_registry
            .registered_name_of(decl)
            .map(str::to_string)
            .unwrap_or_else(|| {
                type_table
                    .nominal_head(type_id)
                    .expect("a nominal type names a declaration")
                    .0
            });
        cm_interface_registry
            .type_decls()
            .decl_type(decl, name, args, span)
    };
    match resolved {
        ResolvedType::Primitive(p) => {
            Type::Named(NamedType::new(AstId::fresh(), p.as_str().to_string(), span))
        }
        ResolvedType::Unit => Type::unit(AstId::fresh(), span),
        // `Flags` joins them: its own CM type, 1 byte at <=8 labels, not a
        // four-byte `i32`.
        ResolvedType::Struct { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::Newtype { .. } => declared(
            type_table
                .nominal_def(type_id)
                .expect("a type crossing the boundary is declared"),
            Vec::new(),
        ),
        // The tuple family is a `GenericInstance`, but its CM surface is a
        // structural tuple — emit `Type::Tuple` so lift/lower dispatch on the
        // tuple arm rather than the generic catch-all.
        ResolvedType::GenericInstance { def, type_args } if type_table.is_tuple_def(*def) => {
            Type::Tuple(type_args.iter().map(to_ast).collect())
        }
        ResolvedType::GenericInstance { def, type_args } => {
            declared(*def, type_args.iter().map(to_ast).collect())
        }
        ResolvedType::Ref(inner) => Type::Reference(Box::new(to_ast(inner))),
        ResolvedType::MutRef(inner) => Type::MutReference(Box::new(to_ast(inner))),
        ResolvedType::Never
        | ResolvedType::Function { .. }
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::TypeParam { .. }
        | ResolvedType::AssocParam { .. }
        | ResolvedType::InferVar(_)
        | ResolvedType::TypePack { .. }
        | ResolvedType::PackElement { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::Unknown
        | ResolvedType::Error => {
            panic!("type has no Component Model surface: {resolved:?}")
        }
    }
}

/// Whether a parameter needs CM flat-ABI lifting at the export boundary — that
/// is, whether its flat representation is anything but a single-slot passthrough
/// of the same Wasm value type. Enum, flags and every primitive but `bool` travel
/// as one scalar at both layers and need none; a resource (a boxed `i32` or an
/// `f64`-carried handle), `bool`, `Unit`, and everything else must be
/// reconstructed Wado-side.
pub(super) fn param_needs_lifting(type_id: TypeId, tt: &TypeTable) -> bool {
    match tt.get(type_id) {
        ResolvedType::Primitive(prim) => matches!(prim, PrimitiveType::Bool),
        ResolvedType::Unit => true,
        // One-scalar handle-shaped types flow through.
        ResolvedType::Enum { .. } | ResolvedType::Flags { .. } => false,
        // `ResolvedType::Newtype` unwraps at the CM boundary, so recurse on
        // the base type rather than treating the newtype itself as
        // opaque.
        ResolvedType::Newtype { base_type, .. } => param_needs_lifting(*base_type, tt),
        // Everything else (Struct, Variant, tuples via GenericInstance,
        // `List<T>`, `Option<T>`, `Result<T, E>`, references, etc.)
        // either widens or splits at the flat ABI.
        _ => true,
    }
}

/// Check if any parameter of the user's exported function needs lifting.
pub(super) fn export_needs_param_lifting(
    user_params: &[TirParam],
    type_table: &RefCell<TypeTable>,
) -> bool {
    let tt = type_table.borrow();
    user_params
        .iter()
        .any(|p| param_needs_lifting(p.type_id, &tt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cm_interface_module_carries_the_namespace_that_owns_it() {
        assert_eq!(
            cm_interface_module("wasi:sockets/ip-name-lookup@0.3.0"),
            Some((
                Some(CmNamespace::Wasi),
                "sockets/ip_name_lookup.wado".into()
            ))
        );
        // A `core:` module carries no `CmNamespace`, and must not pair with one.
        assert_eq!(
            cm_interface_module("core:kiln/types@0.1.0"),
            Some((None, "kiln/types.wado".into()))
        );
        assert_eq!(cm_interface_module("my:pkg/iface"), None);
    }

    /// The bridge back to the AST spells unit the way the parser does, since
    /// [`Type::is_unit`] is what every AST-level predicate asks.
    #[test]
    fn the_unit_type_id_spells_the_unit_type() {
        let (registry, _) = CmInterfaceRegistry::build_from_stdlib();
        let type_table = TypeTable::new();
        let unit = type_id_to_ast_type(TypeTable::UNIT, &type_table, &registry);
        assert!(unit.is_unit(), "unit spelled as {unit:?}");
    }
}
