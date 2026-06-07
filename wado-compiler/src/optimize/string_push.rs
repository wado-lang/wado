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
//!
//! Ported to the worklist rewrite engine (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a block-level
//! [`Rule`]: `push_str` is a `&mut self` method whose result is dropped, so it
//! always appears as a statement, and expanding it produces N statements — a
//! statement-list edit (`set_block_stmts`). Nested blocks are separate worklist
//! nodes, so the rule only ever rewrites one block's direct statement list,
//! matching the old visitor's recurse-then-rewrite order.

use crate::compiler_item::CompilerItem;
use crate::nir::{FunctionRef, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, Body, ExprId, ExprKind, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;

/// Maximum byte length of the literal that triggers the rewrite.
/// Matches the threshold of the former WIR pass; the per-byte
/// `push` is faster than `push_str` only when the cost saved by
/// avoiding the string allocation outweighs the per-`push` overhead.
const MAX_SHORT_PUSH_STR_LEN: usize = 8;

/// Resolve the whole-package context for the short-`push_str` rule, or `None`
/// when the `String::push_str` / `push` markers are absent. Public to the
/// `optimize` module so the unified [`super::peephole`] pass can build the rule
/// alongside the other peephole rules over one shared engine session.
pub(super) fn resolve_ctx(project: &NirPackage) -> Option<Ctx> {
    Ctx::resolve(project)
}

pub(super) struct Ctx {
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

pub(super) struct ShortPushStrRule {
    ctx: Ctx,
}

impl ShortPushStrRule {
    pub(super) fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }
}

impl Rule for ShortPushStrRule {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        let mut new_stmts: Vec<StmtId> = Vec::with_capacity(stmts.len());
        let mut changed = false;
        for stmt in stmts {
            if let Some(replacements) = try_split_stmt(engine, stmt, &self.ctx) {
                new_stmts.extend(replacements);
                changed = true;
            } else {
                new_stmts.push(stmt);
            }
        }
        if changed {
            engine.set_block_stmts(id, new_stmts);
        }
        changed
    }
}

/// If `stmt` is a `place.push_str("short")` statement with a duplicable
/// receiver and a short ASCII literal, build the equivalent per-byte
/// `place.push(ch)` statements and return them; otherwise `None`.
fn try_split_stmt(engine: &mut Engine, stmt: StmtId, ctx: &Ctx) -> Option<Vec<StmtId>> {
    let StmtKind::Expr(expr_id) = engine.body.stmts[stmt].kind else {
        return None;
    };

    let (receiver, arg0) = {
        let ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } = &engine.body.exprs[expr_id].kind
        else {
            return None;
        };
        if !func_matches(func, &ctx.push_str) || args.len() != 1 {
            return None;
        }
        (*receiver, args[0].expr)
    };

    if !is_duplicable_receiver(&*engine.body, receiver) {
        return None;
    }

    // `push_str` takes `&String`, so every call site — source-level
    // `push_str(&"...")` and template lowering alike — passes the literal
    // through an explicit `Ref`. Match through it to reach the
    // `StringLiteral`.
    let s = {
        let ExprKind::Unary {
            op: NirUnaryOp::Ref,
            expr: inner,
        } = &engine.body.exprs[arg0].kind
        else {
            return None;
        };
        let ExprKind::StringLiteral(s) = &engine.body.exprs[*inner].kind else {
            return None;
        };
        s.clone()
    };
    if s.is_empty() || s.len() > MAX_SHORT_PUSH_STR_LEN || !s.is_ascii() {
        return None;
    }

    let span = engine.body.exprs[expr_id].span;
    let mut stmts = Vec::with_capacity(s.len());
    for byte in s.bytes() {
        let ch = char::from(byte);
        let recv_clone = engine.clone_expr(receiver);
        let char_arg = engine.alloc_expr(ExprKind::CharLiteral(ch), TypeTable::CHAR, span);
        let call = engine.alloc_expr(
            ExprKind::MethodCall {
                receiver: recv_clone,
                func: ctx.push_char.clone(),
                type_args: Vec::new(),
                args: vec![ArenaCallArg {
                    expr: char_arg,
                    is_mut: false,
                }],
            },
            TypeTable::UNIT,
            span,
        );
        stmts.push(engine.alloc_stmt(StmtKind::Expr(call), span));
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
/// (`GlobalVarGet`) are accepted defensively because they are pure reads
/// with no observable side effects of their own — were they to appear
/// here, cloning them would still be sound.
fn is_duplicable_receiver(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::Local { .. } | ExprKind::GlobalVarGet { .. } => true,
        ExprKind::FieldAccess { expr: inner, .. } => is_duplicable_receiver(body, *inner),
        ExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => is_duplicable_receiver(body, *inner),
        _ => false,
    }
}
