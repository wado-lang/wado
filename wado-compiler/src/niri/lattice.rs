//! What an expression denotes.
//!
//! A projection reads: it answers what value an expression stands for, given
//! what is known about its inputs, and changes nothing. Every method here takes
//! `&self`, and that is load-bearing rather than incidental — the engine walks
//! the same node any number of times, so anything performed here would be
//! performed again. What has to happen once belongs to the frame.

use crate::compiler_item::SeqField;
use crate::const_eval::{
    Value, eval_binary, eval_cast, eval_unary, is_f32_type, is_int_prim, prim_of,
};
use crate::module_source::ModuleSource;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{ArmData, BlockId, Body, ExprId, ExprKind, Operand, StmtKind};
use crate::nir_value_graph::{ValueId, ValueKind};
use crate::tir::{PrimitiveType, TypeId};

use super::{
    GlobalKey, Interpreter, Lattice, PatBindings, PatternMatch, arm_lattice_for_feasible_join,
    is_provably_exhaustive_a, join_all, local_binds_to_global_ref, option_to_lattice,
};

impl Interpreter<'_> {
    /// What an operand denotes: the promoted constant for `Operand::Value`,
    /// else the skeleton subtree's lattice. Promoted pure values live in
    /// `body.values`, so a literal that left the skeleton still folds.
    pub fn operand_to_lattice_a(&self, body: &Body, op: Operand) -> Lattice {
        match op {
            Operand::Expr(e) => self.expr_to_lattice_a(body, e),
            Operand::Value(v) => self.value_to_lattice(body, v),
        }
    }

    /// Convert a promoted pure value to a `Lattice::Const` when it is a constant
    /// kind of a known primitive type; `Unevaluated` otherwise (a derived
    /// `Binary` / `Opaque` / non-primitive value niri does not evaluate here).
    fn value_to_lattice(&self, body: &Body, v: ValueId) -> Lattice {
        let Some(ty) = body.values.type_of(v) else {
            return Lattice::Unevaluated;
        };
        match body.values.kind(v) {
            ValueKind::Bool(b) => Lattice::Const(Value::Bool(*b)),
            ValueKind::Char(c) => Lattice::Const(Value::Char(*c)),
            ValueKind::Int(value, _) => {
                let Some(prim) = prim_of(ty, self.type_table).filter(|p| is_int_prim(*p)) else {
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
                let key = self.ref_global_aliases.get(index)?;
                debug_assert!(
                    local_binds_to_global_ref(body, *index, key),
                    "ref_global_aliases[{index}] = {key:?} is stale: the body being folded does \
                     not bind local {index} to that reference — per-function alias state leaked \
                     across a body boundary (e.g. a CTFE scratch reduction that did not \
                     save/clear it)",
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
        match self.operand_to_lattice_a(body, inner) {
            Lattice::Const(receiver) => receiver
                .field(field_index)
                .cloned()
                .map_or(Lattice::Unevaluated, Lattice::Const),
            Lattice::NonConst => Lattice::NonConst,
            Lattice::Unevaluated => Lattice::Unevaluated,
        }
    }

    /// Read an element out of a constant sequence. An index past the end is
    /// `NonConst`, so the run-time trap survives.
    pub(super) fn index_lattice(&self, body: &Body, receiver: Operand, index: Operand) -> Lattice {
        let (Lattice::Const(receiver), Lattice::Const(index)) = (
            self.operand_to_lattice_a(body, receiver),
            self.operand_to_lattice_a(body, index),
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
            match self.operand_to_lattice_a(body, *op) {
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
            match self.operand_to_lattice_a(body, op) {
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

    pub fn expr_to_lattice_a(&self, body: &Body, e: ExprId) -> Lattice {
        // A scratch-CTFE fold memoized for `e` (no node form for pure scalars).
        if let Some(v) = self.scratch_folds.get(&e) {
            return Lattice::Const(v.clone());
        }
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Local { index, .. } => {
                self.env.get(index).cloned().unwrap_or(Lattice::Unevaluated)
            }
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
            // An array literal denotes the whole container: `wir_build` lowers
            // it to `{ repr: array.new_fixed, used: N }`.
            ExprKind::ArrayLiteral { elements } => {
                match self.seq_lattice(body, node.type_id, elements) {
                    Lattice::Const(backing) => Lattice::Const(Value::aggregate(
                        node.type_id,
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
            ExprKind::Index { expr: inner, index } => self.index_lattice(body, *inner, *index),
            // A builtin denotes its value structurally, so a container built
            // over one is constant before any rewrite — an allocation has no
            // literal node form to be rewritten to.
            ExprKind::Call { .. } => self.try_ctfe_builtin_fold_a(body, e),
            // The engine models referents by value, so neither step changes
            // what is denoted.
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::Deref,
                expr: inner,
            } => self.operand_to_lattice_a(body, *inner),
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => self.global_lattice(module_source, name),
            ExprKind::Block(b) => self.block_lattice_a(body, *b),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.operand_to_lattice_a(body, *condition);
                match cond {
                    Lattice::Const(Value::Bool(true)) => self.block_lattice_a(body, *then_branch),
                    Lattice::Const(Value::Bool(false)) => match else_branch {
                        Some(eb) => self.block_lattice_a(body, *eb),
                        None => Lattice::Unevaluated,
                    },
                    _ => {
                        let then_lat =
                            arm_lattice_for_feasible_join(self.block_lattice_a(body, *then_branch));
                        let else_lat = match else_branch {
                            Some(eb) => {
                                arm_lattice_for_feasible_join(self.block_lattice_a(body, *eb))
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
                Some(e) => self.match_lattice_a(body, e, arms),
                // A promoted-constant scrutinee is not evaluated here; the
                // flow-fold visitor collapses constant matches structurally.
                None => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// Fold a `Binary` / `Unary` / `Cast` of constant operands to a value;
    /// `NonConst` (not `Unevaluated`) when the op would trap, so the node survives.
    pub fn try_fold_a(&self, body: &Body, e: ExprId) -> Lattice {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Binary { left, op, right } => {
                let l = match self.operand_to_lattice_a(body, *left) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                let r = match self.operand_to_lattice_a(body, *right) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_binary(l, *op, r))
            }
            // A shared borrow denotes what it points at rather than operating
            // on it. `eval_unary` has no rule for that and would bury the
            // referent's own constant as non-constant.
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                ..
            } => Lattice::Unevaluated,
            ExprKind::Unary { op, expr: inner } => {
                let v = match self.operand_to_lattice_a(body, *inner) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_unary(*op, v))
            }
            ExprKind::Cast { expr: inner, .. } => {
                let Some(target) = prim_of(node.type_id, self.type_table) else {
                    return Lattice::Unevaluated;
                };
                match self.operand_to_lattice_a(body, *inner) {
                    Lattice::Const(v) => option_to_lattice(eval_cast(v, target)),
                    other => other,
                }
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a block: its single tail `Expr`, else `Unevaluated`.
    pub(super) fn block_lattice_a(&self, body: &Body, b: BlockId) -> Lattice {
        match body.blocks[b].stmts.as_slice() {
            [] => Lattice::Unevaluated,
            [single] => match &body.stmts[*single].kind {
                StmtKind::Expr(e) => self.operand_to_lattice_a(body, *e),
                _ => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a `match`: the chosen arm under a constant scrutinee,
    /// else the join over the feasible arms.
    fn match_lattice_a(&self, body: &Body, scrutinee: ExprId, arms: &[ArmData]) -> Lattice {
        let scrut_const = self.expr_to_lattice_a(body, scrutinee).as_const();
        if arms.is_empty() {
            return Lattice::Unevaluated;
        }
        if let Some(scrut_v) = scrut_const {
            let mut candidates = Vec::<Lattice>::new();
            let mut yes_found = false;
            for arm in arms {
                // Guards are decided by the rewrite path, which can scope the
                // pattern's bindings; here they leave the arm undecided.
                let pm = if arm.guard.is_some() {
                    PatternMatch::Unknown
                } else {
                    self.pattern_matches_a(body, &scrut_v, arm.pattern, &mut PatBindings::new())
                };
                let body_lat =
                    arm_lattice_for_feasible_join(self.operand_to_lattice_a(body, arm.body));
                match pm {
                    PatternMatch::No => {}
                    PatternMatch::Yes => {
                        if candidates.is_empty() {
                            return self.operand_to_lattice_a(body, arm.body);
                        }
                        candidates.push(body_lat);
                        yes_found = true;
                        break;
                    }
                    PatternMatch::Unknown => candidates.push(body_lat),
                }
            }
            if !yes_found {
                return Lattice::NonConst;
            }
            join_all(&candidates)
        } else {
            if !is_provably_exhaustive_a(body, arms) {
                return Lattice::NonConst;
            }
            let mut acc = Lattice::Unevaluated;
            for arm in arms {
                acc = acc.join(arm_lattice_for_feasible_join(
                    self.operand_to_lattice_a(body, arm.body),
                ));
            }
            acc
        }
    }

    /// Look up a `(module_source, name)` global in the installed
    /// [`GlobalEnv`]. Absent keys default to [`Lattice::Unevaluated`]
    /// — the engine simply has no information, same convention as
    /// un-bound locals.
    ///
    /// `IndexMap` lookup needs an owned tuple key, so each call clones
    /// `ModuleSource` (one `String` allocation per variant) and the
    /// global name. If profiling shows this on a hot path, switch the
    /// env to `IndexMap<ModuleSource, IndexMap<String, Lattice>>` or
    /// implement `Borrow`-keyed lookup; it's left flat for now since
    /// `GlobalVarGet` nodes are sparse compared to local reads.
    pub(super) fn global_lattice(&self, module_source: &ModuleSource, name: &str) -> Lattice {
        let Some(globals) = self.globals else {
            return Lattice::Unevaluated;
        };
        globals
            .get(&(module_source.clone(), name.to_string()))
            .cloned()
            .unwrap_or(Lattice::Unevaluated)
    }
}
