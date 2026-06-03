//! Type-level helpers for CM binding synthesis.
//!
//! Houses utilities that translate between AST `Type`, TIR `TypeId`, and
//! Canonical ABI flat / size / alignment information. Shared by the lift,
//! lower, and adapter synthesis paths.

use std::cell::RefCell;

use crate::ast::{AstId, GenericType, NamedType, Type};
use crate::cm_abi;
use crate::compiler_item::{CompilerItem, CompilerItems};
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::IndexMap;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirModule, TirParam, TirStruct,
    TirVariantDecl, TypeId, TypeTable,
};

use crate::synthesis::common::{binary, i32_const, i64_const, synth_span};

/// Snapshot of the stdlib type / variant names the CM binding code
/// matches against — `String`, `List`, `Option`, `Result` — resolved
/// once through the `CompilerItem` registry so a stdlib rename of any
/// of these flows through every CM lift / lower / adapter site
/// without hard-coded literals scattered across `synthesis::cm_binding`.
///
/// `result` is kept for downstream callers that match against the
/// `Result` variant name even when the current consumer set does not
/// need it; populating it costs one registry hit + clone and keeps
/// the snapshot's shape complete for the next CM-binding site that
/// wants to read it.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct CmStdlibNames {
    pub string: String,
    pub array: String,
    pub option: String,
    pub result: String,
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
}

impl CmStdlibNames {
    /// Look up every name through the [`CompilerItems`] registry.
    /// Cheap (a handful of registry hits + clones); `cm_binding`'s
    /// multi-entry-point shape (lift / lower / adapter / `type_fixup` /
    /// `task_return` are all called independently from outside paths)
    /// means each entry rebuilds the snapshot locally rather than
    /// threading a single one through a single context — mirrors the
    /// `from_compiler_items` constructor shape used by the other
    /// synthesis passes (`SerdeStdlibNames`, `FormatStdlibNames`,
    /// `TraitsStdlibNames`).
    pub fn from_compiler_items(items: &CompilerItems) -> Self {
        let (_, _, some_name, some_index) = items.require_variant_case(CompilerItem::OptionSome);
        let (_, _, none_name, none_index) = items.require_variant_case(CompilerItem::OptionNone);
        let (_, _, ok_name, ok_index) = items.require_variant_case(CompilerItem::ResultOk);
        let (_, _, err_name, err_index) = items.require_variant_case(CompilerItem::ResultErr);
        Self {
            string: items.struct_name(CompilerItem::String).to_string(),
            array: items.struct_name(CompilerItem::List).to_string(),
            option: items.variant_name(CompilerItem::Option).to_string(),
            result: items.variant_name(CompilerItem::Result).to_string(),
            some_name: some_name.to_string(),
            some_index,
            none_name: none_name.to_string(),
            none_index,
            ok_name: ok_name.to_string(),
            ok_index,
            err_name: err_name.to_string(),
            err_index,
        }
    }

    /// Canonical-name snapshot for unit tests that do not bootstrap a
    /// full `TypeTable` with the stdlib registered. The names are the
    /// production stdlib defaults; tests that want to exercise rename
    /// behaviour must construct a [`Self`] explicitly.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            string: "String".to_string(),
            array: "List".to_string(),
            option: "Option".to_string(),
            result: "Result".to_string(),
            some_name: "Some".to_string(),
            some_index: 0,
            none_name: "None".to_string(),
            none_index: 1,
            ok_name: "Ok".to_string(),
            ok_index: 0,
            err_name: "Err".to_string(),
            err_index: 1,
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
    /// CM package owning the binding being synthesized (e.g., `"http"`
    /// for `wasi:http/*` bindings, `"kiln"` for `core:kiln/*` bindings).
    /// Required: every CM binding is emitted inside a known package,
    /// and named-type lookups are always scoped by `(name, package)` to
    /// prevent collisions such as `wasi:cli/ErrorCode` vs.
    /// `wasi:http/ErrorCode` — or, across schemes,
    /// `wasi:http/types::Response` vs. `core:kiln/types::Response`.
    pub cm_package: &'a str,
    /// `ModuleSource` interner shared with the package; used by
    /// `module_source_for_cm_interface` to canonicalise the module
    /// identity of synthesised types (e.g. `wasi:http/types`,
    /// `core:kiln/types.wado`) so they match the elaborator's registered
    /// `StructName`s ptr-eq.
    ///
    /// Re-entrancy: the lift call chain may `borrow_mut()` this cell;
    /// callers must not hold a `RefMut` to the same cell across calls
    /// into `synthesize_lift` or its helpers. The lift path itself only
    /// borrows transiently inside `module_source_for_cm_interface`.
    pub interner: &'a RefCell<ModuleSourceInterner>,
}

/// Convert a WASI AST `Type` to a `TypeId` in the type table.
///
/// Every WASI binding is emitted inside a known package (e.g. `"http"`), so
/// both the registry and the owning package are required — there is no
/// unscoped variant. Named types are resolved by `(name, wasi_package)`; if
/// the primary scope misses we consult the registry for the canonical owner
/// of the bare name (e.g. `ErrorCode` is declared in `filesystem/types.wado`
/// but referenced from `http` bindings). Same-named types from distinct
/// interfaces are always distinct `TypeId`s.
///
/// This is needed for synthesized binding code that calls generic methods
/// (e.g., `List::<String>::with_capacity()`). The monomorphizer requires
/// concrete `TypeId`s in `MonomorphInfo::type_args` to instantiate generic
/// methods.
pub fn cm_type_to_type_id(
    ty: &Type,
    type_table: &mut TypeTable,
    registry: &CmInterfaceRegistry,
    wasi_package: &str,
) -> TypeId {
    let string_struct_name = type_table
        .compiler_items()
        .struct_name(crate::compiler_item::CompilerItem::String)
        .to_string();
    match ty {
        Type::Named(named) if named.name.as_str() == string_struct_name => {
            type_table.make_compiler_struct(crate::compiler_item::CompilerItem::String)
        }
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
            // Unit type written as a named type "()"
            "()" => TypeTable::UNIT,
            // Resource/enum/variant types - look up the already-resolved TypeId.
            // Lookups are strictly scoped by `(name, wasi_package)`. If the
            // primary scope misses, we consult the registry for the canonical
            // owning package and retry — never a bare-name scan, which would
            // conflate same-named types from distinct interfaces (e.g.
            // `wasi:filesystem/ErrorCode` vs. `wasi:http/ErrorCode`).
            _ => type_table
                .find_named_type_by_cm_package(named.name.as_str(), wasi_package)
                .or_else(|| {
                    canonical_wasi_package(registry, named.name.as_str()).and_then(|pkg| {
                        type_table.find_named_type_by_cm_package(named.name.as_str(), pkg)
                    })
                })
                .unwrap_or(TypeTable::I32),
        },
        Type::Generic(g) => {
            let list_name = type_table
                .compiler_items()
                .struct_name(crate::compiler_item::CompilerItem::List)
                .to_string();
            if g.name.as_str() == list_name && g.args.len() == 1 {
                let elem_type = cm_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                return type_table.make_list(elem_type);
            }
            let option_name = type_table
                .compiler_items()
                .variant_name(crate::compiler_item::CompilerItem::Option)
                .to_string();
            let result_name = type_table
                .compiler_items()
                .variant_name(crate::compiler_item::CompilerItem::Result)
                .to_string();
            if g.name.as_str() == option_name && g.args.len() == 1 {
                let inner_type = cm_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                return type_table.make_option(inner_type);
            }
            if g.name.as_str() == result_name && g.args.len() == 2 {
                let ok_type = cm_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                let err_type = cm_type_to_type_id(&g.args[1], type_table, registry, wasi_package);
                return type_table.make_result(ok_type, err_type);
            }
            match g.name.as_str() {
                "Stream" if g.args.len() == 1 => {
                    let inner = cm_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                    type_table.make_stream(inner)
                }
                "Future" if g.args.len() == 1 => {
                    let inner = cm_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                    type_table.make_future(inner)
                }
                "AsyncCall" if g.args.len() == 1 => {
                    let inner = cm_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                    type_table.make_async_call(inner)
                }
                // Own/Borrow are handle types represented as i32
                "Own" | "Borrow" => TypeTable::I32,
                _ => TypeTable::UNIT,
            }
        }
        Type::Tuple(types) if types.is_empty() => TypeTable::UNIT,
        Type::Tuple(types) => {
            let resolved: Vec<TypeId> = types
                .iter()
                .map(|t| cm_type_to_type_id(t, type_table, registry, wasi_package))
                .collect();
            type_table.make_tuple(resolved)
        }
        _ => TypeTable::UNIT,
    }
}

/// Extract the WASI package (e.g. `"filesystem"`) from a CM source string like
/// `"wasi:filesystem/types@0.3.0-rc-2026-03-15"`. Returns `None` for
/// non-`wasi:` sources or malformed strings.
pub(super) fn wasi_package_from_cm_source(source: &str) -> Option<&str> {
    let after_colon = source.strip_prefix("wasi:")?;
    let without_version = after_colon.split('@').next().unwrap_or(after_colon);
    without_version.split('/').next()
}

/// Given a bare type name, ask the registry for its canonical owner and return
/// the WASI package (e.g. `"filesystem"`). Used to disambiguate name lookups
/// for types whose canonical owner differs from the currently-processed WASI
/// package (e.g. `ErrorCode` is owned by `filesystem` but referenced from
/// `http` bindings).
pub(super) fn canonical_wasi_package<'a>(
    registry: &'a CmInterfaceRegistry,
    name: &str,
) -> Option<&'a str> {
    for kind in [
        "variants",
        "enums",
        "resources",
        "structs",
        "flags",
        "newtypes",
    ] {
        if let Some(source) = registry.bare_name_owner(kind, name)
            && let Some(pkg) = wasi_package_from_cm_source(source)
        {
            return Some(pkg);
        }
    }
    None
}

/// Derive the Wado-side `ModuleSource` interface suffix from a fully
/// qualified `#[cm]` source interface like `"wasi:clocks/system-clock@0.3.0-rc-..."`.
///
/// Returns e.g. `"clocks/system_clock.wado"`. The WIT kebab-case interface
/// name is converted to Wado's `snake_case` filename convention (matching
/// `wado-from-idl`'s output). Returns an empty string if the source is not a
/// `wasi:` interface (such inputs never occur in WASI-side synthesis because
/// every caller supplies a `NamedType.source_interface` populated by stdlib
/// bootstrap from a WASI module, but we're defensive).
pub(super) fn wasi_interface_suffix(source_interface: &str) -> String {
    let Some(after_colon) = source_interface.strip_prefix("wasi:") else {
        return String::new();
    };
    let without_version = after_colon.split('@').next().unwrap_or(after_colon);
    if let Some((pkg, iface)) = without_version.split_once('/') {
        return format!("{pkg}/{}.wado", iface.replace('-', "_"));
    }
    format!("{without_version}.wado")
}

/// Resolve a CM source interface (e.g. `wasi:filesystem/types@0.3.0`,
/// `core:kiln/types@0.1.0`) to the `ModuleSource` the elaborator uses when
/// registering its types. Keeps the lift path's fabricated `TypeId`s
/// matching the `StructName`s under which the WIR types pass registered
/// them (see `wir_build::types::register_struct`).
pub(super) fn module_source_for_cm_interface(
    interner: &mut ModuleSourceInterner,
    source_interface: &str,
) -> ModuleSource {
    if source_interface.starts_with("wasi:") {
        return interner.wasi(&wasi_interface_suffix(source_interface));
    }
    if let Some(rest) = source_interface.strip_prefix("core:") {
        let without_version = rest.split('@').next().unwrap_or(rest);
        let name = if let Some((pkg, iface)) = without_version.split_once('/') {
            format!("{pkg}/{}.wado", iface.replace('-', "_"))
        } else {
            format!("{without_version}.wado")
        };
        return interner.core(&name);
    }
    ModuleSource::default()
}

/// Create an i32 addition expression.
pub(super) fn binary_add(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(TirBinaryOp::Add, left, right, TypeTable::I32)
}

pub(super) fn binary_ne(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(TirBinaryOp::NotEq, left, right, TypeTable::BOOL)
}

pub(super) fn kebab_to_pascal(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    upper + chars.as_str()
                }
                None => String::new(),
            }
        })
        .collect()
}

pub(super) fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(elems) if elems.is_empty())
        || matches!(ty, Type::Named(n) if n.name == "()")
}

pub(super) fn is_gc_passthrough_param(
    ty: &Type,
    cm_interface_registry: &CmInterfaceRegistry,
    names: &CmStdlibNames,
) -> bool {
    match ty {
        Type::Named(n) if n.name == names.string => true,
        Type::Named(n) => n.source_interface.as_deref().is_some_and(|s| {
            cm_interface_registry
                .get_variant_cases_by_source(s, &n.name)
                .is_some()
                || cm_interface_registry
                    .get_struct_fields_by_source(s, &n.name)
                    .is_some()
        }),
        Type::Generic(g) if g.name == names.array && g.args.len() == 1 => true,
        Type::Generic(g) if g.name == names.option && g.args.len() == 1 => true,
        _ => false,
    }
}

pub(super) fn is_wasm_flat_type(type_id: TypeId) -> bool {
    matches!(
        type_id,
        TypeTable::I32 | TypeTable::I64 | TypeTable::F32 | TypeTable::F64
    )
}

/// Compute the flat ABI parameter types for a WASI function parameter.
pub fn flatten_param_type(
    ty: &Type,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
    names: &CmStdlibNames,
) -> Vec<TypeId> {
    fn cm_val_to_type_id(v: &cm_abi::CmValType) -> TypeId {
        match v {
            cm_abi::CmValType::I32 => TypeTable::I32,
            cm_abi::CmValType::I64 => TypeTable::I64,
            cm_abi::CmValType::F32 => TypeTable::F32,
            cm_abi::CmValType::F64 => TypeTable::F64,
        }
    }

    let resolved = cm_interface_registry.resolve_type(ty);
    match &resolved {
        Type::Named(named) => {
            if named.name == names.string {
                return vec![TypeTable::I32, TypeTable::I32];
            }
            match named.name.as_str() {
                "i32" | "u32" | "bool" | "char" | "i8" | "u8" | "i16" | "u16" => {
                    vec![TypeTable::I32]
                }
                "i64" | "u64" => vec![TypeTable::I64],
                "f32" => vec![TypeTable::F32],
                "f64" => vec![TypeTable::F64],
                name => {
                    // Without a resolved CM-interface source the reference is
                    // not a CM variant/struct — flatten to a single i32 handle.
                    let Some(source) = named
                        .source_interface
                        .as_deref()
                        .filter(|s| crate::component_model::source_uses_cm_abi(s))
                    else {
                        return vec![TypeTable::I32];
                    };
                    // WASI variant: discriminant + join of all case payload flat types.
                    if let Some(cases) =
                        cm_interface_registry.get_variant_cases_by_source(source, name)
                    {
                        let mut result = vec![TypeTable::I32]; // discriminant
                        let case_flats: Vec<Vec<TypeId>> = cases
                            .iter()
                            .map(|c| {
                                c.payload
                                    .as_ref()
                                    .map(|t| flatten_param_type(t, cm_interface_registry, names))
                                    .unwrap_or_default()
                            })
                            .collect();
                        let max_len = case_flats.iter().map(Vec::len).max().unwrap_or(0);
                        for i in 0..max_len {
                            // Join: if all non-empty cases at position i agree on a type,
                            // use that type; otherwise use i32 (per CM spec join).
                            let joined = case_flats
                                .iter()
                                .filter_map(|f| f.get(i).copied())
                                .reduce(|a, b| if a == b { a } else { TypeTable::I32 })
                                .unwrap_or(TypeTable::I32);
                            result.push(joined);
                        }
                        return result;
                    }
                    // WASI struct (record): concatenation of all field flat types.
                    if let Some(fields) = cm_interface_registry
                        .get_struct_fields_with_wado_names_by_source(source, name)
                    {
                        return fields
                            .iter()
                            .flat_map(|(_, _, ft)| {
                                flatten_param_type(ft, cm_interface_registry, names)
                            })
                            .collect();
                    }
                    // Resource handles, enums, flags, etc.: single i32
                    vec![TypeTable::I32]
                }
            }
        }
        Type::Generic(g) if g.name == "Stream" => vec![TypeTable::I32],
        Type::Reference(_) | Type::MutReference(_) => vec![TypeTable::I32],
        Type::Tuple(elems) if elems.is_empty() => vec![],
        _ => {
            let flat = cm_abi::cm_flat_types(&resolved);
            flat.iter().map(cm_val_to_type_id).collect()
        }
    }
}

/// Compute the CM Canonical ABI byte size for a flags type given its label count.
/// Per the CM spec: ≤8 labels → 1 byte, ≤16 → 2 bytes, >16 → ceil(n/32)*4 bytes.
pub fn cm_flags_byte_size(count: usize) -> u32 {
    if count == 0 {
        0
    } else if count <= 8 {
        1
    } else if count <= 16 {
        2
    } else {
        4 * (count as u32).div_ceil(32)
    }
}

/// Compute the CM Canonical ABI alignment for a flags type given its label count.
pub fn cm_flags_byte_align(count: usize) -> u32 {
    if count <= 8 {
        1
    } else if count <= 16 {
        2
    } else {
        4
    }
}

/// Compute the CM Canonical ABI byte size for an enum type given its variant count.
/// Per the CM spec `discriminant_type`: ≤256 → 1 byte, ≤65536 → 2 bytes, else 4 bytes.
pub fn cm_enum_byte_size(count: usize) -> u32 {
    if count <= 256 {
        1
    } else if count <= 65536 {
        2
    } else {
        4
    }
}

/// Compute the CM Canonical ABI size for a param type, resolving WASI types through the registry.
pub(super) fn cm_param_size(
    ty: &Type,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
) -> u32 {
    crate::component_model::cm_size_with_registry(ty, cm_interface_registry)
}

/// Compute the CM Canonical ABI alignment for a param type, resolving WASI types through the registry.
pub(super) fn cm_param_align(
    ty: &Type,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
) -> u32 {
    crate::component_model::cm_align_with_registry(ty, cm_interface_registry)
}

pub(super) fn cm_param_store_plan(
    ty: &Type,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
    names: &CmStdlibNames,
) -> Vec<(u32, &'static str)> {
    if let Type::Named(named) = ty {
        if named.name == names.string {
            return vec![(0, "i32_store"), (4, "i32_store")];
        }
        let source = named
            .source_interface
            .as_deref()
            .filter(|s| s.starts_with("wasi:"));
        // Check WASI flags types.
        if let Some(members) =
            source.and_then(|s| cm_interface_registry.get_flags_members_by_source(s, &named.name))
        {
            let store = match cm_flags_byte_size(members.len()) {
                0 => return vec![],
                1 => "i32_store8",
                2 => "i32_store16",
                _ => "i32_store",
            };
            return vec![(0, store)];
        }
        // Check WASI enum types.
        if let Some(variants) =
            source.and_then(|s| cm_interface_registry.get_enum_variants_by_source(s, &named.name))
        {
            let store = match cm_enum_byte_size(variants.len()) {
                1 => "i32_store8",
                2 => "i32_store16",
                _ => "i32_store",
            };
            return vec![(0, store)];
        }
        // Standard named types
        return match named.name.as_str() {
            "bool" | "u8" | "i8" => vec![(0, "i32_store8")],
            "u16" | "i16" => vec![(0, "i32_store16")],
            "i64" | "u64" => vec![(0, "i64_store")],
            "f32" => vec![(0, "f32_store")],
            "f64" => vec![(0, "f64_store")],
            // i32, u32, char, resource handles
            _ => vec![(0, "i32_store")],
        };
    }
    match ty {
        Type::Reference(_) | Type::MutReference(_) => vec![(0, "i32_store")],
        Type::Generic(g) if g.name == names.array => vec![(0, "i32_store"), (4, "i32_store")],
        Type::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => {
                // option<T>: disc (u8) at offset 0, payload at align_to(1, align(T))
                let inner_align = crate::component_model::cm_align_with_registry(
                    &g.args[0],
                    cm_interface_registry,
                );
                let payload_offset = crate::cm_abi::align_to(1, inner_align);
                let inner_store = cm_param_store_plan(&g.args[0], cm_interface_registry, names);
                let mut stores = vec![(0, "i32_store8")]; // discriminant
                for (sub_offset, store_name) in inner_store {
                    stores.push((payload_offset + sub_offset, store_name));
                }
                stores
            }
            _ => vec![(0, "i32_store")],
        },
        _ => vec![(0, "i32_store")],
    }
}

/// Check whether a return type needs lifting from a flat i32 discriminant to a GC struct.
/// This is true for Result types where all payloads are empty (unit), so the raw call
/// returns just a discriminant on the stack without an outptr.
pub(super) fn needs_flat_result_lifting(ty: &Type) -> bool {
    matches!(ty, Type::Generic(g) if g.name == "Result" && g.args.len() == 2)
}

pub(super) fn compute_export_flat_return_types(
    ty: &Type,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) -> Vec<cm_abi::CmValType> {
    let mut out = Vec::new();
    flatten_export_type(ty, &mut out, tir_modules, type_table);
    out
}

/// Recursively flatten an export type to CM ABI flat values.
pub(super) fn flatten_export_type(
    ty: &Type,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) {
    let names = CmStdlibNames::from_compiler_items(type_table.compiler_items());
    match ty {
        Type::Named(named) if named.name == names.string => {
            out.push(cm_abi::CmValType::I32); // ptr
            out.push(cm_abi::CmValType::I32); // len
        }
        Type::Named(named) => match named.name.as_str() {
            "bool" | "u8" | "i8" | "u16" | "i16" | "i32" | "u32" | "char" => {
                out.push(cm_abi::CmValType::I32);
            }
            "i64" | "u64" => out.push(cm_abi::CmValType::I64),
            "f32" => out.push(cm_abi::CmValType::F32),
            "f64" => out.push(cm_abi::CmValType::F64),
            "()" => {} // unit — no values
            _ => {
                // Check if it's a variant type defined in TIR modules
                if let Some(variant_decl) = find_variant_decl(&named.name, tir_modules) {
                    flatten_variant_type(&variant_decl, out, tir_modules, type_table);
                } else if let Some(struct_decl) = find_struct_decl(&named.name, tir_modules) {
                    flatten_struct_type(&struct_decl, out, tir_modules, type_table);
                } else {
                    // Resource handles, enums, unknown → i32
                    out.push(cm_abi::CmValType::I32);
                }
            }
        },
        Type::Generic(generic) if generic.name == names.array => {
            out.push(cm_abi::CmValType::I32); // ptr
            out.push(cm_abi::CmValType::I32); // len
        }
        Type::Generic(generic) => match generic.name.as_str() {
            "Stream" | "Future" | "Own" | "Borrow" => out.push(cm_abi::CmValType::I32),
            "Option" if generic.args.len() == 1 => {
                out.push(cm_abi::CmValType::I32); // discriminant
                flatten_export_type(&generic.args[0], out, tir_modules, type_table);
            }
            "Result" if generic.args.len() == 2 => {
                out.push(cm_abi::CmValType::I32); // discriminant
                let mut ok_flat = Vec::new();
                let mut err_flat = Vec::new();
                flatten_export_type(&generic.args[0], &mut ok_flat, tir_modules, type_table);
                flatten_export_type(&generic.args[1], &mut err_flat, tir_modules, type_table);
                let max_len = ok_flat.len().max(err_flat.len());
                for i in 0..max_len {
                    let ok_val = ok_flat.get(i).copied();
                    let err_val = err_flat.get(i).copied();
                    out.push(cm_abi::CmValType::join(ok_val, err_val));
                }
            }
            _ => out.push(cm_abi::CmValType::I32),
        },
        Type::Tuple(elems) => {
            for elem in elems {
                flatten_export_type(elem, out, tir_modules, type_table);
            }
        }
        Type::Reference(_) | Type::MutReference(_) => out.push(cm_abi::CmValType::I32),
        _ => {}
    }
}

/// Flatten a variant type: discriminant + union of all case payloads.
pub(super) fn flatten_variant_type(
    variant_decl: &TirVariantDecl,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) {
    out.push(cm_abi::CmValType::I32); // variant discriminant
    let mut max_payload: Vec<cm_abi::CmValType> = Vec::new();
    for case in &variant_decl.cases {
        let case_flat = flat_types_from_type_id(case.payload, tir_modules, type_table);
        // Union: extend with join at each position
        for (i, &val) in case_flat.iter().enumerate() {
            if i < max_payload.len() {
                max_payload[i] = cm_abi::CmValType::join(Some(max_payload[i]), Some(val));
            } else {
                max_payload.push(val);
            }
        }
    }
    out.extend(max_payload);
}

/// Flatten a struct type: concatenation of all field flat types.
pub(super) fn flatten_struct_type(
    struct_decl: &TirStruct,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) {
    for field in &struct_decl.fields {
        flat_types_from_type_id_into(field.type_id, out, tir_modules, type_table);
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
pub(super) fn flat_types_from_type_id_into(
    type_id: TypeId,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) {
    let names = CmStdlibNames::from_compiler_items(type_table.compiler_items());
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
            PrimitiveType::I128 | PrimitiveType::U128 => {
                panic!("i128/u128 cannot appear at CM boundary")
            }
            PrimitiveType::V128 => {
                panic!("v128 cannot appear at CM boundary")
            }
        },
        ResolvedType::Unit => {} // no flat values
        ResolvedType::Struct { name, .. } => {
            if name == &names.string {
                out.push(cm_abi::CmValType::I32); // ptr
                out.push(cm_abi::CmValType::I32); // len
            } else if let Some(struct_decl) = find_struct_decl(name, tir_modules) {
                flatten_struct_type(&struct_decl, out, tir_modules, type_table);
            } else {
                out.push(cm_abi::CmValType::I32); // unknown struct → i32
            }
        }
        ResolvedType::Resource { .. } => out.push(cm_abi::CmValType::I32),
        ResolvedType::Enum { .. } => out.push(cm_abi::CmValType::I32),
        ResolvedType::Variant { name, .. } => {
            if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
                flatten_variant_type(&variant_decl, out, tir_modules, type_table);
            } else {
                out.push(cm_abi::CmValType::I32);
            }
        }
        ResolvedType::GenericInstance {
            name,
            type_args,
            module_source,
        } => {
            if TypeTable::is_tuple_type(name, module_source) {
                for &elem in type_args {
                    flat_types_from_type_id_into(elem, out, tir_modules, type_table);
                }
            } else if name == &names.option && type_args.len() == 1 {
                out.push(cm_abi::CmValType::I32); // discriminant
                flat_types_from_type_id_into(type_args[0], out, tir_modules, type_table);
            } else if name == &names.result && type_args.len() == 2 {
                out.push(cm_abi::CmValType::I32); // discriminant
                let mut ok_flat = Vec::new();
                let mut err_flat = Vec::new();
                flat_types_from_type_id_into(type_args[0], &mut ok_flat, tir_modules, type_table);
                flat_types_from_type_id_into(type_args[1], &mut err_flat, tir_modules, type_table);
                let max_len = ok_flat.len().max(err_flat.len());
                for i in 0..max_len {
                    let ok_val = ok_flat.get(i).copied();
                    let err_val = err_flat.get(i).copied();
                    out.push(cm_abi::CmValType::join(ok_val, err_val));
                }
            } else if name == &names.array {
                out.push(cm_abi::CmValType::I32); // ptr
                out.push(cm_abi::CmValType::I32); // len
            } else {
                out.push(cm_abi::CmValType::I32);
            }
        }
        ResolvedType::Newtype { base_type, .. } => {
            flat_types_from_type_id_into(*base_type, out, tir_modules, type_table);
        }
        ResolvedType::Flags { .. } => {
            // Flags are u32 at the CM ABI level
            out.push(cm_abi::CmValType::I32);
        }
        ResolvedType::GenericResource { .. } => {
            out.push(cm_abi::CmValType::I32);
        }
        _ => {} // Never, Error, Unknown, etc.
    }
}

/// Find a variant declaration by name across all TIR modules.
pub(super) fn find_variant_decl(
    name: &str,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
) -> Option<TirVariantDecl> {
    for module in tir_modules.values() {
        for variant in &module.variants {
            if variant.name == name {
                return Some(variant.clone());
            }
        }
    }
    None
}

/// Find a struct declaration by name across all TIR modules.
pub(super) fn find_struct_decl(
    name: &str,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
) -> Option<TirStruct> {
    for module in tir_modules.values() {
        for s in &module.structs {
            if s.name == name {
                return Some(s.clone());
            }
        }
    }
    None
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
        cm_abi::CmValType::F64 => TirExpr::new(
            TirExprKind::FloatLiteral {
                value: 0.0,
                repr: "0.0".to_string(),
            },
            TypeTable::F64,
            synth_span(),
        ),
    }
}

/// Reconstruct a minimal AST `Type` surface from a TIR `TypeId`.
///
/// Used by callers that need to re-enter AST-shaped `Type` match arms
/// (struct/generic field recursion in
/// `synthesize_lift_from_flat_params`, element lowering in the
/// `List<T>` arm of `lower_to_flat_inner`). The returned value only
/// needs the top-level `name` and (for `GenericInstance`) immediate
/// type args; deeper structural data is already reachable through
/// `tir_modules` + `type_table` and is looked up lazily.
///
/// Named types receive their `source_interface` populated via
/// [`CmInterfaceRegistry::resolve_cm_source_for`] when the registry knows the
/// type (`wasi:*` records or `core:kiln/*` records). Without this,
/// downstream lower / lift helpers can't find the record's field
/// layout because they key the registry lookup by
/// `(source_interface, name)`.
pub(super) fn type_id_to_ast_type(
    type_id: TypeId,
    type_table: &TypeTable,
    cm_interface_registry: &CmInterfaceRegistry,
) -> Type {
    let span = synth_span();
    let resolved = type_table.get(type_id);
    // Only populate `source_interface` when the TIR type's `module_source`
    // proves the type came from a CM namespace (`wasi:*` or
    // `core:kiln/*`). User-local structs may share names with WASI / kiln
    // records (`Span`, `Error`, `Token` …) but must not pick up the CM
    // source — otherwise downstream lift/lower paths look up the wrong
    // record layout and the WIR ends up with mismatched struct refs.
    let named_no_source =
        |name: &str| Type::Named(NamedType::new(AstId::fresh(), name.to_string(), span));
    let cm_named = |name: &str, ms: &ModuleSource| {
        let mut nt = NamedType::new(AstId::fresh(), name.to_string(), span);
        let cm_namespace = match ms {
            ModuleSource::Wasi { .. } => true,
            ModuleSource::Core { name } => name.starts_with("kiln"),
            _ => false,
        };
        if cm_namespace && let Some(source) = cm_interface_registry.resolve_cm_source_for(&nt, None)
        {
            nt.source_interface = Some(source.to_string());
        }
        Type::Named(nt)
    };
    match resolved {
        ResolvedType::Primitive(p) => named_no_source(p.as_str()),
        ResolvedType::Unit => Type::Tuple(Vec::new()),
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => cm_named(name, module_source),
        ResolvedType::Variant {
            name,
            module_source,
            ..
        } => cm_named(name, module_source),
        ResolvedType::Enum {
            name,
            module_source,
        } => cm_named(name, module_source),
        ResolvedType::Resource { name, .. } => named_no_source(name),
        ResolvedType::GenericInstance {
            name, type_args, ..
        } => {
            let args: Vec<Type> = type_args
                .iter()
                .map(|&tid| type_id_to_ast_type(tid, type_table, cm_interface_registry))
                .collect();
            Type::Generic(GenericType {
                id: AstId::fresh(),
                name: name.clone(),
                args,
                span,
            })
        }
        ResolvedType::GenericResource {
            name, type_args, ..
        } => {
            let args: Vec<Type> = type_args
                .iter()
                .map(|&tid| type_id_to_ast_type(tid, type_table, cm_interface_registry))
                .collect();
            Type::Generic(GenericType {
                id: AstId::fresh(),
                name: name.clone(),
                args,
                span,
            })
        }
        ResolvedType::Ref(inner) => Type::Reference(Box::new(type_id_to_ast_type(
            *inner,
            type_table,
            cm_interface_registry,
        ))),
        ResolvedType::MutRef(inner) => Type::MutReference(Box::new(type_id_to_ast_type(
            *inner,
            type_table,
            cm_interface_registry,
        ))),
        _ => named_no_source("i32"),
    }
}

pub(super) fn compute_export_flat_param_types(
    params: &[(String, Type)],
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) -> Vec<cm_abi::CmValType> {
    let mut out = Vec::new();
    for (_name, ty) in params {
        flatten_export_type(ty, &mut out, tir_modules, type_table);
    }
    out
}

/// Does a user function parameter whose TIR `TypeId` is `type_id` need CM
/// flat-ABI lifting at the export boundary?
///
/// A parameter "needs lifting" when its canonical CM flat representation
/// is not a single-slot passthrough of the same Wasm value type.
/// Primitives (`i32`, `f64`, `bool`, `char`, ...) and handle-shaped
/// types (resources, enums, flags) all travel as a single i32 / i64 /
/// f32 / f64 both at the Wasm layer and at the CM layer, so no lifting
/// step is required. Everything else — `String`, `List<T>`,
/// `Option<T>`, `Result<T, E>`, tuples, user structs, variants — expands
/// to either a different value type or multiple values under the flat
/// ABI, and therefore must be reconstructed into a Wado-side value.
///
/// Consults [`TypeTable`] directly; no `tir_modules` or AST traversal.
pub(super) fn param_needs_lifting(type_id: TypeId, tt: &TypeTable) -> bool {
    match tt.get(type_id) {
        ResolvedType::Primitive(prim) => matches!(prim, PrimitiveType::Bool),
        ResolvedType::Unit => true,
        // Single-i32 handle-shaped types flow through.
        ResolvedType::Resource { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::GenericResource { .. } => false,
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
