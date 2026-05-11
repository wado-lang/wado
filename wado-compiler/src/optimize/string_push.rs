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

use crate::nir::{
    CallArg, FunctionRef, NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind, NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;
use crate::wir::{COMP_FEATURE_STRING_PUSH_CHAR, COMP_FEATURE_STRING_PUSH_STR};

/// Maximum byte length of the literal that triggers the rewrite.
/// Matches the threshold of the former WIR pass; the per-byte
/// `push` is faster than `push_str` only when the cost saved by
/// avoiding the string allocation outweighs the per-`push` overhead.
const MAX_SHORT_PUSH_STR_LEN: usize = 8;

pub fn simplify_short_push_str(project: &mut NirPackage) -> bool {
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
    fn resolve(project: &NirPackage) -> Option<Self> {
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

fn rewrite_block(block: &mut NirBlock, ctx: &Ctx) -> bool {
    let mut changed = false;
    let mut new_stmts: Vec<NirStmt> = Vec::with_capacity(block.stmts.len());
    for mut stmt in std::mem::take(&mut block.stmts) {
        if let NirStmtKind::Expr(expr) = &stmt.kind
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

fn try_split_stmt(expr: &NirExpr, ctx: &Ctx) -> Option<Vec<NirStmt>> {
    let NirExprKind::MethodCall {
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
    let NirExprKind::StringLiteral(s) = &args[0].expr.kind else {
        return None;
    };
    if s.is_empty() || s.len() > MAX_SHORT_PUSH_STR_LEN || !s.is_ascii() {
        return None;
    }

    let span = expr.span;
    let mut stmts = Vec::with_capacity(s.len());
    for byte in s.bytes() {
        let ch = char::from(byte);
        let char_arg = NirExpr::new(NirExprKind::CharLiteral(ch), TypeTable::CHAR, span);
        let kind = NirExprKind::method_call(
            Box::new((**receiver).clone()),
            ctx.push_char.clone(),
            Vec::new(),
            vec![CallArg::new(char_arg, false)],
        );
        let call_expr = NirExpr::new(kind, TypeTable::UNIT, span);
        stmts.push(NirStmt::new(NirStmtKind::Expr(call_expr), span));
    }
    Some(stmts)
}

/// Receivers we are willing to clone N times. The set is intentionally
/// narrow: anything that may allocate, trap, or be observably stateful is
/// excluded so duplicating it cannot change semantics.
///
/// `String::push_str`'s `&mut self` parameter constrains what a TIR
/// `MethodCall` receiver can syntactically be: it must be a place, so
/// in practice we only ever see `Local`, an `&mut`-wrapped `Local`, or
/// a `FieldAccess` chain rooted at a `Local`. The broader leaves
/// (`Capture`, `GlobalVarGet`) are accepted defensively because they
/// are pure reads with no observable side effects of their own — were
/// they to appear here, cloning them would still be sound.
fn is_duplicable_receiver(e: &NirExpr) -> bool {
    match &e.kind {
        NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. } => true,
        NirExprKind::FieldAccess { expr: inner, .. } => is_duplicable_receiver(inner),
        NirExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => is_duplicable_receiver(inner),
        _ => false,
    }
}

fn rewrite_stmt(stmt: &mut NirStmt, ctx: &Ctx, changed: &mut bool) {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            *changed |= rewrite_expr(value, ctx);
        }
        NirStmtKind::Expr(expr) => {
            *changed |= rewrite_expr(expr, ctx);
        }
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                *changed |= rewrite_expr(v, ctx);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            *changed |= rewrite_block(body, ctx);
        }
        NirStmtKind::Continue => {}
    }
}

/// Walk an expression looking for nested `NirBlock`s whose statements may
/// hold rewritable `push_str` calls. The structural traversal mirrors
/// `value_copy_elide::strip_in_expr` so every block-bearing variant is
/// reached.
fn rewrite_expr(expr: &mut NirExpr, ctx: &Ctx) -> bool {
    match &mut expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            rewrite_block(block, ctx)
        }
        NirExprKind::If {
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
        NirExprKind::Match { expr: scrut, arms } => {
            let mut c = rewrite_expr(scrut, ctx);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    c |= rewrite_expr(g, ctx);
                }
                c |= rewrite_expr(&mut arm.body, ctx);
            }
            c
        }
        NirExprKind::Switch {
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
        NirExprKind::Call { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= rewrite_expr(&mut arg.expr, ctx);
            }
            c
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            let mut c = rewrite_expr(receiver, ctx);
            for arg in args {
                c |= rewrite_expr(&mut arg.expr, ctx);
            }
            c
        }
        NirExprKind::CmRawCall { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= rewrite_expr(arg, ctx);
            }
            c
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            let mut c = rewrite_expr(callee, ctx);
            for arg in args {
                c |= rewrite_expr(arg, ctx);
            }
            c
        }
        NirExprKind::Binary { left, right, .. } => {
            rewrite_expr(left, ctx) | rewrite_expr(right, ctx)
        }
        NirExprKind::Assign { target, value } => {
            rewrite_expr(target, ctx) | rewrite_expr(value, ctx)
        }
        NirExprKind::Index { expr: inner, index } => {
            rewrite_expr(inner, ctx) | rewrite_expr(index, ctx)
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. } => rewrite_expr(inner, ctx),
        NirExprKind::StructLiteral { fields, .. } => {
            let mut c = false;
            for field in fields {
                c |= rewrite_expr(&mut field.value, ctx);
            }
            c
        }
        NirExprKind::TupleLiteral { elements } => {
            let mut c = false;
            for elem in elements {
                c |= rewrite_expr(elem, ctx);
            }
            c
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_expr(p, ctx)
            } else {
                false
            }
        }
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => false,
    }
}
