//! Body globalization — hoist a constant, read-only aggregate `let` binding
//! out of a function body into a shared immutable module global.
//!
//! A constant-shaped aggregate bound by a `let` and only ever read is rebuilt
//! on every call (or loop iteration) — pure waste, since Wasm 3.0 GC can build
//! it once at instantiation. This pass detects such a binding and hoists it:
//!
//! ```text
//! fn f() { let xs = [10, 20, 30]; … xs.repr … }   // rebuilt per call
//!   ⇒
//! global __const_obj_0 = <lazy>;                   // built once
//! fn f() { __const_obj_0 = [10, 20, 30]; … global:__const_obj_0.repr … }
//! ```
//!
//! The hoisted global mirrors what `lower::plan::globals::extract` produces for
//! a non-const user global: a Wasm-mutable, Wado-immutable slot with a `null`
//! placeholder init, assigned once by an inline `GlobalVarSet`. The existing
//! [`crate::wir_optimize::const_global`] pass classifies it eager/lazy.
//!
//! ## Soundness
//!
//! Two independent gates, both load-bearing:
//!
//! - **Closed const aggregate** ([`is_globalizable_const`]). The initializer
//!   must be a side-effect-free constant with no free locals.
//! - **Read-only** ([`is_readonly_body`]). Every use of the binding must be a
//!   borrowing / reading position; any `&mut`, any `&mut self` method, any
//!   assignment to it or a projection, and any by-value consuming use
//!   disqualify it. See the per-arm comments in [`expr_readonly`].
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): a project-level pass
//! whose analysis (read-only gate, const check) and mutation (read rewrite,
//! `let` → `GlobalVarSet`) read and mutate the arena `Body` directly. The
//! `expr_readonly` arms mirror the former tree gate exactly to keep the
//! soundness decision — and therefore codegen — identical.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{NirFunction, NirGlobal, NirUnaryOp};
use crate::nir_arena::{
    BlockId, BlockNode, Body, ExprBody, ExprId, ExprKind, ExprNode, NodeRef, Operand, StmtId,
    StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::arena_query::{expr_mentions_local, is_local, strip_refs};

/// A hoisting candidate, identified by its owning function. Resolved in an
/// immutable analysis phase, applied in a later mutation phase to avoid
/// `RefCell` borrow conflicts.
struct Candidate {
    func_idx: usize,
    ty: TypeId,
    module_source: ModuleSource,
    kind: CandidateKind,
}

enum CandidateKind {
    /// A `let` binding whose value is a closed constant aggregate.
    LetBinding { local_index: u32 },
    /// A constant aggregate literal referenced via `&` directly at an
    /// expression position (typically a call argument) with no enclosing
    /// `let` — e.g. the synthesized `serde` field key in
    /// `st.field(&"id_str", &self.id_str)`. Hoisted in place: the
    /// `Unary::Ref`'s inner literal is wrapped in a
    /// `{ GlobalVarSet(G, <literal>); GlobalVarGet(G) }` block, so it's
    /// nameable and promotable to an eager Wasm constant just like a hoisted
    /// `let`, without moving it out of its original call site.
    InlineRef {
        /// The `Unary { op: Ref, .. }` node whose inner operand is hoisted.
        ref_expr: ExprId,
    },
}

pub fn globalize_const_objects(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.clone();

    // Phase 1 — analysis (all immutable borrows).
    let gate = Gate {
        funcs: &project.functions,
        type_table: &type_table,
    };
    let mut candidates: Vec<Candidate> = Vec::new();
    for (fi, f) in project.functions.iter().enumerate() {
        let f = f.borrow();
        if skip_function(&f) {
            continue;
        }
        let Some(body) = &f.body else {
            continue;
        };
        collect_candidates(body, &gate, fi, &f.module_source, &mut candidates);
    }
    if candidates.is_empty() {
        return false;
    }

    // Phase 2 — mutation. Number from the count of pre-existing `__const_obj_*`
    // globals so names stay unique across invocations.
    let base = project
        .globals
        .iter()
        .filter(|g| g.name.starts_with(crate::name::CONST_OBJ_GLOBAL_PREFIX))
        .count();
    for (n, cand) in (base..).zip(candidates) {
        let name = format!("{}{n}", crate::name::CONST_OBJ_GLOBAL_PREFIX);
        let Candidate {
            func_idx,
            ty,
            module_source,
            kind,
        } = cand;
        let is_inline_ref = matches!(kind, CandidateKind::InlineRef { .. });

        let mut func = project.functions[func_idx].borrow_mut();
        let body = func.body.as_mut().expect("candidate function has a body");
        match kind {
            CandidateKind::LetBinding { local_index } => {
                // Rewrite reads first (the let's own value is const and
                // references no local index, so it is untouched), then
                // replace the binding.
                rewrite_reads(body, local_index, &module_source, &name, ty);
                assert!(
                    replace_let_with_set(body, local_index, &module_source, &name),
                    "[NIR] const_object_globalization: LetBinding candidate's `let` \
                     (local {local_index}) went missing between collection and mutation"
                );
            }
            CandidateKind::InlineRef { ref_expr } => {
                hoist_inline_ref(body, ref_expr, &module_source, &name, ty);
            }
        }
        drop(func);

        project.globals.push(NirGlobal {
            name,
            ty,
            initializer: ExprBody::wrapping_value(
                crate::nir_value_graph::ValueKind::Null,
                ty,
                crate::token::Span::new(0, 0, 1, 1),
            ),
            mutable: true,
            wado_mutable: false,
            visibility: crate::ast::Visibility::Private,
            module_source,
            span: crate::token::Span::new(0, 0, 1, 1),
            is_nullable: true,
            lazy_init: true,
            locals: Vec::new(),
            prefer_fixed_string_repr: is_inline_ref,
        });
    }
    true
}

/// Find every hoisting candidate in `body`: a qualifying `let` binding
/// ([`let_stmt_qualifies`]) or an unnested `Unary { op: Ref, expr: <closed
/// const aggregate> }` node. Neither is recursed into further.
fn collect_candidates(
    body: &Body,
    gate: &Gate<'_>,
    func_idx: usize,
    module_source: &ModuleSource,
    out: &mut Vec<Candidate>,
) {
    let single_decl_locals = locals_declared_once(body);
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Let {
                local_index,
                type_id,
                ..
            } = &body.stmts[s].kind
            && single_decl_locals.contains(local_index)
            && let_stmt_qualifies(body, s, gate)
        {
            out.push(Candidate {
                func_idx,
                ty: *type_id,
                module_source: module_source.clone(),
                kind: CandidateKind::LetBinding {
                    local_index: *local_index,
                },
            });
            continue;
        }
        if let NodeRef::Expr(id) = node
            && let ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: Operand::Expr(inner),
            } = &body.exprs[id].kind
        {
            let inner = *inner;
            let inner_ty = body.exprs[inner].type_id;
            if gate.is_reference_type(inner_ty)
                && is_globalizable_const(body, inner, &mut IndexSet::default())
                && contains_aggregate(body, inner)
            {
                out.push(Candidate {
                    func_idx,
                    ty: inner_ty,
                    module_source: module_source.clone(),
                    kind: CandidateKind::InlineRef { ref_expr: id },
                });
                continue;
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// Local indices declared by exactly one `let` statement in `body`.
/// `rewrite_reads`/`replace_let_with_set` operate on a local index across the
/// whole body, so a locally-reused index (e.g. from `labeled_block_fusion`
/// threading one arm into several mutually exclusive branches) must not be
/// hoisted — it would leave some branches reading a global only a different
/// branch ever sets.
fn locals_declared_once(body: &Body) -> IndexSet<u32> {
    let mut seen: IndexSet<u32> = IndexSet::default();
    let mut dupes: IndexSet<u32> = IndexSet::default();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
            && !seen.insert(*local_index)
        {
            dupes.insert(*local_index);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    seen.retain(|idx| !dupes.contains(idx));
    seen
}

/// Rewrite an `InlineRef` candidate in place: `Unary { op: Ref, expr: E }`
/// becomes `Unary { op: Ref, expr: { GlobalVarSet(name, E); GlobalVarGet(name) } }`.
/// `E` itself is untouched (moved, not copied) — it still runs exactly where
/// it always did, but under a name `wir_optimize::const_global` can promote
/// to a Wasm-instantiation-time constant, dropping the runtime assignment
/// entirely when `E` is const-expressible.
fn hoist_inline_ref(
    body: &mut Body,
    ref_expr: ExprId,
    module_source: &ModuleSource,
    name: &str,
    ty: TypeId,
) {
    let ExprKind::Unary {
        expr: Operand::Expr(inner),
        ..
    } = body.exprs[ref_expr].kind
    else {
        unreachable!("InlineRef candidate must still be a Unary{{ op: Ref, .. }} node")
    };
    let span = body.exprs[ref_expr].span;

    let set_expr = body.exprs.push(ExprNode {
        kind: ExprKind::GlobalVarSet {
            module_source: module_source.clone(),
            name: name.to_string(),
            value: Operand::Expr(inner),
        },
        type_id: TypeTable::UNIT,
        span,
    });
    let set_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(Operand::Expr(set_expr)),
        span,
    });
    let get_expr = body.exprs.push(ExprNode {
        kind: ExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.to_string(),
        },
        type_id: ty,
        span,
    });
    let get_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(Operand::Expr(get_expr)),
        span,
    });
    let wrap_block = body.blocks.push(BlockNode {
        stmts: vec![set_stmt, get_stmt],
        span,
    });
    let block_expr = body.exprs.push(ExprNode {
        kind: ExprKind::Block(wrap_block),
        type_id: ty,
        span,
    });
    let ExprKind::Unary { expr, .. } = &mut body.exprs[ref_expr].kind else {
        unreachable!("checked above")
    };
    *expr = Operand::Expr(block_expr);
}

/// Skip synthesized init / CM-binding functions.
fn skip_function(f: &NirFunction) -> bool {
    f.is_cm_binding
        || f.is_dispatch_wrapper
        || f.name == crate::name::MODULE_INIT_FUNCTION
        || f.name == crate::name::MODULES_INIT_FUNCTION
        || f.value_copy_type().is_some()
        // `wir_build::register_globals` asserts no `NirGlobal` has a WASI
        // `module_source` — a plain helper function can live in a
        // `wasi:*`-namespaced file even though its own CM-binding glue is
        // excluded above, so this still needs its own check.
        || f.module_source.is_wasi()
}

// ---------------------------------------------------------------------------
// Phase 1 — candidate collection
// ---------------------------------------------------------------------------

/// True when `stmt` is a `let` binding [`collect_candidates`] hoists whole.
/// Also consulted inline by [`collect_candidates`]'s own walk to decide
/// whether to recurse into that same `let`'s value looking for a nested
/// `&`-literal: the whole binding already subsumes it, and hoisting both
/// would nest one global's `GlobalVarSet` inside another's initializer — a
/// shape nothing downstream (`wir_optimize::const_global`'s
/// single-assignment classifier, in particular) is prepared to see.
fn let_stmt_qualifies(body: &Body, stmt: StmtId, gate: &Gate<'_>) -> bool {
    let StmtKind::Let {
        local_index,
        value,
        type_id,
        ..
    } = &body.stmts[stmt].kind
    else {
        return false;
    };
    let (local_index, value, type_id) = (*local_index, *value, *type_id);
    gate.is_reference_type(type_id)
        && is_globalizable_const_operand(body, value, &mut IndexSet::default())
        && contains_aggregate_operand(body, value)
        && is_readonly_body(body, local_index, gate)
}

fn is_globalizable_const_operand(body: &Body, op: Operand, bound: &mut IndexSet<u32>) -> bool {
    op.as_expr()
        .is_none_or(|e| is_globalizable_const(body, e, bound))
}

/// Recursively true when `expr` is a closed constant aggregate value.
fn is_globalizable_const(body: &Body, expr: ExprId, bound: &mut IndexSet<u32>) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::EnumConstruct { .. } => true,
        // A packed `Array<u8>` (a `String` / `List<u8>` literal's `repr`) is a
        // closed constant with no free locals.
        ExprKind::PackedArray(_) => true,
        ExprKind::Local { index, .. } => bound.contains(index),
        ExprKind::StructLiteral { fields, .. } => {
            let fields: Vec<ExprId> = fields.iter().filter_map(|f| f.value.as_expr()).collect();
            fields
                .iter()
                .all(|&v| is_globalizable_const(body, v, bound))
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            elements
                .iter()
                .all(|&e| is_globalizable_const_operand(body, e, bound))
        }
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_none_or(|p| is_globalizable_const_operand(body, p, bound))
        }
        // Transparent value wrappers.
        ExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::Neg,
            expr: inner,
        }
        | ExprKind::Cast { expr: inner, .. } => is_globalizable_const_operand(body, *inner, bound),
        // The builder-temp block an array / list literal leaves.
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            block_is_const(body, *block, bound)
        }
        _ => false,
    }
}

fn block_is_const(body: &Body, block: BlockId, bound: &mut IndexSet<u32>) -> bool {
    let stmts = &body.blocks[block].stmts;
    let Some((last, init)) = stmts.split_last() else {
        return false;
    };
    let (last, init) = (*last, init.to_vec());
    for stmt in init {
        let StmtKind::Let {
            local_index, value, ..
        } = &body.stmts[stmt].kind
        else {
            return false;
        };
        let (local_index, value) = (*local_index, *value);
        if !is_globalizable_const_operand(body, value, bound) {
            return false;
        }
        bound.insert(local_index);
    }
    match &body.stmts[last].kind {
        StmtKind::Expr(e) => e
            .as_expr()
            .is_some_and(|e| is_globalizable_const(body, e, bound)),
        _ => false,
    }
}

fn contains_aggregate_operand(body: &Body, op: Operand) -> bool {
    op.as_expr().is_some_and(|e| contains_aggregate(body, e))
}

/// True when `expr` contains at least one aggregate constructor.
fn contains_aggregate(body: &Body, expr: ExprId) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::StructLiteral { .. }
        | ExprKind::TupleLiteral { .. }
        | ExprKind::ArrayLiteral { .. }
        | ExprKind::VariantConstruct { .. } => true,
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            contains_aggregate_operand(body, *inner)
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            let stmts = body.blocks[*block].stmts.clone();
            stmts.iter().any(|&s| match &body.stmts[s].kind {
                StmtKind::Let { value, .. } => contains_aggregate_operand(body, *value),
                StmtKind::Expr(value) => {
                    value.as_expr().is_some_and(|e| contains_aggregate(body, e))
                }
                _ => false,
            })
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Read-only gate
// ---------------------------------------------------------------------------

struct Gate<'a> {
    funcs: &'a [Rc<RefCell<NirFunction>>],
    type_table: &'a Rc<RefCell<TypeTable>>,
}

impl Gate<'_> {
    fn is_reference_type(&self, ty: TypeId) -> bool {
        !matches!(
            self.type_table.borrow().get(ty),
            ResolvedType::Primitive(_) | ResolvedType::Unit | ResolvedType::Never
        )
    }

    /// `Some(true)` when `func`'s `self` parameter is `&mut self`,
    /// `Some(false)` when it is `&self` / by-value, `None` when unresolvable
    /// (conservatively treated as mutating).
    fn callee_mutates_self(&self, func_id: crate::nir::FuncId) -> Option<bool> {
        use cranelift_entity::EntityRef;
        let f = self.funcs.get(func_id.index())?.borrow();
        let p0 = f.params.first()?;
        Some(matches!(
            self.type_table.borrow().get(p0.type_id),
            ResolvedType::MutRef(_)
        ))
    }
}

/// True when every use of local `idx` in `body` keeps it immutable.
fn is_readonly_body(body: &Body, idx: u32, gate: &Gate<'_>) -> bool {
    block_readonly(body, body.root, idx, gate)
}

fn block_readonly(body: &Body, block: BlockId, idx: u32, gate: &Gate<'_>) -> bool {
    body.blocks[block]
        .stmts
        .clone()
        .iter()
        .all(|&s| stmt_readonly(body, s, idx, gate))
}

fn stmt_readonly(body: &Body, stmt: StmtId, idx: u32, gate: &Gate<'_>) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. } => expr_readonly_operand(body, *value, idx, gate),
        StmtKind::Expr(e) => e
            .as_expr()
            .is_none_or(|e| expr_readonly(body, e, idx, gate)),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            value.is_none_or(|v| expr_readonly_operand(body, v, idx, gate))
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            expr_readonly_operand(body, condition, idx, gate)
                && block_readonly(body, then_block, idx, gate)
                && else_block.is_none_or(|eb| block_readonly(body, eb, idx, gate))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            block_readonly(body, *b, idx, gate)
        }
        StmtKind::LetDestructure { value, .. } => expr_readonly_operand(body, *value, idx, gate),
        StmtKind::Continue => true,
    }
}

fn expr_readonly_operand(body: &Body, op: Operand, idx: u32, gate: &Gate<'_>) -> bool {
    op.as_expr()
        .is_none_or(|e| expr_readonly(body, e, idx, gate))
}

fn expr_readonly(body: &Body, expr: ExprId, idx: u32, gate: &Gate<'_>) -> bool {
    match &body.exprs[expr].kind {
        // A bare whole-value read not intercepted by a borrowing parent is a
        // consuming use. Reject.
        ExprKind::Local { index, .. } => *index != idx,

        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            let receiver = *receiver;
            let callee_id = *func_id;
            let args: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
            let recv = receiver.as_expr().map(|e| strip_refs(body, e));
            if recv.is_some_and(|r| is_local(body, r, idx)) {
                if gate.callee_mutates_self(callee_id) != Some(false) {
                    return false;
                }
            } else if receiver
                .as_expr()
                .is_some_and(|e| expr_mentions_local(body, e, idx))
            {
                if gate.callee_mutates_self(callee_id) != Some(false) {
                    return false;
                }
                if !expr_readonly_operand(body, receiver, idx, gate) {
                    return false;
                }
            } else if !expr_readonly_operand(body, receiver, idx, gate) {
                return false;
            }
            args.iter().all(|&a| call_arg_readonly(body, a, idx, gate))
        }
        ExprKind::Call { args, .. } => {
            let args: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
            args.iter().all(|&a| call_arg_readonly(body, a, idx, gate))
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let args = args.clone();
            expr_readonly_operand(body, callee, idx, gate)
                && args
                    .iter()
                    .all(|&a| call_arg_readonly_operand(body, a, idx, gate))
        }

        // `&mut <…xs…>` — a mutable reference into the binding escapes.
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => !inner
            .as_expr()
            .is_some_and(|e| expr_mentions_local(body, e, idx)),

        // Pure scalar reads.
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            expr_readonly_operand(body, left, idx, gate)
                && expr_readonly_operand(body, right, idx, gate)
        }
        ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary {
            op: NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot,
            expr: inner,
        } => expr_readonly_operand(body, *inner, idx, gate),

        // Reads through projections.
        ExprKind::Index { expr: base, index } => {
            let (base, index) = (*base, *index);
            (base.as_expr().is_some_and(|e| is_local(body, e, idx))
                || expr_readonly_operand(body, base, idx, gate))
                && expr_readonly_operand(body, index, idx, gate)
        }
        ExprKind::FieldAccess { expr: base, .. } => {
            let base = *base;
            base.as_expr().is_some_and(|e| is_local(body, e, idx))
                || expr_readonly_operand(body, base, idx, gate)
        }

        // A write whose target touches the binding escapes.
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            !expr_mentions_local(body, target, idx) && expr_readonly_operand(body, value, idx, gate)
        }

        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            block_readonly(body, *b, idx, gate)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            expr_readonly_operand(body, condition, idx, gate)
                && block_readonly(body, then_branch, idx, gate)
                && else_branch.is_none_or(|eb| block_readonly(body, eb, idx, gate))
        }
        ExprKind::Match { expr: scrut, arms } => {
            let scrut = *scrut;
            let arms: Vec<(Option<Operand>, Operand)> =
                arms.iter().map(|a| (a.guard, a.body)).collect();
            expr_readonly_operand(body, scrut, idx, gate)
                && arms.iter().all(|(guard, arm_body)| {
                    guard.is_none_or(|g| expr_readonly_operand(body, g, idx, gate))
                        && expr_readonly_operand(body, *arm_body, idx, gate)
                })
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            expr_readonly_operand(body, scrutinee, idx, gate)
                && arms.iter().all(|&a| block_readonly(body, a, idx, gate))
                && block_readonly(body, default, idx, gate)
        }

        // Any other expression kind: a non-whitelisted use. Reject if it
        // mentions the binding.
        _ => !expr_mentions_local(body, expr, idx),
    }
}

fn call_arg_readonly_operand(body: &Body, op: Operand, idx: u32, gate: &Gate<'_>) -> bool {
    op.as_expr()
        .is_none_or(|e| call_arg_readonly(body, e, idx, gate))
}

/// A binding handed to a call as an argument. `&` borrow is a read; `&mut`
/// escapes; passing the binding itself by value is a consuming use (rejected).
fn call_arg_readonly(body: &Body, arg: ExprId, idx: u32, gate: &Gate<'_>) -> bool {
    match &body.exprs[arg].kind {
        ExprKind::Local { index, .. } => *index != idx,
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => !inner
            .as_expr()
            .is_some_and(|e| expr_mentions_local(body, e, idx)),
        ExprKind::Unary {
            op: NirUnaryOp::Ref,
            expr: inner,
        } => {
            let inner = *inner;
            inner.as_expr().is_some_and(|e| is_local(body, e, idx))
                || expr_readonly_operand(body, inner, idx, gate)
        }
        _ => expr_readonly(body, arg, idx, gate),
    }
}

// ---------------------------------------------------------------------------
// Phase 2 — mutation
// ---------------------------------------------------------------------------

/// Rewrite every `Local(local_index)` read into a `GlobalVarGet`.
fn rewrite_reads(
    body: &mut Body,
    local_index: u32,
    module_source: &ModuleSource,
    name: &str,
    ty: TypeId,
) {
    let mut targets = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && matches!(&body.exprs[id].kind, ExprKind::Local { index, .. } if *index == local_index)
        {
            targets.push(id);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    for id in targets {
        body.exprs[id].kind = ExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.to_string(),
        };
        body.exprs[id].type_id = ty;
    }
}

/// Replace the `let local_index = value` statement with an inline
/// `GlobalVarSet(name, value)`, searching the whole body exhaustively —
/// matching [`collect_candidates`]'s reach, so this always finds whatever it
/// collected. Returns `false` if not found; the caller asserts on this,
/// since a miss would leave the local's reads pointing at a global that's
/// never set.
fn replace_let_with_set(
    body: &mut Body,
    local_index: u32,
    module_source: &ModuleSource,
    name: &str,
) -> bool {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Let {
                local_index: li,
                value,
                ..
            } = &body.stmts[s].kind
            && *li == local_index
        {
            let value = *value;
            let span = body.stmts[s].span;
            let set = body.exprs.push(ExprNode {
                kind: ExprKind::GlobalVarSet {
                    module_source: module_source.clone(),
                    name: name.to_string(),
                    value,
                },
                type_id: TypeTable::UNIT,
                span,
            });
            body.stmts[s].kind = StmtKind::Expr(set.into());
            return true;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}
