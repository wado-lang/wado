//! Type-level helpers for CM binding synthesis.
//!
//! Houses utilities that translate between AST `Type`, TIR `TypeId`, and
//! Canonical ABI flat / size / alignment information. Shared by the lift,
//! lower, and adapter synthesis paths.

use std::cell::RefCell;

use crate::ast::{AstId, GenericType, NamedType, Type};
use crate::cm_abi;
use crate::compiler_item::CompilerItem;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::IndexMap;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirModule, TirParam, TirStruct,
    TirVariantDecl, TypeId, TypeTable,
};

use crate::synthesis::common::{binary, builtin_call, cast, i32_const, i64_const, synth_span};

/// Snapshot of the stdlib type / variant names CM binding matches against,
/// resolved once through the `CompilerItem` registry so a stdlib rename flows
/// through every lift / lower / adapter site rather than through literals
/// scattered across `synthesis::cm_binding`.
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
    /// The `IndexValue` trait the list adapters call through, as the
    /// declaration the registry records — never a spelling a user trait could
    /// share.
    pub index_value: crate::name::FqTraitName,
    /// `List`'s head, likewise the declaration the registry records.
    pub array_fq: crate::name::FqTypeName,
}

impl CmStdlibNames {
    /// Look up every name through the [`CompilerItems`] registry.
    /// Cheap (a handful of registry hits + clones). Each synthesis entry
    /// point builds the snapshot once per binding — the lower side threads
    /// it through [`LowerContext`] — mirroring the `from_compiler_items`
    /// constructor shape used by the other synthesis passes
    /// (`SerdeStdlibNames`, `FormatStdlibNames`, `TraitsStdlibNames`).
    pub fn from_type_table(type_table: &crate::tir::TypeTable) -> Self {
        let items = type_table.compiler_items();
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
            index_value: items.trait_fq(CompilerItem::IndexValue),
            array_fq: type_table.compiler_struct_fq_name(CompilerItem::List),
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
            index_value: {
                let mut defs = crate::defs::DefTable::default();
                let def = defs.declare_for_test(
                    &crate::module_source::ModuleSource::traits(),
                    "IndexValue",
                    crate::defs::DefKind::Trait,
                );
                crate::name::FqTraitName::declared(&defs, def)
            },
            array_fq: {
                let mut defs = crate::defs::DefTable::default();
                let def = defs.declare_for_test(
                    &crate::module_source::ModuleSource::list(),
                    "List",
                    crate::defs::DefKind::Struct,
                );
                crate::name::FqTypeName::declared(&defs, def)
            },
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
    /// `ModuleSource` interner shared with the package, used to canonicalise a
    /// synthesised type's module identity so it matches the elaborator's
    /// registered `StructName`s pointer-equal. The lift chain may `borrow_mut`
    /// it, so no caller may hold a `RefMut` across a call into `synthesize_lift`;
    /// the lift path itself borrows only transiently.
    pub interner: &'a RefCell<ModuleSourceInterner>,
}

impl LiftContext<'_> {
    /// Resolve the `ModuleSource` for a CM named type declared by interface FQ
    /// `source`. A lib-local interface returns its recorded entry source; a
    /// WASI/core interface derives its source from the FQ by the canonical
    /// naming convention. Provenance comes from the registry, not an FQ prefix
    /// check, so the resulting struct/variant `TypeId` matches the
    /// elaborator-registered one in every world.
    pub(super) fn module_source_for(&self, source: &str) -> ModuleSource {
        if let Some(entry) = self
            .cm_interface_registry
            .cm_interface_module_source_of(source)
        {
            return entry.clone();
        }
        module_source_for_cm_interface(&mut self.interner.borrow_mut(), source)
    }

    /// Resolve a CM `Type` to its elaborator-registered `TypeId`, lib-aware:
    /// unlike [`cm_type_to_type_id`], a lib-local named type resolves through its
    /// recorded entry `ModuleSource` and yields the concrete GC id rather than
    /// falling back to `i32`. Provenance comes from the registry, not an FQ
    /// prefix check, and containers recurse. WASI and core types go by package.
    pub(super) fn cm_type_id(&self, ty: &Type, tt: &mut TypeTable) -> TypeId {
        match ty {
            Type::Named(n) => {
                if let Some(src) = self.cm_interface_registry.resolve_cm_source_for(n, None)
                    && self
                        .cm_interface_registry
                        .cm_interface_module_source_of(&src)
                        .is_some()
                {
                    if self
                        .cm_interface_registry
                        .get_struct_fields_by_source(&src, &n.name)
                        .is_some()
                    {
                        let ms = self.module_source_for(&src);
                        return {
                            let def = tt
                                .decl_named_in(&n.name, &ms)
                                .expect("the declaration this type names exists");
                            tt.make_struct(crate::tir::StructDef::Decl(def))
                        };
                    }
                    if self
                        .cm_interface_registry
                        .get_variant_cases_by_source(&src, &n.name)
                        .is_some()
                    {
                        let ms = self.module_source_for(&src);
                        return {
                            let def = tt
                                .decl_named_in(&n.name, &ms)
                                .expect("the declaration this type names exists");
                            tt.make_variant(def)
                        };
                    }
                }
                cm_type_to_type_id(ty, tt, self.cm_interface_registry, self.cm_package)
            }
            Type::Tuple(elems) if !elems.is_empty() => {
                let ids: Vec<TypeId> = elems.iter().map(|e| self.cm_type_id(e, tt)).collect();
                tt.make_tuple(ids)
            }
            Type::Generic(g) => {
                let (list_name, option_name, result_name) = {
                    let items = tt.compiler_items();
                    (
                        items
                            .struct_name(crate::compiler_item::CompilerItem::List)
                            .to_string(),
                        items
                            .variant_name(crate::compiler_item::CompilerItem::Option)
                            .to_string(),
                        items
                            .variant_name(crate::compiler_item::CompilerItem::Result)
                            .to_string(),
                    )
                };
                if g.name == list_name && g.args.len() == 1 {
                    let elem = self.cm_type_id(&g.args[0], tt);
                    return tt.make_list(elem);
                }
                if g.name == option_name && g.args.len() == 1 {
                    let inner = self.cm_type_id(&g.args[0], tt);
                    return tt.make_option(inner);
                }
                if g.name == result_name && g.args.len() == 2 {
                    let ok = self.cm_type_id(&g.args[0], tt);
                    let err = self.cm_type_id(&g.args[1], tt);
                    return tt.make_result(ok, err);
                }
                cm_type_to_type_id(ty, tt, self.cm_interface_registry, self.cm_package)
            }
            _ => cm_type_to_type_id(ty, tt, self.cm_interface_registry, self.cm_package),
        }
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
    /// CM package owning the binding being synthesized (same semantics as
    /// `LiftContext::cm_package`).
    pub wasi_package: &'a str,
    /// Stdlib-name snapshot; built once per binding instead of per call.
    pub names: CmStdlibNames,
}

/// Convert a WASI AST `Type` to a `TypeId`. Every WASI binding is emitted inside
/// a known package, so both the registry and the owner are required. A named
/// type resolves by `(name, wasi_package)`, falling back to the registry's
/// canonical owner of the bare name, since `http` bindings may reference an
/// `ErrorCode` declared in `filesystem`.
pub fn cm_type_to_type_id(
    ty: &Type,
    type_table: &mut TypeTable,
    registry: &CmInterfaceRegistry,
    wasi_package: &str,
) -> TypeId {
    let string_struct_name = type_table
        .compiler_struct_name(CompilerItem::String)
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
            //
            // The type's own `source_interface` leads: a package holds several
            // interfaces and two can declare the same name (`wasi:sockets/types`
            // and `wasi:sockets/ip-name-lookup` both declare `ErrorCode`), so
            // scoping by package alone returns whichever registered first. The
            // package lookups behind it cover a type with no recorded interface;
            // neither is ever a bare-name scan.
            _ => registry
                .source_interface(named)
                .and_then(|fq| registry.cm_interface_module_source_of(&fq))
                .and_then(|ms| type_table.find_named_type_by_source(&named.name, ms))
                // A stdlib WASI interface has no recorded `ModuleSource`, so
                // derive the module the FQ maps to and match it exactly.
                .or_else(|| {
                    registry
                        .source_interface(named)
                        .and_then(|fq| cm_interface_module_name(&fq))
                        .and_then(|m| type_table.find_named_type_by_module_name(&named.name, &m))
                })
                .or_else(|| {
                    type_table.find_named_type_by_cm_package(named.name.as_str(), wasi_package)
                })
                .or_else(|| {
                    canonical_wasi_package(registry, named.name.as_str()).and_then(|pkg| {
                        type_table.find_named_type_by_cm_package(named.name.as_str(), pkg)
                    })
                })
                // A lib-local type defined in a submodule: the interface FQ maps
                // to the entry module, so resolve via the type's own recorded
                // module instead.
                .or_else(|| {
                    registry
                        .lib_local_type_source(&named.name)
                        .and_then(|ms| type_table.find_named_type_by_source(&named.name, ms))
                })
                // Resources are bare i32 handles at the CM boundary and need no
                // registered GC type. Anything else without a TypeId would
                // miscompile (e.g. FieldAccess on an i32), so fail loudly.
                .unwrap_or_else(|| {
                    let is_resource = registry
                        .resolve_cm_source_for(named, Some(wasi_package))
                        .is_some_and(|s| {
                            registry
                                .get_resource_cm_name_by_source(&s, &named.name)
                                .is_some()
                        });
                    if is_resource {
                        TypeTable::I32
                    } else {
                        panic!(
                            "CM type `{}` (package `{wasi_package}`) has no registered TypeId",
                            named.name
                        )
                    }
                }),
        },
        Type::Generic(g) => {
            let list_name = type_table
                .compiler_struct_name(CompilerItem::List)
                .to_string();
            if g.name.as_str() == list_name && g.args.len() == 1 {
                let elem_type = cm_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                return type_table.make_list(elem_type);
            }
            let option_name = type_table
                .compiler_variant_name(CompilerItem::Option)
                .to_string();
            let result_name = type_table
                .compiler_variant_name(CompilerItem::Result)
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
                other => panic!("unsupported generic type at CM boundary: {other}"),
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
        // Borrowed resource handles are i32 at the CM boundary.
        Type::Reference(_) | Type::MutReference(_) => TypeTable::I32,
        other => panic!("unsupported type at CM boundary: {other:?}"),
    }
}

/// Extract the WASI package (e.g. `"filesystem"`) from a CM source string like
/// `"wasi:filesystem/types@0.3.0"`. Returns `None` for
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
/// every caller supplies a `cm_interface_registry.source_interface(NamedType)` populated by stdlib
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
        return interner.core(&interface_module_name(rest));
    }
    ModuleSource::default()
}

/// The module name a CM interface FQ maps to (`wasi:sockets/ip-name-lookup@0.3.0`
/// → `sockets/ip_name_lookup.wado`), or `None` for an FQ in neither namespace.
pub(super) fn cm_interface_module_name(source_interface: &str) -> Option<String> {
    if source_interface.starts_with("wasi:") {
        return Some(wasi_interface_suffix(source_interface));
    }
    source_interface
        .strip_prefix("core:")
        .map(interface_module_name)
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

pub(super) fn kebab_to_pascal(s: &str) -> String {
    use heck::ToUpperCamelCase;
    s.to_upper_camel_case()
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
        Type::Named(n) => cm_interface_registry.source_interface(n).is_some_and(|s| {
            cm_interface_registry
                .get_variant_cases_by_source(&s, &n.name)
                .is_some()
                || cm_interface_registry
                    .get_struct_fields_by_source(&s, &n.name)
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

/// Validate that `type_id` has a Component Model value representation, erroring
/// for one that has none in any world — an empty record, which the CM binary
/// format forbids, or a 128-bit / `v128` scalar — rather than emitting an
/// invalid component. Recurses through containers, and rejects a type revisiting
/// itself: WIT has no recursive types, and synthesis would inline one forever.
pub(super) fn check_cm_boundary_representable(
    type_id: TypeId,
    type_table: &TypeTable,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    visited: &mut Vec<TypeId>,
) -> Result<(), String> {
    let names = CmStdlibNames::from_type_table(&type_table);
    check_cm_boundary_representable_inner(type_id, type_table, tir_modules, &names, visited)
}

fn check_cm_boundary_representable_inner(
    type_id: TypeId,
    type_table: &TypeTable,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    names: &CmStdlibNames,
    visited: &mut Vec<TypeId>,
) -> Result<(), String> {
    use crate::tir::{PrimitiveType, ResolvedType as R};

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
    let recurse = |tid, visited: &mut Vec<TypeId>| {
        check_cm_boundary_representable_inner(tid, type_table, tir_modules, names, visited)
    };
    let result = (|visited: &mut Vec<TypeId>| {
        if let Some(inner) = type_table.as_option(type_id) {
            return recurse(inner, visited);
        }
        if let Some(elem) = type_table.as_list(type_id) {
            return recurse(elem, visited);
        }
        if let Some(elems) = type_table.as_tuple(type_id) {
            for e in elems {
                recurse(e, visited)?;
            }
            return Ok(());
        }
        // Exhaustive over `ResolvedType` on purpose — no wildcard. A wildcard
        // would silently classify an unhandled (or future) variant as
        // representable and let it fall through to the i32 lowering, which is
        // exactly the bug this check exists to prevent. New variants must be
        // classified explicitly.
        match type_table.get(type_id) {
            // No CM value representation in any world.
            R::Primitive(PrimitiveType::I128 | PrimitiveType::U128 | PrimitiveType::V128) => {
                Err(format!(
                    "`{}` has no Component Model value representation",
                    type_table.type_name(type_id)
                ))
            }
            // Scalars, plain discriminants, bitflags, and plain resource
            // handles lower to an i32 handle identically in every world.
            R::Primitive(_) | R::Unit | R::Enum { .. } | R::Flags { .. } | R::Resource { .. } => {
                Ok(())
            }
            // The handle is an i32, but its payload is lifted and lowered by
            // value, so it must be classifiable too — `()` is representable
            // yet has no payload type.
            R::GenericResource { def, type_args } => {
                let name = type_table.def_name(*def).to_string();
                let args = type_args.clone();
                for &a in &args {
                    recurse(a, visited)?;
                }
                if let Some(&payload) = args.first()
                    && let Some(reason) = match name.as_str() {
                        "Future" | "FutureWritable" => {
                            crate::component_model::future_payload_rejection(type_table, payload)
                        }
                        "Stream" | "StreamWritable" => {
                            crate::component_model::stream_payload_rejection(type_table, payload)
                        }
                        _ => None,
                    }
                {
                    return Err(reason);
                }
                Ok(())
            }
            R::Struct { def, .. } => {
                let name = type_table.struct_head_name(*def);
                if name == names.string {
                    return Ok(());
                }
                match find_struct_decl(&name, tir_modules) {
                    Some(decl) if decl.fields.is_empty() => Err(format!(
                        "record `{name}` has no fields; an empty record has no \
                         Component Model representation — add at least one field"
                    )),
                    Some(decl) => {
                        let field_tys: Vec<TypeId> =
                            decl.fields.iter().map(|f| f.type_id).collect();
                        for ft in field_tys {
                            recurse(ft, visited)?;
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
            R::Variant { def } => {
                let name = type_table.def_name(*def).to_string();
                match find_variant_decl(&name, tir_modules) {
                    Some(decl) => {
                        let payloads: Vec<TypeId> = decl.cases.iter().map(|c| c.payload).collect();
                        for p in payloads {
                            recurse(p, visited)?;
                        }
                        Ok(())
                    }
                    None => Ok(()),
                }
            }
            R::GenericInstance { def, type_args } => {
                let name = &type_table.def_name(*def).to_string();
                // Option/List/Tuple were handled above by the `as_*` accessors;
                // `Result<T, E>` recurses into its arms. Any other generic
                // instance has no concrete CM lowering at this boundary (it
                // should have monomorphized to a named type), so reject it
                // rather than lowering it as an opaque i32.
                let result_name = type_table
                    .compiler_items()
                    .variant_name(crate::compiler_item::CompilerItem::Result);
                if name == result_name {
                    let args = type_args.clone();
                    for a in args {
                        recurse(a, visited)?;
                    }
                    Ok(())
                } else {
                    Err(format!(
                        "generic type `{}` has no Component Model value \
                         representation at an export boundary",
                        type_table.type_name(type_id)
                    ))
                }
            }
            R::Newtype { base_type, .. } => {
                let base = *base_type;
                recurse(base, visited)
            }
            // These never carry a CM value at a concrete export boundary
            // (diverging/never, closures, reactive cells, raw GC arrays,
            // unmonomorphized type parameters, or unresolved/error types).
            // Reject explicitly instead of silently lowering to i32.
            R::Never
            | R::Ref(_)
            | R::MutRef(_)
            | R::Function { .. }
            | R::Reactive(_)
            | R::TypeParam { .. }
            | R::TypePack { .. }
            | R::InferVar(_)
            | R::AssocTypeProjection { .. }
            | R::BuiltinArray(_)
            | R::Unknown
            | R::Error => Err(format!(
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
    let (as_int, int_ty) = flat_to_int_bits(value, have);
    let sized = if cmval_size(have) > cmval_size(want) {
        cast(as_int, TypeTable::I32)
    } else {
        let _ = int_ty;
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
    let (as_int, _int_ty) = flat_to_int_bits(value, want);
    let sized = if cmval_size(have) > cmval_size(want) {
        cast(as_int, TypeTable::I64)
    } else {
        as_int
    };
    flat_from_int_bits(sized, have)
}

/// Compute the flat ABI parameter types for a CM function parameter.
///
/// Adapter mapping [`CmInterfaceRegistry::cm_flatten`] to `TypeId`s; the
/// `names.string` guard handles a non-`"String"` prelude String name first.
pub fn flatten_param_type(
    ty: &Type,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
    names: &CmStdlibNames,
) -> Vec<TypeId> {
    let resolved = cm_interface_registry.resolve_type(ty);
    if matches!(&resolved, Type::Named(n) if n.name == names.string) {
        return vec![TypeTable::I32, TypeTable::I32];
    }
    cm_interface_registry
        .cm_flatten(&resolved)
        .into_iter()
        .map(cm_val_type_to_type_id)
        .collect()
}

pub use crate::cm_abi::{cm_enum_byte_size, cm_flags_byte_size};

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

pub(super) fn cm_param_store_plan(
    ty: &Type,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
    names: &CmStdlibNames,
) -> Vec<(u32, &'static str)> {
    if let Type::Named(named) = ty {
        if named.name == names.string {
            return vec![(0, "i32_store"), (4, "i32_store")];
        }
        let source = cm_interface_registry
            .source_interface(named)
            .filter(|s| s.starts_with("wasi:"));
        // Check WASI flags types.
        if let Some(members) = source
            .as_deref()
            .and_then(|s| cm_interface_registry.get_flags_members_by_source(s, &named.name))
        {
            let store = match cm_flags_byte_size(members.len()) {
                0 => return vec![],
                size @ (1 | 2 | 4) => disc_store_op(size),
                size => panic!(
                    "flags `{}` with {} members ({size} bytes) exceeds the single-i32 store plan",
                    named.name,
                    members.len()
                ),
            };
            return vec![(0, store)];
        }
        // Check WASI enum types.
        if let Some(variants) = source
            .as_deref()
            .and_then(|s| cm_interface_registry.get_enum_variants_by_source(s, &named.name))
        {
            let store = disc_store_op(cm_enum_byte_size(variants.len()));
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
        Type::Generic(g) if g.name == names.option && g.args.len() == 1 => {
            // option<T>: disc (u8) at offset 0, payload at align_to(1, align(T))
            let inner_align =
                crate::component_model::cm_align_with_registry(&g.args[0], cm_interface_registry);
            let payload_offset = crate::cm_abi::align_to(1, inner_align);
            let inner_store = cm_param_store_plan(&g.args[0], cm_interface_registry, names);
            let mut stores = vec![(0, "i32_store8")]; // discriminant
            for (sub_offset, store_name) in inner_store {
                stores.push((payload_offset + sub_offset, store_name));
            }
            stores
        }
        Type::Generic(_) => vec![(0, "i32_store")],
        _ => vec![(0, "i32_store")],
    }
}

/// Check whether a return type needs lifting from a flat i32 discriminant to a GC struct.
/// This is true for Result types where all payloads are empty (unit), so the raw call
/// returns just a discriminant on the stack without an outptr.
pub(super) fn needs_flat_result_lifting(ty: &Type, names: &CmStdlibNames) -> bool {
    matches!(ty, Type::Generic(g) if g.name == names.result && g.args.len() == 2)
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
///
/// Thin wrapper that builds the [`CmStdlibNames`] snapshot once and delegates
/// to the recursive inner function, so recursion does not rebuild it per level.
pub(super) fn flatten_export_type(
    ty: &Type,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
) {
    let names = CmStdlibNames::from_type_table(&type_table);
    flatten_export_type_inner(ty, out, tir_modules, type_table, &names);
}

fn flatten_export_type_inner(
    ty: &Type,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &TypeTable,
    names: &CmStdlibNames,
) {
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
                    flatten_variant_type(&variant_decl, out, tir_modules, type_table, names);
                } else if let Some(struct_decl) = find_struct_decl(&named.name, tir_modules) {
                    flatten_struct_type(&struct_decl, out, tir_modules, type_table, names);
                } else if let Some(nt_type_id) = find_newtype_type_id(&named.name, tir_modules) {
                    // A newtype flattens as its base, not the i32 fallback below,
                    // so the flat signature matches the canonical ABI.
                    flat_types_from_type_id_inner(nt_type_id, out, tir_modules, type_table, names);
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
        Type::Generic(generic) if generic.name == names.option && generic.args.len() == 1 => {
            out.push(cm_abi::CmValType::I32); // discriminant
            flatten_export_type_inner(&generic.args[0], out, tir_modules, type_table, names);
        }
        Type::Generic(generic) if generic.name == names.result && generic.args.len() == 2 => {
            out.push(cm_abi::CmValType::I32); // discriminant
            let mut ok_flat = Vec::new();
            let mut err_flat = Vec::new();
            flatten_export_type_inner(
                &generic.args[0],
                &mut ok_flat,
                tir_modules,
                type_table,
                names,
            );
            flatten_export_type_inner(
                &generic.args[1],
                &mut err_flat,
                tir_modules,
                type_table,
                names,
            );
            out.extend(cm_abi::join_flat_unions(&ok_flat, &err_flat));
        }
        // Stream / Future / Own / Borrow and other generics are i32 handles.
        Type::Generic(_) => out.push(cm_abi::CmValType::I32),
        Type::Tuple(elems) => {
            for elem in elems {
                flatten_export_type_inner(elem, out, tir_modules, type_table, names);
            }
        }
        Type::Reference(_) | Type::MutReference(_) => out.push(cm_abi::CmValType::I32),
        _ => {}
    }
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
    let names = CmStdlibNames::from_type_table(&type_table);
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
            PrimitiveType::I128 | PrimitiveType::U128 => {
                panic!("i128/u128 cannot appear at CM boundary")
            }
            PrimitiveType::V128 => {
                panic!("v128 cannot appear at CM boundary")
            }
        },
        ResolvedType::Unit => {} // no flat values
        ResolvedType::Struct { def, .. } => {
            let name = &type_table.struct_head_name(*def);
            if name == &names.string {
                out.push(cm_abi::CmValType::I32); // ptr
                out.push(cm_abi::CmValType::I32); // len
            } else if let Some(struct_decl) = find_struct_decl(name, tir_modules) {
                flatten_struct_type(&struct_decl, out, tir_modules, type_table, names);
            } else {
                // A record with no TIR declaration has no known field layout;
                // flattening it as one i32 would emit a wrong-arity lowering for
                // a multi-field record. Fail loudly rather than corrupt the
                // component (the memory lowerer panics on the same condition).
                panic!("struct `{name}` has no TIR declaration; cannot compute its flat CM types");
            }
        }
        ResolvedType::Resource { .. } => out.push(cm_abi::CmValType::I32),
        ResolvedType::Enum { .. } => out.push(cm_abi::CmValType::I32),
        ResolvedType::Variant { def } => {
            let name = &type_table.def_name(*def).to_string();
            if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
                flatten_variant_type(&variant_decl, out, tir_modules, type_table, names);
            } else {
                out.push(cm_abi::CmValType::I32);
            }
        }
        ResolvedType::GenericInstance { def, type_args } => {
            let name = &type_table.def_name(*def).to_string();
            if TypeTable::is_tuple_type(name) {
                for &elem in type_args {
                    flat_types_from_type_id_inner(elem, out, tir_modules, type_table, names);
                }
            } else if name == &names.option && type_args.len() == 1 {
                out.push(cm_abi::CmValType::I32); // discriminant
                flat_types_from_type_id_inner(type_args[0], out, tir_modules, type_table, names);
            } else if name == &names.result && type_args.len() == 2 {
                out.push(cm_abi::CmValType::I32); // discriminant
                let mut ok_flat = Vec::new();
                let mut err_flat = Vec::new();
                flat_types_from_type_id_inner(
                    type_args[0],
                    &mut ok_flat,
                    tir_modules,
                    type_table,
                    names,
                );
                flat_types_from_type_id_inner(
                    type_args[1],
                    &mut err_flat,
                    tir_modules,
                    type_table,
                    names,
                );
                out.extend(cm_abi::join_flat_unions(&ok_flat, &err_flat));
            } else if name == &names.array {
                out.push(cm_abi::CmValType::I32); // ptr
                out.push(cm_abi::CmValType::I32); // len
            } else {
                out.push(cm_abi::CmValType::I32);
            }
        }
        ResolvedType::Newtype { base_type, .. } => {
            flat_types_from_type_id_inner(*base_type, out, tir_modules, type_table, names);
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

/// Find the `TypeId` of a newtype declaration by name across all TIR modules.
pub(super) fn find_newtype_type_id(
    name: &str,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
) -> Option<TypeId> {
    for module in tir_modules.values() {
        for nt in &module.newtypes {
            if nt.name == name {
                return Some(nt.type_id);
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

/// Reconstruct a minimal AST `Type` from a TIR `TypeId`, for callers that need
/// to re-enter the AST-shaped match arms. Only the top-level name and immediate
/// type args are filled in; deeper structure is looked up lazily. A named type
/// gets its `source_interface` where the registry knows it, since the lift /
/// lower helpers key by `(source_interface, name)`.
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
        let nt = NamedType::new(AstId::fresh(), name.to_string(), span);
        // Derive the owning WASI package from the type's own `module_source`
        // so a name shared across packages (e.g. `ErrorCode` in `wasi:cli`,
        // `wasi:filesystem`, `wasi:http`, `wasi:sockets`) resolves to *this*
        // type's package — not whichever unique-by-name match the registry
        // happens to find first. Without the hint, the three variant
        // `ErrorCode`s are non-unique and resolution falls through to the
        // lone `wasi:cli` enum, mis-lifting a filesystem variant as an i32.
        let (cm_namespace, pkg_hint) = match ms {
            ModuleSource::Wasi { interface } => (true, interface.split('/').next()),
            ModuleSource::Core { name } if name == "kiln" || name.starts_with("kiln/") => {
                (true, None)
            }
            _ => (false, None),
        };
        if cm_namespace
            && let Some(source) = cm_interface_registry.resolve_cm_source_for(&nt, pkg_hint)
        {
            cm_interface_registry.set_source_interface(nt.id, source);
        }
        Type::Named(nt)
    };
    match resolved {
        ResolvedType::Primitive(p) => named_no_source(p.as_str()),
        ResolvedType::Unit => Type::Tuple(Vec::new()),
        // `Flags` joins them: its own CM type, 1 byte at <=8 labels, not a
        // four-byte `i32`.
        ResolvedType::Struct { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. } => {
            let (name, module_source) = type_table
                .nominal_head(type_id)
                .expect("a nominal type names a declaration");
            cm_named(&name, &module_source)
        }
        ResolvedType::Resource { def } => named_no_source(type_table.def_name(*def)),
        ResolvedType::GenericInstance { def, type_args } => {
            let name = &type_table.def_name(*def).to_string();

            let args: Vec<Type> = type_args
                .iter()
                .map(|&tid| type_id_to_ast_type(tid, type_table, cm_interface_registry))
                .collect();
            // The tuple family is a `GenericInstance`, but its CM surface is a
            // structural tuple — emit `Type::Tuple` so lift/lower dispatch on
            // the tuple arm rather than the generic catch-all.
            if TypeTable::is_tuple_type(name) {
                Type::Tuple(args)
            } else {
                Type::Generic(GenericType {
                    id: AstId::fresh(),
                    name: name.clone(),
                    args,
                    span,
                })
            }
        }
        ResolvedType::GenericResource { def, type_args } => {
            let name = &type_table.def_name(*def).to_string();
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
        ResolvedType::Newtype { .. } => {
            let (name, module_source) = type_table
                .nominal_head(type_id)
                .expect("a newtype names a declaration");
            cm_named(&name, &module_source)
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
        ResolvedType::Never
        | ResolvedType::Function { .. }
        | ResolvedType::Reactive(_)
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::TypeParam { .. }
        | ResolvedType::InferVar(_)
        | ResolvedType::TypePack { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::Unknown
        | ResolvedType::Error => {
            panic!("type has no Component Model surface: {resolved:?}")
        }
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

/// Whether a parameter needs CM flat-ABI lifting at the export boundary — that
/// is, whether its flat representation is anything but a single-slot passthrough
/// of the same Wasm value type. Handle-shaped types (resource, enum, flags) and
/// every primitive but `bool` travel as one scalar at both layers and need none;
/// `bool`, `Unit`, and everything else must be reconstructed Wado-side.
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
