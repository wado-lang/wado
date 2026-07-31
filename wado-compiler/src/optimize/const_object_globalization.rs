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
//! A constant handed to a call *by value* ([`CandidateKind::ValueArg`]) runs the
//! read-only gate over the *callee's* parameter instead
//! ([`Gate::callee_param_readonly`]): the value crosses into the callee
//! uncopied — the value-copy planner skipped the copy because the literal is
//! fresh — so the callee is the only party that could write the shared object.
//! Being read-only is not enough there: a by-value parameter is the callee's
//! own copy, so it may legitimately hand a *projection* of it back
//! (`return s.data`), which the return-convention fixpoint calls owned and the
//! caller then mutates. [`param_storage_escapes`] rules that out.
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

use super::arena_query::{collect_reads, expr_mentions_local, is_local, strip_refs};

/// A hoisting candidate, identified by its owning function. Resolved in an
/// immutable analysis phase, applied in a later mutation phase to avoid
/// `RefCell` borrow conflicts.
struct Candidate {
    func_idx: usize,
    ty: TypeId,
    module_source: ModuleSource,
    kind: CandidateKind,
    /// Initializer cannot become a Wasm constant — it contains a call, or a
    /// `PackedArray` past the eager repr bound whose WIR lowering is
    /// `array.new_data` — so `wir_optimize::const_global` cannot delete the
    /// assignment. Such a candidate needs the lazy-init guard; an
    /// eager-promotable one keeps the unguarded shape so its promotion is
    /// unchanged.
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
    /// A constant aggregate handed to a call *by value* — `append(name,
    /// b"application/json")`. The value-copy planner leaves such an argument
    /// uncopied because the literal is fresh, so hoisting it is sound only
    /// when the callee cannot write the value it receives; see
    /// [`Gate::callee_param_readonly`]. Hoisted in place like [`Self::InlineRef`],
    /// wrapping the argument itself instead of a `&`'s operand.
    ValueArg {
        /// The argument node whose value is hoisted.
        arg_expr: ExprId,
    },
}

impl CandidateKind {
    /// Whether the hoisted global gets `prefer_fixed_string_repr`: an in-place
    /// hoist keeps the value at its original call site, where a lazy
    /// `array.new_data` global would cost a guard branch per call, so WIR
    /// raises its eager bound to `INLINE_REF_EAGER_MAX_BYTES`. The in-place
    /// guard decisions and the global's marking at mutation time derive from
    /// this one answer.
    fn prefer_fixed_repr(&self) -> bool {
        match self {
            CandidateKind::InlineRef { .. } | CandidateKind::ValueArg { .. } => true,
            CandidateKind::LetBinding { .. } => false,
        }
    }
}

pub fn globalize_const_objects(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.clone();
    // One id serves every instantiation — the hoisted type rides the call node.
    let is_uninitialized = project.intern_extern(&crate::nir::FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "is_uninitialized".to_string(),
        monomorph_info: None,
        method_info: None,
    });

    // Phase 1 — analysis (all immutable borrows).
    let fn_effects =
        super::mod_ref::compute_fn_effects(&project.functions, &project.builtin_registry);
    let hoistable_pure: Vec<bool> = project
        .functions
        .iter()
        .zip(&fn_effects)
        .map(|(f, e)| e.is_pure() && crate::niri::is_ctfe_eligible(&f.borrow()))
        .collect();
    let gate = Gate {
        funcs: &project.functions,
        type_table: &type_table,
        hoistable_pure: &hoistable_pure,
        structs: &project.structs,
        param_readonly: RefCell::new(IndexMap::default()),
        string_inline_max_bytes: project.string_inline_max_bytes,
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
        let prefer_fixed_repr = kind.prefer_fixed_repr();

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
                    replace_let_with_set(
                        body,
                        local_index,
                        &module_source,
                        &name,
                        ty,
                        guarded.then_some(is_uninitialized)
                    ),
                    "[NIR] const_object_globalization: LetBinding candidate's `let` \
                     (local {local_index}) went missing between collection and mutation"
                );
                inline_sibling_lets(body, local_index, &sibling_lets, &module_source, &name);
            }
            CandidateKind::InlineRef { ref_expr } => {
                hoist_inline_ref(
                    body,
                    ref_expr,
                    &module_source,
                    &name,
                    ty,
                    guarded.then_some(is_uninitialized),
                );
            }
            CandidateKind::ValueArg { arg_expr } => {
                hoist_value_arg(
                    body,
                    arg_expr,
                    &module_source,
                    &name,
                    ty,
                    guarded.then_some(is_uninitialized),
                );
            }
        }
        drop(func);

        project.globals.push(NirGlobal {
            name,
            ty,
            // The hoisted value is written by the `GlobalVarSet` this pass
            // emits at the use site, so the storage starts at a placeholder.
            init: crate::tir::GlobalInit::Deferred(ExprBody::wrapping_value(
                crate::nir_value_graph::ValueKind::Null,
                ty,
                crate::token::Span::new(0, 0, 1, 1),
            )),
            wado_mutable: false,
            visibility: crate::ast::Visibility::Private,
            module_source,
            span: crate::token::Span::new(0, 0, 1, 1),
            locals: Vec::new(),
            prefer_fixed_string_repr: prefer_fixed_repr,
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
    let mut read_locals = IndexSet::default();
    collect_reads(body, &mut read_locals);
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Let {
                local_index,
                type_id,
                ..
            } = &body.stmts[s].kind
            && single_decl_locals.contains(local_index)
            && let Some(sibling_lets) = let_stmt_qualifies(body, s, gate, &siblings, &read_locals)
        {
            // The sibling `let`s move into the initializer at mutation time,
            // so a non-promotable value among them needs the guard just as
            // much as one in the candidate's own value.
            let guarded = std::iter::once(s)
                .chain(sibling_lets.iter().copied())
                .any(|st| stmt_needs_lazy_guard(body, st, gate, false));
            let kind = CandidateKind::LetBinding {
                local_index: *local_index,
                sibling_lets,
            };
            out.push(Candidate {
                func_idx,
                ty: *type_id,
                module_source: module_source.clone(),
                kind,
                guarded,
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
                let kind = CandidateKind::InlineRef { ref_expr: id };
                let guarded = needs_lazy_guard(body, inner, gate, kind.prefer_fixed_repr());
                out.push(Candidate {
                    func_idx,
                    ty: inner_ty,
                    module_source: module_source.clone(),
                    kind,
                    guarded,
                });
                continue;
            }
        }
        if let NodeRef::Expr(id) = node {
            let hoisted = value_arg_candidates(body, id, gate);
            if !hoisted.is_empty() {
                for &arg in &hoisted {
                    let kind = CandidateKind::ValueArg { arg_expr: arg };
                    let guarded = needs_lazy_guard(body, arg, gate, kind.prefer_fixed_repr());
                    out.push(Candidate {
                        func_idx,
                        ty: body.exprs[arg].type_id,
                        module_source: module_source.clone(),
                        kind,
                        guarded,
                    });
                }
                // The hoisted arguments keep their subtrees verbatim inside the
                // global's initializer, so a nested candidate there would nest
                // one global's `GlobalVarSet` inside another's — the shape the
                // `let`/`&` cases avoid by not recursing either.
                body.for_each_child(node, |c| {
                    if !matches!(c, NodeRef::Expr(e) if hoisted.contains(&e)) {
                        stack.push(c);
                    }
                });
                continue;
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// The arguments of the call at `expr` that qualify for by-value hoisting.
///
/// An argument qualifies when it is a closed constant aggregate (the gate the
/// `let` and `&` cases use) *and* the callee's matching parameter is read-only
/// in the callee's own body. That second gate is what makes sharing sound: a
/// fresh literal is handed over uncopied, so a callee that writes its parameter
/// would write the shared global.
fn value_arg_candidates(body: &Body, expr: ExprId, gate: &Gate<'_>) -> Vec<ExprId> {
    // `self` occupies parameter 0 of a method, so its explicit arguments start
    // one position later.
    let (func_id, args, first_param) = match &body.exprs[expr].kind {
        ExprKind::Call { func_id, args, .. } => (*func_id, args, 0),
        ExprKind::MethodCall { func_id, args, .. } => (*func_id, args, 1),
        _ => return Vec::new(),
    };
    args.iter()
        .enumerate()
        .filter(|(_, arg)| !arg.is_mut)
        .filter_map(|(pos, arg)| Some((pos, arg.expr.as_expr()?)))
        .filter(|&(_, arg)| {
            // A `&`-wrapped literal is the `InlineRef` case; leave it there.
            !matches!(
                body.exprs[arg].kind,
                ExprKind::Unary {
                    op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                    ..
                }
            )
        })
        .filter(|&(pos, arg)| {
            let ty = body.exprs[arg].type_id;
            gate.is_reference_type(ty)
                && is_globalizable_const(body, arg, gate, &mut IndexSet::default())
                && contains_aggregate(body, arg, gate)
                && gate.callee_param_readonly(func_id, first_param + pos)
        })
        .map(|(_, arg)| arg)
        .collect()
}

/// Rewrite a `ValueArg` candidate in place: the argument node `E` becomes
/// `{ GlobalVarSet(name, E); GlobalVarGet(name) }`. The original subtree moves
/// into the set, keeping its evaluation position, and the call now reads the
/// named global — which `wir_optimize::const_global` promotes to a Wasm
/// instantiation-time constant when `E` is const-expressible.
fn hoist_value_arg(
    body: &mut Body,
    arg_expr: ExprId,
    module_source: &ModuleSource,
    name: &str,
    ty: TypeId,
    guarded: Option<crate::nir::FuncId>,
) {
    let span = body.exprs[arg_expr].span;
    let inner_kind = std::mem::replace(
        &mut body.exprs[arg_expr].kind,
        ExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.to_string(),
        },
    );
    let inner = body.exprs.push(ExprNode {
        kind: inner_kind,
        type_id: ty,
        span,
    });

    let set_expr = body.exprs.push(ExprNode {
        kind: ExprKind::GlobalVarSet {
            module_source: module_source.clone(),
            name: name.to_string(),
            value: Operand::Expr(inner),
        },
        type_id: TypeTable::UNIT,
        span,
    });
    let set_stmt = if let Some(is_uninitialized) = guarded {
        guard_set_on_uninit(
            body,
            set_expr,
            module_source,
            name,
            ty,
            is_uninitialized,
            span,
        )
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
    body.exprs[arg_expr].kind = ExprKind::Block(wrap_block);
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
    guarded: Option<crate::nir::FuncId>,
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
    let set_stmt = if let Some(is_uninitialized) = guarded {
        guard_set_on_uninit(
            body,
            set_expr,
            module_source,
            name,
            ty,
            is_uninitialized,
            span,
        )
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
    read_locals: &IndexSet<u32>,
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
    // A binding nothing reads any more — its uses all folded — is dead code
    // on its way out, not a hoist target: hoisting it manufactures a live
    // global (and, once guarded, one no WIR cleanup deletes) from a `let`
    // the write-only elider would otherwise drop.
    if !read_locals.contains(&local_index) {
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

fn block_is_const(body: &Body, block: BlockId, gate: &Gate<'_>, bound: &mut IndexSet<u32>) -> bool {
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
    op.as_expr()
        .is_some_and(|e| contains_aggregate(body, e, gate))
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
                StmtKind::Expr(value) => value
                    .as_expr()
                    .is_some_and(|e| contains_aggregate(body, e, gate)),
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
    /// Indexed by `func_id.index()`.
    hoistable_pure: &'a [bool],
    structs: &'a [crate::nir::NirStruct],
    /// `(callee index, parameter position)` → [`Gate::callee_param_readonly`].
    /// Each verdict costs two walks of the callee body, and one helper taking a
    /// constant is typically called from many sites.
    param_readonly: RefCell<IndexMap<(usize, usize), bool>>,
    /// The opt-level's `NirPackage::string_inline_max_bytes`, the eager
    /// `array.new_fixed` bound `translate_packed_array` applies to a `let`-shape
    /// global's value.
    string_inline_max_bytes: usize,
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
    /// Hoisting is not free: it costs a global, a guard branch, and an object
    /// that stays live for the whole program. That only pays when rebuilding
    /// the value would re-allocate — i.e. when it (transitively) owns a GC
    /// array, as `String` and `List` do.
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
            | ResolvedType::Newtype {
                base_type: inner, ..
            }
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

    /// Whether the callee takes its receiver by `&self` — the only receiver
    /// convention that neither writes the caller's storage (`&mut self`) nor
    /// takes it over (a by-value `self`). An unknown callee answers `false`.
    fn callee_borrows_self(&self, func_id: crate::nir::FuncId) -> bool {
        use cranelift_entity::EntityRef;
        let Some(f) = self.funcs.get(func_id.index()) else {
            return false;
        };
        let f = f.borrow();
        f.params.first().is_some_and(|p0| {
            matches!(
                self.type_table.borrow().get(p0.type_id),
                ResolvedType::Ref(_)
            )
        })
    }

    /// Whether parameter `param_pos` is taken by value and never written or
    /// consumed inside the callee — the precondition for handing it a shared
    /// global instead of a fresh object.
    ///
    /// A bodyless callee (an import, an unresolved id) answers `false`: nothing
    /// here can prove what it does with the value. A `&` / `&mut` parameter
    /// answers `false` too — a by-value argument never lands there, and `&mut`
    /// writes the caller's storage outright.
    fn callee_param_readonly(&self, func_id: crate::nir::FuncId, param_pos: usize) -> bool {
        use cranelift_entity::EntityRef;
        let key = (func_id.index(), param_pos);
        if let Some(&cached) = self.param_readonly.borrow().get(&key) {
            return cached;
        }
        let verdict = self.compute_param_readonly(func_id, param_pos);
        self.param_readonly.borrow_mut().insert(key, verdict);
        verdict
    }

    fn compute_param_readonly(&self, func_id: crate::nir::FuncId, param_pos: usize) -> bool {
        use cranelift_entity::EntityRef;
        let Some(f) = self.funcs.get(func_id.index()) else {
            return false;
        };
        let f = f.borrow();
        let Some(param) = f.params.get(param_pos) else {
            return false;
        };
        if matches!(
            self.type_table.borrow().get(param.type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        ) {
            return false;
        }
        f.body.as_ref().is_some_and(|body| {
            is_readonly_body(body, param.local_index, self)
                && !param_storage_escapes(body, param.local_index, self)
        })
    }
}

/// True when every use of local `idx` in `body` keeps it immutable.
fn is_readonly_body(body: &Body, idx: u32, gate: &Gate<'_>) -> bool {
    block_readonly(body, body.root, idx, gate)
}

/// Whether the callee hands the *storage* of by-value parameter `idx` back out
/// — returning `s.data`, stashing it in a global, or passing it on by value.
///
/// [`is_readonly_body`] does not cover this: reading a field of the parameter is
/// a read, and a by-value parameter is the callee's own copy, so returning that
/// field is legitimate — the return-convention fixpoint even calls it *owned*,
/// which is what lets the caller skip a defensive copy of the result. Hoisting
/// the argument invalidates the premise: the "owned" value handed back is the
/// shared global's storage, and the first mutation corrupts the constant.
/// Passing the storage on by value hands the same problem to the next callee.
///
/// A borrow (`&s.data`) is not an escape — the callee's own read-only gate
/// already bounds what the borrow can do — and a scalar projection copies its
/// value outright.
fn param_storage_escapes(body: &Body, idx: u32, gate: &Gate<'_>) -> bool {
    let roots = projection_alias_roots(body, idx, gate);
    let escapes = |op: Operand| delivers_projection_operand(body, op, &roots, gate);
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Stmt(s) => match &body.stmts[s].kind {
                StmtKind::Return { value } | StmtKind::Break { value, .. } => {
                    if value.is_some_and(escapes) {
                        return true;
                    }
                }
                StmtKind::Let { value, .. } => {
                    // A `let` alias is itself a root, so its own uses are
                    // covered by this walk.
                    let _ = value;
                }
                StmtKind::Expr(_)
                | StmtKind::If { .. }
                | StmtKind::Loop { .. }
                | StmtKind::LabeledBlock { .. }
                | StmtKind::LetDestructure { .. }
                | StmtKind::Continue => {}
            },
            NodeRef::Expr(e) => match &body.exprs[e].kind {
                ExprKind::Assign { value, .. } => {
                    if escapes(*value) {
                        return true;
                    }
                }
                ExprKind::Call { args, .. } => {
                    if args.iter().any(|a| escapes(a.expr)) {
                        return true;
                    }
                }
                // A by-value `self` receiver hands the storage to the callee,
                // which may return or store it in turn; `&self` only reads it.
                ExprKind::MethodCall {
                    receiver,
                    func_id,
                    args,
                    ..
                } => {
                    if args.iter().any(|a| escapes(a.expr))
                        || (escapes(*receiver) && !gate.callee_borrows_self(*func_id))
                    {
                        return true;
                    }
                }
                ExprKind::IndirectCall { args, .. } => {
                    if args.iter().any(|&a| escapes(a)) {
                        return true;
                    }
                }
                _ => {}
            },
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}

/// `idx` plus every local *bound* from something that can yield one of their
/// storages: a `let` (`let r = s.data;`, which LICM also produces), a
/// destructuring `let`, or a `match` arm pattern over such a scrutinee
/// (`match s.inner { Inner { blob } => …`). They all name the same storage, so
/// an escape through any of them is an escape of the parameter.
///
/// The source only has to *contain* a reference-typed projection of a root —
/// `let r = if c { s.a } else { s.b };` binds one of two projections, and
/// neither the branch shape nor the pattern says which. Over-approximating a
/// binding as an alias only makes the escape check stricter: a scalar binding
/// can never carry a reference-typed projection of itself, so it is inert.
fn projection_alias_roots(body: &Body, idx: u32, gate: &Gate<'_>) -> Vec<u32> {
    let mut roots = vec![idx];
    let mut i = 0;
    while i < roots.len() {
        let root = roots[i];
        let mut stack = vec![NodeRef::Block(body.root)];
        while let Some(node) = stack.pop() {
            match node {
                NodeRef::Stmt(s) => match &body.stmts[s].kind {
                    StmtKind::Let {
                        local_index, value, ..
                    } => {
                        if yields_projection_of(body, *value, root, gate)
                            && !roots.contains(local_index)
                        {
                            roots.push(*local_index);
                        }
                    }
                    StmtKind::LetDestructure { pattern, value, .. } => {
                        if yields_projection_of(body, *value, root, gate) {
                            collect_pattern_bindings(body, *pattern, &mut roots);
                        }
                    }
                    StmtKind::Expr(_)
                    | StmtKind::Return { .. }
                    | StmtKind::Break { .. }
                    | StmtKind::If { .. }
                    | StmtKind::Loop { .. }
                    | StmtKind::LabeledBlock { .. }
                    | StmtKind::Continue => {}
                },
                NodeRef::Expr(e) => {
                    if let ExprKind::Match { expr, arms } = &body.exprs[e].kind
                        && yields_projection_of(body, *expr, root, gate)
                    {
                        for arm in arms {
                            collect_pattern_bindings(body, arm.pattern, &mut roots);
                        }
                    }
                }
                NodeRef::Block(_) | NodeRef::Pat(_) => {}
            }
            body.for_each_child(node, |c| stack.push(c));
        }
        i += 1;
    }
    roots
}

fn delivers_projection_operand(body: &Body, op: Operand, roots: &[u32], gate: &Gate<'_>) -> bool {
    op.as_expr()
        .is_some_and(|e| delivers_projection(body, e, roots, gate))
}

/// Whether evaluating `expr` yields the storage of a root — directly, or as the
/// value a branch delivers.
///
/// `return if c { s.tag } else { s.inner.blob };` hands out a projection just as
/// plainly as `return s.tag;` does, and `analyze::is_owned_value` treats such a
/// branch result as owned, so the caller keeps no copy of it. A block's value is
/// its tail expression, an `if`'s is the tail of whichever branch runs, and a
/// `match`/`switch`'s is its arm's — the same rule the freshness side follows.
fn delivers_projection(body: &Body, expr: ExprId, roots: &[u32], gate: &Gate<'_>) -> bool {
    if gate.is_reference_type(body.exprs[expr].type_id)
        && roots.iter().any(|&r| projection_roots_at(body, expr, r))
    {
        return true;
    }
    match &body.exprs[expr].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            block_tail_delivers(body, *block, roots, gate)
        }
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            block_tail_delivers(body, *then_branch, roots, gate)
                || else_branch.is_some_and(|eb| block_tail_delivers(body, eb, roots, gate))
        }
        ExprKind::Match { arms, .. } => arms
            .iter()
            .any(|arm| delivers_projection_operand(body, arm.body, roots, gate)),
        ExprKind::Switch { arms, default, .. } => {
            arms.iter()
                .any(|&a| block_tail_delivers(body, a, roots, gate))
                || block_tail_delivers(body, *default, roots, gate)
        }
        _ => false,
    }
}

/// The value a block falls off its end with: its tail expression statement.
/// A `break`-delivered value is checked where the `break` is.
fn block_tail_delivers(body: &Body, block: BlockId, roots: &[u32], gate: &Gate<'_>) -> bool {
    body.blocks[block]
        .stmts
        .last()
        .is_some_and(|&s| match &body.stmts[s].kind {
            StmtKind::Expr(op) => delivers_projection_operand(body, *op, roots, gate),
            _ => false,
        })
}

/// Every local a pattern binds, appended to `out` if not already there.
fn collect_pattern_bindings(body: &Body, pattern: crate::nir_arena::PatId, out: &mut Vec<u32>) {
    let mut stack = vec![NodeRef::Pat(pattern)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Pat(p) = node
            && let crate::nir_arena::PatKind::Binding { local_index, .. } = &body.pats[p].kind
            && !out.contains(local_index)
        {
            out.push(*local_index);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// Whether evaluating `op` can produce the storage of `root` itself: it is, or
/// contains, a reference-typed projection rooted there. A scalar projection
/// (`s.used`) copies its value out and does not count.
fn yields_projection_of(body: &Body, op: Operand, root: u32, gate: &Gate<'_>) -> bool {
    let Some(expr) = op.as_expr() else {
        return false;
    };
    let mut stack = vec![NodeRef::Expr(expr)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && projection_roots_at(body, e, root)
            && gate.is_reference_type(body.exprs[e].type_id)
        {
            return true;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}

/// Whether `expr` is a projection chain (`x`, `x.f`, `x[i].f`) rooted at local
/// `idx`.
fn projection_roots_at(body: &Body, expr: ExprId, idx: u32) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => *index == idx,
        ExprKind::FieldAccess { expr: base, .. }
        | ExprKind::Index { expr: base, .. }
        | ExprKind::Cast { expr: base, .. } => base
            .as_expr()
            .is_some_and(|e| projection_roots_at(body, e, idx)),
        _ => false,
    }
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

/// True when `expr` cannot end as a Wasm constant instruction, so
/// `wir_optimize::const_global` cannot delete the assignment and the
/// candidate needs the lazy-init guard. Three shapes qualify:
///
/// - a call, never const-expressible;
/// - a `PackedArray` outside the eager `array.new_fixed` bound
///   ([`crate::wir_build::packed_array_is_eager`], the same choice
///   `translate_packed_array` makes);
/// - an `ArrayLiteral` of scalar constants at or past
///   `ARRAY_NEW_DATA_THRESHOLD`, which `promote_constant_arrays_to_data`
///   rewrites to `array.new_data` before the classifier runs. Scalar-ness is
///   read off the operands — a pure scalar element is born as
///   `Operand::Value` — since only packable primitives lose the fixed repr.
fn needs_lazy_guard(body: &Body, expr: ExprId, gate: &Gate<'_>, prefer_fixed: bool) -> bool {
    let mut stack = vec![NodeRef::Expr(expr)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            match &body.exprs[id].kind {
                ExprKind::Call { .. }
                | ExprKind::MethodCall { .. }
                | ExprKind::IndirectCall { .. }
                | ExprKind::CmRawCall { .. } => return true,
                ExprKind::PackedArray(bytes) => {
                    if !crate::wir_build::packed_array_is_eager(
                        bytes.len(),
                        gate.string_inline_max_bytes,
                        prefer_fixed,
                    ) {
                        return true;
                    }
                }
                ExprKind::ArrayLiteral { elements } => {
                    let packable = |op: &Operand| match op {
                        Operand::Value(_) => true,
                        Operand::Expr(e) => {
                            matches!(body.exprs[*e].kind, ExprKind::EnumConstruct { .. })
                        }
                    };
                    if elements.len() >= crate::wir_optimize::array::ARRAY_NEW_DATA_THRESHOLD
                        && elements.iter().all(packable)
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}

fn stmt_needs_lazy_guard(body: &Body, stmt: StmtId, gate: &Gate<'_>, prefer_fixed: bool) -> bool {
    let StmtKind::Let { value, .. } = &body.stmts[stmt].kind else {
        unreachable!("[NIR] const_object_globalization: guard candidates are `let` bindings");
    };
    value
        .as_expr()
        .is_some_and(|e| needs_lazy_guard(body, e, gate, prefer_fixed))
}

/// Wrap a hoisted `GlobalVarSet` in `if builtin::is_uninitialized(<global>)`.
///
/// The unguarded form is correct only because `wir_optimize::const_global`
/// promotes a Wasm-const-expressible initializer into the global's eager
/// `init` and deletes the assignment. A call is never const-expressible, and
/// neither is a `PackedArray` past its eager bound (its repr is
/// `array.new_data`), so such an assignment survives — and an unguarded one
/// re-runs on every activation, which is the opposite of hoisting.
///
/// The guard also pins the semantics: initialization happens at the first
/// execution of the expression it replaced, so a callee that traps or diverges
/// still does so, at the same point. Moving the work to module-init instead
/// would drag both to instantiation time.
fn guard_set_on_uninit(
    body: &mut Body,
    set: ExprId,
    module_source: &ModuleSource,
    name: &str,
    ty: TypeId,
    is_uninitialized: crate::nir::FuncId,
    span: crate::token::Span,
) -> StmtId {
    let global_get = body.exprs.push(ExprNode {
        kind: ExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.to_string(),
        },
        type_id: ty,
        span,
    });
    let cond = body.exprs.push(ExprNode {
        kind: ExprKind::Call {
            func_id: is_uninitialized,
            type_args: vec![ty],
            args: vec![crate::nir_arena::ArenaCallArg {
                expr: global_get.into(),
                is_mut: false,
            }],
        },
        type_id: TypeTable::BOOL,
        span,
    });
    let set_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(set.into()),
        span,
    });
    let then_block = body.blocks.push(BlockNode {
        stmts: vec![set_stmt],
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
    ty: TypeId,
    guarded: Option<crate::nir::FuncId>,
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
            if let Some(is_uninitialized) = guarded {
                let guard =
                    guard_set_on_uninit(body, set, module_source, name, ty, is_uninitialized, span);
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
