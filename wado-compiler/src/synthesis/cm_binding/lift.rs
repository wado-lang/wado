//! CM ABI lift: load a Wado value from linear memory.
//!
//! [`synthesize_lift`] produces TIR that materialises a Wado value at a given
//! linear-memory address. Composite types (lists, options, results, tuples,
//! WASI variants/enums/records) materialise into mutable locals; primitives
//! stay as plain `*_load` builtins. The list and tuple paths free the
//! underlying buffers as they read, so callers don't need to clean up after a
//! lift.

use crate::ast::{NamedType, Type};
use crate::cm_abi;
use crate::component_model::CmVariantCase;
use crate::module_source::ModuleSource;
use crate::tir::{
    TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirLocal, TirStmt, TirStructField, TypeId,
    TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, expr_stmt, generic_method_call,
    generic_static_call, i32_const, if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref,
    loop_stmt, null_expr, option_none, option_some, synth_span,
};

use super::types::{
    LiftContext, binary_add, canonical_wasi_package, cm_flags_byte_size, is_unit_type,
    kebab_to_pascal, module_source_for_cm_interface, wasi_type_to_type_id,
};

/// Synthesize a TIR expression that loads a CM value from linear memory.
///
/// For primitives, returns a single expression (no setup statements).
/// For composites (list, option, result), emits setup statements into `stmts`
/// and returns a local reference to the lifted value. The CM/WASI registry on
/// `ctx` is used to resolve named struct/variant/enum/flags types and to
/// compute their canonical-ABI layout.
///
/// `next_local` / `locals` track local variable allocation.
pub fn synthesize_lift(
    ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    synthesize_lift_inner(ty, addr, next_local, stmts, locals, ctx)
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
pub(super) fn materialize_if_needed(
    expr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
) -> TirExpr {
    if matches!(
        expr.kind,
        TirExprKind::Local { .. } | TirExprKind::Unit | TirExprKind::TupleLiteral { .. }
    ) {
        // Local and Unit are already evaluated. Tuple-literal elements
        // (heap or multi-value form) are individually materialised in
        // synthesize_lift_tuple, so the whole expression is safe to evaluate
        // after freeing.
        return expr;
    }
    // For non-local expressions, check if they reference memory by looking at
    // the expression tree. Builtins like i32_load, i32_load8_u, etc. read from
    // linear memory and must be materialized before any free.
    // Use the expression's own type_id for the local, which is correct for
    // primitive types and handles (i32, i64, f32, f64, u8, u16, bool, char).
    let type_id = expr.type_id;
    let local = alloc_local(next_local, locals, type_id);
    let name = format!("__lifted_result_{local}");
    stmts.push(let_stmt(&name, local, type_id, expr));
    local_ref(local, &name, type_id)
}

fn synthesize_lift_inner(
    ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    // Unwrap newtype aliases (e.g. `type Ipv4Address = [u8, u8, u8, u8]`)
    // before dispatching, so the CM-lift logic sees the underlying shape
    // rather than treating an unknown name as an i32 handle. Other type
    // shapes pass through `resolve_type` unchanged.
    let resolved = ctx.wasi_registry.resolve_type(ty);
    let ty = &resolved;
    // Resolve stdlib struct names through the compiler-item registry so
    // a rename of `String` / `Array` / `Option` / `Result` flows through
    // CM lifting without code changes here.
    let (string_name, array_name, option_name, result_name) = {
        let tt = ctx.type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .struct_name(crate::compiler_item::CompilerItem::String)
                .to_string(),
            items
                .struct_name(crate::compiler_item::CompilerItem::Array)
                .to_string(),
            items
                .variant_name(crate::compiler_item::CompilerItem::Option)
                .to_string(),
            items
                .variant_name(crate::compiler_item::CompilerItem::Result)
                .to_string(),
        )
    };
    match ty {
        Type::Named(named) => {
            let named_name = named.name.as_str();
            if named_name == string_name {
                let ptr = builtin_call("i32_load", vec![addr.clone()], TypeTable::I32);
                let len = builtin_call(
                    "i32_load",
                    vec![binary_add(addr, i32_const(4))],
                    TypeTable::I32,
                );
                let string_type_id = ctx
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                return internal_call("memory_to_gc_string", vec![ptr, len], string_type_id);
            }
            match named_name {
                "i32" | "u32" => builtin_call("i32_load", vec![addr], TypeTable::I32),
                "i64" | "u64" => builtin_call("i64_load", vec![addr], TypeTable::I64),
                "f32" => builtin_call("f32_load", vec![addr], TypeTable::F32),
                "f64" => builtin_call("f64_load", vec![addr], TypeTable::F64),
                "i8" | "u8" => builtin_call("i32_load8_u", vec![addr], TypeTable::U8),
                "i16" | "u16" => builtin_call("i32_load16_u", vec![addr], TypeTable::U16),
                "bool" => {
                    let raw = builtin_call("i32_load8_u", vec![addr], TypeTable::U8);
                    binary(TirBinaryOp::NotEq, raw, i32_const(0), TypeTable::BOOL)
                }
                "char" => builtin_call("i32_load", vec![addr], TypeTable::CHAR),
                _ => {
                    // CM named types arrive with `source_interface` populated
                    // either by stdlib bootstrap or by `resolve_cm_source_for`
                    // (fallback to the unique `wasi:*` registrant, biased by the
                    // current binding's WASI package, then to `core:kiln/*` for
                    // generator-world bindings). Non-CM references fall through
                    // to the i32-handle default.
                    if let Some(source) = ctx
                        .wasi_registry
                        .resolve_cm_source_for(named, Some(ctx.cm_package))
                        .map(str::to_string)
                    {
                        let source = source.as_str();
                        if let Some(lifted) = try_lift_wasi_variant_or_enum(
                            named,
                            source,
                            addr.clone(),
                            next_local,
                            stmts,
                            locals,
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
                            locals,
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
            }
        }
        Type::Generic(g) => {
            let gname = g.name.as_str();
            if gname == array_name && g.args.len() == 1 {
                synthesize_lift_list(&g.args[0], addr, next_local, stmts, locals, ctx)
            } else if gname == option_name && g.args.len() == 1 {
                synthesize_lift_option_inner(&g.args[0], addr, next_local, stmts, locals, ctx)
            } else if gname == result_name && g.args.len() == 2 {
                synthesize_lift_result_inner(
                    &g.args[0], &g.args[1], addr, next_local, stmts, locals, ctx,
                )
            } else {
                // Own<T>, Borrow<T>, Stream<T>, Future<T> are i32 handles
                builtin_call("i32_load", vec![addr], TypeTable::I32)
            }
        }
        Type::Tuple(elems) if elems.is_empty() => {
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span())
        }
        Type::Tuple(elems) => synthesize_lift_tuple(elems, addr, next_local, stmts, locals, ctx),
        Type::Reference(_) | Type::MutReference(_) => {
            builtin_call("i32_load", vec![addr], TypeTable::I32)
        }
        _ => builtin_call("i32_load", vec![addr], TypeTable::I32),
    }
}

/// Try to lift a WASI variant or enum type from linear memory into a GC
/// struct. Returns `None` if `(source, named.name)` is not a variant or
/// enum in the registry.
pub(super) fn try_lift_wasi_variant_or_enum(
    named: &NamedType,
    source: &str,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> Option<TirExpr> {
    let tt = ctx.type_table.borrow();
    if let Some(cases) = ctx
        .wasi_registry
        .get_variant_cases_by_source(source, &named.name)
    {
        let cases = cases.to_vec();
        let variant_type = tt
            .find_named_type_by_cm_package(&named.name, ctx.cm_package)
            .or_else(|| {
                canonical_wasi_package(ctx.wasi_registry, &named.name)
                    .and_then(|pkg| tt.find_named_type_by_cm_package(&named.name, pkg))
            })?;
        drop(tt);
        return Some(synthesize_lift_wasi_variant(
            &named.name,
            variant_type,
            &cases,
            addr,
            next_local,
            stmts,
            locals,
            ctx,
        ));
    }
    if let Some(case_names) = ctx
        .wasi_registry
        .get_enum_variants_by_source(source, &named.name)
    {
        let case_names = case_names.to_vec();
        let enum_type = tt
            .find_named_type_by_cm_package(&named.name, ctx.cm_package)
            .or_else(|| {
                canonical_wasi_package(ctx.wasi_registry, &named.name)
                    .and_then(|pkg| tt.find_named_type_by_cm_package(&named.name, pkg))
            })?;
        drop(tt);
        return Some(synthesize_lift_wasi_enum(
            &named.name,
            enum_type,
            &case_names,
            addr,
            next_local,
            stmts,
            locals,
        ));
    }
    None
}

/// Try to lift a WASI struct (record) type from linear memory into a GC
/// struct. Returns `None` if `(source, named.name)` is not a registered
/// struct.
fn try_lift_wasi_struct(
    named: &NamedType,
    source: &str,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
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
        offset = cm_abi::align_to(offset, fa);
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
        let module_source = module_source_for_cm_interface(&mut ctx.interner.borrow_mut(), source);
        tt.make_struct(named.name.clone(), module_source)
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
        let lifted_field =
            synthesize_lift_inner(field_ty, field_addr, next_local, stmts, locals, ctx);
        let lifted_field = materialize_if_needed(lifted_field, next_local, stmts, locals);
        let field_name = &wado_fields[i];
        tir_fields.push(TirStructField {
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
    let result_local = alloc_local(next_local, locals, struct_type_id);
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
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    // Load discriminant: 1 byte (u8) for variants with ≤ 256 cases
    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt(
        "__vdisc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    ));

    // Result local (typed as the variant type)
    let result_local = alloc_local(next_local, locals, variant_type);
    stmts.push(let_mut_stmt(
        "__vresult",
        result_local,
        variant_type,
        null_expr(variant_type),
    ));

    // Compute max payload alignment for payload offset calculation
    let max_payload_align = cases
        .iter()
        .filter_map(|case| case.payload.as_ref())
        .map(|ty| {
            crate::component_model::cm_align_with_registry_scoped(
                ty,
                ctx.wasi_registry,
                Some(ctx.cm_package),
            )
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
                locals,
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
                TirBinaryOp::Eq,
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
    locals: &mut Vec<TirLocal>,
) -> TirExpr {
    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
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

    let result_local = alloc_local(next_local, locals, enum_type);
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
                TirBinaryOp::Eq,
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

/// Lift a `list<T>` from linear memory at `addr`.
///
/// Layout: `[base_ptr: i32, count: i32]` at addr.
/// Elements at `base_ptr + i * cm_size(T)`. The element stride and the
/// alignment passed to `realloc` are computed via the registry so that
/// named WASI types (records / variants / enums / flags) walk the buffer
/// at their true canonical-ABI size and align rather than the i32-handle
/// fallback baked into `cm_abi::cm_size` / `cm_abi::cm_align`.
fn synthesize_lift_list(
    elem_ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    let elem_size = crate::component_model::cm_size_with_registry_scoped(
        elem_ty,
        ctx.wasi_registry,
        Some(ctx.cm_package),
    );
    let elem_align = crate::component_model::cm_align_with_registry_scoped(
        elem_ty,
        ctx.wasi_registry,
        Some(ctx.cm_package),
    );

    // Resolve TypeIds for the element type and Array<ElemType>.
    // These are needed by the monomorphizer to instantiate Array::with_capacity and .push().
    let (elem_type_id, array_type_id, array_struct_name) = {
        let mut tt = ctx.type_table.borrow_mut();
        let elem_tid = wasi_type_to_type_id(elem_ty, &mut tt, ctx.wasi_registry, ctx.cm_package);
        let array_tid = tt.make_array(elem_tid);
        let array_name = tt
            .compiler_items()
            .struct_name(crate::compiler_item::CompilerItem::Array)
            .to_string();
        (elem_tid, array_tid, array_name)
    };

    let base_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt(
        "__base",
        base_local,
        TypeTable::I32,
        builtin_call("i32_load", vec![addr.clone()], TypeTable::I32),
    ));

    let count_local = alloc_local(next_local, locals, TypeTable::I32);
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

    let result_local = alloc_local(next_local, locals, array_type_id);
    stmts.push(let_mut_stmt(
        "__result",
        result_local,
        array_type_id,
        generic_static_call(
            &array_struct_name,
            "with_capacity",
            ModuleSource::array(),
            vec![elem_type_id],
            vec![local_ref(count_local, "__count", TypeTable::I32)],
            array_type_id,
        ),
    ));

    let i_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_mut_stmt("__i", i_local, TypeTable::I32, i32_const(0)));

    // Build loop body
    let mut loop_stmts: Vec<TirStmt> = Vec::new();

    // if __i >= __count { break; }
    loop_stmts.push(if_stmt(
        binary(
            TirBinaryOp::GtEq,
            local_ref(i_local, "__i", TypeTable::I32),
            local_ref(count_local, "__count", TypeTable::I32),
            TypeTable::BOOL,
        ),
        block(vec![break_stmt()]),
        None,
    ));

    // __elem_addr = __base + __i * elem_size
    let elem_addr_local = alloc_local(next_local, locals, TypeTable::I32);
    loop_stmts.push(let_stmt(
        "__elem_addr",
        elem_addr_local,
        TypeTable::I32,
        binary_add(
            local_ref(base_local, "__base", TypeTable::I32),
            binary(
                TirBinaryOp::Mul,
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
        locals,
        ctx,
    );
    loop_stmts.extend(elem_lift_stmts);

    // __result.push(lifted_elem)
    loop_stmts.push(expr_stmt(generic_method_call(
        local_ref(result_local, "__result", array_type_id),
        &array_struct_name,
        "push",
        ModuleSource::array(),
        vec![lifted_elem],
        TypeTable::UNIT,
    )));

    // Free element's linear memory
    loop_stmts.extend(synthesize_free_element(
        elem_ty,
        local_ref(elem_addr_local, "__elem_addr", TypeTable::I32),
        &string_name_for_free(ctx),
    ));

    // __i = __i + 1
    loop_stmts.push(expr_stmt(assign(
        local_ref(i_local, "__i", TypeTable::I32),
        binary_add(local_ref(i_local, "__i", TypeTable::I32), i32_const(1)),
    )));

    stmts.push(loop_stmt(block(loop_stmts)));

    // Free list buffer: realloc(__base, __count * elem_size, elem_align, 0)
    stmts.push(if_stmt(
        binary(
            TirBinaryOp::Gt,
            local_ref(count_local, "__count", TypeTable::I32),
            i32_const(0),
            TypeTable::BOOL,
        ),
        block(vec![expr_stmt(builtin_call(
            "realloc",
            vec![
                local_ref(base_local, "__base", TypeTable::I32),
                binary(
                    TirBinaryOp::Mul,
                    local_ref(count_local, "__count", TypeTable::I32),
                    i32_const(elem_size as i32),
                    TypeTable::I32,
                ),
                i32_const(elem_align as i32),
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
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    let layout = cm_abi::layout_option_with_registry_scoped(
        inner_ty,
        ctx.wasi_registry,
        Some(ctx.cm_package),
    );
    let payload_offset = layout.offsets[1];

    // Resolve the concrete Option<T> TypeId so the local and null/some exprs
    // use the correct GC reference type.
    let option_type_id = {
        let mut tt = ctx.type_table.borrow_mut();
        let inner_type_id =
            wasi_type_to_type_id(inner_ty, &mut tt, ctx.wasi_registry, ctx.cm_package);
        tt.make_option(inner_type_id)
    };

    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt(
        "__disc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    ));

    let result_local = alloc_local(next_local, locals, option_type_id);
    let none_init = {
        let tt = ctx.type_table.borrow();
        option_none(option_type_id, tt.compiler_items())
    };
    stmts.push(let_mut_stmt(
        "__option_result",
        result_local,
        option_type_id,
        none_init,
    ));

    // if __disc != 0 { __option_result = Some(lift(inner, addr + offset)); }
    let mut then_stmts: Vec<TirStmt> = Vec::new();
    let payload_addr = binary_add(addr, i32_const(payload_offset as i32));
    let lifted = synthesize_lift_inner(
        inner_ty,
        payload_addr.clone(),
        next_local,
        &mut then_stmts,
        locals,
        ctx,
    );
    let some_expr = {
        let tt = ctx.type_table.borrow();
        option_some(lifted, option_type_id, tt.compiler_items())
    };
    then_stmts.push(expr_stmt(assign(
        local_ref(result_local, "__option_result", option_type_id),
        some_expr,
    )));
    then_stmts.extend(synthesize_free_element(
        inner_ty,
        payload_addr,
        &string_name_for_free(ctx),
    ));

    stmts.push(if_stmt(
        binary(
            TirBinaryOp::NotEq,
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
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    let layout = cm_abi::layout_result_with_registry_scoped(
        ok_ty,
        err_ty,
        ctx.wasi_registry,
        Some(ctx.cm_package),
    );
    let payload_offset = layout.offsets[1];

    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
    // Result is a variant with 2 cases; CM spec discriminant is u8 (1 byte).
    stmts.push(let_stmt(
        "__disc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    ));

    // Determine the proper variant TypeId for Result<ok_ty, err_ty> so that the
    // mutable local is typed as a GC reference (not i32).
    let (result_type_id, ok_name, ok_index, err_name, err_index) = {
        let mut tt = ctx.type_table.borrow_mut();
        let ok_type_id = wasi_type_to_type_id(ok_ty, &mut tt, ctx.wasi_registry, ctx.cm_package);
        let err_type_id = wasi_type_to_type_id(err_ty, &mut tt, ctx.wasi_registry, ctx.cm_package);
        let result_type_id = tt.make_result(ok_type_id, err_type_id);
        let items = tt.compiler_items();
        let (_, _, ok_n, ok_i) =
            items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
        let (_, _, err_n, err_i) =
            items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
        (
            result_type_id,
            ok_n.to_string(),
            ok_i,
            err_n.to_string(),
            err_i,
        )
    };

    let result_local = alloc_local(next_local, locals, result_type_id);
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
            locals,
            ctx,
        );
        Some(Box::new(lifted))
    };
    ok_stmts.push(expr_stmt(assign(
        local_ref(result_local, "__result_val", result_type_id),
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: result_type_id,
                case_index: ok_index,
                case_name: ok_name,
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
            locals,
            ctx,
        );
        Some(Box::new(lifted))
    };
    err_stmts.push(expr_stmt(assign(
        local_ref(result_local, "__result_val", result_type_id),
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: result_type_id,
                case_index: err_index,
                case_name: err_name,
                payload: err_payload,
            },
            result_type_id,
            synth_span(),
        ),
    )));

    stmts.push(if_stmt(
        binary(
            TirBinaryOp::Eq,
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
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    let layout = cm_abi::layout_tuple(elems);
    let mut elem_exprs = Vec::new();
    for (i, elem_ty) in elems.iter().enumerate() {
        let elem_addr = binary_add(addr.clone(), i32_const(layout.offsets[i] as i32));
        let lifted = synthesize_lift_inner(elem_ty, elem_addr, next_local, stmts, locals, ctx);
        // Materialize each element to ensure memory loads are evaluated before
        // the outptr buffer is freed (prevents use-after-free with debug allocator).
        let lifted = materialize_if_needed(lifted, next_local, stmts, locals);
        elem_exprs.push(lifted);
    }
    let tuple_type_id = {
        let mut tt = ctx.type_table.borrow_mut();
        let elem_type_ids: Vec<TypeId> = elems
            .iter()
            .map(|t| wasi_type_to_type_id(t, &mut tt, ctx.wasi_registry, ctx.cm_package))
            .collect();
        tt.make_tuple(elem_type_ids)
    };
    // CM lift bindings synthesise tuple values that flow across the
    // linear-memory ↔ GC boundary; downstream consumers (CM record
    // fields, list elements, result wrappers) all expect a heap struct
    // ref. The multi-value-return ABI classifier in
    // `optimize::multi_value_return` deliberately rejects CM-binding
    // functions, so this `TupleLiteral` always materialises as a single
    // `struct.new` rather than being unwrapped to a stack-only
    // sequence.
    TirExpr::new(
        TirExprKind::TupleLiteral {
            elements: elem_exprs,
        },
        tuple_type_id,
        synth_span(),
    )
}

/// Free a CM element's linear memory (within a list iteration).
///
/// For primitives: no-op.
/// For String: frees the string data buffer.
/// Look up the canonical stdlib `String` struct name via the
/// compiler-item registry attached to `ctx`'s type table. Used to
/// drive [`synthesize_free_element`]'s string match without
/// hard-coding the literal `"String"`.
fn string_name_for_free(ctx: &LiftContext<'_>) -> String {
    ctx.type_table
        .borrow()
        .compiler_items()
        .struct_name(crate::compiler_item::CompilerItem::String)
        .to_string()
}

fn synthesize_free_element(ty: &Type, addr: TirExpr, string_name: &str) -> Vec<TirStmt> {
    match ty {
        Type::Named(named) if named.name == string_name => {
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
                free_stmts.extend(synthesize_free_element(elem_ty, elem_addr, string_name));
            }
            free_stmts
        }
        _ => vec![],
    }
}
