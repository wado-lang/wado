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
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::arena_query::storage_root;
use super::escape::EscapeMap;
use super::value_copy::carries_identity;
use super::value_copy::mutation::{MutationOracle, Witness, expr_witnesses};

// ──────────────────────────────────────────────────────────────────────────────
// Access paths (field-sensitive storage locations)
// ──────────────────────────────────────────────────────────────────────────────

/// One projection step of an access path. `Index` is a dynamic wildcard: two
/// `Index` steps may address the same element, so they never *prove*
/// disjointness on their own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Selector {
    Field(u32),
    Variant(u32),
    Index,
}

/// A storage location as a root local plus a root-first chain of projection
/// steps. `self.index` is `AccessPath { root: self, selectors: [Field(index)] }`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct AccessPath {
    root: u32,
    selectors: Vec<Selector>,
}

/// The access path of a pure place expression, or `None` when it is not a
/// local-rooted place: a `Deref` (the pointee has no local identity — this
/// preserves the [`reads_through_deref`] guard, #1522), a call/composition
/// result, or a projection off one. `Cast` and non-`Deref` `Unary` are
/// transparent (they do not move storage).
fn place_path(body: &Body, expr: ExprId) -> Option<AccessPath> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(AccessPath {
            root: *index,
            selectors: Vec::new(),
        }),
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let mut p = place_path(body, inner.as_expr()?)?;
            p.selectors.push(Selector::Field(*field_index));
            Some(p)
        }
        ExprKind::VariantPayload {
            expr: inner,
            case_index,
            ..
        } => {
            let mut p = place_path(body, inner.as_expr()?)?;
            p.selectors.push(Selector::Variant(*case_index));
            Some(p)
        }
        ExprKind::Index { expr: inner, .. } => {
            let mut p = place_path(body, inner.as_expr()?)?;
            p.selectors.push(Selector::Index);
            Some(p)
        }
        ExprKind::Cast { expr: inner, .. } => place_path(body, inner.as_expr()?),
        ExprKind::Unary { op, expr: inner } if *op != NirUnaryOp::Deref => {
            place_path(body, inner.as_expr()?)
        }
        _ => None,
    }
}

/// Whether a write to `mutated` can never change the value read at `read`
/// (both rooted at the same local). True only when the paths provably address
/// non-overlapping storage: they agree on a shared prefix and then diverge at
/// two distinct struct fields or two distinct variant cases. A dynamic `Index`
/// step (may collide), a mixed selector kind, or one path being a prefix of the
/// other (ancestor/descendant overlap) all yield `false` — conservative.
fn disjoint(mutated: &AccessPath, read: &AccessPath) -> bool {
    for (a, b) in mutated.selectors.iter().zip(read.selectors.iter()) {
        match (a, b) {
            // Distinct struct fields or variant cases prove disjointness.
            (Selector::Field(x), Selector::Field(y)) if x != y => return true,
            (Selector::Variant(x), Selector::Variant(y)) if x != y => return true,
            // Same field / same variant / two dynamic indices: compatible so far,
            // keep comparing deeper selectors.
            (Selector::Field(_), Selector::Field(_))
            | (Selector::Variant(_), Selector::Variant(_))
            | (Selector::Index, Selector::Index) => {}
            // Mixed selector kinds on the same storage cannot arise for two paths
            // that share a prefix (the type at each depth is fixed); treat any as
            // conservatively overlapping.
            (Selector::Field(_), Selector::Variant(_) | Selector::Index)
            | (Selector::Variant(_), Selector::Field(_) | Selector::Index)
            | (Selector::Index, Selector::Field(_) | Selector::Variant(_)) => return false,
        }
    }
    false
}

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
    param_mut: &'a IndexMap<FuncId, Vec<bool>>,
    n_params: u32,
    usage: UsageInfo,
}

impl<'a> ValueCopyElideRule<'a> {
    pub(super) fn new(
        value_copy_ids: &'a IndexSet<FuncId>,
        escape: &'a EscapeMap,
        type_table: &'a TypeTable,
        param_mut: &'a IndexMap<FuncId, Vec<bool>>,
        n_params: u32,
        usage: UsageInfo,
    ) -> Self {
        Self {
            value_copy_ids,
            escape,
            type_table,
            param_mut,
            n_params,
            usage,
        }
    }
}

/// Per-function usage facts plus the alias relation they were closed
/// over: `has_field_mutation` marks every member of an alias component,
/// and `alias_rep` answers "may these two locals share storage".
pub(super) struct UsageInfo {
    locals: IndexMap<u32, LocalUsage>,
    alias_rep: IndexMap<u32, u32>,
    /// Locals whose `direct_mutated_paths` is the complete mutation profile of
    /// their storage: the sole direct mutator in their alias component and free
    /// of imprecise mutation. Only these may use field-sensitive disjointness;
    /// every other local falls back to the coarse `has_field_mutation` bit.
    precise_eligible: IndexSet<u32>,
}

impl UsageInfo {
    fn local(&self, index: u32) -> Option<&LocalUsage> {
        self.locals.get(&index)
    }

    fn rep(&self, index: u32) -> u32 {
        self.alias_rep.get(&index).copied().unwrap_or(index)
    }

    fn may_alias(&self, a: u32, b: u32) -> bool {
        a == b || self.rep(a) == self.rep(b)
    }

    fn is_precise_eligible(&self, index: u32) -> bool {
        self.precise_eligible.contains(&index)
    }
}

/// Build the per-function usage map a [`ValueCopyElideRule`] needs, from the
/// pristine body before the engine session rewrites it.
pub(super) fn build_usage(
    body: &Body,
    type_table: &TypeTable,
    oracle: &MutationOracle<'_>,
    escape: &EscapeMap,
) -> UsageInfo {
    analyze_usage(body, type_table, oracle, escape)
}

/// Per-parameter `&mut`-ness of every callee, keyed by id: bit `i` is `true`
/// when parameter `i` is a `&mut T` borrow — the only kind that can mutate the
/// caller's argument storage (a `&T` cannot be written through). `NirParam`
/// records this before boxing collapses `&mut T` / `&T` onto the same
/// `Box<T>`, so it stays precise where the parameter type no longer is. A
/// callee absent from the map is bodyless (its argument mutability falls back
/// to the argument expression's own type at the call site).
pub(super) fn build_param_mut(project: &NirPackage) -> IndexMap<FuncId, Vec<bool>> {
    let mut map = IndexMap::default();
    for func in &project.functions {
        let func = func.borrow();
        // Only bodied functions are addressable callees whose parameter
        // metadata we can trust here; bodyless callees stay absent.
        if func.body.is_none() {
            continue;
        }
        let Some(id) = func.id else { continue };
        map.insert(id, func.params.iter().map(|p| p.is_mut_ref).collect());
    }
    map
}

impl Rule for ValueCopyElideRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if self.value_copy_ids.is_empty() {
            return false;
        }
        let stmts = engine.body.blocks[block].stmts.clone();
        let mut changed = false;
        for stmt in stmts {
            let mut targets = self.collect_strippable(engine.body, stmt);
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
    /// Count of `local = expr` assignments. `copy_source_strippable` uses
    /// [`Self::is_assigned`] to refuse aliasing a *source* root that is
    /// itself ever reassigned: the flat, flow-insensitive usage map cannot
    /// tell whether a given read sees the value before or after such a
    /// reassignment, so a reassigned source is conservatively never treated
    /// as stable enough to alias into.
    assign_count: u32,
    has_field_mutation: bool,
    /// Count of every `Local` occurrence (reads and assignment targets). A
    /// count of 1 means the local is mentioned exactly once, so a copy of it at
    /// that single site is its last use — a move.
    occurrences: u32,
    /// The access paths this local is *directly* mutated at (its own writes /
    /// `&mut` borrows / mutating call arguments), before alias closure. The
    /// field-sensitive source-side check consults these to strip a copy whose
    /// read path is disjoint from every mutation — but only when this local is
    /// the sole direct mutator in its alias component (see
    /// [`UsageInfo::precise_eligible`]), so this set is the complete mutation
    /// profile.
    direct_mutated_paths: Vec<AccessPath>,
    /// Set alongside every direct mutation (precise or not); drives the
    /// per-component "sole mutator" test.
    direct_field_mutation: bool,
    /// A direct mutation whose exact path could not be captured (a `Deref`
    /// target, a computed receiver, a literal-wrapped `&mut`). Disables the
    /// precise path check for this local — `direct_mutated_paths` is then
    /// incomplete, so it falls back to the coarse `has_field_mutation` bit.
    imprecise_mutation: bool,
}

impl LocalUsage {
    /// True when the local is assigned at least once after its
    /// initialization. See the field doc on [`Self::assign_count`].
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
    oracle: &MutationOracle<'_>,
    escape: &EscapeMap,
) -> UsageInfo {
    let mut usage: IndexMap<u32, LocalUsage> = IndexMap::default();
    let mut alias_edges: Vec<(u32, u32)> = Vec::new();
    let aliasing = AliasWalk {
        body,
        type_table,
        escape,
    };
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Expr(id) => {
                classify_expr(body, id, type_table, oracle, &mut usage);
                aliasing.collect_expr_alias_edge(id, &mut alias_edges);
            }
            NodeRef::Stmt(id) => aliasing.collect_stmt_alias_edge(id, &mut alias_edges),
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    let alias_rep = alias_components(&alias_edges);
    close_alias_mutation(&alias_rep, &mut usage);
    let precise_eligible = compute_precise_eligible(&alias_rep, &usage);
    UsageInfo {
        locals: usage,
        alias_rep,
        precise_eligible,
    }
}

/// A local may use field-sensitive disjointness only when its
/// `direct_mutated_paths` is the *complete* mutation profile of its storage:
/// it mutates itself directly, none of those mutations were imprecise, and it
/// is the sole direct mutator in its alias component (so no sibling's mutation
/// — whose relative offset the union-find edges don't record — reaches it).
fn compute_precise_eligible(
    alias_rep: &IndexMap<u32, u32>,
    usage: &IndexMap<u32, LocalUsage>,
) -> IndexSet<u32> {
    let rep_of = |local: u32| alias_rep.get(&local).copied().unwrap_or(local);
    let mut mutators_by_rep: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    for (local, u) in usage {
        if u.direct_field_mutation {
            mutators_by_rep
                .entry(rep_of(*local))
                .or_default()
                .insert(*local);
        }
    }
    let mut eligible = IndexSet::default();
    for (local, u) in usage {
        if u.direct_field_mutation
            && !u.imprecise_mutation
            && mutators_by_rep
                .get(&rep_of(*local))
                .is_some_and(|s| s.len() == 1)
        {
            eligible.insert(*local);
        }
    }
    eligible
}

/// Union-find over the undirected alias relation; returns each connected
/// local's component representative.
fn alias_components(edges: &[(u32, u32)]) -> IndexMap<u32, u32> {
    let mut parent: IndexMap<u32, u32> = IndexMap::default();
    fn find(parent: &mut IndexMap<u32, u32>, x: u32) -> u32 {
        let p = *parent.entry(x).or_insert(x);
        if p == x {
            return x;
        }
        let root = find(parent, p);
        parent.insert(x, root);
        root
    }
    for &(a, b) in edges {
        let ra = find(&mut parent, a);
        let rb = find(&mut parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let keys: Vec<u32> = parent.keys().copied().collect();
    for k in keys {
        find(&mut parent, k);
    }
    parent
}

/// Close `has_field_mutation` over the alias components. The closure must
/// be symmetric: a sibling pattern binding shares the payload with the
/// scrutinee, so a mutation through either reaches both.
fn close_alias_mutation(alias_rep: &IndexMap<u32, u32>, usage: &mut IndexMap<u32, LocalUsage>) {
    let mut mutated_reps: IndexSet<u32> = IndexSet::default();
    for (local, rep) in alias_rep {
        if usage.get(local).is_some_and(|u| u.has_field_mutation) {
            mutated_reps.insert(*rep);
        }
    }
    for (local, rep) in alias_rep {
        if mutated_reps.contains(rep) {
            usage.entry(*local).or_default().has_field_mutation = true;
        }
    }
}

/// Alias-edge collection over the pristine body. Uses the escape map so a
/// call whose result may alias its inputs contributes edges from its
/// receiver / arguments.
struct AliasWalk<'a> {
    body: &'a Body,
    type_table: &'a TypeTable,
    escape: &'a EscapeMap,
}

impl AliasWalk<'_> {
    /// Record `alias local → root local` for a `let x = <projection of r>;`
    /// (pattern lowering chains such temps from scrutinee to binding).
    fn collect_stmt_alias_edge(&self, stmt: StmtId, edges: &mut Vec<(u32, u32)>) {
        match &self.body.stmts[stmt].kind {
            StmtKind::Let {
                local_index, value, ..
            } => {
                for root in self.value_alias_roots(*value) {
                    if root != *local_index {
                        edges.push((*local_index, root));
                    }
                }
            }
            StmtKind::LetDestructure { pattern, value, .. } => {
                for root in self.value_alias_roots(*value) {
                    self.collect_pattern_binding_edges(*pattern, root, edges);
                }
            }
            StmtKind::Expr(_)
            | StmtKind::Return { .. }
            | StmtKind::Break { .. }
            | StmtKind::Continue
            | StmtKind::If { .. }
            | StmtKind::Loop { .. }
            | StmtKind::LabeledBlock { .. } => {}
        }
    }

    /// [`Self::collect_stmt_alias_edge`] for the `x = <projection of r>`
    /// form and for `match` arms, whose pattern bindings alias the
    /// scrutinee's payload storage.
    fn collect_expr_alias_edge(&self, id: ExprId, edges: &mut Vec<(u32, u32)>) {
        match &self.body.exprs[id].kind {
            ExprKind::Assign { target, value } => {
                if let ExprKind::Local { index, .. } = &self.body.exprs[*target].kind {
                    for root in self.value_alias_roots(*value) {
                        if root != *index {
                            edges.push((*index, root));
                        }
                    }
                }
            }
            ExprKind::Match { expr, arms } => {
                for root in self.value_alias_roots(*expr) {
                    for arm in arms {
                        self.collect_pattern_binding_edges(arm.pattern, root, edges);
                    }
                }
            }
            _ => {}
        }
    }

    /// Every local whose storage the value of a binding may share: the
    /// value's projection roots, plus roots stored into aggregate
    /// literals, `if` / `match` arm values, and the receiver / arguments
    /// of a call whose result is not provably fresh.
    fn value_alias_roots(&self, value: Operand) -> Vec<u32> {
        let body = self.body;
        let mut roots = Vec::new();
        let Some(e) = value.as_expr() else {
            return roots;
        };
        let mut stack = vec![e];
        while let Some(e) = stack.pop() {
            match &body.exprs[e].kind {
                // Only identity-carrying locals root the value — a scalar
                // cannot alias the copied aggregate.
                ExprKind::Local { index, .. } => {
                    let ty = body.exprs[e].type_id;
                    if carries_identity(ty, self.type_table)
                        || self.type_table.box_payload_of(ty).is_some()
                    {
                        roots.push(*index);
                    }
                }
                ExprKind::FieldAccess { expr: inner, .. }
                | ExprKind::VariantPayload { expr: inner, .. }
                | ExprKind::Cast { expr: inner, .. }
                | ExprKind::Unary { expr: inner, .. }
                | ExprKind::Index { expr: inner, .. } => {
                    if let Some(ie) = inner.as_expr() {
                        stack.push(ie);
                    }
                }
                _ => self.push_composition_children(e, &mut stack),
            }
        }
        roots
    }

    /// The composition arms shared by [`Self::value_alias_roots`] and
    /// [`Self::value_alias_read_paths`]: an aggregate literal, `if` / `match` /
    /// `switch` arm values, block tails, and the receiver / arguments of a call
    /// whose result is not provably fresh. Neither identifies a single place, so
    /// the walk descends into their value-position children.
    fn push_composition_children(&self, e: ExprId, stack: &mut Vec<ExprId>) {
        let body = self.body;
        match &body.exprs[e].kind {
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    if let Some(fe) = field.value.as_expr() {
                        stack.push(fe);
                    }
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    if let Some(ee) = element.as_expr() {
                        stack.push(ee);
                    }
                }
            }
            ExprKind::VariantConstruct {
                payload: Some(payload),
                ..
            } => {
                if let Some(pe) = payload.as_expr() {
                    stack.push(pe);
                }
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.push_block_tail_expr(*then_branch, stack);
                if let Some(eb) = else_branch {
                    self.push_block_tail_expr(*eb, stack);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    if let Some(ae) = arm.body.as_expr() {
                        stack.push(ae);
                    }
                }
            }
            ExprKind::Switch {
                arms,
                default: dflt,
                ..
            } => {
                for arm in arms {
                    self.push_block_tail_expr(*arm, stack);
                }
                self.push_block_tail_expr(*dflt, stack);
            }
            ExprKind::Block(block) => {
                self.push_block_tail_expr(*block, stack);
            }
            ExprKind::LabeledBlock { block, .. } => {
                self.push_block_value_exprs(*block, stack);
            }
            ExprKind::Call { args, .. } => {
                if !self.escape.rvalue_is_fresh(body, e, self.type_table) {
                    for arg in args {
                        if let Some(ae) = arg.expr.as_expr() {
                            stack.push(ae);
                        }
                    }
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                if !self.escape.rvalue_is_fresh(body, e, self.type_table) {
                    if let Some(re) = receiver.as_expr() {
                        stack.push(re);
                    }
                    for arg in args {
                        if let Some(ae) = arg.expr.as_expr() {
                            stack.push(ae);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The field-sensitive companion to [`Self::value_alias_roots`]: every
    /// identity-carrying storage location the value may alias, as a full access
    /// path (root + selectors) rather than a bare root local. A pure place is
    /// captured whole (its path recorded, no further descent); a projection over
    /// a composition (`foo().bar`) and every composition descends exactly as in
    /// `value_alias_roots`, so the set of roots covered is identical — only the
    /// selector detail is added.
    fn value_alias_read_paths(&self, value: Operand) -> Vec<AccessPath> {
        let body = self.body;
        let mut paths = Vec::new();
        let Some(e0) = value.as_expr() else {
            return paths;
        };
        let mut stack = vec![e0];
        while let Some(e) = stack.pop() {
            if let Some(ap) = place_path(body, e) {
                let ty = body.exprs[e].type_id;
                if carries_identity(ty, self.type_table)
                    || self.type_table.box_payload_of(ty).is_some()
                {
                    paths.push(ap);
                }
                continue;
            }
            match &body.exprs[e].kind {
                ExprKind::FieldAccess { expr: inner, .. }
                | ExprKind::VariantPayload { expr: inner, .. }
                | ExprKind::Cast { expr: inner, .. }
                | ExprKind::Unary { expr: inner, .. }
                | ExprKind::Index { expr: inner, .. } => {
                    if let Some(ie) = inner.as_expr() {
                        stack.push(ie);
                    }
                }
                _ => self.push_composition_children(e, &mut stack),
            }
        }
        paths
    }

    /// Push the tail expression of a block (its value position) for the
    /// alias walk.
    fn push_block_tail_expr(&self, block: BlockId, stack: &mut Vec<ExprId>) {
        if let Some(last) = self.body.blocks[block].stmts.last()
            && let StmtKind::Expr(op) = &self.body.stmts[*last].kind
            && let Some(e) = op.as_expr()
        {
            stack.push(e);
        }
    }

    /// Push every value a labeled block can yield: each `break … <value>`
    /// inside it (any label — conservative) plus the tail expression.
    fn push_block_value_exprs(&self, block: BlockId, stack: &mut Vec<ExprId>) {
        let mut nodes = vec![NodeRef::Block(block)];
        while let Some(n) = nodes.pop() {
            if let NodeRef::Stmt(s) = n
                && let StmtKind::Break { value: Some(v), .. } = &self.body.stmts[s].kind
                && let Some(ve) = v.as_expr()
            {
                stack.push(ve);
            }
            self.body.for_each_child(n, |c| nodes.push(c));
        }
        self.push_block_tail_expr(block, stack);
    }

    /// Collect an `alias local → scrutinee root` edge for every local a
    /// pattern binds: a binding shares the matched value's storage.
    fn collect_pattern_binding_edges(&self, pat: PatId, root: u32, edges: &mut Vec<(u32, u32)>) {
        match &self.body.pats[pat].kind {
            PatKind::Binding { local_index, .. } => {
                if *local_index != root {
                    edges.push((*local_index, root));
                }
            }
            PatKind::Tuple(subs, _) | PatKind::Or(subs) => {
                for sub in subs {
                    self.collect_pattern_binding_edges(*sub, root, edges);
                }
            }
            PatKind::Variant { bindings, .. } => {
                for sub in bindings {
                    self.collect_pattern_binding_edges(*sub, root, edges);
                }
            }
            PatKind::Struct { fields, .. } => {
                for field in fields {
                    self.collect_pattern_binding_edges(field.pattern, root, edges);
                }
            }
            PatKind::Wildcard
            | PatKind::Literal(_)
            | PatKind::Enum { .. }
            | PatKind::ConstantValue { .. }
            | PatKind::Range { .. } => {}
        }
    }
}

/// Apply the usage-marking rules for a single expression node. No recursion:
/// the caller's walk visits every node.
fn classify_expr(
    body: &Body,
    id: ExprId,
    type_table: &TypeTable,
    oracle: &MutationOracle<'_>,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    if let ExprKind::Local { index, .. } = &body.exprs[id].kind {
        usage.entry(*index).or_default().occurrences += 1;
    }
    // A direct field/index/variant write. The shared `Witness::Write` carries
    // only the operand *under* the outer projection, so mark the assignment
    // target place here to keep the outer selector (a whole-local target is a
    // `Rebind`, handled below).
    if let ExprKind::Assign { target, .. } = &body.exprs[id].kind
        && matches!(
            &body.exprs[*target].kind,
            ExprKind::FieldAccess { .. } | ExprKind::Index { .. } | ExprKind::VariantPayload { .. }
        )
    {
        mark_mutation(body, *target, type_table, usage);
    }
    expr_witnesses(body, id, oracle, &mut |w| match w {
        Witness::Rebind(index) => {
            usage.entry(index).or_default().assign_count += 1;
        }
        // Marked from the assignment target place above; the witness's inner
        // operand has already dropped the outer selector.
        Witness::Write(_) => {}
        Witness::MutBorrow(e) => mark_mutation(body, e, type_table, usage),
        Witness::CalleeArg {
            expr: ae,
            verdict,
            is_mut,
        } => {
            if verdict.unwrap_or_else(|| {
                is_mut || is_mutable_witness_type(body.exprs[ae].type_id, type_table)
            }) {
                mark_mutation(body, ae, type_table, usage);
            }
        }
        // Auto-ref hides a `&mut self` receiver's mode, so an unknown callee is
        // assumed to mutate.
        Witness::Receiver { expr: re, verdict } => {
            if verdict.unwrap_or(true) {
                mark_mutation(body, re, type_table, usage);
            }
        }
        Witness::IndirectArg(ae) => {
            if is_mutable_witness_type(body.exprs[ae].type_id, type_table) {
                mark_mutation(body, ae, type_table, usage);
            }
        }
    });
}

/// Mark a direct mutation at `place`, recording both the coarse
/// `has_field_mutation` bit (the binding-side gate and the field-insensitive
/// fallback) and, when `place` is a local-rooted place, its exact access path
/// for the field-sensitive source-side check. A whole-local place (empty
/// selector path, e.g. `&mut self`) is precise and correctly blocks every read
/// under that root via the prefix rule; a place that is not local-rooted (a
/// `Deref`, computed receiver, or literal-wrapped `&mut`) is marked imprecise
/// so its root falls back to the coarse bit.
fn mark_mutation(
    body: &Body,
    place: ExprId,
    type_table: &TypeTable,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    match place_path(body, place) {
        Some(ap) => {
            let u = usage.entry(ap.root).or_default();
            u.has_field_mutation = true;
            u.direct_field_mutation = true;
            u.direct_mutated_paths.push(ap);
        }
        None => mark_roots_mutated(body, place, type_table, usage),
    }
}

/// [`mark_mutation`] for a place with no single local-rooted access path:
/// follow pure projections (`FieldAccess`, `VariantPayload`, `Cast`, `Unary`,
/// `Index`) and, conservatively, a `MethodCall` receiver and identity-carrying
/// literal elements, marking every root field-mutated *and* imprecise.
/// Imprecise disables the field-sensitive check (the exact mutated path is
/// unknown), leaving the root on the coarse bit; since that bit only ever
/// *blocks* elision, over-approximating here costs a missed optimization, never
/// an unsound alias.
///
/// `List<T>::index_value(i)` (raw `x[i]` before `inline` expands the trait
/// call) returns storage aliased into the receiver but arrives as an opaque
/// `MethodCall`, so recursing into every receiver errs toward marking too much.
fn mark_roots_mutated(
    body: &Body,
    expr: ExprId,
    type_table: &TypeTable,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => {
            let u = usage.entry(*index).or_default();
            u.has_field_mutation = true;
            u.direct_field_mutation = true;
            u.imprecise_mutation = true;
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => {
            mark_roots_mutated_operand(body, *inner, type_table, usage);
        }
        ExprKind::MethodCall { receiver, .. } => {
            mark_roots_mutated_operand(body, *receiver, type_table, usage);
        }
        // A literal in a mutation-witness position (`Box { value: a }`)
        // wraps its sources; only identity-carrying elements are shared.
        ExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                mark_identity_root_mutated(body, field.value, type_table, usage);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for element in elements {
                mark_identity_root_mutated(body, *element, type_table, usage);
            }
        }
        ExprKind::VariantConstruct {
            payload: Some(payload),
            ..
        } => {
            mark_identity_root_mutated(body, *payload, type_table, usage);
        }
        _ => {}
    }
}

/// [`mark_roots_mutated`] for an operand.
fn mark_roots_mutated_operand(
    body: &Body,
    op: Operand,
    type_table: &TypeTable,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    if let Some(e) = op.as_expr() {
        mark_roots_mutated(body, e, type_table, usage);
    }
}

/// [`mark_roots_mutated`] for an aggregate-literal element, applied only when
/// the element's type can carry the enclosing value's identity.
fn mark_identity_root_mutated(
    body: &Body,
    op: Operand,
    type_table: &TypeTable,
    usage: &mut IndexMap<u32, LocalUsage>,
) {
    if let Some(e) = op.as_expr()
        && (carries_identity(body.exprs[e].type_id, type_table)
            || type_table.box_payload_of(body.exprs[e].type_id).is_some())
    {
        mark_roots_mutated(body, e, type_table, usage);
    }
}

/// Whether an argument of this type can carry a mutation into the
/// caller's storage: a `&mut T`, or a `Box<T>` whose payload carries
/// identity — the boxing plan lowers `&` and `&mut` to the same `Box<T>`
/// shape, erasing the mutability witness.
fn is_mutable_witness_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    if is_mut_ref_type(type_id, type_table) {
        return true;
    }
    type_table
        .box_payload_of(type_id)
        .is_some_and(|payload| carries_identity(payload, type_table))
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

/// Whether the local `target_index` is read-only enough to alias a source
/// into: never field-mutated (a whole-value rebind is fine at any count — a
/// bare reassignment replaces which object the local refers to, it never
/// touches the object a prior alias still points at; only an in-place field
/// mutation could make the alias observable). The alias a strip creates is
/// unobservable only when this holds for both ends (the binding side here,
/// the source side in [`copy_source_strippable`]).
fn is_target_read_only(target_index: u32, usage: &UsageInfo) -> bool {
    match usage.local(target_index) {
        Some(u) => !u.has_field_mutation,
        None => true,
    }
}

/// Whether `value` is a `$value_copy$T(arg)` call whose *source* side is safe
/// to alias: the arg does not read through a reference deref
/// (wado-lang/wado#1522), and every local whose storage the source may yield
/// (its alias roots — the projection root, or each arm value / break value of
/// an `if` / `match` / `switch` / labeled block) is never reassigned nor
/// field-mutated. A source with no roots yields only fresh values, so the
/// copy is a no-op. The binding/consumer side is checked separately by the
/// caller.
impl ValueCopyElideRule<'_> {
    fn copy_source_strippable(&self, body: &Body, value: ExprId) -> bool {
        if !is_value_copy_call(body, value, self.value_copy_ids) {
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
        let aliasing = AliasWalk {
            body,
            type_table: self.type_table,
            escape: self.escape,
        };
        aliasing
            .value_alias_read_paths(arg.expr)
            .iter()
            .all(|read| self.read_path_stable(read))
    }

    /// Whether a source aliasing storage at `read` observes no mutation of that
    /// storage. Field-insensitive when the root is not `precise_eligible`
    /// (unchanged: reject any reassignment or field mutation); field-sensitive
    /// otherwise (accept when every direct mutation of the root is at a path
    /// provably disjoint from `read`, e.g. `self.index` vs `self.repr`).
    fn read_path_stable(&self, read: &AccessPath) -> bool {
        let Some(u) = self.usage.local(read.root) else {
            return true;
        };
        if u.is_assigned() {
            return false;
        }
        if !u.has_field_mutation {
            return true;
        }
        self.usage.is_precise_eligible(read.root)
            && u.direct_mutated_paths.iter().all(|m| disjoint(m, read))
    }

    /// Check whether `value` is a `$value_copy$T(arg)` call whose wrapper can
    /// be safely stripped given the binding target's local index and the
    /// function-wide usage map.
    fn elision_safe(&self, body: &Body, target_index: u32, value: ExprId) -> bool {
        is_target_read_only(target_index, &self.usage) && self.copy_source_strippable(body, value)
    }

    /// Collect `$value_copy$T(arg)` calls that sit directly in an aggregate
    /// literal (`Struct { f: copy(arg) }` / `[copy(arg), …]`), descending
    /// through nested literals. Each collected copy stores its source into a
    /// field/element of the literal; the caller only descends here when the
    /// literal's binding local is read-only, so those fields are never mutated
    /// and aliasing the (also read-only) source is unobservable. Non-literal,
    /// non-copy positions (e.g. a nested call) are not descended — a value
    /// crossing a call boundary can escape mutably.
    fn collect_literal_element_copies(&self, body: &Body, expr: ExprId, out: &mut Vec<ExprId>) {
        match &body.exprs[expr].kind {
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    if let Some(fe) = field.value.as_expr() {
                        self.collect_literal_element_copies(body, fe, out);
                    }
                }
            }
            ExprKind::TupleLiteral { elements } => {
                for element in elements {
                    if let Some(ee) = element.as_expr() {
                        self.collect_literal_element_copies(body, ee, out);
                    }
                }
            }
            _ if self.copy_source_strippable(body, expr) => out.push(expr),
            _ => {}
        }
    }

    /// Return the `$value_copy$T(arg)` call expressions of `stmt` that bind /
    /// assign a read-only local to such a call (and are thus safe to unwrap).
    /// The caller performs the unwrap via the engine edit API.
    fn collect_strippable(&self, body: &Body, stmt: StmtId) -> Vec<ExprId> {
        match &body.stmts[stmt].kind {
            // `let x = $value_copy$T(arg)` — a later whole-value reassignment
            // of `x` does not itself defeat the alias (it only replaces which
            // object `x` refers to; see `is_target_read_only`), so only field
            // mutation and `skip_value_copy` block this.
            StmtKind::Let {
                local_index,
                value,
                skip_value_copy,
                ..
            } => {
                let Some(ve) = value.as_expr() else {
                    return vec![];
                };
                if *skip_value_copy {
                    return vec![];
                }
                // `let x = $value_copy$T(arg)` — strip when both x and the
                // source are read-only.
                if self.elision_safe(body, *local_index, ve) {
                    return vec![ve];
                }
                // `let c = Struct { f: $value_copy$T(arg), … }` — a never
                // field-mutated container's element copies may be elided.
                if is_target_read_only(*local_index, &self.usage) {
                    let mut out = Vec::new();
                    self.collect_literal_element_copies(body, ve, &mut out);
                    return out;
                }
                vec![]
            }
            // `x = $value_copy$T(arg)` top-level — the Assign *is* the
            // binding. Any number of other reassignments of `x` elsewhere are
            // fine too (see `is_target_read_only`).
            StmtKind::Expr(Operand::Expr(e)) => {
                if let ExprKind::Assign { target, value } = &body.exprs[*e].kind
                    && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                    && let Some(ve) = value.as_expr()
                    && self.elision_safe(body, *index, ve)
                {
                    vec![ve]
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
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
                        self.scan_call_args(body, *func_id, 0, None, args, out);
                    }
                    // Method parameter 0 is `self`; argument `i` is absolute
                    // parameter `i + 1`.
                    ExprKind::MethodCall {
                        func_id,
                        receiver,
                        args,
                        ..
                    } => {
                        self.scan_call_args(body, *func_id, 1, Some(*receiver), args, out);
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
        receiver: Option<Operand>,
        args: &[crate::nir_arena::ArenaCallArg],
        out: &mut Vec<ExprId>,
    ) {
        // Source roots a sibling mutable argument may mutate while the call
        // runs. A `mut` by-value parameter takes its own copy, so it cannot;
        // a `&mut` reference argument reaches the caller's value, and so does
        // a boxed replace-on-assign argument (the boxing rewrite erases the
        // `&mut` and may wrap the value in a `Box { value: root }` literal,
        // so roots are collected through literals with the multi-root walk).
        let aliasing = AliasWalk {
            body,
            type_table: self.type_table,
            escape: self.escape,
        };
        let oracle = MutationOracle::new(self.param_mut);
        // A mutating receiver is a sibling mutation for the arguments.
        let receiver_roots = receiver
            .filter(|_| oracle.receiver_mutates(func_id).unwrap_or(true))
            .map(|re| aliasing.value_alias_roots(re))
            .unwrap_or_default();
        let mut_roots: Vec<u32> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let e = a.expr.as_expr()?;
                let mutates = match oracle.arg_mutates(func_id, param_offset + i) {
                    Some(m) => m,
                    None => is_mutable_witness_type(body.exprs[e].type_id, self.type_table),
                };
                mutates.then(|| aliasing.value_alias_roots(a.expr))
            })
            .flatten()
            .chain(receiver_roots)
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
            let root = storage_root(body, ae);
            // A fresh arg is stripped by `collect_fresh_copies` instead; this scan
            // needs only the two escape-aware grounds.
            //
            // Move: `root` is a parameter used only here, so this copy is its
            // last use of a value the frame uniquely owns.
            let is_move = root.is_some_and(|r| {
                r < self.n_params && self.usage.local(r).map(|u| u.occurrences).unwrap_or(0) == 1
            });
            // Confined: a non-`mut`, non-escaping parameter, with no sibling
            // mutable argument able to mutate the shared value during the
            // call. Roots compare through the alias components — a pattern
            // binding of `b`'s payload passed beside `&mut b` shares b's
            // storage even though the local indices differ. With an unknown
            // root, any mutable sibling could alias it.
            let no_mut_alias = match root {
                Some(r) => !mut_roots.iter().any(|m| self.usage.may_alias(*m, r)),
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

#[cfg(test)]
mod tests {
    use super::{AccessPath, Selector, disjoint};

    fn path(root: u32, selectors: &[Selector]) -> AccessPath {
        AccessPath {
            root,
            selectors: selectors.to_vec(),
        }
    }

    #[test]
    fn distinct_fields_are_disjoint() {
        // self.index (mutated) vs self.repr (read): the ArrayRefIter case.
        let mutated = path(0, &[Selector::Field(1)]);
        let read = path(0, &[Selector::Field(0)]);
        assert!(disjoint(&mutated, &read));
        assert!(disjoint(&read, &mutated));
    }

    #[test]
    fn same_field_is_not_disjoint() {
        let mutated = path(0, &[Selector::Field(0)]);
        let read = path(0, &[Selector::Field(0)]);
        assert!(!disjoint(&mutated, &read));
    }

    #[test]
    fn prefix_is_not_disjoint() {
        // Mutating self.a overlaps a read of self.a.b, and vice versa.
        let ancestor = path(0, &[Selector::Field(0)]);
        let descendant = path(0, &[Selector::Field(0), Selector::Field(3)]);
        assert!(!disjoint(&ancestor, &descendant));
        assert!(!disjoint(&descendant, &ancestor));
    }

    #[test]
    fn whole_root_overlaps_everything() {
        let whole = path(0, &[]);
        let field = path(0, &[Selector::Field(2)]);
        assert!(!disjoint(&whole, &field));
        assert!(!disjoint(&field, &whole));
    }

    #[test]
    fn distinct_fields_diverge_after_shared_prefix() {
        // self.a.x vs self.a.y: agree on `a`, diverge at distinct fields.
        let mutated = path(0, &[Selector::Field(0), Selector::Field(1)]);
        let read = path(0, &[Selector::Field(0), Selector::Field(2)]);
        assert!(disjoint(&mutated, &read));
    }

    #[test]
    fn dynamic_index_never_proves_disjoint() {
        // a[i] vs a[j] may collide.
        let mutated = path(0, &[Selector::Index]);
        let read = path(0, &[Selector::Index]);
        assert!(!disjoint(&mutated, &read));
    }

    #[test]
    fn distinct_field_after_shared_index_is_disjoint() {
        // a[i].x vs a[j].y: even the same element's distinct fields don't alias.
        let mutated = path(0, &[Selector::Index, Selector::Field(1)]);
        let read = path(0, &[Selector::Index, Selector::Field(2)]);
        assert!(disjoint(&mutated, &read));
    }

    #[test]
    fn distinct_variants_are_disjoint() {
        let mutated = path(0, &[Selector::Variant(0)]);
        let read = path(0, &[Selector::Variant(1)]);
        assert!(disjoint(&mutated, &read));
    }
}
