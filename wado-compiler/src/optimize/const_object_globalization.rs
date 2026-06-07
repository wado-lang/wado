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

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirFunction, NirGlobal, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprBody, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::arena_query::{expr_mentions_local, is_local, strip_refs};

type FuncKey = (ModuleSource, String);

/// A `let` binding selected for hoisting, identified by its owning function and
/// the bound local index. Resolved in an immutable analysis phase, applied in a
/// later mutation phase to avoid `RefCell` borrow conflicts.
struct Candidate {
    func_idx: usize,
    local_index: u32,
    ty: TypeId,
    module_source: ModuleSource,
}

pub fn globalize_const_objects(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.clone();
    let mut by_key: IndexMap<FuncKey, usize> = IndexMap::default();
    for (i, f) in project.functions.iter().enumerate() {
        let f = f.borrow();
        by_key.insert((f.module_source.clone(), f.name.clone()), i);
    }

    // Phase 1 — analysis (all immutable borrows).
    let gate = Gate {
        funcs: &project.functions,
        by_key: &by_key,
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
        collect_candidates(
            body,
            body.root,
            &gate,
            fi,
            &f.module_source,
            &mut candidates,
        );
    }
    if candidates.is_empty() {
        return false;
    }

    // Phase 2 — mutation. Number from the count of pre-existing `__const_obj_*`
    // globals so names stay unique across invocations.
    let base = project
        .globals
        .iter()
        .filter(|g| g.name.starts_with("__const_obj_"))
        .count();
    for (n, cand) in (base..).zip(candidates) {
        let name = format!("__const_obj_{n}");

        let mut func = project.functions[cand.func_idx].borrow_mut();
        let body = func.body.as_mut().expect("candidate function has a body");
        // Rewrite reads first (the let's own value is const and references no
        // local index, so it is untouched), then replace the binding.
        rewrite_reads(body, cand.local_index, &cand.module_source, &name, cand.ty);
        replace_let_with_set(
            body,
            body.root,
            cand.local_index,
            &cand.module_source,
            &name,
        );
        drop(func);

        project.globals.push(NirGlobal {
            name,
            ty: cand.ty,
            initializer: ExprBody::wrapping(
                ExprKind::Null,
                cand.ty,
                crate::token::Span::new(0, 0, 1, 1),
            ),
            mutable: true,
            wado_mutable: false,
            is_pub: false,
            module_source: cand.module_source,
            span: crate::token::Span::new(0, 0, 1, 1),
            is_nullable: true,
            lazy_init: true,
            locals: Vec::new(),
        });
    }
    true
}

/// Skip synthesized init / CM-binding functions.
fn skip_function(f: &NirFunction) -> bool {
    f.is_cm_binding
        || f.is_dispatch_wrapper
        || f.name.starts_with("__initialize")
        || f.value_copy_type().is_some()
}

// ---------------------------------------------------------------------------
// Phase 1 — candidate collection
// ---------------------------------------------------------------------------

fn collect_candidates(
    body: &Body,
    block: BlockId,
    gate: &Gate<'_>,
    func_idx: usize,
    module_source: &ModuleSource,
    out: &mut Vec<Candidate>,
) {
    for &stmt in &body.blocks[block].stmts {
        if let StmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } = &body.stmts[stmt].kind
        {
            let (local_index, value, type_id) = (*local_index, *value, *type_id);
            if gate.is_reference_type(type_id)
                && is_globalizable_const(body, value, &mut IndexSet::default())
                && contains_aggregate(body, value)
                && is_readonly_body(body, local_index, gate)
            {
                out.push(Candidate {
                    func_idx,
                    local_index,
                    ty: type_id,
                    module_source: module_source.clone(),
                });
            }
        }
        // Recurse into nested scopes.
        for inner in stmt_blocks(body, stmt) {
            collect_candidates(body, inner, gate, func_idx, module_source, out);
        }
    }
}

/// The sub-blocks a statement owns (for candidate recursion).
fn stmt_blocks(body: &Body, stmt: StmtId) -> Vec<BlockId> {
    match &body.stmts[stmt].kind {
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut v = vec![*then_block];
            if let Some(eb) = else_block {
                v.push(*eb);
            }
            v
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => vec![*b],
        _ => Vec::new(),
    }
}

/// Recursively true when `expr` is a closed constant aggregate value.
fn is_globalizable_const(body: &Body, expr: ExprId, bound: &mut IndexSet<u32>) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::Null
        | ExprKind::Unit
        | ExprKind::EnumConstruct { .. } => true,
        ExprKind::Local { index, .. } => bound.contains(index),
        ExprKind::StructLiteral { fields, .. } => {
            let fields: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            fields
                .iter()
                .all(|&v| is_globalizable_const(body, v, bound))
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            elements
                .iter()
                .all(|&e| is_globalizable_const(body, e, bound))
        }
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_none_or(|p| is_globalizable_const(body, p, bound))
        }
        // Transparent value wrappers.
        ExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::Neg,
            expr: inner,
        }
        | ExprKind::Cast { expr: inner, .. } => is_globalizable_const(body, *inner, bound),
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
        if !is_globalizable_const(body, value, bound) {
            return false;
        }
        bound.insert(local_index);
    }
    match &body.stmts[last].kind {
        StmtKind::Expr(e) => is_globalizable_const(body, *e, bound),
        _ => false,
    }
}

/// True when `expr` contains at least one aggregate constructor.
fn contains_aggregate(body: &Body, expr: ExprId) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::StructLiteral { .. }
        | ExprKind::TupleLiteral { .. }
        | ExprKind::ArrayLiteral { .. }
        | ExprKind::VariantConstruct { .. } => true,
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            contains_aggregate(body, *inner)
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            let stmts = body.blocks[*block].stmts.clone();
            stmts.iter().any(|&s| match &body.stmts[s].kind {
                StmtKind::Let { value, .. } | StmtKind::Expr(value) => {
                    contains_aggregate(body, *value)
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
    by_key: &'a IndexMap<FuncKey, usize>,
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
    fn callee_mutates_self(&self, func: &FunctionRef) -> Option<bool> {
        let idx = *self
            .by_key
            .get(&(func.module_source.clone(), func.name.clone()))?;
        let f = self.funcs[idx].borrow();
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
        StmtKind::Let { value, .. } => expr_readonly(body, *value, idx, gate),
        StmtKind::Expr(e) => expr_readonly(body, *e, idx, gate),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            value.is_none_or(|v| expr_readonly(body, v, idx, gate))
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            expr_readonly(body, condition, idx, gate)
                && block_readonly(body, then_block, idx, gate)
                && else_block.is_none_or(|eb| block_readonly(body, eb, idx, gate))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            block_readonly(body, *b, idx, gate)
        }
        StmtKind::LetDestructure { value, .. } => expr_readonly(body, *value, idx, gate),
        StmtKind::Continue => true,
    }
}

fn expr_readonly(body: &Body, expr: ExprId, idx: u32, gate: &Gate<'_>) -> bool {
    match &body.exprs[expr].kind {
        // A bare whole-value read not intercepted by a borrowing parent is a
        // consuming use. Reject.
        ExprKind::Local { index, .. } => *index != idx,

        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let receiver = *receiver;
            let func = func.clone();
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            let recv = strip_refs(body, receiver);
            if is_local(body, recv, idx) {
                if gate.callee_mutates_self(&func) != Some(false) {
                    return false;
                }
            } else if expr_mentions_local(body, receiver, idx) {
                if gate.callee_mutates_self(&func) != Some(false) {
                    return false;
                }
                if !expr_readonly(body, receiver, idx, gate) {
                    return false;
                }
            } else if !expr_readonly(body, receiver, idx, gate) {
                return false;
            }
            args.iter().all(|&a| call_arg_readonly(body, a, idx, gate))
        }
        ExprKind::Call { args, .. } => {
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            args.iter().all(|&a| call_arg_readonly(body, a, idx, gate))
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let args = args.clone();
            expr_readonly(body, callee, idx, gate)
                && args.iter().all(|&a| call_arg_readonly(body, a, idx, gate))
        }

        // `&mut <…xs…>` — a mutable reference into the binding escapes.
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => !expr_mentions_local(body, *inner, idx),

        // Pure scalar reads.
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            expr_readonly(body, left, idx, gate) && expr_readonly(body, right, idx, gate)
        }
        ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary {
            op: NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot,
            expr: inner,
        } => expr_readonly(body, *inner, idx, gate),

        // Reads through projections.
        ExprKind::Index { expr: base, index } => {
            let (base, index) = (*base, *index);
            (is_local(body, base, idx) || expr_readonly(body, base, idx, gate))
                && expr_readonly(body, index, idx, gate)
        }
        ExprKind::FieldAccess { expr: base, .. } => {
            let base = *base;
            is_local(body, base, idx) || expr_readonly(body, base, idx, gate)
        }

        // A write whose target touches the binding escapes.
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            !expr_mentions_local(body, target, idx) && expr_readonly(body, value, idx, gate)
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
            expr_readonly(body, condition, idx, gate)
                && block_readonly(body, then_branch, idx, gate)
                && else_branch.is_none_or(|eb| block_readonly(body, eb, idx, gate))
        }
        ExprKind::Match { expr: scrut, arms } => {
            let scrut = *scrut;
            let arms: Vec<(Option<ExprId>, ExprId)> =
                arms.iter().map(|a| (a.guard, a.body)).collect();
            expr_readonly(body, scrut, idx, gate)
                && arms.iter().all(|(guard, arm_body)| {
                    guard.is_none_or(|g| expr_readonly(body, g, idx, gate))
                        && expr_readonly(body, *arm_body, idx, gate)
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
            expr_readonly(body, scrutinee, idx, gate)
                && arms.iter().all(|&a| block_readonly(body, a, idx, gate))
                && block_readonly(body, default, idx, gate)
        }

        // Any other expression kind: a non-whitelisted use. Reject if it
        // mentions the binding.
        _ => !expr_mentions_local(body, expr, idx),
    }
}

/// A binding handed to a call as an argument. `&` borrow is a read; `&mut`
/// escapes; passing the binding itself by value is a consuming use (rejected).
fn call_arg_readonly(body: &Body, arg: ExprId, idx: u32, gate: &Gate<'_>) -> bool {
    match &body.exprs[arg].kind {
        ExprKind::Local { index, .. } => *index != idx,
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => !expr_mentions_local(body, *inner, idx),
        ExprKind::Unary {
            op: NirUnaryOp::Ref,
            expr: inner,
        } => {
            let inner = *inner;
            is_local(body, inner, idx) || expr_readonly(body, inner, idx, gate)
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
/// `GlobalVarSet(name, value)`, searching nested scopes.
fn replace_let_with_set(
    body: &mut Body,
    block: BlockId,
    local_index: u32,
    module_source: &ModuleSource,
    name: &str,
) -> bool {
    let stmts = body.blocks[block].stmts.clone();
    for stmt in stmts {
        if let StmtKind::Let {
            local_index: li,
            value,
            ..
        } = &body.stmts[stmt].kind
        {
            if *li == local_index {
                let value = *value;
                let span = body.stmts[stmt].span;
                let set = body.exprs.push(crate::nir_arena::ExprNode {
                    kind: ExprKind::GlobalVarSet {
                        module_source: module_source.clone(),
                        name: name.to_string(),
                        value,
                    },
                    type_id: TypeTable::UNIT,
                    span,
                });
                body.stmts[stmt].kind = StmtKind::Expr(set);
                return true;
            }
        } else {
            for inner in stmt_blocks(body, stmt) {
                if replace_let_with_set(body, inner, local_index, module_source, name) {
                    return true;
                }
            }
        }
    }
    false
}
