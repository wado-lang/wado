//! Three composed string-append rewrites over the shared peephole session:
//! [`ShortPushStrRule`] expands `buf.push_str("short_constant")` (≤8 ASCII bytes)
//! into per-byte `buf.push(ch)` calls, [`ConstAsciiPushRule`] retargets each
//! constant-ASCII `push` to `push_ascii_unchecked`, and [`AppendFuseRule`]
//! collapses the run of adjacent appends they leave behind into one reservation.
//! Must run *before* `inline`, which replaces the call node the
//! literal-recogniser matches.

use crate::compiler_item::{CompilerItem, SeqField};
use crate::nir::{FuncId, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::tir::TypeTable;
use crate::token::Span;

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
    /// The four `String` primitives [`AppendFuseRule`] writes a fused run in
    /// terms of. All four or none: a missing one only disables that rule.
    fused: Option<FusedIds>,
}

/// The `String` primitives a fused append run is written with.
#[derive(Clone, Copy)]
pub(super) struct FusedIds {
    len: FuncId,
    reserve_uninit: FuncId,
    set_byte: FuncId,
    write_str_at: FuncId,
}

impl Ctx {
    fn resolve(project: &NirPackage) -> Option<Self> {
        let mut push_str_id: Option<crate::nir::FuncId> = None;
        let mut push_char_id: Option<crate::nir::FuncId> = None;
        let mut push_ascii_id: Option<crate::nir::FuncId> = None;
        let mut len_id: Option<FuncId> = None;
        let mut reserve_id: Option<FuncId> = None;
        let mut set_byte_id: Option<FuncId> = None;
        let mut write_str_id: Option<FuncId> = None;
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
                Some(CompilerItem::StringLen) => {
                    len_id = Some(f.id.expect("func_id assigned at lower"));
                }
                Some(CompilerItem::StringReserveUninit) => {
                    reserve_id = Some(f.id.expect("func_id assigned at lower"));
                }
                Some(CompilerItem::StringSetByteUnchecked) => {
                    set_byte_id = Some(f.id.expect("func_id assigned at lower"));
                }
                Some(CompilerItem::StringWriteStrAt) => {
                    write_str_id = Some(f.id.expect("func_id assigned at lower"));
                }
                Some(_) | None => {}
            }
        }
        let fused = (|| {
            Some(FusedIds {
                len: len_id?,
                reserve_uninit: reserve_id?,
                set_byte: set_byte_id?,
                write_str_at: write_str_id?,
            })
        })();
        Some(Self {
            push_str_id: push_str_id?,
            push_char_id: push_char_id?,
            push_ascii_id,
            fused,
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
/// dispatch: a constant ASCII scalar is always one byte. Composes with
/// [`ShortPushStrRule`], whose per-byte output is all ASCII. The rewrite is an
/// in-place call edit — swap the callee, coerce the `char` to its `u8`.
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

/// Receivers safe to clone N times — deliberately narrow, excluding anything
/// that may allocate, trap, or be observably stateful. `push_str`'s `&mut self`
/// already forces a place, so in practice only a `Local`, an `&mut`-wrapped one,
/// or a `FieldAccess` chain rooted at one appears; `GlobalVarGet` is accepted
/// defensively, being a pure read.
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

/// Cap on the pieces one fused run absorbs. What follows the cap simply opens
/// the next run, so this bounds the offset expression each write carries rather
/// than the fusion's reach.
const MAX_FUSED_PIECES: usize = 16;

/// One append in a fused run.
enum Piece {
    /// A constant ASCII byte, from `push_ascii_unchecked`.
    Byte(u8),
    /// A `push_str` argument, with its length when the argument is a literal.
    Str { arg: ExprId, const_len: Option<i32> },
}

/// A term of a running byte offset: the constants collapse into one summand,
/// each dynamic length keeps the local it was bound to.
enum LenTerm {
    Const(i32),
    Local(u32),
}

/// Collapse a run of adjacent appends on one buffer into one
/// `internal_reserve_uninit` and raw writes, sparing each its capacity check.
///
/// A source may be the buffer itself (`buf.push_str(&buf)`), so a run-time
/// length is read at the start of its own group: hoisting it over an earlier
/// write in the same run would measure a buffer the run itself grew.
pub(super) struct AppendFuseRule {
    push_str_id: FuncId,
    push_ascii_id: FuncId,
    ids: FusedIds,
}

impl AppendFuseRule {
    /// `None` unless every primitive the fused form is written with resolved.
    pub(super) fn new(ctx: &Ctx) -> Option<Self> {
        Some(Self {
            push_str_id: ctx.push_str_id,
            push_ascii_id: ctx.push_ascii_id?,
            ids: ctx.fused?,
        })
    }

    /// The append `stmt` performs, with the buffer it appends to.
    fn piece_of(&self, body: &Body, stmt: StmtId) -> Option<(ExprId, Piece)> {
        let StmtKind::Expr(Operand::Expr(expr_id)) = body.stmts[stmt].kind else {
            return None;
        };
        let (receiver, func_id, args) = body.exprs[expr_id].kind.as_method_call()?;
        let recv = receiver.as_expr()?;
        if !is_duplicable_receiver(body, recv) {
            return None;
        }
        let [arg] = args else {
            return None;
        };
        if func_id == self.push_ascii_id {
            let byte = u8::try_from(body.operand_const_int(arg.expr)?).ok()?;
            return Some((recv, Piece::Byte(byte)));
        }
        if func_id != self.push_str_id {
            return None;
        }
        let arg = arg.expr.as_expr()?;
        // A literal argument keeps its byte length, so it does not open a group
        // — and fusing it leaves the literal's `repr` as the bare packed array
        // `const_object_globalization` hoists into a module global.
        let const_len = const_str_len(body, arg);
        if const_len.is_none() && !is_duplicable_receiver(body, arg) {
            return None;
        }
        Some((recv, Piece::Str { arg, const_len }))
    }

    /// Build the fused statements for `run`, all appending to `recv`.
    fn emit(&self, engine: &mut Engine, recv: ExprId, run: Vec<Piece>, span: Span) -> Vec<StmtId> {
        let mut stmts = Vec::with_capacity(run.len() + 2);
        let mut terms: Vec<LenTerm> = Vec::with_capacity(run.len());
        for piece in &run {
            terms.push(match piece {
                Piece::Byte(_) => LenTerm::Const(1),
                Piece::Str {
                    const_len: Some(n), ..
                } => LenTerm::Const(*n),
                Piece::Str { arg, .. } => {
                    LenTerm::Local(self.bind_len(engine, *arg, span, &mut stmts))
                }
            });
        }

        let total = self.sum_expr(engine, &terms, span);
        let at = engine.alloc_local(
            format!("__fuse_at_{}", engine.locals().len()),
            TypeTable::I32,
            /* is_mut */ false,
        );
        let reserve = self.call(
            engine,
            self.ids.reserve_uninit,
            recv,
            true,
            vec![total],
            TypeTable::I32,
            span,
        );
        stmts.push(let_stmt(engine, at, TypeTable::I32, reserve, span));

        for (i, piece) in run.iter().enumerate() {
            let offset = self.offset_expr(engine, at, &terms[..i], span);
            match piece {
                Piece::Byte(byte) => {
                    let value = engine.const_operand(
                        ValueKind::Int(u64::from(*byte), TypeTable::U8),
                        TypeTable::U8,
                    );
                    let call = self.call(
                        engine,
                        self.ids.set_byte,
                        recv,
                        true,
                        vec![offset, value],
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(engine.alloc_stmt(StmtKind::Expr(call), span));
                }
                Piece::Str { arg, .. } => {
                    let len = self.term_operand(engine, &terms[i], span);
                    let source = Operand::Expr(engine.clone_expr(*arg));
                    let call = self.call(
                        engine,
                        self.ids.write_str_at,
                        recv,
                        true,
                        vec![offset, source, len],
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(engine.alloc_stmt(StmtKind::Expr(call), span));
                }
            }
        }
        stmts
    }

    /// Bind `arg.len()` to a fresh local, read before the reservation runs.
    fn bind_len(
        &self,
        engine: &mut Engine,
        arg: ExprId,
        span: Span,
        stmts: &mut Vec<StmtId>,
    ) -> u32 {
        let local = engine.alloc_local(
            format!("__fuse_len_{}", engine.locals().len()),
            TypeTable::I32,
            /* is_mut */ false,
        );
        let source = engine.clone_expr(arg);
        let call = engine.alloc_expr(
            ExprKind::method_call(self.ids.len, Operand::Expr(source), false, vec![]),
            TypeTable::I32,
            span,
        );
        stmts.push(let_stmt(
            engine,
            local,
            TypeTable::I32,
            Operand::Expr(call),
            span,
        ));
        local
    }

    /// `Σ terms`, with the constant terms collapsed into one summand. A zero
    /// constant is dropped rather than added, so an offset built only from
    /// run-time lengths reads as those lengths.
    fn sum_expr(&self, engine: &mut Engine, terms: &[LenTerm], span: Span) -> Operand {
        let constant: i32 = terms
            .iter()
            .filter_map(|t| match t {
                LenTerm::Const(n) => Some(*n),
                LenTerm::Local(_) => None,
            })
            .sum();
        let locals: Vec<u32> = terms
            .iter()
            .filter_map(|t| match t {
                LenTerm::Local(local) => Some(*local),
                LenTerm::Const(_) => None,
            })
            .collect();
        let mut acc = (constant != 0 || locals.is_empty()).then(|| {
            engine.const_operand(
                ValueKind::Int(i64::from(constant) as u64, TypeTable::I32),
                TypeTable::I32,
            )
        });
        for local in locals {
            let rhs = local_operand(engine, local, span);
            acc = Some(match acc {
                Some(left) => Operand::Expr(engine.alloc_expr(
                    ExprKind::Binary {
                        left,
                        op: NirBinaryOp::Add,
                        right: rhs,
                    },
                    TypeTable::I32,
                    span,
                )),
                None => rhs,
            });
        }
        acc.expect("a constant summand stands in for an empty term list")
    }

    /// The write offset after `before` — the reservation's start plus the
    /// lengths of the pieces already written.
    fn offset_expr(&self, engine: &mut Engine, at: u32, before: &[LenTerm], span: Span) -> Operand {
        let start = local_operand(engine, at, span);
        if before.is_empty() {
            return start;
        }
        let rest = self.sum_expr(engine, before, span);
        Operand::Expr(engine.alloc_expr(
            ExprKind::Binary {
                left: start,
                op: NirBinaryOp::Add,
                right: rest,
            },
            TypeTable::I32,
            span,
        ))
    }

    /// One length term as an operand.
    fn term_operand(&self, engine: &mut Engine, term: &LenTerm, span: Span) -> Operand {
        match term {
            LenTerm::Const(n) => engine.const_operand(
                ValueKind::Int(i64::from(*n) as u64, TypeTable::I32),
                TypeTable::I32,
            ),
            LenTerm::Local(local) => local_operand(engine, *local, span),
        }
    }

    /// A method call on a fresh clone of `recv`.
    fn call(
        &self,
        engine: &mut Engine,
        func_id: FuncId,
        recv: ExprId,
        receiver_is_mut: bool,
        args: Vec<Operand>,
        type_id: crate::tir::TypeId,
        span: Span,
    ) -> Operand {
        let receiver = engine.clone_expr(recv);
        let args = args
            .into_iter()
            .map(|expr| ArenaCallArg {
                expr,
                is_mut: false,
            })
            .collect();
        Operand::Expr(engine.alloc_expr(
            ExprKind::method_call(func_id, Operand::Expr(receiver), receiver_is_mut, args),
            type_id,
            span,
        ))
    }
}

impl Rule for AppendFuseRule {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        let mut out: Vec<StmtId> = Vec::with_capacity(stmts.len());
        let mut changed = false;
        let mut i = 0;
        while i < stmts.len() {
            let Some((recv, piece)) = self.piece_of(&*engine.body, stmts[i]) else {
                out.push(stmts[i]);
                i += 1;
                continue;
            };
            let mut run = vec![(stmts[i], piece)];
            let mut j = i + 1;
            while run.len() < MAX_FUSED_PIECES && j < stmts.len() {
                match self.piece_of(&*engine.body, stmts[j]) {
                    Some((next_recv, next)) if same_place(&*engine.body, recv, next_recv) => {
                        run.push((stmts[j], next));
                        j += 1;
                    }
                    Some(_) | None => break,
                }
            }
            for group in groups(run) {
                if group.len() < 2 {
                    out.extend(group.into_iter().map(|(stmt, _)| stmt));
                    continue;
                }
                let span = engine.body.stmts[group[0].0].span;
                let group = group.into_iter().map(|(_, piece)| piece).collect();
                out.extend(self.emit(engine, recv, group, span));
                changed = true;
            }
            i = j;
        }
        if changed {
            engine.set_block_stmts(id, out);
        }
        changed
    }
}

/// Split a run into groups one reservation each can cover. A piece whose
/// length is only known at run time opens a group, so its length is read
/// before any of that group's writes and after every earlier one.
fn groups(run: Vec<(StmtId, Piece)>) -> Vec<Vec<(StmtId, Piece)>> {
    let mut out: Vec<Vec<(StmtId, Piece)>> = Vec::new();
    for item in run {
        let opens = matches!(
            item.1,
            Piece::Str {
                const_len: None,
                ..
            }
        );
        match out.last_mut() {
            Some(group) if !opens => group.push(item),
            Some(_) | None => out.push(vec![item]),
        }
    }
    out
}

/// A `Let` binding `local` to `value`.
fn let_stmt(
    engine: &mut Engine,
    local: u32,
    type_id: crate::tir::TypeId,
    value: Operand,
    span: Span,
) -> StmtId {
    let name = engine.locals()[local as usize].name.clone();
    engine.alloc_stmt(
        StmtKind::Let {
            name,
            local_index: local,
            is_mut: false,
            is_reactive: false,
            type_id,
            value,
            skip_value_copy: true,
        },
        span,
    )
}

fn local_operand(engine: &mut Engine, local: u32, span: Span) -> Operand {
    let name = engine.locals()[local as usize].name.clone();
    Operand::Expr(engine.alloc_expr(ExprKind::Local { index: local, name }, TypeTable::I32, span))
}

/// The byte length of a `&"literal"` argument, or `None` when the argument is
/// not a string literal.
fn const_str_len(body: &Body, arg: ExprId) -> Option<i32> {
    let ExprKind::Unary {
        op: NirUnaryOp::Ref,
        expr: inner,
    } = &body.exprs[arg].kind
    else {
        return None;
    };
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[inner.as_expr()?].kind else {
        return None;
    };
    let field = |which: SeqField| {
        fields
            .iter()
            .find(|f| f.name == which.field_name())
            .map(|f| f.value)
    };
    let repr = field(SeqField::Backing)?;
    let ExprKind::PackedArray(bytes) = &body.exprs[repr.as_expr()?].kind else {
        return None;
    };
    let backing = i32::try_from(bytes.len()).ok()?;
    let len = field(SeqField::Len)
        .and_then(|op| body.operand_const_int(op))
        .and_then(|v| i32::try_from(v).ok())
        .expect("a packed-array literal carries a constant length");
    assert_eq!(
        len, backing,
        "[NIR] string_push: literal length disagrees with its backing array"
    );
    Some(len)
}

/// Whether two duplicable receiver expressions name the same place.
fn same_place(body: &Body, a: ExprId, b: ExprId) -> bool {
    match (&body.exprs[a].kind, &body.exprs[b].kind) {
        (ExprKind::Local { index: x, .. }, ExprKind::Local { index: y, .. }) => x == y,
        (
            ExprKind::GlobalVarGet {
                module_source: ms_a,
                name: na,
            },
            ExprKind::GlobalVarGet {
                module_source: ms_b,
                name: nb,
            },
        ) => ms_a == ms_b && na == nb,
        (
            ExprKind::FieldAccess {
                expr: ia,
                field_index: fa,
                ..
            },
            ExprKind::FieldAccess {
                expr: ib,
                field_index: fb,
                ..
            },
        ) => fa == fb && same_operand(body, *ia, *ib),
        (ExprKind::Unary { op: oa, expr: ia }, ExprKind::Unary { op: ob, expr: ib }) => {
            oa == ob && same_operand(body, *ia, *ib)
        }
        _ => false,
    }
}

/// [`same_place`] for the operand a place expression is built over. A promoted
/// operand has no place to compare, so it never matches.
fn same_operand(body: &Body, a: Operand, b: Operand) -> bool {
    match (a.as_expr(), b.as_expr()) {
        (Some(x), Some(y)) => same_place(body, x, y),
        _ => false,
    }
}
