//! Per-expression mod/ref summary for Wado NIR.
//!
//! A [`ModRef`] is a coarse, conservative summary of what a [`NirExpr`]
//! or [`NirStmt`] (together with its sub-tree) does to machine state:
//! which locals/globals it reads and writes, whether it touches the GC
//! heap or linear memory, whether it may transfer control non-locally,
//! whether it can call into arbitrary user code, and whether it may
//! trap.
//!
//! Unrelated to Wado's algebraic-effect / `with`-clause machinery; this
//! is the classical compiler-optimization notion (cf. LLVM
//! `ModRefInfo`, GCC `mod` / `ref` sets). Lives at NIR because that's
//! where every modref consumer in this compiler also lives (`cse`,
//! `dae`, `dce`, `copy_prop`, `store_load_forward`, `alias`, and the
//! `elide_box_local` pass that motivated v1).
//!
//! ## Client API
//!
//! Passes consume the summary through three predicates:
//!
//! - [`ModRef::is_re_evaluation_safe`] — can an expression with this
//!   summary be moved to a later program point without changing
//!   observable behavior?
//! - [`ModRef::may_clobber`] — could the writes implied by `self`
//!   invalidate any read implied by `other`? Wasm-semantics-accurate:
//!   `calls` are treated as touching only globals / GC heap / linear
//!   memory (callees cannot reach the caller's locals).
//! - [`can_move_past`] — convenience for the common "skip an
//!   intervening statement while erasing a candidate local" check used
//!   by adjacent-use elision passes.
//!
//! ## Granularity (v1)
//!
//! Coarse: one R/W flag pair per heap / memory channel; calls are
//! "everything except locals"; traps are a single boolean. The
//! representation is private to this module — passes only call the
//! predicates above — so refining the internals (per-`TypeId` GC heap,
//! per-callee `stores`-aware effect summaries, `Cast`-kind precision,
//! …) does not require call-site churn.
//!
//! NIR-specific assets the v1 implementation does NOT yet exploit but
//! is designed to grow into:
//!
//! - `NirFunction::stores` declares which `&` / `&mut` parameters the
//!   callee may store references through. A `Call { func }` whose
//!   callee's `stores` is empty cannot mutate the caller's locals via
//!   any argument, even when the argument is a reference type.
//! - `Cast` kind: today every `Cast` is conservatively marked
//!   `may_trap`. Refining to the actual numeric / ref-cast taxonomy
//!   lets pure float→float / int-widening casts ride past
//!   trap-conflicting intervening stmts.
//!
//! ## Discipline for extending [`NirExprKind`] / [`NirStmtKind`]
//!
//! [`ModRef::accumulate_expr`] / [`ModRef::accumulate_stmt`] enumerate
//! every effectful variant explicitly. Pure value-producing variants
//! (constants, arithmetic, etc.) fall into terminal arms that
//! contribute nothing of their own. When adding a new variant that
//! introduces a new kind of effect, add it explicitly to the relevant
//! `accumulate_*` — otherwise it silently defaults to "pure" and the
//! soundness of downstream passes is lost.
//!
//! The companion test [`tests::known_effectful_variants_are_explicit`]
//! constructs one expression / statement of each effectful shape and
//! asserts the summary picks up the expected flag, so an accidental
//! fall-through into a default arm surfaces as a test failure.

use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{
    NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirPattern, NirStmt, NirStmtKind, NirUnaryOp,
};

/// Read / write flags for a single state channel (e.g., GC heap or
/// linear memory).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Channel {
    pub reads: bool,
    pub writes: bool,
}

impl Channel {
    #[allow(dead_code)] // consumed by ModRef::join — exposed for future passes
    fn join(&mut self, other: Channel) {
        self.reads |= other.reads;
        self.writes |= other.writes;
    }
}

/// Control-flow effect of an instruction tree.
///
/// Ordered weakest → strongest; merging takes the [`Ord`] max.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Control {
    /// Completes normally and returns the value of the last
    /// sub-expression. Covers pure value ops, GC reads, calls, and
    /// writes (assignments, struct/index stores).
    #[default]
    Linear,
    /// Body is executed only on some inputs (`If`) or repeatedly
    /// (`Loop`). Effects are tracked as the union of all paths; the
    /// `Conditional` value only signals to motion passes that the path
    /// through this construct is not unique.
    Conditional,
    /// Transfers control out of the enclosing scope (`Return`, `Break`,
    /// `Continue`). Once an intervening statement carries this label,
    /// subsequent siblings — including any use we hoped to substitute
    /// into — may never execute.
    NonLocal,
}

impl Control {
    fn join(&mut self, other: Control) {
        if other > *self {
            *self = other;
        }
    }
}

/// Modifies / references summary of a [`NirExpr`] or [`NirStmt`] and
/// its sub-tree.
///
/// All fields are conservatively monotonic: once a flag is `true` or a
/// set is non-empty, refining the summary only adds more information
/// — it never invalidates a pass that relied on the coarse signal.
#[derive(Debug, Clone, Default)]
pub(super) struct ModRef {
    /// Locals (by dense `local_index`) named by any `Local { .. }` read.
    pub local_reads: IndexSet<u32>,
    /// Locals written by `Let { local_index, .. }`, `Assign { target:
    /// Local { .. } }`, or `LetDestructure` bindings.
    pub local_writes: IndexSet<u32>,
    /// Globals read by `GlobalVarGet`. Keyed by `(module, name)` —
    /// `ModuleSource` is interned so equality is cheap.
    pub global_reads: IndexSet<(ModuleSource, String)>,
    /// Globals written by `GlobalVarSet`.
    pub global_writes: IndexSet<(ModuleSource, String)>,
    /// GC heap: structs, arrays, and tables. v1 is coarse: a single
    /// read / write pair, no per-type or per-field refinement.
    pub heap: Channel,
    /// Linear memory. NIR rarely surfaces raw memory ops; this field
    /// is reserved for future refinements (today it stays `false`
    /// because no NIR construct lowers to a memory op without first
    /// going through a `Call`).
    pub memory: Channel,
    /// Control transfer behavior of the tree.
    pub control: Control,
    /// Tree contains a direct, method, indirect, or CM raw call.
    /// Callees may touch any global / heap / memory state we cannot
    /// see; locals are NOT clobbered by calls (Wasm locals are private
    /// to the calling frame and not addressable from another function).
    pub calls: bool,
    /// Tree allocates one or more fresh GC objects. Distinct from
    /// `heap.writes`: allocation does not clobber existing heap state,
    /// but it produces a new object identity and so prevents
    /// re-evaluation at a different program point.
    pub allocates: bool,
    /// Tree may trap at runtime (null dereference, OOB index, integer
    /// divide / remainder by zero, narrowing cast out of range, …).
    /// Independent of [`Control::NonLocal`].
    pub may_trap: bool,
}

impl ModRef {
    /// Compute the summary of `expr` and its sub-tree.
    pub fn of_expr(expr: &NirExpr) -> Self {
        let mut mr = ModRef::default();
        mr.accumulate_expr(expr);
        mr
    }

    /// Compute the summary of `stmt` and its sub-tree.
    pub fn of_stmt(stmt: &NirStmt) -> Self {
        let mut mr = ModRef::default();
        mr.accumulate_stmt(stmt);
        mr
    }

    /// Merge another summary into `self`. Sets union, flags OR,
    /// control takes [`Ord`] max.
    #[allow(dead_code)]
    pub fn join(&mut self, other: ModRef) {
        self.local_reads.extend(other.local_reads);
        self.local_writes.extend(other.local_writes);
        self.global_reads.extend(other.global_reads);
        self.global_writes.extend(other.global_writes);
        self.heap.join(other.heap);
        self.memory.join(other.memory);
        self.control.join(other.control);
        self.calls |= other.calls;
        self.allocates |= other.allocates;
        self.may_trap |= other.may_trap;
    }

    /// True iff re-evaluating an expression with this summary at a
    /// later program point cannot change observable behavior.
    ///
    /// Rejects calls, allocations (would create a new identity), any
    /// heap or memory access, any write, and any non-local control
    /// transfer. Trapping ops are NOT rejected — re-evaluation
    /// preserves the trap condition.
    #[allow(dead_code)]
    pub fn is_re_evaluation_safe(&self) -> bool {
        !self.calls
            && !self.allocates
            && !self.heap.reads
            && !self.heap.writes
            && !self.memory.reads
            && !self.memory.writes
            && self.local_writes.is_empty()
            && self.global_writes.is_empty()
            && matches!(self.control, Control::Linear)
    }

    /// True iff `self`'s writes (or any call inside `self`) might
    /// invalidate any of `other`'s reads.
    ///
    /// Call effects: callees can mutate globals, the GC heap, and
    /// linear memory, so a call clobbers reads of those channels.
    /// Callees CANNOT reach the caller's Wasm locals — locals live in
    /// the calling frame and are not addressable from another function
    /// — so a call does not clobber a `local_reads`-only expression.
    pub fn may_clobber(&self, other: &ModRef) -> bool {
        if self.calls && (!other.global_reads.is_empty() || other.heap.reads || other.memory.reads)
        {
            return true;
        }
        if !self.local_writes.is_disjoint(&other.local_reads) {
            return true;
        }
        if !self.global_writes.is_disjoint(&other.global_reads) {
            return true;
        }
        if self.heap.writes && other.heap.reads {
            return true;
        }
        if self.memory.writes && other.memory.reads {
            return true;
        }
        false
    }

    fn accumulate_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            // === Locals ===
            NirExprKind::Local { index, .. } => {
                self.local_reads.insert(*index);
            }
            NirExprKind::Assign { target, value } => {
                self.accumulate_assign_target(target);
                self.accumulate_expr(value);
            }

            // === Globals ===
            NirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                self.global_reads
                    .insert((module_source.clone(), name.clone()));
            }
            NirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => {
                self.global_writes
                    .insert((module_source.clone(), name.clone()));
                self.accumulate_expr(value);
            }

            // === Heap reads ===
            NirExprKind::FieldAccess { expr, .. } => {
                self.heap.reads = true;
                self.may_trap = true; // null receiver
                self.accumulate_expr(expr);
            }
            NirExprKind::Index { expr, index } => {
                self.heap.reads = true;
                self.may_trap = true; // null + OOB
                self.accumulate_expr(expr);
                self.accumulate_expr(index);
            }

            // === Heap allocations ===
            NirExprKind::StructLiteral { fields, .. } => {
                self.allocates = true;
                for f in fields {
                    self.accumulate_expr(&f.value);
                }
            }
            NirExprKind::TupleLiteral { elements } => {
                self.allocates = true;
                for e in elements {
                    self.accumulate_expr(e);
                }
            }
            NirExprKind::VariantConstruct { payload, .. } => {
                self.allocates = true;
                if let Some(p) = payload {
                    self.accumulate_expr(p);
                }
            }
            NirExprKind::ClosureToCanonical { functor, .. } => {
                self.allocates = true;
                self.accumulate_expr(functor);
            }

            // === Calls ===
            NirExprKind::Call { args, .. } => {
                self.calls = true;
                for a in args {
                    self.accumulate_expr(&a.expr);
                }
            }
            NirExprKind::MethodCall { receiver, args, .. } => {
                self.calls = true;
                self.accumulate_expr(receiver);
                for a in args {
                    self.accumulate_expr(&a.expr);
                }
            }
            NirExprKind::IndirectCall { callee, args } => {
                self.calls = true;
                self.accumulate_expr(callee);
                for a in args {
                    self.accumulate_expr(a);
                }
            }
            NirExprKind::CmRawCall { args, .. } => {
                self.calls = true;
                for a in args {
                    self.accumulate_expr(a);
                }
            }

            // === Trapping arithmetic / unary ===
            NirExprKind::Binary { left, op, right } => {
                if matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod) {
                    self.may_trap = true;
                }
                self.accumulate_expr(left);
                self.accumulate_expr(right);
            }
            NirExprKind::Unary { op, expr } => {
                match op {
                    NirUnaryOp::Deref => {
                        self.heap.reads = true;
                        self.may_trap = true;
                    }
                    NirUnaryOp::Ref | NirUnaryOp::MutRef => {}
                    NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot => {}
                }
                self.accumulate_expr(expr);
            }
            NirExprKind::Cast { expr, .. } => {
                // v1: conservatively trap-capable (numeric narrowing /
                // ref.cast). Refine when a consumer needs the precision.
                self.may_trap = true;
                self.accumulate_expr(expr);
            }

            // === Variant projection ===
            NirExprKind::VariantPayload { expr, .. } => {
                self.may_trap = true; // case mismatch
                self.accumulate_expr(expr);
            }
            NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
                self.accumulate_expr(expr);
            }
            NirExprKind::EnumConstruct { .. } => {}

            // === Control flow ===
            NirExprKind::Block(block) => {
                self.accumulate_block(block);
            }
            NirExprKind::LabeledBlock { block, .. } => {
                self.accumulate_block(block);
            }
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.accumulate_expr(condition);
                self.accumulate_block(then_branch);
                if let Some(eb) = else_branch {
                    self.accumulate_block(eb);
                }
                if self.control < Control::NonLocal {
                    self.control = Control::Conditional;
                }
            }
            NirExprKind::Match { expr, arms } => {
                self.accumulate_expr(expr);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.accumulate_expr(g);
                    }
                    self.accumulate_expr(&arm.body);
                }
                if self.control < Control::NonLocal {
                    self.control = Control::Conditional;
                }
            }
            NirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.accumulate_expr(scrutinee);
                for arm in arms {
                    self.accumulate_block(arm);
                }
                self.accumulate_block(default);
                if self.control < Control::NonLocal {
                    self.control = Control::Conditional;
                }
            }

            // === Pure value-producing leaves ===
            NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit => {}
        }
    }

    fn accumulate_assign_target(&mut self, target: &NirExpr) {
        match &target.kind {
            NirExprKind::Local { index, .. } => {
                self.local_writes.insert(*index);
            }
            NirExprKind::FieldAccess { expr, .. } => {
                self.heap.writes = true;
                self.may_trap = true; // null receiver
                self.accumulate_expr(expr);
            }
            NirExprKind::Index { expr, index } => {
                self.heap.writes = true;
                self.may_trap = true; // null + OOB
                self.accumulate_expr(expr);
                self.accumulate_expr(index);
            }
            NirExprKind::Unary {
                op: NirUnaryOp::Deref,
                expr,
            } => {
                self.heap.writes = true;
                self.may_trap = true;
                self.accumulate_expr(expr);
            }
            NirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                self.global_writes
                    .insert((module_source.clone(), name.clone()));
            }
            _ => {
                self.accumulate_expr(target);
            }
        }
    }

    fn accumulate_stmt(&mut self, stmt: &NirStmt) {
        match &stmt.kind {
            NirStmtKind::Let {
                local_index, value, ..
            } => {
                self.local_writes.insert(*local_index);
                self.accumulate_expr(value);
            }
            NirStmtKind::Expr(e) => {
                self.accumulate_expr(e);
            }
            NirStmtKind::Return { value } => {
                self.control.join(Control::NonLocal);
                if let Some(v) = value {
                    self.accumulate_expr(v);
                }
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.accumulate_expr(condition);
                self.accumulate_block(then_block);
                if let Some(eb) = else_block {
                    self.accumulate_block(eb);
                }
                if self.control < Control::NonLocal {
                    self.control = Control::Conditional;
                }
            }
            NirStmtKind::Loop { body } => {
                self.accumulate_block(body);
                if self.control < Control::NonLocal {
                    self.control = Control::Conditional;
                }
            }
            NirStmtKind::Break { value, .. } => {
                self.control.join(Control::NonLocal);
                if let Some(v) = value {
                    self.accumulate_expr(v);
                }
            }
            NirStmtKind::Continue => {
                self.control.join(Control::NonLocal);
            }
            NirStmtKind::LabeledBlock { block, .. } => {
                self.accumulate_block(block);
            }
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                self.accumulate_pattern_writes(pattern);
                self.accumulate_expr(value);
            }
        }
    }

    fn accumulate_block(&mut self, block: &NirBlock) {
        for s in &block.stmts {
            self.accumulate_stmt(s);
        }
    }

    fn accumulate_pattern_writes(&mut self, pat: &NirPattern) {
        match pat {
            NirPattern::Binding { local_index, .. } => {
                self.local_writes.insert(*local_index);
            }
            NirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.accumulate_pattern_writes(p);
                }
            }
            NirPattern::Variant { bindings, .. } => {
                for p in bindings {
                    self.accumulate_pattern_writes(p);
                }
            }
            NirPattern::Struct { fields, .. } => {
                for f in fields {
                    self.accumulate_pattern_writes(&f.pattern);
                }
            }
            NirPattern::Or(alts) => {
                for a in alts {
                    self.accumulate_pattern_writes(a);
                }
            }
            NirPattern::ConstantValue { expr } => {
                self.accumulate_expr(expr);
            }
            NirPattern::Wildcard
            | NirPattern::Literal(_)
            | NirPattern::Enum { .. }
            | NirPattern::Range { .. } => {}
        }
    }
}

/// Can the expression with summary `expr_mr` be moved past an
/// intervening statement with summary `int_mr`, while a candidate
/// local `candidate` is being eliminated by the rewrite?
///
/// Soundness conditions (all must hold):
///
/// 1. The intervening statement transfers control linearly
///    (`control == Linear`). `Conditional` and `NonLocal` intervenings
///    are rejected: in the `NonLocal` case the use site may never
///    execute; in the `Conditional` case some path through the
///    intervening might still escape via an inner `Break`, and we
///    over-approximate by bailing.
/// 2. The intervening statement does not read the candidate local.
///    With single-use candidates this is normally already guaranteed
///    by the pass's stats check; the test is a cheap defense against
///    future refactors.
/// 3. The intervening and the expression do not both `may_trap`.
///    If only one of them traps, the surviving trap fires at its own
///    point; if both can trap, the observable trap location differs.
///    Conservative for v1.
/// 4. The intervening statement's writes (or any call inside it) do
///    not clobber any of the expression's reads (the may-alias core of
///    [`ModRef::may_clobber`]).
pub(super) fn can_move_past(expr_mr: &ModRef, int_mr: &ModRef, candidate: u32) -> bool {
    if !matches!(int_mr.control, Control::Linear) {
        return false;
    }
    if int_mr.local_reads.contains(&candidate) {
        return false;
    }
    if int_mr.may_trap && expr_mr.may_trap {
        return false;
    }
    !int_mr.may_clobber(expr_mr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{
        CallArg, FunctionRef, NirBlock, NirExpr, NirExprKind, NirMatchArm, NirPattern, NirStmt,
        NirStmtKind, NirStructField,
    };
    use crate::tir::TypeId;
    use crate::token::Span;

    fn ty() -> TypeId {
        TypeId(0)
    }
    fn sp() -> Span {
        Span::default()
    }
    fn local(index: u32) -> NirExpr {
        NirExpr::new(
            NirExprKind::Local {
                index,
                name: format!("__l{index}"),
            },
            ty(),
            sp(),
        )
    }
    fn int(v: i64) -> NirExpr {
        NirExpr::new(
            NirExprKind::IntLiteral {
                value: v as u64,
                repr: v.to_string(),
            },
            ty(),
            sp(),
        )
    }
    fn let_stmt(index: u32, value: NirExpr) -> NirStmt {
        NirStmt::new(
            NirStmtKind::Let {
                name: format!("__l{index}"),
                local_index: index,
                is_mut: false,
                is_reactive: false,
                type_id: ty(),
                value,
                skip_value_copy: false,
            },
            sp(),
        )
    }
    fn assign(target: NirExpr, value: NirExpr) -> NirExpr {
        NirExpr::new(
            NirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value),
            },
            ty(),
            sp(),
        )
    }
    fn expr_stmt(e: NirExpr) -> NirStmt {
        NirStmt::new(NirStmtKind::Expr(e), sp())
    }
    fn global_get(name: &str) -> NirExpr {
        NirExpr::new(
            NirExprKind::GlobalVarGet {
                module_source: ModuleSource::prelude(),
                name: name.to_string(),
            },
            ty(),
            sp(),
        )
    }
    fn global_set(name: &str, value: NirExpr) -> NirExpr {
        NirExpr::new(
            NirExprKind::GlobalVarSet {
                module_source: ModuleSource::prelude(),
                name: name.to_string(),
                value: Box::new(value),
            },
            ty(),
            sp(),
        )
    }
    fn field_access(expr: NirExpr) -> NirExpr {
        NirExpr::new(
            NirExprKind::FieldAccess {
                expr: Box::new(expr),
                field_index: 0,
                field_name: "value".to_string(),
            },
            ty(),
            sp(),
        )
    }
    fn struct_literal(fields: Vec<NirExpr>) -> NirExpr {
        NirExpr::new(
            NirExprKind::StructLiteral {
                struct_type: ty(),
                struct_name: "T".to_string(),
                fields: fields
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| NirStructField {
                        name: format!("f{i}"),
                        value,
                        field_index: i as u32,
                    })
                    .collect(),
            },
            ty(),
            sp(),
        )
    }
    fn func_ref(name: &str) -> FunctionRef {
        FunctionRef {
            module_source: ModuleSource::prelude(),
            name: name.to_string(),
            monomorph_info: None,
            method_info: None,
        }
    }
    fn call(args: Vec<NirExpr>) -> NirExpr {
        NirExpr::new(
            NirExprKind::Call {
                func: func_ref("f"),
                type_args: vec![],
                args: args.into_iter().map(|e| CallArg::new(e, false)).collect(),
            },
            ty(),
            sp(),
        )
    }
    fn block(stmts: Vec<NirStmt>) -> NirBlock {
        NirBlock::new(stmts, sp())
    }

    // -----------------------------------------------------------------
    // Leaves
    // -----------------------------------------------------------------

    #[test]
    fn local_read_records_index() {
        let mr = ModRef::of_expr(&local(3));
        assert!(mr.local_reads.contains(&3));
        assert!(mr.local_writes.is_empty());
    }

    #[test]
    fn let_writes_local_and_inherits_rhs_reads() {
        let mr = ModRef::of_stmt(&let_stmt(7, local(3)));
        assert!(mr.local_writes.contains(&7));
        assert!(mr.local_reads.contains(&3));
    }

    #[test]
    fn assign_to_local_writes_local() {
        let mr = ModRef::of_expr(&assign(local(7), local(3)));
        assert!(mr.local_writes.contains(&7));
        assert!(mr.local_reads.contains(&3));
    }

    #[test]
    fn assign_to_field_is_heap_write() {
        let mr = ModRef::of_expr(&assign(field_access(local(3)), int(0)));
        assert!(mr.heap.writes);
        assert!(mr.local_writes.is_empty());
        assert!(mr.may_trap);
    }

    #[test]
    fn global_get_records_module_qualified_name() {
        let mr = ModRef::of_expr(&global_get("G"));
        assert!(
            mr.global_reads
                .contains(&(ModuleSource::prelude(), "G".to_string()))
        );
    }

    #[test]
    fn global_set_records_write_and_rhs() {
        let mr = ModRef::of_expr(&global_set("G", local(3)));
        assert!(
            mr.global_writes
                .contains(&(ModuleSource::prelude(), "G".to_string()))
        );
        assert!(mr.local_reads.contains(&3));
    }

    // -----------------------------------------------------------------
    // Heap
    // -----------------------------------------------------------------

    #[test]
    fn field_access_is_heap_read_and_may_trap() {
        let mr = ModRef::of_expr(&field_access(local(0)));
        assert!(mr.heap.reads);
        assert!(!mr.heap.writes);
        assert!(mr.may_trap);
        assert!(!mr.allocates);
    }

    #[test]
    fn struct_literal_allocates_but_does_not_write_heap() {
        let mr = ModRef::of_expr(&struct_literal(vec![local(0)]));
        assert!(mr.allocates);
        assert!(!mr.heap.writes);
    }

    #[test]
    fn deref_is_heap_read_and_may_trap() {
        let mr = ModRef::of_expr(&NirExpr::new(
            NirExprKind::Unary {
                op: NirUnaryOp::Deref,
                expr: Box::new(local(0)),
            },
            ty(),
            sp(),
        ));
        assert!(mr.heap.reads);
        assert!(mr.may_trap);
    }

    // -----------------------------------------------------------------
    // Calls
    // -----------------------------------------------------------------

    #[test]
    fn call_sets_calls_and_inherits_arg_reads() {
        let mr = ModRef::of_expr(&call(vec![local(0)]));
        assert!(mr.calls);
        assert!(mr.local_reads.contains(&0));
    }

    // -----------------------------------------------------------------
    // Trapping arithmetic
    // -----------------------------------------------------------------

    #[test]
    fn integer_divide_may_trap() {
        let mr = ModRef::of_expr(&NirExpr::new(
            NirExprKind::Binary {
                left: Box::new(int(1)),
                op: NirBinaryOp::Div,
                right: Box::new(int(0)),
            },
            ty(),
            sp(),
        ));
        assert!(mr.may_trap);
    }

    #[test]
    fn integer_add_does_not_trap() {
        let mr = ModRef::of_expr(&NirExpr::new(
            NirExprKind::Binary {
                left: Box::new(int(1)),
                op: NirBinaryOp::Add,
                right: Box::new(int(2)),
            },
            ty(),
            sp(),
        ));
        assert!(!mr.may_trap);
    }

    // -----------------------------------------------------------------
    // Control
    // -----------------------------------------------------------------

    #[test]
    fn return_is_non_local() {
        let mr = ModRef::of_stmt(&NirStmt::new(NirStmtKind::Return { value: None }, sp()));
        assert_eq!(mr.control, Control::NonLocal);
    }

    #[test]
    fn break_is_non_local() {
        let mr = ModRef::of_stmt(&NirStmt::new(
            NirStmtKind::Break {
                label: None,
                value: None,
            },
            sp(),
        ));
        assert_eq!(mr.control, Control::NonLocal);
    }

    #[test]
    fn if_stmt_with_linear_arms_is_conditional() {
        let mr = ModRef::of_stmt(&NirStmt::new(
            NirStmtKind::If {
                condition: int(1),
                then_block: block(vec![expr_stmt(int(0))]),
                else_block: Some(block(vec![expr_stmt(int(1))])),
            },
            sp(),
        ));
        assert_eq!(mr.control, Control::Conditional);
    }

    #[test]
    fn loop_with_pure_body_is_conditional() {
        let mr = ModRef::of_stmt(&NirStmt::new(
            NirStmtKind::Loop {
                body: block(vec![expr_stmt(int(0))]),
            },
            sp(),
        ));
        assert_eq!(mr.control, Control::Conditional);
    }

    #[test]
    fn match_is_conditional() {
        let mr = ModRef::of_expr(&NirExpr::new(
            NirExprKind::Match {
                expr: Box::new(local(0)),
                arms: vec![NirMatchArm {
                    pattern: NirPattern::Wildcard,
                    guard: None,
                    body: int(0),
                    span: sp(),
                }],
            },
            ty(),
            sp(),
        ));
        assert_eq!(mr.control, Control::Conditional);
    }

    // -----------------------------------------------------------------
    // Predicates
    // -----------------------------------------------------------------

    #[test]
    fn pure_expression_is_re_evaluation_safe() {
        let mr = ModRef::of_expr(&NirExpr::new(
            NirExprKind::Binary {
                left: Box::new(int(5)),
                op: NirBinaryOp::Add,
                right: Box::new(local(0)),
            },
            ty(),
            sp(),
        ));
        assert!(mr.is_re_evaluation_safe());
    }

    #[test]
    fn heap_read_is_not_re_evaluation_safe() {
        let mr = ModRef::of_expr(&field_access(local(0)));
        assert!(!mr.is_re_evaluation_safe());
    }

    #[test]
    fn may_clobber_local_write_vs_local_read() {
        let writer = ModRef::of_expr(&assign(local(1), int(0)));
        let reader = ModRef::of_expr(&local(1));
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_unrelated_locals_is_false() {
        let writer = ModRef::of_expr(&assign(local(2), int(0)));
        let reader = ModRef::of_expr(&local(1));
        assert!(!writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_heap_write_vs_heap_read() {
        let writer = ModRef::of_expr(&assign(field_access(local(1)), int(0)));
        let reader = ModRef::of_expr(&field_access(local(2)));
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_call_clobbers_heap_read() {
        let writer = ModRef::of_expr(&call(vec![]));
        let reader = ModRef::of_expr(&field_access(local(0)));
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_call_clobbers_global_read() {
        let writer = ModRef::of_expr(&call(vec![]));
        let reader = ModRef::of_expr(&global_get("G"));
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_call_does_not_clobber_local_only_read() {
        let writer = ModRef::of_expr(&call(vec![]));
        let reader = ModRef::of_expr(&local(0));
        assert!(!writer.may_clobber(&reader));
    }

    // -----------------------------------------------------------------
    // can_move_past
    // -----------------------------------------------------------------

    #[test]
    fn can_move_past_pure_local_copy() {
        let expr = ModRef::of_expr(&field_access(local(0)));
        let intervening = ModRef::of_stmt(&let_stmt(5, local(6)));
        assert!(can_move_past(&expr, &intervening, 1));
    }

    #[test]
    fn cannot_move_past_when_intervening_writes_inner_read() {
        let expr = ModRef::of_expr(&field_access(local(0)));
        let intervening = ModRef::of_expr(&assign(local(0), int(0)));
        assert!(!can_move_past(&expr, &intervening, 1));
    }

    #[test]
    fn cannot_move_past_heap_write_when_expr_reads_heap() {
        let expr = ModRef::of_expr(&field_access(local(0)));
        let intervening = ModRef::of_expr(&assign(field_access(local(2)), int(0)));
        assert!(!can_move_past(&expr, &intervening, 1));
    }

    #[test]
    fn cannot_move_heap_read_past_call() {
        let expr = ModRef::of_expr(&field_access(local(0)));
        let intervening = ModRef::of_expr(&call(vec![]));
        assert!(!can_move_past(&expr, &intervening, 1));
    }

    #[test]
    fn can_move_local_only_read_past_call() {
        let expr = ModRef::of_expr(&local(0));
        let intervening = ModRef::of_expr(&call(vec![]));
        assert!(can_move_past(&expr, &intervening, 1));
    }

    #[test]
    fn cannot_move_past_return() {
        let expr = ModRef::of_expr(&local(0));
        let intervening = ModRef::of_stmt(&NirStmt::new(NirStmtKind::Return { value: None }, sp()));
        assert!(!can_move_past(&expr, &intervening, 1));
    }

    #[test]
    fn cannot_move_past_if_even_with_linear_body() {
        let expr = ModRef::of_expr(&local(0));
        let intervening = ModRef::of_stmt(&NirStmt::new(
            NirStmtKind::If {
                condition: int(1),
                then_block: block(vec![]),
                else_block: None,
            },
            sp(),
        ));
        assert!(!can_move_past(&expr, &intervening, 1));
    }

    #[test]
    fn cannot_move_when_intervening_reads_candidate() {
        let expr = ModRef::of_expr(&int(0));
        let intervening = ModRef::of_stmt(&let_stmt(5, local(7)));
        assert!(!can_move_past(&expr, &intervening, 7));
    }

    #[test]
    fn cannot_move_when_both_may_trap() {
        let expr = ModRef::of_expr(&field_access(local(0)));
        let intervening = ModRef::of_stmt(&let_stmt(
            5,
            NirExpr::new(
                NirExprKind::Binary {
                    left: Box::new(local(1)),
                    op: NirBinaryOp::Div,
                    right: Box::new(local(2)),
                },
                ty(),
                sp(),
            ),
        ));
        assert!(!can_move_past(&expr, &intervening, 99));
    }

    // -----------------------------------------------------------------
    // Extension discipline canary
    // -----------------------------------------------------------------

    #[test]
    fn known_effectful_variants_are_explicit() {
        assert!(ModRef::of_expr(&local(0)).local_reads.contains(&0));
        assert!(
            ModRef::of_stmt(&let_stmt(0, int(0)))
                .local_writes
                .contains(&0)
        );
        assert!(
            ModRef::of_expr(&global_get("G"))
                .global_reads
                .contains(&(ModuleSource::prelude(), "G".to_string()))
        );
        assert!(ModRef::of_expr(&field_access(local(0))).heap.reads);
        assert!(
            ModRef::of_expr(&assign(field_access(local(0)), int(0)))
                .heap
                .writes
        );
        assert!(ModRef::of_expr(&struct_literal(vec![int(0)])).allocates);
        assert!(ModRef::of_expr(&call(vec![])).calls);
        assert_eq!(
            ModRef::of_stmt(&NirStmt::new(NirStmtKind::Return { value: None }, sp())).control,
            Control::NonLocal
        );
    }
}
