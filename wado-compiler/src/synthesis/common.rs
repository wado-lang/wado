//! Common utilities for TIR synthesis phases.
//!
//! Provides shared helpers for constructing synthetic TIR nodes:
//! - Synthetic span generation
//! - TIR expression and statement builders
//! - Synthetic function creation

use crate::hashmap::IndexSet;

use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirLocal, TirParam, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::token::Span;

/// Synthetic span used for all generated code.
///
/// All synthesized TIR nodes share this span, making them identifiable
/// as compiler-generated rather than user-written code.
pub fn synth_span() -> Span {
    Span::new(0, 0, 1, 1)
}

/// Create a call to a builtin function (e.g., `builtin::i32_load`).
pub fn builtin_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::builtin(),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args: args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        },
        return_type,
        synth_span(),
    )
}

/// Create a call to an internal function (e.g., `internal::cm_lower_string`).
pub fn internal_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::internal(),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args: args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        },
        return_type,
        synth_span(),
    )
}

/// Create a call to a synthesized function in the entry module.
pub fn entry_call(
    name: &str,
    args: Vec<TirExpr>,
    return_type: TypeId,
    entry_module_source: ModuleSource,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: entry_module_source,
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args: args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        },
        return_type,
        synth_span(),
    )
}

/// Create an i32 literal expression.
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
pub fn binary(op: TirBinaryOp, left: TirExpr, right: TirExpr, ty: TypeId) -> TirExpr {
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
            skip_value_copy: false,
        },
        synth_span(),
    )
}

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
            skip_value_copy: false,
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

/// Create an `Option::Some(value)` expression.
pub fn option_some(value: TirExpr, option_type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantConstruct {
            variant_type: option_type_id,
            case_index: 0,
            case_name: "Some".to_string(),
            payload: Some(Box::new(value)),
        },
        option_type_id,
        synth_span(),
    )
}

/// Create a `null` (`Option::None`) expression.
pub fn option_none(option_type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantConstruct {
            variant_type: option_type_id,
            case_index: 1,
            case_name: "None".to_string(),
            payload: None,
        },
        option_type_id,
        synth_span(),
    )
}

/// Create a default/zero value expression (ref.null for reference types, 0 for integers).
/// Used as a placeholder initial value for mutable locals in synthesized code.
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

/// Allocate a local variable, returning its index. The synthesised local
/// is named `__local_N` (matching the codegen fallback convention) and
/// marked immutable; sites that need a more descriptive name should use
/// `alloc_named_local` instead.
pub fn alloc_local(next_local: &mut u32, locals: &mut Vec<TirLocal>, ty: TypeId) -> u32 {
    alloc_named_local(next_local, locals, None, ty, false)
}

/// Allocate a local with an explicit name and mutability. Pass `name = None`
/// to get the default `__local_N` synthesised name.
pub fn alloc_named_local(
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    name: Option<String>,
    ty: TypeId,
    is_mut: bool,
) -> u32 {
    let idx = *next_local;
    *next_local += 1;
    let name = name.unwrap_or_else(|| format!("__local_{idx}"));
    locals.push(TirLocal {
        name,
        type_id: ty,
        is_mut,
    });
    idx
}

/// Seed a synthesised function's `locals` table from its parameter list.
/// The returned vector holds one `TirLocal` per param at indices `0..N`,
/// inheriting each param's name, type, and mutability so the parameter
/// slot's metadata matches the corresponding `TirParam`.
pub fn locals_from_params(params: &[TirParam]) -> Vec<TirLocal> {
    params
        .iter()
        .map(|p| TirLocal {
            name: p.name.clone(),
            type_id: p.type_id,
            is_mut: p.is_mut,
        })
        .collect()
}

/// Build a single `TirLocal` for a synthesised parameter slot. Convenience
/// wrapper around the literal struct construction so synthesis sites can
/// seed their `locals` table with `vec![param_local(...), ...]`.
pub fn param_local(name: &str, type_id: TypeId, is_mut: bool) -> TirLocal {
    TirLocal {
        name: name.to_string(),
        type_id,
        is_mut,
    }
}

/// Create a static call to a generic struct method with proper monomorphization info.
///
/// For example, `Array::<String>::with_capacity(n)` needs:
/// - `method_info` with `struct_name: "Array"`, `method_name: "with_capacity"`
/// - `monomorph_info` with `generic_name: "Array::with_capacity"`, `type_args: [String]`
///
/// Without these, the monomorphizer won't instantiate the generic method.
pub fn generic_static_call(
    struct_name: &str,
    method_name: &str,
    module_source: ModuleSource,
    type_args: Vec<TypeId>,
    args: Vec<TirExpr>,
    return_type: TypeId,
) -> TirExpr {
    let info = LocalMethodName::new(struct_name.to_string(), None, method_name.to_string());
    let mangled_name = info.to_mangled_name();
    let monomorph_info = if type_args.is_empty() {
        None
    } else {
        Some(MonomorphInfo {
            generic_name: mangled_name.clone(),
            impl_type_args: type_args,
            method_type_args: vec![],
            is_blanket: false,
        })
    };
    let _n = args.len();
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source,
                name: mangled_name,
                monomorph_info,
                method_info: Some(info),
            },
            type_args: vec![],
            args: args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        },
        return_type,
        synth_span(),
    )
}

/// Create a method call on a generic struct with proper monomorphization info.
///
/// For example, `arr.push(elem)` where `arr: Array<String>` needs:
/// - `method_info` with `struct_name: "Array"`, `method_name: "push"`
/// - The receiver's `type_id` must be the concrete `Array<String>` `TypeId`
pub fn generic_method_call(
    receiver: TirExpr,
    struct_name: &str,
    method_name: &str,
    method_module_source: ModuleSource,
    args: Vec<TirExpr>,
    return_type: TypeId,
) -> TirExpr {
    let info = LocalMethodName::new(struct_name.to_string(), None, method_name.to_string());
    let mangled_name = info.to_mangled_name();
    let _n = args.len();
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: method_module_source,
                name: mangled_name,
                monomorph_info: None,
                method_info: Some(info),
            },
            vec![],
            args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        ),
        return_type,
        synth_span(),
    )
}

/// Create a synthetic `TirFunction` for an auto-derived trait method.
pub fn make_synthetic_method(
    name: String,
    method_info: LocalMethodName,
    params: Vec<TirParam>,
    return_type: TypeId,
    body: TirBlock,
    locals: Vec<TirLocal>,
) -> TirFunction {
    let local_count = locals.len() as u32;

    TirFunction {
        module_source: ModuleSource::default(),
        name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params,
        return_type,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span: synth_span(),
        local_count,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    }
}

/// Build a `f.write_str(&"text")` statement using Formatter's `write_str`
/// method. `Formatter::write_str` takes `&String`, so the literal is
/// wrapped in an explicit `Ref`; `ref_string_type` is the cached `&String`
/// type id the caller already produced from `string_type`.
pub fn write_str_stmt(
    text: impl Into<String>,
    fmt: TirExpr,
    string_type: TypeId,
    ref_string_type: TypeId,
    span: Span,
) -> TirStmt {
    let literal = TirExpr::new(TirExprKind::StringLiteral(text.into()), string_type, span);
    let arg = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(literal),
        },
        ref_string_type,
        span,
    );
    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(fmt),
            FunctionRef {
                module_source: ModuleSource::format(),
                name: "Formatter::write_str".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "Formatter".to_string(),
                    None,
                    "write_str".to_string(),
                )),
            },
            vec![],
            vec![CallArg::new(arg, false)],
        ),
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Build a trait method call statement: `receiver.TraitName::method(args)`.
pub fn trait_method_call(
    receiver: TirExpr,
    method_info: LocalMethodName,
    module_source: ModuleSource,
    args: Vec<TirExpr>,
    span: Span,
) -> TirStmt {
    let fn_name = method_info.to_mangled_name();
    let _n = args.len();
    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source,
                name: fn_name,
                monomorph_info: None,
                method_info: Some(method_info),
            },
            vec![],
            args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        ),
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Create a dereference expression: `*expr`.
pub fn deref_expr(expr: TirExpr, inner_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(expr),
        },
        inner_type,
        span,
    )
}

/// Create a reference expression: `&expr`.
pub fn ref_expr(expr: TirExpr, ref_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(expr),
        },
        ref_type,
        span,
    )
}

/// Create a string literal expression.
pub fn string_lit(text: impl Into<String>, string_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(TirExprKind::StringLiteral(text.into()), string_type, span)
}

/// Create a field access expression: `expr.field_name`.
pub fn field_access(
    expr: TirExpr,
    field_index: u32,
    field_name: impl Into<String>,
    field_type: TypeId,
    span: Span,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::FieldAccess {
            expr: Box::new(expr),
            field_index,
            field_name: field_name.into(),
        },
        field_type,
        span,
    )
}
