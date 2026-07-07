//! Strip the `$value_copy$T<id>` wrapper wherever the fresh clone it produces is
//! unobservable, so the binding / argument aliases the source instead. Two
//! families of site are handled:
//!
//! - Binding positions — `let x = $value_copy$T(arg)` (and the analogous
//!   single-assignment and read-only aggregate-literal forms): safe when both
//!   `x` and `arg`'s source root are observably read-only, so the alias is never
//!   mutated.
//! - Call-argument positions — `f(…, $value_copy$T(arg), …)`: safe when the
//!   copy is a no-op (a fresh rvalue), a move (a parameter used only here), or
//!   the callee parameter is confined — non-`mut` and non-escaping per the
//!   interprocedural [`super::escape`] analysis. This recovers the elision the
//!   blanket by-value-argument copy would otherwise defeat (println / CM glue,
//!   `SequenceLiteralBuilder::push_literal`, …).
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

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::escape::EscapeMap;

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
    value_copy_ids: &'a IndexSet<FuncId>,
    escape: &'a EscapeMap,
    type_table: &'a TypeTable,
    n_params: u32,
    usage: IndexMap<u32, LocalUsage>,
}

impl<'a> ValueCopyElideRule<'a> {
    pub(super) fn new(
        value_copy_ids: &'a IndexSet<FuncId>,
        escape: &'a EscapeMap,
        type_table: &'a TypeTable,
        n_params: u32,
        usage: IndexMap<u32, LocalUsage>,
    ) -> Self {
        Self {
            value_copy_ids,
            escape,
            type_table,
            n_params,
            usage,
        }
    }
}

/// Build the per-function usage map a [`ValueCopyElideRule`] needs, from the
/// pristine body before the engine session rewrites it.
pub(super) fn build_usage(
    body: &Body,
    type_table: &TypeTable,
    receiver_mut: &IndexMap<FuncId, bool>,
) -> IndexMap<u32, LocalUsage> {
    analyze_usage(body, type_table, receiver_mut)
}

/// Whether each function mutates its receiver, keyed by id: `true` when the
/// first parameter is `&mut T`. A method call's receiver is auto-referenced to
/// `T` regardless of the callee's `self` mode, so the receiver expression's
/// type can't tell `&self` from `&mut self` — the callee signature is the only
/// witness. Ids absent from the map are treated conservatively as mutating.
pub(super) fn build_receiver_mut(
    project: &NirPackage,
    type_table: &TypeTable,
) -> IndexMap<FuncId, bool> {
    let mut map = IndexMap::default();
    for func in &project.functions {
        let func = func.borrow();
        // Only bodied functions carry monomorphized parameter type ids valid in
        // this table; bodyless callees stay absent (conservatively mutating).
        if func.body.is_none() {
            continue;
        }
        let Some(id) = func.id else { continue };
        let mutates = func
            .params
            .first()
            .is_some_and(|p| is_mut_ref_type(p.type_id, type_table));
        map.insert(id, mutates);
    }
    map
}

impl Rule for ValueCopyElideRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if self.value_copy_ids.is_empty() {
            return false;
        }
        let usage = &self.usage;
        let stmts = engine.body.blocks[block].stmts.clone();
        let mut changed = false;
        for stmt in stmts {
            let mut targets = collect_strippable(engine.body, stmt, self.value_copy_ids, usage);
            self.collect_call_arg_copies(engine.body, NodeRef::Stmt(stmt), &mut targets);
            self.collect_fresh_copies(engine.body, NodeRef::Stmt(stmt), &mut targets);
            for value in targets {
                // The collectors overlap (a fresh copy can also sit in a call-arg
                // or binding position) and a strip can rewrite an ancestor target
                // in place, so re-check that `value` is still a `$value_copy$T`
                // call before unwrapping — otherwise we would grab the first
                // argument of whatever kind replaced it.
                if !is_value_copy_call(engine.body, value, self.value_copy_ids) {
                    continue;
                }
                // `value` is `$value_copy$T(arg)`; replace it with `arg` so the
                // binding aliases the source. The call returns `arg`'s own type,
                // so `value`'s type/span are unchanged.
                let ExprKind::Call { args, .. } = &engine.body.exprs[value].kind else {
                    continue;
                };
                let Some(arg) = args.first().map(|a| a.expr) else {
                    continue;
                };
                // A promoted `Operand::Value` arg (a constant — copying it is a
                // no-op) redirects directly; otherwise adopt the skeleton arg's
                // kind.
                match arg.as_expr() {
                    Some(ae) => {
                        let arg_kind = engine.body.exprs[ae].kind.clone();
                        engine.replace_expr_kind(value, arg_kind);
                    }
                    None => {
                        engine.redirect_expr(value, arg);
                    }
                }
                changed = true;
            }
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
    /// Count of every `Local` occurrence (reads and assignment targets). A
    /// count of 1 means the local is mentioned exactly once, so a copy of it at
    /// that single site is its last use — a move.
    occurrences: u32,
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
fn analyze_usage(
    body: &Body,
    type_table: &TypeTable,
    receiver_mut: &IndexMap<FuncId, bool>,
) -> IndexMap<u32, LocalUsage> {
    let mut usage: IndexMap<u32, LocalUsage> = IndexMap::default();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            classify_expr(body, id, type_table, receiver_mut, &mut usage);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    usage
}

/// Apply the usage-marking rules for a single expression node. No recursion:
/// the caller's walk visits every node.
fn classify_expr(
    body: &Body,
    id: ExprId,
    type_table: &TypeTable,
    receiver_mut: &IndexMap<FuncId, bool>,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    if let ExprKind::Local { index, .. } = &body.exprs[id].kind {
        usage.entry(*index).or_default().occurrences += 1;
    }
    match &body.exprs[id].kind {
        ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
            ExprKind::Local { index, .. } => {
                usage.entry(*index).or_default().assign_count += 1;
            }
            ExprKind::FieldAccess { expr: inner, .. } => {
                mark_root_field_mutated_operand(body, *inner, usage);
            }
            _ => {}
        },
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let Some(e) = inner.as_expr() {
                mark_root_field_mutated(body, e, usage);
            }
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                if let Some(ae) = arg.expr.as_expr()
                    && (arg.is_mut || is_mut_ref_type(body.exprs[ae].type_id, type_table))
                {
                    mark_root_field_mutated(body, ae, usage);
                }
            }
        }
        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            // Auto-ref carries the receiver as `T` even for `&mut self`
            // methods, so its expr type can't witness the `self` mode; consult
            // the callee (unknown ids treated conservatively as mutating). A
            // `&self` / by-value method never mutates the caller's receiver, so
            // its receiver stays read-only and a binding copy of it can strip.
            if receiver_mut.get(func_id).copied().unwrap_or(true)
                && let Some(re) = receiver.as_expr()
            {
                mark_root_field_mutated(body, re, usage);
            }
            for arg in args {
                if let Some(ae) = arg.expr.as_expr()
                    && (arg.is_mut || is_mut_ref_type(body.exprs[ae].type_id, type_table))
                {
                    mark_root_field_mutated(body, ae, usage);
                }
            }
        }
        ExprKind::IndirectCall { args, .. } => {
            for &arg in args {
                if let Some(ae) = arg.as_expr()
                    && is_mut_ref_type(body.exprs[ae].type_id, type_table)
                {
                    mark_root_field_mutated_operand(body, arg, usage);
                }
            }
        }
        _ => {}
    }
}

/// [`mark_root_field_mutated`] for an operand.
fn mark_root_field_mutated_operand(
    body: &Body,
    op: Operand,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    if let Some(e) = op.as_expr() {
        mark_root_field_mutated(body, e, usage);
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
            mark_root_field_mutated_operand(body, *inner, usage);
        }
        _ => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Wrapper stripping
// ──────────────────────────────────────────────────────────────────────────────

fn is_value_copy_call(body: &Body, expr: ExprId, value_copy_ids: &IndexSet<FuncId>) -> bool {
    if let ExprKind::Call { func_id, args, .. } = &body.exprs[expr].kind
        && args.len() == 1
    {
        value_copy_ids.contains(func_id)
    } else {
        false
    }
}

/// Find the root local that `arg` reads from, descending through projections
/// and indexing that share storage with the container they read from. Returns
/// `None` when no local root is reachable — a call result, a constant, or a
/// bare literal. `None` does *not* by itself mean "fresh": an accessor call such
/// as `container.index_value(i)` also returns `None` yet aliases the container's
/// element, so callers pair it with a freshness gate (`EscapeMap::rvalue_is_fresh`
/// or `yields_subexpression`).
fn arg_source_root(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. }
        // An indexed element shares the container's storage.
        | ExprKind::Index { expr: inner, .. } => {
            inner.as_expr().and_then(|e| arg_source_root(body, e))
        }
        _ => None,
    }
}

/// True when `arg` reads storage reachable only by dereferencing a reference
/// (`*r`, `*r as T`, a field of `*r`, …). The pointee of a reference has no
/// local identity: mutations through the reference (`*r = v` lowers to
/// `tmp = r; tmp.field = …` on a fresh alias local, so they never touch `r`'s
/// own usage) are invisible to the local-usage oracle. Eliding such a copy
/// would alias the pointee and let a later write through the reference corrupt
/// the binding (wado-lang/wado#1522), so these copies are never stripped.
fn reads_through_deref(body: &Body, expr: ExprId) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::Unary { op, expr: inner } => {
            *op == NirUnaryOp::Deref
                || inner
                    .as_expr()
                    .is_some_and(|e| reads_through_deref(body, e))
        }
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. } => inner
            .as_expr()
            .is_some_and(|e| reads_through_deref(body, e)),
        _ => false,
    }
}

/// Whether the local `target_index` is read-only enough to alias a source into:
/// at most `assign_limit` reassignments and no field mutation. The alias a strip
/// creates is unobservable only when this holds for both ends (the binding side
/// here, the source side in [`copy_source_strippable`]).
fn is_target_read_only(
    target_index: u32,
    assign_limit: u32,
    usage: &IndexMap<u32, LocalUsage>,
) -> bool {
    match usage.get(&target_index) {
        Some(u) => u.assign_count <= assign_limit && !u.has_field_mutation,
        None => true,
    }
}

/// Whether `value` is a `$value_copy$T(arg)` call whose *source* side is safe to
/// alias: the arg does not read through a reference deref (wado-lang/wado#1522),
/// and its source root local is never mutated. The binding/consumer side is
/// checked separately by the caller.
fn copy_source_strippable(
    body: &Body,
    value: ExprId,
    value_copy_ids: &IndexSet<FuncId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> bool {
    if !is_value_copy_call(body, value, value_copy_ids) {
        return false;
    }
    let ExprKind::Call { args, .. } = &body.exprs[value].kind else {
        return false;
    };
    let Some(arg) = args.first() else {
        return false;
    };
    let src = arg.expr.as_expr();
    if src.is_some_and(|e| reads_through_deref(body, e)) {
        return false;
    }
    match src.and_then(|e| arg_source_root(body, e)) {
        Some(root) => match usage.get(&root) {
            Some(u) => !u.is_assigned() && !u.has_field_mutation,
            None => true,
        },
        None => src.is_none_or(|e| !yields_subexpression(body, e)),
    }
}

/// Whether `e` takes its value from one of several arms (`if` / `match`) that
/// may be a live local, so a read-only binding of this rootless source must keep
/// its copy rather than alias the arm. A `block` / labeled-block is excluded: a
/// fresh single tail value is const-promoted and elided, so blocking it would
/// only keep redundant copies of constants.
fn yields_subexpression(body: &Body, e: ExprId) -> bool {
    matches!(body.exprs[e].kind, ExprKind::If { .. } | ExprKind::Match { .. })
}

/// Check whether `value` is a `$value_copy$T(arg)` call whose wrapper can be
/// safely stripped given the binding target's local index and the
/// function-wide usage map.
fn elision_safe(
    body: &Body,
    target_index: u32,
    target_assign_limit: u32,
    value: ExprId,
    value_copy_ids: &IndexSet<FuncId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> bool {
    is_target_read_only(target_index, target_assign_limit, usage)
        && copy_source_strippable(body, value, value_copy_ids, usage)
}

/// Collect `$value_copy$T(arg)` calls that sit directly in an aggregate literal
/// (`Struct { f: copy(arg) }` / `[copy(arg), …]`), descending through nested
/// literals. Each collected copy stores its source into a field/element of the
/// literal; the caller only descends here when the literal's binding local is
/// read-only, so those fields are never mutated and aliasing the (also read-only)
/// source is unobservable. Non-literal, non-copy positions (e.g. a nested call)
/// are not descended — a value crossing a call boundary can escape mutably.
fn collect_literal_element_copies(
    body: &Body,
    expr: ExprId,
    value_copy_ids: &IndexSet<FuncId>,
    usage: &IndexMap<u32, LocalUsage>,
    out: &mut Vec<ExprId>,
) {
    match &body.exprs[expr].kind {
        ExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                if let Some(fe) = field.value.as_expr() {
                    collect_literal_element_copies(body, fe, value_copy_ids, usage, out);
                }
            }
        }
        ExprKind::TupleLiteral { elements } => {
            for element in elements {
                if let Some(ee) = element.as_expr() {
                    collect_literal_element_copies(body, ee, value_copy_ids, usage, out);
                }
            }
        }
        _ if copy_source_strippable(body, expr, value_copy_ids, usage) => out.push(expr),
        _ => {}
    }
}

/// Replace the `$value_copy$T(arg)` call at `value` with its single argument,
/// in place. The call returns the argument's own type, so keeping `value`'s
/// `type_id` / `span` matches the old `*value = arg` rewrite; the orphaned
/// Return the `$value_copy$T(arg)` call expression of `stmt` when `stmt` binds /
/// assigns a read-only local to such a call (and is thus safe to unwrap). The
/// caller performs the unwrap via the engine edit API.
fn collect_strippable(
    body: &Body,
    stmt: StmtId,
    value_copy_ids: &IndexSet<FuncId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> Vec<ExprId> {
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
            let Some(ve) = value.as_expr() else {
                return vec![];
            };
            if *is_mut || *skip_value_copy {
                return vec![];
            }
            // `let x = $value_copy$T(arg)` — strip when both x and the source
            // are read-only.
            if elision_safe(body, *local_index, 0, ve, value_copy_ids, usage) {
                return vec![ve];
            }
            // `let c = Struct { f: $value_copy$T(arg), … }` — when the container
            // `c` is never reassigned or field-mutated its fields never change,
            // so the copies stored into them may be elided (source-side check
            // per copy).
            if is_target_read_only(*local_index, 0, usage) {
                let mut out = Vec::new();
                collect_literal_element_copies(body, ve, value_copy_ids, usage, &mut out);
                return out;
            }
            vec![]
        }
        // `x = $value_copy$T(arg)` top-level — the Assign *is* the binding.
        // Allow elision when this is the only assignment (`assign_count == 1`).
        StmtKind::Expr(Operand::Expr(e)) => {
            if let ExprKind::Assign { target, value } = &body.exprs[*e].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                && let Some(ve) = value.as_expr()
                && elision_safe(body, *index, 1, ve, value_copy_ids, usage)
            {
                vec![ve]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Call-argument wrapper stripping
// ──────────────────────────────────────────────────────────────────────────────

/// Walk `node`'s subtree collecting `$value_copy$T(arg)` calls in call-argument
/// positions whose wrapper is safe to strip: the callee only ever observes a
/// transient, read-only alias. Two independent grounds each make that so:
///
/// - the argument is a *fresh* rvalue (no source root) — copying a uniquely
///   owned value is a no-op, safe for any parameter, mutable or escaping; or
/// - the callee parameter is non-`mut` (read-only inside the callee) *and*
///   non-escaping (no alias of it outlives the call, per [`EscapeMap`]), with no
///   sibling `&mut` argument able to mutate the shared source during the call; or
/// - the source is a *move* — a parameter mentioned exactly once (this copy is
///   its last use). A parameter is the frame's private, unaliased copy, so
///   handing it to the callee instead of a clone is observably identical even
///   when the callee escapes it.
///
/// A deref-sourced argument is never stripped (wado-lang/wado#1522): the pointee
/// has no local identity the usage oracle tracks.
impl ValueCopyElideRule<'_> {
    /// Collect every `$value_copy$T(arg)` in `node`'s subtree whose `arg` is a
    /// fresh rvalue — the value semantics of the conservative copy inserted for a
    /// call result (and any construction) is a no-op when the source aliases
    /// nothing live, so it can be stripped in any position.
    fn collect_fresh_copies(&self, body: &Body, node: NodeRef, out: &mut Vec<ExprId>) {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if let NodeRef::Expr(e) = n
                && is_value_copy_call(body, e, self.value_copy_ids)
                && let Some(arg) = call_arg(body, e)
                && arg
                    .as_expr()
                    .is_some_and(|ae| self.escape.rvalue_is_fresh(body, ae, self.type_table))
            {
                out.push(e);
            }
            body.for_each_child(n, |c| stack.push(c));
        }
    }

    fn collect_call_arg_copies(&self, body: &Body, node: NodeRef, out: &mut Vec<ExprId>) {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if let NodeRef::Expr(e) = n {
                match &body.exprs[e].kind {
                    ExprKind::Call { func_id, args, .. } => {
                        self.scan_call_args(body, *func_id, 0, args, out);
                    }
                    // A method's parameter 0 is `self`; the i-th argument is
                    // absolute parameter i + 1.
                    ExprKind::MethodCall { func_id, args, .. } => {
                        self.scan_call_args(body, *func_id, 1, args, out);
                    }
                    _ => {}
                }
            }
            body.for_each_child(n, |c| stack.push(c));
        }
    }

    fn scan_call_args(
        &self,
        body: &Body,
        func_id: FuncId,
        param_offset: usize,
        args: &[crate::nir_arena::ArenaCallArg],
        out: &mut Vec<ExprId>,
    ) {
        // Source roots a sibling `&mut` argument may mutate while the call runs.
        // A `mut` by-value parameter takes its own copy, so it cannot; only a
        // `&mut` reference argument reaches the caller's value.
        let mut_roots: Vec<u32> = args
            .iter()
            .filter_map(|a| {
                let e = a.expr.as_expr()?;
                is_mut_ref_type(body.exprs[e].type_id, self.type_table)
                    .then(|| arg_source_root(body, e))
                    .flatten()
            })
            .collect();
        for (i, a) in args.iter().enumerate() {
            let Some(e) = a.expr.as_expr() else { continue };
            if !is_value_copy_call(body, e, self.value_copy_ids) {
                continue;
            }
            let Some(arg) = call_arg(body, e) else {
                continue;
            };
            // A promoted constant argument is uniquely owned — copying it is a
            // no-op, safe for any callee.
            let Some(ae) = arg.as_expr() else {
                out.push(e);
                continue;
            };
            if reads_through_deref(body, ae) {
                continue;
            }
            let root = arg_source_root(body, ae);
            // A fresh arg is stripped by `collect_fresh_copies` instead; this scan
            // needs only the two escape-aware grounds.
            //
            // Move: `root` is a parameter used only here, so this copy is its
            // last use of a value the frame uniquely owns.
            let is_move = root.is_some_and(|r| {
                r < self.n_params && self.usage.get(&r).map(|u| u.occurrences).unwrap_or(0) == 1
            });
            // Confined: a non-`mut`, non-escaping parameter, with no sibling
            // `&mut` able to mutate the shared value. With a known root, guard
            // that root; with an unknown root, any `&mut` sibling could alias it.
            let no_mut_alias = match root {
                Some(r) => !mut_roots.contains(&r),
                None => mut_roots.is_empty(),
            };
            let is_confined =
                !a.is_mut && !self.escape.param_escapes(func_id, param_offset + i) && no_mut_alias;
            if is_move || is_confined {
                out.push(e);
            }
        }
    }
}

/// The single argument operand of a `$value_copy$T(arg)` call.
fn call_arg(body: &Body, value_copy: ExprId) -> Option<Operand> {
    if let ExprKind::Call { args, .. } = &body.exprs[value_copy].kind {
        args.first().map(|a| a.expr)
    } else {
        None
    }
}
