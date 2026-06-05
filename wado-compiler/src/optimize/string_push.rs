//! Rewrite `buf.push_str("short_constant")` (≤8 ASCII bytes) into a
//! sequence of `buf.push(ch)` calls — eliminates the temporary `String`
//! allocation that the literal would otherwise materialize at lowering.
//!
//! Must run *before* `inline`: once the inliner expands `push_str`'s body
//! the `MethodCall` node is replaced by a labeled block, after which the
//! literal-recognising rewrite no longer fires.
//!
//! Identifies the two methods via their [`crate::compiler_item::CompilerItem`]
//! markers (`StringPushStr` / `StringPushChar`) so the pass does not depend
//! on the canonical paths of `String::push_str` / `String::push`.
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

use crate::compiler_item::CompilerItem;
use crate::nir::{
    CallArg, FunctionRef, NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind, NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirOptVisitor, opt_walk_block};
use crate::tir::TypeTable;

/// Maximum byte length of the literal that triggers the rewrite.
/// Matches the threshold of the former WIR pass; the per-byte
/// `push` is faster than `push_str` only when the cost saved by
/// avoiding the string allocation outweighs the per-`push` overhead.
const MAX_SHORT_PUSH_STR_LEN: usize = 8;

pub fn simplify_short_push_str(project: &mut NirPackage) -> bool {
    let Some(ctx) = Ctx::resolve(project) else {
        return false;
    };
    let mut visitor = ShortPushStrVisitor { ctx };
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(mut body) = func.body_block() {
            changed |= visitor.visit_block(&mut body);
            func.set_body_block(body);
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
            match f.compiler_item {
                Some(CompilerItem::StringPushStr) => {
                    push_str = Some(FunctionRef::from_resolved(&f, f.module_source.clone()));
                }
                Some(CompilerItem::StringPushChar) => {
                    push_char = Some(FunctionRef::from_resolved(&f, f.module_source.clone()));
                }
                Some(_) | None => {}
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

struct ShortPushStrVisitor {
    ctx: Ctx,
}

impl NirOptVisitor for ShortPushStrVisitor {
    /// A statement-level visitor is the natural fit: `push_str` is a
    /// `&mut self` method whose result is dropped, so source-level call
    /// sites always appear as `NirStmtKind::Expr(MethodCall(...))`.
    /// `opt_walk_block` lets us return a fresh statement list when we
    /// expand one `push_str` into N `push` statements.
    fn visit_block(&mut self, block: &mut NirBlock) -> bool {
        // First recurse into nested blocks so inner candidates are
        // handled before we restructure the outer statement vector.
        let mut changed = opt_walk_block(self, block);

        let mut new_stmts: Vec<NirStmt> = Vec::with_capacity(block.stmts.len());
        for stmt in std::mem::take(&mut block.stmts) {
            if let NirStmtKind::Expr(expr) = &stmt.kind
                && let Some(replacements) = try_split_stmt(expr, &self.ctx)
            {
                new_stmts.extend(replacements);
                changed = true;
            } else {
                new_stmts.push(stmt);
            }
        }
        block.stmts = new_stmts;
        changed
    }
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
    // `push_str` takes `&String`, so every call site — source-level
    // `push_str(&"...")` and template lowering alike — passes the literal
    // through an explicit `Ref`. Match through it to reach the
    // `StringLiteral`.
    let NirExprKind::Unary {
        op: NirUnaryOp::Ref,
        expr: inner,
    } = &args[0].expr.kind
    else {
        return None;
    };
    let NirExprKind::StringLiteral(s) = &inner.kind else {
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
/// `String::push_str`'s `&mut self` parameter constrains what a NIR
/// `MethodCall` receiver can syntactically be: it must be a place, so
/// in practice we only ever see `Local`, an `&mut`-wrapped `Local`, or
/// a `FieldAccess` chain rooted at a `Local`. The broader leaves
/// (`Capture`, `GlobalVarGet`) are accepted defensively because they
/// are pure reads with no observable side effects of their own — were
/// they to appear here, cloning them would still be sound.
fn is_duplicable_receiver(e: &NirExpr) -> bool {
    match &e.kind {
        NirExprKind::Local { .. } | NirExprKind::GlobalVarGet { .. } => true,
        NirExprKind::FieldAccess { expr: inner, .. } => is_duplicable_receiver(inner),
        NirExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => is_duplicable_receiver(inner),
        NirExprKind::Unary { .. }
        | NirExprKind::Binary { .. }
        | NirExprKind::Cast { .. }
        | NirExprKind::Assign { .. }
        | NirExprKind::Index { .. }
        | NirExprKind::Call { .. }
        | NirExprKind::MethodCall { .. }
        | NirExprKind::CmRawCall { .. }
        | NirExprKind::IndirectCall { .. }
        | NirExprKind::ClosureToCanonical { .. }
        | NirExprKind::Block(_)
        | NirExprKind::LabeledBlock { .. }
        | NirExprKind::If { .. }
        | NirExprKind::Match { .. }
        | NirExprKind::Switch { .. }
        | NirExprKind::StructLiteral { .. }
        | NirExprKind::TupleLiteral { .. }
        | NirExprKind::ArrayLiteral { .. }
        | NirExprKind::VariantConstruct { .. }
        | NirExprKind::VariantTag { .. }
        | NirExprKind::VariantTest { .. }
        | NirExprKind::VariantPayload { .. }
        | NirExprKind::GlobalVarSet { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => false,
    }
}
