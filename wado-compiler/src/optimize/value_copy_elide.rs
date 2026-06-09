//! Strip the `$value_copy$T<id>` wrapper from `let x = $value_copy$T(arg)`
//! bindings whose target is observably read-only — the resulting `let x = arg`
//! aliases `arg`'s data exactly when the synthesized helper would have
//! returned a fresh struct, but the alias is unobservable because nothing
//! mutates `x` or the source root that `arg` reads from.
//!
//! Runs once after `synthesize_value_copy_funcs`, recovering the freshness
//! elision that the former WIR-level `value_copy` instruction performed
//! inline. Helpers whose remaining call sites are all elided are removed by
//! the post-elision DCE pass.
//!
//! Capture aliasing: NIR materialises closure captures as `NirCapture`
//! entries on a `ClosureFunctor`, snapshotted at functor construction
//! time. Outer mutations after the snapshot don't reach the captured
//! value, so eliding the wrapper at the binding site does not change
//! what the closure observes — no closure-capture safety gate is
//! needed here.
//!
//! Runs as `ValueCopyElideRule` inside the unified pre-inline peephole session
//! (combine migration; see `docs/wep-2026-06-05-worklist-rewrite-engine.md`).
//! The whole-function usage map (`analyze_usage`) is the safety oracle; it is
//! computed once per function from the pristine body before the session runs,
//! so eligibility decisions match the old standalone pass even as other rules
//! interleave. Strips go through the engine edit API.

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Strips `$value_copy$T(arg)` wrappers off observably read-only bindings, run
/// as a rule inside the unified peephole session (formerly the standalone
/// `nir/value_copy_elide` pass). `usage` is the same whole-function map
/// `analyze_usage` built, computed once per function from the pristine body
/// before the session runs (`build_usage`). It keys on local indices, not
/// nodes, so it stays valid as the session rewrites: the map is the maximal
/// (pristine) assign / field-mutation profile, and no peephole rule introduces
/// a new mutation of a local, so an entry can only become conservatively stale
/// (fewer strips), never unsound. Strips go through the engine edit API so the
/// worklist re-examines the unwrapped value.
pub(super) struct ValueCopyElideRule<'a> {
    value_copy_set: &'a IndexMap<(ModuleSource, String), TypeId>,
    usage: IndexMap<u32, LocalUsage>,
}

impl<'a> ValueCopyElideRule<'a> {
    pub(super) fn new(
        value_copy_set: &'a IndexMap<(ModuleSource, String), TypeId>,
        usage: IndexMap<u32, LocalUsage>,
    ) -> Self {
        Self {
            value_copy_set,
            usage,
        }
    }
}

/// Build the per-function usage map a [`ValueCopyElideRule`] needs, from the
/// pristine body before the engine session rewrites it.
pub(super) fn build_usage(body: &Body, type_table: &TypeTable) -> IndexMap<u32, LocalUsage> {
    analyze_usage(body, type_table)
}

impl Rule for ValueCopyElideRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if self.value_copy_set.is_empty() {
            return false;
        }
        let usage = &self.usage;
        let stmts = engine.body.blocks[block].stmts.clone();
        let mut changed = false;
        for stmt in stmts {
            let Some(value) = eligible_value(engine.body, stmt, self.value_copy_set, usage) else {
                continue;
            };
            // `value` is `$value_copy$T(arg)`; replace it with `arg` so the
            // binding aliases the source. The call returns `arg`'s own type, so
            // `value`'s type/span are unchanged — matching the old `*value = arg`.
            let ExprKind::Call { args, .. } = &engine.body.exprs[value].kind else {
                continue;
            };
            let Some(arg) = args.first().map(|a| a.expr) else {
                continue;
            };
            let arg_kind = engine.body.exprs[arg].kind.clone();
            engine.replace_expr_kind(value, arg_kind);
            changed = true;
        }
        changed
    }
}

fn is_mut_ref_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(type_id), ResolvedType::MutRef(_))
}

#[derive(Debug, Default)]
pub(super) struct LocalUsage {
    /// Count of `local = expr` assignments. Used by the Assign-form
    /// elision branch to recognize "binding-style" assignments (count
    /// == 1) — those behave like a `Let` and are eligible to strip.
    assign_count: u32,
    has_field_mutation: bool,
}

impl LocalUsage {
    /// True when the local is assigned at least once after its
    /// initialization. The Let-form elision uses this to refuse stripping
    /// a binding whose target is later overwritten (and therefore can't
    /// be safely aliased to the source).
    fn is_assigned(&self) -> bool {
        self.assign_count > 0
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Usage analysis
// ──────────────────────────────────────────────────────────────────────────────

/// Build the per-local usage map by walking every live expression reachable
/// from the body root. Visiting each live node once (in any order) is
/// equivalent to the old tree walk for an accumulating analysis, and walking
/// from the root rather than over every arena slot keeps dead nodes left by an
/// earlier in-place pass from being counted.
fn analyze_usage(body: &Body, type_table: &TypeTable) -> IndexMap<u32, LocalUsage> {
    let mut usage: IndexMap<u32, LocalUsage> = IndexMap::default();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            classify_expr(body, id, type_table, &mut usage);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    usage
}

/// Apply the usage-marking rules for a single expression node — the analog of
/// the old `UsageAnalyzer::visit_expr` arms, minus the recursion (the caller's
/// walk visits every node).
fn classify_expr(
    body: &Body,
    id: ExprId,
    type_table: &TypeTable,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    match &body.exprs[id].kind {
        ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
            ExprKind::Local { index, .. } => {
                usage.entry(*index).or_default().assign_count += 1;
            }
            ExprKind::FieldAccess { expr: inner, .. } => {
                mark_root_field_mutated(body, *inner, usage);
            }
            _ => {}
        },
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            mark_root_field_mutated(body, *inner, usage);
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                if arg.is_mut || is_mut_ref_type(body.exprs[arg.expr].type_id, type_table) {
                    mark_root_field_mutated(body, arg.expr, usage);
                }
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            // Auto-ref: the receiver carries `T` even for `&mut self`
            // methods, so be conservative and treat any local receiver as
            // potentially field-mutated by the call.
            mark_root_field_mutated(body, *receiver, usage);
            for arg in args {
                if arg.is_mut || is_mut_ref_type(body.exprs[arg.expr].type_id, type_table) {
                    mark_root_field_mutated(body, arg.expr, usage);
                }
            }
        }
        ExprKind::IndirectCall { args, .. } => {
            for &arg in args {
                if is_mut_ref_type(body.exprs[arg].type_id, type_table) {
                    mark_root_field_mutated(body, arg, usage);
                }
            }
        }
        _ => {}
    }
}

/// Mark every local that contributes to `expr`'s observable storage as
/// potentially field-mutated, following pure projections (`FieldAccess`,
/// `VariantPayload`, `Cast`, `Unary`). Mirrors `copy_prop`'s
/// `mark_potentially_mutated_local`.
fn mark_root_field_mutated(body: &Body, expr: ExprId, usage: &mut IndexMap<u32, LocalUsage>) {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => {
            usage.entry(*index).or_default().has_field_mutation = true;
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => {
            mark_root_field_mutated(body, *inner, usage);
        }
        _ => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Wrapper stripping
// ──────────────────────────────────────────────────────────────────────────────

fn is_value_copy_call(
    body: &Body,
    expr: ExprId,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
) -> bool {
    if let ExprKind::Call { func, args, .. } = &body.exprs[expr].kind
        && args.len() == 1
    {
        value_copy_set.contains_key(&(func.module_source.clone(), func.name.clone()))
    } else {
        false
    }
}

/// Find the root local that `arg` reads from, descending through pure
/// projections that share storage with their inner expression. Returns `None`
/// for non-projection expressions (calls, literals, constructors): those
/// produce fresh GC values that cannot be aliased from outside, so elision is
/// always safe regardless of surrounding state.
fn arg_source_root(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. } => arg_source_root(body, *inner),
        _ => None,
    }
}

/// Check whether `value` is a `$value_copy$T(arg)` call whose wrapper can be
/// safely stripped given the binding target's local index and the
/// function-wide usage map.
fn elision_safe(
    body: &Body,
    target_index: u32,
    target_assign_limit: u32,
    value: ExprId,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> bool {
    if !is_value_copy_call(body, value, value_copy_set) {
        return false;
    }
    let target_ok = match usage.get(&target_index) {
        Some(u) => u.assign_count <= target_assign_limit && !u.has_field_mutation,
        None => true,
    };
    if !target_ok {
        return false;
    }
    let ExprKind::Call { args, .. } = &body.exprs[value].kind else {
        return false;
    };
    let Some(arg) = args.first() else {
        return false;
    };
    match arg_source_root(body, arg.expr) {
        Some(root) => match usage.get(&root) {
            Some(u) => !u.is_assigned() && !u.has_field_mutation,
            None => true,
        },
        None => true,
    }
}

/// Replace the `$value_copy$T(arg)` call at `value` with its single argument,
/// in place. The call returns the argument's own type, so keeping `value`'s
/// `type_id` / `span` matches the old `*value = arg` rewrite; the orphaned
/// Return the `$value_copy$T(arg)` call expression of `stmt` when `stmt` binds /
/// assigns a read-only local to such a call (and is thus safe to unwrap). The
/// caller performs the unwrap via the engine edit API.
fn eligible_value(
    body: &Body,
    stmt: StmtId,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> Option<ExprId> {
    match &body.stmts[stmt].kind {
        // `let x = $value_copy$T(arg)` — Let establishes a fresh binding, so
        // any subsequent assignment to `x` invalidates the snapshot; require
        // `assign_count == 0`.
        StmtKind::Let {
            local_index,
            value,
            is_mut,
            skip_value_copy,
            ..
        } => {
            if !*is_mut
                && !*skip_value_copy
                && elision_safe(body, *local_index, 0, *value, value_copy_set, usage)
            {
                Some(*value)
            } else {
                None
            }
        }
        // `x = $value_copy$T(arg)` top-level — the Assign *is* the binding.
        // Allow elision when this is the only assignment (`assign_count == 1`).
        StmtKind::Expr(e) => {
            if let ExprKind::Assign { target, value } = &body.exprs[*e].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                && elision_safe(body, *index, 1, *value, value_copy_set, usage)
            {
                Some(*value)
            } else {
                None
            }
        }
        _ => None,
    }
}
