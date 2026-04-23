//! CM Binding Synthesis phase.
//!
//! Generates TIR binding functions for Component Model boundary crossing.
//! Each binding handles lifting Wado values to CM flat ABI (lowering params)
//! and lifting CM flat ABI values back to Wado types (lifting results).
//!
//! Pipeline position: after `effect_check`, before monomorphize.
//! This ensures binding functions go through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-15-cm-binding-synthesis.md` for design details.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{AstId, GenericType, NamedType, Type};
use crate::cm_abi;
use crate::component_model::{CmVariantCase, WasiFunctionInfo, WasiRegistry};
use crate::name::LocalMethodName;
use crate::name::ModuleSource;
use crate::package::Package;
use crate::tir::{
    CallArg, EffectRef, FunctionRef, InlineHint, MonomorphInfo, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirParam, TirStmt, TirStmtKind, TypeId, TypeTable,
};

use super::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, cast, cm_raw_call, entry_call,
    expr_stmt, generic_method_call, generic_static_call, i32_const, i64_const, if_stmt,
    internal_call, let_mut_stmt, let_stmt, local_ref, loop_stmt, null_expr, option_none,
    option_some, return_stmt, synth_span,
};

/// Context for lifting CM values to GC types, providing access to
/// the WASI registry (for variant/enum case info) and type table (for `TypeIds`).
pub struct LiftContext<'a> {
    pub wasi_registry: &'a WasiRegistry,
    pub type_table: &'a RefCell<TypeTable>,
    /// WASI package owning the binding being synthesized (e.g., `"http"`).
    /// Required: every WASI binding is emitted inside a known package, and
    /// named-type lookups are always scoped by `(name, wasi_package)` to
    /// prevent collisions such as `wasi:cli/ErrorCode` vs. `wasi:http/ErrorCode`.
    pub wasi_package: &'a str,
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
/// (e.g., `Array::<String>::with_capacity()`). The monomorphizer requires
/// concrete `TypeId`s in `MonomorphInfo::type_args` to instantiate generic
/// methods.
pub fn wasi_type_to_type_id(
    ty: &Type,
    type_table: &mut TypeTable,
    registry: &WasiRegistry,
    wasi_package: &str,
) -> TypeId {
    match ty {
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
            "String" => type_table.make_struct("String".to_string(), ModuleSource::string()),
            // Resource/enum/variant types - look up the already-resolved TypeId.
            // Lookups are strictly scoped by `(name, wasi_package)`. If the
            // primary scope misses, we consult the registry for the canonical
            // owning package and retry — never a bare-name scan, which would
            // conflate same-named types from distinct interfaces (e.g.
            // `wasi:filesystem/ErrorCode` vs. `wasi:http/ErrorCode`).
            _ => type_table
                .find_named_type_by_wasi_package(named.name.as_str(), wasi_package)
                .or_else(|| {
                    canonical_wasi_package(registry, named.name.as_str()).and_then(|pkg| {
                        type_table.find_named_type_by_wasi_package(named.name.as_str(), pkg)
                    })
                })
                .unwrap_or(TypeTable::I32),
        },
        Type::Generic(g) => match g.name.as_str() {
            "Array" if g.args.len() == 1 => {
                let elem_type =
                    wasi_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                type_table.make_array(elem_type)
            }
            "Option" if g.args.len() == 1 => {
                let inner_type =
                    wasi_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                type_table.make_option(inner_type)
            }
            "Result" if g.args.len() == 2 => {
                let ok_type = wasi_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                let err_type = wasi_type_to_type_id(&g.args[1], type_table, registry, wasi_package);
                type_table.make_result(ok_type, err_type)
            }
            "Stream" if g.args.len() == 1 => {
                let inner = wasi_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                type_table.make_stream(inner)
            }
            "Future" if g.args.len() == 1 => {
                let inner = wasi_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                type_table.make_future(inner)
            }
            "AsyncCall" if g.args.len() == 1 => {
                let inner = wasi_type_to_type_id(&g.args[0], type_table, registry, wasi_package);
                type_table.make_async_call(inner)
            }
            // Own/Borrow are handle types represented as i32
            "Own" | "Borrow" => TypeTable::I32,
            _ => TypeTable::UNIT,
        },
        Type::Tuple(types) if types.is_empty() => TypeTable::UNIT,
        Type::Tuple(types) => {
            let resolved: Vec<TypeId> = types
                .iter()
                .map(|t| wasi_type_to_type_id(t, type_table, registry, wasi_package))
                .collect();
            type_table.make_tuple(resolved)
        }
        _ => TypeTable::UNIT,
    }
}

/// Extract the WASI package (e.g. `"filesystem"`) from a CM source string like
/// `"wasi:filesystem/types@0.3.0-rc-2026-03-15"`. Returns `None` for
/// non-`wasi:` sources or malformed strings.
fn wasi_package_from_cm_source(source: &str) -> Option<&str> {
    let after_colon = source.strip_prefix("wasi:")?;
    let without_version = after_colon.split('@').next().unwrap_or(after_colon);
    without_version.split('/').next()
}

/// Given a bare type name, ask the registry for its canonical owner and return
/// the WASI package (e.g. `"filesystem"`). Used to disambiguate name lookups
/// for types whose canonical owner differs from the currently-processed WASI
/// package (e.g. `ErrorCode` is owned by `filesystem` but referenced from
/// `http` bindings).
fn canonical_wasi_package<'a>(registry: &'a WasiRegistry, name: &str) -> Option<&'a str> {
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
fn wasi_interface_suffix(source_interface: &str) -> String {
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
/// `core:kiln/types@0.1.0`) to the `ModuleSource` the resolver uses when
/// registering its types. Keeps the lift path's fabricated `TypeId`s
/// matching the `StructName`s under which the WIR types pass registered
/// them (see `wir_build::types::register_struct`).
fn module_source_for_cm_interface(source_interface: &str) -> ModuleSource {
    if source_interface.starts_with("wasi:") {
        return ModuleSource::Wasi {
            interface: wasi_interface_suffix(source_interface),
        };
    }
    if let Some(rest) = source_interface.strip_prefix("core:") {
        let without_version = rest.split('@').next().unwrap_or(rest);
        let name = if let Some((pkg, iface)) = without_version.split_once('/') {
            format!("{pkg}/{}.wado", iface.replace('-', "_"))
        } else {
            format!("{without_version}.wado")
        };
        return ModuleSource::Core { name };
    }
    ModuleSource::default()
}

/// Create an i32 addition expression.
fn binary_add(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(crate::tir::TirBinaryOp::Add, left, right, TypeTable::I32)
}

fn binary_ne(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(crate::tir::TirBinaryOp::NotEq, left, right, TypeTable::BOOL)
}

/// Synthesize a TIR expression that loads a CM value from linear memory.
///
/// For primitives, returns a single expression (no setup statements).
/// For composites (list, option, result), emits setup statements into `stmts`
/// and returns a local reference to the lifted value.
///
/// `next_local` / `local_types` track local variable allocation.
/// Callers that don't need composite support may pass empty `&mut vec![]`.
pub fn synthesize_lift(
    ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
) -> TirExpr {
    synthesize_lift_inner(ty, addr, next_local, stmts, local_types, None)
}

/// Lift with WASI context, enabling proper variant/enum construction.
pub fn synthesize_lift_with_context(
    ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    synthesize_lift_inner(ty, addr, next_local, stmts, local_types, Some(ctx))
}

/// Ensure a lifted expression is evaluated before subsequent memory operations.
///
/// If the expression is already a local reference or unit value, it is returned as-is.
/// Otherwise, it is materialized into a local variable. This prevents use-after-free
/// when the outptr buffer is freed after lifting but the lifted expression still
/// contains a bare memory load (e.g., `i32.load(outptr)`).
///
/// For complex types (structs, arrays, variants), `synthesize_lift` already materializes
/// intermediate results into locals, so those expressions are already safe.
fn materialize_if_needed(
    expr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
) -> TirExpr {
    if matches!(
        expr.kind,
        TirExprKind::Local { .. } | TirExprKind::Unit | TirExprKind::TupleLiteral { .. }
    ) {
        // Local and Unit are already evaluated. TupleLiteral elements are
        // individually materialized in synthesize_lift_tuple, so the whole
        // expression is safe to evaluate after freeing.
        return expr;
    }
    // For non-local expressions, check if they reference memory by looking at
    // the expression tree. Builtins like i32_load, i32_load8_u, etc. read from
    // linear memory and must be materialized before any free.
    // Use the expression's own type_id for the local, which is correct for
    // primitive types and handles (i32, i64, f32, f64, u8, u16, bool, char).
    let type_id = expr.type_id;
    let local = alloc_local(next_local, local_types, type_id);
    let name = format!("__lifted_result_{local}");
    stmts.push(let_stmt(&name, local, type_id, expr));
    local_ref(local, &name, type_id)
}

fn synthesize_lift_inner(
    ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => builtin_call("i32_load", vec![addr], TypeTable::I32),
            "i64" | "u64" => builtin_call("i64_load", vec![addr], TypeTable::I64),
            "f32" => builtin_call("f32_load", vec![addr], TypeTable::F32),
            "f64" => builtin_call("f64_load", vec![addr], TypeTable::F64),
            "i8" | "u8" => builtin_call("i32_load8_u", vec![addr], TypeTable::U8),
            "i16" | "u16" => builtin_call("i32_load16_u", vec![addr], TypeTable::U16),
            "bool" => {
                let raw = builtin_call("i32_load8_u", vec![addr], TypeTable::U8);
                binary(
                    crate::tir::TirBinaryOp::NotEq,
                    raw,
                    i32_const(0),
                    TypeTable::BOOL,
                )
            }
            "char" => builtin_call("i32_load", vec![addr], TypeTable::CHAR),
            "String" => {
                let ptr = builtin_call("i32_load", vec![addr.clone()], TypeTable::I32);
                let len = builtin_call(
                    "i32_load",
                    vec![binary_add(addr, i32_const(4))],
                    TypeTable::I32,
                );
                let string_type_id = ctx.map_or(TypeTable::I32, |c| {
                    c.type_table
                        .borrow_mut()
                        .make_struct("String".to_string(), ModuleSource::string())
                });
                internal_call("memory_to_gc_string", vec![ptr, len], string_type_id)
            }
            _ => {
                // CM named types arrive with `source_interface` populated
                // either by stdlib bootstrap or by `resolve_cm_source_for`
                // (fallback to the unique `wasi:*` registrant, biased by the
                // current binding's WASI package, then to `core:kiln/*` for
                // generator-world bindings). Non-CM references fall through
                // to the i32-handle default.
                if let Some(ctx) = ctx
                    && let Some(source) = ctx
                        .wasi_registry
                        .resolve_cm_source_for(named, Some(ctx.wasi_package))
                        .map(str::to_string)
                {
                    let source = source.as_str();
                    if let Some(lifted) = try_lift_wasi_variant_or_enum(
                        named,
                        source,
                        addr.clone(),
                        next_local,
                        stmts,
                        local_types,
                        ctx,
                    ) {
                        return lifted;
                    }
                    if let Some(lifted) = try_lift_wasi_struct(
                        named,
                        source,
                        addr.clone(),
                        next_local,
                        stmts,
                        local_types,
                        ctx,
                    ) {
                        return lifted;
                    }
                    if let Some(members) = ctx
                        .wasi_registry
                        .get_flags_members_by_source(source, &named.name)
                    {
                        let load_name = match cm_flags_byte_size(members.len()) {
                            0 => return i32_const(0),
                            1 => "i32_load8_u",
                            2 => "i32_load16_u",
                            _ => "i32_load",
                        };
                        return builtin_call(load_name, vec![addr], TypeTable::I32);
                    }
                    if let Some(variants) = ctx
                        .wasi_registry
                        .get_enum_variants_by_source(source, &named.name)
                    {
                        let load_name = if variants.len() <= 256 {
                            "i32_load8_u"
                        } else if variants.len() <= 65536 {
                            "i32_load16_u"
                        } else {
                            "i32_load"
                        };
                        return builtin_call(load_name, vec![addr], TypeTable::I32);
                    }
                }
                // Default: treat as i32 handles (resources, unknown types)
                builtin_call("i32_load", vec![addr], TypeTable::I32)
            }
        },
        Type::Generic(g) => match g.name.as_str() {
            "Array" if g.args.len() == 1 => {
                synthesize_lift_list(&g.args[0], addr, next_local, stmts, local_types, ctx)
            }
            "Option" if g.args.len() == 1 => {
                synthesize_lift_option_inner(&g.args[0], addr, next_local, stmts, local_types, ctx)
            }
            "Result" if g.args.len() == 2 => synthesize_lift_result_inner(
                &g.args[0],
                &g.args[1],
                addr,
                next_local,
                stmts,
                local_types,
                ctx,
            ),
            // Own<T>, Borrow<T>, Stream<T>, Future<T> are i32 handles
            _ => builtin_call("i32_load", vec![addr], TypeTable::I32),
        },
        Type::Tuple(elems) if elems.is_empty() => {
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span())
        }
        Type::Tuple(elems) => {
            synthesize_lift_tuple(elems, addr, next_local, stmts, local_types, ctx)
        }
        Type::Reference(_) | Type::MutReference(_) => {
            builtin_call("i32_load", vec![addr], TypeTable::I32)
        }
        _ => builtin_call("i32_load", vec![addr], TypeTable::I32),
    }
}

/// Try to lift a WASI variant or enum type from linear memory into a GC
/// struct. Returns `None` if `(source, named.name)` is not a variant or
/// enum in the registry.
fn try_lift_wasi_variant_or_enum(
    named: &crate::ast::NamedType,
    source: &str,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: &LiftContext<'_>,
) -> Option<TirExpr> {
    let tt = ctx.type_table.borrow();
    if let Some(cases) = ctx
        .wasi_registry
        .get_variant_cases_by_source(source, &named.name)
    {
        let cases = cases.to_vec();
        let variant_type = tt
            .find_named_type_by_wasi_package(&named.name, ctx.wasi_package)
            .or_else(|| {
                canonical_wasi_package(ctx.wasi_registry, &named.name)
                    .and_then(|pkg| tt.find_named_type_by_wasi_package(&named.name, pkg))
            })?;
        drop(tt);
        return Some(synthesize_lift_wasi_variant(
            &named.name,
            variant_type,
            &cases,
            addr,
            next_local,
            stmts,
            local_types,
            Some(ctx),
        ));
    }
    if let Some(case_names) = ctx
        .wasi_registry
        .get_enum_variants_by_source(source, &named.name)
    {
        let case_names = case_names.to_vec();
        let enum_type = tt
            .find_named_type_by_wasi_package(&named.name, ctx.wasi_package)
            .or_else(|| {
                canonical_wasi_package(ctx.wasi_registry, &named.name)
                    .and_then(|pkg| tt.find_named_type_by_wasi_package(&named.name, pkg))
            })?;
        drop(tt);
        return Some(synthesize_lift_wasi_enum(
            &named.name,
            enum_type,
            &case_names,
            addr,
            next_local,
            stmts,
            local_types,
        ));
    }
    None
}

/// Try to lift a WASI struct (record) type from linear memory into a GC
/// struct. Returns `None` if `(source, named.name)` is not a registered
/// struct.
fn try_lift_wasi_struct(
    named: &crate::ast::NamedType,
    source: &str,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: &LiftContext<'_>,
) -> Option<TirExpr> {
    let fields = ctx
        .wasi_registry
        .get_struct_fields_by_source(source, &named.name)?;
    let fields = fields.to_vec();

    // Resolve field types through newtypes and compute the record layout
    let resolved_fields: Vec<(String, Type)> = fields
        .iter()
        .map(|(fname, fty)| (fname.clone(), ctx.wasi_registry.resolve_type(fty)))
        .collect();
    let field_types: Vec<&Type> = resolved_fields.iter().map(|(_, ty)| ty).collect();

    // Compute field offsets using registry-aware layout
    let mut offset = 0u32;
    let mut max_align = 1u32;
    let mut offsets = Vec::with_capacity(field_types.len());
    for ft in &field_types {
        let fa = crate::component_model::cm_align_with_registry(ft, ctx.wasi_registry);
        let fs = crate::component_model::cm_size_with_registry(ft, ctx.wasi_registry);
        offset = crate::cm_abi::align_to(offset, fa);
        offsets.push(offset);
        offset += fs;
        max_align = max_align.max(fa);
    }

    // Create the struct type in the type table using the exact source
    // interface — no scan across packages. Covers both `wasi:*` and
    // `core:kiln/*` so that nested struct lifts (e.g. `InputFile` inside
    // `Array<InputFile>`) hit the same `StructName` that
    // `wir_build::types::register_struct` registered.
    let struct_type_id = {
        let mut tt = ctx.type_table.borrow_mut();
        tt.make_struct(named.name.clone(), module_source_for_cm_interface(source))
    };

    // Lift each field — Wado field names come directly from this interface's
    // registration, which must exist since we just proved the struct does.
    let wado_fields: Vec<String> = ctx
        .wasi_registry
        .get_struct_fields_with_wado_names_by_source(source, &named.name)
        .expect("struct fields_with_wado_names present when fields are")
        .iter()
        .map(|(wn, _, _)| wn.clone())
        .collect();
    let mut tir_fields = Vec::with_capacity(resolved_fields.len());
    for (i, (_, field_ty)) in resolved_fields.iter().enumerate() {
        let field_addr = if offsets[i] == 0 {
            addr.clone()
        } else {
            binary_add(addr.clone(), i32_const(offsets[i] as i32))
        };
        let lifted_field = synthesize_lift_inner(
            field_ty,
            field_addr,
            next_local,
            stmts,
            local_types,
            Some(ctx),
        );
        let lifted_field = materialize_if_needed(lifted_field, next_local, stmts, local_types);
        let field_name = &wado_fields[i];
        tir_fields.push(crate::tir::TirStructField {
            name: field_name.clone(),
            value: lifted_field,
            field_index: i as u32,
        });
    }

    // Build the StructLiteral expression
    let struct_expr = TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: struct_type_id,
            struct_name: named.name.clone(),
            fields: tir_fields,
        },
        struct_type_id,
        synth_span(),
    );

    // Materialize the struct into a local
    let result_local = alloc_local(next_local, local_types, struct_type_id);
    stmts.push(let_stmt(
        "__struct_result",
        result_local,
        struct_type_id,
        struct_expr,
    ));

    Some(local_ref(result_local, "__struct_result", struct_type_id))
}

/// Lift a WASI variant type (e.g., `Method`) from linear memory.
///
/// CM variant layout:
/// - discriminant: 1 byte (u8) at offset 0 (for variants with ≤ 256 cases)
/// - payload: at `align_to(1, max_payload_align)` (only for payload cases)
///
/// Generates an if/else chain: disc==0 → Case0, disc==1 → Case1, ...
/// Payload cases lift the payload from the appropriate memory offset.
fn synthesize_lift_wasi_variant(
    _name: &str,
    variant_type: TypeId,
    cases: &[CmVariantCase],
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    // Load discriminant: 1 byte (u8) for variants with ≤ 256 cases
    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        "__vdisc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    ));

    // Result local (typed as the variant type)
    let result_local = alloc_local(next_local, local_types, variant_type);
    stmts.push(let_mut_stmt(
        "__vresult",
        result_local,
        variant_type,
        null_expr(variant_type),
    ));

    // Compute max payload alignment for payload offset calculation
    let wasi_package = ctx.map(|c| c.wasi_package);
    let max_payload_align = cases
        .iter()
        .filter_map(|case| case.payload.as_ref())
        .map(|ty| {
            if let Some(c) = ctx {
                crate::component_model::cm_align_with_registry_scoped(
                    ty,
                    c.wasi_registry,
                    wasi_package,
                )
            } else {
                cm_abi::cm_align(ty)
            }
        })
        .max()
        .unwrap_or(1);
    let payload_offset = cm_abi::align_to(1, max_payload_align); // after 1-byte disc

    // Build if/else chain for each case (last case is the else branch)
    let case_count = cases.len();
    let mut current_else: Option<TirBlock> = None;

    for (i, case) in cases.iter().enumerate().rev() {
        let case_name = case.wado_name.clone();
        let payload_type = case.payload.as_ref();

        // Lift payload if present
        let mut case_stmts: Vec<TirStmt> = Vec::new();
        let payload_box = if let Some(payload_ty) = payload_type {
            let payload_addr = binary_add(addr.clone(), i32_const(payload_offset as i32));
            let lifted = synthesize_lift_inner(
                payload_ty,
                payload_addr,
                next_local,
                &mut case_stmts,
                local_types,
                ctx,
            );
            Some(Box::new(lifted))
        } else {
            None
        };

        let construct = TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type,
                case_index: i as u32,
                case_name,
                payload: payload_box,
            },
            variant_type,
            synth_span(),
        );
        case_stmts.push(expr_stmt(assign(
            local_ref(result_local, "__vresult", variant_type),
            construct,
        )));

        if i == case_count - 1 {
            // Last case: becomes the else branch
            current_else = Some(block(case_stmts));
        } else {
            // Build if statement: if disc == i { ... } else { current_else }
            let cond = binary(
                crate::tir::TirBinaryOp::Eq,
                local_ref(disc_local, "__vdisc", TypeTable::I32),
                i32_const(i as i32),
                TypeTable::BOOL,
            );
            let if_stmt_node = if_stmt(cond, block(case_stmts), current_else);
            current_else = Some(block(vec![if_stmt_node]));
        }
    }

    if let Some(outer) = current_else {
        // Unwrap the block: push its statements into the parent
        for stmt in outer.stmts {
            stmts.push(stmt);
        }
    }

    local_ref(result_local, "__vresult", variant_type)
}

/// Lift a WASI enum type from an i32 discriminant.
/// Same pattern as variant but uses `EnumConstruct`.
fn synthesize_lift_wasi_enum(
    _name: &str,
    enum_type: TypeId,
    case_names: &[String],
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
) -> TirExpr {
    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    // CM spec: enum discriminant is u8 for ≤256 cases, u16 for ≤65536, u32 otherwise.
    let load_name = if case_names.len() <= 256 {
        "i32_load8_u"
    } else if case_names.len() <= 65536 {
        "i32_load16_u"
    } else {
        "i32_load"
    };
    stmts.push(let_stmt(
        "__edisc",
        disc_local,
        TypeTable::I32,
        builtin_call(load_name, vec![addr], TypeTable::I32),
    ));

    let result_local = alloc_local(next_local, local_types, enum_type);
    // Enums are represented as i32, so use 0 as the initial value (not null_expr
    // which emits ref.null for non-Option types).
    stmts.push(let_mut_stmt(
        "__eresult",
        result_local,
        enum_type,
        i32_const(0),
    ));

    let case_count = case_names.len();
    let mut current_else: Option<TirBlock> = None;

    for (i, cm_case_name) in case_names.iter().enumerate().rev() {
        let case_name = kebab_to_pascal(cm_case_name);
        let construct = TirExpr::new(
            TirExprKind::EnumConstruct {
                enum_type,
                case_index: i as u32,
                case_name,
            },
            enum_type,
            synth_span(),
        );
        let assign_stmt = expr_stmt(assign(
            local_ref(result_local, "__eresult", enum_type),
            construct,
        ));

        if i == case_count - 1 {
            current_else = Some(block(vec![assign_stmt]));
        } else {
            let cond = binary(
                crate::tir::TirBinaryOp::Eq,
                local_ref(disc_local, "__edisc", TypeTable::I32),
                i32_const(i as i32),
                TypeTable::BOOL,
            );
            let if_stmt_node = if_stmt(cond, block(vec![assign_stmt]), current_else);
            current_else = Some(block(vec![if_stmt_node]));
        }
    }

    if let Some(outer) = current_else {
        for stmt in outer.stmts {
            stmts.push(stmt);
        }
    }

    local_ref(result_local, "__eresult", enum_type)
}

/// Convert a kebab-case name to `PascalCase` (e.g., "invalid-syntax" → "`InvalidSyntax`").
fn kebab_to_pascal(s: &str) -> String {
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

/// Lift a `list<T>` from linear memory at `addr`.
///
/// Layout: `[base_ptr: i32, count: i32]` at addr.
/// Elements at `base_ptr + i * cm_size(T)`.
fn synthesize_lift_list(
    elem_ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    let elem_size = cm_abi::cm_size(elem_ty);

    // Resolve TypeIds for the element type and Array<ElemType>.
    // These are needed by the monomorphizer to instantiate Array::with_capacity and .push().
    let (elem_type_id, array_type_id) = if let Some(ctx) = ctx {
        let mut tt = ctx.type_table.borrow_mut();
        let elem_tid = wasi_type_to_type_id(elem_ty, &mut tt, ctx.wasi_registry, ctx.wasi_package);
        let array_tid = tt.make_array(elem_tid);
        (elem_tid, array_tid)
    } else {
        // Fallback: use placeholder types (existing behavior for callers without context)
        (TypeTable::I32, TypeTable::I32)
    };

    let base_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        "__base",
        base_local,
        TypeTable::I32,
        builtin_call("i32_load", vec![addr.clone()], TypeTable::I32),
    ));

    let count_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        "__count",
        count_local,
        TypeTable::I32,
        builtin_call(
            "i32_load",
            vec![binary_add(addr, i32_const(4))],
            TypeTable::I32,
        ),
    ));

    let result_local = alloc_local(next_local, local_types, array_type_id);
    stmts.push(let_mut_stmt(
        "__result",
        result_local,
        array_type_id,
        generic_static_call(
            "Array",
            "with_capacity",
            ModuleSource::prelude(),
            vec![elem_type_id],
            vec![local_ref(count_local, "__count", TypeTable::I32)],
            array_type_id,
        ),
    ));

    let i_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_mut_stmt("__i", i_local, TypeTable::I32, i32_const(0)));

    // Build loop body
    let mut loop_stmts: Vec<TirStmt> = Vec::new();

    // if __i >= __count { break; }
    loop_stmts.push(if_stmt(
        binary(
            crate::tir::TirBinaryOp::GtEq,
            local_ref(i_local, "__i", TypeTable::I32),
            local_ref(count_local, "__count", TypeTable::I32),
            TypeTable::BOOL,
        ),
        block(vec![break_stmt()]),
        None,
    ));

    // __elem_addr = __base + __i * elem_size
    let elem_addr_local = alloc_local(next_local, local_types, TypeTable::I32);
    loop_stmts.push(let_stmt(
        "__elem_addr",
        elem_addr_local,
        TypeTable::I32,
        binary_add(
            local_ref(base_local, "__base", TypeTable::I32),
            binary(
                crate::tir::TirBinaryOp::Mul,
                local_ref(i_local, "__i", TypeTable::I32),
                i32_const(elem_size as i32),
                TypeTable::I32,
            ),
        ),
    ));

    // Lift element (recursive: inner lists, options, etc. will also get proper types)
    let mut elem_lift_stmts: Vec<TirStmt> = Vec::new();
    let lifted_elem = synthesize_lift_inner(
        elem_ty,
        local_ref(elem_addr_local, "__elem_addr", TypeTable::I32),
        next_local,
        &mut elem_lift_stmts,
        local_types,
        ctx,
    );
    loop_stmts.extend(elem_lift_stmts);

    // __result.push(lifted_elem)
    loop_stmts.push(expr_stmt(generic_method_call(
        local_ref(result_local, "__result", array_type_id),
        "Array",
        "push",
        ModuleSource::prelude(),
        vec![lifted_elem],
        TypeTable::UNIT,
    )));

    // Free element's linear memory
    loop_stmts.extend(synthesize_free_element(
        elem_ty,
        local_ref(elem_addr_local, "__elem_addr", TypeTable::I32),
    ));

    // __i = __i + 1
    loop_stmts.push(expr_stmt(assign(
        local_ref(i_local, "__i", TypeTable::I32),
        binary_add(local_ref(i_local, "__i", TypeTable::I32), i32_const(1)),
    )));

    stmts.push(loop_stmt(block(loop_stmts)));

    // Free list buffer: realloc(__base, __count * elem_size, 4, 0)
    stmts.push(if_stmt(
        binary(
            crate::tir::TirBinaryOp::Gt,
            local_ref(count_local, "__count", TypeTable::I32),
            i32_const(0),
            TypeTable::BOOL,
        ),
        block(vec![expr_stmt(builtin_call(
            "realloc",
            vec![
                local_ref(base_local, "__base", TypeTable::I32),
                binary(
                    crate::tir::TirBinaryOp::Mul,
                    local_ref(count_local, "__count", TypeTable::I32),
                    i32_const(elem_size as i32),
                    TypeTable::I32,
                ),
                i32_const(4),
                i32_const(0),
            ],
            TypeTable::I32,
        ))]),
        None,
    ));

    local_ref(result_local, "__result", array_type_id)
}

/// Lift an `option<T>` from linear memory at `addr`.
///
/// Layout: discriminant byte at offset 0, payload at `align_to(1, align(T))`.
fn synthesize_lift_option_inner(
    inner_ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    let layout = if let Some(c) = ctx {
        cm_abi::layout_option_with_registry_scoped(inner_ty, c.wasi_registry, Some(c.wasi_package))
    } else {
        cm_abi::layout_option(inner_ty)
    };
    let payload_offset = layout.offsets[1];

    // Resolve the concrete Option<T> TypeId so the local and null/some exprs
    // use the correct GC reference type rather than an i32 placeholder.
    let option_type_id = if let Some(c) = ctx {
        let mut tt = c.type_table.borrow_mut();
        let inner_type_id =
            wasi_type_to_type_id(inner_ty, &mut tt, c.wasi_registry, c.wasi_package);
        tt.make_option(inner_type_id)
    } else {
        TypeTable::I32 // placeholder when no context
    };

    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        "__disc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    ));

    let result_local = alloc_local(next_local, local_types, option_type_id);
    stmts.push(let_mut_stmt(
        "__option_result",
        result_local,
        option_type_id,
        option_none(option_type_id),
    ));

    // if __disc != 0 { __option_result = Some(lift(inner, addr + offset)); }
    let mut then_stmts: Vec<TirStmt> = Vec::new();
    let payload_addr = binary_add(addr, i32_const(payload_offset as i32));
    let lifted = synthesize_lift_inner(
        inner_ty,
        payload_addr.clone(),
        next_local,
        &mut then_stmts,
        local_types,
        ctx,
    );
    then_stmts.push(expr_stmt(assign(
        local_ref(result_local, "__option_result", option_type_id),
        option_some(lifted, option_type_id),
    )));
    then_stmts.extend(synthesize_free_element(inner_ty, payload_addr));

    stmts.push(if_stmt(
        binary(
            crate::tir::TirBinaryOp::NotEq,
            local_ref(disc_local, "__disc", TypeTable::I32),
            i32_const(0),
            TypeTable::BOOL,
        ),
        block(then_stmts),
        None,
    ));

    local_ref(result_local, "__option_result", option_type_id)
}

/// Lift a `result<T, E>` from linear memory at `addr`.
///
/// Layout: discriminant i32 at offset 0, payload at aligned offset.
fn synthesize_lift_result_inner(
    ok_ty: &Type,
    err_ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    let layout = if let Some(c) = ctx {
        cm_abi::layout_result_with_registry_scoped(
            ok_ty,
            err_ty,
            c.wasi_registry,
            Some(c.wasi_package),
        )
    } else {
        cm_abi::layout_result(ok_ty, err_ty)
    };
    let payload_offset = layout.offsets[1];

    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    // Result is a variant with 2 cases; CM spec discriminant is u8 (1 byte).
    stmts.push(let_stmt(
        "__disc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    ));

    // Determine the proper variant TypeId for Result<ok_ty, err_ty> so that the
    // mutable local is typed as a GC reference (not i32). Using TypeTable::I32 as a
    // placeholder would cause a wasm validation error: the local would be declared as
    // i32 but initialized with `ref.null none` (a reference type).
    let result_type_id = if let Some(ctx) = ctx {
        let mut tt = ctx.type_table.borrow_mut();
        let ok_type_id = wasi_type_to_type_id(ok_ty, &mut tt, ctx.wasi_registry, ctx.wasi_package);
        let err_type_id =
            wasi_type_to_type_id(err_ty, &mut tt, ctx.wasi_registry, ctx.wasi_package);
        tt.make_result(ok_type_id, err_type_id)
    } else {
        TypeTable::I32 // placeholder when no context
    };

    let result_local = alloc_local(next_local, local_types, result_type_id);
    stmts.push(let_mut_stmt(
        "__result_val",
        result_local,
        result_type_id,
        null_expr(result_type_id),
    ));

    let payload_addr = binary_add(addr, i32_const(payload_offset as i32));

    // Ok case
    let mut ok_stmts: Vec<TirStmt> = Vec::new();
    let ok_is_unit = is_unit_type(ok_ty);
    let ok_payload = if ok_is_unit {
        None
    } else {
        let lifted = synthesize_lift_inner(
            ok_ty,
            payload_addr.clone(),
            next_local,
            &mut ok_stmts,
            local_types,
            ctx,
        );
        Some(Box::new(lifted))
    };
    ok_stmts.push(expr_stmt(assign(
        local_ref(result_local, "__result_val", result_type_id),
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: result_type_id,
                case_index: 0,
                case_name: "Ok".to_string(),
                payload: ok_payload,
            },
            result_type_id,
            synth_span(),
        ),
    )));

    // Err case
    let mut err_stmts: Vec<TirStmt> = Vec::new();
    let err_is_unit = is_unit_type(err_ty);
    let err_payload = if err_is_unit {
        None
    } else {
        let lifted = synthesize_lift_inner(
            err_ty,
            payload_addr,
            next_local,
            &mut err_stmts,
            local_types,
            ctx,
        );
        Some(Box::new(lifted))
    };
    err_stmts.push(expr_stmt(assign(
        local_ref(result_local, "__result_val", result_type_id),
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: result_type_id,
                case_index: 1,
                case_name: "Err".to_string(),
                payload: err_payload,
            },
            result_type_id,
            synth_span(),
        ),
    )));

    stmts.push(if_stmt(
        binary(
            crate::tir::TirBinaryOp::Eq,
            local_ref(disc_local, "__disc", TypeTable::I32),
            i32_const(0),
            TypeTable::BOOL,
        ),
        block(ok_stmts),
        Some(block(err_stmts)),
    ));

    local_ref(result_local, "__result_val", result_type_id)
}

/// Lift a tuple from linear memory at `addr`.
fn synthesize_lift_tuple(
    elems: &[Type],
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    let layout = cm_abi::layout_tuple(elems);
    let mut elem_exprs = Vec::new();
    for (i, elem_ty) in elems.iter().enumerate() {
        let elem_addr = binary_add(addr.clone(), i32_const(layout.offsets[i] as i32));
        let lifted = synthesize_lift_inner(elem_ty, elem_addr, next_local, stmts, local_types, ctx);
        // Materialize each element to ensure memory loads are evaluated before
        // the outptr buffer is freed (prevents use-after-free with debug allocator).
        let lifted = materialize_if_needed(lifted, next_local, stmts, local_types);
        elem_exprs.push(lifted);
    }
    // Resolve the tuple TypeId if we have a type table context.
    let tuple_type_id = if let Some(ctx) = ctx {
        let mut tt = ctx.type_table.borrow_mut();
        let elem_type_ids: Vec<TypeId> = elems
            .iter()
            .map(|t| wasi_type_to_type_id(t, &mut tt, ctx.wasi_registry, ctx.wasi_package))
            .collect();
        tt.make_tuple(elem_type_ids)
    } else {
        TypeTable::I32
    };
    TirExpr::new(
        TirExprKind::TupleLiteral {
            elements: elem_exprs,
        },
        tuple_type_id,
        synth_span(),
    )
}

/// Check if a type is a unit type.
fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(elems) if elems.is_empty())
        || matches!(ty, Type::Named(n) if n.name == "()")
}

/// Free a CM element's linear memory (within a list iteration).
///
/// For primitives: no-op.
/// For String: frees the string data buffer.
fn synthesize_free_element(ty: &Type, addr: TirExpr) -> Vec<TirStmt> {
    match ty {
        Type::Named(named) if named.name == "String" => {
            let ptr = builtin_call("i32_load", vec![addr.clone()], TypeTable::I32);
            let len = builtin_call(
                "i32_load",
                vec![binary_add(addr, i32_const(4))],
                TypeTable::I32,
            );
            vec![expr_stmt(builtin_call(
                "realloc",
                vec![ptr, len, i32_const(1), i32_const(0)],
                TypeTable::I32,
            ))]
        }
        Type::Tuple(elems) if !elems.is_empty() => {
            let layout = cm_abi::layout_tuple(elems);
            let mut free_stmts = Vec::new();
            for (i, elem_ty) in elems.iter().enumerate() {
                let elem_addr = binary_add(addr.clone(), i32_const(layout.offsets[i] as i32));
                free_stmts.extend(synthesize_free_element(elem_ty, elem_addr));
            }
            free_stmts
        }
        _ => vec![],
    }
}

/// Synthesize TIR statements that store a Wado value into linear memory.
///
/// For primitives, this is a single `builtin::i32_store` (or similar).
/// For String, calls `cm_lower_string` and stores ptr/len.
///
/// `next_local` is a counter for allocating intermediate local variables.
pub fn synthesize_lower(
    ty: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    local_types: &mut Vec<TypeId>,
) -> Vec<TirStmt> {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i64" | "u64" => vec![expr_stmt(builtin_call(
                "i64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f32" => vec![expr_stmt(builtin_call(
                "f32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f64" => vec![expr_stmt(builtin_call(
                "f64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i8" | "u8" => vec![expr_stmt(builtin_call(
                "i32_store8",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i16" | "u16" => vec![expr_stmt(builtin_call(
                "i32_store16",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "bool" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store8",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            "char" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            "String" => {
                // cm_lower_string(value) returns packed i64: (ptr | (len << 32))
                let packed_local = *next_local;
                local_types.push(TypeTable::I64);
                *next_local += 1;
                let packed = internal_call("cm_lower_string", vec![value], TypeTable::I64);
                let mut stmts = vec![let_stmt("__packed", packed_local, TypeTable::I64, packed)];

                // Store ptr (low 32 bits) at addr
                let ptr = cast(
                    local_ref(packed_local, "__packed", TypeTable::I64),
                    TypeTable::I32,
                );
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr.clone(), ptr],
                    TypeTable::UNIT,
                )));

                // Store len (high 32 bits) at addr + 4
                let shifted = binary(
                    crate::tir::TirBinaryOp::Shr,
                    local_ref(packed_local, "__packed", TypeTable::I64),
                    i64_const(32),
                    TypeTable::I64,
                );
                let len = cast(shifted, TypeTable::I32);
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![
                        binary(
                            crate::tir::TirBinaryOp::Add,
                            addr,
                            i32_const(4),
                            TypeTable::I32,
                        ),
                        len,
                    ],
                    TypeTable::UNIT,
                )));
                stmts
            }
            // Unknown named types: treat as i32 handles (enums, resources)
            _ => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
        },
        Type::Generic(g) => match g.name.as_str() {
            // list<T>: lowered as (ptr, len) pair stored at addr
            "Array"
                if g.args.len() == 1 && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                // list<u8>: use cm_lower_array_u8 → packed i64, store (ptr, len)
                let packed_local = *next_local;
                local_types.push(TypeTable::I64);
                *next_local += 1;
                let packed = internal_call("cm_lower_array_u8", vec![value], TypeTable::I64);
                let mut stmts = vec![let_stmt(
                    "__elem_packed",
                    packed_local,
                    TypeTable::I64,
                    packed,
                )];
                // Store ptr (low 32 bits) at addr
                let ptr = cast(
                    local_ref(packed_local, "__elem_packed", TypeTable::I64),
                    TypeTable::I32,
                );
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr.clone(), ptr],
                    TypeTable::UNIT,
                )));
                // Store len (high 32 bits) at addr + 4
                let shifted = binary(
                    crate::tir::TirBinaryOp::Shr,
                    local_ref(packed_local, "__elem_packed", TypeTable::I64),
                    i64_const(32),
                    TypeTable::I64,
                );
                let len = cast(shifted, TypeTable::I32);
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![
                        binary(
                            crate::tir::TirBinaryOp::Add,
                            addr,
                            i32_const(4),
                            TypeTable::I32,
                        ),
                        len,
                    ],
                    TypeTable::UNIT,
                )));
                stmts
            }
            "Array" => {
                // General list<T>: treat as opaque i32 for now
                vec![expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr, value],
                    TypeTable::UNIT,
                ))]
            }
            // Own<T>, Borrow<T>, Stream<T>, Future<T>: i32 handles
            _ => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
        },
        Type::Tuple(elems) if elems.is_empty() => vec![],
        Type::Tuple(_) => {
            // Tuple lowering requires type_table for correct TypeIds.
            // Callers with tuple elements should call synthesize_lower_tuple directly.
            vec![]
        }
        Type::Reference(_) | Type::MutReference(_) => vec![expr_stmt(builtin_call(
            "i32_store",
            vec![addr, value],
            TypeTable::UNIT,
        ))],
        _ => vec![],
    }
}

/// Lower a GC tuple to linear memory at `addr`.
///
/// Each tuple element is extracted via `FieldAccess` and recursively lowered
/// at the computed CM ABI offset.
fn synthesize_lower_tuple(
    elems: &[Type],
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    local_types: &mut Vec<TypeId>,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) -> Vec<TirStmt> {
    let layout = cm_abi::layout_tuple(elems);
    let mut stmts = Vec::new();

    // Materialize the tuple value into a local so we can access fields
    let tuple_local = *next_local;
    local_types.push(value.type_id);
    *next_local += 1;
    stmts.push(let_stmt("__tuple", tuple_local, value.type_id, value));

    for (i, elem_ty) in elems.iter().enumerate() {
        let offset = layout.offsets[i] as i32;
        let field_addr = if offset == 0 {
            addr.clone()
        } else {
            binary_add(addr.clone(), i32_const(offset))
        };

        // Determine the type_id for this field
        let field_type_id = {
            let mut tt = type_table.borrow_mut();
            wasi_type_to_type_id(elem_ty, &mut tt, wasi_registry, wasi_package)
        };

        // Extract the i-th field from the tuple using FieldAccess
        let field_expr = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(local_ref(
                    tuple_local,
                    "__tuple",
                    local_types[tuple_local as usize],
                )),
                field_index: i as u32,
                field_name: format!("{i}"),
            },
            field_type_id,
            synth_span(),
        );

        // Recursively lower: for tuples use synthesize_lower_tuple, otherwise synthesize_lower
        let field_stmts = if let Type::Tuple(sub_elems) = elem_ty {
            synthesize_lower_tuple(
                sub_elems,
                field_expr,
                field_addr,
                next_local,
                local_types,
                wasi_registry,
                wasi_package,
                type_table,
            )
        } else {
            synthesize_lower(elem_ty, field_expr, field_addr, next_local, local_types)
        };
        stmts.extend(field_stmts);
    }

    stmts
}

/// Check if a type is a "GC passthrough" parameter — one that the binding
/// accepts as a GC reference and lowers internally, rather than requiring
/// the call site to flatten into CM ABI i32 args. Variant lookup is scoped to
/// the `wasi:` namespace; same-named variants in other namespaces (e.g.
/// `core:kiln/types::Error`) do not affect WASI binding synthesis.
fn is_gc_passthrough_param(ty: &Type, wasi_registry: &WasiRegistry) -> bool {
    match ty {
        Type::Named(n) if n.name == "String" => true,
        Type::Named(n) => n.source_interface.as_deref().is_some_and(|s| {
            s.starts_with("wasi:")
                && wasi_registry
                    .get_variant_cases_by_source(s, &n.name)
                    .is_some()
        }),
        Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => true,
        Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => true,
        _ => false,
    }
}

fn is_wasm_flat_type(type_id: TypeId) -> bool {
    matches!(
        type_id,
        TypeTable::I32 | TypeTable::I64 | TypeTable::F32 | TypeTable::F64
    )
}

/// Compute the flat ABI parameter types for a WASI function parameter.
pub fn flatten_param_type(
    ty: &Type,
    wasi_registry: &crate::component_model::WasiRegistry,
) -> Vec<TypeId> {
    fn cm_val_to_type_id(v: &cm_abi::CmValType) -> TypeId {
        match v {
            cm_abi::CmValType::I32 => TypeTable::I32,
            cm_abi::CmValType::I64 => TypeTable::I64,
            cm_abi::CmValType::F32 => TypeTable::F32,
            cm_abi::CmValType::F64 => TypeTable::F64,
        }
    }

    let resolved = wasi_registry.resolve_type(ty);
    match &resolved {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" | "bool" | "char" | "i8" | "u8" | "i16" | "u16" => {
                vec![TypeTable::I32]
            }
            "i64" | "u64" => vec![TypeTable::I64],
            "f32" => vec![TypeTable::F32],
            "f64" => vec![TypeTable::F64],
            "String" => vec![TypeTable::I32, TypeTable::I32],
            name => {
                // Without a resolved WASI source the reference is not a WASI
                // variant/struct — flatten to a single i32 handle.
                let Some(source) = named
                    .source_interface
                    .as_deref()
                    .filter(|s| s.starts_with("wasi:"))
                else {
                    return vec![TypeTable::I32];
                };
                // WASI variant: discriminant + join of all case payload flat types.
                if let Some(cases) = wasi_registry.get_variant_cases_by_source(source, name) {
                    let mut result = vec![TypeTable::I32]; // discriminant
                    let case_flats: Vec<Vec<TypeId>> = cases
                        .iter()
                        .map(|c| {
                            c.payload
                                .as_ref()
                                .map(|t| flatten_param_type(t, wasi_registry))
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
                if let Some(fields) =
                    wasi_registry.get_struct_fields_with_wado_names_by_source(source, name)
                {
                    return fields
                        .iter()
                        .flat_map(|(_, _, ft)| flatten_param_type(ft, wasi_registry))
                        .collect();
                }
                // Resource handles, enums, flags, etc.: single i32
                vec![TypeTable::I32]
            }
        },
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
fn cm_param_size(ty: &Type, wasi_registry: &crate::component_model::WasiRegistry) -> u32 {
    crate::component_model::cm_size_with_registry(ty, wasi_registry)
}

/// Compute the CM Canonical ABI alignment for a param type, resolving WASI types through the registry.
fn cm_param_align(ty: &Type, wasi_registry: &crate::component_model::WasiRegistry) -> u32 {
    crate::component_model::cm_align_with_registry(ty, wasi_registry)
}

/// Lower a WASI variant (GC struct) to a linear memory buffer at the given address.
///
/// Memory layout (Canonical ABI):
/// - discriminant byte at offset 0
/// - payload at `align_to(1, payload_align)`, lowered using `synthesize_lower`
///
/// For each variant case with a payload, generates:
///   if `variant_test(value`, `case_i`) { `store_payload(payload_addr`, `variant_payload(value`, i)) }
fn synthesize_lower_wasi_variant_to_memory(
    named: &crate::ast::NamedType,
    source: &str,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    wasi_registry: &crate::component_model::WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let name = named.name.as_str();
    let cases = if let Some(c) = wasi_registry.get_variant_cases_by_source(source, name) {
        c.to_vec()
    } else {
        // Fallback: store as i32
        stmts.push(expr_stmt(builtin_call(
            "i32_store8",
            vec![addr, variant_tag(value)],
            TypeTable::UNIT,
        )));
        return;
    };

    let value_type_id = value.type_id;

    // Materialize value into a local so we can reference it multiple times
    let value_local = alloc_local(next_local, local_types, value_type_id);
    stmts.push(let_stmt("__variant_val", value_local, value_type_id, value));

    // Store discriminant byte
    stmts.push(expr_stmt(builtin_call(
        "i32_store8",
        vec![
            addr.clone(),
            variant_tag(local_ref(value_local, "__variant_val", value_type_id)),
        ],
        TypeTable::UNIT,
    )));

    // Compute payload offset (aligned to max payload alignment)
    let mut max_payload_align = 1u32;
    for case in &cases {
        if let Some(payload_ty) = &case.payload {
            max_payload_align = max_payload_align.max(
                crate::component_model::cm_align_with_registry(payload_ty, wasi_registry),
            );
        }
    }
    let payload_offset = crate::cm_abi::align_to(1, max_payload_align);

    let payload_addr = if payload_offset == 0 {
        addr
    } else {
        binary_add(addr, i32_const(payload_offset as i32))
    };

    // For each case with a payload, generate conditional store
    for (case_idx, case) in cases.iter().enumerate() {
        if let Some(payload_ty) = &case.payload {
            let payload_type_id = {
                let mut tt = type_table.borrow_mut();
                wasi_type_to_type_id(payload_ty, &mut tt, wasi_registry, wasi_package)
            };

            let payload_expr = variant_payload(
                local_ref(value_local, "__variant_val", value_type_id),
                case_idx as u32,
                payload_type_id,
            );

            let case_name = kebab_to_pascal(&case.cm_name);
            let case_stmts = synthesize_lower_wasi_type_to_memory(
                payload_ty,
                payload_expr,
                payload_addr.clone(),
                next_local,
                local_types,
                wasi_registry,
                wasi_package,
                type_table,
            );

            stmts.push(if_stmt(
                variant_test(
                    local_ref(value_local, "__variant_val", value_type_id),
                    case_idx as u32,
                    &case_name,
                ),
                block(case_stmts),
                None,
            ));
        }
    }
}

/// Lower an `Option<T>` value to linear memory at the given address.
///
/// Memory layout (Canonical ABI for `option<T>`):
/// - discriminant byte at offset 0 (0=None, 1=Some)
/// - payload at `align_to(1, payload_align)`, lowered using `synthesize_lower_wasi_type_to_memory`
fn synthesize_lower_option_to_memory(
    inner_type: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    wasi_registry: &crate::component_model::WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let value_type_id = value.type_id;

    // Materialize value into a local so we can reference it multiple times
    let value_local = alloc_local(next_local, local_types, value_type_id);
    stmts.push(let_stmt("__opt_val", value_local, value_type_id, value));

    // Store discriminant byte: variant_test(Some) → 1 = Some, 0 = None.
    // Use variant_test (ref.test) rather than variant_tag (struct.get)
    // because variant_tag traps on null refs.
    stmts.push(expr_stmt(builtin_call(
        "i32_store8",
        vec![
            addr.clone(),
            variant_test(
                local_ref(value_local, "__opt_val", value_type_id),
                0,
                "Some",
            ),
        ],
        TypeTable::UNIT,
    )));

    // Compute payload offset (aligned to payload alignment)
    let payload_align = crate::component_model::cm_align_with_registry(inner_type, wasi_registry);
    let payload_offset = crate::cm_abi::align_to(1, payload_align);

    let payload_addr = if payload_offset == 0 {
        addr
    } else {
        binary_add(addr, i32_const(payload_offset as i32))
    };

    // If Some: lower payload to memory
    let inner_type_id = {
        let mut tt = type_table.borrow_mut();
        wasi_type_to_type_id(inner_type, &mut tt, wasi_registry, wasi_package)
    };
    let payload_expr = variant_payload(
        local_ref(value_local, "__opt_val", value_type_id),
        0,
        inner_type_id,
    );

    let case_stmts = synthesize_lower_wasi_type_to_memory(
        inner_type,
        payload_expr,
        payload_addr,
        next_local,
        local_types,
        wasi_registry,
        wasi_package,
        type_table,
    );

    stmts.push(if_stmt(
        variant_test(
            local_ref(value_local, "__opt_val", value_type_id),
            0,
            "Some",
        ),
        block(case_stmts),
        None,
    ));
}

/// Flatten a WASI type value (GC ref) to flat CM ABI args (i32/i64/f32/f64).
///
/// Used for sync function params where the binding receives a GC value
/// but needs to pass flat values to the WASI import.
/// Appends lowering statements to `stmts` and flat value expressions to `flat_args`.
fn synthesize_flatten_value_to_flat_args(
    ty: &Type,
    value: TirExpr,
    prefix: &str,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    flat_args: &mut Vec<TirExpr>,
    wasi_registry: &crate::component_model::WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let resolved = wasi_registry.resolve_type(ty);
    match &resolved {
        // String → cm_lower_string → packed i64 → (ptr, len)
        Type::Named(n) if n.name == "String" => {
            let packed_local = alloc_local(next_local, local_types, TypeTable::I64);
            stmts.push(let_stmt(
                &format!("{prefix}_packed"),
                packed_local,
                TypeTable::I64,
                internal_call("cm_lower_string", vec![value], TypeTable::I64),
            ));
            // ptr = packed as i32 (low 32 bits)
            flat_args.push(cast(
                local_ref(packed_local, &format!("{prefix}_packed"), TypeTable::I64),
                TypeTable::I32,
            ));
            // len = (packed >> 32) as i32 (high 32 bits)
            flat_args.push(cast(
                binary(
                    crate::tir::TirBinaryOp::Shr,
                    local_ref(packed_local, &format!("{prefix}_packed"), TypeTable::I64),
                    i64_const(32),
                    TypeTable::I64,
                ),
                TypeTable::I32,
            ));
        }
        // Enum → variant_tag (single i32)
        Type::Named(n)
            if n.source_interface.as_deref().is_some_and(|s| {
                s.starts_with("wasi:")
                    && wasi_registry
                        .get_enum_variants_by_source(s, &n.name)
                        .is_some()
            }) =>
        {
            flat_args.push(variant_tag(value));
        }
        // Variant → disc + join of all case payload flats
        Type::Named(n)
            if n.source_interface.as_deref().is_some_and(|s| {
                s.starts_with("wasi:")
                    && wasi_registry
                        .get_variant_cases_by_source(s, &n.name)
                        .is_some()
            }) =>
        {
            let source = n
                .source_interface
                .as_deref()
                .expect("wasi variant source_interface present");
            let vt = value.type_id;
            let val_local = alloc_local(next_local, local_types, vt);
            stmts.push(let_stmt(&format!("{prefix}_val"), val_local, vt, value));

            // Push discriminant
            flat_args.push(variant_tag(local_ref(
                val_local,
                &format!("{prefix}_val"),
                vt,
            )));

            // Compute max flat payload count across all cases (the "join")
            let cases = wasi_registry
                .get_variant_cases_by_source(source, &n.name)
                .unwrap_or(&[]);
            let max_flat_count: usize = cases
                .iter()
                .map(|c| {
                    c.payload
                        .as_ref()
                        .map(|t| flatten_param_type(t, wasi_registry).len())
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);

            if max_flat_count > 0 {
                // Allocate mutable locals for each payload flat slot, initialized to 0
                let mut payload_locals: Vec<(u32, TypeId)> = Vec::new();
                for i in 0..max_flat_count {
                    let local = alloc_local(next_local, local_types, TypeTable::I32);
                    stmts.push(let_mut_stmt(
                        &format!("{prefix}_p{i}"),
                        local,
                        TypeTable::I32,
                        i32_const(0),
                    ));
                    payload_locals.push((local, TypeTable::I32));
                }

                // For each case with a payload, generate conditional flattening
                for (case_idx, case) in cases.iter().enumerate() {
                    if let Some(payload_ty) = &case.payload {
                        let payload_type_id = {
                            let mut tt = type_table.borrow_mut();
                            wasi_type_to_type_id(payload_ty, &mut tt, wasi_registry, wasi_package)
                        };
                        let payload_expr = variant_payload(
                            local_ref(val_local, &format!("{prefix}_val"), vt),
                            case_idx as u32,
                            payload_type_id,
                        );

                        let mut case_stmts = Vec::new();
                        let mut case_flat = Vec::new();
                        synthesize_flatten_value_to_flat_args(
                            payload_ty,
                            payload_expr,
                            &format!("{prefix}_c{case_idx}"),
                            next_local,
                            &mut case_stmts,
                            local_types,
                            &mut case_flat,
                            wasi_registry,
                            wasi_package,
                            type_table,
                        );
                        // Assign case flat values to shared payload locals
                        for (i, flat_val) in case_flat.into_iter().enumerate() {
                            if i < payload_locals.len() {
                                let (pl, pt) = payload_locals[i];
                                case_stmts.push(expr_stmt(assign(
                                    local_ref(pl, &format!("{prefix}_p{i}"), pt),
                                    flat_val,
                                )));
                            }
                        }
                        stmts.push(if_stmt(
                            variant_test(
                                local_ref(val_local, &format!("{prefix}_val"), vt),
                                case_idx as u32,
                                &case.wado_name,
                            ),
                            block(case_stmts),
                            None,
                        ));
                    }
                }

                // Push all payload locals as flat args
                for (i, (pl, pt)) in payload_locals.iter().enumerate() {
                    flat_args.push(local_ref(*pl, &format!("{prefix}_p{i}"), *pt));
                }
            }
        }
        // Simple primitives / handles → pass through directly
        _ => {
            flat_args.push(value);
        }
    }
}

/// Flatten an `Option<T>` GC value to flat CM ABI args for sync function calls.
///
/// Produces: [discriminant (0=None, 1=Some), ...flatten(T)]
/// The discriminant and payload locals are mutable, initialized to 0,
/// and populated conditionally when the option is `Some`.
fn synthesize_flatten_option_to_flat_args(
    inner_type: &Type,
    value: TirExpr,
    prefix: &str,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    flat_args: &mut Vec<TirExpr>,
    wasi_registry: &crate::component_model::WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let vt = value.type_id;
    let val_local = alloc_local(next_local, local_types, vt);
    stmts.push(let_stmt(&format!("{prefix}_optval"), val_local, vt, value));

    // CM ABI option discriminant: 0 = None, 1 = Some.
    // Use variant_test (ref.test) instead of variant_tag (struct.get)
    // because variant_tag traps on null refs.
    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        &format!("{prefix}_disc"),
        disc_local,
        TypeTable::I32,
        variant_test(
            local_ref(val_local, &format!("{prefix}_optval"), vt),
            0,
            "Some",
        ),
    ));
    flat_args.push(local_ref(
        disc_local,
        &format!("{prefix}_disc"),
        TypeTable::I32,
    ));

    // Compute inner flat types
    let inner_flat_types = flatten_param_type(inner_type, wasi_registry);
    if inner_flat_types.is_empty() {
        return;
    }

    // Allocate mutable locals for inner flats, initialized to 0
    let mut inner_locals: Vec<(u32, TypeId)> = Vec::new();
    for (i, &ft) in inner_flat_types.iter().enumerate() {
        let local = alloc_local(next_local, local_types, ft);
        let zero = match ft {
            TypeTable::I64 => i64_const(0),
            _ => i32_const(0),
        };
        stmts.push(let_mut_stmt(&format!("{prefix}_inner{i}"), local, ft, zero));
        inner_locals.push((local, ft));
    }

    // If Some: extract payload and flatten it
    let inner_type_id = {
        let mut tt = type_table.borrow_mut();
        wasi_type_to_type_id(inner_type, &mut tt, wasi_registry, wasi_package)
    };
    let payload_expr = variant_payload(
        local_ref(val_local, &format!("{prefix}_optval"), vt),
        0, // case_index 0 = Some
        inner_type_id,
    );

    let mut some_stmts = Vec::new();
    let mut some_flat = Vec::new();
    synthesize_flatten_value_to_flat_args(
        inner_type,
        payload_expr,
        &format!("{prefix}_s"),
        next_local,
        &mut some_stmts,
        local_types,
        &mut some_flat,
        wasi_registry,
        wasi_package,
        type_table,
    );
    // Assign flattened values to the mutable locals
    for (i, flat_val) in some_flat.into_iter().enumerate() {
        if i < inner_locals.len() {
            let (il, it) = inner_locals[i];
            some_stmts.push(expr_stmt(assign(
                local_ref(il, &format!("{prefix}_inner{i}"), it),
                flat_val,
            )));
        }
    }
    stmts.push(if_stmt(
        variant_test(
            local_ref(val_local, &format!("{prefix}_optval"), vt),
            0,
            "Some",
        ),
        block(some_stmts),
        None,
    ));

    // Push inner locals as flat args
    for (i, (il, it)) in inner_locals.iter().enumerate() {
        flat_args.push(local_ref(*il, &format!("{prefix}_inner{i}"), *it));
    }
}

/// Lower a WASI type value to linear memory at the given address.
fn synthesize_lower_wasi_type_to_memory(
    ty: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    local_types: &mut Vec<TypeId>,
    wasi_registry: &crate::component_model::WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) -> Vec<TirStmt> {
    let resolved = wasi_registry.resolve_type(ty);
    match &resolved {
        Type::Named(n) => {
            // WASI struct: store each field at its offset, keyed on the exact
            // source interface the Named reference was resolved to.
            let source = n
                .source_interface
                .as_deref()
                .filter(|s| s.starts_with("wasi:"));
            if let Some(fields) = source
                .and_then(|s| wasi_registry.get_struct_fields_with_wado_names_by_source(s, &n.name))
            {
                let resolved_fields: Vec<(String, Type)> = fields
                    .iter()
                    .map(|(wn, _, ft)| (wn.clone(), wasi_registry.resolve_type(ft)))
                    .collect();
                let mut stmts = Vec::new();
                let mut offset = 0u32;
                let mut max_align = 1u32;
                let value_type_id = value.type_id;
                let val_local = alloc_local(next_local, local_types, value_type_id);
                stmts.push(let_stmt("__struct_val", val_local, value_type_id, value));

                for (field_idx, (wado_name, field_ty)) in resolved_fields.iter().enumerate() {
                    let fa =
                        crate::component_model::cm_align_with_registry(field_ty, wasi_registry);
                    let fs = crate::component_model::cm_size_with_registry(field_ty, wasi_registry);
                    offset = crate::cm_abi::align_to(offset, fa);
                    let field_type_id = {
                        let mut tt = type_table.borrow_mut();
                        wasi_type_to_type_id(field_ty, &mut tt, wasi_registry, wasi_package)
                    };
                    let field_expr = TirExpr {
                        kind: crate::tir::TirExprKind::FieldAccess {
                            expr: Box::new(local_ref(val_local, "__struct_val", value_type_id)),
                            field_index: field_idx as u32,
                            field_name: wado_name.clone(),
                        },
                        type_id: field_type_id,
                        span: synth_span(),
                    };
                    let field_addr = if offset == 0 {
                        addr.clone()
                    } else {
                        binary_add(addr.clone(), i32_const(offset as i32))
                    };
                    stmts.extend(synthesize_lower_wasi_type_to_memory(
                        field_ty,
                        field_expr,
                        field_addr,
                        next_local,
                        local_types,
                        wasi_registry,
                        wasi_package,
                        type_table,
                    ));
                    offset += fs;
                    max_align = max_align.max(fa);
                }
                return stmts;
            }
            // Fall through to synthesize_lower for primitives and simple types
            synthesize_lower(&resolved, value, addr, next_local, local_types)
        }
        _ => synthesize_lower(&resolved, value, addr, next_local, local_types),
    }
}

/// Produce a store plan for writing a param type to memory: list of (`sub_offset`, `store_instruction`).
/// Each entry consumes one flat arg from the `flat_args` vector.
fn cm_param_store_plan(
    ty: &Type,
    wasi_registry: &crate::component_model::WasiRegistry,
) -> Vec<(u32, &'static str)> {
    if let Type::Named(named) = ty {
        let source = named
            .source_interface
            .as_deref()
            .filter(|s| s.starts_with("wasi:"));
        // Check WASI flags types.
        if let Some(members) =
            source.and_then(|s| wasi_registry.get_flags_members_by_source(s, &named.name))
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
            source.and_then(|s| wasi_registry.get_enum_variants_by_source(s, &named.name))
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
            "String" => vec![(0, "i32_store"), (4, "i32_store")],
            // i32, u32, char, resource handles
            _ => vec![(0, "i32_store")],
        };
    }
    match ty {
        Type::Reference(_) | Type::MutReference(_) => vec![(0, "i32_store")],
        Type::Generic(g) => match g.name.as_str() {
            "Array" => vec![(0, "i32_store"), (4, "i32_store")],
            "Option" if g.args.len() == 1 => {
                // option<T>: disc (u8) at offset 0, payload at align_to(1, align(T))
                let inner_align =
                    crate::component_model::cm_align_with_registry(&g.args[0], wasi_registry);
                let payload_offset = crate::cm_abi::align_to(1, inner_align);
                let inner_store = cm_param_store_plan(&g.args[0], wasi_registry);
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

/// Build the binding function name for a WASI import.
pub fn binding_func_name(effect_name: &str, method_name: &str) -> String {
    format!("__cm_binding__{effect_name}_{method_name}")
}

/// Build the export binding function name for a world export.
pub fn export_binding_func_name(export_name: &str) -> String {
    format!("__cm_export__{export_name}")
}

/// Canonical ABI: maximum number of flat return values before outptr is used.
const MAX_FLAT_RESULTS: usize = 1;

/// Check whether a return type needs lifting from a flat i32 discriminant to a GC struct.
/// This is true for Result types where all payloads are empty (unit), so the raw call
/// returns just a discriminant on the stack without an outptr.
fn needs_flat_result_lifting(ty: &Type) -> bool {
    matches!(ty, Type::Generic(g) if g.name == "Result" && g.args.len() == 2)
}

/// Synthesize lifting of a flat Result discriminant into a GC variant struct.
///
/// For `Result<(), ()>`: disc==0 → Ok, disc==1 → Err (no payloads)
/// For `Result<(), ErrorCode>`: disc==0 → Ok, disc!=0 → `Err(lift_error)`
fn synthesize_lift_flat_result(
    ty: &Type,
    disc_expr: TirExpr,
    result_local: u32,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    if let Type::Generic(g) = ty
        && g.name == "Result"
        && g.args.len() == 2
    {
        let ok_ty = &g.args[0];
        let err_ty = &g.args[1];

        let ok_is_unit = matches!(ok_ty, Type::Named(n) if n.name == "()")
            || matches!(ok_ty, Type::Tuple(elems) if elems.is_empty());
        let err_is_unit = matches!(err_ty, Type::Named(n) if n.name == "()")
            || matches!(err_ty, Type::Tuple(elems) if elems.is_empty());

        let ok_construct = if ok_is_unit {
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: TypeTable::I32,
                    case_index: 0,
                    case_name: "Ok".to_string(),
                    payload: None,
                },
                TypeTable::I32,
                synth_span(),
            )
        } else {
            // Ok with payload — flat result should use outptr instead
            // This shouldn't happen, but handle gracefully
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: TypeTable::I32,
                    case_index: 0,
                    case_name: "Ok".to_string(),
                    payload: None,
                },
                TypeTable::I32,
                synth_span(),
            )
        };

        let err_construct = if err_is_unit {
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: TypeTable::I32,
                    case_index: 1,
                    case_name: "Err".to_string(),
                    payload: None,
                },
                TypeTable::I32,
                synth_span(),
            )
        } else {
            // Err with a flat payload — the remaining flat values encode the error.
            // Only lift when the error type is a named WASI variant/enum carrying
            // a resolved source_interface; otherwise fall back to a bare Err.
            let lifted_variant = if let Type::Named(n) = err_ty
                && let Some(source) = n.source_interface.as_deref()
                && source.starts_with("wasi:")
            {
                try_lift_wasi_variant_or_enum(
                    n,
                    source,
                    disc_expr.clone(),
                    next_local,
                    stmts,
                    local_types,
                    ctx,
                )
            } else {
                None
            };
            if let Some(lifted) = lifted_variant {
                TirExpr::new(
                    TirExprKind::VariantConstruct {
                        variant_type: TypeTable::I32,
                        case_index: 1,
                        case_name: "Err".to_string(),
                        payload: Some(Box::new(lifted)),
                    },
                    TypeTable::I32,
                    synth_span(),
                )
            } else {
                TirExpr::new(
                    TirExprKind::VariantConstruct {
                        variant_type: TypeTable::I32,
                        case_index: 1,
                        case_name: "Err".to_string(),
                        payload: None,
                    },
                    TypeTable::I32,
                    synth_span(),
                )
            }
        };

        stmts.push(if_stmt(
            binary(
                crate::tir::TirBinaryOp::Eq,
                disc_expr,
                i32_const(0),
                TypeTable::BOOL,
            ),
            block(vec![expr_stmt(assign(
                local_ref(result_local, "__result_val", TypeTable::I32),
                ok_construct,
            ))]),
            Some(block(vec![expr_stmt(assign(
                local_ref(result_local, "__result_val", TypeTable::I32),
                err_construct,
            ))])),
        ));

        return local_ref(result_local, "__result_val", TypeTable::I32);
    }

    // Fallback: just return the discriminant as-is
    disc_expr
}

/// Create a `TirFunction` with default metadata fields.
fn make_binding_function(
    name: String,
    params: Vec<TirParam>,
    return_type: TypeId,
    body: TirBlock,
    local_count: u32,
    local_types: Vec<TypeId>,
) -> Rc<RefCell<TirFunction>> {
    Rc::new(RefCell::new(TirFunction {
        module_source: ModuleSource::default(),
        name,
        is_pub: false,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params,
        return_type,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(body),
        span: synth_span(),
        local_count,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: true,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    }))
}

/// Map a WASI return type to the flat return `TypeId` for the binding.
/// Sync functions with outptr return void from the raw call itself.
fn wasi_return_type_id(
    func_info: &WasiFunctionInfo,
    wasi_registry: &crate::component_model::WasiRegistry,
) -> TypeId {
    // Truly async imports (e.g., Client::send) use canon lower async and
    // return a subtask handle. Non-async imports with stream/future params
    // use sync lower (handles passed as i32, results returned directly).
    let needs_async_lower = func_info.is_async;
    if needs_async_lower {
        // Async canon lower: raw call returns subtask handle (i32)
        TypeTable::I32
    } else {
        let needs_outptr = func_info.return_type.as_ref().is_some_and(|rt| {
            crate::cm_abi::cm_flat_types(rt).len() > MAX_FLAT_RESULTS
                || crate::component_model::wasi_named_type_return_needs_outptr(rt, wasi_registry)
        });
        if needs_outptr {
            // Outptr: raw call returns void; result is read from outptr
            TypeTable::UNIT
        } else if let Some(ty) = &func_info.return_type {
            // Flat return: use the core type
            match ty {
                Type::Named(n) => match n.name.as_str() {
                    "i32" | "u32" => TypeTable::I32,
                    "i64" | "u64" => TypeTable::I64,
                    "f32" => TypeTable::F32,
                    "f64" => TypeTable::F64,
                    "bool" => TypeTable::I32, // CM returns bool as i32
                    _ => TypeTable::I32,
                },
                _ => TypeTable::I32,
            }
        } else {
            TypeTable::UNIT
        }
    }
}

/// Synthesize a CM binding function for a WASI import.
///
/// The binding function:
/// 1. Accepts the same parameter types as the WASI function
/// 2. Lowers parameters to flat CM ABI (String → ptr/len, etc.)
/// 3. Calls the lowered WASI function via `CmRawCall`
/// 4. Lifts the result from flat CM ABI back to Wado types
/// 5. Returns the Wado-typed result
///
/// The binding's Wado-level return type matches the WASI function declaration.
/// All return types are lifted inline using `synthesize_lift` — no per-type
/// converter functions are needed.
fn synthesize_adapter(
    func_info: &WasiFunctionInfo,
    wasi_registry: &crate::component_model::WasiRegistry,
    type_table: &RefCell<TypeTable>,
    owner_module: &ModuleSource,
) -> Rc<RefCell<TirFunction>> {
    let name = binding_func_name(&func_info.effect_name, &func_info.method_name);
    let local_name = func_info.local_alias_name();

    // Derive outptr needs from return type using Canonical ABI layout.
    // Also check WASI variants with payload cases (e.g., Method with Other(String)):
    // cm_flat_types treats unknown named types as i32, missing their true flat count.
    //
    // For `async fn foo(...) -> AsyncCall<T>` imports, `func_info.return_type`
    // already stores the CM-ABI `T` (the registry strips the `AsyncCall<T>`
    // wrapper at registration time, see `WasiRegistry::register`). The
    // wrapping is re-applied below when emitting the Wado-visible adapter
    // return type.
    let cm_return_type: Option<crate::ast::Type> = func_info.return_type.clone();
    let needs_outptr = cm_return_type.as_ref().is_some_and(|rt| {
        crate::cm_abi::cm_flat_types(rt).len() > MAX_FLAT_RESULTS
            || crate::component_model::wasi_named_type_return_needs_outptr(rt, wasi_registry)
    });
    let pkg = Some(func_info.package.as_str());
    let outptr_alloc = if needs_outptr {
        cm_return_type.as_ref().map(|rt| {
            // WASI variants need their registry-computed size/align, not the generic cm_size
            if let crate::ast::Type::Named(named) = rt
                && let Some(sa) = crate::component_model::wasi_variant_cm_size_align_scoped(
                    named,
                    wasi_registry,
                    pkg,
                )
            {
                return sa;
            }
            // Use registry-aware size/align for WASI structs and other complex types
            (
                crate::component_model::cm_size_with_registry_scoped(rt, wasi_registry, pkg),
                crate::component_model::cm_align_with_registry_scoped(rt, wasi_registry, pkg),
            )
        })
    } else {
        None
    };

    let mut next_local: u32 = 0;
    let mut params = Vec::new();
    let mut local_types: Vec<TypeId> = Vec::new();
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut flat_args: Vec<TirExpr> = Vec::new();

    // ---- Pass 1: Allocate all parameter locals (contiguous) ----
    // Wasm requires params at indices [0..n-1], so allocate them first.
    //
    // For types that the binding lowers internally (String, Array<u8>), we create
    // a single placeholder param. The binding body will lower them to flat CM args.
    //
    // For other types (handles, Option<T>, etc.), we create flat params matching
    // the CM ABI directly. The call site must flatten args before passing them.
    //
    // Track (start_param_idx, param_count) per WASI param for Pass 2 indexing.
    let mut param_mapping: Vec<(usize, usize)> = Vec::new();
    for (param_name, _, param_type) in &func_info.params {
        let flat_tys = flatten_param_type(param_type, wasi_registry);
        if flat_tys.is_empty() {
            continue; // unit param, skip
        }
        let start = params.len();
        match param_type {
            // String: single placeholder param (binding body lowers to ptr+len)
            Type::Named(n) if n.name == "String" => {
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                local_types.push(TypeTable::I32);
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Array<u8>: single placeholder param (binding body lowers to ptr+len)
            Type::Generic(g)
                if g.name == "Array"
                    && g.args.len() == 1
                    && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                local_types.push(TypeTable::I32);
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // General Array<T>: single placeholder param (binding body lowers to ptr+len)
            Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                local_types.push(TypeTable::I32);
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Struct (record) param: single GC reference, binding extracts fields
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_struct_fields_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                let struct_type_id = {
                    let mut tt = type_table.borrow_mut();
                    wasi_type_to_type_id(param_type, &mut tt, wasi_registry, &func_info.package)
                };
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: struct_type_id,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                local_types.push(struct_type_id);
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Variant param: single GC reference, binding lowers to flat args
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_variant_cases_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                let variant_type_id = {
                    let mut tt = type_table.borrow_mut();
                    wasi_type_to_type_id(param_type, &mut tt, wasi_registry, &func_info.package)
                };
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: variant_type_id,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                local_types.push(variant_type_id);
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Option<T>: single GC ref param (binding body lowers to discriminant + payload)
            Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
                let option_type_id = {
                    let mut tt = type_table.borrow_mut();
                    wasi_type_to_type_id(param_type, &mut tt, wasi_registry, &func_info.package)
                };
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: option_type_id,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                local_types.push(option_type_id);
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // All other types: create flat params matching CM ABI
            _ => {
                for (j, flat_ty) in flat_tys.iter().enumerate() {
                    let name = if flat_tys.len() == 1 {
                        param_name.clone()
                    } else {
                        format!("{param_name}_flat{j}")
                    };
                    params.push(TirParam {
                        name,
                        type_id: *flat_ty,
                        local_index: next_local,
                        is_mut: false,
                        span: synth_span(),
                        default_expr: None,
                    });
                    local_types.push(*flat_ty);
                    next_local += 1;
                }
                param_mapping.push((start, flat_tys.len()));
            }
        }
    }

    // ---- Pass 2: Generate parameter lowering code ----
    // Intermediate locals (packed i64, etc.) are allocated after all params.
    let mut mapping_idx = 0usize;
    for (param_name, _, param_type) in &func_info.params {
        let flat_tys = flatten_param_type(param_type, wasi_registry);
        if flat_tys.is_empty() {
            continue; // unit param, skip
        }
        let (start_idx, count) = param_mapping[mapping_idx];
        mapping_idx += 1;
        let param_local = params[start_idx].local_index;

        match param_type {
            // String param: accept Wado String, lower to (ptr, len) pair
            Type::Named(n) if n.name == "String" => {
                // Call cm_lower_string → packed i64
                let packed_local = next_local;
                let packed = internal_call(
                    "cm_lower_string",
                    vec![local_ref(param_local, param_name, TypeTable::I32)],
                    TypeTable::I64,
                );
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_packed"),
                    packed_local,
                    TypeTable::I64,
                    packed,
                ));
                local_types.push(TypeTable::I64);
                next_local += 1;

                // ptr = packed as i32 (low 32 bits)
                flat_args.push(cast(
                    local_ref(
                        packed_local,
                        &format!("__{param_name}_packed"),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
                // len = (packed >> 32) as i32 (high 32 bits)
                flat_args.push(cast(
                    binary(
                        crate::tir::TirBinaryOp::Shr,
                        local_ref(
                            packed_local,
                            &format!("__{param_name}_packed"),
                            TypeTable::I64,
                        ),
                        i64_const(32),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
            }

            // Array<u8> param: accept Wado Array<u8>, lower to (ptr, len) pair
            Type::Generic(g)
                if g.name == "Array"
                    && g.args.len() == 1
                    && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                // Call cm_lower_array_u8 → packed i64
                let packed_local = next_local;
                let packed = internal_call(
                    "cm_lower_array_u8",
                    vec![local_ref(param_local, param_name, TypeTable::I32)],
                    TypeTable::I64,
                );
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_packed"),
                    packed_local,
                    TypeTable::I64,
                    packed,
                ));
                local_types.push(TypeTable::I64);
                next_local += 1;

                // Split packed i64 → (ptr, len)
                flat_args.push(cast(
                    local_ref(
                        packed_local,
                        &format!("__{param_name}_packed"),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
                flat_args.push(cast(
                    binary(
                        crate::tir::TirBinaryOp::Shr,
                        local_ref(
                            packed_local,
                            &format!("__{param_name}_packed"),
                            TypeTable::I64,
                        ),
                        i64_const(32),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
            }

            // General Array<T> param: lower to (ptr, len) in linear memory
            Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
                let elem_type = &g.args[0];
                let elem_size = cm_abi::cm_size(elem_type) as i32;
                let elem_align = cm_abi::cm_align(elem_type) as i32;

                // Resolve proper TypeIds for the element and array types
                let (elem_type_id, array_type_id) = {
                    let mut tt = type_table.borrow_mut();
                    let elem_tid =
                        wasi_type_to_type_id(elem_type, &mut tt, wasi_registry, &func_info.package);
                    let array_tid = tt.make_array(elem_tid);
                    (elem_tid, array_tid)
                };

                // __len = Array<T>::len(param)
                let len_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_len"),
                    len_local,
                    TypeTable::I32,
                    generic_method_call(
                        local_ref(param_local, param_name, array_type_id),
                        "Array",
                        "len",
                        ModuleSource::prelude(),
                        vec![],
                        TypeTable::I32,
                    ),
                ));

                // __base = realloc(0, 0, align, __len * elem_size)
                let base_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_base"),
                    base_local,
                    TypeTable::I32,
                    builtin_call(
                        "realloc",
                        vec![
                            i32_const(0),
                            i32_const(0),
                            i32_const(elem_align),
                            binary(
                                crate::tir::TirBinaryOp::Mul,
                                local_ref(
                                    len_local,
                                    &format!("__{param_name}_len"),
                                    TypeTable::I32,
                                ),
                                i32_const(elem_size),
                                TypeTable::I32,
                            ),
                        ],
                        TypeTable::I32,
                    ),
                ));

                // __i = 0; loop { if __i >= __len { break; } lower elem[__i]; __i += 1; }
                let i_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
                body_stmts.push(let_mut_stmt(
                    &format!("__{param_name}_i"),
                    i_local,
                    TypeTable::I32,
                    i32_const(0),
                ));

                let mut loop_body = Vec::new();
                // break if __i >= __len
                loop_body.push(if_stmt(
                    binary(
                        crate::tir::TirBinaryOp::GtEq,
                        local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                        local_ref(len_local, &format!("__{param_name}_len"), TypeTable::I32),
                        TypeTable::BOOL,
                    ),
                    block(vec![break_stmt()]),
                    None,
                ));
                // __addr = __base + __i * elem_size
                let addr_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
                loop_body.push(let_stmt(
                    &format!("__{param_name}_addr"),
                    addr_local,
                    TypeTable::I32,
                    binary(
                        crate::tir::TirBinaryOp::Add,
                        local_ref(base_local, &format!("__{param_name}_base"), TypeTable::I32),
                        binary(
                            crate::tir::TirBinaryOp::Mul,
                            local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                            i32_const(elem_size),
                            TypeTable::I32,
                        ),
                        TypeTable::I32,
                    ),
                ));
                // __elem = param[__i] (IndexValue trait method)
                let elem_local = alloc_local(&mut next_local, &mut local_types, elem_type_id);
                let iv_info = crate::name::LocalMethodName::new(
                    "Array".to_string(),
                    Some("IndexValue<i32>".to_string()),
                    "index_value".to_string(),
                );
                let iv_mangled = iv_info.to_mangled_name();
                loop_body.push(let_stmt(
                    &format!("__{param_name}_elem"),
                    elem_local,
                    elem_type_id,
                    TirExpr::new(
                        TirExprKind::method_call(
                            Box::new(local_ref(param_local, param_name, array_type_id)),
                            FunctionRef {
                                module_source: ModuleSource::array(),
                                name: iv_mangled,
                                monomorph_info: None,
                                method_info: Some(iv_info),
                            },
                            vec![],
                            vec![CallArg::new(
                                local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                                false,
                            )],
                        ),
                        elem_type_id,
                        synth_span(),
                    ),
                ));
                // Lower element to linear memory at __addr
                let elem_ref = local_ref(elem_local, &format!("__{param_name}_elem"), elem_type_id);
                let addr_ref =
                    local_ref(addr_local, &format!("__{param_name}_addr"), TypeTable::I32);
                let lower_stmts = if let Type::Tuple(sub_elems) = elem_type {
                    synthesize_lower_tuple(
                        sub_elems,
                        elem_ref,
                        addr_ref,
                        &mut next_local,
                        &mut local_types,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    )
                } else {
                    synthesize_lower(
                        elem_type,
                        elem_ref,
                        addr_ref,
                        &mut next_local,
                        &mut local_types,
                    )
                };
                loop_body.extend(lower_stmts);
                // __i += 1
                loop_body.push(expr_stmt(assign(
                    local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                    binary(
                        crate::tir::TirBinaryOp::Add,
                        local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                        i32_const(1),
                        TypeTable::I32,
                    ),
                )));
                body_stmts.push(loop_stmt(block(loop_body)));

                // Push (base, len) as flat args
                flat_args.push(local_ref(
                    base_local,
                    &format!("__{param_name}_base"),
                    TypeTable::I32,
                ));
                flat_args.push(local_ref(
                    len_local,
                    &format!("__{param_name}_len"),
                    TypeTable::I32,
                ));
            }

            // Struct (record) param: extract fields as flat args
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_struct_fields_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                let struct_type_id = params[start_idx].type_id;
                let source = n
                    .source_interface
                    .as_deref()
                    .expect("wasi struct source_interface present");
                let wado_fields = wasi_registry
                    .get_struct_fields_with_wado_names_by_source(source, &n.name)
                    .expect("struct fields_with_wado_names present when fields are");
                for (field_idx, (wado_name, _, field_ty)) in wado_fields.iter().enumerate() {
                    let field_type_id = {
                        let mut tt = type_table.borrow_mut();
                        wasi_type_to_type_id(field_ty, &mut tt, wasi_registry, &func_info.package)
                    };
                    flat_args.push(TirExpr {
                        kind: crate::tir::TirExprKind::FieldAccess {
                            expr: Box::new(local_ref(param_local, param_name, struct_type_id)),
                            field_index: field_idx as u32,
                            field_name: wado_name.clone(),
                        },
                        type_id: field_type_id,
                        span: synth_span(),
                    });
                }
            }
            // Variant param: for async, pass GC ref (lowered in Step 3 indirect params);
            // for sync, flatten directly to flat i32 args.
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_variant_cases_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                if func_info.is_async {
                    let variant_type_id = params[start_idx].type_id;
                    flat_args.push(local_ref(param_local, param_name, variant_type_id));
                } else {
                    synthesize_flatten_value_to_flat_args(
                        param_type,
                        local_ref(param_local, param_name, params[start_idx].type_id),
                        &format!("__{param_name}"),
                        &mut next_local,
                        &mut body_stmts,
                        &mut local_types,
                        &mut flat_args,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                }
            }
            // Option<T>: for async, pass GC ref (lowered in Step 3 indirect params);
            // for sync, flatten directly to flat args.
            Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
                if func_info.is_async {
                    let option_type_id = params[start_idx].type_id;
                    flat_args.push(local_ref(param_local, param_name, option_type_id));
                } else {
                    synthesize_flatten_option_to_flat_args(
                        &g.args[0],
                        local_ref(param_local, param_name, params[start_idx].type_id),
                        &format!("__{param_name}"),
                        &mut next_local,
                        &mut body_stmts,
                        &mut local_types,
                        &mut flat_args,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                }
            }
            // All other types: flat params passed through directly
            _ => {
                for j in 0..count {
                    let p = &params[start_idx + j];
                    flat_args.push(local_ref(p.local_index, &p.name, p.type_id));
                }
            }
        }
    }

    // ---- Handle outptr for async or complex returns ----
    // Track async outptr allocation info for later freeing.
    let mut async_outptr_info: Option<(u32, u32, u32)> = None; // (local_index, size, align)
    // Only truly async imports use canon lower async (callback-style).
    // Non-async imports with stream/future params use sync lower.
    let needs_async_lower = func_info.is_async;
    if needs_async_lower {
        // Callback-style async (not used by Wado):
        // - MAX_FLAT_ASYNC_PARAMS = 4 flat params before switching to indirect.
        // - If flat_args exceeds 4, all params are passed via a single params_ptr
        //   (pointer to a linear-memory buffer with all lowered params).
        // - Per CM spec flatten_functype: the results_ptr is only added when
        //   len(flat_results) > 0 (i.e., when there IS a return type).
        // - Async void functions have no results_ptr.
        const MAX_FLAT_ASYNC_PARAMS: usize = 4;

        // The CM-level result type (for layout) is the inner `T` of
        // `AsyncCall<T>` for async imports; the `func_info.return_type`
        // itself is the Wado-visible `AsyncCall<T>` wrapper.
        let has_results = cm_return_type.is_some();

        // Allocate the async results buffer via realloc (only when there are results).
        if has_results {
            let pkg = Some(func_info.package.as_str());
            let (async_result_size, async_result_align) = if let Some(return_type) = &cm_return_type
            {
                if let crate::ast::Type::Named(named) = return_type
                    && let Some(sa) = crate::component_model::wasi_variant_cm_size_align_scoped(
                        named,
                        wasi_registry,
                        pkg,
                    )
                {
                    sa
                } else {
                    (
                        crate::component_model::cm_size_with_registry_scoped(
                            return_type,
                            wasi_registry,
                            pkg,
                        ),
                        crate::component_model::cm_align_with_registry_scoped(
                            return_type,
                            wasi_registry,
                            pkg,
                        ),
                    )
                }
            } else {
                unreachable!()
            };
            let async_outptr_local = next_local;
            body_stmts.push(let_stmt(
                "__async_outptr",
                async_outptr_local,
                TypeTable::I32,
                builtin_call(
                    "realloc",
                    vec![
                        i32_const(0),
                        i32_const(0),
                        i32_const(async_result_align as i32),
                        i32_const(async_result_size as i32),
                    ],
                    TypeTable::I32,
                ),
            ));
            local_types.push(TypeTable::I32);
            next_local += 1;
            async_outptr_info = Some((async_outptr_local, async_result_size, async_result_align));
        }

        // Force indirect path when variant or Option params are present
        // (they need memory lowering, not direct flat passing).
        let has_variant_params = func_info.params.iter().any(|(_, _, ty)| {
            matches!(ty, Type::Named(n) if n
                .source_interface
                .as_deref()
                .is_some_and(|s| s.starts_with("wasi:")
                    && wasi_registry
                        .get_variant_cases_by_source(s, &n.name)
                        .is_some()))
                || matches!(ty, Type::Generic(g) if g.name == "Option" && g.args.len() == 1)
        });

        if flat_args.len() > MAX_FLAT_ASYNC_PARAMS || has_variant_params {
            // Indirect calling: write all params to a memory buffer using CM layout.
            // The buffer layout follows the Component Model Canonical ABI spec,
            // which uses component-level type sizes (e.g., flags with ≤8 labels = 1 byte,
            // enums with ≤256 cases = 1 byte), NOT flat type sizes (all i32 = 4 bytes).

            // Step 1: Compute buffer layout using CM component-level param types.
            let mut buf_offset = 0u32;
            let mut buf_max_align = 1u32;
            let mut param_offsets: Vec<u32> = Vec::with_capacity(func_info.params.len());
            for (_, _, ty) in &func_info.params {
                let sz = cm_param_size(ty, wasi_registry);
                let al = cm_param_align(ty, wasi_registry);
                buf_offset = (buf_offset + al - 1) & !(al - 1);
                param_offsets.push(buf_offset);
                buf_offset += sz;
                buf_max_align = buf_max_align.max(al);
            }
            let buf_total_size = (buf_offset + buf_max_align - 1) & !(buf_max_align - 1);

            // Step 2: Allocate the params buffer.
            let params_buf_local = next_local;
            body_stmts.push(let_stmt(
                "__params_buf",
                params_buf_local,
                TypeTable::I32,
                builtin_call(
                    "realloc",
                    vec![
                        i32_const(0),
                        i32_const(0),
                        i32_const(buf_max_align as i32),
                        i32_const(buf_total_size as i32),
                    ],
                    TypeTable::I32,
                ),
            ));
            local_types.push(TypeTable::I32);
            next_local += 1;

            // Step 3: Write each param's values to the buffer at CM-computed offsets.
            let mut flat_idx = 0;
            for (param_idx, (_, _, ty)) in func_info.params.iter().enumerate() {
                let base_offset = param_offsets[param_idx];
                // WASI variants: lower directly to the buffer using registry-aware layout
                if let Type::Named(n) = ty
                    && let Some(source) = n.source_interface.as_deref()
                    && source.starts_with("wasi:")
                    && wasi_registry
                        .get_variant_cases_by_source(source, &n.name)
                        .is_some()
                {
                    let buf_addr = if base_offset == 0 {
                        local_ref(params_buf_local, "__params_buf", TypeTable::I32)
                    } else {
                        binary_add(
                            local_ref(params_buf_local, "__params_buf", TypeTable::I32),
                            i32_const(base_offset as i32),
                        )
                    };
                    // flat_args has one entry for this variant (the GC ref from Pass 2)
                    let variant_value = flat_args[flat_idx].clone();
                    flat_idx += 1;
                    synthesize_lower_wasi_variant_to_memory(
                        n,
                        source,
                        variant_value,
                        buf_addr,
                        &mut next_local,
                        &mut body_stmts,
                        &mut local_types,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                    continue;
                }
                // Option<T>: lower directly to the buffer
                if let Type::Generic(g) = ty
                    && g.name == "Option"
                    && g.args.len() == 1
                {
                    let buf_addr = if base_offset == 0 {
                        local_ref(params_buf_local, "__params_buf", TypeTable::I32)
                    } else {
                        binary_add(
                            local_ref(params_buf_local, "__params_buf", TypeTable::I32),
                            i32_const(base_offset as i32),
                        )
                    };
                    let option_value = flat_args[flat_idx].clone();
                    flat_idx += 1;
                    synthesize_lower_option_to_memory(
                        &g.args[0],
                        option_value,
                        buf_addr,
                        &mut next_local,
                        &mut body_stmts,
                        &mut local_types,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                    continue;
                }
                let stores = cm_param_store_plan(ty, wasi_registry);
                for (sub_offset, store_name) in &stores {
                    let offset = base_offset + sub_offset;
                    let addr = if offset == 0 {
                        local_ref(params_buf_local, "__params_buf", TypeTable::I32)
                    } else {
                        binary(
                            crate::tir::TirBinaryOp::Add,
                            local_ref(params_buf_local, "__params_buf", TypeTable::I32),
                            i32_const(offset as i32),
                            TypeTable::I32,
                        )
                    };
                    body_stmts.push(expr_stmt(builtin_call(
                        store_name,
                        vec![addr, flat_args[flat_idx].clone()],
                        TypeTable::UNIT,
                    )));
                    flat_idx += 1;
                }
            }

            // Replace flat_args with params_buf (+ async_outptr if results exist).
            flat_args = vec![local_ref(params_buf_local, "__params_buf", TypeTable::I32)];
            if let Some((outptr_local, _, _)) = async_outptr_info {
                flat_args.push(local_ref(outptr_local, "__async_outptr", TypeTable::I32));
            }
        } else {
            // Direct calling: params fit within MAX_FLAT_ASYNC_PARAMS.
            // Only add outptr if there are results.
            if let Some((outptr_local, _, _)) = async_outptr_info {
                flat_args.push(local_ref(outptr_local, "__async_outptr", TypeTable::I32));
            }
        }
    } else if let Some((size, align)) = outptr_alloc {
        // Allocate outptr via realloc
        let outptr_local = next_local;
        let outptr_alloc = builtin_call(
            "realloc",
            vec![
                i32_const(0),            // old_ptr
                i32_const(0),            // old_size
                i32_const(align as i32), // align
                i32_const(size as i32),  // new_size
            ],
            TypeTable::I32,
        );
        body_stmts.push(let_stmt(
            "__outptr",
            outptr_local,
            TypeTable::I32,
            outptr_alloc,
        ));
        local_types.push(TypeTable::I32);
        next_local += 1;

        flat_args.push(local_ref(outptr_local, "__outptr", TypeTable::I32));
    }

    // ---- Build CmRawCall ----
    let raw_call_return_type = wasi_return_type_id(func_info, wasi_registry);
    let raw_call_expr = cm_raw_call(&local_name, flat_args, raw_call_return_type);

    // ---- Handle result ----
    // The binding's return type to the Wado caller:
    let adapter_return_type;

    // Async/streaming path: functions lowered with `async` canon option.
    // This covers both truly async functions (func_info.is_async) and sync
    // functions with streaming params (Stream/Future) that require async lowering.
    // Non-async functions with streaming params complete synchronously (RETURNED
    // status), so wait_for_subtask is a no-op. The result is always written to the
    // outptr and lifted via synthesize_lift based on the return type metadata.
    if needs_async_lower {
        // WASI P3 async calling convention: the lowered function returns a
        // packed subtask handle/status `(subtask_handle << 4) | status`. The
        // result (if any) is written to the async outptr buffer when the
        // subtask eventually reaches `Status::Returned`.
        //
        // For Wado-level `async fn foo(...) -> AsyncCall<T>` imports, the
        // adapter does NOT wait for the subtask or lift the result here.
        // Instead it packages `(packed_handle, outptr, size, align)` into a
        // `AsyncCall<T>` struct and returns it immediately, letting the
        // caller interleave stream-parameter writes with the host subtask
        // before explicitly `.wait()`-ing. The wait + lift + free logic
        // lives in the synthesised `AsyncCall<T>::wait` method.
        let subtask_local = next_local;
        local_types.push(TypeTable::I32);
        next_local += 1;
        body_stmts.push(let_stmt(
            "__subtask_packed",
            subtask_local,
            TypeTable::I32,
            raw_call_expr,
        ));

        // Assemble the AsyncCall<T> struct fields.
        let (outptr_expr, size_expr, align_expr) =
            if let Some((outptr_local, outptr_size, outptr_align)) = async_outptr_info {
                (
                    local_ref(outptr_local, "__async_outptr", TypeTable::I32),
                    i32_const(outptr_size as i32),
                    i32_const(outptr_align as i32),
                )
            } else {
                // Void async import: no outptr. Carry zeroes so the struct
                // layout is uniform; `AsyncCall<()>::wait` is a no-op.
                (i32_const(0), i32_const(0), i32_const(0))
            };

        // Determine the type argument T for AsyncCall<T>. The CM-level
        // result type (inner T) was computed in `cm_return_type`; for
        // void async we use `()`.
        let inner_type_id = if let Some(return_type) = &cm_return_type {
            let resolved = wasi_registry.resolve_type(return_type);
            wasi_type_to_type_id(
                &resolved,
                &mut type_table.borrow_mut(),
                wasi_registry,
                &func_info.package,
            )
        } else {
            TypeTable::UNIT
        };
        let subtask_type = type_table.borrow_mut().make_async_call(inner_type_id);

        let subtask_struct = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: subtask_type,
                struct_name: "AsyncCall".to_string(),
                fields: vec![
                    crate::tir::TirStructField {
                        name: "__cm_packed".to_string(),
                        value: local_ref(subtask_local, "__subtask_packed", TypeTable::I32),
                        field_index: 0,
                    },
                    crate::tir::TirStructField {
                        name: "__cm_outptr".to_string(),
                        value: outptr_expr,
                        field_index: 1,
                    },
                    crate::tir::TirStructField {
                        name: "__cm_size".to_string(),
                        value: size_expr,
                        field_index: 2,
                    },
                    crate::tir::TirStructField {
                        name: "__cm_align".to_string(),
                        value: align_expr,
                        field_index: 3,
                    },
                ],
            },
            subtask_type,
            synth_span(),
        );
        body_stmts.push(return_stmt(Some(subtask_struct)));
        adapter_return_type = subtask_type;
    } else if let Some((alloc_size, alloc_align)) = outptr_alloc {
        body_stmts.push(expr_stmt(raw_call_expr));
        let outptr_local = next_local - 1;

        let return_type = func_info.return_type.as_ref().unwrap();
        let resolved = wasi_registry.resolve_type(return_type);

        // Inline lifting for all types, including list<T> which uses
        // Array::<T>::with_capacity() and .push() with proper monomorphization info.
        let lift_ctx = LiftContext {
            wasi_registry,
            type_table,
            wasi_package: &func_info.package,
        };
        let lifted = synthesize_lift_with_context(
            &resolved,
            local_ref(outptr_local, "__outptr", TypeTable::I32),
            &mut next_local,
            &mut body_stmts,
            &mut local_types,
            &lift_ctx,
        );

        // Materialize the lifted value into a local before freeing if it
        // contains a bare memory load (e.g., i32.load from the outptr buffer).
        // Complex types are already materialized into locals by synthesize_lift.
        let lifted =
            materialize_if_needed(lifted, &mut next_local, &mut body_stmts, &mut local_types);

        // Free the outptr
        body_stmts.push(expr_stmt(builtin_call(
            "realloc",
            vec![
                local_ref(outptr_local, "__outptr", TypeTable::I32),
                i32_const(alloc_size as i32),
                i32_const(alloc_align as i32),
                i32_const(0),
            ],
            TypeTable::I32,
        )));

        let lifted_type_id = lifted.type_id;
        body_stmts.push(return_stmt(Some(lifted)));
        adapter_return_type = lifted_type_id; // real type, fixed up at call site if needed
    } else if let Some(return_type) = &func_info.return_type {
        let resolved = wasi_registry.resolve_type(return_type);
        if needs_flat_result_lifting(&resolved) {
            // Flat return with complex type (e.g., Result<(), ()>): the raw call returns
            // an i32 discriminant on the stack, but the binding needs to return a GC struct.
            // Synthesize VariantConstruct from the discriminant.
            let disc_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
            body_stmts.push(let_stmt(
                "__disc",
                disc_local,
                TypeTable::I32,
                raw_call_expr,
            ));

            let result_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
            body_stmts.push(let_mut_stmt(
                "__result_val",
                result_local,
                TypeTable::I32,
                null_expr(TypeTable::I32),
            ));

            let lift_ctx = LiftContext {
                wasi_registry,
                type_table,
                wasi_package: &func_info.package,
            };
            let lifted = synthesize_lift_flat_result(
                &resolved,
                local_ref(disc_local, "__disc", TypeTable::I32),
                result_local,
                &mut next_local,
                &mut body_stmts,
                &mut local_types,
                &lift_ctx,
            );
            let lifted_type_id = lifted.type_id;
            body_stmts.push(return_stmt(Some(lifted)));
            adapter_return_type = lifted_type_id; // real type, fixed up at call site if needed
        } else {
            // Truly flat return (primitive): cm_raw_call directly returns the value
            body_stmts.push(return_stmt(Some(raw_call_expr)));
            adapter_return_type = raw_call_return_type;
        }
    } else {
        // No return: just call
        body_stmts.push(expr_stmt(raw_call_expr));
        adapter_return_type = TypeTable::UNIT;
    }

    let body = block(body_stmts);

    let binding = make_binding_function(
        name,
        params,
        adapter_return_type,
        body,
        next_local,
        local_types,
    );
    // Resources and effects are unified at the effect-system level: every
    // operation on `<E>` (whether `<E>` is declared as `effect` or `resource`)
    // requires the caller to hold `with <E>`. The binding for a CM-imported
    // operation therefore carries its owning name as its single concrete
    // effect. The propagation closure (built in `effect_check`) walks
    // operation signatures separately, so additional resources reachable
    // through `<E>`'s operations are admitted without listing them here.
    {
        let mut b = binding.borrow_mut();
        b.effects.push(EffectRef::Concrete {
            name: func_info.effect_name.clone(),
            module_source: owner_module.clone(),
        });
    }
    binding
}

/// Build a `(module_source, name)` set for every effect/resource declared in
/// the loaded TIR modules. The CM binding synthesizer uses this to attach the
/// owning effect to each generated binding using the same `module_source` the
/// resolver assigns to user-written `with E` clauses.
///
/// Keying by `(module_source, name)` (rather than name alone) prevents
/// collisions when two modules declare an effect or resource with the same
/// name — `lookup_effect_owner` selects the canonical WASI module.
fn effect_owner_module_sources(
    modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
) -> IndexMap<(ModuleSource, String), ()> {
    let mut out: IndexMap<(ModuleSource, String), ()> = IndexMap::default();
    for (module_source, module) in modules {
        for effect in &module.effects {
            out.insert((module_source.clone(), effect.name.clone()), ());
        }
        for resource in &module.resources {
            out.insert((module_source.clone(), resource.name.clone()), ());
        }
    }
    out
}

/// Look up the canonical owning module for an effect/resource named `name`
/// whose binding targets WASI `package` (e.g. `"cli"`).
///
/// Preferred match: a `ModuleSource::Wasi { interface }` whose interface starts
/// with `"{package}/"` (e.g. `wasi:cli/stdio.wado` for package `"cli"`).
/// Falls back to any other owner with the same name if no WASI match exists.
fn lookup_effect_owner(
    owners: &IndexMap<(ModuleSource, String), ()>,
    name: &str,
    package: &str,
) -> Option<ModuleSource> {
    let wasi_prefix = format!("{package}/");
    let mut fallback: Option<ModuleSource> = None;
    for ((ms, n), ()) in owners {
        if n != name {
            continue;
        }
        if let ModuleSource::Wasi { interface } = ms
            && interface.starts_with(&wasi_prefix)
        {
            return Some(ms.clone());
        }
        if fallback.is_none() {
            fallback = Some(ms.clone());
        }
    }
    fallback
}

/// Compute flat CM ABI types for an export return type, resolving variant and
/// struct definitions from the TIR modules. This is signature-driven: it works
/// for any return type shape, not just known names.
fn compute_export_flat_return_types(
    ty: &Type,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) -> Vec<cm_abi::CmValType> {
    let mut out = Vec::new();
    flatten_export_type(ty, &mut out, tir_modules, type_table);
    out
}

/// Recursively flatten an export type to CM ABI flat values.
fn flatten_export_type(
    ty: &Type,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "bool" | "u8" | "i8" | "u16" | "i16" | "i32" | "u32" | "char" => {
                out.push(cm_abi::CmValType::I32);
            }
            "i64" | "u64" => out.push(cm_abi::CmValType::I64),
            "f32" => out.push(cm_abi::CmValType::F32),
            "f64" => out.push(cm_abi::CmValType::F64),
            "String" => {
                out.push(cm_abi::CmValType::I32); // ptr
                out.push(cm_abi::CmValType::I32); // len
            }
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
        Type::Generic(generic) => match generic.name.as_str() {
            "Array" => {
                out.push(cm_abi::CmValType::I32); // ptr
                out.push(cm_abi::CmValType::I32); // len
            }
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
fn flatten_variant_type(
    variant_decl: &crate::tir::TirVariantDecl,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
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
fn flatten_struct_type(
    struct_decl: &crate::tir::TirStruct,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) {
    for field in &struct_decl.fields {
        flat_types_from_type_id_into(field.type_id, out, tir_modules, type_table);
    }
}

/// Compute flat CM ABI types from a `TypeId`, resolving through the type table.
fn flat_types_from_type_id(
    type_id: TypeId,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) -> Vec<cm_abi::CmValType> {
    let mut out = Vec::new();
    flat_types_from_type_id_into(type_id, &mut out, tir_modules, type_table);
    out
}

/// Append flat CM ABI types from a `TypeId` to `out`.
fn flat_types_from_type_id_into(
    type_id: TypeId,
    out: &mut Vec<cm_abi::CmValType>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) {
    use crate::tir::{PrimitiveType, ResolvedType};
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
            if name == "String" {
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
            } else {
                match name.as_str() {
                    "Option" if type_args.len() == 1 => {
                        out.push(cm_abi::CmValType::I32); // discriminant
                        flat_types_from_type_id_into(type_args[0], out, tir_modules, type_table);
                    }
                    "Result" if type_args.len() == 2 => {
                        out.push(cm_abi::CmValType::I32); // discriminant
                        let mut ok_flat = Vec::new();
                        let mut err_flat = Vec::new();
                        flat_types_from_type_id_into(
                            type_args[0],
                            &mut ok_flat,
                            tir_modules,
                            type_table,
                        );
                        flat_types_from_type_id_into(
                            type_args[1],
                            &mut err_flat,
                            tir_modules,
                            type_table,
                        );
                        let max_len = ok_flat.len().max(err_flat.len());
                        for i in 0..max_len {
                            let ok_val = ok_flat.get(i).copied();
                            let err_val = err_flat.get(i).copied();
                            out.push(cm_abi::CmValType::join(ok_val, err_val));
                        }
                    }
                    "Array" => {
                        out.push(cm_abi::CmValType::I32); // ptr
                        out.push(cm_abi::CmValType::I32); // len
                    }
                    _ => out.push(cm_abi::CmValType::I32),
                }
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
fn find_variant_decl(
    name: &str,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
) -> Option<crate::tir::TirVariantDecl> {
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
fn find_struct_decl(
    name: &str,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
) -> Option<crate::tir::TirStruct> {
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
fn variant_tag(expr: TirExpr) -> TirExpr {
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
fn variant_test(expr: TirExpr, case_index: u32, case_name: &str) -> TirExpr {
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
fn variant_payload(expr: TirExpr, case_index: u32, payload_type: TypeId) -> TirExpr {
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
fn field_access(expr: TirExpr, field_name: &str, field_index: u32, field_type: TypeId) -> TirExpr {
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

/// Synthesize TIR that lowers a Wado value to flat CM ABI values (on-stack).
///
/// Unlike `synthesize_lower` which stores to linear memory, this produces
/// TIR that yields individual flat values as locals. Used for export bindings
/// where results are passed to `task-return` as flat params.
///
/// Returns: list of local indices containing the flat values, and appends
/// statements to `stmts` for computing them.
fn synthesize_lower_to_flat(
    value: TirExpr,
    type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) -> Vec<FlatLocal> {
    let resolved = type_table.get(type_id);
    lower_to_flat_inner(
        value,
        type_id,
        resolved,
        next_local,
        stmts,
        local_types,
        tir_modules,
        type_table,
    )
}

/// A flat local: holds a lowered CM value with its CM type.
struct FlatLocal {
    index: u32,
    cm_type: cm_abi::CmValType,
}

/// Inner recursive implementation of `synthesize_lower_to_flat`.
fn lower_to_flat_inner(
    value: TirExpr,
    type_id: TypeId,
    resolved: &crate::tir::ResolvedType,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) -> Vec<FlatLocal> {
    use crate::tir::{PrimitiveType, ResolvedType};

    match resolved {
        ResolvedType::Primitive(p) => {
            let (flat_type_id, cm_type) = match p {
                PrimitiveType::I8
                | PrimitiveType::U8
                | PrimitiveType::I16
                | PrimitiveType::U16
                | PrimitiveType::I32
                | PrimitiveType::U32
                | PrimitiveType::Bool
                | PrimitiveType::Char => (TypeTable::I32, cm_abi::CmValType::I32),
                PrimitiveType::I64 | PrimitiveType::U64 => (TypeTable::I64, cm_abi::CmValType::I64),
                PrimitiveType::F32 => (TypeTable::F32, cm_abi::CmValType::F32),
                PrimitiveType::F64 => (TypeTable::F64, cm_abi::CmValType::F64),
                PrimitiveType::I128 | PrimitiveType::U128 => {
                    panic!("i128/u128 cannot appear at CM boundary")
                }
                PrimitiveType::V128 => {
                    panic!("v128 cannot appear at CM boundary")
                }
            };
            let cast_value = if flat_type_id == type_id {
                value
            } else {
                cast(value, flat_type_id)
            };
            let local = alloc_local(next_local, local_types, flat_type_id);
            stmts.push(let_stmt("__flat", local, flat_type_id, cast_value));
            vec![FlatLocal {
                index: local,
                cm_type,
            }]
        }
        ResolvedType::Resource { .. } | ResolvedType::Enum { .. } => {
            // Resource handles and enums are i32
            let local = alloc_local(next_local, local_types, TypeTable::I32);
            stmts.push(let_stmt("__flat", local, TypeTable::I32, value));
            vec![FlatLocal {
                index: local,
                cm_type: cm_abi::CmValType::I32,
            }]
        }
        ResolvedType::Struct { name, .. } if name == "String" => {
            // String → cm_lower_string → packed i64, split to ptr(i32) and len(i32)
            let packed = internal_call("cm_lower_string", vec![value], TypeTable::I64);
            let packed_local = alloc_local(next_local, local_types, TypeTable::I64);
            stmts.push(let_stmt("__packed", packed_local, TypeTable::I64, packed));

            // ptr = packed as i32
            let ptr = cast(
                local_ref(packed_local, "__packed", TypeTable::I64),
                TypeTable::I32,
            );
            let ptr_local = alloc_local(next_local, local_types, TypeTable::I32);
            stmts.push(let_stmt("__ptr", ptr_local, TypeTable::I32, ptr));

            // len = (packed >> 32) as i32
            let shifted = binary(
                crate::tir::TirBinaryOp::Shr,
                local_ref(packed_local, "__packed", TypeTable::I64),
                i64_const(32),
                TypeTable::I64,
            );
            let len = cast(shifted, TypeTable::I32);
            let len_local = alloc_local(next_local, local_types, TypeTable::I32);
            stmts.push(let_stmt("__len", len_local, TypeTable::I32, len));

            vec![
                FlatLocal {
                    index: ptr_local,
                    cm_type: cm_abi::CmValType::I32,
                },
                FlatLocal {
                    index: len_local,
                    cm_type: cm_abi::CmValType::I32,
                },
            ]
        }
        ResolvedType::Unit => vec![],
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == "Array" && type_args.len() == 1 => {
            // Array<T> flat ABI: (ptr: i32, len: i32).
            //
            // v1 correctness bracket: empty arrays round-trip cleanly;
            // non-empty arrays trap at runtime via
            // `builtin::unreachable()` so the CM boundary never silently
            // delivers `len == 0` in place of real data. The full
            // implementation — walk elements into linear memory per CM
            // canonical ABI, respecting per-element size and alignment —
            // is tracked as the lower-side adapter follow-up in
            // `docs/wep-2026-04-12-kiln.md` §"Planned follow-up". Until
            // it lands, a generator that returns a non-empty
            // `Response { files: [...] }` traps loudly instead of
            // returning a zero-pointed tombstone.
            let _ = type_args; // Element type reserved for the full impl.
            let arr_local = alloc_local(next_local, local_types, type_id);
            stmts.push(let_stmt("__arr_val", arr_local, type_id, value));

            // __len = Array::len(arr)  (generic receiver, monomorphized
            // by the monomorphizer into `Array<T>::len`).
            let len_local = alloc_local(next_local, local_types, TypeTable::I32);
            stmts.push(let_stmt(
                "__arr_len",
                len_local,
                TypeTable::I32,
                generic_method_call(
                    local_ref(arr_local, "__arr_val", type_id),
                    "Array",
                    "len",
                    ModuleSource::prelude(),
                    vec![],
                    TypeTable::I32,
                ),
            ));

            // if __arr_len != 0 { builtin::unreachable(); }
            stmts.push(if_stmt(
                binary_ne(
                    local_ref(len_local, "__arr_len", TypeTable::I32),
                    i32_const(0),
                ),
                block(vec![expr_stmt(builtin_call(
                    "unreachable",
                    vec![],
                    TypeTable::UNIT,
                ))]),
                None,
            ));

            // ptr is always 0 in the empty-array fast path. Reading
            // past ptr would be a pointer into unallocated memory, but
            // the trap above guarantees we never reach the consumer
            // with len > 0.
            let ptr_local = alloc_local(next_local, local_types, TypeTable::I32);
            stmts.push(let_stmt(
                "__arr_ptr",
                ptr_local,
                TypeTable::I32,
                i32_const(0),
            ));
            vec![
                FlatLocal {
                    index: ptr_local,
                    cm_type: cm_abi::CmValType::I32,
                },
                FlatLocal {
                    index: len_local,
                    cm_type: cm_abi::CmValType::I32,
                },
            ]
        }
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == "Option" && type_args.len() == 1 => {
            // Option<T> → disc(i32) + flat(T)
            let inner_type_id = type_args[0];
            let mut result = Vec::new();

            // Save value to a local for reuse
            let opt_local = alloc_local(next_local, local_types, type_id);
            stmts.push(let_stmt("__opt_val", opt_local, type_id, value));

            // Discriminant: VariantTest(Some) → 1 = Some, 0 = None
            let disc_expr = TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(variant_test(
                        local_ref(opt_local, "__opt_val", type_id),
                        0,
                        "Some",
                    )),
                    target_type: TypeTable::I32,
                },
                TypeTable::I32,
                synth_span(),
            );
            let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
            stmts.push(let_stmt(
                "__opt_disc",
                disc_local,
                TypeTable::I32,
                disc_expr,
            ));
            result.push(FlatLocal {
                index: disc_local,
                cm_type: cm_abi::CmValType::I32,
            });

            // If Some, lower the inner value
            let inner_flat_types = flat_types_from_type_id(inner_type_id, tir_modules, type_table);
            if !inner_flat_types.is_empty() {
                // Allocate locals for inner flat values (initialized to zero)
                let inner_locals: Vec<(u32, cm_abi::CmValType, String)> = inner_flat_types
                    .iter()
                    .enumerate()
                    .map(|(i, &vt)| {
                        let tid = cm_val_type_to_type_id(vt);
                        let l = alloc_local(next_local, local_types, tid);
                        let name = format!("__opt_inner_{i}");
                        stmts.push(let_mut_stmt(&name, l, tid, cm_zero(vt)));
                        (l, vt, name)
                    })
                    .collect();

                // if disc != 0 { lower(variant_payload(value)) → inner_locals }
                let mut then_stmts: Vec<TirStmt> = Vec::new();
                let unwrapped =
                    variant_payload(local_ref(opt_local, "__opt_val", type_id), 0, inner_type_id);
                let inner_lowered = synthesize_lower_to_flat(
                    unwrapped,
                    inner_type_id,
                    next_local,
                    &mut then_stmts,
                    local_types,
                    tir_modules,
                    type_table,
                );
                for (i, flat_val) in inner_lowered.iter().enumerate() {
                    if i < inner_locals.len() {
                        let (target_local, target_vt, ref target_name) = inner_locals[i];
                        let target_type = cm_val_type_to_type_id(target_vt);
                        let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                        let mut val = local_ref(flat_val.index, "__flat", source_type);
                        if flat_val.cm_type != target_vt {
                            val = cast(val, target_type);
                        }
                        then_stmts.push(expr_stmt(assign(
                            local_ref(target_local, target_name, target_type),
                            val,
                        )));
                    }
                }

                stmts.push(if_stmt(
                    binary(
                        crate::tir::TirBinaryOp::NotEq,
                        local_ref(disc_local, "__opt_disc", TypeTable::I32),
                        i32_const(0),
                        TypeTable::BOOL,
                    ),
                    block(then_stmts),
                    None,
                ));

                for (l, vt, _) in inner_locals {
                    result.push(FlatLocal {
                        index: l,
                        cm_type: vt,
                    });
                }
            }

            result
        }
        ResolvedType::Struct { name, .. } if name != "String" => {
            // Struct: concatenation of field flat types
            if let Some(struct_decl) = find_struct_decl(name, tir_modules) {
                let mut result = Vec::new();

                // Save value to a local
                let struct_local = alloc_local(next_local, local_types, type_id);
                stmts.push(let_stmt("__struct_val", struct_local, type_id, value));

                for field in &struct_decl.fields {
                    let field_value = field_access(
                        local_ref(struct_local, "__struct_val", type_id),
                        &field.name,
                        field.index,
                        field.type_id,
                    );
                    let field_lowered = synthesize_lower_to_flat(
                        field_value,
                        field.type_id,
                        next_local,
                        stmts,
                        local_types,
                        tir_modules,
                        type_table,
                    );
                    result.extend(field_lowered);
                }
                result
            } else {
                let local = alloc_local(next_local, local_types, TypeTable::I32);
                stmts.push(let_stmt("__flat", local, TypeTable::I32, value));
                vec![FlatLocal {
                    index: local,
                    cm_type: cm_abi::CmValType::I32,
                }]
            }
        }
        _ => {
            // For other types (including complex variants, newtypes, etc.), lower as i32
            let local = alloc_local(next_local, local_types, TypeTable::I32);
            stmts.push(let_stmt("__flat", local, TypeTable::I32, value));
            vec![FlatLocal {
                index: local,
                cm_type: cm_abi::CmValType::I32,
            }]
        }
    }
}

/// Convert `CmValType` to the TIR `TypeId` used for locals.
fn cm_val_type_to_type_id(vt: cm_abi::CmValType) -> TypeId {
    match vt {
        cm_abi::CmValType::I32 => TypeTable::I32,
        cm_abi::CmValType::I64 => TypeTable::I64,
        cm_abi::CmValType::F32 => TypeTable::F32,
        cm_abi::CmValType::F64 => TypeTable::F64,
    }
}

/// Create a zero constant for a given CM value type.
fn cm_zero(vt: cm_abi::CmValType) -> TirExpr {
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

/// Lift a single export parameter from flat CM params to a Wado-typed value.
///
/// Flat parameters are the Wasm function parameters corresponding to a single
/// CM parameter. For example, a `String` parameter becomes two flat params
/// `(ptr: i32, len: i32)` pointing to data in linear memory.
///
/// Returns the lifted TIR expression and the number of flat params consumed.
///
/// Known limitations:
/// - Struct parameters (non-String): treated as i32 passthrough (should
///   lift each field from consecutive flat params)
/// - Result<T, E> parameters: not handled (falls to unit default)
/// - Variant parameters: treated as i32 passthrough (should lift discriminant
///   + case-specific payloads)
/// - Array<T> for non-u8 elements: uses temp linear memory round-trip via
///   realloc, which assumes realloc is linked
/// Context threaded through the flat-parameter lift so the struct/list
/// paths can reach the full CM resolution stack (WASI + kiln registries,
/// the `RefCell<TypeTable>` needed to construct element `TypeId`s, and
/// the owning WASI package used to scope bare-name lookups).
///
/// Optional: callers without a context — notably unit tests that only
/// exercise primitives, strings, options, tuples — pass `None` and get
/// the passthrough behaviour that pre-dates struct lifting.
#[derive(Clone, Copy)]
struct FlatLiftContext<'a> {
    wasi_registry: &'a WasiRegistry,
    type_table_cell: &'a std::cell::RefCell<TypeTable>,
    wasi_package: &'a str,
}

impl<'a> FlatLiftContext<'a> {
    fn lift_context(&self) -> LiftContext<'a> {
        LiftContext {
            wasi_registry: self.wasi_registry,
            type_table: self.type_table_cell,
            wasi_package: self.wasi_package,
        }
    }
}

/// Reconstruct a minimal AST `Type` surface from a TIR `TypeId`.
///
/// Used by the struct/generic recursion inside
/// [`synthesize_lift_from_flat_params`] so a field's type — which TIR
/// stores as a `TypeId` — can be re-entered through the AST-shaped
/// match arms. The returned value only needs the top-level `name`
/// and (for `GenericInstance`) immediate type args; deeper structural
/// data is already reachable through `tir_modules` + `type_table` and
/// is looked up lazily.
fn type_id_to_ast_type(type_id: TypeId, type_table: &TypeTable) -> Type {
    use crate::tir::ResolvedType;
    let span = synth_span();
    let resolved = type_table.get(type_id);
    let named = |name: &str| Type::Named(NamedType::new(AstId::fresh(), name.to_string(), span));
    match resolved {
        ResolvedType::Primitive(p) => named(p.as_str()),
        ResolvedType::Unit => Type::Tuple(Vec::new()),
        ResolvedType::Struct { name, .. } => named(name),
        ResolvedType::Variant { name, .. } => named(name),
        ResolvedType::Enum { name, .. } => named(name),
        ResolvedType::Resource { name, .. } => named(name),
        ResolvedType::GenericInstance {
            name, type_args, ..
        } => {
            let args: Vec<Type> = type_args
                .iter()
                .map(|&tid| type_id_to_ast_type(tid, type_table))
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
                .map(|&tid| type_id_to_ast_type(tid, type_table))
                .collect();
            Type::Generic(GenericType {
                id: AstId::fresh(),
                name: name.clone(),
                args,
                span,
            })
        }
        ResolvedType::Ref(inner) => {
            Type::Reference(Box::new(type_id_to_ast_type(*inner, type_table)))
        }
        ResolvedType::MutRef(inner) => {
            Type::MutReference(Box::new(type_id_to_ast_type(*inner, type_table)))
        }
        _ => named("i32"),
    }
}

fn synthesize_lift_from_flat_params(
    ty: &Type,
    flat_param_locals: &[u32],
    flat_types: &[cm_abi::CmValType],
    target_type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table_cell: &std::cell::RefCell<TypeTable>,
    lift_ctx: Option<FlatLiftContext<'_>>,
) -> (TirExpr, usize) {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1),
            "i64" | "u64" => (local_ref(flat_param_locals[0], "__p", TypeTable::I64), 1),
            "f32" => (local_ref(flat_param_locals[0], "__p", TypeTable::F32), 1),
            "f64" => (local_ref(flat_param_locals[0], "__p", TypeTable::F64), 1),
            "i8" | "u8" | "i16" | "u16" => {
                (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1)
            }
            "bool" => {
                let raw = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let lifted = binary(
                    crate::tir::TirBinaryOp::NotEq,
                    raw,
                    i32_const(0),
                    TypeTable::BOOL,
                );
                (lifted, 1)
            }
            "char" => (local_ref(flat_param_locals[0], "__p", TypeTable::CHAR), 1),
            "String" => {
                // String flat ABI: (ptr: i32, len: i32) pointing to linear memory
                let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
                let lifted = internal_call("memory_to_gc_string", vec![ptr, len], target_type_id);
                (lifted, 2)
            }
            "()" => {
                let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
                (unit, 0)
            }
            _ => {
                // Struct parameter: flatten to concatenation of field flat
                // values per the canonical ABI. Iterate the TIR struct decl
                // and recursively lift each field, then construct a
                // `StructLiteral`. Resource handles, enums, flags, and
                // unknown types fall through to i32 passthrough.
                if let Some(struct_decl) = find_struct_decl(&named.name, tir_modules) {
                    let mut offset = 0;
                    let mut fields_out = Vec::with_capacity(struct_decl.fields.len());
                    // Precompute each field's AST surface while the
                    // `TypeTable` borrow is live; drop the borrow before
                    // recursion so the inner list lift may take
                    // `borrow_mut()` via the `FlatLiftContext`. Also
                    // resolve the struct's own type_id — `target_type_id`
                    // may arrive as a reference wrapper when the user
                    // function took the struct by value, so we consult
                    // the TIR struct decl's module source for the
                    // concrete `ResolvedType::Struct` id the
                    // `StructLiteral` WIR pass expects.
                    let (field_ast_tys, struct_type_id) = {
                        let tt = type_table_cell.borrow();
                        let field_tys: Vec<Type> = struct_decl
                            .fields
                            .iter()
                            .map(|f| type_id_to_ast_type(f.type_id, &tt))
                            .collect();
                        // Prefer the already-registered TypeId so the WIR
                        // `struct_type_map` lookup hits — the
                        // `find_struct_by_name` index is populated when
                        // the resolver first processed the struct decl,
                        // and `target_type_id` may arrive as a reference
                        // wrapper or an unregistered intern.
                        let stid = tt
                            .find_struct_by_name(&struct_decl.name, &struct_decl.module_source)
                            .unwrap_or(target_type_id);
                        (field_tys, stid)
                    };
                    for (field, field_ast_ty) in struct_decl.fields.iter().zip(field_ast_tys.iter())
                    {
                        let (lifted, consumed) = synthesize_lift_from_flat_params(
                            field_ast_ty,
                            &flat_param_locals[offset..],
                            &flat_types[offset..],
                            field.type_id,
                            next_local,
                            stmts,
                            local_types,
                            tir_modules,
                            type_table_cell,
                            lift_ctx,
                        );
                        fields_out.push(crate::tir::TirStructField {
                            name: field.name.clone(),
                            value: lifted,
                            field_index: field.index,
                        });
                        offset += consumed;
                    }
                    let struct_expr = TirExpr::new(
                        TirExprKind::StructLiteral {
                            struct_type: struct_type_id,
                            struct_name: named.name.clone(),
                            fields: fields_out,
                        },
                        struct_type_id,
                        synth_span(),
                    );
                    // Materialise into a local so it can be passed by
                    // value to the user function without re-evaluation.
                    let result_local = alloc_local(next_local, local_types, struct_type_id);
                    stmts.push(let_stmt(
                        "__struct_lift",
                        result_local,
                        struct_type_id,
                        struct_expr,
                    ));
                    return (
                        local_ref(result_local, "__struct_lift", struct_type_id),
                        offset,
                    );
                }
                // Resource handles, enums, unknown types → i32 passthrough
                (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1)
            }
        },
        Type::Generic(generic) => match generic.name.as_str() {
            "Array"
                if generic.args.len() == 1
                    && matches!(generic.args[0], Type::Named(ref n) if n.name == "u8") =>
            {
                // Array<u8> flat ABI: (ptr: i32, len: i32) pointing to linear memory
                let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
                let lifted = internal_call("memory_to_gc_array", vec![ptr, len], target_type_id);
                (lifted, 2)
            }
            "Array" => {
                // list<T> flat ABI: (ptr: i32, len: i32) — elements in linear memory
                // Write ptr/len to a temp memory block so we can reuse synthesize_lift
                let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
                // Allocate 8 bytes for ptr+len
                let tmp_ptr_local = alloc_local(next_local, local_types, TypeTable::I32);
                stmts.push(let_stmt(
                    "__lift_tmp",
                    tmp_ptr_local,
                    TypeTable::I32,
                    builtin_call(
                        "realloc",
                        vec![i32_const(0), i32_const(0), i32_const(4), i32_const(8)],
                        TypeTable::I32,
                    ),
                ));
                // Write ptr at offset 0
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32), ptr],
                    TypeTable::UNIT,
                )));
                // Write len at offset 4
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![
                        binary_add(
                            local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                            i32_const(4),
                        ),
                        len,
                    ],
                    TypeTable::UNIT,
                )));
                // Use synthesize_lift to lift from linear memory. When a
                // `FlatLiftContext` is available (real export binding calls),
                // route through `synthesize_lift_with_context` so the element
                // type and its registry (WASI or kiln) resolve correctly —
                // without it the list lift falls back to `Array<i32>` and
                // non-primitive element types blow up at monomorphization.
                let lifted = if let Some(ctx) = lift_ctx {
                    synthesize_lift_with_context(
                        ty,
                        local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                        next_local,
                        stmts,
                        local_types,
                        &ctx.lift_context(),
                    )
                } else {
                    synthesize_lift(
                        ty,
                        local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                        next_local,
                        stmts,
                        local_types,
                    )
                };
                // Free temp memory
                stmts.push(expr_stmt(builtin_call(
                    "realloc",
                    vec![
                        local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                        i32_const(8),
                        i32_const(4),
                        i32_const(0),
                    ],
                    TypeTable::I32,
                )));
                (lifted, 2)
            }
            "Option" if generic.args.len() == 1 => {
                // option<T> flat ABI: (disc: i32, ...T_flat)
                let (inner_flat, inner_type_id) = {
                    let tt = type_table_cell.borrow();
                    let mut out = Vec::new();
                    flatten_export_type(&generic.args[0], &mut out, tir_modules, &tt);
                    let inner_tid = tt.as_option(target_type_id).unwrap_or(target_type_id);
                    (out, inner_tid)
                };
                let total_flat = 1 + inner_flat.len();

                let disc = local_ref(flat_param_locals[0], "__p", TypeTable::I32);

                // if disc == 0 { None } else { Some(lift(inner_flat)) }
                let result_local = alloc_local(next_local, local_types, target_type_id);
                // Default: None
                stmts.push(let_mut_stmt(
                    "__opt_result",
                    result_local,
                    target_type_id,
                    option_none(target_type_id),
                ));

                // if disc != 0
                let mut then_stmts: Vec<TirStmt> = Vec::new();
                if inner_flat.is_empty() {
                    // Inner is unit — Some(())
                    let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
                    then_stmts.push(expr_stmt(assign(
                        local_ref(result_local, "__opt_result", target_type_id),
                        option_some(unit, target_type_id),
                    )));
                } else {
                    let (inner_lifted, _) = synthesize_lift_from_flat_params(
                        &generic.args[0],
                        &flat_param_locals[1..],
                        &flat_types[1..],
                        inner_type_id,
                        next_local,
                        &mut then_stmts,
                        local_types,
                        tir_modules,
                        type_table_cell,
                        lift_ctx,
                    );
                    then_stmts.push(expr_stmt(assign(
                        local_ref(result_local, "__opt_result", target_type_id),
                        option_some(inner_lifted, target_type_id),
                    )));
                }

                stmts.push(if_stmt(
                    binary_ne(disc, i32_const(0)),
                    block(then_stmts),
                    None,
                ));

                (
                    local_ref(result_local, "__opt_result", target_type_id),
                    total_flat,
                )
            }
            // Stream<T>, Future<T>, Own<T>, Borrow<T> — i32 handles
            _ => (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1),
        },
        Type::Tuple(elems) if elems.is_empty() => {
            let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
            (unit, 0)
        }
        Type::Tuple(elems) => {
            // Tuple: lift each element from consecutive flat params
            let mut total_consumed = 0;
            let mut elem_exprs = Vec::new();
            let elem_type_ids = {
                let tt = type_table_cell.borrow();
                tt.as_tuple(target_type_id)
                    .unwrap_or_else(|| vec![target_type_id; elems.len()])
            };

            for (i, elem_ty) in elems.iter().enumerate() {
                let elem_tid = elem_type_ids.get(i).copied().unwrap_or(TypeTable::I32);
                let (lifted, consumed) = synthesize_lift_from_flat_params(
                    elem_ty,
                    &flat_param_locals[total_consumed..],
                    &flat_types[total_consumed..],
                    elem_tid,
                    next_local,
                    stmts,
                    local_types,
                    tir_modules,
                    type_table_cell,
                    lift_ctx,
                );
                elem_exprs.push(lifted);
                total_consumed += consumed;
            }

            let tuple_expr = TirExpr::new(
                TirExprKind::TupleLiteral {
                    elements: elem_exprs,
                },
                target_type_id,
                synth_span(),
            );
            (tuple_expr, total_consumed)
        }
        Type::Reference(_) | Type::MutReference(_) => {
            (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1)
        }
        _ => (
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
            0,
        ),
    }
}

/// Compute flat CM param types for all parameters of a world export.
fn compute_export_flat_param_types(
    params: &[(String, Type)],
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) -> Vec<cm_abi::CmValType> {
    let mut out = Vec::new();
    for (_name, ty) in params {
        flatten_export_type(ty, &mut out, tir_modules, type_table);
    }
    out
}

/// Check if a world export parameter type needs lifting (is not a simple i32 passthrough).
fn param_needs_lifting(
    ty: &Type,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
) -> bool {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "String" | "bool" | "()" => true,
            "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "f32" | "f64"
            | "char" => false,
            _ => {
                // Structs (and variants) flatten to multiple CM values at the
                // boundary and therefore need a lift step. Resources / enums
                // / flags stay as a single i32 — treat them as passthrough.
                find_struct_decl(&named.name, tir_modules).is_some()
                    || find_variant_decl(&named.name, tir_modules).is_some()
            }
        },
        Type::Generic(generic) => matches!(generic.name.as_str(), "Array" | "Option" | "Result"),
        Type::Tuple(elems) => !elems.is_empty(),
        _ => false,
    }
}

/// Check if any parameter in a world export needs lifting.
fn export_needs_param_lifting(
    params: &[(String, Type)],
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
) -> bool {
    params
        .iter()
        .any(|(_, ty)| param_needs_lifting(ty, tir_modules))
}

/// Synthesize a CM export binding for an async export with a Result return type.
///
/// The binding:
/// 1. Lifts flat CM params to Wado-typed values (if needed)
/// 2. Calls the user's export function with lifted args
/// 3. Pattern-matches the Result<T, E> return value
/// 4. For Ok(T): lowers T to flat CM values, calls task-return
/// 5. For Err(E): lowers E to flat CM values, calls task-return
///
/// This is signature-driven: it examines the param/return types to generate
/// appropriate lifting/lowering code for any export signature.
fn synthesize_result_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    _return_type: &Type,
    flat_return_types: &[cm_abi::CmValType],
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut local_types: Vec<TypeId> = Vec::new();

    let user_func_ref = user_func.borrow();
    let user_return_type = user_func_ref.return_type;
    let needs_lifting = export_needs_param_lifting(world_params, tir_modules);

    // Build adapter params and call args
    let (adapter_params, call_args, param_count) = if needs_lifting {
        // Compute flat param types from world export signature
        let tt = type_table.borrow();
        let flat_param_types = compute_export_flat_param_types(world_params, tir_modules, &tt);
        drop(tt);

        // Create adapter params with flat types
        let flat_params: Vec<TirParam> = flat_param_types
            .iter()
            .enumerate()
            .map(|(i, &vt)| TirParam {
                name: format!("__p{i}"),
                type_id: cm_val_type_to_type_id(vt),
                local_index: i as u32,
                is_mut: false,
                default_expr: None,
                span: synth_span(),
            })
            .collect();

        let flat_count = flat_params.len() as u32;
        for p in &flat_params {
            local_types.push(p.type_id);
        }

        let mut next_local_tmp = flat_count;
        let flat_param_locals: Vec<u32> = (0..flat_count).collect();

        // Lift flat params to Wado-typed call args
        let mut lifted_args = Vec::new();
        let mut flat_offset = 0;
        let lift_ctx = FlatLiftContext {
            wasi_registry,
            type_table_cell: type_table,
            wasi_package,
        };
        for (i, (_name, param_ty)) in world_params.iter().enumerate() {
            let user_type_id = user_func_ref
                .params
                .get(i)
                .map(|p| p.type_id)
                .unwrap_or(TypeTable::I32);
            let (lifted, consumed) = synthesize_lift_from_flat_params(
                param_ty,
                &flat_param_locals[flat_offset..],
                &flat_param_types[flat_offset..],
                user_type_id,
                &mut next_local_tmp,
                &mut body_stmts,
                &mut local_types,
                tir_modules,
                type_table,
                Some(lift_ctx),
            );
            lifted_args.push(lifted);
            flat_offset += consumed;
        }

        (flat_params, lifted_args, next_local_tmp)
    } else {
        // No lifting needed — pass params through (resource handles, primitives)
        let params: Vec<TirParam> = user_func_ref
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| TirParam {
                name: p.name.clone(),
                type_id: p.type_id,
                local_index: i as u32,
                is_mut: false,
                span: synth_span(),
                default_expr: None,
            })
            .collect();

        let count = params.len() as u32;
        for p in &params {
            local_types.push(p.type_id);
        }

        let args: Vec<TirExpr> = params
            .iter()
            .map(|p| local_ref(p.local_index, &p.name, p.type_id))
            .collect();

        (params, args, count)
    };

    let mut next_local = param_count;

    // Call user function — derive param_is_mut from the actual function params.
    let call_user_param_is_mut: Vec<bool> =
        user_func.borrow().params.iter().map(|p| p.is_mut).collect();
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: call_args
                .into_iter()
                .zip(
                    call_user_param_is_mut
                        .into_iter()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
        },
        user_return_type,
        synth_span(),
    );

    // Store result in a local
    let result_local = alloc_local(&mut next_local, &mut local_types, user_return_type);
    body_stmts.push(let_stmt(
        "__result",
        result_local,
        user_return_type,
        call_user,
    ));

    // Determine Ok and Err type IDs from the Result type
    let tt = type_table.borrow();
    let (ok_type_id, err_type_id) = match tt.get(user_return_type) {
        crate::tir::ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
            (type_args[0], type_args[1])
        }
        _ => {
            // Not a Result — shouldn't happen for result export bindings
            panic!(
                "Expected Result type for export binding return, got: {:?}",
                tt.get(user_return_type)
            );
        }
    };
    drop(tt);

    // Allocate mutable flat value locals (initialized to zero)
    // These hold the flattened task-return args
    let flat_locals: Vec<(u32, String)> = flat_return_types
        .iter()
        .enumerate()
        .map(|(i, &vt)| {
            let type_id = cm_val_type_to_type_id(vt);
            let local = alloc_local(&mut next_local, &mut local_types, type_id);
            let name = format!("__tv_{i}");
            body_stmts.push(let_mut_stmt(&name, local, type_id, cm_zero(vt)));
            (local, name)
        })
        .collect();

    // === Ok case ===
    let mut ok_stmts: Vec<TirStmt> = Vec::new();

    // Set flat[0] = 0 (Ok discriminant)
    ok_stmts.push(expr_stmt(assign(
        local_ref(
            flat_locals[0].0,
            &flat_locals[0].1,
            cm_val_type_to_type_id(flat_return_types[0]),
        ),
        i32_const(0),
    )));

    // Extract Ok payload
    let ok_value = variant_payload(
        local_ref(result_local, "__result", user_return_type),
        0, // Ok case index
        ok_type_id,
    );

    // Lower Ok payload to flat values starting at flat[1]
    let tt = type_table.borrow();
    let ok_flat_types = flat_types_from_type_id(ok_type_id, tir_modules, &tt);
    drop(tt);

    if !ok_flat_types.is_empty() {
        // Store Ok payload in a local for reference
        let ok_local = alloc_local(&mut next_local, &mut local_types, ok_type_id);
        ok_stmts.push(let_stmt("__ok_val", ok_local, ok_type_id, ok_value));

        let tt = type_table.borrow();
        let ok_lowered = synthesize_lower_to_flat(
            local_ref(ok_local, "__ok_val", ok_type_id),
            ok_type_id,
            &mut next_local,
            &mut ok_stmts,
            &mut local_types,
            tir_modules,
            &tt,
        );
        drop(tt);

        // Assign lowered values to flat locals [1..1+ok_flat_count]
        for (i, flat_val) in ok_lowered.iter().enumerate() {
            if 1 + i < flat_locals.len() {
                let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                let mut val = local_ref(flat_val.index, "__flat", source_type);
                if flat_val.cm_type != flat_return_types[1 + i] {
                    val = cast(val, target_type);
                }
                ok_stmts.push(expr_stmt(assign(
                    local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                    val,
                )));
            }
        }
    }

    // Call task-return with flat values
    let task_return_args: Vec<TirExpr> = flat_locals
        .iter()
        .zip(flat_return_types.iter())
        .map(|((local, name), &vt)| local_ref(*local, name, cm_val_type_to_type_id(vt)))
        .collect();
    ok_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        task_return_args,
        TypeTable::UNIT,
    )));

    ok_stmts.push(return_stmt(None));

    // === Err case ===
    let mut err_stmts: Vec<TirStmt> = Vec::new();

    // Set flat[0] = 1 (Err discriminant)
    err_stmts.push(expr_stmt(assign(
        local_ref(
            flat_locals[0].0,
            &flat_locals[0].1,
            cm_val_type_to_type_id(flat_return_types[0]),
        ),
        i32_const(1),
    )));

    // Extract Err payload
    let err_value = variant_payload(
        local_ref(result_local, "__result", user_return_type),
        1, // Err case index
        err_type_id,
    );
    let err_local = alloc_local(&mut next_local, &mut local_types, err_type_id);
    err_stmts.push(let_stmt("__err_val", err_local, err_type_id, err_value));

    // Lower Err payload to flat values
    // For variant Err types (like ErrorCode), we need the discriminant and per-case payload
    let tt = type_table.borrow();
    let err_resolved = tt.get(err_type_id);

    // Check if Err type is a variant with payloads
    if let crate::tir::ResolvedType::Variant { name, .. } = &err_resolved {
        if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
            // Variant lowering: discriminant + per-case payload extraction
            synthesize_variant_lower_to_flat(
                err_local,
                err_type_id,
                &variant_decl,
                &flat_locals[1..],
                &flat_return_types[1..],
                &mut next_local,
                &mut err_stmts,
                &mut local_types,
                tir_modules,
                &tt,
            );
        } else {
            // Unknown variant — lower as i32
            if flat_locals.len() > 1 {
                err_stmts.push(expr_stmt(assign(
                    local_ref(
                        flat_locals[1].0,
                        &flat_locals[1].1,
                        cm_val_type_to_type_id(flat_return_types[1]),
                    ),
                    local_ref(err_local, "__err_val", err_type_id),
                )));
            }
        }
    } else {
        // Non-variant Err type — lower directly
        let err_lowered = synthesize_lower_to_flat(
            local_ref(err_local, "__err_val", err_type_id),
            err_type_id,
            &mut next_local,
            &mut err_stmts,
            &mut local_types,
            tir_modules,
            &tt,
        );
        for (i, flat_val) in err_lowered.iter().enumerate() {
            if 1 + i < flat_locals.len() {
                let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                let mut val = local_ref(flat_val.index, "__flat", source_type);
                if flat_val.cm_type != flat_return_types[1 + i] {
                    val = cast(val, target_type);
                }
                err_stmts.push(expr_stmt(assign(
                    local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                    val,
                )));
            }
        }
    }

    drop(tt);

    // Call task-return with flat values
    let task_return_args: Vec<TirExpr> = flat_locals
        .iter()
        .zip(flat_return_types.iter())
        .map(|((local, name), &vt)| local_ref(*local, name, cm_val_type_to_type_id(vt)))
        .collect();
    err_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        task_return_args,
        TypeTable::UNIT,
    )));

    err_stmts.push(return_stmt(None));

    // === Combine Ok/Err into if-else ===
    body_stmts.push(if_stmt(
        variant_test(
            local_ref(result_local, "__result", user_return_type),
            0,
            "Ok",
        ),
        block(ok_stmts),
        Some(block(err_stmts)),
    ));

    // Fallthrough (unreachable because both arms return, but emit task-return just in case)
    let fallthrough_args: Vec<TirExpr> = flat_return_types.iter().map(|&vt| cm_zero(vt)).collect();
    body_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        fallthrough_args,
        TypeTable::UNIT,
    )));

    let body = block(body_stmts);
    let local_count = next_local;

    let binding = make_binding_function(
        binding_name,
        adapter_params,
        TypeTable::UNIT,
        body,
        local_count,
        local_types,
    );
    {
        let mut b = binding.borrow_mut();
        b.is_export = true;
        b.is_cm_export = true;
    }
    binding
}

/// Synthesize TIR for lowering a variant value to flat CM locals.
///
/// Generates:
/// ```text
/// flat[0] = variant_tag(value)  // discriminant
/// if variant_test(value, case_0) { flat[1..] = lower(payload_0) }
/// if variant_test(value, case_1) { flat[1..] = lower(payload_1) }
/// ...
/// ```
fn synthesize_variant_lower_to_flat(
    value_local: u32,
    value_type_id: TypeId,
    variant_decl: &crate::tir::TirVariantDecl,
    flat_locals: &[(u32, String)], // flat locals for [disc, p2, p3, ...]
    flat_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &TypeTable,
) {
    // Set flat[0] = discriminant
    if !flat_locals.is_empty() {
        stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_types[0]),
            ),
            variant_tag(local_ref(value_local, "__err_val", value_type_id)),
        )));
    }

    // For each non-unit case, generate: if variant_test { extract payload, lower to flat }
    for case in &variant_decl.cases {
        let case_flat = flat_types_from_type_id(case.payload, tir_modules, type_table);
        if case_flat.is_empty() {
            continue; // Unit case — no payload to lower
        }

        let mut case_stmts: Vec<TirStmt> = Vec::new();

        // Extract payload
        let payload = variant_payload(
            local_ref(value_local, "__err_val", value_type_id),
            case.index,
            case.payload,
        );
        let payload_local = alloc_local(next_local, local_types, case.payload);
        case_stmts.push(let_stmt(
            "__case_payload",
            payload_local,
            case.payload,
            payload,
        ));

        // Lower payload to flat values
        let lowered = synthesize_lower_to_flat(
            local_ref(payload_local, "__case_payload", case.payload),
            case.payload,
            next_local,
            &mut case_stmts,
            local_types,
            tir_modules,
            type_table,
        );

        // Assign lowered values to flat locals [1..]
        for (i, flat_val) in lowered.iter().enumerate() {
            if 1 + i < flat_locals.len() {
                let target_type = cm_val_type_to_type_id(flat_types[1 + i]);
                let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                let mut val = local_ref(flat_val.index, "__flat", source_type);
                if flat_val.cm_type != flat_types[1 + i] {
                    val = cast(val, target_type);
                }
                case_stmts.push(expr_stmt(assign(
                    local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                    val,
                )));
            }
        }

        stmts.push(if_stmt(
            variant_test(
                local_ref(value_local, "__err_val", value_type_id),
                case.index,
                &case.name,
            ),
            block(case_stmts),
            None,
        ));
    }
}

/// Synthesize a CM export binding for a `() -> ()` async export.
///
/// The binding calls the user's export function and then calls `task-return(0)`
/// to signal successful completion. This replaces the task-return wrapping
/// that was previously done at the codegen level.
///
/// Generated TIR (for export name "run"):
/// ```text
/// fn __cm_export__run() {
///     run();
///     cm_raw_call task-return(0);
/// }
/// ```
fn synthesize_void_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();

    // Call the user's export function
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: vec![],
        },
        TypeTable::UNIT,
        synth_span(),
    );
    body_stmts.push(expr_stmt(call_user));

    // Call task-return(0) — Ok discriminant for result<_, _>
    body_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        vec![i32_const(0)],
        TypeTable::UNIT,
    )));

    let body = block(body_stmts);

    let binding = make_binding_function(binding_name, vec![], TypeTable::UNIT, body, 0, vec![]);
    // Mark as export so DCE keeps it as a root
    {
        let mut b = binding.borrow_mut();
        b.is_export = true;
        b.is_cm_export = true;
    }
    binding
}

/// Synthesize a CM export binding for a non-Result return type.
///
/// For exports where the user function returns a non-Result type (e.g., `-> i32`,
/// `-> String`, `-> ()`), the CM export still wraps the return in `result<T, error-context>`.
/// The binding calls `task-return(0, ...flat_values)` — Ok with the lowered return value.
///
/// This handles parameter lifting from flat CM params when needed.
///
/// Generated TIR (for `export fn add(a: i32, b: i32) -> i32`):
/// ```text
/// fn __cm_export__add(__p0: i32, __p1: i32) {
///     let __result = add(__p0, __p1);
///     let __flat0 = __result as i32;
///     cm_raw_call task-return(0, __flat0);
/// }
/// ```
///
/// Known limitation: flat return types are computed from the user's return type,
/// not from the world's `result<T, error-context>` wrapper. If `error-context`
/// has more flat slots than the Ok payload, the binding may emit too few
/// task-return args. In practice this is safe because:
/// - Current worlds use `Result<(), ()>` (no error-context) or `Result<T, E>`
///   (handled by `synthesize_result_export_binding`)
/// - This function is only used when the user doesn't return Result
fn synthesize_general_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut local_types: Vec<TypeId> = Vec::new();

    let user_func_ref = user_func.borrow();
    let user_return_type = user_func_ref.return_type;
    let needs_lifting = export_needs_param_lifting(world_params, tir_modules);

    // Build adapter params and call args
    let (adapter_params, call_args, param_count) = if needs_lifting {
        let tt = type_table.borrow();
        let flat_param_types = compute_export_flat_param_types(world_params, tir_modules, &tt);
        drop(tt);

        let flat_params: Vec<TirParam> = flat_param_types
            .iter()
            .enumerate()
            .map(|(i, &vt)| TirParam {
                name: format!("__p{i}"),
                type_id: cm_val_type_to_type_id(vt),
                local_index: i as u32,
                is_mut: false,
                default_expr: None,
                span: synth_span(),
            })
            .collect();

        let flat_count = flat_params.len() as u32;
        for p in &flat_params {
            local_types.push(p.type_id);
        }

        let mut next_local_tmp = flat_count;
        let flat_param_locals: Vec<u32> = (0..flat_count).collect();

        let mut lifted_args = Vec::new();
        let mut flat_offset = 0;
        let lift_ctx = FlatLiftContext {
            wasi_registry,
            type_table_cell: type_table,
            wasi_package,
        };
        for (i, (_name, param_ty)) in world_params.iter().enumerate() {
            let user_type_id = user_func_ref
                .params
                .get(i)
                .map(|p| p.type_id)
                .unwrap_or(TypeTable::I32);
            let (lifted, consumed) = synthesize_lift_from_flat_params(
                param_ty,
                &flat_param_locals[flat_offset..],
                &flat_param_types[flat_offset..],
                user_type_id,
                &mut next_local_tmp,
                &mut body_stmts,
                &mut local_types,
                tir_modules,
                type_table,
                Some(lift_ctx),
            );
            lifted_args.push(lifted);
            flat_offset += consumed;
        }

        (flat_params, lifted_args, next_local_tmp)
    } else {
        let params: Vec<TirParam> = user_func_ref
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| TirParam {
                name: p.name.clone(),
                type_id: p.type_id,
                local_index: i as u32,
                is_mut: false,
                span: synth_span(),
                default_expr: None,
            })
            .collect();

        let count = params.len() as u32;
        for p in &params {
            local_types.push(p.type_id);
        }

        let args: Vec<TirExpr> = params
            .iter()
            .map(|p| local_ref(p.local_index, &p.name, p.type_id))
            .collect();

        (params, args, count)
    };

    let mut next_local = param_count;

    // Call user function — derive param_is_mut from the actual function params.
    let call_user_param_is_mut: Vec<bool> =
        user_func.borrow().params.iter().map(|p| p.is_mut).collect();
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: call_args
                .into_iter()
                .zip(
                    call_user_param_is_mut
                        .into_iter()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
        },
        user_return_type,
        synth_span(),
    );

    // Check if return type is unit
    let tt = type_table.borrow();
    let is_unit_return = matches!(tt.get(user_return_type), crate::tir::ResolvedType::Unit);
    let return_flat_types = flat_types_from_type_id(user_return_type, tir_modules, &tt);
    drop(tt);

    if is_unit_return || return_flat_types.is_empty() {
        // Unit return — just call user function and task-return(0)
        body_stmts.push(expr_stmt(call_user));
        body_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            vec![i32_const(0)],
            TypeTable::UNIT,
        )));
    } else {
        // Non-unit return — lower return value and call task-return(0, ...flat_values)
        let result_local = alloc_local(&mut next_local, &mut local_types, user_return_type);
        body_stmts.push(let_stmt(
            "__result",
            result_local,
            user_return_type,
            call_user,
        ));

        let tt = type_table.borrow();
        let lowered = synthesize_lower_to_flat(
            local_ref(result_local, "__result", user_return_type),
            user_return_type,
            &mut next_local,
            &mut body_stmts,
            &mut local_types,
            tir_modules,
            &tt,
        );
        drop(tt);

        // Build task-return args: [0 (Ok disc), ...flat_return_values]
        let mut task_return_args = vec![i32_const(0)];
        for flat_val in &lowered {
            let val_type = cm_val_type_to_type_id(flat_val.cm_type);
            task_return_args.push(local_ref(flat_val.index, "__flat", val_type));
        }

        body_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            task_return_args,
            TypeTable::UNIT,
        )));
    }

    let body = block(body_stmts);
    let local_count = next_local;

    let binding = make_binding_function(
        binding_name,
        adapter_params,
        TypeTable::UNIT,
        body,
        local_count,
        local_types,
    );
    {
        let mut b = binding.borrow_mut();
        b.is_export = true;
        b.is_cm_export = true;
    }
    binding
}

/// Synthesize a stub export binding that just calls `task-return(0)`.
///
/// Used when the world declares an export but the user didn't define the function
/// (e.g., test-only files that have `test` blocks but no `export fn run()`).
fn synthesize_void_stub_adapter(export_name: &str) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);

    // Just call task-return(0) — Ok discriminant for result<_, _>
    let body = block(vec![expr_stmt(cm_raw_call(
        "task-return",
        vec![i32_const(0)],
        TypeTable::UNIT,
    ))]);

    let binding = make_binding_function(binding_name, vec![], TypeTable::UNIT, body, 0, vec![]);
    {
        let mut b = binding.borrow_mut();
        b.is_export = true;
        b.is_cm_export = true;
    }
    binding
}

/// Synthesize an export binding for `export async fn` functions.
///
/// The user function calls `task-return` internally via `task return expr` stmts
/// (which are expanded by `expand_task_returns_in_func`). The binding only needs to
/// lift flat CM params to Wado types and call the user function.
///
/// Generated TIR (for `export async fn handle(request: Request)`):
/// ```text
/// fn __cm_export__handle(__p0: i32, ...) {
///     let __request = lift_request(__p0, ...);
///     handle(__request);  // user fn calls task-return internally
/// }
/// ```
fn synthesize_async_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut local_types: Vec<TypeId> = Vec::new();

    let user_func_ref = user_func.borrow();
    let needs_lifting = export_needs_param_lifting(world_params, tir_modules);

    let (adapter_params, call_args) = if needs_lifting {
        let tt = type_table.borrow();
        let flat_param_types = compute_export_flat_param_types(world_params, tir_modules, &tt);
        drop(tt);

        let flat_params: Vec<TirParam> = flat_param_types
            .iter()
            .enumerate()
            .map(|(i, &vt)| TirParam {
                name: format!("__p{i}"),
                type_id: cm_val_type_to_type_id(vt),
                local_index: i as u32,
                is_mut: false,
                default_expr: None,
                span: synth_span(),
            })
            .collect();

        let flat_count = flat_params.len() as u32;
        for p in &flat_params {
            local_types.push(p.type_id);
        }

        let mut next_local_tmp = flat_count;
        let flat_param_locals: Vec<u32> = (0..flat_count).collect();

        let mut lifted_args = Vec::new();
        let mut flat_offset = 0;
        let lift_ctx = FlatLiftContext {
            wasi_registry,
            type_table_cell: type_table,
            wasi_package,
        };
        for (i, (_name, param_ty)) in world_params.iter().enumerate() {
            let user_type_id = user_func_ref
                .params
                .get(i)
                .map(|p| p.type_id)
                .unwrap_or(TypeTable::I32);
            let (lifted, consumed) = synthesize_lift_from_flat_params(
                param_ty,
                &flat_param_locals[flat_offset..],
                &flat_param_types[flat_offset..],
                user_type_id,
                &mut next_local_tmp,
                &mut body_stmts,
                &mut local_types,
                tir_modules,
                type_table,
                Some(lift_ctx),
            );
            lifted_args.push(lifted);
            flat_offset += consumed;
        }

        (flat_params, lifted_args)
    } else {
        let params: Vec<TirParam> = user_func_ref
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| TirParam {
                name: p.name.clone(),
                type_id: p.type_id,
                local_index: i as u32,
                is_mut: false,
                span: synth_span(),
                default_expr: None,
            })
            .collect();

        for p in &params {
            local_types.push(p.type_id);
        }

        let args: Vec<TirExpr> = params
            .iter()
            .map(|p| local_ref(p.local_index, &p.name, p.type_id))
            .collect();

        (params, args)
    };

    // Call user function (it returns unit; task-return is called internally)
    let call_user_param_is_mut: Vec<bool> =
        user_func.borrow().params.iter().map(|p| p.is_mut).collect();
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: call_args
                .into_iter()
                .zip(
                    call_user_param_is_mut
                        .into_iter()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
        },
        TypeTable::UNIT,
        synth_span(),
    );
    body_stmts.push(expr_stmt(call_user));
    // No task-return here: user function handles it via task return stmts.

    let body = block(body_stmts);
    let local_count = local_types.len() as u32;

    let binding = make_binding_function(
        binding_name,
        adapter_params,
        TypeTable::UNIT,
        body,
        local_count,
        local_types,
    );
    {
        let mut b = binding.borrow_mut();
        b.is_export = true;
        b.is_cm_export = true;
    }
    binding
}

/// Expand `TaskReturn` stmts in an `export async fn` user function into inline CM calls.
///
/// Walks the function body and replaces each `TirStmtKind::TaskReturn { value }` with
/// the flat lowering + `cm_raw_call("task-return", flat_args)` sequence.
/// New locals are appended to the function's `local_types` and `local_count` is updated.
fn expand_task_returns_in_func(
    user_func: &Rc<RefCell<TirFunction>>,
    flat_return_types: &[cm_abi::CmValType],
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    let mut func = user_func.borrow_mut();
    let mut next_local = func.local_count;
    let mut extra_local_types: Vec<TypeId> = Vec::new();
    // Take the body out to avoid simultaneous mutable/immutable borrows of func
    let Some(mut body) = func.body.take() else {
        return;
    };
    expand_task_return_in_block(
        &mut body,
        flat_return_types,
        &mut next_local,
        &mut extra_local_types,
        tir_modules,
        type_table,
    );
    func.body = Some(body);
    func.local_count = next_local;
    func.local_types.extend(extra_local_types);
}

/// Replace every `task return` statement in a function body with a no-op (`Continue`).
///
/// Used in the test world where `export async fn` bodies are not exported and will
/// be removed by DCE. The statements must not reach `monomorphize` intact.
fn strip_task_returns_in_func(user_func: &Rc<RefCell<TirFunction>>) {
    let mut func = user_func.borrow_mut();
    let Some(mut body) = func.body.take() else {
        return;
    };
    strip_task_returns_in_block(&mut body);
    func.body = Some(body);
}

fn strip_task_returns_in_block(blk: &mut TirBlock) {
    for stmt in &mut blk.stmts {
        if matches!(&stmt.kind, TirStmtKind::TaskReturn { .. }) {
            stmt.kind = TirStmtKind::Continue;
        } else {
            strip_task_returns_in_stmt(stmt);
        }
    }
}

fn strip_task_returns_in_stmt(stmt: &mut TirStmt) {
    match &mut stmt.kind {
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        }
        | TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            strip_task_returns_in_block(then_block);
            if let Some(else_blk) = else_block {
                strip_task_returns_in_block(else_blk);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            strip_task_returns_in_block(body);
        }
        _ => {}
    }
}

fn expand_task_return_in_block(
    blk: &mut TirBlock,
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    local_types: &mut Vec<TypeId>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    let stmts = std::mem::take(&mut blk.stmts);
    let mut new_stmts: Vec<TirStmt> = Vec::with_capacity(stmts.len());
    for mut stmt in stmts {
        if matches!(&stmt.kind, TirStmtKind::TaskReturn { .. }) {
            if let TirStmtKind::TaskReturn { value } =
                std::mem::replace(&mut stmt.kind, TirStmtKind::Continue)
            {
                let expanded = generate_inline_task_return(
                    value,
                    flat_return_types,
                    next_local,
                    local_types,
                    tir_modules,
                    type_table,
                );
                new_stmts.extend(expanded);
            }
        } else {
            expand_task_return_in_stmt(
                &mut stmt,
                flat_return_types,
                next_local,
                local_types,
                tir_modules,
                type_table,
            );
            new_stmts.push(stmt);
        }
    }
    blk.stmts = new_stmts;
}

fn expand_task_return_in_stmt(
    stmt: &mut TirStmt,
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    local_types: &mut Vec<TypeId>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    match &mut stmt.kind {
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            expand_task_return_in_block(
                then_block,
                flat_return_types,
                next_local,
                local_types,
                tir_modules,
                type_table,
            );
            if let Some(blk) = else_block {
                expand_task_return_in_block(
                    blk,
                    flat_return_types,
                    next_local,
                    local_types,
                    tir_modules,
                    type_table,
                );
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            expand_task_return_in_block(
                body,
                flat_return_types,
                next_local,
                local_types,
                tir_modules,
                type_table,
            );
        }
        TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            expand_task_return_in_block(
                then_block,
                flat_return_types,
                next_local,
                local_types,
                tir_modules,
                type_table,
            );
            if let Some(blk) = else_block {
                expand_task_return_in_block(
                    blk,
                    flat_return_types,
                    next_local,
                    local_types,
                    tir_modules,
                    type_table,
                );
            }
        }
        _ => {}
    }
}

/// Generate the inline task-return sequence for `task return value`.
///
/// For `Result<T, E>` values, generates:
/// - Ok arm: flatten T → call task-return(0, ...`flat_ok_values`)
/// - Err arm: flatten E → call task-return(1, ...`flat_err_values`)
///
/// For other types, generates task-return(0, ...`flat_values`).
fn generate_inline_task_return(
    value: TirExpr,
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    local_types: &mut Vec<TypeId>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
) -> Vec<TirStmt> {
    let mut stmts: Vec<TirStmt> = Vec::new();
    let value_type_id = value.type_id;

    let tt = type_table.borrow();
    let is_result = matches!(
        tt.get(value_type_id),
        crate::tir::ResolvedType::GenericInstance { name, .. } if name == "Result"
    );

    if is_result && !flat_return_types.is_empty() {
        let (ok_type_id, err_type_id) = match tt.get(value_type_id) {
            crate::tir::ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => panic!("Expected Result<T, E> type"),
        };
        drop(tt);

        // Store result in local
        let result_local = alloc_local(next_local, local_types, value_type_id);
        stmts.push(let_stmt("__task_ret", result_local, value_type_id, value));

        // Allocate mutable flat value locals (initialized to zero)
        let flat_locals: Vec<(u32, String)> = flat_return_types
            .iter()
            .enumerate()
            .map(|(i, &vt)| {
                let type_id = cm_val_type_to_type_id(vt);
                let local = alloc_local(next_local, local_types, type_id);
                let name = format!("__tv_{i}");
                stmts.push(let_mut_stmt(&name, local, type_id, cm_zero(vt)));
                (local, name)
            })
            .collect();

        let task_return_args: Vec<TirExpr> = flat_locals
            .iter()
            .zip(flat_return_types.iter())
            .map(|((local, name), &vt)| local_ref(*local, name, cm_val_type_to_type_id(vt)))
            .collect();

        // === Ok case ===
        let mut ok_stmts: Vec<TirStmt> = Vec::new();
        ok_stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_return_types[0]),
            ),
            i32_const(0),
        )));
        let ok_value = variant_payload(
            local_ref(result_local, "__task_ret", value_type_id),
            0,
            ok_type_id,
        );
        let tt = type_table.borrow();
        let ok_flat_types = flat_types_from_type_id(ok_type_id, tir_modules, &tt);
        drop(tt);
        if !ok_flat_types.is_empty() {
            let ok_local = alloc_local(next_local, local_types, ok_type_id);
            ok_stmts.push(let_stmt("__ok_val", ok_local, ok_type_id, ok_value));
            let tt = type_table.borrow();
            let ok_lowered = synthesize_lower_to_flat(
                local_ref(ok_local, "__ok_val", ok_type_id),
                ok_type_id,
                next_local,
                &mut ok_stmts,
                local_types,
                tir_modules,
                &tt,
            );
            drop(tt);
            for (i, flat_val) in ok_lowered.iter().enumerate() {
                if 1 + i < flat_locals.len() {
                    let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                    let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                    let mut val = local_ref(flat_val.index, "__flat", source_type);
                    if flat_val.cm_type != flat_return_types[1 + i] {
                        val = cast(val, target_type);
                    }
                    ok_stmts.push(expr_stmt(assign(
                        local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                        val,
                    )));
                }
            }
        }
        ok_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            task_return_args.clone(),
            TypeTable::UNIT,
        )));
        // No return here: task.return is a cooperative yield, not a function exit.
        // Execution continues after task.return so user code after `task return` runs.

        // === Err case ===
        let mut err_stmts: Vec<TirStmt> = Vec::new();
        err_stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_return_types[0]),
            ),
            i32_const(1),
        )));
        let err_value = variant_payload(
            local_ref(result_local, "__task_ret", value_type_id),
            1,
            err_type_id,
        );
        let err_local = alloc_local(next_local, local_types, err_type_id);
        err_stmts.push(let_stmt("__err_val", err_local, err_type_id, err_value));
        let tt = type_table.borrow();
        let err_resolved = tt.get(err_type_id);
        if let crate::tir::ResolvedType::Variant { name, .. } = &err_resolved {
            if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
                synthesize_variant_lower_to_flat(
                    err_local,
                    err_type_id,
                    &variant_decl,
                    &flat_locals[1..],
                    &flat_return_types[1..],
                    next_local,
                    &mut err_stmts,
                    local_types,
                    tir_modules,
                    &tt,
                );
            } else if flat_locals.len() > 1 {
                err_stmts.push(expr_stmt(assign(
                    local_ref(
                        flat_locals[1].0,
                        &flat_locals[1].1,
                        cm_val_type_to_type_id(flat_return_types[1]),
                    ),
                    local_ref(err_local, "__err_val", err_type_id),
                )));
            }
        } else {
            let err_lowered = synthesize_lower_to_flat(
                local_ref(err_local, "__err_val", err_type_id),
                err_type_id,
                next_local,
                &mut err_stmts,
                local_types,
                tir_modules,
                &tt,
            );
            for (i, flat_val) in err_lowered.iter().enumerate() {
                if 1 + i < flat_locals.len() {
                    let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                    let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                    let mut val = local_ref(flat_val.index, "__flat", source_type);
                    if flat_val.cm_type != flat_return_types[1 + i] {
                        val = cast(val, target_type);
                    }
                    err_stmts.push(expr_stmt(assign(
                        local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                        val,
                    )));
                }
            }
        }
        drop(tt);
        err_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            task_return_args,
            TypeTable::UNIT,
        )));
        // No return here: see comment in ok_stmts above.

        // Combine Ok/Err branches
        stmts.push(if_stmt(
            variant_test(
                local_ref(result_local, "__task_ret", value_type_id),
                0,
                "Ok",
            ),
            block(ok_stmts),
            Some(block(err_stmts)),
        ));
    } else {
        drop(tt);
        // Non-Result (or empty flat types): just emit task-return(0)
        stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            vec![i32_const(0)],
            TypeTable::UNIT,
        )));
    }

    stmts
}

/// Phase entry point: generate CM binding functions and rewrite call sites.
///
/// For each WASI import function used in the program:
/// 1. Synthesizes a binding TIR function that handles CM boundary crossing
/// 2. Rewrites effect-like `Call` nodes to target the binding function
///
/// For each world export function:
/// 3. Synthesizes an export binding that wraps the user function with task-return
///
/// Adapter functions flow through monomorphize → lower → optimize → codegen
/// like any other function.
pub fn generate_adapters(mut project: Package) -> Result<Package, String> {
    let entry_source = project.entry_module_source.clone();

    // ---- Import adapters ----

    // Step 1: Collect all used WASI effect calls and resource method calls
    let mut seen_effects: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(body, &mut seen_effects, project.wasi_registry);
            }
        }
    }

    if !seen_effects.is_empty() {
        // Step 2: Synthesize binding functions for each used WASI function
        let entry_type_table = project
            .tir_modules
            .get(&project.entry_module_source)
            .map(|m| m.type_table.clone())
            .unwrap_or_else(|| Rc::new(RefCell::new(TypeTable::new())));
        // Map effect/resource name → defining module source. Used to attach the
        // canonical owner as an effect on each generated binding so the
        // checker's `(module_source, name)` identity matches user-written
        // `with E` clauses (which the resolver also canonicalises to the
        // defining module).
        let owner_sources = effect_owner_module_sources(&project.tir_modules);
        let mut adapters: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
        for qualified_name in &seen_effects {
            if let Some(func_info) = project.wasi_registry.get_function(qualified_name) {
                let func_info = func_info.clone();
                let binding_name =
                    binding_func_name(&func_info.effect_name, &func_info.method_name);
                let owner_module =
                    lookup_effect_owner(&owner_sources, &func_info.effect_name, &func_info.package)
                        .unwrap_or_else(|| ModuleSource::wasi(&func_info.package));
                let adapter = synthesize_adapter(
                    &func_info,
                    project.wasi_registry,
                    &entry_type_table,
                    &owner_module,
                );
                adapters.insert(qualified_name.clone(), adapter.clone());
                // Also index by binding function name for lookup
                adapters.insert(binding_name, adapter);
            }
        }

        // Step 3: Add binding functions to the entry module
        if let Some(entry_module) = project.tir_modules.get_mut(&entry_source) {
            for (key, adapter_rc) in &adapters {
                // Only add each adapter once (skip the duplicate keyed by binding_name)
                if key.contains("::") {
                    entry_module.functions.push(adapter_rc.clone());
                }
            }
        }

        // Step 4: Rewrite effect-like call nodes to target adapters
        let adapter_map: IndexMap<String, Rc<RefCell<TirFunction>>> = adapters
            .iter()
            .filter(|(k, _)| k.contains("::"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for module in project.tir_modules.values() {
            for func_rc in &module.functions {
                let mut func = func_rc.borrow_mut();
                if let Some(body) = &mut func.body {
                    rewrite_calls_in_block(
                        body,
                        &adapter_map,
                        &entry_source,
                        project.wasi_registry,
                        &entry_type_table,
                    );
                }
                // Sync local_types with any Let stmts that were updated by the rewrite
                // (e.g., streaming binding calls changing the let binding type to i32).
                if !func.local_types.is_empty() {
                    let mut updates = Vec::new();
                    if let Some(body) = &func.body {
                        collect_local_type_updates(body, &func.local_types, &mut updates);
                    }
                    for (idx, type_id) in updates {
                        func.local_types[idx] = type_id;
                    }
                }
            }
        }
    }

    // ---- Export adapters ----

    // Step 5: Synthesize export bindings for world exports (signature-driven)
    let world_info = project.world_registry.get(&project.target_world).cloned();
    if let Some(world_info) = world_info {
        let entry_type_table = project
            .tir_modules
            .get(&entry_source)
            .map(|m| m.type_table.clone())
            .unwrap_or_else(|| Rc::new(RefCell::new(TypeTable::new())));

        // Collect adapters in a read-only pass (synthesize_result_export_binding needs &tir_modules)
        let mut export_adapters: Vec<(String, String, Rc<RefCell<TirFunction>>)> = Vec::new();
        {
            let entry_module = project
                .tir_modules
                .get(&entry_source)
                .expect("entry module should exist");

            // Package hint for CM name resolution inside export adapters.
            // For `wasi:http/service` this is `"http"`; for
            // `core:kiln/generator` it is `"kiln"`. The hint biases bare-name
            // resolution towards the binding's owning package (e.g.
            // `ErrorCode` in `wasi:http` bindings) and feeds
            // `resolve_cm_source_for` as a fallback anchor.
            let binding_wasi_package: String = {
                let fq = &world_info.fq_name;
                fq.split_once(':')
                    .and_then(|(_, rest)| rest.split_once('/'))
                    .map(|(pkg, _)| pkg.to_string())
                    .unwrap_or_default()
            };

            for export in &world_info.exports {
                // Find the user's export function and check for missing `export` keyword
                let mut found_exported = None;
                let mut found_without_export = false;
                for f in &entry_module.functions {
                    let func = f.borrow();
                    if func.name == export.name {
                        if func.is_export {
                            found_exported = Some(f.clone());
                        } else {
                            found_without_export = true;
                        }
                    }
                }

                if found_exported.is_none() && found_without_export {
                    return Err(format!(
                        "function `{}` exists but is not marked with `export` keyword. \
                         Add `export` to make it a world entry point: `export fn {}(...)`",
                        export.name, export.name
                    ));
                }

                let binding_name = export_binding_func_name(&export.name);
                let adapter = if let Some(user_func_rc) = found_exported {
                    // Validate parameter count matches world declaration
                    {
                        let user_func = user_func_rc.borrow();
                        if user_func.params.len() != export.params.len() {
                            return Err(format!(
                                "export function `{}` has {} parameter(s), \
                                 but the world expects {} parameter(s)",
                                export.name,
                                user_func.params.len(),
                                export.params.len()
                            ));
                        }
                    }

                    // Check if user function is `export async fn`
                    let is_async_export = {
                        let user_func = user_func_rc.borrow();
                        user_func.is_async
                    };

                    if is_async_export {
                        // Async export: the user function calls task-return internally via
                        // `task return expr` stmts. Expand those stmts into CM task-return
                        // calls and synthesize a simple lifting adapter.
                        if let Some(return_type) = &export.return_type {
                            let tt = entry_type_table.borrow();
                            let flat_types = compute_export_flat_return_types(
                                return_type,
                                &project.tir_modules,
                                &tt,
                            );
                            drop(tt);
                            expand_task_returns_in_func(
                                &user_func_rc,
                                &flat_types,
                                &project.tir_modules,
                                &entry_type_table,
                            );
                        }
                        synthesize_async_export_binding(
                            &export.name,
                            user_func_rc,
                            &entry_source,
                            &project.tir_modules,
                            &entry_type_table,
                            &export.params,
                            project.wasi_registry,
                            &binding_wasi_package,
                        )
                    } else {
                        // Check the user function's actual return type (signature-driven)
                        let user_returns_result = {
                            let user_func = user_func_rc.borrow();
                            let tt = entry_type_table.borrow();
                            matches!(
                                tt.get(user_func.return_type),
                                crate::tir::ResolvedType::GenericInstance { name, .. }
                                    if name == "Result"
                            )
                        };

                        if user_returns_result {
                            // Result<T, E> return: full lowering adapter (signature-driven)
                            let tt = entry_type_table.borrow();
                            let flat_types = compute_export_flat_return_types(
                                export.return_type.as_ref().unwrap(),
                                &project.tir_modules,
                                &tt,
                            );
                            drop(tt);
                            synthesize_result_export_binding(
                                &export.name,
                                user_func_rc,
                                &entry_source,
                                export.return_type.as_ref().unwrap(),
                                &flat_types,
                                &project.tir_modules,
                                &entry_type_table,
                                &export.params,
                                project.wasi_registry,
                                &binding_wasi_package,
                            )
                        } else {
                            // Non-Result return: check if we can use the simple void adapter
                            // (only when no params AND unit return type)
                            let is_void_no_params = export.params.is_empty() && {
                                let user_func = user_func_rc.borrow();
                                let tt = entry_type_table.borrow();
                                matches!(
                                    tt.get(user_func.return_type),
                                    crate::tir::ResolvedType::Unit
                                )
                            };

                            if is_void_no_params {
                                // Simple void adapter for () -> ()
                                synthesize_void_export_binding(
                                    &export.name,
                                    user_func_rc,
                                    &entry_source,
                                )
                            } else {
                                // General adapter: handles params (with lifting if needed)
                                // and non-void return types
                                synthesize_general_export_binding(
                                    &export.name,
                                    user_func_rc,
                                    &entry_source,
                                    &project.tir_modules,
                                    &entry_type_table,
                                    &export.params,
                                    project.wasi_registry,
                                    &binding_wasi_package,
                                )
                            }
                        }
                    }
                } else {
                    // No user function: stub that just calls task-return(0)
                    synthesize_void_stub_adapter(&export.name)
                };
                export_adapters.push((export.name.clone(), binding_name, adapter));
            }
        }

        // Compute the correct task-return params from the export's flat return types.
        // The builtin registry defines task_return with a single i32 param, but for
        // Result-returning exports the task-return call passes the full flattened type.
        // Store on Package so optimize_dce can use it when creating the import.
        for export in &world_info.exports {
            if let Some(return_type) = &export.return_type {
                let tt = entry_type_table.borrow();
                let flat_types =
                    compute_export_flat_return_types(return_type, &project.tir_modules, &tt);
                project.task_return_flat_params = Some(
                    flat_types
                        .iter()
                        .map(|&vt| cm_val_type_to_type_id(vt))
                        .collect(),
                );
                break; // One export is enough — all share the same task-return
            }
        }

        // Push adapters with mutable access
        let entry_module = project
            .tir_modules
            .get_mut(&entry_source)
            .expect("entry module should exist");
        for (export_name, binding_name, adapter) in export_adapters {
            project
                .export_binding_names
                .insert(export_name, binding_name);
            entry_module.functions.push(adapter);
        }
    }

    // Step 6: Synthesize export bindings for test functions (__test_*)
    // Only when targeting the test world — in other worlds, tests are dead code.
    if project.is_test_world() {
        let entry_module = project
            .tir_modules
            .get_mut(&entry_source)
            .expect("entry module should exist");

        // Collect test functions first to avoid borrow conflict.
        // Test functions have is_export=false (they're not world exports),
        // but they need adapters for task-return when called via `wado test`.
        let test_funcs: Vec<(String, Rc<RefCell<TirFunction>>)> = entry_module
            .functions
            .iter()
            .filter(|f| f.borrow().name.starts_with("__test_"))
            .map(|f| (f.borrow().name.clone(), f.clone()))
            .collect();

        for (test_name, user_func_rc) in test_funcs {
            let binding_name = export_binding_func_name(&test_name);
            let adapter = synthesize_void_export_binding(&test_name, user_func_rc, &entry_source);
            project.export_binding_names.insert(test_name, binding_name);
            entry_module.functions.push(adapter);
        }
    }

    // Strip remaining TaskReturn from all modules.
    // `task return` is only valid inside `async fn` (checked by resolver).
    // Step 5 expands TaskReturn into CM calls for async exports that match the
    // target world. Any remaining async fn (unmatched exports, imported modules)
    // will be DCE'd — strip their TaskReturn stmts so they don't reach monomorphize.
    // This is idempotent: already-expanded functions have no TaskReturn stmts left.
    for module in project.tir_modules.values() {
        for f in &module.functions {
            let needs_strip = {
                let func = f.borrow();
                func.is_async
            };
            if needs_strip {
                strip_task_returns_in_func(f);
            }
        }
    }

    // ---- Record Stream Read Adapters ----
    // Generate binding functions for Stream<T>.read() where T is a WASI record type.
    // Must run before rewrite_cm_resource_methods so the generated functions are available.
    synthesize_record_stream_reads(&mut project);

    // ---- CM Resource Method Adapters ----
    // Rewrite #[cm("...")] resource method calls to target internal/builtin binding functions.
    // This replaces the inline WIR emission in wir_build/translate.rs with pre-monomorphization
    // synthesis, so the binding functions go through the normal compilation pipeline.
    rewrite_cm_resource_methods(&mut project);

    Ok(project)
}

/// Generate binding functions for Stream<T>.`read()` where T is a non-u8 WASI record type.
///
/// For each unique stream element type T found in stream-read calls, generates a
/// TIR function `__cm_stream_read_<T>` that:
/// 1. Calls `cm_stream_read_raw(handle, max, elem_size, elem_align)` to get raw buffer
/// 2. Loops through the buffer, lifting each record from linear memory
/// 3. Constructs `Array<T>` and returns it
///
/// The generated functions are added to the entry module so they can be called
/// by the CM resource method rewriter.
fn synthesize_record_stream_reads(project: &mut Package) {
    use std::cell::RefCell;
    use std::rc::Rc;

    let wasi_registry = project.wasi_registry;
    // Find all non-u8 stream-read element types
    let mut needed_element_types: IndexMap<String, (TypeId, TypeId)> = IndexMap::default();
    for module in project.tir_modules.values() {
        let tt = module.type_table.borrow();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                find_record_stream_reads(body, &tt, &mut needed_element_types);
            }
        }
    }
    if needed_element_types.is_empty() {
        return;
    }

    // Generate binding functions for each element type.
    // Use the actual entry module — not `values().next()`, which returns the
    // first module in the IndexMap and is not guaranteed to be the entry module.
    // Calls synthesized by `rewrite_cm_resource_methods` target the entry module
    // via `entry_call`, so the binding functions must live there for resolution
    // to succeed in wir_build.
    let entry_source = project.entry_module_source.clone();
    let entry_module = project
        .tir_modules
        .get(&entry_source)
        .expect("entry module must exist in tir_modules");
    let type_table = entry_module.type_table.clone();
    let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();

    for (elem_name, (elem_type_id, array_type_id)) in &needed_element_types {
        // Stream-record element types come from `find_record_stream_reads`,
        // which only produces WASI record names. Resolve the name to its
        // defining `wasi:*` interface and then fetch fields strictly.
        let Some(source) = wasi_registry.find_wasi_struct_source(elem_name) else {
            continue;
        };
        let source = source.to_string();
        let Some(fields) = wasi_registry.get_struct_fields_by_source(&source, elem_name) else {
            continue;
        };
        let ast_type = crate::ast::Type::Named(crate::ast::NamedType {
            id: crate::ast::AstId::fresh(),
            name: elem_name.clone(),
            span: synth_span(),
            source_interface: Some(source.clone()),
        });
        let elem_size =
            crate::component_model::cm_size_with_registry(&ast_type, wasi_registry) as i32;
        let elem_align =
            crate::component_model::cm_align_with_registry(&ast_type, wasi_registry) as i32;

        let func = synthesize_stream_read_func(
            elem_name,
            *elem_type_id,
            *array_type_id,
            fields,
            elem_size,
            elem_align,
            wasi_registry,
            &type_table,
        );
        new_functions.push(Rc::new(RefCell::new(func)));
    }

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules");
    for func in new_functions {
        entry_module.functions.push(func);
    }
}

/// Find all stream-read method calls that return Array<T> where T is not u8.
fn find_record_stream_reads(
    block: &TirBlock,
    tt: &TypeTable,
    results: &mut IndexMap<String, (TypeId, TypeId)>,
) {
    for stmt in &block.stmts {
        find_record_stream_reads_in_stmt(stmt, tt, results);
    }
}

fn find_record_stream_reads_in_stmt(
    stmt: &TirStmt,
    tt: &TypeTable,
    results: &mut IndexMap<String, (TypeId, TypeId)>,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => find_record_stream_reads_in_expr(value, tt, results),
        TirStmtKind::Expr(value) => find_record_stream_reads_in_expr(value, tt, results),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                find_record_stream_reads_in_expr(v, tt, results);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            find_record_stream_reads_in_expr(condition, tt, results);
            find_record_stream_reads(then_block, tt, results);
            if let Some(blk) = else_block {
                find_record_stream_reads(blk, tt, results);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            find_record_stream_reads(body, tt, results);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            find_record_stream_reads_in_expr(scrutinee, tt, results);
            find_record_stream_reads(then_block, tt, results);
            if let Some(blk) = else_block {
                find_record_stream_reads(blk, tt, results);
            }
        }
        _ => {}
    }
}

fn find_record_stream_reads_in_expr(
    expr: &TirExpr,
    tt: &TypeTable,
    results: &mut IndexMap<String, (TypeId, TypeId)>,
) {
    // Recurse into sub-expressions
    match &expr.kind {
        TirExprKind::MethodCall { receiver, args, .. } => {
            find_record_stream_reads_in_expr(receiver, tt, results);
            for arg in args {
                find_record_stream_reads_in_expr(&arg.expr, tt, results);
            }
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                find_record_stream_reads_in_expr(&arg.expr, tt, results);
            }
        }
        _ => {}
    }

    // Check if this is a stream-read call with non-u8 element type
    let cm_name = match &expr.kind {
        TirExprKind::MethodCall { func, .. } => {
            func.method_info.as_ref().and_then(|m| m.cm_name.clone())
        }
        _ => None,
    };
    if cm_name.as_deref() == Some("stream-read") && !is_u8_array_type(expr.type_id, tt) {
        // Extract element type from Array<T>
        if let Some(type_args) = tt.generic_type_args(expr.type_id)
            && let Some(&elem_type_id) = type_args.first()
        {
            let elem_name = tt.base_type_name(elem_type_id);
            results
                .entry(elem_name)
                .or_insert((elem_type_id, expr.type_id));
        }
    }
}

/// Generate a TIR function for reading records from a stream.
///
/// Generates `__cm_stream_read_<T>(handle: i32, max: i32) -> Array<T>`:
/// 1. Call `cm_stream_read_raw` to get raw buffer [ptr, count]
/// 2. Loop: lift each record from buffer at ptr + i * `elem_size`
/// 3. Append to result array
/// 4. Free buffer
/// 5. Return array
fn synthesize_stream_read_func(
    elem_name: &str,
    elem_type_id: TypeId,
    array_type_id: TypeId,
    _fields: &[(String, crate::ast::Type)],
    elem_size: i32,
    elem_align: i32,
    wasi_registry: &crate::component_model::WasiRegistry,
    type_table: &RefCell<TypeTable>,
) -> TirFunction {
    use crate::synthesis::common::{
        assign, binary, break_stmt, builtin_call, cm_raw_call, expr_stmt, i32_const, if_stmt,
        internal_call, let_mut_stmt, let_stmt, local_ref, loop_stmt, return_stmt, synth_span,
    };

    let func_name = format!("__cm_stream_read_{elem_name}");
    let _tuple_type_id = type_table
        .borrow_mut()
        .make_tuple(vec![TypeTable::I32, TypeTable::I32]);

    let mut next_local: u32 = 0;
    let mut local_types: Vec<TypeId> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    // Params: handle (i32), max (i32)
    let handle_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);
    let max_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);

    // Use the CM kebab-case name for the stream-read intrinsic
    let cm_record_name = wasi_registry
        .get_struct_cm_name(elem_name)
        .unwrap_or(elem_name)
        .to_string();
    let stream_read_name = format!("stream-read:{cm_record_name}");

    // let byte_count = max * elem_size
    let byte_count_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);
    let byte_count = binary(
        TirBinaryOp::Mul,
        local_ref(max_idx, "max", TypeTable::I32),
        i32_const(elem_size),
        TypeTable::I32,
    );
    stmts.push(let_stmt(
        "byte_count",
        byte_count_idx,
        TypeTable::I32,
        byte_count,
    ));

    // let ptr = realloc(0, 0, elem_align, byte_count)
    let ptr_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);
    let alloc_call = builtin_call(
        "realloc",
        vec![
            i32_const(0),
            i32_const(0),
            i32_const(elem_align),
            local_ref(byte_count_idx, "byte_count", TypeTable::I32),
        ],
        TypeTable::I32,
    );
    stmts.push(let_stmt("ptr", ptr_idx, TypeTable::I32, alloc_call));

    // let mut result = stream-read:directory-entry(handle, ptr, max)
    let result_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);
    let stream_read_call = cm_raw_call(
        &stream_read_name,
        vec![
            local_ref(handle_idx, "handle", TypeTable::I32),
            local_ref(ptr_idx, "ptr", TypeTable::I32),
            local_ref(max_idx, "max", TypeTable::I32),
        ],
        TypeTable::I32,
    );
    stmts.push(let_mut_stmt(
        "result",
        result_idx,
        TypeTable::I32,
        stream_read_call,
    ));

    // if result == -1 { result = wait_for_blocked(handle); }
    let blocked_check = binary(
        TirBinaryOp::Eq,
        local_ref(result_idx, "result", TypeTable::I32),
        i32_const(-1),
        TypeTable::BOOL,
    );
    let wait_call = internal_call(
        "wait_for_blocked",
        vec![local_ref(handle_idx, "handle", TypeTable::I32)],
        TypeTable::I32,
    );
    stmts.push(if_stmt(
        blocked_check,
        TirBlock {
            stmts: vec![expr_stmt(assign(
                local_ref(result_idx, "result", TypeTable::I32),
                wait_call,
            ))],
            span: synth_span(),
        },
        None,
    ));

    // let count = result >> 4
    let count_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);
    let count_expr = binary(
        TirBinaryOp::Shr,
        local_ref(result_idx, "result", TypeTable::I32),
        i32_const(4),
        TypeTable::I32,
    );
    stmts.push(let_stmt("count", count_idx, TypeTable::I32, count_expr));

    // let mut arr = Array::<T>::with_capacity(count)
    // Use internal_from_raw with a new GC array
    // Actually, build the array by appending elements one by one
    let arr_idx = next_local;
    next_local += 1;
    local_types.push(array_type_id);

    // Create empty array via Array<T>::with_capacity(count)
    let empty_arr = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::array(),
                name: format!("Array<{elem_name}>::with_capacity"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "Array::with_capacity".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("Array<{elem_name}>"),
                    base_struct_name: "Array".to_string(),
                    trait_name: None,
                    method_name: "with_capacity".to_string(),
                    method_type_args: vec![],
                    is_type_param_receiver: false,
                    is_ref_impl: false,
                    cm_name: None,
                }),
            },
            type_args: vec![],
            args: vec![CallArg::new(
                local_ref(count_idx, "count", TypeTable::I32),
                false,
            )],
        },
        array_type_id,
        synth_span(),
    );
    stmts.push(let_mut_stmt("arr", arr_idx, array_type_id, empty_arr));

    // let mut i = 0
    let i_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);
    stmts.push(let_mut_stmt("i", i_idx, TypeTable::I32, i32_const(0)));

    // Loop body: while i < count
    let mut loop_body_stmts = Vec::new();

    // if i >= count { break; }
    let break_cond = binary(
        TirBinaryOp::GtEq,
        local_ref(i_idx, "i", TypeTable::I32),
        local_ref(count_idx, "count", TypeTable::I32),
        TypeTable::BOOL,
    );
    loop_body_stmts.push(if_stmt(
        break_cond,
        TirBlock {
            stmts: vec![break_stmt()],
            span: synth_span(),
        },
        None,
    ));

    // let addr = ptr + i * elem_size
    let addr_idx = next_local;
    next_local += 1;
    local_types.push(TypeTable::I32);
    let offset = binary(
        TirBinaryOp::Mul,
        local_ref(i_idx, "i", TypeTable::I32),
        i32_const(elem_size),
        TypeTable::I32,
    );
    let addr = binary_add(local_ref(ptr_idx, "ptr", TypeTable::I32), offset);
    loop_body_stmts.push(let_stmt("addr", addr_idx, TypeTable::I32, addr));

    // Lift each field from linear memory at addr + field_offset
    let lift_ctx = LiftContext {
        wasi_registry,
        type_table,
        wasi_package: "filesystem",
    };
    let ast_type = crate::ast::Type::Named(crate::ast::NamedType {
        id: crate::ast::AstId::fresh(),
        name: elem_name.to_string(),
        span: synth_span(),
        source_interface: None,
    });
    let lifted_elem = synthesize_lift_with_context(
        &ast_type,
        local_ref(addr_idx, "addr", TypeTable::I32),
        &mut next_local,
        &mut loop_body_stmts,
        &mut local_types,
        &lift_ctx,
    );

    // Push to array - use Array::push method pattern
    // arr.push(elem) → internal call
    let elem_idx = next_local;
    next_local += 1;
    local_types.push(elem_type_id);
    loop_body_stmts.push(let_stmt("elem", elem_idx, elem_type_id, lifted_elem));

    let push_call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(local_ref(arr_idx, "arr", array_type_id)),
            FunctionRef {
                module_source: ModuleSource::array(),
                name: format!("Array<{elem_name}>::push"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "Array::push".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("Array<{elem_name}>"),
                    base_struct_name: "Array".to_string(),
                    trait_name: None,
                    method_name: "push".to_string(),
                    method_type_args: vec![],
                    is_type_param_receiver: false,
                    is_ref_impl: false,
                    cm_name: None,
                }),
            },
            vec![],
            vec![CallArg::new(
                local_ref(elem_idx, "elem", elem_type_id),
                false,
            )],
        ),
        TypeTable::UNIT,
        synth_span(),
    );
    loop_body_stmts.push(expr_stmt(push_call));

    // i += 1
    let increment = assign(
        local_ref(i_idx, "i", TypeTable::I32),
        binary_add(local_ref(i_idx, "i", TypeTable::I32), i32_const(1)),
    );
    loop_body_stmts.push(expr_stmt(increment));

    stmts.push(loop_stmt(TirBlock {
        stmts: loop_body_stmts,
        span: synth_span(),
    }));

    // Free buffer: realloc(ptr, byte_count, elem_align, 0)
    let free_call = builtin_call(
        "realloc",
        vec![
            local_ref(ptr_idx, "ptr", TypeTable::I32),
            local_ref(byte_count_idx, "byte_count", TypeTable::I32),
            i32_const(elem_align),
            i32_const(0),
        ],
        TypeTable::I32,
    );
    stmts.push(let_stmt("__freed", next_local, TypeTable::I32, free_call));
    next_local += 1;
    local_types.push(TypeTable::I32);

    // return arr
    stmts.push(return_stmt(Some(local_ref(arr_idx, "arr", array_type_id))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        is_pub: false,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params: vec![
            TirParam {
                name: "handle".to_string(),
                local_index: handle_idx,
                type_id: TypeTable::I32,
                is_mut: false,
                span: synth_span(),
                default_expr: None,
            },
            TirParam {
                name: "max".to_string(),
                local_index: max_idx,
                type_id: TypeTable::I32,
                is_mut: false,
                span: synth_span(),
                default_expr: None,
            },
        ],
        return_type: array_type_id,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(TirBlock {
            stmts,
            span: synth_span(),
        }),
        span: synth_span(),
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: true,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    }
}

/// Determine the internal binding function name for a CM resource method.
/// Returns `Some(("internal" | "builtin", function_name))` or `None` if not handled.
/// Maps CM method names to their adapter dispatch.
/// - `"raw"`: direct `CmRawCall` to canonical Wasm import (for simple void operations)
/// - `"internal"`: call to internal.wado binding function (for complex operations)
fn cm_binding_function(cm_name: &str) -> Option<(&'static str, &'static str)> {
    match cm_name {
        // Simple drops → direct CmRawCall (non-parameterized)
        "stream-drop-readable" => Some(("raw", "stream-drop-readable")),
        "stream-drop-writable" => Some(("raw", "stream-drop-writable")),
        "waitable-set-drop" => Some(("raw", "waitable-set-drop")),
        "subtask-drop" => Some(("raw", "subtask-drop")),
        "error-context-drop" => Some(("raw", "error-context-drop")),

        // Simple cancel → direct CmRawCall (non-parameterized)
        "stream-cancel-read" => Some(("raw", "stream-cancel-read")),
        "stream-cancel-write" => Some(("raw", "stream-cancel-write")),
        "subtask-cancel" => Some(("raw", "subtask-cancel")),

        // Future drops/cancels are parameterized by payload type — leave for WIR translate

        // waitable-join: void canonical, returns the handle as Waitable
        "waitable-join" => Some(("internal", "cm_waitable_join")),

        // Simple constructors → direct CmRawCall (returns i32 handle)
        "waitable-set-new" => Some(("raw", "waitable-set-new")),

        // Complex operations → internal binding functions
        "stream-read" => Some(("internal", "cm_stream_read_u8")),
        "stream-write" => Some(("internal", "cm_stream_write_u8")),
        "stream-write-raw" => Some(("internal", "cm_stream_write_raw_u8")),
        "error-context-new" => Some(("internal", "cm_error_context_new")),
        "error-context-debug-message" => Some(("internal", "cm_error_context_debug_message")),
        "waitable-set-wait" => Some(("internal", "cm_waitable_set_wait")),
        "waitable-set-poll" => Some(("internal", "cm_waitable_set_poll")),

        _ => None,
    }
}

/// Rewrite all #[cm("...")] resource method calls in the project.
fn rewrite_cm_resource_methods(project: &mut Package) {
    let entry_source = project.entry_module_source.clone();
    for module in project.tir_modules.values() {
        let type_table = module.type_table.clone();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                rewrite_cm_methods_in_block(body, &type_table.borrow(), &entry_source);
            }
        }
    }
}

fn rewrite_cm_methods_in_block(block: &mut TirBlock, tt: &TypeTable, entry_source: &ModuleSource) {
    for stmt in &mut block.stmts {
        rewrite_cm_methods_in_stmt(stmt, tt, entry_source);
    }
}

fn rewrite_cm_methods_in_stmt(stmt: &mut TirStmt, tt: &TypeTable, entry_source: &ModuleSource) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, type_id, .. } => {
            let old_type = value.type_id;
            rewrite_cm_methods_in_expr(value, tt, entry_source);
            if value.type_id != old_type {
                *type_id = value.type_id;
            }
        }
        TirStmtKind::Expr(value) => {
            rewrite_cm_methods_in_expr(value, tt, entry_source);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_cm_methods_in_expr(v, tt, entry_source);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_cm_methods_in_expr(condition, tt, entry_source);
            rewrite_cm_methods_in_block(then_block, tt, entry_source);
            if let Some(blk) = else_block {
                rewrite_cm_methods_in_block(blk, tt, entry_source);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_cm_methods_in_block(body, tt, entry_source);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_cm_methods_in_expr(scrutinee, tt, entry_source);
            rewrite_cm_methods_in_block(then_block, tt, entry_source);
            if let Some(blk) = else_block {
                rewrite_cm_methods_in_block(blk, tt, entry_source);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_cm_methods_in_expr(v, tt, entry_source);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_cm_methods_in_expr(value, tt, entry_source);
        }
        TirStmtKind::Continue => {}
        TirStmtKind::TaskReturn { value } => {
            rewrite_cm_methods_in_expr(value, tt, entry_source);
        }
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn rewrite_cm_methods_in_expr(expr: &mut TirExpr, tt: &TypeTable, entry_source: &ModuleSource) {
    // First, recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Call { args, .. } => {
            for arg in args.iter_mut() {
                rewrite_cm_methods_in_expr(&mut arg.expr, tt, entry_source);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_cm_methods_in_expr(receiver, tt, entry_source);
            for arg in args.iter_mut() {
                rewrite_cm_methods_in_expr(&mut arg.expr, tt, entry_source);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_cm_methods_in_expr(left, tt, entry_source);
            rewrite_cm_methods_in_expr(right, tt, entry_source);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            rewrite_cm_methods_in_expr(inner, tt, entry_source);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            rewrite_cm_methods_in_expr(inner, tt, entry_source);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_cm_methods_in_expr(condition, tt, entry_source);
            rewrite_cm_methods_in_block(then_branch, tt, entry_source);
            if let Some(blk) = else_branch {
                rewrite_cm_methods_in_block(blk, tt, entry_source);
            }
        }
        TirExprKind::Match { expr, arms } => {
            rewrite_cm_methods_in_expr(expr, tt, entry_source);
            for arm in arms {
                rewrite_cm_methods_in_expr(&mut arm.body, tt, entry_source);
                if let Some(guard) = &mut arm.guard {
                    rewrite_cm_methods_in_expr(guard, tt, entry_source);
                }
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                rewrite_cm_methods_in_expr(&mut f.value, tt, entry_source);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for e in elements {
                rewrite_cm_methods_in_expr(e, tt, entry_source);
            }
        }
        TirExprKind::FieldAccess { expr, .. } => {
            rewrite_cm_methods_in_expr(expr, tt, entry_source);
        }
        TirExprKind::Index { expr, index, .. } => {
            rewrite_cm_methods_in_expr(expr, tt, entry_source);
            rewrite_cm_methods_in_expr(index, tt, entry_source);
        }
        TirExprKind::Assign { target, value } => {
            rewrite_cm_methods_in_expr(target, tt, entry_source);
            rewrite_cm_methods_in_expr(value, tt, entry_source);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_cm_methods_in_expr(p, tt, entry_source);
            }
        }
        TirExprKind::Block(block) => {
            rewrite_cm_methods_in_block(block, tt, entry_source);
        }
        _ => {}
    }

    // Now check if this expression is a CM resource method call
    let cm_name = match &expr.kind {
        TirExprKind::MethodCall { func, .. } => {
            func.method_info.as_ref().and_then(|m| m.cm_name.clone())
        }
        TirExprKind::Call { func, .. } => func.method_info.as_ref().and_then(|m| m.cm_name.clone()),
        _ => None,
    };

    let Some(cm_name) = cm_name else {
        return;
    };

    // stream-new / future-new remain handled by WIR translate for now,
    // because they require i64→tuple splitting with proper GC type casting.
    // stream-read for non-u8 element types also stays for WIR translate,
    // which generates proper record lifting from linear memory.
    if matches!(cm_name.as_str(), "stream-new" | "future-new") {
        return;
    }
    if cm_name == "stream-read" && !is_u8_array_type(expr.type_id, tt) {
        // Non-u8 stream reads use a generated binding function
        if let Some(type_args) = tt.generic_type_args(expr.type_id)
            && let Some(&elem_type_id) = type_args.first()
        {
            let elem_name = tt.base_type_name(elem_type_id);
            let func_name = format!("__cm_stream_read_{elem_name}");
            rewrite_cm_instance_method(expr, "entry", &func_name, entry_source);
            return;
        }
        return;
    }

    // For stream operations on non-u8 types, parameterize the canonical name
    // and rewrite as CmRawCall directly (since the name is dynamic).
    if is_stream_cm_method(&cm_name) {
        let parameterized = parameterize_stream_cm_name(&cm_name, expr, tt);
        if parameterized != cm_name {
            rewrite_cm_instance_method(expr, "raw", &parameterized, entry_source);
            return;
        }
    }

    // Look up the binding function
    let Some((kind, func_name)) = cm_binding_function(&cm_name) else {
        // Not handled by synthesis yet — will fall through to WIR translate
        return;
    };

    match &mut expr.kind {
        TirExprKind::MethodCall { .. } => {
            rewrite_cm_instance_method(expr, kind, func_name, entry_source);
        }
        TirExprKind::Call { .. } => {
            rewrite_cm_static_method(expr, kind, func_name, entry_source);
        }
        _ => {}
    }
}

/// Rewrite a CM instance method call (receiver.method(args)) to a builtin/internal call.
/// The receiver is cast to i32 (resource handle) and passed as the first argument.
fn rewrite_cm_instance_method(
    expr: &mut TirExpr,
    kind: &str,
    func_name: &str,
    entry_source: &ModuleSource,
) {
    let TirExprKind::MethodCall { receiver, args, .. } = &mut expr.kind else {
        return;
    };

    // Take ownership of receiver and args
    let taken_receiver = std::mem::replace(
        receiver.as_mut(),
        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
    );
    let taken_args: Vec<TirExpr> = std::mem::take(args).into_iter().map(|a| a.expr).collect();

    // Cast receiver to i32 (resource handle)
    let handle = cast(taken_receiver, TypeTable::I32);

    // Build argument list: handle first, then the rest
    let mut all_args = vec![handle];
    all_args.extend(taken_args);

    // Create the replacement call
    let new_expr = match kind {
        "raw" => cm_raw_call(func_name, all_args, expr.type_id),
        "internal" => internal_call(func_name, all_args, expr.type_id),
        // "entry": call to a synthesized function in the entry module
        "entry" => entry_call(func_name, all_args, expr.type_id, entry_source.clone()),
        _ => unreachable!(),
    };

    *expr = new_expr;
}

/// Rewrite a CM static method call (`Type::method(args)`) to a raw/internal call.
fn rewrite_cm_static_method(
    expr: &mut TirExpr,
    kind: &str,
    func_name: &str,
    entry_source: &ModuleSource,
) {
    let TirExprKind::Call { args, .. } = &mut expr.kind else {
        return;
    };

    let taken_args: Vec<TirExpr> = std::mem::take(args).into_iter().map(|a| a.expr).collect();

    let new_expr = match kind {
        "raw" => cm_raw_call(func_name, taken_args, expr.type_id),
        "internal" => internal_call(func_name, taken_args, expr.type_id),
        "entry" => entry_call(func_name, taken_args, expr.type_id, entry_source.clone()),
        _ => unreachable!(),
    };

    *expr = new_expr;
}

/// Check if a CM method name is a stream operation.
fn is_stream_cm_method(cm_name: &str) -> bool {
    matches!(
        cm_name,
        "stream-drop-readable"
            | "stream-drop-writable"
            | "stream-cancel-read"
            | "stream-cancel-write"
    )
}

/// Parameterize a stream CM name based on the receiver type.
/// For non-u8 streams (e.g., `Stream<DirectoryEntry>`), appends the CM record name
/// (e.g., "stream-drop-readable:directory-entry").
fn parameterize_stream_cm_name(cm_name: &str, expr: &TirExpr, tt: &TypeTable) -> String {
    // Get the receiver's type from the method call
    let receiver_type_id = match &expr.kind {
        TirExprKind::MethodCall { receiver, .. } => receiver.type_id,
        _ => return cm_name.to_string(),
    };
    // Resolve through references: &Stream<T> → Stream<T>
    use crate::tir::ResolvedType;
    let mut type_id = receiver_type_id;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(type_id) {
        type_id = *inner;
    }
    // Extract element type from Stream<T>
    if let Some(type_args) = tt.generic_type_args(type_id)
        && let Some(&elem) = type_args.first()
    {
        let elem_name = tt.base_type_name(elem);
        if elem_name != "u8" {
            // Convert PascalCase to kebab-case for the CM name
            let cm_elem = elem_name.chars().fold(String::new(), |mut s, c| {
                if c.is_uppercase() && !s.is_empty() {
                    s.push('-');
                }
                s.push(c.to_ascii_lowercase());
                s
            });
            return format!("{cm_name}:{cm_elem}");
        }
    }
    cm_name.to_string()
}

/// Check if a `TypeId` represents `Array<u8>`.
fn is_u8_array_type(type_id: TypeId, tt: &TypeTable) -> bool {
    let name = tt.type_name(type_id);
    name == "Array<u8>"
}

/// Recursively replace WASI-derived types with user types in the binding.
/// Given a WASI AST `Type` and the user's `TypeId`, compute the WASI-derived `TypeId`
/// and replace it, then recurse into sub-types (Array elements, Tuple fields, etc.).
fn replace_wasi_derived_type_recursive(
    adapter: &mut TirFunction,
    wasi_type: &Type,
    user_type: TypeId,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let old_type = {
        let mut tt = type_table.borrow_mut();
        wasi_type_to_type_id(wasi_type, &mut tt, wasi_registry, wasi_package)
    };
    if old_type != user_type && old_type != TypeTable::I32 && old_type != TypeTable::UNIT {
        // Skip replacement if the user type resolves to the same base type
        // after deep newtype resolution. Introducing newtypes into adapter
        // bodies creates monomorphization and WIR type lookup issues.
        let tt = type_table.borrow();
        let old_name = tt.mangle_type_name(old_type);
        let user_resolved_name = tt.mangle_type_name_resolving_newtypes(user_type);
        drop(tt);
        if old_name == user_resolved_name {
            // Same underlying type after resolving newtypes — no replacement needed
        } else {
            let tt = type_table.borrow();
            let new_name = tt.mangle_type_name(user_type);
            drop(tt);
            if old_name == new_name {
                replace_type_in_adapter(adapter, old_type, user_type);
            } else {
                replace_type_in_adapter_with_names(
                    adapter, old_type, user_type, &old_name, &new_name,
                );
            }
        }
    }
    match wasi_type {
        Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
            let tt = type_table.borrow();
            if let Some(new_elem_args) = tt.generic_type_args(user_type)
                && new_elem_args.len() == 1
            {
                let new_elem = new_elem_args[0];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_elem,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        Type::Tuple(elems) => {
            // Get the user's tuple field types
            if let Some(user_elems) = type_table.borrow().as_tuple(user_type) {
                for (wasi_elem, &user_elem) in elems.iter().zip(user_elems.iter()) {
                    replace_wasi_derived_type_recursive(
                        adapter,
                        wasi_elem,
                        user_elem,
                        wasi_registry,
                        wasi_package,
                        type_table,
                    );
                }
            }
        }
        Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
            let tt = type_table.borrow();
            if let Some(new_args) = tt.generic_type_args(user_type)
                && new_args.len() == 1
            {
                let new_inner = new_args[0];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_inner,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        Type::Generic(g) if g.name == "Result" && g.args.len() == 2 => {
            let tt = type_table.borrow();
            if let Some(new_args) = tt.generic_type_args(user_type)
                && new_args.len() == 2
            {
                let new_ok = new_args[0];
                let new_err = new_args[1];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_ok,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[1],
                    new_err,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        _ => {}
    }
}

/// Fix up WASI-derived types in the binding body to match the user's types.
///
/// The binding body uses `TypeIds` from `wasi_type_to_type_id` (e.g., `Array<Tuple<String, Array<u8>>>`).
/// The call site uses user types with newtype aliases (e.g., `Array<Tuple<FieldName, FieldValue>>`).
/// This function computes the WASI-derived `TypeId` for each param and replaces it in the body.
fn fixup_wasi_derived_types_in_adapter(
    adapter: &mut TirFunction,
    func_info: &crate::component_model::WasiFunctionInfo,
    call_args: &[TirExpr],
    user_return_type: TypeId,
    type_table: &RefCell<TypeTable>,
    wasi_registry: &crate::component_model::WasiRegistry,
    skip_self: bool,
) {
    let wasi_package = func_info.package.as_str();
    let params_iter: Box<dyn Iterator<Item = &(String, String, Type)>> = if skip_self {
        Box::new(func_info.params.iter().skip(1))
    } else {
        Box::new(func_info.params.iter())
    };
    for (i, (_param_name, _, param_type)) in params_iter.enumerate() {
        if i >= call_args.len() {
            break;
        }
        let new_type = call_args[i].type_id;
        // Resolve newtypes (e.g., FieldName → String) before computing WASI-derived TypeId
        let resolved = wasi_registry.resolve_type(param_type);
        replace_wasi_derived_type_recursive(
            adapter,
            &resolved,
            new_type,
            wasi_registry,
            wasi_package,
            type_table,
        );
    }
    // Also fix up return type's WASI-derived sub-types — but skip for CM bindings
    // that return non-flat types (tuples, variants). Their return TypeId was set
    // precisely by synthesis and should not be modified by the recursive type
    // replacement, which can produce different TypeIds for the same logical type
    // through different wasi_type_to_type_id resolution paths.
    if !adapter.is_cm_binding
        && let Some(return_type) = &func_info.return_type
    {
        let resolved = wasi_registry.resolve_type(return_type);
        replace_wasi_derived_type_recursive(
            adapter,
            &resolved,
            user_return_type,
            wasi_registry,
            wasi_package,
            type_table,
        );
    }
}

/// Fix up the return expression's type in the binding body to match the caller's
/// expected return type. The binding was created with placeholder `TypeId`s
/// (e.g., `TypeTable::I32`) that need to be corrected to actual Wado types.
fn fixup_return_type_in_body(adapter: &mut TirFunction, old_type: TypeId, new_type: TypeId) {
    if let Some(body) = &mut adapter.body {
        fixup_types_in_block(body, old_type, new_type, &mut adapter.local_types);
    }
}

/// Replace ALL occurrences of `old_type` with `new_type` throughout the binding's
/// body, locals, and params. Used when a param or return type is fixed up from
/// WASI-derived types to the user code's newtype aliases.
fn replace_type_in_adapter(adapter: &mut TirFunction, old_type: TypeId, new_type: TypeId) {
    if old_type == new_type {
        return;
    }
    // Don't replace the return type of CM binding adapters.
    // The return type was set by synthesis with precise TypeIds from the entry
    // module's TypeTable. Replacing it with a TypeId computed by wasi_type_to_type_id
    // (which may produce different TypeIds for Stream/Future/Result composition)
    // corrupts the type and causes WIR build failures.
    if !adapter.is_cm_binding && adapter.return_type == old_type {
        adapter.return_type = new_type;
    }
    // Fix params
    for param in &mut adapter.params {
        if param.type_id == old_type {
            param.type_id = new_type;
        }
    }
    // Fix local_types
    for lt in &mut adapter.local_types {
        if *lt == old_type {
            *lt = new_type;
        }
    }
    // Fix body
    if let Some(body) = &mut adapter.body {
        replace_type_in_block(body, old_type, new_type);
    }
}

/// Like `replace_type_in_adapter` but also renames function references that
/// contain the old type name to use the new type name. This is needed when
/// the binding body calls monomorphized functions like `Array<T>::with_capacity`
/// where T is a WASI-derived type that differs from the user's newtype alias.
fn replace_type_in_adapter_with_names(
    adapter: &mut TirFunction,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    if old_type == new_type {
        return;
    }
    // Don't replace return type of CM binding adapters (same as replace_type_in_adapter)
    if !adapter.is_cm_binding && adapter.return_type == old_type {
        adapter.return_type = new_type;
    }
    // Fix params
    for param in &mut adapter.params {
        if param.type_id == old_type {
            param.type_id = new_type;
        }
    }
    for lt in &mut adapter.local_types {
        if *lt == old_type {
            *lt = new_type;
        }
    }
    if let Some(body) = &mut adapter.body {
        replace_type_and_names_in_block(body, old_type, new_type, old_name, new_name);
    }
}

fn replace_type_and_names_in_block(
    block: &mut TirBlock,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    for stmt in &mut block.stmts {
        replace_type_and_names_in_stmt(stmt, old_type, new_type, old_name, new_name);
    }
}

fn replace_type_and_names_in_stmt(
    stmt: &mut TirStmt,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    match &mut stmt.kind {
        TirStmtKind::Expr(e) => {
            replace_type_and_names_in_expr(e, old_type, new_type, old_name, new_name);
        }
        TirStmtKind::Return { value: Some(e), .. } => {
            replace_type_and_names_in_expr(e, old_type, new_type, old_name, new_name);
        }
        TirStmtKind::Let { value, type_id, .. } => {
            if *type_id == old_type {
                *type_id = new_type;
            }
            replace_type_and_names_in_expr(value, old_type, new_type, old_name, new_name);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
            ..
        } => {
            replace_type_and_names_in_expr(condition, old_type, new_type, old_name, new_name);
            replace_type_and_names_in_block(then_block, old_type, new_type, old_name, new_name);
            if let Some(eb) = else_block {
                replace_type_and_names_in_block(eb, old_type, new_type, old_name, new_name);
            }
        }
        TirStmtKind::Loop { body, .. } => {
            replace_type_and_names_in_block(body, old_type, new_type, old_name, new_name);
        }
        _ => {}
    }
}

fn replace_type_and_names_in_expr(
    expr: &mut TirExpr,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    if expr.type_id == old_type {
        expr.type_id = new_type;
    }
    match &mut expr.kind {
        TirExprKind::Call { func, args, .. } => {
            if func.name.contains(old_name) {
                func.name = func.name.replace(old_name, new_name);
            }
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            for arg in args {
                replace_type_and_names_in_expr(
                    &mut arg.expr,
                    old_type,
                    new_type,
                    old_name,
                    new_name,
                );
            }
        }
        TirExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            if func.name.contains(old_name) {
                func.name = func.name.replace(old_name, new_name);
            }
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            replace_type_and_names_in_expr(receiver, old_type, new_type, old_name, new_name);
            for arg in args {
                replace_type_and_names_in_expr(
                    &mut arg.expr,
                    old_type,
                    new_type,
                    old_name,
                    new_name,
                );
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            replace_type_and_names_in_expr(inner, old_type, new_type, old_name, new_name);
        }
        TirExprKind::Assign { target, value } => {
            replace_type_and_names_in_expr(target, old_type, new_type, old_name, new_name);
            replace_type_and_names_in_expr(value, old_type, new_type, old_name, new_name);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_type_and_names_in_expr(left, old_type, new_type, old_name, new_name);
            replace_type_and_names_in_expr(right, old_type, new_type, old_name, new_name);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            replace_type_and_names_in_expr(inner, old_type, new_type, old_name, new_name);
        }
        TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } => {
            if *variant_type == old_type {
                *variant_type = new_type;
            }
            if let Some(p) = payload {
                replace_type_and_names_in_expr(p, old_type, new_type, old_name, new_name);
            }
        }
        TirExprKind::Block(blk) => {
            replace_type_and_names_in_block(blk, old_type, new_type, old_name, new_name);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                replace_type_and_names_in_expr(
                    &mut f.value,
                    old_type,
                    new_type,
                    old_name,
                    new_name,
                );
            }
        }
        _ => {}
    }
}

fn replace_type_in_block(block: &mut TirBlock, old_type: TypeId, new_type: TypeId) {
    for stmt in &mut block.stmts {
        replace_type_in_stmt(stmt, old_type, new_type);
    }
}

fn replace_type_in_stmt(stmt: &mut TirStmt, old_type: TypeId, new_type: TypeId) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, type_id, .. } => {
            if *type_id == old_type {
                *type_id = new_type;
            }
            replace_type_in_expr(value, old_type, new_type);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_type_in_expr(v, old_type, new_type);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_type_in_expr(condition, old_type, new_type);
            replace_type_in_block(then_block, old_type, new_type);
            if let Some(blk) = else_block {
                replace_type_in_block(blk, old_type, new_type);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_type_in_block(body, old_type, new_type);
        }
        TirStmtKind::Expr(expr) => {
            replace_type_in_expr(expr, old_type, new_type);
        }
        _ => {}
    }
}

fn replace_type_in_expr(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if expr.type_id == old_type {
        expr.type_id = new_type;
    }
    match &mut expr.kind {
        TirExprKind::Call { func, args, .. } => {
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            for arg in args {
                replace_type_in_expr(&mut arg.expr, old_type, new_type);
            }
        }
        TirExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            replace_type_in_expr(receiver, old_type, new_type);
            for arg in args {
                replace_type_in_expr(&mut arg.expr, old_type, new_type);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            replace_type_in_expr(inner, old_type, new_type);
        }
        TirExprKind::Assign { target, value } => {
            replace_type_in_expr(target, old_type, new_type);
            replace_type_in_expr(value, old_type, new_type);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_type_in_expr(left, old_type, new_type);
            replace_type_in_expr(right, old_type, new_type);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            replace_type_in_expr(inner, old_type, new_type);
        }
        TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } => {
            if *variant_type == old_type {
                *variant_type = new_type;
            }
            if let Some(p) = payload {
                replace_type_in_expr(p, old_type, new_type);
            }
        }
        TirExprKind::Block(blk) => {
            replace_type_in_block(blk, old_type, new_type);
        }
        _ => {}
    }
}

/// Recursively fix types in a block — replaces `old_type` with `new_type`
/// in return statements, let bindings, and expressions.
/// Also replaces `TypeTable::I32` placeholders with `new_type` (for cases
/// where the binding used I32 as a placeholder).
fn fixup_types_in_block(
    block: &mut TirBlock,
    old_type: TypeId,
    new_type: TypeId,
    local_types: &mut Vec<TypeId>,
) {
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            TirStmtKind::Return {
                value: Some(ret_expr),
            } => {
                fixup_expr_type(ret_expr, old_type, new_type);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                fixup_types_in_block(then_block, old_type, new_type, local_types);
                if let Some(blk) = else_block {
                    fixup_types_in_block(blk, old_type, new_type, local_types);
                }
            }
            TirStmtKind::Loop { body } => {
                fixup_types_in_block(body, old_type, new_type, local_types);
            }
            TirStmtKind::Let {
                value,
                local_index,
                type_id,
                ..
            } => {
                let idx = *local_index;
                fixup_adapter_let(value, idx, old_type, new_type, type_id, local_types);
            }
            TirStmtKind::Expr(expr) => {
                fixup_adapter_expr(expr, old_type, new_type);
            }
            _ => {}
        }
    }
}

/// Fix up an expression in a Let statement that might hold adapter intermediate values.
fn fixup_adapter_let(
    expr: &mut TirExpr,
    local_index: u32,
    old_type: TypeId,
    new_type: TypeId,
    let_type_id: &mut TypeId,
    local_types: &mut [TypeId],
) {
    let should_fix = expr.type_id == TypeTable::I32 || expr.type_id == old_type;
    match &mut expr.kind {
        TirExprKind::Call { func, .. } if func.method_info.is_some() => {
            if should_fix {
                expr.type_id = new_type;
                *let_type_id = new_type;
                if (local_index as usize) < local_types.len() {
                    local_types[local_index as usize] = new_type;
                }
            }
        }
        TirExprKind::Null => {
            if should_fix {
                expr.type_id = new_type;
                *let_type_id = new_type;
                if (local_index as usize) < local_types.len() {
                    local_types[local_index as usize] = new_type;
                }
            }
        }
        _ => {}
    }
}

/// Fix up an expression statement (e.g., Assign with `VariantConstruct`).
fn fixup_adapter_expr(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if let TirExprKind::Assign { target, value } = &mut expr.kind {
        fixup_variant_construct(value, old_type, new_type);
        if target.type_id == TypeTable::I32 || target.type_id == old_type {
            target.type_id = new_type;
        }
    }
}

/// Fix up `VariantConstruct` expressions to use the real type.
fn fixup_variant_construct(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if let TirExprKind::VariantConstruct { variant_type, .. } = &mut expr.kind {
        if *variant_type == TypeTable::I32 || *variant_type == old_type {
            *variant_type = new_type;
        }
        if expr.type_id == TypeTable::I32 || expr.type_id == old_type {
            expr.type_id = new_type;
        }
    }
}

/// Recursively fix the `type_id` of an expression and its leaf nodes.
fn fixup_expr_type(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if expr.type_id == old_type || expr.type_id == TypeTable::I32 {
        expr.type_id = new_type;
    }
    match &mut expr.kind {
        TirExprKind::TupleLiteral { .. } | TirExprKind::Call { .. } | TirExprKind::Local { .. } => {
        }
        TirExprKind::VariantConstruct { variant_type, .. } => {
            if *variant_type == TypeTable::I32 || *variant_type == old_type {
                *variant_type = new_type;
            }
        }
        _ => {}
    }
}

/// Flatten a Wado-level arg into flat CM ABI args at the call site.
///
/// For multi-flat types like `Option<T>`, the Wado-level arg (e.g., `null`)
/// is expanded into multiple i32 args (discriminant + payload).
fn flatten_arg_for_call_site(arg: &TirExpr, flat_tys: &[TypeId], flat_args: &mut Vec<TirExpr>) {
    // Unwrap Cast nodes transparently
    let inner = match &arg.kind {
        TirExprKind::Cast { expr, .. } => expr.as_ref(),
        _ => arg,
    };
    match &inner.kind {
        // null literal → discriminant=0, payload=0 for each flat type
        TirExprKind::Null => {
            for _ in flat_tys {
                flat_args.push(i32_const(0));
            }
        }
        // VariantConstruct None → discriminant=0, payload=0 for each flat type
        TirExprKind::VariantConstruct {
            case_name,
            payload: None,
            ..
        } if case_name == "None" => {
            for _ in flat_tys {
                flat_args.push(i32_const(0));
            }
        }
        // VariantConstruct Some(value) → discriminant=1, then flatten inner value
        TirExprKind::VariantConstruct {
            case_name,
            payload: Some(value),
            ..
        } if case_name == "Some" => {
            flat_args.push(i32_const(1));
            let remaining = &flat_tys[1..];
            if remaining.len() == 1 {
                // Single-value payload: pass through (e.g., enum discriminant)
                flatten_arg_for_call_site(value, remaining, flat_args);
            } else {
                // Multi-value payload (e.g., String → ptr+len): pass through as-is
                // The binding will lower it internally
                flat_args.push((**value).clone());
                for _ in 2..flat_tys.len() {
                    flat_args.push(i32_const(0));
                }
            }
        }
        // For any other expression, this is an arbitrary Option<T> value.
        // Currently not supported — would need runtime null-check logic.
        _ => {
            panic!(
                "StaticCall adapter: cannot flatten arg of kind {:?} into {} flat types at call site; \
                 only null and VariantConstruct literals are supported",
                inner.kind,
                flat_tys.len()
            );
        }
    }
}

/// Collect local type updates from Let stmts that were modified by the rewrite.
/// This is needed because the lower phase pre-populates `local_types`, and the streaming
/// adapter rewrite changes Let binding types from Result<..> to i32.
fn collect_local_type_updates(
    block: &TirBlock,
    local_types: &[TypeId],
    updates: &mut Vec<(usize, TypeId)>,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                type_id,
                ..
            } => {
                let idx = *local_index as usize;
                if idx < local_types.len() && local_types[idx] != *type_id {
                    updates.push((idx, *type_id));
                }
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            }
            | TirStmtKind::IfLet {
                then_block,
                else_block,
                ..
            } => {
                collect_local_type_updates(then_block, local_types, updates);
                if let Some(blk) = else_block {
                    collect_local_type_updates(blk, local_types, updates);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                collect_local_type_updates(body, local_types, updates);
            }
            _ => {}
        }
    }
}

fn rewrite_calls_in_block(
    block: &mut TirBlock,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    for stmt in &mut block.stmts {
        rewrite_calls_in_stmt(stmt, adapters, entry_source, wasi_registry, type_table);
    }
}

fn rewrite_calls_in_stmt(
    stmt: &mut TirStmt,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, type_id, .. } => {
            let old_type = value.type_id;
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
            // If the expression type changed (e.g., streaming adapter returns i32
            // instead of Result<(), ErrorCode>), update the let binding's type.
            if value.type_id != old_type {
                *type_id = value.type_id;
            }
        }
        TirStmtKind::Expr(value) => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_block(
                then_block,
                adapters,
                entry_source,
                wasi_registry,
                type_table,
            );
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_calls_in_block(body, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_block(
                then_block,
                adapters,
                entry_source,
                wasi_registry,
                type_table,
            );
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::Continue => {}
        TirStmtKind::TaskReturn { value } => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn rewrite_calls_in_expr(
    expr: &mut TirExpr,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    // Check if this is an effect-like Call that should be rewritten
    let is_effect_call = matches!(&expr.kind, TirExprKind::Call { func, .. }
        if func.module_source.clone().is_effect_like() && func.module_source.clone().effect_name().is_some());
    if is_effect_call
        && let TirExprKind::Call {
            func,
            args,
            type_args,
            ..
        } = &mut expr.kind
    {
        let effect_name = func.module_source.clone().effect_name().unwrap_or_default();
        let method_name = func.name.clone();
        let qualified = format!("{effect_name}::{method_name}");

        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Check if this is a streaming async function
            let is_streaming = wasi_registry
                .get_function(&qualified)
                .is_some_and(|f| !f.is_async && f.has_streaming_param());

            // Fix up binding function types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if is_streaming {
                    // Streaming adapters return i32 (Future handle) at WIR level.
                    // Keep the caller's original Future type for Wado-level type
                    // checking and CM method dispatch (.drop(), .read()).
                    // WIR flattens Future<T> (GenericResource) to i32 automatically.
                } else if adapter.return_type != expr.type_id {
                    let old_return_type = adapter.return_type;
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, old_return_type, expr.type_id);
                }
                let wasi_func = wasi_registry.get_function(&qualified);
                for (i, arg) in args.iter().enumerate() {
                    if i < adapter.params.len() && adapter.params[i].type_id != arg.expr.type_id {
                        let is_gc_passthrough = wasi_func.is_some_and(|f| {
                            i < f.params.len()
                                && is_gc_passthrough_param(&f.params[i].2, wasi_registry)
                        });
                        if is_streaming && adapter.params[i].type_id == TypeTable::I32 {
                            // Streaming: keep adapter param as i32, cast the arg instead
                        } else if !is_gc_passthrough && is_wasm_flat_type(adapter.params[i].type_id)
                        {
                            // Non-GC flat param: adapter type is authoritative, cast arg.
                        } else {
                            let local_idx = adapter.params[i].local_index as usize;
                            adapter.params[i].type_id = arg.expr.type_id;
                            if local_idx < adapter.local_types.len() {
                                adapter.local_types[local_idx] = arg.expr.type_id;
                            }
                        }
                    }
                }
            }

            // Cast call-site args to match adapter param types when they differ
            // (e.g., i32 literal → i64 for Duration newtype)
            {
                let adapter = adapter_rc.borrow();
                for (i, arg) in args.iter_mut().enumerate() {
                    if i < adapter.params.len()
                        && adapter.params[i].type_id != arg.expr.type_id
                        && is_wasm_flat_type(adapter.params[i].type_id)
                    {
                        let target = adapter.params[i].type_id;
                        let original = std::mem::replace(
                            &mut arg.expr,
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        );
                        arg.expr = cast(original, target);
                    }
                }
            }

            // For streaming adapters, cast GC ref args to i32
            if is_streaming {
                for arg in args.iter_mut() {
                    if arg.expr.type_id != TypeTable::I32 {
                        let original = std::mem::replace(
                            &mut arg.expr,
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        );
                        arg.expr = cast(original, TypeTable::I32);
                    }
                }
            }

            // Rewrite to call the binding function
            *func = FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone());
            *type_args = vec![];

            // Recurse into args
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
            return;
        }
    }

    // Check if this is a resource MethodCall that should be rewritten to target a binding
    if let TirExprKind::MethodCall { func, .. } = &expr.kind
        && let Some(method_info) = func.method_info.clone()
    {
        let mut qualified = format!(
            "{}::{}",
            method_info.base_struct_name, method_info.method_name
        );
        // Resolve through type aliases (e.g., Headers -> Fields). Scoped to
        // `wasi:` — the method resolution path is WASI-only.
        if !adapters.contains_key(&qualified)
            && let Some(source) =
                wasi_registry.find_wasi_newtype_source(&method_info.base_struct_name)
            && let Some(Type::Named(resolved)) =
                wasi_registry.get_newtype_by_source(source, &method_info.base_struct_name)
        {
            let aliased = format!("{}::{}", resolved.name, method_info.method_name);
            if adapters.contains_key(&aliased) {
                qualified = aliased;
            }
        }
        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Check if this is a streaming async function
            let is_streaming = wasi_registry
                .get_function(&qualified)
                .is_some_and(|f| !f.is_async && f.has_streaming_param());

            // Extract receiver and args before replacing
            let (taken_receiver, mut taken_args) =
                if let TirExprKind::MethodCall { receiver, args, .. } = &mut expr.kind {
                    (
                        std::mem::replace(
                            receiver.as_mut(),
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        ),
                        std::mem::take(args),
                    )
                } else {
                    unreachable!()
                };

            // Fix up binding function types from the call site
            // The binding params include self as the first param
            {
                let mut adapter = adapter_rc.borrow_mut();
                if is_streaming {
                    // Streaming adapters return i32 (Future handle) at WIR level.
                    // Keep the caller's original Future type for Wado-level type
                    // checking and CM method dispatch (.drop(), .read()).
                    // WIR flattens Future<T> (GenericResource) to i32 automatically.
                } else if adapter.return_type != expr.type_id {
                    let old_return_type = adapter.return_type;
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, old_return_type, expr.type_id);
                }
                // Fix up self param (index 0) from the receiver
                if !adapter.params.is_empty() && adapter.params[0].type_id != taken_receiver.type_id
                {
                    let local_idx = adapter.params[0].local_index as usize;
                    adapter.params[0].type_id = taken_receiver.type_id;
                    if local_idx < adapter.local_types.len() {
                        adapter.local_types[local_idx] = taken_receiver.type_id;
                    }
                }
                // Fix up remaining params from the args
                let method_func = wasi_registry.get_function(&qualified);
                for (i, arg) in taken_args.iter().enumerate() {
                    let param_idx = i + 1; // +1 to skip self
                    if param_idx < adapter.params.len()
                        && adapter.params[param_idx].type_id != arg.expr.type_id
                    {
                        // For method calls, WASI params include self at index 0, so
                        // the i-th arg corresponds to WASI param index i+1
                        let wasi_param_idx = i + 1;
                        let is_gc_passthrough = method_func.is_some_and(|f| {
                            wasi_param_idx < f.params.len()
                                && is_gc_passthrough_param(
                                    &f.params[wasi_param_idx].2,
                                    wasi_registry,
                                )
                        });
                        if is_streaming && adapter.params[param_idx].type_id == TypeTable::I32 {
                            // Streaming: keep adapter param as i32, cast the arg instead
                        } else if !is_gc_passthrough
                            && is_wasm_flat_type(adapter.params[param_idx].type_id)
                        {
                            // Non-GC flat param: adapter type is authoritative, cast arg.
                        } else {
                            let local_idx = adapter.params[param_idx].local_index as usize;
                            adapter.params[param_idx].type_id = arg.expr.type_id;
                            if local_idx < adapter.local_types.len() {
                                adapter.local_types[local_idx] = arg.expr.type_id;
                            }
                        }
                    }
                }
                // Replace WASI-derived types in the body (including function names).
                // Skip for CM bindings — their types were set precisely by synthesis
                // and the recursive replacement can produce TypeId mismatches for
                // complex return types like [Stream<T>, Future<Result<_, E>>].
                if !adapter.is_cm_binding
                    && let Some(func_info) = wasi_registry.get_function(&qualified)
                {
                    let call_args: Vec<TirExpr> =
                        taken_args.iter().map(|a| a.expr.clone()).collect();
                    fixup_wasi_derived_types_in_adapter(
                        &mut adapter,
                        func_info,
                        &call_args,
                        expr.type_id,
                        type_table,
                        wasi_registry,
                        true, // skip_self: call_args excludes self
                    );
                }
            }

            // Cast call-site args to match adapter param types when they differ
            {
                let adapter = adapter_rc.borrow();
                for (i, arg) in taken_args.iter_mut().enumerate() {
                    let param_idx = i + 1; // +1 to skip self
                    if param_idx < adapter.params.len()
                        && adapter.params[param_idx].type_id != arg.expr.type_id
                        && is_wasm_flat_type(adapter.params[param_idx].type_id)
                    {
                        let target = adapter.params[param_idx].type_id;
                        let original = std::mem::replace(
                            &mut arg.expr,
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        );
                        arg.expr = cast(original, target);
                    }
                }
            }

            // Flatten call site args to match the binding's flat CM params.
            // For method calls, self is the first param; remaining args may need flattening.
            let method_func = wasi_registry.get_function(&qualified);
            let flat_taken_args = if let Some(func_info) = method_func {
                let mut flat = Vec::new();
                for (i, arg) in taken_args.iter().enumerate() {
                    // WASI param index: i+1 to skip self
                    let wasi_param_idx = i + 1;
                    if wasi_param_idx >= func_info.params.len() {
                        flat.push(arg.expr.clone());
                        continue;
                    }
                    let param_type = &func_info.params[wasi_param_idx].2;
                    let flat_tys = flatten_param_type(param_type, wasi_registry);
                    if flat_tys.is_empty() {
                        continue;
                    }
                    if is_gc_passthrough_param(param_type, wasi_registry) {
                        if matches!(arg.expr.kind, TirExprKind::Null) {
                            let option_type_id = {
                                let mut tt = type_table.borrow_mut();
                                wasi_type_to_type_id(
                                    param_type,
                                    &mut tt,
                                    wasi_registry,
                                    &func_info.package,
                                )
                            };
                            flat.push(option_none(option_type_id));
                        } else {
                            flat.push(arg.expr.clone());
                        }
                    } else if flat_tys.len() == 1 {
                        flat.push(arg.expr.clone());
                    } else {
                        flatten_arg_for_call_site(&arg.expr, &flat_tys, &mut flat);
                    }
                }
                flat
            } else {
                taken_args.into_iter().map(|a| a.expr).collect()
            };

            // Replace MethodCall with Call targeting the binding
            // Prepend receiver to args
            let mut all_args = vec![taken_receiver];
            all_args.extend(flat_taken_args);

            expr.kind = TirExprKind::Call {
                func: FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone()),
                args: all_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(
                        &mut arg.expr,
                        adapters,
                        entry_source,
                        wasi_registry,
                        type_table,
                    );
                }
            }
            return;
        }
    }

    // Check if this is a resource static Call (with method_info) that should be rewritten to target a binding
    if let TirExprKind::Call { func, .. } = &expr.kind
        && func.method_info.is_some()
    {
        let func_name = func.name.clone();
        if let Some(adapter_rc) = adapters.get(&func_name) {
            // Look up WASI function info to flatten args at the call site
            let wasi_func_info = wasi_registry.get_function(&func_name).cloned();

            // Extract args before replacing
            let taken_args = if let TirExprKind::Call { args, .. } = &mut expr.kind {
                std::mem::take(args)
                    .into_iter()
                    .map(|a| a.expr)
                    .collect::<Vec<_>>()
            } else {
                unreachable!()
            };

            // Fix up adapter return type and param types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if adapter.return_type != expr.type_id {
                    let old_return_type = adapter.return_type;
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, old_return_type, expr.type_id);
                }
                // Fix up adapter param types for GC pass-through params (String,
                // Array<T>) where the binding receives the GC value directly.
                // Do NOT fix up params that get flattened (Option, resource handles)
                // because taken_args indices don't match flat adapter param indices.
                if let Some(func_info) = &wasi_func_info {
                    let mut flat_idx = 0;
                    for (i, (_name, _, param_type)) in func_info.params.iter().enumerate() {
                        let is_gc_passthrough = matches!(
                            param_type,
                            Type::Named(n) if n.name == "String"
                        ) || matches!(
                            param_type,
                            Type::Generic(g) if g.name == "Array" && g.args.len() == 1
                        );
                        if is_gc_passthrough {
                            if flat_idx < adapter.params.len()
                                && i < taken_args.len()
                                && adapter.params[flat_idx].type_id != taken_args[i].type_id
                            {
                                let local_idx = adapter.params[flat_idx].local_index as usize;
                                adapter.params[flat_idx].type_id = taken_args[i].type_id;
                                if local_idx < adapter.local_types.len() {
                                    adapter.local_types[local_idx] = taken_args[i].type_id;
                                }
                            }
                            flat_idx += 1;
                        } else {
                            let flat_tys = flatten_param_type(param_type, wasi_registry);
                            flat_idx += flat_tys.len().max(1);
                        }
                    }
                }
                // Replace WASI-derived types in the body with the user's types.
                // Skip for CM bindings (same reason as above).
                if !adapter.is_cm_binding
                    && let Some(func_info) = &wasi_func_info
                {
                    fixup_wasi_derived_types_in_adapter(
                        &mut adapter,
                        func_info,
                        &taken_args,
                        expr.type_id,
                        type_table,
                        wasi_registry,
                        false, // skip_self: static calls have no self
                    );
                }
            }

            // Flatten call site args to match the binding's flat CM params.
            // GC passthrough types (String, Array<u8>, Option<T>) are passed
            // through as GC refs — the binding body handles lowering.
            // Other multi-flat types are flattened here into individual i32 args.
            let flat_call_args = if let Some(func_info) = &wasi_func_info {
                let mut flat = Vec::new();
                for (i, (_param_name, _, param_type)) in func_info.params.iter().enumerate() {
                    let flat_tys = flatten_param_type(param_type, wasi_registry);
                    if flat_tys.is_empty() || i >= taken_args.len() {
                        continue;
                    }
                    if is_gc_passthrough_param(param_type, wasi_registry) {
                        let arg = &taken_args[i];
                        // Convert bare Null to VariantConstruct None.
                        // Use the binding's param type (from the WASI registry)
                        // to get a properly-resolved type_id, since the
                        // source null's type_id may have unknown inner type.
                        if matches!(arg.kind, TirExprKind::Null) {
                            let option_type_id = {
                                let mut tt = type_table.borrow_mut();
                                wasi_type_to_type_id(
                                    param_type,
                                    &mut tt,
                                    wasi_registry,
                                    &func_info.package,
                                )
                            };
                            flat.push(option_none(option_type_id));
                        } else {
                            flat.push(arg.clone());
                        }
                    } else if flat_tys.len() == 1 {
                        flat.push(taken_args[i].clone());
                    } else {
                        flatten_arg_for_call_site(&taken_args[i], &flat_tys, &mut flat);
                    }
                }
                flat
            } else {
                // No WASI function info: pass args as-is (fallback)
                taken_args
            };

            // Replace static Call with Call targeting the binding
            expr.kind = TirExprKind::Call {
                func: FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone()),
                args: flat_call_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(
                        &mut arg.expr,
                        adapters,
                        entry_source,
                        wasi_registry,
                        type_table,
                    );
                }
            }
            return;
        }
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Call { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_calls_in_expr(receiver, adapters, entry_source, wasi_registry, type_table);
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_calls_in_expr(callee, adapters, entry_source, wasi_registry, type_table);
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_calls_in_block(block, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_calls_in_expr(left, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_expr(right, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. } => {
            rewrite_calls_in_expr(inner, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_block(
                then_branch,
                adapters,
                entry_source,
                wasi_registry,
                type_table,
            );
            if let Some(blk) = else_branch {
                rewrite_calls_in_block(blk, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            rewrite_calls_in_expr(e, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_expr(index, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source, wasi_registry, type_table);
            for arm in arms {
                rewrite_calls_in_block(arm, adapters, entry_source, wasi_registry, type_table);
            }
            rewrite_calls_in_block(default, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source, wasi_registry, type_table);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rewrite_calls_in_expr(guard, adapters, entry_source, wasi_registry, type_table);
                }
                rewrite_calls_in_expr(
                    &mut arm.body,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_calls_in_expr(body, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_calls_in_expr(functor, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in &mut fields.iter_mut() {
                rewrite_calls_in_expr(
                    &mut field.value,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        _ => {} // Leaf nodes: no sub-expressions
    }
}

fn collect_effect_calls_in_block(
    block: &TirBlock,
    effects: &mut IndexSet<String>,
    wasi_registry: &crate::component_model::WasiRegistry,
) {
    for stmt in &block.stmts {
        collect_effect_calls_in_stmt(stmt, effects, wasi_registry);
    }
}

fn collect_effect_calls_in_stmt(
    stmt: &TirStmt,
    effects: &mut IndexSet<String>,
    wasi_registry: &crate::component_model::WasiRegistry,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects, wasi_registry);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_effect_calls_in_expr(condition, effects, wasi_registry);
            collect_effect_calls_in_block(then_block, effects, wasi_registry);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects, wasi_registry);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_effect_calls_in_block(body, effects, wasi_registry);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects, wasi_registry);
            collect_effect_calls_in_block(then_block, effects, wasi_registry);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects, wasi_registry);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects, wasi_registry);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirStmtKind::Continue => {}
        TirStmtKind::TaskReturn { value } => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn collect_effect_calls_in_expr(
    expr: &TirExpr,
    effects: &mut IndexSet<String>,
    wasi_registry: &crate::component_model::WasiRegistry,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            // Collect effect-like Call nodes (sync WASI calls like Environment::get_arguments)
            if func.module_source.clone().is_effect_like()
                && let Some(effect_name) = func.module_source.clone().effect_name()
            {
                let method_name = func.name.clone();
                let qualified = format!("{effect_name}::{method_name}");
                if wasi_registry.get_function(&qualified).is_some() {
                    effects.insert(qualified);
                }
            }
            // Also check if this is a WASI resource static method call (e.g., Response::new)
            if func.method_info.is_some() {
                let func_name = func.name.clone();
                if wasi_registry.get_function(&func_name).is_some() {
                    effects.insert(func_name);
                }
            }
            for arg in args {
                collect_effect_calls_in_expr(&arg.expr, effects, wasi_registry);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_effect_calls_in_expr(arg, effects, wasi_registry);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            // Check if this is a WASI resource method call
            if let Some(method_info) = func.method_info.clone() {
                let qualified = format!(
                    "{}::{}",
                    method_info.base_struct_name, method_info.method_name
                );
                if wasi_registry.get_function(&qualified).is_some() {
                    effects.insert(qualified);
                } else if let Some(source) =
                    wasi_registry.find_wasi_newtype_source(&method_info.base_struct_name)
                    && let Some(Type::Named(resolved)) =
                        wasi_registry.get_newtype_by_source(source, &method_info.base_struct_name)
                {
                    // Resolve through type aliases (e.g., Headers -> Fields)
                    let aliased = format!("{}::{}", resolved.name, method_info.method_name);
                    if wasi_registry.get_function(&aliased).is_some() {
                        effects.insert(aliased);
                    }
                }
            }
            collect_effect_calls_in_expr(receiver, effects, wasi_registry);
            for arg in args {
                collect_effect_calls_in_expr(&arg.expr, effects, wasi_registry);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_effect_calls_in_expr(callee, effects, wasi_registry);
            for arg in args {
                collect_effect_calls_in_expr(arg, effects, wasi_registry);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_effect_calls_in_block(block, effects, wasi_registry);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_effect_calls_in_expr(left, effects, wasi_registry);
            collect_effect_calls_in_expr(right, effects, wasi_registry);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_effect_calls_in_expr(inner, effects, wasi_registry);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_effect_calls_in_expr(condition, effects, wasi_registry);
            collect_effect_calls_in_block(then_branch, effects, wasi_registry);
            if let Some(blk) = else_branch {
                collect_effect_calls_in_block(blk, effects, wasi_registry);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            collect_effect_calls_in_expr(e, effects, wasi_registry);
            collect_effect_calls_in_expr(index, effects, wasi_registry);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects, wasi_registry);
            for arm in arms {
                collect_effect_calls_in_block(arm, effects, wasi_registry);
            }
            collect_effect_calls_in_block(default, effects, wasi_registry);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            collect_effect_calls_in_expr(scrutinee, effects, wasi_registry);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_effect_calls_in_expr(guard, effects, wasi_registry);
                }
                collect_effect_calls_in_expr(&arm.body, effects, wasi_registry);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_effect_calls_in_expr(body, effects, wasi_registry);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_effect_calls_in_expr(functor, effects, wasi_registry);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_effect_calls_in_expr(&field.value, effects, wasi_registry);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Local { .. }
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Capture { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        // Catch-all for any remaining leaf or rare variants
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NamedType;
    use crate::component_model::WasiRegistry;

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            span: synth_span(),
            source_interface: None,
        })
    }

    #[test]
    fn flatten_param_i32() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i32"), &reg),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_i64() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i64"), &reg),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn flatten_param_f64() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("f64"), &reg),
            vec![TypeTable::F64]
        );
    }

    #[test]
    fn flatten_param_string() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("String"), &reg),
            vec![TypeTable::I32, TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_bool() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("bool"), &reg),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_unit() {
        let reg = WasiRegistry::new();
        assert!(flatten_param_type(&Type::Tuple(vec![]), &reg).is_empty());
    }

    #[test]
    fn flatten_param_newtype_u64() {
        let (reg, _) = WasiRegistry::build_from_stdlib();
        assert_eq!(
            flatten_param_type(&named_type("Duration"), reg),
            vec![TypeTable::I64]
        );
        assert_eq!(
            flatten_param_type(&named_type("Mark"), reg),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn binding_name() {
        assert_eq!(
            binding_func_name("Stdout", "write_via_stream"),
            "__cm_binding__Stdout_write_via_stream"
        );
    }

    #[test]
    fn lift_i32() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let expr = synthesize_lift(
            &named_type("i32"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut local_types,
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
        assert!(stmts.is_empty()); // primitives need no setup
    }

    #[test]
    fn lift_bool() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let expr = synthesize_lift(
            &named_type("bool"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut local_types,
        );
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_string() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let expr = synthesize_lift(
            &named_type("String"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut local_types,
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_list_i32() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 0_u32;
        let list_ty = cm_abi::generic_type("Array", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &list_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut local_types,
        );
        // Should produce setup stmts and return a local ref
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 5); // base, count, result, i, elem_addr
    }

    #[test]
    fn lift_option_i32() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 0_u32;
        let opt_ty = cm_abi::generic_type("Option", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &opt_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut local_types,
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 2); // disc, result
    }

    #[test]
    fn lift_result_unit_unit() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 0_u32;
        let result_ty =
            cm_abi::generic_type("Result", vec![Type::Tuple(vec![]), Type::Tuple(vec![])]);
        let expr = synthesize_lift(
            &result_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut local_types,
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
    }

    #[test]
    fn lift_resource_handle() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let own_ty = cm_abi::generic_type("Own", vec![named_type("Fields")]);
        let expr = synthesize_lift(
            &own_ty,
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut local_types,
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lower_i32() {
        let stmts = synthesize_lower(
            &named_type("i32"),
            i32_const(42),
            i32_const(100),
            &mut 0,
            &mut vec![],
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_bool() {
        let value = TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            synth_span(),
        );
        let stmts = synthesize_lower(
            &named_type("bool"),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_unit() {
        let value = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
        let stmts = synthesize_lower(
            &Type::Tuple(vec![]),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
        );
        assert!(stmts.is_empty());
    }

    #[test]
    fn lower_string() {
        let value = TirExpr::new(
            TirExprKind::StringLiteral("hello".to_string()),
            TypeTable::I32, // placeholder
            synth_span(),
        );
        let mut next_local = 10_u32;
        let stmts = synthesize_lower(
            &named_type("String"),
            value,
            i32_const(100),
            &mut next_local,
            &mut vec![],
        );
        // Should produce: let __packed = cm_lower_string(value); store ptr; store len
        assert_eq!(stmts.len(), 3);
        assert_eq!(next_local, 11); // one local allocated for __packed
    }

    #[test]
    fn helpers_i32_const() {
        let expr = i32_const(42);
        assert_eq!(expr.type_id, TypeTable::I32);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 42),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_i64_const() {
        let expr = i64_const(123);
        assert_eq!(expr.type_id, TypeTable::I64);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 123),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_builtin_call() {
        let call = builtin_call("i32_load", vec![i32_const(0)], TypeTable::I32);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "i32_load");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_internal_call() {
        let call = internal_call("cm_lower_string", vec![i32_const(0)], TypeTable::I64);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "cm_lower_string");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_cm_raw_call() {
        let call = cm_raw_call(
            "wasi:cli/Stdout::write_via_stream",
            vec![i32_const(0), i32_const(1), i32_const(2)],
            TypeTable::I32,
        );
        match &call.kind {
            TirExprKind::CmRawCall { local_name, args } => {
                assert_eq!(local_name, "wasi:cli/Stdout::write_via_stream");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected CmRawCall, got {other:?}"),
        }
    }

    #[test]
    fn helpers_let_stmt() {
        let stmt = let_stmt("x", 0, TypeTable::I32, i32_const(42));
        match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                type_id,
                ..
            } => {
                assert_eq!(name, "x");
                assert_eq!(*local_index, 0);
                assert_eq!(*type_id, TypeTable::I32);
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    // ---- Parameter lifting tests ----

    #[test]
    fn param_needs_lifting_string() {
        let modules = IndexMap::default();
        assert!(param_needs_lifting(&named_type("String"), &modules));
    }

    #[test]
    fn param_needs_lifting_bool() {
        let modules = IndexMap::default();
        assert!(param_needs_lifting(&named_type("bool"), &modules));
    }

    #[test]
    fn param_needs_lifting_i32() {
        let modules = IndexMap::default();
        assert!(!param_needs_lifting(&named_type("i32"), &modules));
    }

    #[test]
    fn param_needs_lifting_resource() {
        // With no TIR modules to register `Request` as a struct/variant,
        // the param falls through to resource-handle semantics.
        let modules = IndexMap::default();
        assert!(!param_needs_lifting(&named_type("Request"), &modules));
    }

    #[test]
    fn param_needs_lifting_option() {
        let ty = cm_abi::generic_type("Option", vec![named_type("i32")]);
        let modules = IndexMap::default();
        assert!(param_needs_lifting(&ty, &modules));
    }

    #[test]
    fn param_needs_lifting_array() {
        let ty = cm_abi::generic_type("Array", vec![named_type("i32")]);
        let modules = IndexMap::default();
        assert!(param_needs_lifting(&ty, &modules));
    }

    #[test]
    fn export_needs_lifting_empty() {
        let modules = IndexMap::default();
        assert!(!export_needs_param_lifting(&[], &modules));
    }

    #[test]
    fn export_needs_lifting_primitives_only() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("b".to_string(), named_type("f64")),
        ];
        let modules = IndexMap::default();
        assert!(!export_needs_param_lifting(&params, &modules));
    }

    #[test]
    fn export_needs_lifting_with_string() {
        let params = vec![("name".to_string(), named_type("String"))];
        let modules = IndexMap::default();
        assert!(export_needs_param_lifting(&params, &modules));
    }

    #[test]
    fn compute_flat_params_empty() {
        let params: Vec<(String, Type)> = vec![];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert!(flat.is_empty());
    }

    #[test]
    fn compute_flat_params_primitives() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("b".to_string(), named_type("f64")),
        ];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::F64]);
    }

    #[test]
    fn compute_flat_params_string() {
        let params = vec![("name".to_string(), named_type("String"))];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::I32]);
    }

    #[test]
    fn compute_flat_params_mixed() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("name".to_string(), named_type("String")),
            ("b".to_string(), named_type("f32")),
        ];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(
            flat,
            vec![
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::F32,
            ]
        );
    }

    // ---- Lift from flat params tests ----

    #[test]
    fn lift_from_flat_i32() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 1_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("i32"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut local_types,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lift_from_flat_string() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 2_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("String"),
            &[0, 1],
            &[cm_abi::CmValType::I32, cm_abi::CmValType::I32],
            TypeTable::I32, // placeholder
            &mut next_local,
            &mut stmts,
            &mut local_types,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 2);
        // Should be a call to memory_to_gc_string
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_from_flat_bool() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 1_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("bool"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::BOOL,
            &mut next_local,
            &mut stmts,
            &mut local_types,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_from_flat_unit() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 0_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &Type::Tuple(vec![]),
            &[],
            &[],
            TypeTable::UNIT,
            &mut next_local,
            &mut stmts,
            &mut local_types,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 0);
        assert!(matches!(expr.kind, TirExprKind::Unit));
    }

    #[test]
    fn lift_from_flat_resource() {
        let mut stmts = Vec::new();
        let mut local_types = Vec::new();
        let mut next_local = 1_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("Request"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut local_types,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }
}
