//! What an expression denotes.
//!
//! A projection reads: it answers what value an expression stands for, given
//! what is known about its inputs, and changes nothing. Every method here takes
//! `&self`, and that is load-bearing rather than incidental — the engine walks
//! the same node any number of times, so anything performed here would be
//! performed again. What has to happen once belongs to the frame.
//!
//! A reference denotes its referent's value: the engine has no reference
//! values, so borrowing and dereferencing change nothing about what is denoted.
//! Where a frame gave a local a place of its own, that holds only for reads
//! made *through* it — see [`Interpreter::projected_lattice`].

use std::rc::Rc;

use crate::compiler_item::SeqField;
use crate::const_eval::{
    MAX_SEQ_ELEMENTS, Value, eval_binary, eval_cast, eval_unary, is_f32_type, is_int_prim, prim_of,
};
use crate::module_source::ModuleSource;
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, Operand, PatId, PatKind, StmtKind,
};
use crate::nir_value_graph::{ValueId, ValueKind};
use crate::tir::{PrimitiveType, ResolvedType, TypeId};

use super::CtfeBuiltin;
use super::pattern::PatternMatch;
use super::{GlobalKey, Interpreter, Lattice, PatBindings, let_ref_global};

impl Interpreter<'_> {
    /// What an operand denotes: the promoted constant for `Operand::Value`,
    /// else the skeleton subtree's lattice. Promoted pure values live in
    /// `body.values`, so a literal that left the skeleton still folds.
    pub fn operand_to_lattice(&self, body: &Body, op: Operand) -> Lattice {
        match op {
            Operand::Expr(e) => self.expr_to_lattice(body, e),
            Operand::Value(v) => self.value_to_lattice(body, v),
        }
    }

    /// A promoted pure value as a `Lattice::Const` when it is a constant kind of
    /// a known primitive type; `Unevaluated` otherwise.
    fn value_to_lattice(&self, body: &Body, v: ValueId) -> Lattice {
        let Some(ty) = body.values.type_of(v) else {
            return Lattice::Unevaluated;
        };
        match body.values.kind(v) {
            ValueKind::Bool(b) => Lattice::Const(Value::Bool(*b)),
            ValueKind::Char(c) => Lattice::Const(Value::Char(*c)),
            ValueKind::Int(value, _) => {
                // An enum value is its discriminant, promoted under the enum's
                // own type, so the pool holds an `i32` no primitive names.
                let prim = prim_of(ty, self.type_table)
                    .filter(|p| is_int_prim(*p))
                    .or_else(|| {
                        matches!(self.type_table.get(ty), ResolvedType::Enum { .. })
                            .then_some(PrimitiveType::I32)
                    });
                let Some(prim) = prim else {
                    return Lattice::Unevaluated;
                };
                Lattice::Const(Value::Int {
                    value: *value,
                    prim,
                })
            }
            ValueKind::Float(bits, _) => {
                let prim = if is_f32_type(ty, self.type_table) {
                    PrimitiveType::F32
                } else {
                    PrimitiveType::F64
                };
                Lattice::Const(Value::Float {
                    value: f64::from_bits(*bits),
                    prim,
                })
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// The global a field read resolves against: a direct `GLOBAL.f`, or a
    /// local bound to `&GLOBAL` earlier in this body.
    fn global_receiver_key(&self, body: &Body, inner: Operand) -> Option<GlobalKey> {
        match inner.as_expr().map(|e| &body.exprs[e].kind)? {
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => Some((module_source.clone(), name.clone())),
            ExprKind::Local { index, .. } => {
                let (stmt, key) = self.frame.ref_global_aliases.get(index)?;
                debug_assert_eq!(
                    body.stmts
                        .get(*stmt)
                        .and_then(|st| let_ref_global(body, &st.kind)),
                    Some((*index, key.clone())),
                    "ref_global_aliases holds {key:?} for local {index}, but the body being \
                     folded does not bind it there — per-function alias state leaked across a \
                     body boundary (e.g. a CTFE scratch reduction that did not save/clear it)",
                );
                Some(key.clone())
            }
            _ => None,
        }
    }

    /// The lattice of `receiver.field`: the [`GlobalFieldEnv`] entry for a
    /// global receiver, else the field projected out of a constant aggregate
    /// receiver (a literal, an env-bound local, or a CTFE-folded call result).
    ///
    /// The field env wins where it has an answer — it knows fields no
    /// initializer shows, such as the length body globalization records for a
    /// hoisted sequence — and otherwise the receiver's own value decides.
    pub(super) fn field_access_lattice(
        &self,
        body: &Body,
        inner: Operand,
        field_index: u32,
        field_name: &str,
    ) -> Lattice {
        if let Some(key) = self.global_receiver_key(body, inner) {
            let known = self.global_field(&key, field_name);
            if !matches!(known, Lattice::Unevaluated) {
                return known;
            }
        }
        match self.projected_lattice(body, inner) {
            Lattice::Const(receiver) => receiver
                .field(field_index)
                .cloned()
                .map_or(Lattice::Unevaluated, Lattice::Const),
            Lattice::NonConst => Lattice::NonConst,
            Lattice::Unevaluated => Lattice::Unevaluated,
        }
    }

    /// What a projection's receiver denotes, resolving a frame place alias.
    ///
    /// Reading *through* a reference is what a borrow is for, so a field read,
    /// an element read and a deref all reach the place's current value. Reading
    /// the reference *itself* is a different act — it names storage the engine
    /// has no value for — and [`Self::expr_to_lattice`] leaves that
    /// unevaluated, so a rebind or a capture never turns into a copy.
    pub(super) fn projected_lattice(&self, body: &Body, op: Operand) -> Lattice {
        if let Some(e) = op.as_expr()
            && let ExprKind::Local { index, .. } = &body.exprs[e].kind
            && let Some((root, path)) = self.frame.place_aliases.get(index)
        {
            return self
                .place_value(*root, path)
                .map_or(Lattice::Unevaluated, Lattice::Const);
        }
        self.operand_to_lattice(body, op)
    }

    /// Read an element out of a constant sequence. An index past the end is
    /// `NonConst`, so the run-time trap survives.
    pub(super) fn index_lattice(&self, body: &Body, receiver: Operand, index: Operand) -> Lattice {
        let (Lattice::Const(receiver), Lattice::Const(index)) = (
            self.projected_lattice(body, receiver),
            self.operand_to_lattice(body, index),
        ) else {
            return Lattice::Unevaluated;
        };
        let Some((index, _)) = index.as_int() else {
            return Lattice::Unevaluated;
        };
        receiver
            .element(index)
            .cloned()
            .map_or(Lattice::NonConst, Lattice::Const)
    }

    /// `Const` only when every element is itself constant, and only up to
    /// [`MAX_SEQ_ELEMENTS`].
    fn seq_lattice(&self, body: &Body, type_id: TypeId, elements: &[Operand]) -> Lattice {
        let mut values = Vec::with_capacity(elements.len());
        for op in elements {
            match self.operand_to_lattice(body, *op) {
                Lattice::Const(v) => values.push(v),
                Lattice::NonConst => return Lattice::NonConst,
                Lattice::Unevaluated => return Lattice::Unevaluated,
            }
        }
        Value::seq(type_id, values).map_or(Lattice::NonConst, Lattice::Const)
    }

    /// The lattice of a struct / tuple literal: `Const` only when every field
    /// is itself constant, since a partially-known aggregate is not a value the
    /// engine can substitute or compare.
    fn aggregate_lattice(
        &self,
        body: &Body,
        type_id: TypeId,
        fields: impl Iterator<Item = (u32, Operand)>,
    ) -> Lattice {
        let mut values = Vec::new();
        let mut has_non_const = false;
        for (field_index, op) in fields {
            match self.operand_to_lattice(body, op) {
                Lattice::Const(v) => values.push((field_index, v)),
                Lattice::NonConst => has_non_const = true,
                Lattice::Unevaluated => return Lattice::Unevaluated,
            }
        }
        if has_non_const {
            return Lattice::NonConst;
        }
        Lattice::Const(Value::aggregate(type_id, values))
    }

    /// The lattice of a variant construction: the case it names, plus its
    /// payload where it carries one. A payload that is not itself constant
    /// leaves the whole value unknown, as an aggregate field does.
    fn variant_lattice(
        &self,
        body: &Body,
        type_id: TypeId,
        case_name: &str,
        payload: Option<Operand>,
    ) -> Lattice {
        let payload = match payload {
            None => None,
            Some(op) => match self.operand_to_lattice(body, op) {
                Lattice::Const(v) => Some(Rc::new(v)),
                Lattice::NonConst => return Lattice::NonConst,
                Lattice::Unevaluated => return Lattice::Unevaluated,
            },
        };
        Lattice::Const(Value::Variant {
            type_id,
            case_name: case_name.into(),
            payload,
        })
    }

    /// An array literal denotes the whole container, not just its elements:
    /// `wir_build` lowers it to `{ backing: array.new_fixed, len: N }`.
    fn array_literal_lattice(&self, body: &Body, type_id: TypeId, elements: &[Operand]) -> Lattice {
        match self.seq_lattice(body, type_id, elements) {
            Lattice::Const(backing) => Lattice::Const(Value::aggregate(
                type_id,
                vec![
                    (SeqField::Backing.index(), backing),
                    (
                        SeqField::Len.index(),
                        Value::Int {
                            value: elements.len() as u64,
                            prim: PrimitiveType::I32,
                        },
                    ),
                ],
            )),
            other => other,
        }
    }

    pub fn expr_to_lattice(&self, body: &Body, e: ExprId) -> Lattice {
        if let Some(v) = self.frame.scratch_folds.get(&e) {
            return Lattice::Const(v.clone());
        }
        let node = &body.exprs[e];
        match &node.kind {
            // Only a projection resolves an alias — see
            // [`Self::projected_lattice`].
            ExprKind::Local { index, .. } if self.frame.place_aliases.contains_key(index) => {
                Lattice::Unevaluated
            }
            ExprKind::Local { index, .. } => self
                .frame
                .env
                .get(index)
                .cloned()
                .unwrap_or(Lattice::Unevaluated),
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                field_name,
            } => self.field_access_lattice(body, *inner, *field_index, field_name),
            ExprKind::StructLiteral { fields, .. } => self.aggregate_lattice(
                body,
                node.type_id,
                fields.iter().map(|f| (f.field_index, f.value)),
            ),
            ExprKind::TupleLiteral { elements } => self.aggregate_lattice(
                body,
                node.type_id,
                elements
                    .iter()
                    .enumerate()
                    .map(|(i, op)| (u32::try_from(i).expect("tuple arity fits u32"), *op)),
            ),
            ExprKind::ArrayLiteral { elements } => {
                self.array_literal_lattice(body, node.type_id, elements)
            }
            ExprKind::PackedArray(bytes) => {
                let elements = bytes
                    .iter()
                    .map(|b| Value::Int {
                        value: u64::from(*b),
                        prim: PrimitiveType::U8,
                    })
                    .collect();
                Value::seq(node.type_id, elements).map_or(Lattice::NonConst, Lattice::Const)
            }
            // A plain enum case is its discriminant, which is the whole value:
            // an enum carries no payload, so nothing else distinguishes two
            // values of one case.
            ExprKind::EnumConstruct { case_index, .. } => Lattice::Const(Value::Int {
                value: u64::from(*case_index),
                prim: PrimitiveType::I32,
            }),
            ExprKind::VariantConstruct {
                variant_type,
                case_name,
                payload,
                ..
            } => self.variant_lattice(body, *variant_type, case_name, *payload),
            ExprKind::Index { expr: inner, index } => self.index_lattice(body, *inner, *index),
            ExprKind::Call { .. } => self.try_ctfe_builtin_fold(body, e),
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::Deref,
                expr: inner,
            } => self.projected_lattice(body, *inner),
            // A cast that converts nothing denotes its operand; a converting
            // one is `try_fold`'s case.
            ExprKind::Cast { expr: inner, .. }
                if operand_type(body, *inner)
                    .is_some_and(|t| self.same_ref_shape(node.type_id, t)) =>
            {
                self.operand_to_lattice(body, *inner)
            }
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => self.global_lattice(module_source, name),
            ExprKind::Block(b) => self.block_lattice(body, *b),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.operand_to_lattice(body, *condition);
                match cond {
                    Lattice::Const(Value::Bool(true)) => self.block_lattice(body, *then_branch),
                    Lattice::Const(Value::Bool(false)) => match else_branch {
                        Some(eb) => self.block_lattice(body, *eb),
                        None => Lattice::Unevaluated,
                    },
                    _ => {
                        let then_lat =
                            arm_lattice_for_feasible_join(self.block_lattice(body, *then_branch));
                        let else_lat = match else_branch {
                            Some(eb) => {
                                arm_lattice_for_feasible_join(self.block_lattice(body, *eb))
                            }
                            None => Lattice::NonConst,
                        };
                        then_lat.join(else_lat)
                    }
                }
            }
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => match scrutinee.as_expr() {
                Some(e) => self.match_lattice(body, e, arms),
                None => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// Fold a `Binary` / `Unary` / `Cast` of constant operands to a value;
    /// `NonConst` (not `Unevaluated`) when the op would trap, so the node
    /// survives.
    ///
    /// A shared borrow is excluded: it denotes its referent rather than
    /// operating on it, and `eval_unary` would bury the referent's own constant
    /// as non-constant.
    pub fn try_fold(&self, body: &Body, e: ExprId) -> Lattice {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Binary { left, op, right } => {
                let l = match self.operand_to_lattice(body, *left) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                // `&&` and `||` decide on the left alone when it is their
                // absorbing element. The right operand is not evaluated at run
                // time either, so what it would have done — including a read
                // past the end, guarded by the very test that short-circuits —
                // must not leave the whole test unknown.
                if let Some(decided) = short_circuit_result(&l, *op) {
                    return Lattice::Const(Value::Bool(decided));
                }
                let r = match self.operand_to_lattice(body, *right) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_binary(l, *op, r))
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                ..
            } => Lattice::Unevaluated,
            ExprKind::Unary { op, expr: inner } => {
                let v = match self.operand_to_lattice(body, *inner) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_unary(*op, v))
            }
            ExprKind::Cast { expr: inner, .. } => {
                let Some(target) = prim_of(node.type_id, self.type_table) else {
                    return Lattice::Unevaluated;
                };
                match self.operand_to_lattice(body, *inner) {
                    Lattice::Const(v) => option_to_lattice(eval_cast(v, target)),
                    other => other,
                }
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a block: its single tail `Expr`, else `Unevaluated`.
    pub(super) fn block_lattice(&self, body: &Body, b: BlockId) -> Lattice {
        match body.blocks[b].stmts.as_slice() {
            [] => Lattice::Unevaluated,
            [single] => match &body.stmts[*single].kind {
                StmtKind::Expr(e) => self.operand_to_lattice(body, *e),
                _ => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a `match`: the chosen arm under a constant scrutinee,
    /// else the join over the feasible arms.
    ///
    /// A guarded arm is undecided here. Deciding one means scoping the
    /// pattern's bindings, which only the rewrite path can do.
    fn match_lattice(&self, body: &Body, scrutinee: ExprId, arms: &[ArmData]) -> Lattice {
        if arms.is_empty() {
            return Lattice::Unevaluated;
        }
        match self.expr_to_lattice(body, scrutinee).as_const() {
            Some(value) => self.chosen_arm_lattice(body, &value, arms),
            None => self.every_arm_lattice(body, arms),
        }
    }

    /// The join over the arms a constant scrutinee leaves feasible: the ones
    /// still undecided, up to and including the first that definitely matches.
    /// A later arm is unreachable once one matches, and an arm that cannot
    /// match contributes nothing — so neither is valued at all.
    ///
    /// `NonConst` when no arm is found to match: the implicit no-match trap is
    /// live, and folding to any arm's value would erase it.
    fn chosen_arm_lattice(&self, body: &Body, value: &Value, arms: &[ArmData]) -> Lattice {
        let mut acc = Lattice::Unevaluated;
        let mut undecided = false;
        for arm in arms {
            let pm = if arm.guard.is_some() {
                PatternMatch::Unknown
            } else {
                self.pattern_matches(body, value, arm.pattern, &mut PatBindings::new())
            };
            match pm {
                PatternMatch::No => {}
                PatternMatch::Yes if !undecided => {
                    return self.operand_to_lattice(body, arm.body);
                }
                PatternMatch::Yes => return acc.join(self.feasible_arm(body, arm)),
                PatternMatch::Unknown => {
                    undecided = true;
                    acc = acc.join(self.feasible_arm(body, arm));
                }
            }
        }
        Lattice::NonConst
    }

    /// The join over every arm, for a scrutinee no arm can be decided against.
    /// Only sound where the arms cover it: otherwise the no-match trap decides
    /// the value instead.
    fn every_arm_lattice(&self, body: &Body, arms: &[ArmData]) -> Lattice {
        if !is_provably_exhaustive(body, arms.iter().map(|a| (a.guard, a.pattern))) {
            return Lattice::NonConst;
        }
        let mut acc = Lattice::Unevaluated;
        for arm in arms {
            acc = acc.join(self.feasible_arm(body, arm));
        }
        acc
    }

    /// An arm's value as it reaches a join: reachable, so an unknown one is
    /// SCCP-Top rather than an absorbed infeasible edge.
    fn feasible_arm(&self, body: &Body, arm: &ArmData) -> Lattice {
        arm_lattice_for_feasible_join(self.operand_to_lattice(body, arm.body))
    }

    /// Evaluate `array_get(seq, i)` / `array_len(seq)` over a constant
    /// sequence, or the sequence `array_new(len)` allocates. A read's argument
    /// is a reference to the array, and a reference to a constant reads as that
    /// constant, so no separate deref step is needed.
    ///
    /// A write denotes nothing — the executor performs it as a statement — and
    /// nor does a hint, which it steps past.
    pub(super) fn try_ctfe_builtin_fold(&self, body: &Body, e: ExprId) -> Lattice {
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return Lattice::Unevaluated;
        };
        let Some(builtin) = self.facts.ctfe_builtins.and_then(|m| m.get(func_id)) else {
            return Lattice::Unevaluated;
        };
        let args = args.as_slice();
        match builtin {
            CtfeBuiltin::ArrayLen => {
                let [arr] = args else {
                    return Lattice::Unevaluated;
                };
                let Lattice::Const(v) = self.operand_to_lattice(body, arr.expr) else {
                    return Lattice::Unevaluated;
                };
                v.seq_len().map_or(Lattice::Unevaluated, |len| {
                    Lattice::Const(Value::Int {
                        value: len as u64,
                        prim: PrimitiveType::I32,
                    })
                })
            }
            CtfeBuiltin::ArrayGet => match args {
                [arr, index] => self.index_lattice(body, arr.expr, index.expr),
                _ => Lattice::Unevaluated,
            },
            CtfeBuiltin::ArrayNew => match args {
                [len] => self.allocation_lattice(body, e, len.expr),
                _ => Lattice::Unevaluated,
            },
            CtfeBuiltin::ArrayClonePrefix => match args {
                [src, len] => self.clone_prefix_lattice(body, e, src.expr, len.expr),
                _ => Lattice::Unevaluated,
            },
            CtfeBuiltin::Select => match args {
                [condition, if_true, if_false] => {
                    self.select_lattice(body, condition.expr, if_true.expr, if_false.expr)
                }
                _ => Lattice::Unevaluated,
            },
            CtfeBuiltin::I32AsChar => match args {
                [value] => self.i32_as_char_lattice(body, value.expr),
                _ => Lattice::Unevaluated,
            },
            CtfeBuiltin::ArraySet | CtfeBuiltin::ArrayCopy | CtfeBuiltin::ColdPath => {
                Lattice::Unevaluated
            }
        }
    }

    /// `i32_as_char` reinterprets unchecked, so a codepoint outside the
    /// scalar-value range stays as written rather than folding a value
    /// `char` cannot hold.
    fn i32_as_char_lattice(&self, body: &Body, value: Operand) -> Lattice {
        let Lattice::Const(value) = self.operand_lattice_folded(body, value) else {
            return Lattice::Unevaluated;
        };
        let Some((value, _)) = value.as_int() else {
            return Lattice::Unevaluated;
        };
        u32::try_from(value)
            .ok()
            .and_then(char::from_u32)
            .map_or(Lattice::Unevaluated, |c| Lattice::Const(Value::Char(c)))
    }

    /// The arm `select` picks. Both arms run at run time, so the one not taken
    /// has to compute rather than trap; a constant is exactly that.
    fn select_lattice(
        &self,
        body: &Body,
        condition: Operand,
        if_true: Operand,
        if_false: Operand,
    ) -> Lattice {
        let Lattice::Const(Value::Bool(condition)) = self.operand_lattice_folded(body, condition)
        else {
            return Lattice::Unevaluated;
        };
        let (Lattice::Const(if_true), Lattice::Const(if_false)) = (
            self.operand_lattice_folded(body, if_true),
            self.operand_lattice_folded(body, if_false),
        ) else {
            return Lattice::Unevaluated;
        };
        Lattice::Const(if condition { if_true } else { if_false })
    }

    /// The sequence `array_new(len)` allocates: `len` elements at the default
    /// `array.new_default` leaves. A negative or oversized length, or an
    /// element type with no compile-time default, is not a constant here — the
    /// call stays and traps or allocates at run time as written.
    fn allocation_lattice(&self, body: &Body, e: ExprId, len: Operand) -> Lattice {
        let Lattice::Const(len) = self.operand_to_lattice(body, len) else {
            return Lattice::Unevaluated;
        };
        let Some((len, PrimitiveType::I32)) = len.as_int() else {
            return Lattice::Unevaluated;
        };
        let array_type = body.exprs[e].type_id;
        let ResolvedType::BuiltinArray(element_type) = self.type_table.get(array_type) else {
            return Lattice::Unevaluated;
        };
        let (Ok(len), Some(default)) = (
            usize::try_from(len as i32),
            prim_of(*element_type, self.type_table).and_then(Value::default_of),
        ) else {
            return Lattice::Unevaluated;
        };
        if len > MAX_SEQ_ELEMENTS {
            return Lattice::Unevaluated;
        }
        Value::seq(array_type, vec![default; len]).map_or(Lattice::Unevaluated, Lattice::Const)
    }

    /// The sequence `array_clone_prefix(src, len)` returns: a fresh backing
    /// holding `src`'s first `len` elements. A length past what `src` holds
    /// traps at run time, so it is not a constant here.
    fn clone_prefix_lattice(&self, body: &Body, e: ExprId, src: Operand, len: Operand) -> Lattice {
        let (Lattice::Const(src), Lattice::Const(len)) = (
            self.projected_lattice(body, src),
            self.operand_lattice_folded(body, len),
        ) else {
            return Lattice::Unevaluated;
        };
        let (Some((len, PrimitiveType::I32)), Value::Seq { elements, .. }) = (len.as_int(), &src)
        else {
            return Lattice::Unevaluated;
        };
        let Ok(len) = usize::try_from(len as i32) else {
            return Lattice::Unevaluated;
        };
        let Some(prefix) = elements.get(..len) else {
            return Lattice::Unevaluated;
        };
        Value::seq(body.exprs[e].type_id, prefix.to_vec())
            .map_or(Lattice::Unevaluated, Lattice::Const)
    }

    /// An argument's value, folding the arithmetic it may still be spelled as:
    /// an argument reaches a call as written, and the structural projection
    /// alone reads only what already stands as a literal.
    pub(super) fn operand_lattice_folded(&self, body: &Body, op: Operand) -> Lattice {
        match op.as_expr() {
            Some(e) => self.reduce_to_lattice(body, e),
            None => self.operand_to_lattice(body, op),
        }
    }

    /// Whether `a` and `b` are the same array or borrow structure, so a cast
    /// between them cannot convert a value. Identical ids are trivially so;
    /// otherwise both must be the same reference shape over element types that
    /// are — which is what makes duplicate-interned `Array<u8>` ids compare
    /// equal. A borrow denotes its referent either way, so `&mut` and `&`
    /// over the same referent shape read alike.
    fn same_ref_shape(&self, a: TypeId, b: TypeId) -> bool {
        if a == b {
            return true;
        }
        match (self.type_table.get(a), self.type_table.get(b)) {
            (ResolvedType::BuiltinArray(x), ResolvedType::BuiltinArray(y)) => {
                self.same_ref_shape(*x, *y)
            }
            (
                ResolvedType::Ref(x) | ResolvedType::MutRef(x),
                ResolvedType::Ref(y) | ResolvedType::MutRef(y),
            ) => self.same_ref_shape(*x, *y),
            _ => false,
        }
    }

    /// Look up a `(module_source, name)` global in the installed
    /// [`GlobalEnv`]. Absent keys default to [`Lattice::Unevaluated`]
    /// — the engine simply has no information, same convention as
    /// un-bound locals.
    pub(super) fn global_lattice(&self, module_source: &ModuleSource, name: &str) -> Lattice {
        let Some(globals) = self.facts.globals else {
            return Lattice::Unevaluated;
        };
        globals
            .get(&(module_source.clone(), name.to_string()))
            .cloned()
            .unwrap_or(Lattice::Unevaluated)
    }
}

/// The static type of an operand: the node's recorded type for a skeleton
/// expression, the pool's for a promoted value.
fn operand_type(body: &Body, op: Operand) -> Option<TypeId> {
    match op {
        Operand::Expr(e) => Some(body.exprs[e].type_id),
        Operand::Value(v) => body.values.type_of(v),
    }
}

/// `Some(v)` ↦ `Const(v)`, `None` ↦ `NonConst` — the boundary where a
/// numeric-evaluation helper returns, whose `None` means a runtime trap rather
/// than "not yet tried".
pub(super) fn option_to_lattice(opt: Option<Value>) -> Lattice {
    match opt {
        Some(v) => Lattice::Const(v),
        None => Lattice::NonConst,
    }
}

/// Promote an arm's `Unevaluated` to `NonConst` before joining it.
///
/// `join` absorbs an `Unevaluated` operand, which is the infeasible-edge rule
/// and holds only where that arm really is unreachable. A reachable arm whose
/// value is simply unknown is SCCP-Top, so it must reach the join as such.
pub(super) fn arm_lattice_for_feasible_join(lat: Lattice) -> Lattice {
    match lat {
        Lattice::Unevaluated => Lattice::NonConst,
        other => other,
    }
}

/// Whether the arms cover every scrutinee (a guardless catch-all exists).
///
/// Takes what it asks of an arm rather than the arm, so the rewrite path can
/// answer from the parts it already lifted out of the body.
pub(super) fn is_provably_exhaustive(
    body: &Body,
    arms: impl IntoIterator<Item = (Option<Operand>, PatId)>,
) -> bool {
    arms.into_iter()
        .any(|(guard, pattern)| guard.is_none() && pattern_is_catch_all(body, pattern))
}

pub(super) fn pattern_is_catch_all(body: &Body, pat: PatId) -> bool {
    match &body.pats[pat].kind {
        PatKind::Wildcard | PatKind::Binding { .. } => true,
        PatKind::Or(alts) => alts.iter().any(|p| pattern_is_catch_all(body, *p)),
        _ => false,
    }
}

/// What a short-circuit yields on its left operand alone: `false && _` and
/// `true || _`. `None` where the right operand still decides.
fn short_circuit_result(left: &Value, op: NirBinaryOp) -> Option<bool> {
    match (left.as_bool()?, op) {
        (false, NirBinaryOp::And) => Some(false),
        (true, NirBinaryOp::Or) => Some(true),
        _ => None,
    }
}
