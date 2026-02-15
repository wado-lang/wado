//! CM Adapter Synthesis phase.
//!
//! Generates TIR adapter functions for Component Model boundary crossing.
//! Each adapter handles lifting Wado values to CM flat ABI (lowering params)
//! and lifting CM flat ABI values back to Wado types (lifting results).
//!
//! Pipeline position: after `effect_check`, before monomorphize.
//! This ensures adapter functions go through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-15-cm-adapter-synthesis.md` for design details.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};

use crate::ast::Type;
use crate::cm_abi;
use crate::component_model::{WasiFunctionInfo, WasiRegistry};
use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{
    FunctionRef, TirBlock, TirExpr, TirExprKind, TirFunction, TirParam, TirStmt, TirStmtKind,
    TypeId, TypeTable,
};
use crate::token::Span;

/// Context for lifting CM values to GC types, providing access to
/// the WASI registry (for variant/enum case info) and type table (for `TypeIds`).
pub struct LiftContext<'a> {
    pub wasi_registry: &'a WasiRegistry,
    pub type_table: &'a RefCell<TypeTable>,
}

/// Synthetic span used for all generated adapter code.
fn synth_span() -> Span {
    Span::new(0, 0, 1, 1)
}

// ============================================================================
// TIR construction helpers
// ============================================================================

/// Create a call to a builtin function (e.g., `builtin::i32_load`).
pub fn builtin_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: crate::tir::FunctionRef::External {
                module_source: ModuleSource::core("builtin"),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create a call to an internal function (e.g., `internal::cm_lower_string`).
pub fn internal_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: crate::tir::FunctionRef::External {
                module_source: ModuleSource::core("internal"),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create an i32 literal expression.
#[allow(clippy::cast_sign_loss)]
pub fn i32_const(value: i32) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: value as u64,
            repr: value.to_string(),
        },
        TypeTable::I32,
        synth_span(),
    )
}

/// Create an i64 literal expression.
#[allow(clippy::cast_sign_loss)]
pub fn i64_const(value: i64) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: value as u64,
            repr: value.to_string(),
        },
        TypeTable::I64,
        synth_span(),
    )
}

/// Create a local variable reference.
pub fn local_ref(index: u32, name: &str, type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Local {
            index,
            name: name.to_string(),
        },
        type_id,
        synth_span(),
    )
}

/// Create a binary expression.
pub fn binary(op: crate::tir::TirBinaryOp, left: TirExpr, right: TirExpr, ty: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty,
        synth_span(),
    )
}

/// Create an i32 addition expression.
fn binary_add(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(crate::tir::TirBinaryOp::Add, left, right, TypeTable::I32)
}

/// Create a cast expression.
pub fn cast(expr: TirExpr, target_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(expr),
            target_type,
        },
        target_type,
        synth_span(),
    )
}

/// Create a let statement.
pub fn let_stmt(name: &str, local_index: u32, type_id: TypeId, value: TirExpr) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Let {
            name: name.to_string(),
            local_index,
            is_mut: false,
            is_reactive: false,
            type_id,
            value,
        },
        synth_span(),
    )
}

/// Create an expression statement.
pub fn expr_stmt(expr: TirExpr) -> TirStmt {
    TirStmt::new(TirStmtKind::Expr(expr), synth_span())
}

/// Create a return statement.
pub fn return_stmt(value: Option<TirExpr>) -> TirStmt {
    TirStmt::new(TirStmtKind::Return { value }, synth_span())
}

/// Create a `CmRawCall` expression targeting a lowered WASI import.
pub fn cm_raw_call(local_name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::CmRawCall {
            local_name: local_name.to_string(),
            args,
        },
        return_type,
        synth_span(),
    )
}

// ============================================================================
// Lift / Lower synthesis
// ============================================================================

/// Create a mutable let statement.
pub fn let_mut_stmt(name: &str, local_index: u32, type_id: TypeId, value: TirExpr) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Let {
            name: name.to_string(),
            local_index,
            is_mut: true,
            is_reactive: false,
            type_id,
            value,
        },
        synth_span(),
    )
}

/// Create an if statement.
pub fn if_stmt(condition: TirExpr, then_block: TirBlock, else_block: Option<TirBlock>) -> TirStmt {
    TirStmt::new(
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        },
        synth_span(),
    )
}

/// Create a loop statement.
pub fn loop_stmt(body: TirBlock) -> TirStmt {
    TirStmt::new(TirStmtKind::Loop { body }, synth_span())
}

/// Create a break statement.
pub fn break_stmt() -> TirStmt {
    TirStmt::new(
        TirStmtKind::Break {
            label: None,
            value: None,
        },
        synth_span(),
    )
}

/// Create a local assignment expression.
pub fn assign(target: TirExpr, value: TirExpr) -> TirExpr {
    TirExpr::new(
        TirExprKind::Assign {
            target: Box::new(target),
            value: Box::new(value),
        },
        TypeTable::UNIT,
        synth_span(),
    )
}

/// Create a method call expression.
pub fn method_call(
    receiver: TirExpr,
    method_name: &str,
    method_module_source: ModuleSource,
    type_args: Vec<TypeId>,
    args: Vec<TirExpr>,
    return_type: TypeId,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: crate::tir::FunctionRef::External {
                module_source: method_module_source,
                name: method_name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args,
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create a static call expression (e.g., `Array::<T>::with_capacity(n)`).
pub fn static_call(
    method_name: &str,
    module_source: ModuleSource,
    args: Vec<TirExpr>,
    return_type: TypeId,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::StaticCall {
            func: crate::tir::FunctionRef::External {
                module_source,
                name: method_name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create an `Option::Some(value)` expression.
pub fn option_some(value: TirExpr, option_type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::OptionSome {
            value: Box::new(value),
        },
        option_type_id,
        synth_span(),
    )
}

/// Create a `null` (`Option::None`) expression.
pub fn null_expr(type_id: TypeId) -> TirExpr {
    TirExpr::new(TirExprKind::Null, type_id, synth_span())
}

/// Create a TIR block from statements.
pub fn block(stmts: Vec<TirStmt>) -> TirBlock {
    TirBlock {
        stmts,
        span: synth_span(),
    }
}

/// Allocate a local variable, returning its index.
fn alloc_local(next_local: &mut u32, local_types: &mut Vec<TypeId>, ty: TypeId) -> u32 {
    let idx = *next_local;
    *next_local += 1;
    local_types.push(ty);
    idx
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
            "i8" | "u8" => builtin_call("i32_load8_u", vec![addr], TypeTable::I32),
            "i16" | "u16" => builtin_call("i32_load16_u", vec![addr], TypeTable::I32),
            "bool" => {
                let raw = builtin_call("i32_load8_u", vec![addr], TypeTable::I32);
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
                internal_call("memory_to_gc_string", vec![ptr, len], TypeTable::I32)
            }
            _ => {
                // Check if this is a WASI variant/enum that needs GC struct construction
                if let Some(ctx) = ctx
                    && let Some(lifted) = try_lift_wasi_variant_or_enum(
                        &named.name,
                        addr.clone(),
                        next_local,
                        stmts,
                        local_types,
                        ctx,
                    )
                {
                    return lifted;
                }
                // Default: treat as i32 handles (resources, unknown types)
                builtin_call("i32_load", vec![addr], TypeTable::I32)
            }
        },
        Type::Generic(g) => match g.name.as_str() {
            "Array" if g.args.len() == 1 => {
                synthesize_lift_list(&g.args[0], addr, next_local, stmts, local_types)
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
        Type::Tuple(elems) => synthesize_lift_tuple(elems, addr, next_local, stmts, local_types),
        Type::Reference(_) | Type::MutReference(_) => {
            builtin_call("i32_load", vec![addr], TypeTable::I32)
        }
        _ => builtin_call("i32_load", vec![addr], TypeTable::I32),
    }
}

/// Try to lift a WASI variant or enum type from linear memory into a GC struct.
/// Returns `None` if the type is not a known WASI variant/enum.
fn try_lift_wasi_variant_or_enum(
    name: &str,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: &LiftContext<'_>,
) -> Option<TirExpr> {
    let tt = ctx.type_table.borrow();

    // Check WASI variants (e.g., HeaderError with cases InvalidSyntax, Forbidden, Immutable)
    if let Some(cases) = ctx.wasi_registry.get_variant_cases(name) {
        let cases = cases.to_vec();
        let variant_type = tt.find_variant_type_by_name(name)?;
        drop(tt);
        return Some(synthesize_lift_wasi_variant(
            name,
            variant_type,
            &cases,
            addr,
            next_local,
            stmts,
            local_types,
        ));
    }

    // Check WASI enums (e.g., ErrorCode)
    if let Some(case_names) = ctx.wasi_registry.get_enum_variants(name) {
        let case_names = case_names.to_vec();
        let enum_type = tt.find_enum_type_by_name(name)?;
        drop(tt);
        return Some(synthesize_lift_wasi_enum(
            name,
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

/// Lift a WASI variant type (e.g., `HeaderError`) from an i32 discriminant.
/// Generates an if/else chain: disc==0 → Case0, disc==1 → Case1, ...
fn synthesize_lift_wasi_variant(
    _name: &str,
    variant_type: TypeId,
    cases: &[(String, bool)],
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
) -> TirExpr {
    // Load discriminant
    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        "__vdisc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load", vec![addr], TypeTable::I32),
    ));

    // Result local (typed as the variant type)
    let result_local = alloc_local(next_local, local_types, variant_type);
    stmts.push(let_mut_stmt(
        "__vresult",
        result_local,
        variant_type,
        null_expr(variant_type),
    ));

    // Build if/else chain for each case (last case is the else branch)
    // Note: currently only handles no-payload cases
    let case_count = cases.len();
    let mut current_else: Option<TirBlock> = None;

    for (i, (cm_case_name, _has_payload)) in cases.iter().enumerate().rev() {
        // Convert kebab-case CM name to PascalCase Wado name
        let case_name = kebab_to_pascal(cm_case_name);
        let construct = TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type,
                case_index: i as u32,
                case_name,
                payload: None,
            },
            variant_type,
            synth_span(),
        );
        let assign_stmt = expr_stmt(assign(
            local_ref(result_local, "__vresult", variant_type),
            construct,
        ));

        if i == case_count - 1 {
            // Last case: becomes the else branch
            current_else = Some(block(vec![assign_stmt]));
        } else {
            // Build if statement: if disc == i { assign } else { current_else }
            let cond = binary(
                crate::tir::TirBinaryOp::Eq,
                local_ref(disc_local, "__vdisc", TypeTable::I32),
                i32_const(i as i32),
                TypeTable::BOOL,
            );
            let if_stmt_node = if_stmt(cond, block(vec![assign_stmt]), current_else);
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
    stmts.push(let_stmt(
        "__edisc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load", vec![addr], TypeTable::I32),
    ));

    let result_local = alloc_local(next_local, local_types, enum_type);
    stmts.push(let_mut_stmt(
        "__eresult",
        result_local,
        enum_type,
        null_expr(enum_type),
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
#[allow(clippy::cast_possible_wrap)]
fn synthesize_lift_list(
    elem_ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
) -> TirExpr {
    let elem_size = cm_abi::cm_size(elem_ty);

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

    let result_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_mut_stmt(
        "__result",
        result_local,
        TypeTable::I32,
        static_call(
            "with_capacity",
            ModuleSource::core("prelude"),
            vec![local_ref(count_local, "__count", TypeTable::I32)],
            TypeTable::I32,
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

    // Lift element
    let mut elem_lift_stmts: Vec<TirStmt> = Vec::new();
    let lifted_elem = synthesize_lift(
        elem_ty,
        local_ref(elem_addr_local, "__elem_addr", TypeTable::I32),
        next_local,
        &mut elem_lift_stmts,
        local_types,
    );
    loop_stmts.extend(elem_lift_stmts);

    // __result.append(lifted_elem)
    loop_stmts.push(expr_stmt(method_call(
        local_ref(result_local, "__result", TypeTable::I32),
        "append",
        ModuleSource::core("prelude"),
        vec![],
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

    local_ref(result_local, "__result", TypeTable::I32)
}

/// Lift an `option<T>` from linear memory at `addr`.
///
/// Layout: discriminant byte at offset 0, payload at `align_to(1, align(T))`.
#[allow(clippy::cast_possible_wrap)]
fn synthesize_lift_option_inner(
    inner_ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    let layout = cm_abi::layout_option(inner_ty);
    let payload_offset = layout.offsets[1];

    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        "__disc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    ));

    let result_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_mut_stmt(
        "__option_result",
        result_local,
        TypeTable::I32,
        null_expr(TypeTable::I32),
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
    then_stmts.extend(synthesize_free_element(inner_ty, payload_addr));
    then_stmts.push(expr_stmt(assign(
        local_ref(result_local, "__option_result", TypeTable::I32),
        option_some(lifted, TypeTable::I32),
    )));

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

    local_ref(result_local, "__option_result", TypeTable::I32)
}

/// Lift a `result<T, E>` from linear memory at `addr`.
///
/// Layout: discriminant i32 at offset 0, payload at aligned offset.

#[allow(clippy::cast_possible_wrap)]
fn synthesize_lift_result_inner(
    ok_ty: &Type,
    err_ty: &Type,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
    ctx: Option<&LiftContext<'_>>,
) -> TirExpr {
    let layout = cm_abi::layout_result(ok_ty, err_ty);
    let payload_offset = layout.offsets[1];

    let disc_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_stmt(
        "__disc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load", vec![addr.clone()], TypeTable::I32),
    ));

    let result_local = alloc_local(next_local, local_types, TypeTable::I32);
    stmts.push(let_mut_stmt(
        "__result_val",
        result_local,
        TypeTable::I32,
        null_expr(TypeTable::I32),
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
        local_ref(result_local, "__result_val", TypeTable::I32),
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: TypeTable::I32,
                case_index: 0,
                case_name: "Ok".to_string(),
                payload: ok_payload,
            },
            TypeTable::I32,
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
        local_ref(result_local, "__result_val", TypeTable::I32),
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: TypeTable::I32,
                case_index: 1,
                case_name: "Err".to_string(),
                payload: err_payload,
            },
            TypeTable::I32,
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

    local_ref(result_local, "__result_val", TypeTable::I32)
}

/// Lift a tuple from linear memory at `addr`.
#[allow(clippy::cast_possible_wrap)]
fn synthesize_lift_tuple(
    elems: &[Type],
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    local_types: &mut Vec<TypeId>,
) -> TirExpr {
    let layout = cm_abi::layout_tuple(elems);
    let mut elem_exprs = Vec::new();
    for (i, elem_ty) in elems.iter().enumerate() {
        let elem_addr = binary_add(addr.clone(), i32_const(layout.offsets[i] as i32));
        let lifted = synthesize_lift(elem_ty, elem_addr, next_local, stmts, local_types);
        elem_exprs.push(lifted);
    }
    TirExpr::new(
        TirExprKind::TupleLiteral {
            elements: elem_exprs,
        },
        TypeTable::I32,
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
            #[allow(clippy::cast_possible_wrap)]
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
            // list<T> and other composite types: lowered as (ptr, len) pair
            "Array" => {
                // Call cm_lower_array_u8 or similar — for now, delegate to existing helpers
                // Array<u8> is the most common case; other types need element-by-element lowering
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
        Type::Reference(_) | Type::MutReference(_) => vec![expr_stmt(builtin_call(
            "i32_store",
            vec![addr, value],
            TypeTable::UNIT,
        ))],
        _ => vec![],
    }
}

// ============================================================================
// Flat ABI parameter/result computation
// ============================================================================

/// Compute the flat ABI parameter types for a WASI function parameter.
pub fn flatten_param_type(ty: &Type) -> Vec<TypeId> {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" | "bool" | "char" | "i8" | "u8" | "i16" | "u16" => {
                vec![TypeTable::I32]
            }
            "i64" | "u64" => vec![TypeTable::I64],
            "f32" => vec![TypeTable::F32],
            "f64" => vec![TypeTable::F64],
            "String" => vec![TypeTable::I32, TypeTable::I32],
            _ => vec![TypeTable::I32],
        },
        Type::Generic(g) if g.name == "Stream" => vec![TypeTable::I32],
        Type::Reference(_) | Type::MutReference(_) => vec![TypeTable::I32],
        Type::Tuple(elems) if elems.is_empty() => vec![],
        _ => {
            let flat = cm_abi::cm_flat_types(ty);
            flat.iter()
                .map(|t| match t {
                    cm_abi::CmValType::I32 => TypeTable::I32,
                    cm_abi::CmValType::I64 => TypeTable::I64,
                    cm_abi::CmValType::F32 => TypeTable::F32,
                    cm_abi::CmValType::F64 => TypeTable::F64,
                })
                .collect()
        }
    }
}

// ============================================================================
// Adapter function generation
// ============================================================================

/// Build the adapter function name for a WASI import.
pub fn adapter_func_name(effect_name: &str, method_name: &str) -> String {
    format!("__cm_adapter__{effect_name}_{method_name}")
}

// ============================================================================
// Adapter TirFunction synthesis
// ============================================================================

/// Fixed async outptr address (matches codegen convention).
const ASYNC_OUTPTR: i32 = 2048;

/// Canonical ABI: maximum number of flat return values before outptr is used.
const MAX_FLAT_RESULTS: usize = 1;

/// Returns the internal.wado converter function name for list types that need
/// pre-compiled generic array operations, or `None` if the type can be lifted inline.
///
/// List types require `Array::<T>::with_capacity()` and `.append()` which are generic
/// methods needing monomorphization. Since adapter synthesis runs after the resolver,
/// we delegate to internal.wado converter functions for these types.
fn list_converter_for_type(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
            match &g.args[0] {
                Type::Named(n) if n.name == "String" => Some("cm_list_string_to_array"),
                Type::Named(n) if n.name == "u8" => None, // u8 lists use memory_to_gc_array inline
                Type::Tuple(elems) if elems.len() == 2 => {
                    // list<tuple<string, string>> → cm_list_tuple_string_string_to_array
                    if elems
                        .iter()
                        .all(|e| matches!(e, Type::Named(n) if n.name == "String"))
                    {
                        Some("cm_list_tuple_string_string_to_array")
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Check whether a return type needs lifting from a flat i32 discriminant to a GC struct.
/// This is true for Result types where all payloads are empty (unit), so the raw call
/// returns just a discriminant on the stack without an outptr.
fn needs_flat_result_lifting(ty: &Type) -> bool {
    match ty {
        Type::Generic(g) if g.name == "Result" && g.args.len() == 2 => true,
        _ => false,
    }
}

/// Synthesize lifting of a flat Result discriminant into a GC variant struct.
///
/// For `Result<(), ()>`: disc==0 → Ok, disc==1 → Err (no payloads)
/// For `Result<(), ErrorCode>`: disc==0 → Ok, disc!=0 → `Err(lift_error)`
#[allow(clippy::too_many_arguments)]
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
            // Err with a flat payload — the remaining flat values encode the error
            // For enums/variants, the error value is the disc shifted appropriately
            // For now, try to lift the error type using the WASI variant/enum path
            let err_name = match err_ty {
                Type::Named(n) => n.name.as_str(),
                _ => "",
            };
            if let Some(lifted) = try_lift_wasi_variant_or_enum(
                err_name,
                // The error discriminant is in the remaining flat values after the Result disc
                // For flat result, the second flat value is the error payload
                disc_expr.clone(), // placeholder — we'll fix below
                next_local,
                stmts,
                local_types,
                ctx,
            ) {
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
fn make_adapter_function(
    name: String,
    params: Vec<TirParam>,
    return_type: TypeId,
    body: TirBlock,
    local_count: u32,
    local_types: Vec<TypeId>,
) -> Rc<RefCell<TirFunction>> {
    Rc::new(RefCell::new(TirFunction {
        name,
        is_pub: false,
        is_export: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params,
        return_type,
        effects: vec![],
        body: Some(body),
        span: synth_span(),
        local_count,
        local_types,
        address_taken_locals: IndexSet::new(),
        needed_copy_types: IndexSet::new(),
        scratch_locals: vec![],
        copy_source_types: IndexSet::new(),
        indirect_call_counts: IndexMap::new(),
        match_scrutinee_types: vec![],
        let_pattern_types: vec![],
        is_cm_adapter: true,
        cm_export_info: None,
    }))
}

/// Map a WASI return type to the flat return `TypeId` for the adapter.
/// Sync functions with outptr return void from the raw call itself.
fn wasi_return_type_id(func_info: &WasiFunctionInfo) -> TypeId {
    if func_info.is_async {
        // Async: raw call returns subtask handle (i32)
        TypeTable::I32
    } else {
        let needs_outptr = func_info
            .return_type
            .as_ref()
            .is_some_and(|rt| crate::cm_abi::cm_flat_types(rt).len() > MAX_FLAT_RESULTS);
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

/// Synthesize a CM adapter function for a WASI import.
///
/// The adapter function:
/// 1. Accepts the same parameter types as the WASI function
/// 2. Lowers parameters to flat CM ABI (String → ptr/len, etc.)
/// 3. Calls the lowered WASI function via `CmRawCall`
/// 4. Lifts the result from flat CM ABI back to Wado types
/// 5. Returns the Wado-typed result
///
/// The adapter's Wado-level return type matches the WASI function declaration.
/// All return types are lifted inline using `synthesize_lift` — no per-type
/// converter functions are needed.
fn synthesize_adapter(
    func_info: &WasiFunctionInfo,
    wasi_registry: &crate::component_model::WasiRegistry,
    type_table: &RefCell<TypeTable>,
) -> Rc<RefCell<TirFunction>> {
    let name = adapter_func_name(&func_info.effect_name, &func_info.method_name);
    let local_name = func_info.local_alias_name();

    // Derive outptr needs from return type using Canonical ABI layout
    let needs_outptr = func_info
        .return_type
        .as_ref()
        .is_some_and(|rt| crate::cm_abi::cm_flat_types(rt).len() > MAX_FLAT_RESULTS);
    let outptr_alloc = if needs_outptr {
        func_info
            .return_type
            .as_ref()
            .map(|rt| (crate::cm_abi::cm_size(rt), crate::cm_abi::cm_align(rt)))
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
    for (param_name, param_type) in &func_info.params {
        let flat_tys = flatten_param_type(param_type);
        if flat_tys.is_empty() {
            continue; // unit param, skip
        }
        let param_type_id = match param_type {
            Type::Named(n) if n.name == "String" => TypeTable::I32, // placeholder for String
            Type::Generic(g)
                if g.name == "Array"
                    && g.args.len() == 1
                    && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                TypeTable::I32 // placeholder for Array<u8>
            }
            _ => flat_tys[0],
        };
        let param_local = next_local;
        params.push(TirParam {
            name: param_name.clone(),
            type_id: param_type_id,
            local_index: param_local,
            span: synth_span(),
        });
        local_types.push(param_type_id);
        next_local += 1;
    }

    // ---- Pass 2: Generate parameter lowering code ----
    // Intermediate locals (packed i64, etc.) are allocated after all params.
    let mut param_idx = 0usize;
    for (param_name, param_type) in &func_info.params {
        let flat_tys = flatten_param_type(param_type);
        if flat_tys.is_empty() {
            continue; // unit param, skip
        }
        let param_local = params[param_idx].local_index;
        param_idx += 1;

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

            // Simple types: pass directly as flat args
            _ => {
                let param_type_id = flat_tys[0];
                flat_args.push(local_ref(param_local, param_name, param_type_id));
            }
        }
    }

    // ---- Handle outptr for async or complex returns ----
    if func_info.is_async {
        flat_args.push(i32_const(ASYNC_OUTPTR));
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
    let raw_call_return_type = wasi_return_type_id(func_info);
    let raw_call_expr = cm_raw_call(&local_name, flat_args, raw_call_return_type);

    // ---- Handle result ----
    // The adapter's return type to the Wado caller:
    let adapter_return_type;

    if func_info.is_async {
        // Async: discard subtask handle, return void
        body_stmts.push(expr_stmt(raw_call_expr));
        adapter_return_type = TypeTable::UNIT;
    } else if let Some((alloc_size, alloc_align)) = outptr_alloc {
        body_stmts.push(expr_stmt(raw_call_expr));
        let outptr_local = next_local - 1;

        let return_type = func_info.return_type.as_ref().unwrap();
        let resolved = wasi_registry.resolve_type(return_type);

        // Check if this return type needs a pre-compiled internal converter function.
        // List types require generic Array<T> operations (with_capacity, append) which
        // need monomorphization. Since adapters are synthesized after resolution,
        // we delegate to internal.wado converter functions for these types.
        if let Some(converter_name) = list_converter_for_type(&resolved) {
            let converter_call = internal_call(
                converter_name,
                vec![local_ref(outptr_local, "__outptr", TypeTable::I32)],
                TypeTable::I32,
            );
            body_stmts.push(return_stmt(Some(converter_call)));
        } else {
            // Inline lifting for all other types (primitives, string, option, result, tuple, etc.)
            let lift_ctx = LiftContext {
                wasi_registry,
                type_table,
            };
            let lifted = synthesize_lift_with_context(
                &resolved,
                local_ref(outptr_local, "__outptr", TypeTable::I32),
                &mut next_local,
                &mut body_stmts,
                &mut local_types,
                &lift_ctx,
            );

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

            body_stmts.push(return_stmt(Some(lifted)));
        }
        adapter_return_type = TypeTable::I32; // placeholder, fixed up at call site
    } else if func_info.return_type.is_some() {
        let return_type = func_info.return_type.as_ref().unwrap();
        let resolved = wasi_registry.resolve_type(return_type);
        if needs_flat_result_lifting(&resolved) {
            // Flat return with complex type (e.g., Result<(), ()>): the raw call returns
            // an i32 discriminant on the stack, but the adapter needs to return a GC struct.
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
            body_stmts.push(return_stmt(Some(lifted)));
            adapter_return_type = TypeTable::I32; // placeholder
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

    make_adapter_function(
        name,
        params,
        adapter_return_type,
        body,
        next_local,
        local_types,
    )
}

// ============================================================================
// Phase entry point
// ============================================================================

/// Phase entry point: generate CM adapter functions and rewrite call sites.
///
/// For each WASI import function used in the program:
/// 1. Synthesizes an adapter TIR function that handles CM boundary crossing
/// 2. Rewrites effect-like `Call` nodes to target the adapter function
///
/// Adapter functions flow through monomorphize → lower → optimize → codegen
/// like any other function.
pub fn generate_adapters(mut project: Project) -> Project {
    // Step 1: Collect all used WASI effect calls and resource method calls
    let mut seen_effects: IndexSet<String> = IndexSet::new();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(body, &mut seen_effects, project.wasi_registry);
            }
        }
    }

    if seen_effects.is_empty() {
        return project;
    }

    // Step 2: Synthesize adapter functions for each used WASI function
    let entry_type_table = project
        .tir_modules
        .get(&project.entry_module_source)
        .map(|m| m.type_table.clone())
        .unwrap_or_else(|| Rc::new(RefCell::new(TypeTable::new())));
    let mut adapters: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::new();
    for qualified_name in &seen_effects {
        if let Some(func_info) = project.wasi_registry.get_function(qualified_name) {
            let func_info = func_info.clone();
            let adapter_name = adapter_func_name(&func_info.effect_name, &func_info.method_name);
            let adapter = synthesize_adapter(&func_info, project.wasi_registry, &entry_type_table);
            adapters.insert(qualified_name.clone(), adapter.clone());
            // Also index by adapter function name for lookup
            adapters.insert(adapter_name, adapter);
        }
    }

    // Step 3: Add adapter functions to the entry module
    let entry_source = project.entry_module_source.clone();
    if let Some(entry_module) = project.tir_modules.get_mut(&entry_source) {
        for (key, adapter_rc) in &adapters {
            // Only add each adapter once (skip the duplicate keyed by adapter_name)
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
                rewrite_calls_in_block(body, &adapter_map, &entry_source);
            }
        }
    }

    project
}

// ============================================================================
// Adapter type fixup
// ============================================================================

/// Fix up the return expression's type in the adapter body to match the caller's
/// expected return type. The adapter was created with placeholder `TypeId`s
/// (e.g., `TypeTable::I32`) that need to be corrected to actual Wado types.
fn fixup_return_type_in_body(adapter: &mut TirFunction, return_type: TypeId) {
    if let Some(body) = &mut adapter.body {
        fixup_types_in_block(body, return_type, &mut adapter.local_types);
    }
}

/// Recursively fix placeholder types in a block's return statements and
/// their nested expressions.
fn fixup_types_in_block(block: &mut TirBlock, return_type: TypeId, local_types: &mut Vec<TypeId>) {
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            TirStmtKind::Return {
                value: Some(ret_expr),
            } => {
                fixup_expr_type(ret_expr, return_type);
            }
            // Recurse into control flow that contains return statements
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                fixup_types_in_block(then_block, return_type, local_types);
                if let Some(blk) = else_block {
                    fixup_types_in_block(blk, return_type, local_types);
                }
            }
            TirStmtKind::Loop { body } => {
                fixup_types_in_block(body, return_type, local_types);
            }
            // Fix up Let stmts that hold adapter intermediate results
            TirStmtKind::Let {
                value,
                local_index,
                type_id,
                ..
            } => {
                let idx = *local_index;
                fixup_adapter_let(value, idx, return_type, type_id, local_types);
            }
            TirStmtKind::Expr(expr) => {
                fixup_adapter_expr(expr, return_type);
            }
            _ => {}
        }
    }
}

/// Fix up an expression in a Let statement that might hold adapter intermediate values.
/// Also fixes the Let's own `type_id` and the corresponding `local_types` entry.
fn fixup_adapter_let(
    expr: &mut TirExpr,
    local_index: u32,
    return_type: TypeId,
    let_type_id: &mut TypeId,
    local_types: &mut Vec<TypeId>,
) {
    match &mut expr.kind {
        TirExprKind::StaticCall { .. } => {
            if expr.type_id == TypeTable::I32 {
                expr.type_id = return_type;
                *let_type_id = return_type;
                if (local_index as usize) < local_types.len() {
                    local_types[local_index as usize] = return_type;
                }
            }
        }
        TirExprKind::Null => {
            if expr.type_id == TypeTable::I32 {
                expr.type_id = return_type;
                *let_type_id = return_type;
                if (local_index as usize) < local_types.len() {
                    local_types[local_index as usize] = return_type;
                }
            }
        }
        _ => {}
    }
}

/// Fix up an expression statement (e.g., Assign with VariantConstruct/OptionSome).
fn fixup_adapter_expr(expr: &mut TirExpr, return_type: TypeId) {
    if let TirExprKind::Assign { target, value } = &mut expr.kind {
        fixup_variant_construct(value, return_type);
        // Also fix up the assign target (Local ref) type
        if target.type_id == TypeTable::I32 {
            target.type_id = return_type;
        }
    }
}

/// Fix up `VariantConstruct` and `OptionSome` expressions to use the real type.
fn fixup_variant_construct(expr: &mut TirExpr, return_type: TypeId) {
    match &mut expr.kind {
        TirExprKind::VariantConstruct { variant_type, .. } => {
            if *variant_type == TypeTable::I32 {
                *variant_type = return_type;
            }
            if expr.type_id == TypeTable::I32 {
                expr.type_id = return_type;
            }
        }
        TirExprKind::OptionSome { .. } => {
            if expr.type_id == TypeTable::I32 {
                expr.type_id = return_type;
            }
        }
        _ => {}
    }
}

/// Recursively fix the `type_id` of an expression and its leaf nodes.
fn fixup_expr_type(expr: &mut TirExpr, type_id: TypeId) {
    expr.type_id = type_id;
    match &mut expr.kind {
        TirExprKind::TupleLiteral { .. } | TirExprKind::Call { .. } | TirExprKind::Local { .. } => {
        }
        TirExprKind::VariantConstruct { variant_type, .. } => {
            if *variant_type == TypeTable::I32 {
                *variant_type = type_id;
            }
        }
        TirExprKind::OptionSome { .. } | TirExprKind::Null => {}
        _ => {}
    }
}

// ============================================================================
// Call site rewriting
// ============================================================================

fn rewrite_calls_in_block(
    block: &mut TirBlock,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
) {
    for stmt in &mut block.stmts {
        rewrite_calls_in_stmt(stmt, adapters, entry_source);
    }
}

fn rewrite_calls_in_stmt(
    stmt: &mut TirStmt,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            rewrite_calls_in_expr(value, adapters, entry_source);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source);
            rewrite_calls_in_block(then_block, adapters, entry_source);
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_calls_in_block(body, adapters, entry_source);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source);
            rewrite_calls_in_block(then_block, adapters, entry_source);
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source);
            }
        }
        TirStmtKind::Continue | TirStmtKind::LetPattern { .. } => {}
    }
}

fn rewrite_calls_in_expr(
    expr: &mut TirExpr,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
) {
    // Check if this is an EffectCall that should be rewritten to target an adapter
    if let TirExprKind::EffectCall {
        effect_name,
        op_name,
        ..
    } = &expr.kind
    {
        let qualified = format!("{effect_name}::{op_name}");
        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Collect args before replacing (need to move them out)
            let mut taken_args: Vec<TirExpr> = Vec::new();
            if let TirExprKind::EffectCall { args, .. } = &mut expr.kind {
                taken_args = std::mem::take(args);
            }

            // Fix up adapter function types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if adapter.return_type != expr.type_id {
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, expr.type_id);
                }
                for (i, arg) in taken_args.iter().enumerate() {
                    if i < adapter.params.len() && adapter.params[i].type_id != arg.type_id {
                        let local_idx = adapter.params[i].local_index as usize;
                        adapter.params[i].type_id = arg.type_id;
                        if local_idx < adapter.local_types.len() {
                            adapter.local_types[local_idx] = arg.type_id;
                        }
                    }
                }
            }

            // Replace EffectCall with Call targeting the adapter
            expr.kind = TirExprKind::Call {
                func: FunctionRef::Resolved {
                    func: adapter_rc.clone(),
                    module_source: entry_source.clone(),
                },
                args: taken_args,
                type_args: vec![],
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(arg, adapters, entry_source);
                }
            }
            return;
        }
    }

    // Check if this is an effect-like Call that should be rewritten
    let is_effect_call = matches!(&expr.kind, TirExprKind::Call { func, .. }
        if func.module_source().is_effect_like() && func.module_source().effect_name().is_some());
    if is_effect_call
        && let TirExprKind::Call {
            func,
            args,
            type_args,
        } = &mut expr.kind
    {
        let effect_name = func.module_source().effect_name().unwrap_or_default();
        let method_name = func.name();
        let qualified = format!("{effect_name}::{method_name}");

        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Fix up adapter function types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if adapter.return_type != expr.type_id {
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, expr.type_id);
                }
                for (i, arg) in args.iter().enumerate() {
                    if i < adapter.params.len() && adapter.params[i].type_id != arg.type_id {
                        let local_idx = adapter.params[i].local_index as usize;
                        adapter.params[i].type_id = arg.type_id;
                        if local_idx < adapter.local_types.len() {
                            adapter.local_types[local_idx] = arg.type_id;
                        }
                    }
                }
            }

            // Rewrite to call the adapter function
            *func = FunctionRef::Resolved {
                func: adapter_rc.clone(),
                module_source: entry_source.clone(),
            };
            *type_args = vec![];

            // Recurse into args
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
            return;
        }
    }

    // Check if this is a resource MethodCall that should be rewritten to target an adapter
    if let TirExprKind::MethodCall { func, .. } = &expr.kind
        && let Some(method_info) = func.method_info()
    {
        let qualified = format!(
            "{}::{}",
            method_info.base_struct_name, method_info.method_name
        );
        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Extract receiver and args before replacing
            let (taken_receiver, taken_args) =
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

            // Fix up adapter function types from the call site
            // The adapter params include self as the first param
            {
                let mut adapter = adapter_rc.borrow_mut();
                if adapter.return_type != expr.type_id {
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, expr.type_id);
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
                for (i, arg) in taken_args.iter().enumerate() {
                    let param_idx = i + 1; // +1 to skip self
                    if param_idx < adapter.params.len()
                        && adapter.params[param_idx].type_id != arg.type_id
                    {
                        let local_idx = adapter.params[param_idx].local_index as usize;
                        adapter.params[param_idx].type_id = arg.type_id;
                        if local_idx < adapter.local_types.len() {
                            adapter.local_types[local_idx] = arg.type_id;
                        }
                    }
                }
            }

            // Replace MethodCall with Call targeting the adapter
            // Prepend receiver to args
            let mut all_args = vec![taken_receiver];
            all_args.extend(taken_args);

            expr.kind = TirExprKind::Call {
                func: FunctionRef::Resolved {
                    func: adapter_rc.clone(),
                    module_source: entry_source.clone(),
                },
                args: all_args,
                type_args: vec![],
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(arg, adapters, entry_source);
                }
            }
            return;
        }
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Call { args, .. }
        | TirExprKind::EffectCall { args, .. }
        | TirExprKind::CmRawCall { args, .. }
        | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_calls_in_expr(receiver, adapters, entry_source);
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_calls_in_expr(callee, adapters, entry_source);
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_calls_in_block(block, adapters, entry_source);
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_calls_in_expr(left, adapters, entry_source);
            rewrite_calls_in_expr(right, adapters, entry_source);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { expr: inner } => {
            rewrite_calls_in_expr(inner, adapters, entry_source);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source);
            rewrite_calls_in_block(then_branch, adapters, entry_source);
            if let Some(blk) = else_branch {
                rewrite_calls_in_block(blk, adapters, entry_source);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            rewrite_calls_in_expr(e, adapters, entry_source);
            rewrite_calls_in_expr(index, adapters, entry_source);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source);
            for arm in arms {
                rewrite_calls_in_block(arm, adapters, entry_source);
            }
            rewrite_calls_in_block(default, adapters, entry_source);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rewrite_calls_in_expr(guard, adapters, entry_source);
                }
                rewrite_calls_in_expr(&mut arm.body, adapters, entry_source);
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_calls_in_expr(body, adapters, entry_source);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_calls_in_expr(functor, adapters, entry_source);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in &mut fields.iter_mut() {
                rewrite_calls_in_expr(&mut field.value, adapters, entry_source);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_calls_in_expr(value, adapters, entry_source);
        }
        _ => {} // Leaf nodes: no sub-expressions
    }
}

// ============================================================================
// Effect call collection
// ============================================================================

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
        TirStmtKind::IfPattern {
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
        TirStmtKind::Continue | TirStmtKind::LetPattern { .. } => {}
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
            if func.module_source().is_effect_like()
                && let Some(effect_name) = func.module_source().effect_name()
            {
                let method_name = func.name();
                let qualified = format!("{effect_name}::{method_name}");
                if wasi_registry.get_function(&qualified).is_some() {
                    effects.insert(qualified);
                }
            }
            for arg in args {
                collect_effect_calls_in_expr(arg, effects, wasi_registry);
            }
        }
        TirExprKind::EffectCall {
            effect_name,
            op_name,
            args,
            ..
        } => {
            // Collect async WASI calls (e.g., Stdout::write_via_stream)
            let qualified = format!("{effect_name}::{op_name}");
            if wasi_registry.get_function(&qualified).is_some() {
                effects.insert(qualified);
            }
            for arg in args {
                collect_effect_calls_in_expr(arg, effects, wasi_registry);
            }
        }
        TirExprKind::CmRawCall { args, .. } | TirExprKind::StaticCall { args, .. } => {
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
            if let Some(method_info) = func.method_info() {
                let qualified = format!(
                    "{}::{}",
                    method_info.base_struct_name, method_info.method_name
                );
                if wasi_registry.get_function(&qualified).is_some() {
                    effects.insert(qualified);
                }
            }
            collect_effect_calls_in_expr(receiver, effects, wasi_registry);
            for arg in args {
                collect_effect_calls_in_expr(arg, effects, wasi_registry);
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
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { expr: inner } => {
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
        | TirExprKind::Local { .. }
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Capture { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        // Catch-all for any remaining leaf or rare variants
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NamedType;

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType {
            name: name.to_string(),
            span: Span::new(0, 0, 1, 1),
        })
    }

    #[test]
    fn flatten_param_i32() {
        assert_eq!(flatten_param_type(&named_type("i32")), vec![TypeTable::I32]);
    }

    #[test]
    fn flatten_param_i64() {
        assert_eq!(flatten_param_type(&named_type("i64")), vec![TypeTable::I64]);
    }

    #[test]
    fn flatten_param_f64() {
        assert_eq!(flatten_param_type(&named_type("f64")), vec![TypeTable::F64]);
    }

    #[test]
    fn flatten_param_string() {
        assert_eq!(
            flatten_param_type(&named_type("String")),
            vec![TypeTable::I32, TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_bool() {
        assert_eq!(
            flatten_param_type(&named_type("bool")),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_unit() {
        assert!(flatten_param_type(&Type::Tuple(vec![])).is_empty());
    }

    #[test]
    fn adapter_name() {
        assert_eq!(
            adapter_func_name("Stdout", "write_via_stream"),
            "__cm_adapter__Stdout_write_via_stream"
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
        let stmts = synthesize_lower(&named_type("i32"), i32_const(42), i32_const(100), &mut 0);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_bool() {
        let value = TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            synth_span(),
        );
        let stmts = synthesize_lower(&named_type("bool"), value, i32_const(100), &mut 0);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_unit() {
        let value = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
        let stmts = synthesize_lower(&Type::Tuple(vec![]), value, i32_const(100), &mut 0);
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
                assert_eq!(func.name(), "i32_load");
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
                assert_eq!(func.name(), "cm_lower_string");
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
}
