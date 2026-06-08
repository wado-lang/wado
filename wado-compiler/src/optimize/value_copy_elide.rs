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
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a direct arena walk
//! rather than a worklist rule: this is a single-pass, flow-insensitive
//! transform, so it walks the live blocks once and strips each eligible
//! binding's wrapper in place — exactly the old visitor's one-shot behaviour,
//! with none of a fixed-point loop's risk of stripping a freshly exposed
//! nested wrapper that the original would have left for the next iteration.

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

/// Returns whether any wrapper was stripped. Gate-skipped: a pure per-function
/// rewrite (it removes `$value_copy$T(arg)` `Call` wrappers within a body, with
/// no signature or call-graph change), so it skips functions unchanged since it
/// last ran and reports the ones it strips via the gate.
pub fn elide_synthesized_value_copies(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let value_copy_set = project.value_copy_helper_types();
    if value_copy_set.is_empty() {
        return false;
    }
    let type_table = project.type_table.clone();
    let len = project.functions.len();
    gate.run_gated(GatedPass::ValueCopyElide, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.is_value_copy() {
            return false;
        }
        if let Some(body) = func.body.as_mut() {
            let usage = analyze_usage(body, &type_table.borrow());
            strip_wrappers_in_body(body, &value_copy_set, &usage)
        } else {
            false
        }
    })
}

fn is_mut_ref_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(type_id), ResolvedType::MutRef(_))
}

#[derive(Debug, Default)]
struct LocalUsage {
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
/// argument node becomes dead and is purged by the next `from_block` rebuild.
fn strip_wrapper(body: &mut Body, value: ExprId) {
    let arg = match &body.exprs[value].kind {
        ExprKind::Call { args, .. } => args.first().map(|a| a.expr),
        _ => None,
    };
    if let Some(arg) = arg {
        let arg_kind = body.exprs[arg].kind.clone();
        body.exprs[value].kind = arg_kind;
    }
}

/// Walk the live blocks and strip every eligible `$value_copy$T` wrapper in a
/// single pass.
fn strip_wrappers_in_body(
    body: &mut Body,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> bool {
    let mut blocks = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Block(b) = node {
            blocks.push(b);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    let mut changed = false;
    for b in blocks {
        let stmts = body.blocks[b].stmts.clone();
        for stmt in stmts {
            changed |= try_strip_stmt(body, stmt, value_copy_set, usage);
        }
    }
    changed
}

/// Strip the wrapper of one statement when it binds / assigns a read-only local
/// to a `$value_copy$T(arg)` call.
fn try_strip_stmt(
    body: &mut Body,
    stmt: StmtId,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> bool {
    let target = match &body.stmts[stmt].kind {
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
    };
    if let Some(value) = target {
        strip_wrapper(body, value);
        true
    } else {
        false
    }
}
