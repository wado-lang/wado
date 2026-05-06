//! Rewrite `buf.push_str("short_constant")` (≤8 ASCII bytes) into a
//! sequence of `buf.push(ch)` calls — eliminates the temporary `String`
//! allocation that the literal would otherwise materialize at lowering.
//!
//! Must run *before* `inline`: once the inliner expands `push_str`'s body
//! the `MethodCall` node is replaced by a labeled block, after which the
//! literal-recognising rewrite no longer fires.
//!
//! Identifies the two methods by their `comp_features` flags
//! (`COMP_FEATURE_STRING_PUSH_STR` / `COMP_FEATURE_STRING_PUSH_CHAR`)
//! so the pass does not depend on the canonical paths of `String::push_str`
//! / `String::push`.
//!
//! The receiver is duplicated once per output `push` call. We only rewrite
//! when the receiver is one of the syntactically pure forms accepted by
//! [`is_duplicable_receiver`] — anything that could allocate, trap, or
//! observe state is left alone.
//!
//! ASCII-only: each output `push` is given a `CharLiteral` whose code point
//! equals the source byte. For byte ≥ 0x80 that would push raw UTF-8
//! continuation bytes through `push`, which expects a Unicode scalar and
//! would re-encode them — corrupting the string. We therefore skip any
//! literal containing a non-ASCII byte.
//!
//! Empty literals are also skipped: `push_str("")` is a no-op already.

use crate::flat_package::FlatPackage;
use crate::tir::{
    CallArg, FunctionRef, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirUnaryOp,
    TypeTable,
};
use crate::wir::{COMP_FEATURE_STRING_PUSH_CHAR, COMP_FEATURE_STRING_PUSH_STR};

/// Maximum byte length of the literal that triggers the rewrite.
/// Matches the threshold of the former WIR pass; the per-byte
/// `push` is faster than `push_str` only when the cost saved by
/// avoiding the string allocation outweighs the per-`push` overhead.
const MAX_SHORT_PUSH_STR_LEN: usize = 8;

pub fn simplify_short_push_str(project: &mut FlatPackage) -> bool {
    let Some(ctx) = Ctx::resolve(project) else {
        return false;
    };
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = &mut func.body {
            changed |= rewrite_block(body, &ctx);
        }
    }
    changed
}

struct Ctx {
    push_str: FunctionRef,
    push_char: FunctionRef,
}

impl Ctx {
    fn resolve(project: &FlatPackage) -> Option<Self> {
        let mut push_str: Option<FunctionRef> = None;
        let mut push_char: Option<FunctionRef> = None;
        for func_rc in &project.functions {
            let f = func_rc.borrow();
            if f.comp_features & COMP_FEATURE_STRING_PUSH_STR != 0 {
                push_str = Some(FunctionRef::from_resolved(&f, f.module_source.clone()));
            }
            if f.comp_features & COMP_FEATURE_STRING_PUSH_CHAR != 0 {
                push_char = Some(FunctionRef::from_resolved(&f, f.module_source.clone()));
            }
        }
        Some(Self {
            push_str: push_str?,
            push_char: push_char?,
        })
    }
}

fn func_matches(func: &FunctionRef, target: &FunctionRef) -> bool {
    func.module_source == target.module_source && func.name == target.name
}

fn rewrite_block(block: &mut TirBlock, ctx: &Ctx) -> bool {
    let mut changed = false;
    let mut new_stmts: Vec<TirStmt> = Vec::with_capacity(block.stmts.len());
    for mut stmt in std::mem::take(&mut block.stmts) {
        if let TirStmtKind::Expr(expr) = &stmt.kind
            && let Some(replacements) = try_split_stmt(expr, ctx)
        {
            new_stmts.extend(replacements);
            changed = true;
            continue;
        }
        rewrite_stmt(&mut stmt, ctx, &mut changed);
        new_stmts.push(stmt);
    }
    block.stmts = new_stmts;
    changed
}

fn try_split_stmt(expr: &TirExpr, ctx: &Ctx) -> Option<Vec<TirStmt>> {
    let TirExprKind::MethodCall {
        receiver,
        func,
        args,
        ..
    } = &expr.kind
    else {
        return None;
    };
    if !func_matches(func, &ctx.push_str) || args.len() != 1 {
        return None;
    }
    if !is_duplicable_receiver(receiver) {
        return None;
    }
    let TirExprKind::StringLiteral(s) = &args[0].expr.kind else {
        return None;
    };
    if s.is_empty() || s.len() > MAX_SHORT_PUSH_STR_LEN || !s.is_ascii() {
        return None;
    }

    let span = expr.span;
    let mut stmts = Vec::with_capacity(s.len());
    for byte in s.bytes() {
        let ch = char::from(byte);
        let char_arg = TirExpr::new(TirExprKind::CharLiteral(ch), TypeTable::CHAR, span);
        let kind = TirExprKind::method_call(
            Box::new((**receiver).clone()),
            ctx.push_char.clone(),
            Vec::new(),
            vec![CallArg::new(char_arg, false)],
        );
        let call_expr = TirExpr::new(kind, TypeTable::UNIT, span);
        stmts.push(TirStmt::new(TirStmtKind::Expr(call_expr), span));
    }
    Some(stmts)
}

/// Receivers we are willing to clone N times. The set is intentionally
/// narrow: anything that may allocate, trap, or be observably stateful is
/// excluded so duplicating it cannot change semantics.
fn is_duplicable_receiver(e: &TirExpr) -> bool {
    match &e.kind {
        TirExprKind::Local { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::GlobalVarGet { .. } => true,
        TirExprKind::FieldAccess { expr: inner, .. } => is_duplicable_receiver(inner),
        TirExprKind::Unary {
            op: TirUnaryOp::Deref | TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } => is_duplicable_receiver(inner),
        _ => false,
    }
}

fn rewrite_stmt(stmt: &mut TirStmt, ctx: &Ctx, changed: &mut bool) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            *changed |= rewrite_expr(value, ctx);
        }
        TirStmtKind::Expr(expr) | TirStmtKind::TaskReturn { value: expr } => {
            *changed |= rewrite_expr(expr, ctx);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                *changed |= rewrite_expr(v, ctx);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            *changed |= rewrite_expr(condition, ctx);
            *changed |= rewrite_block(then_block, ctx);
            if let Some(eb) = else_block {
                *changed |= rewrite_block(eb, ctx);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            *changed |= rewrite_block(body, ctx);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            *changed |= rewrite_expr(scrutinee, ctx);
            *changed |= rewrite_block(then_block, ctx);
            if let Some(eb) = else_block {
                *changed |= rewrite_block(eb, ctx);
            }
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            *changed |= rewrite_expr(iterable, ctx);
            *changed |= rewrite_block(body, ctx);
        }
        TirStmtKind::Continue => {}
    }
}

/// Walk an expression looking for nested `TirBlock`s whose statements may
/// hold rewritable `push_str` calls. The structural traversal mirrors
/// `value_copy_elide::strip_in_expr` so every block-bearing variant is
/// reached.
fn rewrite_expr(expr: &mut TirExpr, ctx: &Ctx) -> bool {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_block(block, ctx)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut c = rewrite_expr(condition, ctx);
            c |= rewrite_block(then_branch, ctx);
            if let Some(eb) = else_branch {
                c |= rewrite_block(eb, ctx);
            }
            c
        }
        TirExprKind::Match { expr: scrut, arms } => {
            let mut c = rewrite_expr(scrut, ctx);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    c |= rewrite_expr(g, ctx);
                }
                c |= rewrite_expr(&mut arm.body, ctx);
            }
            c
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut c = rewrite_expr(scrutinee, ctx);
            for arm in arms {
                c |= rewrite_block(arm, ctx);
            }
            c |= rewrite_block(default, ctx);
            c
        }
        TirExprKind::Call { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= rewrite_expr(&mut arg.expr, ctx);
            }
            c
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let mut c = rewrite_expr(receiver, ctx);
            for arg in args {
                c |= rewrite_expr(&mut arg.expr, ctx);
            }
            c
        }
        TirExprKind::CmRawCall { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= rewrite_expr(arg, ctx);
            }
            c
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            let mut c = rewrite_expr(callee, ctx);
            for arg in args {
                c |= rewrite_expr(arg, ctx);
            }
            c
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_expr(left, ctx) | rewrite_expr(right, ctx)
        }
        TirExprKind::Assign { target, value } => {
            rewrite_expr(target, ctx) | rewrite_expr(value, ctx)
        }
        TirExprKind::Index { expr: inner, index } => {
            rewrite_expr(inner, ctx) | rewrite_expr(index, ctx)
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. } => rewrite_expr(inner, ctx),
        TirExprKind::StructLiteral { fields, .. } => {
            let mut c = false;
            for field in fields {
                c |= rewrite_expr(&mut field.value, ctx);
            }
            c
        }
        TirExprKind::TupleLiteral { elements } => {
            let mut c = false;
            for elem in elements {
                c |= rewrite_expr(elem, ctx);
            }
            c
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_expr(p, ctx)
            } else {
                false
            }
        }
        TirExprKind::Closure { body, .. } => rewrite_expr(body, ctx),
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => false,
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => unreachable!(
            "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
        ),
    }
}
