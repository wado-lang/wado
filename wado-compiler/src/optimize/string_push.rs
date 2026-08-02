//! Two composed string-append rewrites over the shared peephole session:
//!
//! * [`ShortPushStrRule`] rewrites `buf.push_str("short_constant")` (≤8 ASCII
//!   bytes) into a sequence of `buf.push(ch)` calls — eliminating the temporary
//!   `String` allocation that the literal would otherwise materialize at
//!   lowering.
//! * [`ConstAsciiPushRule`] then retargets each `buf.push(<const char < 0x80>)`
//!   — the ones above, plus any direct constant-ASCII `push` — to
//!   `buf.push_ascii_unchecked(<byte>)`, skipping `encode_char`'s UTF-8 width dispatch.
//!
//! Must run *before* `inline`: once the inliner expands `push_str`'s body
//! the call node is replaced by a labeled block, after which the
//! literal-recognising rewrite no longer fires.
//!
//! Identifies the methods via their [`crate::compiler_item::CompilerItem`]
//! markers (`StringPushStr` / `StringPushChar` / `StringPushAscii`) so the pass
//! does not depend on the canonical paths of `String::push_str` /
//! `String::push` / `String::push_ascii_unchecked`.
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
use crate::nir::NirUnaryOp;
use crate::nir_arena::{ArenaCallArg, BlockId, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
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
    /// `FuncId` of `push_str`, the call this rule recognizes.
    push_str_id: crate::nir::FuncId,
    /// `FuncId` of `push_char`, captured at resolution so the synthesized
    /// per-byte `push(ch)` calls are born resolved.
    push_char_id: crate::nir::FuncId,
    /// `FuncId` of `push_ascii_unchecked`, the retarget for a constant-ASCII
    /// `push`. Independent of the two above: absent (`None`) it only disables
    /// [`ConstAsciiPushRule`], leaving [`ShortPushStrRule`] intact.
    push_ascii_id: Option<crate::nir::FuncId>,
}

impl Ctx {
    fn resolve(project: &NirPackage) -> Option<Self> {
        let mut push_str_id: Option<crate::nir::FuncId> = None;
        let mut push_char_id: Option<crate::nir::FuncId> = None;
        let mut push_ascii_id: Option<crate::nir::FuncId> = None;
        for func_rc in &project.functions {
            let f = func_rc.borrow();
            match f.compiler_item {
                Some(CompilerItem::StringPushStr) => {
                    push_str_id = Some(f.id.expect("func_id assigned at lower"));
                }
                Some(CompilerItem::StringPushChar) => {
                    push_char_id = Some(f.id.expect("func_id assigned at lower"));
                }
                Some(CompilerItem::StringPushAscii) => {
                    push_ascii_id = Some(f.id.expect("func_id assigned at lower"));
                }
                Some(_) | None => {}
            }
        }
        Some(Self {
            push_str_id: push_str_id?,
            push_char_id: push_char_id?,
            push_ascii_id,
        })
    }
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

/// Retarget `buf.push(<const char < 0x80>)` to
/// `buf.push_ascii_unchecked(<byte>)`, skipping `encode_char`'s UTF-8 width
/// dispatch: a constant ASCII scalar is always a one-byte sequence. Composes
/// with [`ShortPushStrRule`] — the per-byte `push(ch)` calls it emits for a
/// short constant `push_str` are all ASCII, so they flow straight into this
/// rule on the shared worklist.
///
/// `push` is `&mut self`-returning-unit, so its call is always a statement
/// expression; the rewrite is an in-place call edit (swap the callee
/// and coerce the `char` argument to its `u8` byte).
pub(super) struct ConstAsciiPushRule {
    push_char_id: crate::nir::FuncId,
    push_ascii_id: crate::nir::FuncId,
}

impl ConstAsciiPushRule {
    /// `None` when the `push_ascii_unchecked` marker is absent — the rule has no
    /// retarget to point at, so it is simply not built.
    pub(super) fn new(ctx: &Ctx) -> Option<Self> {
        Some(Self {
            push_char_id: ctx.push_char_id,
            push_ascii_id: ctx.push_ascii_id?,
        })
    }
}

impl Rule for ConstAsciiPushRule {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let (receiver, arg0, type_args_empty) = {
            let ExprKind::Call {
                func_id,
                args,
                type_args,
                has_receiver: true,
            } = &engine.body.exprs[id].kind
            else {
                return false;
            };
            let [receiver, arg0] = args.as_slice() else {
                return false;
            };
            if *func_id != self.push_char_id {
                return false;
            }
            (receiver.expr, arg0.expr, type_args.is_empty())
        };
        if !type_args_empty {
            return false;
        }
        let Some(ch) = engine.body.operand_const_char(arg0) else {
            return false;
        };
        let code = u32::from(ch);
        if code >= 0x80 {
            return false;
        }
        let byte_arg = engine.const_operand(
            crate::nir_value_graph::ValueKind::Int(u64::from(code), TypeTable::U8),
            TypeTable::U8,
        );
        engine.replace_expr_kind(
            id,
            ExprKind::method_call(
                self.push_ascii_id,
                receiver,
                true,
                vec![ArenaCallArg {
                    expr: byte_arg,
                    is_mut: false,
                }],
            ),
        );
        true
    }
}

/// If `stmt` is a `place.push_str("short")` statement with a duplicable
/// receiver and a short ASCII literal, build the equivalent per-byte
/// `place.push(ch)` statements and return them; otherwise `None`.
fn try_split_stmt(engine: &mut Engine, stmt: StmtId, ctx: &Ctx) -> Option<Vec<StmtId>> {
    let StmtKind::Expr(Operand::Expr(expr_id)) = engine.body.stmts[stmt].kind else {
        return None;
    };

    let (receiver, arg0) = {
        let (receiver, func_id, args) = engine.body.exprs[expr_id].kind.as_method_call()?;
        if func_id != ctx.push_str_id || args.len() != 1 {
            return None;
        }
        (receiver, args[0].expr)
    };

    let receiver_expr = receiver.as_expr()?;
    if !is_duplicable_receiver(&*engine.body, receiver_expr) {
        return None;
    }

    // `push_str` takes `&String`, so every call site — source-level
    // `push_str(&"...")` and template lowering alike — passes the literal
    // through an explicit `Ref`. Match through it to reach the string literal,
    // now a `StructLiteral String { repr: PackedArray(bytes), used }`, and read
    // the bytes off its packed `repr`. The expansion is byte-wise (each byte
    // becomes a `push_char`) and only fires for short ASCII literals, so we
    // gate on the borrowed `&[u8]` directly — no `String`/UTF-8 round-trip —
    // and copy out only the (bounded) bytes we will actually expand.
    let bytes: Vec<u8> = {
        let arg0_expr = arg0.as_expr()?;
        let ExprKind::Unary {
            op: NirUnaryOp::Ref,
            expr: inner,
        } = &engine.body.exprs[arg0_expr].kind
        else {
            return None;
        };
        let inner_e = inner.as_expr()?;
        let repr = {
            let ExprKind::StructLiteral { fields, .. } = &engine.body.exprs[inner_e].kind else {
                return None;
            };
            fields
                .iter()
                .find(|f| f.name == crate::compiler_item::SeqField::Backing.field_name())
                .map(|f| f.value)?
        };
        let repr_e = repr.as_expr()?;
        let ExprKind::PackedArray(bytes) = &engine.body.exprs[repr_e].kind else {
            return None;
        };
        if bytes.is_empty() || bytes.len() > MAX_SHORT_PUSH_STR_LEN || !bytes.is_ascii() {
            return None;
        }
        bytes.clone()
    };

    let span = engine.body.exprs[expr_id].span;
    let mut stmts = Vec::with_capacity(bytes.len());
    for &byte in &bytes {
        let ch = char::from(byte);
        let recv_clone = engine.clone_expr(receiver_expr);
        let char_arg =
            engine.const_operand(crate::nir_value_graph::ValueKind::Char(ch), TypeTable::CHAR);
        let call = engine.alloc_expr(
            ExprKind::method_call(
                ctx.push_char_id,
                recv_clone.into(),
                true,
                vec![ArenaCallArg {
                    expr: char_arg,
                    is_mut: false,
                }],
            ),
            TypeTable::UNIT,
            span,
        );
        stmts.push(engine.alloc_stmt(StmtKind::Expr(call.into()), span));
    }
    Some(stmts)
}

/// Receivers we are willing to clone N times. The set is intentionally
/// narrow: anything that may allocate, trap, or be observably stateful is
/// excluded so duplicating it cannot change semantics.
///
/// `String::push_str`'s `&mut self` parameter constrains what a NIR
/// call receiver can syntactically be: it must be a place, so
/// in practice we only ever see `Local`, an `&mut`-wrapped `Local`, or
/// a `FieldAccess` chain rooted at a `Local`. The broader leaves
/// (`GlobalVarGet`) are accepted defensively because they are pure reads
/// with no observable side effects of their own — were they to appear
/// here, cloning them would still be sound.
fn is_duplicable_receiver(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::Local { .. } | ExprKind::GlobalVarGet { .. } => true,
        ExprKind::FieldAccess { expr: inner, .. } => inner
            .as_expr()
            .is_some_and(|e| is_duplicable_receiver(body, e)),
        ExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => inner
            .as_expr()
            .is_some_and(|e| is_duplicable_receiver(body, e)),
        _ => false,
    }
}
