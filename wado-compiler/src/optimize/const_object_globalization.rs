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

use cranelift_entity::EntityRef;
use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
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
    /// Initializer contains a call, so it can never become a Wasm constant
    /// and `wir_optimize::const_global` cannot delete the assignment. Such a
    /// candidate needs the lazy-init guard; a literal one keeps the existing
    /// unguarded shape so its eager promotion is unchanged.
    guarded: bool,
}

enum CandidateKind {
    /// A `let` binding whose value is a closed constant aggregate. Sibling
    /// const bindings the value reads (`sibling_lets`, in source order) are
    /// moved into the hoisted initializer block at mutation time, so the
    /// global's set value stays self-contained (eager-promotable, dedupable)
    /// exactly like the pre-normal-form builder-temp block.
    LetBinding {
        local_index: u32,
        sibling_lets: Vec<StmtId>,
    },
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
    let fn_effects =
        super::mod_ref::compute_fn_effects(&project.functions, &project.builtin_registry);
    let hoistable_pure: Vec<bool> = project
        .functions
        .iter()
        .zip(&fn_effects)
        .map(|(f, e)| e.is_pure() && is_hoistable_shape(&f.borrow()))
        .collect();
    let gate = Gate {
        funcs: &project.functions,
        type_table: &type_table,
        hoistable_pure: &hoistable_pure,
        structs: &project.structs,
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
            guarded,
        } = cand;
        let is_inline_ref = matches!(kind, CandidateKind::InlineRef { .. });

        let mut func = project.functions[func_idx].borrow_mut();
        let body = func.body.as_mut().expect("candidate function has a body");
        match kind {
            CandidateKind::LetBinding {
                local_index,
                sibling_lets,
            } => {
                // Rewrite reads first (the let's own value is const and
                // references no local index, so it is untouched), then
                // replace the binding.
                rewrite_reads(body, local_index, &module_source, &name, ty);
                assert!(
                    replace_let_with_set(body, local_index, &module_source, &name, guarded),
                    "[NIR] const_object_globalization: LetBinding candidate's `let` \
                     (local {local_index}) went missing between collection and mutation"
                );
                inline_sibling_lets(body, local_index, &sibling_lets, &module_source, &name);
            }
            CandidateKind::InlineRef { ref_expr } => {
                hoist_inline_ref(body, ref_expr, &module_source, &name, ty, guarded);
            }
        }
        drop(func);

        if guarded {
            project.globals.push(NirGlobal {
                name: guard_flag_name(&name),
                ty: TypeTable::BOOL,
                initializer: ExprBody::wrapping_value(
                    crate::nir_value_graph::ValueKind::Bool(false),
                    TypeTable::BOOL,
                    crate::token::Span::new(0, 0, 1, 1),
                ),
                mutable: true,
                wado_mutable: true,
                visibility: crate::ast::Visibility::Private,
                module_source: module_source.clone(),
                span: crate::token::Span::new(0, 0, 1, 1),
                is_nullable: false,
                lazy_init: false,
                locals: Vec::new(),
                prefer_fixed_string_repr: false,
            });
        }
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
    let siblings = sibling_const_locals(body, gate, &single_decl_locals);
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Let {
                local_index,
                type_id,
                ..
            } = &body.stmts[s].kind
            && single_decl_locals.contains(local_index)
            && let Some(sibling_lets) = let_stmt_qualifies(body, s, gate, &siblings)
        {
            out.push(Candidate {
                func_idx,
                ty: *type_id,
                module_source: module_source.clone(),
                kind: CandidateKind::LetBinding {
                    local_index: *local_index,
                    sibling_lets,
                },
                guarded: stmt_value_contains_call(body, s),
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
                && is_globalizable_const(body, inner, gate, &mut IndexSet::default())
                && contains_aggregate(body, inner, gate)
            {
                out.push(Candidate {
                    func_idx,
                    ty: inner_ty,
                    module_source: module_source.clone(),
                    kind: CandidateKind::InlineRef { ref_expr: id },
                    guarded: expr_contains_call(body, inner),
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
    guarded: bool,
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
    let set_stmt = if guarded {
        guard_set_on_flag(body, set_expr, module_source, &guard_flag_name(name), span)
    } else {
        body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(set_expr)),
            span,
        })
    };
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
fn let_stmt_qualifies(
    body: &Body,
    stmt: StmtId,
    gate: &Gate<'_>,
    siblings: &SiblingConsts,
) -> Option<Vec<StmtId>> {
    let StmtKind::Let {
        local_index,
        value,
        type_id,
        ..
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    let (local_index, value, type_id) = (*local_index, *value, *type_id);
    if !gate.is_reference_type(type_id)
        || !is_globalizable_const_operand(body, value, gate, &mut siblings.set.clone())
        || !is_readonly_body(body, local_index, gate)
    {
        return None;
    }
    // A sibling-const read (the flattened builder-temp pair `let mut __b =
    // <literal>; let xs = *__b`) qualifies only when every read of the sibling
    // sits inside this initializer: the hoisted `GlobalVarSet` still evaluates
    // at the binding's flow position, and confinement rules out another alias
    // mutating the shared object behind the global. One whole-body and one
    // in-`value` tally (not one per sibling) settle it.
    let used_siblings = seeded_locals_read(body, value, siblings);
    if !used_siblings.is_empty() {
        let body_reads = count_reads_of(body, NodeRef::Block(body.root), &used_siblings);
        let value_reads = value
            .as_expr()
            .map(|e| count_reads_of(body, NodeRef::Expr(e), &used_siblings))
            .unwrap_or_default();
        for &l in &used_siblings {
            if body_reads.get(&l).copied().unwrap_or(0) != value_reads.get(&l).copied().unwrap_or(0)
            {
                return None;
            }
        }
    }
    let has_aggregate = contains_aggregate_operand(body, value, gate)
        || used_siblings.iter().any(|l| {
            siblings
                .defs
                .get(l)
                .is_some_and(|&d| contains_aggregate_operand(body, d, gate))
        });
    if !has_aggregate {
        return None;
    }
    // Source order: the moved defs must precede the tail in the initializer
    // block exactly as they did in the function.
    let mut lets: Vec<StmtId> = used_siblings
        .iter()
        .filter_map(|l| siblings.let_stmts.get(l).copied())
        .collect();
    lets.sort_by_key(|s| s.index());
    Some(lets)
}

/// Sibling bindings a candidate initializer may read: declared once, never
/// mutated (assigned, `&mut`-borrowed, `mut`-argument, or mutating-method
/// receiver), reference-typed, and bound to a closed constant aggregate.
/// Seeded to a fixpoint so a sibling may itself read an earlier sibling.
#[derive(Default)]
struct SiblingConsts {
    set: IndexSet<u32>,
    defs: IndexMap<u32, Operand>,
    let_stmts: IndexMap<u32, StmtId>,
}

fn sibling_const_locals(
    body: &Body,
    gate: &Gate<'_>,
    declared_once: &IndexSet<u32>,
) -> SiblingConsts {
    let mutated = mutated_locals(body, gate);
    let mut sc = SiblingConsts::default();
    loop {
        let mut changed = false;
        let mut stack = vec![NodeRef::Block(body.root)];
        while let Some(node) = stack.pop() {
            if let NodeRef::Stmt(s) = node
                && let StmtKind::Let {
                    local_index,
                    value,
                    type_id,
                    ..
                } = &body.stmts[s].kind
                && declared_once.contains(local_index)
                && !mutated.contains(local_index)
                && !sc.set.contains(local_index)
                && gate.is_reference_type(*type_id)
                && is_globalizable_const_operand(body, *value, gate, &mut sc.set.clone())
            {
                sc.set.insert(*local_index);
                sc.defs.insert(*local_index, *value);
                sc.let_stmts.insert(*local_index, s);
                changed = true;
            }
            body.for_each_child(node, |c| stack.push(c));
        }
        if !changed {
            break;
        }
    }
    sc
}

/// Locals that may be mutated anywhere in `body`: an `Assign` target root, a
/// `&mut` borrow root, a `mut` call argument root, or the receiver root of a
/// method not proven non-mutating. The complement is safe to share behind an
/// immutable global.
fn mutated_locals(body: &Body, gate: &Gate<'_>) -> IndexSet<u32> {
    let root_of = |e: ExprId| -> Option<u32> {
        let mut cur = e;
        loop {
            match &body.exprs[cur].kind {
                ExprKind::Local { index, .. } => return Some(*index),
                ExprKind::FieldAccess { expr: inner, .. }
                | ExprKind::Index { expr: inner, .. }
                | ExprKind::Unary { expr: inner, .. }
                | ExprKind::Cast { expr: inner, .. } => cur = inner.as_expr()?,
                _ => return None,
            }
        }
    };
    let mut out: IndexSet<u32> = IndexSet::default();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            match &body.exprs[id].kind {
                ExprKind::Assign { target, .. } => {
                    if let Some(r) = root_of(*target) {
                        out.insert(r);
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: inner,
                } => {
                    if let Some(r) = inner.as_expr().and_then(root_of) {
                        out.insert(r);
                    }
                }
                ExprKind::MethodCall {
                    receiver,
                    func_id,
                    args,
                    ..
                } => {
                    if gate.callee_mutates_self(*func_id) != Some(false)
                        && let Some(r) = receiver.as_expr().and_then(root_of)
                    {
                        out.insert(r);
                    }
                    for a in args {
                        if a.is_mut
                            && let Some(r) = a.expr.as_expr().and_then(root_of)
                        {
                            out.insert(r);
                        }
                    }
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        if a.is_mut
                            && let Some(r) = a.expr.as_expr().and_then(root_of)
                        {
                            out.insert(r);
                        }
                    }
                }
                _ => {}
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

/// The sibling-const locals read anywhere inside `value`.
fn seeded_locals_read(body: &Body, value: Operand, siblings: &SiblingConsts) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    let Some(e) = value.as_expr() else {
        return out;
    };
    let mut stack = vec![NodeRef::Expr(e)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Local { index, .. } = &body.exprs[id].kind
            && siblings.set.contains(index)
        {
            out.insert(*index);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

/// Tally reads of each local in `wanted` under `node`, in a single walk.
fn count_reads_of(body: &Body, node: NodeRef, wanted: &IndexSet<u32>) -> IndexMap<u32, usize> {
    let mut counts: IndexMap<u32, usize> = IndexMap::default();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Local { index, .. } = &body.exprs[id].kind
            && wanted.contains(index)
        {
            *counts.entry(*index).or_default() += 1;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    counts
}

fn is_globalizable_const_operand(
    body: &Body,
    op: Operand,
    gate: &Gate<'_>,
    bound: &mut IndexSet<u32>,
) -> bool {
    match op {
        // A promoted operand is a closed constant only if its value graph node is
        // a constant literal. `promote_pure_values_early` (and the born-as-operands
        // path) can freeze a runtime-dependent pure value — e.g. `depth * 2` over a
        // recursion parameter — into an `Operand::Value`; hoisting such a `let`
        // into a shared global would re-initialize it per activation, so an outer
        // recursive frame would observe an inner frame's value. Only a genuine
        // constant is safe.
        Operand::Value(v) => crate::nir_value_graph::builder::is_const_value(&body.values, v),
        Operand::Expr(e) => is_globalizable_const(body, e, gate, bound),
    }
}

/// Recursively true when `expr` is a closed constant aggregate value.
fn is_globalizable_const(
    body: &Body,
    expr: ExprId,
    gate: &Gate<'_>,
    bound: &mut IndexSet<u32>,
) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::EnumConstruct { .. } => true,
        // A packed `Array<u8>` (a `String` / `List<u8>` literal's `repr`) is a
        // closed constant with no free locals.
        ExprKind::PackedArray(_) => true,
        ExprKind::Local { index, .. } => bound.contains(index),
        ExprKind::StructLiteral { fields, .. } => {
            // Each field's operand must itself be a closed constant. A promoted
            // `Operand::Value` field must be checked (not skipped): `filter_map`
            // over `as_expr` would silently drop a runtime-dependent promoted
            // field, wrongly classifying the aggregate as const.
            let field_ops: Vec<Operand> = fields.iter().map(|f| f.value).collect();
            field_ops
                .iter()
                .all(|&op| is_globalizable_const_operand(body, op, gate, bound))
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            elements
                .iter()
                .all(|&e| is_globalizable_const_operand(body, e, gate, bound))
        }
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_none_or(|p| is_globalizable_const_operand(body, p, gate, bound))
        }
        // A pure call on closed constants is itself a closed constant
        // expression: same arguments, same result, and collapsing repeats is
        // unobservable. `FnEffect` (see `mod_ref`) is what establishes that.
        ExprKind::Call { func_id, args, .. } if gate.is_hoistable_pure(*func_id) => {
            let arg_ops: Vec<Operand> = args.iter().map(|a| a.expr).collect();
            arg_ops
                .iter()
                .all(|&op| is_globalizable_const_operand(body, op, gate, bound))
        }
        // Transparent value wrappers.
        ExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::Neg,
            expr: inner,
        }
        | ExprKind::Cast { expr: inner, .. } => {
            is_globalizable_const_operand(body, *inner, gate, bound)
        }
        // The builder-temp block an array / list literal leaves.
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            block_is_const(body, *block, gate, bound)
        }
        _ => false,
    }
}

fn block_is_const(
    body: &Body,
    block: BlockId,
    gate: &Gate<'_>,
    bound: &mut IndexSet<u32>,
) -> bool {
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
        if !is_globalizable_const_operand(body, value, gate, bound) {
            return false;
        }
        bound.insert(local_index);
    }
    match &body.stmts[last].kind {
        StmtKind::Expr(e) => e
            .as_expr()
            .is_some_and(|e| is_globalizable_const(body, e, gate, bound)),
        _ => false,
    }
}

fn contains_aggregate_operand(body: &Body, op: Operand, gate: &Gate<'_>) -> bool {
    op.as_expr().is_some_and(|e| contains_aggregate(body, e, gate))
}

/// True when `expr` contains at least one aggregate constructor.
///
/// The gate exists to skip scalars, which are cheaper to rematerialize than to
/// load from a global. A hoistable pure call returning a reference type builds
/// a heap object just as a literal constructor does — the constructor is simply
/// in the callee — so it counts too.
fn contains_aggregate(body: &Body, expr: ExprId, gate: &Gate<'_>) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::Call { func_id, .. }
            if gate.is_hoistable_pure(*func_id)
                && gate.is_reference_type(body.exprs[expr].type_id)
                && gate.owns_heap_storage(body.exprs[expr].type_id) =>
        {
            true
        }
        ExprKind::StructLiteral { .. }
        | ExprKind::TupleLiteral { .. }
        | ExprKind::ArrayLiteral { .. }
        | ExprKind::VariantConstruct { .. } => true,
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            contains_aggregate_operand(body, *inner, gate)
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            let stmts = body.blocks[*block].stmts.clone();
            stmts.iter().any(|&s| match &body.stmts[s].kind {
                StmtKind::Let { value, .. } => contains_aggregate_operand(body, *value, gate),
                StmtKind::Expr(value) => {
                    value.as_expr().is_some_and(|e| contains_aggregate(body, e, gate))
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
    /// Indexed by `func_id.index()`. See [`is_hoistable_shape`].
    hoistable_pure: &'a [bool],
    structs: &'a [crate::nir::NirStruct],
}

/// Shape preconditions a callee must meet on top of `FnEffect::is_pure`.
///
/// Deliberately not [`crate::niri::is_ctfe_eligible`]: that predicate also
/// rejects `#[inline(never)]`, which is an inlining *policy*, not a statement
/// about determinism. Hoisting does not inline, so the marker is irrelevant
/// here — and honouring it would silently exempt exactly the functions a
/// caller marked "keep this an out-of-line call".
fn is_hoistable_shape(f: &NirFunction) -> bool {
    f.body.is_some()
        && f.task_return_type.is_none()
        && !f.is_cm_binding
        && !f.is_cm_export
        && !f.is_dispatch_wrapper
        && f.type_params.is_empty()
        && f.impl_type_params.is_empty()
}

impl Gate<'_> {
    fn is_reference_type(&self, ty: TypeId) -> bool {
        !matches!(
            self.type_table.borrow().get(ty),
            ResolvedType::Primitive(_) | ResolvedType::Unit | ResolvedType::Never
        )
    }

    /// Whether a value of `ty` owns heap storage worth building only once.
    ///
    /// Hoisting is not free: it costs a global, an init flag, a guard branch,
    /// and an object that stays live for the whole program. That only pays
    /// when rebuilding the value would re-allocate — i.e. when it (transitively)
    /// owns a GC array, as `String` and `List` do.
    ///
    /// A small aggregate of scalars owns nothing: `multi_value_return` already
    /// lifts such a return into Wasm multi-values and allocates *nothing*, so
    /// hoisting it is strictly worse. This is the existing "skip scalars"
    /// rationale one level up — skip whatever the backend can keep in registers.
    fn owns_heap_storage(&self, ty: TypeId) -> bool {
        let mut seen = IndexSet::default();
        self.owns_heap_storage_inner(ty, &mut seen)
    }

    fn owns_heap_storage_inner(&self, ty: TypeId, seen: &mut IndexSet<TypeId>) -> bool {
        if !seen.insert(ty) {
            return false;
        }
        let tt = self.type_table.borrow();
        let inner = match tt.get(ty) {
            ResolvedType::BuiltinArray(_) => return true,
            // Nothing to own.
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::Function { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => return false,
            // Transparent wrappers.
            ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Newtype { base_type: inner, .. }
            | ResolvedType::Reactive(inner) => Some(*inner),
            // Struct / tuple: decided by the fields below. Anything else
            // (a variant and its payloads, a generic instance) cannot be
            // walked here, so assume it owns storage — `Option<String>` and
            // friends live there.
            _ => None,
        };
        if let Some(inner) = inner {
            drop(tt);
            return self.owns_heap_storage_inner(inner, seen);
        }
        let fields = super::multi_value_return::aggregate_field_info(ty, &tt, self.structs);
        drop(tt);
        match fields {
            Some((field_types, _, _)) => field_types
                .into_iter()
                .any(|f| self.owns_heap_storage_inner(f, seen)),
            None => true,
        }
    }

    fn is_hoistable_pure(&self, func_id: crate::nir::FuncId) -> bool {
        self.hoistable_pure
            .get(func_id.index())
            .copied()
            .unwrap_or(false)
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

/// Move a candidate's sibling const `let`s into the hoisted set's value,
/// rebuilding the self-contained initializer block the pre-normal-form shape
/// carried: `G = *__b` (sibling `__b` outside) becomes
/// `G = { let __b = <literal>; *__b }`. Confinement (checked at candidacy)
/// guarantees the moved bindings have no other readers.
fn inline_sibling_lets(
    body: &mut Body,
    local_index: u32,
    sibling_lets: &[StmtId],
    module_source: &ModuleSource,
    name: &str,
) {
    if sibling_lets.is_empty() {
        return;
    }
    // Detach the sibling stmts from every block they appear in. A full scan is
    // load-bearing: an early exit that stops after N removals can leave a
    // sibling behind in a later block, which then double-lists once the new
    // initializer block also claims it.
    let sibling_set: IndexSet<StmtId> = sibling_lets.iter().copied().collect();
    for bid in body.blocks.keys().collect::<Vec<_>>() {
        let stmts = &mut body.blocks[bid].stmts;
        if stmts.iter().any(|s| sibling_set.contains(s)) {
            stmts.retain(|s| !sibling_set.contains(s));
        }
    }
    // Find the freshly planted `GlobalVarSet` and wrap its value.
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let ExprKind::GlobalVarSet {
                name: n,
                value,
                module_source: ms,
            } = &body.exprs[e].kind
            && n == name
            && ms == module_source
        {
            let value = *value;
            let span = body.exprs[e].span;
            let value_ty = match value {
                Operand::Expr(ve) => body.exprs[ve].type_id,
                Operand::Value(_) => TypeTable::UNIT,
            };
            let tail = body.stmts.push(StmtNode {
                kind: StmtKind::Expr(value),
                span,
            });
            let mut stmts: Vec<StmtId> = sibling_lets.to_vec();
            stmts.push(tail);
            let block = body.blocks.push(BlockNode { stmts, span });
            let block_expr = body.exprs.push(ExprNode {
                kind: ExprKind::Block(block),
                type_id: value_ty,
                span,
            });
            let ExprKind::GlobalVarSet { value, .. } = &mut body.exprs[e].kind else {
                unreachable!("matched above");
            };
            *value = Operand::Expr(block_expr);
            return;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    panic!(
        "[NIR] const_object_globalization: GlobalVarSet for {name} (local {local_index}) \
         went missing between planting and sibling inlining"
    );
}

/// Replace the `let local_index = value` statement with an inline
/// `GlobalVarSet(name, value)`, searching the whole body exhaustively —
/// matching [`collect_candidates`]'s reach, so this always finds whatever it
/// collected. Returns `false` if not found; the caller asserts on this,
/// since a miss would leave the local's reads pointing at a global that's
/// never set.


/// True when `expr` contains a call anywhere — the marker for an initializer
/// that cannot reduce to a Wasm constant instruction.
fn expr_contains_call(body: &Body, expr: ExprId) -> bool {
    let mut stack = vec![NodeRef::Expr(expr)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && matches!(
                body.exprs[id].kind,
                ExprKind::Call { .. }
                    | ExprKind::MethodCall { .. }
                    | ExprKind::IndirectCall { .. }
                    | ExprKind::CmRawCall { .. }
            )
        {
            return true;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}

fn stmt_value_contains_call(body: &Body, stmt: StmtId) -> bool {
    let StmtKind::Let { value, .. } = &body.stmts[stmt].kind else {
        return false;
    };
    value.as_expr().is_some_and(|e| expr_contains_call(body, e))
}

/// Wrap a hoisted `GlobalVarSet` in `if !<flag> { …; <flag> = true }`.
///
/// The unguarded form is correct only because `wir_optimize::const_global`
/// promotes a Wasm-const-expressible initializer into the global's eager
/// `init` and deletes the assignment. A call is never const-expressible, so
/// its assignment survives — and an unguarded one re-runs on every activation,
/// which is the opposite of hoisting.
///
/// The guard also pins the semantics: initialization happens at the first
/// execution of the expression it replaced, so a callee that traps or diverges
/// still does so, at the same point. Moving the work to module-init instead
/// would drag both to instantiation time.
///
/// The flag is a separate `bool` global rather than a null test on the value
/// slot itself: `lazy_init` promises codegen that every read of the slot is
/// post-initialization, so it narrows reads with `ref.as_non_null` — and the
/// guard's own read is by construction the one that precedes it. Testing a
/// flag keeps that promise intact. Same shape as `__modules_initialized`.
fn guard_set_on_flag(
    body: &mut Body,
    set: ExprId,
    module_source: &ModuleSource,
    flag_name: &str,
    span: crate::token::Span,
) -> StmtId {
    let flag_get = body.exprs.push(ExprNode {
        kind: ExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: flag_name.to_string(),
        },
        type_id: TypeTable::BOOL,
        span,
    });
    let cond = body.exprs.push(ExprNode {
        kind: ExprKind::Unary {
            op: NirUnaryOp::Not,
            expr: flag_get.into(),
        },
        type_id: TypeTable::BOOL,
        span,
    });
    let truth = body.values.bool(true);
    body.values.set_type(truth, TypeTable::BOOL);
    let flag_set = body.exprs.push(ExprNode {
        kind: ExprKind::GlobalVarSet {
            module_source: module_source.clone(),
            name: flag_name.to_string(),
            value: Operand::Value(truth),
        },
        type_id: TypeTable::UNIT,
        span,
    });
    let set_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(set.into()),
        span,
    });
    let flag_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(flag_set.into()),
        span,
    });
    let then_block = body.blocks.push(BlockNode {
        stmts: vec![set_stmt, flag_stmt],
        span,
    });
    body.stmts.push(StmtNode {
        kind: StmtKind::If {
            condition: cond.into(),
            then_block,
            else_block: None,
        },
        span,
    })
}

/// Name of the init flag paired with a guarded `__const_obj_*` global.
fn guard_flag_name(global_name: &str) -> String {
    format!("{global_name}__init")
}

fn replace_let_with_set(
    body: &mut Body,
    local_index: u32,
    module_source: &ModuleSource,
    name: &str,
    guarded: bool,
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
            if guarded {
                let guard =
                    guard_set_on_flag(body, set, module_source, &guard_flag_name(name), span);
                body.stmts[s].kind = body.stmts[guard].kind.clone();
            } else {
                body.stmts[s].kind = StmtKind::Expr(set.into());
            }
            return true;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}
