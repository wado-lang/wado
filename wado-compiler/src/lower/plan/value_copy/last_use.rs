//! Move and share eligibility for the value-copy fold (WEP 2026-05-21), from one
//! backward liveness walk per body plus small auxiliary scans. A handler or
//! `resume` reads twice, so skips.

use super::analyze::is_owned_value;
use super::funcset::FuncKeySet;
use super::is_reference_type;
use super::ownership::OwnedCalls;
use super::stores::StoredParams;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::place::field_owner;
use crate::lower::plan::value_copy::{ValueCopyPlan, analyze, modref, place};
use crate::tir;
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirMatchArm,
    TirPattern, TirStmt, TirStmtKind, TirTemplatePart, TirUnaryOp, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;

/// May-alias union-find over a function's locals, joining each binding to the
/// root it was read out of. Read by the confinement check's `no_mut_alias`.
pub struct AliasComponents {
    parent: IndexMap<u32, u32>,
}

impl AliasComponents {
    pub fn empty() -> Self {
        AliasComponents {
            parent: IndexMap::default(),
        }
    }

    pub fn build(func: &TirFunction) -> Self {
        let mut ac = AliasComponents {
            parent: IndexMap::default(),
        };
        if let Some(body) = &func.body {
            let mut collector = AliasEdgeCollector { edges: Vec::new() };
            collector.visit_block(body);
            for (a, b) in collector.edges {
                ac.union(a, b);
            }
        }
        ac
    }

    /// Whether locals `a` and `b` may share storage.
    pub fn may_alias(&self, a: u32, b: u32) -> bool {
        a == b || self.find(a) == self.find(b)
    }

    fn find(&self, mut x: u32) -> u32 {
        while let Some(&p) = self.parent.get(&x) {
            if p == x {
                return x;
            }
            x = p;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let ra = self.find(a);
        let rb = self.find(b);
        self.parent.entry(ra).or_insert(ra);
        self.parent.entry(rb).or_insert(rb);
        if ra != rb {
            self.parent.insert(ra, rb);
        }
    }
}

/// Collects `(local, alias-root)` edges: a `let` / whole-local assign bound from
/// a projection, and a match-arm binding rooted at its scrutinee.
struct AliasEdgeCollector {
    edges: Vec<(u32, u32)>,
}

impl TirRefVisitor for AliasEdgeCollector {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let Some(root) = alias_root(value)
        {
            self.edges.push((*local_index, root));
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Local { index, .. } = &target.kind
                    && let Some(root) = alias_root(value)
                {
                    self.edges.push((*index, root));
                }
            }
            TirExprKind::Match { expr: scrut, arms } => {
                if let Some(root) = alias_root(scrut) {
                    for arm in arms {
                        let mut binds: IndexSet<u32> = IndexSet::default();
                        analyze::collect_pattern_bindings(&arm.pattern, &mut binds);
                        for b in binds {
                            self.edges.push((b, root));
                        }
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// What a move can retire in one body.
#[derive(Default)]
pub struct MoveEligible {
    /// Whole locals whose every read is final.
    pub locals: IndexSet<u32>,
    /// Field and whole-value materializations that alias a dead aggregate out
    /// at a literal, keyed by the materialized expression's span.
    pub place_spans: IndexSet<Span>,
}

/// What one backward walk over a body decides: which locals a move can retire,
/// and which read-only bindings may alias the storage they were read out of.
#[derive(Default)]
pub struct Ownership {
    pub move_eligible: MoveEligible,
    /// Locals whose binding copy is elided by sharing the source storage:
    /// `row = self.rows[0]; row.len(); self.rows[0].push(x)`.
    pub share_eligible: IndexSet<u32>,
}

/// Decide `func`'s moves and shares together, both being readings of the one
/// liveness this walk computes.
pub fn analyze_ownership(
    func: &TirFunction,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
    resolver: &Resolver<'_>,
    plan: &ValueCopyPlan,
) -> Ownership {
    let Some(body) = &func.body else {
        return Ownership::default();
    };
    if has_unsupported_form(body) {
        return Ownership::default();
    }
    let stored_params = &plan.stored_params;
    let mut_receiver_methods = &plan.mut_receiver_methods;

    let mut all_locals: IndexSet<u32> = (0..func.local_count).collect();
    // Guard against a local_count that lags a grown local set.
    let mut scan = MaxLocal { max: 0 };
    scan.visit_block(body);
    for i in 0..=scan.max {
        all_locals.insert(i);
    }

    let param_locals: IndexSet<u32> = func.params.iter().map(|p| p.local_index).collect();
    let mut a = Analyzer {
        stored_params,
        mut_receiver_methods,
        ref_receiver_methods: &plan.ref_receiver_methods,
        returns_receiver_alias: &plan.returns_receiver_alias,
        mod_ref: &plan.mod_ref,
        resolver,
        type_table,
        param_locals,
        non_final: IndexSet::default(),
        aliases_live: IndexSet::default(),
        alias_sites: Vec::new(),
        borrow_escaped: IndexMap::default(),
        let_sources: IndexMap::default(),
        match_sources: Vec::new(),
        pending_mut_alias: Vec::new(),
        exits: Vec::new(),
        all_locals,
        place_cands: Vec::new(),
        declared_owned: IndexSet::default(),
        share_sources: IndexMap::default(),
        consumed: IndexMap::default(),
        mutations: Vec::new(),
    };
    let mut live = IndexSet::default();
    a.walk_block(body, &mut live, true);
    let paths = a.alias_paths();
    a.resolve_alias_chains(&paths);
    a.resolve_pending_mut_aliases(&paths);
    a.propagate_escapes_to_referents(func, type_table);

    let fresh = a.owned_locals(func, oracle, type_table);

    let owned: IndexSet<u32> = fresh
        .iter()
        .copied()
        .filter(|idx| a.hands_on_storage(*idx))
        .collect();

    let moved_places: Vec<&PlaceMove> = a
        .place_cands
        .iter()
        .filter(|site| {
            fresh.contains(&site.base)
                && !a.aliases_live.contains(&site.base)
                && !a.place_escaped(site.base, site.top)
                && !storage_shared(&paths, site.base, site.top, None, &site.live)
        })
        .collect();
    let place_move_bases: IndexSet<u32> = moved_places.iter().map(|site| site.base).collect();
    let place_spans: IndexSet<Span> = moved_places.iter().map(|site| site.span).collect();

    Ownership {
        move_eligible: MoveEligible {
            locals: owned,
            place_spans,
        },
        share_eligible: a.share_eligible(body, &paths, &place_move_bases),
    }
}

use super::place::{Names, Place as AccessPath, Resolver, Selector};

/// A write this body makes, and the locals live where it runs. A binding absent
/// from `live` cannot observe the write: nothing reads it afterwards.
struct Mutation {
    path: AccessPath,
    /// `p = x` repoints `p`; the value an earlier binding took out of `p` keeps
    /// the storage it already had. Only a write *inside* that value disturbs it.
    rebinds_place: bool,
    live: IndexSet<u32>,
}

/// A materialization that may take its aggregate's storage rather than copy it,
/// and the locals live where it runs.
struct PlaceMove {
    base: u32,
    /// The one selector the materialization reaches through, `None` for the
    /// whole aggregate.
    top: Option<u32>,
    span: Span,
    live: IndexSet<u32>,
}

/// What every share decision in one body reads, computed once.
struct ShareInputs<'a> {
    /// The storage each mutation's live set can still read, in `mutations` order.
    at_write: Vec<IndexSet<u32>>,
    capacity_observed: IndexSet<u32>,
    consumed_reach: IndexMap<u32, IndexSet<u32>>,
    place_move_bases: &'a IndexSet<u32>,
}

/// Whether the storage at `root`, narrowed to the field `top` names, is
/// reachable through one of `readers` too: two chains that meet share storage,
/// whichever end is walked. `taker` is the binding being handed that storage,
/// which is not a second reader of it.
fn storage_shared(
    paths: &IndexMap<u32, Vec<AccessPath>>,
    root: u32,
    top: Option<u32>,
    taker: Option<u32>,
    readers: &IndexSet<u32>,
) -> bool {
    let mut others = readers.clone();
    if let Some(taker) = taker {
        others.swap_remove(&taker);
    }
    let source = readable_paths(paths, &std::iter::once(root).collect());
    let reads = readable_paths(paths, &others);
    source
        .iter()
        .any(|s| reads.iter().any(|r| paths_overlap(s, top, r)))
}

/// Whether `reader` reaches the storage `source` names, narrowed to the field
/// `top` names. A path that stops short of the other reaches through it.
fn paths_overlap(source: &AccessPath, top: Option<u32>, reader: &AccessPath) -> bool {
    if source.root != reader.root || disjoint(source, reader) {
        return false;
    }
    let Some(top) = top else { return true };
    match reader.selectors.get(source.selectors.len()) {
        Some(Selector::Field { index, .. }) => *index == top,
        _ => true,
    }
}

/// Whether `m` can never change the value read at `read`. Establishes the
/// common root [`disjoint`] assumes.
fn write_cannot_reach(m: &Mutation, read: &AccessPath) -> bool {
    // A borrowed path ends in storage its root only lends, so `m` may be
    // `read`'s own storage under another name.
    if m.path.through_borrow {
        return false;
    }
    if m.path.root != read.root {
        return true;
    }
    if m.rebinds_place {
        return !write_may_reach_inside(&m.path, read);
    }
    disjoint(&m.path, read)
}

/// Whether `write` may name a place strictly inside the value `read` names. Two
/// `Index` selectors carry no subscript, so this answers "may", which is what
/// refusing a share needs.
fn write_may_reach_inside(write: &AccessPath, read: &AccessPath) -> bool {
    if write.selectors.len() <= read.selectors.len() {
        return false;
    }
    for (w, r) in write.selectors.iter().zip(read.selectors.iter()) {
        match (w, r) {
            (Selector::Field { index: a, .. }, Selector::Field { index: b, .. }) if a != b => {
                return false;
            }
            (Selector::Variant(a), Selector::Variant(b)) if a != b => return false,
            _ => {}
        }
    }
    true
}

/// The root each reference local is taken over, so a read through it is asked
/// about the place that owns it. Derived from the one resolver.
#[derive(Default)]
pub struct RefTargets {
    roots: IndexMap<u32, u32>,
}

impl RefTargets {
    #[must_use]
    pub fn referent_root(&self, expr: &TirExpr) -> Option<u32> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.roots.get(index).copied(),
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => place::place_root(place),
            _ => None,
        }
    }
}

#[must_use]
pub fn compute_ref_targets(func: &TirFunction, resolver: &Resolver<'_>) -> RefTargets {
    let mut roots = IndexMap::default();
    for local in 0..func.local_count {
        // A borrowed path names the slot holding the reference, not what it
        // points at, so it yields no referent.
        if let Some(Names::Place(place)) = resolver.binding(local)
            && place.root != local
            && !place.through_borrow
        {
            roots.insert(local, place.root);
        }
    }
    RefTargets { roots }
}

/// Whether a write to `mutated` can never change the value read at `read`, both
/// rooted alike: they share a prefix, then split at two fields or two cases.
fn disjoint(mutated: &AccessPath, read: &AccessPath) -> bool {
    for (a, b) in mutated.selectors.iter().zip(read.selectors.iter()) {
        match (a, b) {
            (Selector::Field { index: x, .. }, Selector::Field { index: y, .. }) if x != y => {
                return true;
            }
            (Selector::Variant(x), Selector::Variant(y)) if x != y => return true,
            (Selector::Field { .. }, Selector::Field { .. })
            | (Selector::Variant(_), Selector::Variant(_))
            | (Selector::Index, Selector::Index) => {}
            _ => return false,
        }
    }
    false
}

impl Analyzer<'_> {
    /// The locals holding storage of their own, however often read. A greatest
    /// fixpoint: every sourced local, less those whose source is not owned.
    fn owned_locals(
        &self,
        func: &TirFunction,
        oracle: &OwnedCalls,
        type_table: &TypeTable,
    ) -> IndexSet<u32> {
        // A by-value parameter is owned, the caller having copied or moved it
        // in; a reference local never is.
        let mut fresh: IndexSet<u32> = self
            .let_sources
            .keys()
            .copied()
            .chain(self.declared_owned.iter().copied())
            .chain(self.match_sources.iter().map(|(l, _)| *l))
            .chain(func.params.iter().map(|p| p.local_index))
            .filter(|idx| {
                !func
                    .locals
                    .get(*idx as usize)
                    .is_some_and(|l| is_reference_type(l.type_id, type_table))
            })
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for local in fresh.iter().copied().collect::<Vec<_>>() {
                let mut sources = self.let_sources.get(&local).into_iter().flatten().chain(
                    self.match_sources
                        .iter()
                        .filter_map(|(l, scrut)| (*l == local).then_some(scrut)),
                );
                if !sources.all(|s| is_owned_value(s, &fresh, oracle, type_table)) {
                    fresh.swap_remove(&local);
                    changed = true;
                }
            }
        }
        fresh
    }

    /// Whether `local` may hand its storage to a new owner: every read of it
    /// final, aliasing nothing still read, and outlived by no reference.
    fn hands_on_storage(&self, local: u32) -> bool {
        !self.non_final.contains(&local)
            && !self.aliases_live.contains(&local)
            && !self.borrow_escaped.contains_key(&local)
    }

    /// The read-only bindings that may alias the storage they were read out of.
    /// Every rule below is stated in WEP 2026-05-21, _Sharing_.
    fn share_eligible(
        &self,
        body: &TirBlock,
        paths: &IndexMap<u32, Vec<AccessPath>>,
        place_move_bases: &IndexSet<u32>,
    ) -> IndexSet<u32> {
        let inputs = ShareInputs {
            at_write: self
                .mutations
                .iter()
                .map(|m| readable_storage(paths, &m.live))
                .collect(),
            capacity_observed: capacity_observed_locals(body, self.type_table),
            // What each consumed root's storage reaches, computed once per root
            // rather than once per `share_sources` entry rooted there.
            consumed_reach: self
                .consumed
                .iter()
                .map(|(&root, at)| (root, readable_storage(paths, at)))
                .collect(),
            place_move_bases,
        };
        self.share_sources
            .keys()
            .copied()
            .filter(|&local| self.share_verdict(local, &inputs))
            .collect()
    }

    /// One binding's verdict. Each rests on facts the walk already has, so a
    /// chain needs no order: every binding on it answers for itself.
    fn share_verdict(&self, local: u32, inputs: &ShareInputs<'_>) -> bool {
        let Some(path) = self.share_sources.get(&local) else {
            return false;
        };
        // `let r = p; p = x;` leaves `r` holding what `p` gave up, so a rebind of
        // the place is never itself a conflict. Every other mutation must be
        // unreachable.
        let share_safe = self
            .mutations
            .iter()
            .zip(&inputs.at_write)
            .all(|(m, r)| !r.contains(&local) || write_cannot_reach(m, path));
        let root_given_away = inputs
            .consumed_reach
            .get(&path.root)
            .is_some_and(|reach| reach.contains(&local));
        let moved_out =
            self.consumed.contains_key(&local) || inputs.place_move_bases.contains(&local);
        // A borrowed source is written through whoever lent it, at a root the
        // scan above never looks at. A share skips the right-sizing a copy does,
        // so a binding whose own capacity is read keeps its copy.
        path.root != local
            && !path.through_borrow
            && !inputs.capacity_observed.contains(&local)
            && !moved_out
            && !self.is_mutated_root(local)
            && !root_given_away
            && share_safe
    }

    /// The places each local's value was read out of, so a write stays
    /// observable through a binding after the local it was read from dies.
    fn alias_paths(&self) -> IndexMap<u32, Vec<AccessPath>> {
        let mut paths: IndexMap<u32, Vec<AccessPath>> = IndexMap::default();
        let mut edge = |child: u32, path: AccessPath| paths.entry(child).or_default().push(path);
        for (local, sources) in &self.let_sources {
            for source in sources {
                if let Some(path) = self.read_place(source) {
                    edge(*local, path);
                }
            }
        }
        for (binding, scrut) in &self.match_sources {
            if let Some(path) = self.read_place(scrut) {
                edge(*binding, path);
            }
        }
        // The resolved root reaches where the syntax stops, and covers the
        // `skip_value_copy` binding `let_sources` leaves out.
        for (local, path) in &self.share_sources {
            edge(*local, path.clone());
        }
        paths
    }

    /// The place an alias edge reads, as far as the selectors can be trusted: a
    /// chain through a borrow ends in storage its root only lends, so it names
    /// the whole root instead.
    fn read_place(&self, source: &TirExpr) -> Option<AccessPath> {
        match self.source_path(source) {
            Some(path) if !path.through_borrow => Some(path),
            Some(path) => Some(AccessPath::local(path.root)),
            None => alias_root(source).map(AccessPath::local),
        }
    }

    /// Record what a call writes through one `&mut` handle, receiver or not: the
    /// fields the callee names, re-rooted at the handle's own path.
    fn record_call_mutation(
        &mut self,
        func: &tir::FunctionRef,
        handle: &TirExpr,
        live: &IndexSet<u32>,
    ) {
        let writes = self.mod_ref.writes(&func.module_source, &func.name);
        let owner = field_owner(handle.type_id, self.type_table);
        let Names::Place(path) = self.resolver.names(handle) else {
            self.record_mutation(handle, live);
            return;
        };
        if writes.is_opaque() || !writes.re_rootable_at(owner) {
            self.record_mutation(handle, live);
            return;
        }
        for field in writes.fields_of(owner) {
            let mut written = path.clone();
            written.selectors.push(Selector::Field {
                owner,
                index: field,
            });
            self.mutations.push(Mutation {
                path: written,
                rebinds_place: false,
                live: live.clone(),
            });
        }
    }

    fn is_mutated_root(&self, local: u32) -> bool {
        self.mutations.iter().any(|m| m.path.root == local)
    }

    /// Record a write through `place`. One the resolver cannot name mutates every
    /// local it mentions, whole.
    fn record_mutation(&mut self, place: &TirExpr, live: &IndexSet<u32>) {
        self.record_write(place, false, live);
    }

    fn record_assign(&mut self, place: &TirExpr, live: &IndexSet<u32>) {
        let rebinds = self.rebinds_place(place);
        self.record_write(place, rebinds, live);
    }

    /// Whether assigning to `place` repoints a slot instead of writing the
    /// referent where it lies, as `*p = v` to an unboxed `&mut` aggregate does.
    fn rebinds_place(&self, place: &TirExpr) -> bool {
        let TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } = &place.kind
        else {
            return true;
        };
        self.type_table.box_payload_of(inner.type_id).is_some()
    }

    fn record_write(&mut self, place: &TirExpr, rebinds_place: bool, live: &IndexSet<u32>) {
        if let Names::Place(path) = self.resolver.names(place) {
            self.mutations.push(Mutation {
                path,
                rebinds_place,
                live: live.clone(),
            });
        } else {
            // Resolved like the branch above: a write rooted at the reference
            // and a read rooted at the referent would never meet.
            let mut roots: IndexSet<u32> = IndexSet::default();
            collect_local_roots(place, &mut roots);
            for r in roots {
                let path = match self.resolver.binding(r) {
                    Some(Names::Place(place)) => place,
                    _ => AccessPath::local(r),
                };
                self.mutations.push(Mutation {
                    path,
                    rebinds_place: false,
                    live: live.clone(),
                });
            }
        }
    }

    fn mark_local_mutated(&mut self, index: u32, rebinds_place: bool, live: &IndexSet<u32>) {
        self.mutations.push(Mutation {
            path: AccessPath::local(index),
            rebinds_place,
            live: live.clone(),
        });
    }

    /// The access path a binding's value projects: a direct place, or a
    /// receiver-aliasing accessor call whose receiver / first arg is a place.
    fn source_path(&self, value: &TirExpr) -> Option<AccessPath> {
        if let Names::Place(p) = self.resolver.names(value) {
            return Some(p);
        }
        match &value.kind {
            TirExprKind::Call { func, args, .. }
                if self
                    .returns_receiver_alias
                    .contains(&func.module_source, &func.name) =>
            {
                match self.resolver.names(&args.first()?.expr) {
                    Names::Place(p) => Some(p),
                    Names::Value | Names::Unknown => None,
                }
            }
            _ => None,
        }
    }
}

/// The locals holding storage a live set can still read, for a caller that asks
/// no finer than a whole local.
fn readable_storage(paths: &IndexMap<u32, Vec<AccessPath>>, live: &IndexSet<u32>) -> IndexSet<u32> {
    readable_paths(paths, live)
        .into_iter()
        .map(|p| p.root)
        .collect()
}

/// The places a live set can still read: each live local as a whole, and every
/// place its value was taken out of, the walked selectors carried along. A chain
/// longer than [`PATH_DEPTH`] keeps the shorter place, which widens it.
fn readable_paths(paths: &IndexMap<u32, Vec<AccessPath>>, live: &IndexSet<u32>) -> Vec<AccessPath> {
    let mut out: Vec<AccessPath> = Vec::new();
    let mut work: Vec<AccessPath> = live.iter().map(|&l| AccessPath::local(l)).collect();
    while let Some(place) = work.pop() {
        if out.contains(&place) {
            continue;
        }
        for source in paths.get(&place.root).into_iter().flatten() {
            let mut up = source.clone();
            if up.selectors.len() + place.selectors.len() <= PATH_DEPTH {
                up.selectors.extend(place.selectors.iter().copied());
            }
            work.push(up);
        }
        out.push(place);
    }
    out
}

/// How far a composed alias path is carried before it widens to its prefix. A
/// cycle of alias edges would otherwise grow one without end.
const PATH_DEPTH: usize = 16;

/// Collect every local mentioned anywhere in `expr`.
fn collect_local_roots(expr: &TirExpr, out: &mut IndexSet<u32>) {
    struct W<'a>(&'a mut IndexSet<u32>);
    impl TirRefVisitor for W<'_> {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if let TirExprKind::Local { index, .. } = &expr.kind {
                self.0.insert(*index);
            }
            self.walk_expr(expr);
        }
    }
    W(out).visit_expr(expr);
}

/// Forms that re-enter this frame and read a local twice, so the whole function
/// falls back to copies. A closure does not: it reaches the frame by capture.
fn has_unsupported_form(body: &TirBlock) -> bool {
    struct Scan {
        found: bool,
    }
    impl TirRefVisitor for Scan {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if matches!(stmt.kind, TirStmtKind::VariadicForOf { .. }) {
                self.found = true;
            }
            self.walk_stmt(stmt);
        }
        fn visit_expr(&mut self, expr: &TirExpr) {
            if matches!(
                expr.kind,
                TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. }
            ) {
                self.found = true;
            }
            self.walk_expr(expr);
        }
    }
    let mut s = Scan { found: false };
    s.visit_block(body);
    s.found
}

struct MaxLocal {
    max: u32,
}
impl TirRefVisitor for MaxLocal {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            self.max = self.max.max(*local_index);
        }
        self.walk_stmt(stmt);
    }
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Local { index, .. } = &expr.kind {
            self.max = self.max.max(*index);
        }
        self.walk_expr(expr);
    }
}

/// A break / continue target on the exit stack. Loops carry `label: None`, which
/// an unlabeled break also finds; a labeled block carries its label.
struct Exit {
    label: Option<String>,
    /// Where a `break` to this target resumes.
    live: IndexSet<u32>,
    /// Where a `continue` resumes — the loop head, which a labeled block is
    /// not, so only a loop carries one.
    continue_live: Option<IndexSet<u32>>,
}

struct Analyzer<'a> {
    /// Which parameter positions each callee may persist a reference to
    /// (position 0 is the receiver). Elsewhere a `&`/`&mut` is transient.
    stored_params: &'a StoredParams,
    mut_receiver_methods: &'a FuncKeySet,
    /// Methods whose receiver is `&self` / `&mut self`: the receiver is a place
    /// they read through, not a value they take.
    ref_receiver_methods: &'a FuncKeySet,
    returns_receiver_alias: &'a FuncKeySet,
    /// What each callee writes through a `&mut` it is handed, so a read of one
    /// field survives a call that writes another.
    mod_ref: &'a modref::ModRef,
    /// The one answer to what an expression names, shared with `RefTargets` and
    /// the return-path walk rather than re-derived from syntax here.
    resolver: &'a Resolver<'a>,
    type_table: &'a TypeTable,
    /// This function's own parameters. Only a functor parameter's `stores` is
    /// checked against every argument, so an indirect call trusts only that one.
    param_locals: IndexSet<u32>,
    non_final: IndexSet<u32>,
    aliases_live: IndexSet<u32>,
    /// Each binding, the root its value was read out of, and what is live there.
    alias_sites: Vec<(u32, u32, IndexSet<u32>)>,
    /// Locals a reference outlives, by the fields it reaches. Such a local may
    /// be read through that reference after a move, so it stays copied.
    borrow_escaped: IndexMap<u32, FieldEscape>,
    let_sources: IndexMap<u32, Vec<TirExpr>>,
    match_sources: Vec<(u32, TirExpr)>,
    /// `(by-value arg root, storage the call mutates)` pairs, resolved once the
    /// alias chains are complete.
    pending_mut_alias: Vec<(u32, Vec<u32>)>,
    exits: Vec<Exit>,
    all_locals: IndexSet<u32>,
    /// Place-level move sites `(root, top-level field, span)` found at literals,
    /// filtered after the walk. A `None` field is a whole-value materialization.
    place_cands: Vec<PlaceMove>,
    /// Locals bound by a `skip_value_copy` `let` — storage handed over by the
    /// binding's producer, so owned without a source to prove it.
    declared_owned: IndexSet<u32>,
    /// The place each `let` reads its value out of, for the share rule.
    share_sources: IndexMap<u32, AccessPath>,
    /// Locals read in a value position, each with the locals live where that
    /// happens. A projection base and a borrow referent consume nothing.
    consumed: IndexMap<u32, IndexSet<u32>>,
    /// Every write this body makes, with the locals live where it runs.
    mutations: Vec<Mutation>,
}

/// Which fields of a local an escaped borrow reaches. A whole-local or imprecise
/// borrow covers every field; a clean projection names the ones it takes.
enum FieldEscape {
    Whole,
    Fields(IndexSet<u32>),
}

impl Analyzer<'_> {
    /// A read in a value position: the whole local is taken, so the value can
    /// leave this binding, and where that happens decides who still sees it.
    fn read(&mut self, index: u32, live: &mut IndexSet<u32>, record: bool) {
        if record {
            let at = self.consumed.entry(index).or_default();
            at.extend(live.iter().copied());
        }
        self.read_base(index, live, record);
    }

    /// A read that hands on a projection rather than the local. The storage is
    /// read, so an earlier value-read is not final, but nothing takes the local.
    fn read_base(&mut self, index: u32, live: &mut IndexSet<u32>, record: bool) {
        if record && live.contains(&index) {
            self.non_final.insert(index);
        }
        live.insert(index);
    }

    /// A place an expression is read *out of*. Move-side this is the read the
    /// default walk already made; share-side it consumes nothing.
    fn walk_place_base(&mut self, expr: &TirExpr, live: &mut IndexSet<u32>, record: bool) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.read_base(*index, live, record),
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => self.walk_place_base(inner, live, record),
            TirExprKind::Index { expr: inner, index } => {
                self.walk_expr(index, live, record);
                self.walk_place_base(inner, live, record);
            }
            _ => self.walk_expr(expr, live, record),
        }
    }

    /// Record that a persisting borrow reaches `field` of `root`. `None` is the
    /// whole local or an imprecise projection, escaping every field.
    fn mark_escaped(&mut self, root: u32, field: Option<u32>) {
        let entry = self
            .borrow_escaped
            .entry(root)
            .or_insert_with(|| FieldEscape::Fields(IndexSet::default()));
        match field {
            None => *entry = FieldEscape::Whole,
            Some(f) => {
                if let FieldEscape::Fields(fs) = entry {
                    fs.insert(f);
                }
            }
        }
    }

    /// Whether a persisting borrow reaches the place `root.field` (`None` field =
    /// a whole-value move, blocked by any escaped field).
    fn place_escaped(&self, root: u32, field: Option<u32>) -> bool {
        match self.borrow_escaped.get(&root) {
            None => false,
            Some(FieldEscape::Whole) => true,
            Some(FieldEscape::Fields(fs)) => match field {
                None => !fs.is_empty(),
                Some(f) => fs.contains(&f),
            },
        }
    }

    /// A `&place` / `&mut place`. The referent stays live, but a borrow takes no
    /// value, so it never ends the local's final use. Returns the referent.
    fn borrow_read(
        &mut self,
        place: &TirExpr,
        live: &mut IndexSet<u32>,
        record: bool,
    ) -> Option<u32> {
        match &place.kind {
            TirExprKind::Local { index, .. } => {
                live.insert(*index);
                Some(*index)
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary { expr: inner, .. } => self.borrow_read(inner, live, record),
            TirExprKind::Index { expr: inner, index } => {
                self.walk_expr(index, live, record);
                self.borrow_read(inner, live, record)
            }
            // A borrow of a non-place (a fresh temporary) escapes nothing.
            _ => {
                self.walk_expr(place, live, record);
                None
            }
        }
    }

    /// Whether the callee may persist a reference passed at position `pos`. One
    /// this walk has no entry for stores nothing.
    fn callee_stores(&self, callee: &FunctionRef, pos: usize) -> bool {
        self.stored_params
            .get(&callee.module_source, &callee.name)
            .is_some_and(|s| s.contains(&u32::try_from(pos).unwrap()))
    }

    /// The positions an indirect callee may store, from the functor type's
    /// `stores`. `None` where nothing checked it, so every position may.
    fn functor_stores(&self, callee: &TirExpr) -> Option<IndexSet<u32>> {
        let TirExprKind::Local { index, .. } = &callee.kind else {
            return None;
        };
        if !self.param_locals.contains(index) {
            return None;
        }
        match self.type_table.get(callee.type_id) {
            ResolvedType::Function {
                stores,
                return_type,
                ..
            } => {
                if matches!(
                    self.type_table.get(*return_type),
                    ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                ) {
                    return None;
                }
                Some(stores.iter().copied().collect())
            }
            _ => None,
        }
    }

    /// An indirect-call argument. A `&`/`&mut` is transient unless the functor
    /// may store that position; unknown `stores` escapes every position.
    fn walk_indirect_arg(
        &mut self,
        arg: &TirExpr,
        pos: usize,
        stores: &Option<IndexSet<u32>>,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if let TirExprKind::Unary {
            op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
            expr: place,
        } = &arg.kind
        {
            if record && matches!(op, TirUnaryOp::MutRef) {
                self.record_mutation(place, live);
            }
            let referent = self.borrow_read(place, live, record);
            let escapes = match stores {
                Some(s) => s.contains(&u32::try_from(pos).unwrap()),
                None => true,
            };
            if record
                && escapes
                && let Some(r) = referent
            {
                self.mark_escaped(r, top_field_of(place));
            }
        } else {
            self.walk_expr(arg, live, record);
        }
    }
    /// One call argument. A `&`/`&mut` is transient unless the callee stores
    /// that position; `borrowing_receiver` marks the one it reads through.
    fn walk_call_arg(
        &mut self,
        arg: &TirExpr,
        callee: Option<&FunctionRef>,
        pos: usize,
        borrowing_receiver: bool,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if let TirExprKind::Unary {
            op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
            expr: place,
        } = &arg.kind
        {
            // Which fields a `&mut` argument is written through is the callee's
            // own answer. Only one this walk cannot name writes the whole place.
            if record && matches!(op, TirUnaryOp::MutRef) && !borrowing_receiver {
                match callee {
                    Some(c) => self.record_call_mutation(c, place, live),
                    None => self.record_mutation(place, live),
                }
            }
            let referent = self.borrow_read(place, live, record);
            if record
                && let Some(r) = referent
                && callee.is_some_and(|c| self.callee_stores(c, pos))
            {
                self.mark_escaped(r, top_field_of(place));
            }
        } else {
            if record && callee.is_some_and(|c| self.callee_stores(c, pos)) {
                self.escape_if_reference(arg);
            }
            if borrowing_receiver {
                self.walk_place_base(arg, live, record);
            } else {
                self.walk_expr(arg, live, record);
            }
        }
    }

    /// A reference handed on as it stands, the spelling [`Analyzer::walk_expr`]
    /// misses. `&place` is left to that arm, which knows the field it borrows.
    fn escape_if_reference(&mut self, expr: &TirExpr) {
        if let Some((root, field)) = reference_escape(expr, self.type_table) {
            self.mark_escaped(root, field);
        }
    }

    /// Walk `expr` where what it yields outlives it, so a reference there pins
    /// its referent as `&place` would, through whichever arm hands it on.
    fn walk_persisting(&mut self, expr: &TirExpr, live: &mut IndexSet<u32>, record: bool) {
        if record {
            let mut escapes = Vec::new();
            yielded_escapes(expr, self.type_table, &mut escapes);
            for (root, field) in escapes {
                self.mark_escaped(root, field);
            }
        }
        self.walk_expr(expr, live, record);
    }

    /// Record that `local` derives from `source`, to be answered once the whole
    /// chain each source root stands on is known.
    fn record_alias(&mut self, local: u32, source: &TirExpr, live: &IndexSet<u32>) {
        if let Some(root) = alias_root(source) {
            self.alias_sites.push((local, root, live.clone()));
        }
    }

    /// Storage still readable after a binding is shared storage, and costs the
    /// binding its move. Both ends stand on their own chain — a match temp on
    /// the place it was hoisted out of, a sibling binding read out of the same
    /// place — so the whole chain answers on each side.
    fn resolve_alias_chains(&mut self, paths: &IndexMap<u32, Vec<AccessPath>>) {
        for (local, root, live) in std::mem::take(&mut self.alias_sites) {
            if storage_shared(paths, root, None, Some(local), &live) {
                self.aliases_live.insert(local);
            }
        }
    }

    /// Resolve the deferred sibling-alias checks: a by-value argument aliasing
    /// storage its own call mutates keeps its copy.
    fn resolve_pending_mut_aliases(&mut self, paths: &IndexMap<u32, Vec<AccessPath>>) {
        for (arg, mut_roots) in std::mem::take(&mut self.pending_mut_alias) {
            let targets: IndexSet<u32> = mut_roots.into_iter().collect();
            if storage_shared(paths, arg, None, None, &targets) {
                self.aliases_live.insert(arg);
            }
        }
    }

    /// Keep the copy of a by-value argument aliasing storage the same call
    /// mutates (wado-lang/wado#1544), once the alias chains are whole.
    fn mark_sibling_mut_aliases(&mut self, args: &[&TirExpr], extra_mut_root: Option<u32>) {
        let mut mut_roots: Vec<u32> = args
            .iter()
            .filter_map(|a| match &a.kind {
                TirExprKind::Unary {
                    op: TirUnaryOp::MutRef,
                    expr: place,
                } => alias_root(place),
                _ => None,
            })
            .collect();
        mut_roots.extend(extra_mut_root);
        if mut_roots.is_empty() {
            return;
        }
        for a in args {
            if matches!(
                &a.kind,
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                    ..
                }
            ) {
                continue;
            }
            if let Some(t) = alias_root(a) {
                self.pending_mut_alias.push((t, mut_roots.clone()));
            }
        }
    }

    fn kill_pattern(&self, pat: &TirPattern, live: &mut IndexSet<u32>) {
        let mut binds: IndexSet<u32> = IndexSet::default();
        analyze::collect_pattern_bindings(pat, &mut binds);
        for b in binds {
            live.swap_remove(&b);
        }
    }

    /// Record place-level move candidates at a literal or `let`: a child
    /// materializing an aggregate root left dead and undisturbed after it.
    fn collect_place_moves(&mut self, children: &[&TirExpr], live_out: &IndexSet<u32>) {
        let mut mats: IndexMap<u32, Vec<(Option<u32>, Span)>> = IndexMap::default();
        let mut conflict: IndexSet<u32> = IndexSet::default();
        for child in children {
            let child = strip_casts(child);
            if let Some((base, top)) = as_materialize(child) {
                mats.entry(base).or_default().push((top, child.span));
            } else {
                self.scan_place_uses(child, &mut conflict);
            }
        }
        for (base, sites) in &mats {
            if live_out.contains(base) || conflict.contains(base) {
                continue;
            }
            let has_whole = sites.iter().any(|(top, _)| top.is_none());
            if has_whole && sites.len() > 1 {
                continue;
            }
            let mut tops: Vec<u32> = sites.iter().filter_map(|(t, _)| *t).collect();
            tops.sort_unstable();
            if tops.windows(2).any(|w| w[0] == w[1]) {
                continue;
            }
            for (top, span) in sites {
                self.place_cands.push(PlaceMove {
                    base: *base,
                    top: *top,
                    span: *span,
                    live: live_out.clone(),
                });
            }
        }
    }

    /// Mark `conflict` for any use of an aggregate root that mutates or moves its
    /// storage. A read-only borrow and a deep copy leave no live alias.
    fn scan_place_uses(&self, expr: &TirExpr, conflict: &mut IndexSet<u32>) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                conflict.insert(*index);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                if clean_root(expr).is_none() {
                    self.scan_place_uses(inner, conflict);
                }
            }
            TirExprKind::Call {
                func,
                args,
                has_receiver: true,
                ..
            } => {
                let Some((receiver, rest)) = args.split_first() else {
                    return;
                };
                let receiver = &receiver.expr;
                let (recv_place, recv_ref) = match &receiver.kind {
                    TirExprKind::Unary {
                        op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
                        expr,
                    } => (expr.as_ref(), Some(*op)),
                    _ => (receiver, None),
                };
                match clean_root(recv_place) {
                    Some(base) => {
                        let read_only = matches!(recv_ref, Some(TirUnaryOp::Ref))
                            && !self
                                .mut_receiver_methods
                                .contains(&func.module_source, &func.name)
                            && !self.callee_stores(func, 0);
                        if !read_only {
                            conflict.insert(base);
                        }
                    }
                    None => self.scan_place_uses(recv_place, conflict),
                }
                for (pos, a) in rest.iter().enumerate() {
                    self.scan_call_arg_place_use(&a.expr, Some(func), pos + 1, conflict);
                }
            }
            TirExprKind::Call { func, args, .. } => {
                for (pos, a) in args.iter().enumerate() {
                    self.scan_call_arg_place_use(&a.expr, Some(func), pos, conflict);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for (pos, a) in args.iter().enumerate() {
                    self.scan_call_arg_place_use(a, None, pos, conflict);
                }
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => match clean_root(place) {
                Some(base) => {
                    conflict.insert(base);
                }
                None => self.scan_place_uses(place, conflict),
            },
            TirExprKind::Closure { captures, .. } => {
                for c in captures {
                    conflict.insert(c.outer_index);
                }
            }
            _ => {
                let mut kids: Vec<&TirExpr> = Vec::new();
                collect_child_exprs(expr, &mut kids);
                for k in kids {
                    self.scan_place_uses(k, conflict);
                }
            }
        }
    }

    /// One call argument, for the place-move scan. A `&mut base`, or a `&base`
    /// the callee stores, conflicts; a transient `&base` does not.
    fn scan_call_arg_place_use(
        &self,
        arg: &TirExpr,
        callee: Option<&FunctionRef>,
        pos: usize,
        conflict: &mut IndexSet<u32>,
    ) {
        match &arg.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: place,
            } => match clean_root(place) {
                Some(base) => {
                    conflict.insert(base);
                }
                None => self.scan_place_uses(place, conflict),
            },
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: place,
            } => match clean_root(place) {
                Some(base) => {
                    if callee.is_some_and(|c| self.callee_stores(c, pos)) {
                        conflict.insert(base);
                    }
                }
                None => self.scan_place_uses(place, conflict),
            },
            _ => self.scan_place_uses(arg, conflict),
        }
    }

    /// Live set at the resume point of a break/continue to `label`. Unknown
    /// target → every local (the sound over-approximation).
    fn exit_live(&self, label: &Option<String>) -> IndexSet<u32> {
        let found = match label {
            Some(l) => self
                .exits
                .iter()
                .rev()
                .find(|e| e.label.as_ref() == Some(l)),
            None => self.exits.iter().rev().find(|e| e.label.is_none()),
        };
        found.map_or_else(|| self.all_locals.clone(), |e| e.live.clone())
    }

    /// Live set where a `continue` resumes: the loop head's, what runs next
    /// being the next iteration rather than whatever follows the loop.
    fn continue_live(&self) -> IndexSet<u32> {
        self.exits
            .iter()
            .rev()
            .find_map(|e| e.continue_live.clone())
            .unwrap_or_else(|| self.all_locals.clone())
    }

    fn walk_block(&mut self, block: &TirBlock, live: &mut IndexSet<u32>, record: bool) {
        for stmt in block.stmts.iter().rev() {
            self.walk_stmt(stmt, live, record);
        }
    }

    fn walk_stmt(&mut self, stmt: &TirStmt, live: &mut IndexSet<u32>, record: bool) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                value,
                skip_value_copy,
                ..
            } => {
                if record {
                    // A `skip_value_copy` binding takes the storage over, its
                    // producer having proved the source dead.
                    if *skip_value_copy {
                        self.declared_owned.insert(*local_index);
                    } else {
                        self.record_alias(*local_index, value, live);
                        self.let_sources
                            .entry(*local_index)
                            .or_default()
                            .push(value.clone());
                    }
                    if let Some(path) = self.source_path(value) {
                        self.share_sources.insert(*local_index, path);
                    }
                }
                live.swap_remove(local_index);
                if record {
                    self.collect_place_moves(&[value], live);
                }
                self.walk_expr(value, live, record);
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                assert!(
                    alias_root(value).is_none(),
                    "a destructured place lowers to one `Let` per binding"
                );
                self.kill_pattern(pattern, live);
                self.walk_expr(value, live, record);
            }
            TirStmtKind::Expr(e) => self.walk_expr(e, live, record),
            TirStmtKind::Return { value } => {
                live.clear();
                if let Some(v) = value {
                    self.walk_persisting(v, live, record);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut then_live = live.clone();
                self.walk_block(then_block, &mut then_live, record);
                let mut else_live = live.clone();
                if let Some(eb) = else_block {
                    self.walk_block(eb, &mut else_live, record);
                }
                *live = union(&then_live, &else_live);
                self.walk_expr(condition, live, record);
            }
            TirStmtKind::Loop { body } => self.walk_loop(body, live, record),
            TirStmtKind::Break { label, value } => {
                *live = self.exit_live(label);
                if let Some(v) = value {
                    self.walk_expr(v, live, record);
                }
            }
            TirStmtKind::Continue => {
                *live = self.continue_live();
            }
            TirStmtKind::LabeledBlock { label, block } => {
                self.walk_labeled_block(label, block, live, record);
            }
            TirStmtKind::TaskReturn { value } => {
                self.walk_persisting(value, live, record);
            }
            TirStmtKind::VariadicForOf { .. } => unreachable!("filtered by has_unsupported_form"),
        }
    }

    /// A labeled block, in statement or expression position. A `break LABEL`
    /// inside resumes where the block ends, which only an entry on the stack
    /// says; without one [`Analyzer::exit_live`] falls back to every local.
    fn walk_labeled_block(
        &mut self,
        label: &str,
        block: &TirBlock,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        self.exits.push(Exit {
            label: Some(label.to_string()),
            live: live.clone(),
            continue_live: None,
        });
        self.walk_block(block, live, record);
        self.exits.pop();
    }

    /// A loop's live-in is the least fixpoint of its body over the back-edge.
    /// Only the pass after it settles records anything.
    fn walk_loop(&mut self, body: &TirBlock, live: &mut IndexSet<u32>, record: bool) {
        let exit_live = live.clone();
        let mut head = exit_live.clone();
        loop {
            self.exits.push(Exit {
                label: None,
                live: exit_live.clone(),
                continue_live: Some(head.clone()),
            });
            let mut work = head.clone();
            self.walk_block(body, &mut work, false);
            self.exits.pop();
            let candidate = union(&work, &exit_live);
            if candidate == head {
                break;
            }
            head = candidate;
        }
        if record {
            self.exits.push(Exit {
                label: None,
                live: exit_live.clone(),
                continue_live: Some(head.clone()),
            });
            let mut work = head.clone();
            self.walk_block(body, &mut work, true);
            self.exits.pop();
            head = union(&work, &exit_live);
        }
        *live = head;
    }

    fn walk_match(
        &mut self,
        scrut: &TirExpr,
        arms: &[TirMatchArm],
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        let after = live.clone();
        let scrut_root = alias_root(scrut);
        let arm_binds: Vec<IndexSet<u32>> = arms
            .iter()
            .map(|arm| {
                let mut binds = IndexSet::default();
                analyze::collect_pattern_bindings(&arm.pattern, &mut binds);
                binds
            })
            .collect();
        if record {
            for binds in &arm_binds {
                for b in binds {
                    self.match_sources.push((*b, scrut.clone()));
                    // An arm binding is its scrutinee's storage under a second
                    // name, so the share rule reads the path the resolver gives.
                    if let Some(Names::Place(path)) = self.resolver.binding(*b) {
                        self.share_sources.insert(*b, path);
                    }
                }
            }
        }
        let mut merged: IndexSet<u32> = IndexSet::default();
        for (arm, binds) in arms.iter().zip(&arm_binds) {
            let mut arm_live = after.clone();
            self.walk_expr(&arm.body, &mut arm_live, record);
            if let Some(guard) = &arm.guard {
                self.walk_expr(guard, &mut arm_live, record);
            }
            // The binding holds its scrutinee's storage under a second name, so
            // it aliases storage something still reads when the scrutinee's chain
            // is live anywhere the binding is: after the match, or at a read the
            // arm makes for itself.
            if record && let Some(root) = scrut_root {
                for b in binds {
                    self.alias_sites.push((*b, root, arm_live.clone()));
                }
            }
            self.kill_pattern(&arm.pattern, &mut arm_live);
            merged = union(&merged, &arm_live);
        }
        *live = merged;
        self.walk_scrutinee(scrut, live, record);
    }

    /// A place scrutinee over a `&` / `&mut` holds only for the match, so the
    /// referent stays move-eligible; what the arms bind reaches it as a borrow.
    fn walk_scrutinee(&mut self, scrut: &TirExpr, live: &mut IndexSet<u32>, record: bool) {
        if is_borrowed_place(scrut) {
            self.borrow_read(scrut, live, record);
        } else {
            // A `match` projects its scrutinee rather than taking it: the arm
            // bindings are the reads, and each decides its own copy.
            self.walk_place_base(scrut, live, record);
        }
    }

    /// Close the escape set over reference bindings: an escaped reference
    /// carries the root it was taken over. Deferred, the walk being backward.
    fn propagate_escapes_to_referents(&mut self, func: &TirFunction, type_table: &TypeTable) {
        let mut work: Vec<u32> = self.borrow_escaped.keys().copied().collect();
        let mut seen: IndexSet<u32> = work.iter().copied().collect();
        while let Some(local) = work.pop() {
            if !func
                .locals
                .get(local as usize)
                .is_some_and(|l| is_reference_type(l.type_id, type_table))
            {
                continue;
            }
            for root in self.referent_roots(local) {
                self.mark_escaped(root, None);
                if seen.insert(root) {
                    work.push(root);
                }
            }
        }
    }

    /// The locals whose storage reference local `local` names: its match
    /// scrutinee's root, and the root of every `let` / assignment source.
    fn referent_roots(&self, local: u32) -> Vec<u32> {
        self.match_sources
            .iter()
            .filter(|(b, _)| *b == local)
            .filter_map(|(_, scrut)| alias_root(scrut))
            .chain(
                self.let_sources
                    .get(&local)
                    .into_iter()
                    .flatten()
                    .filter_map(alias_root),
            )
            .collect()
    }

    fn walk_expr(&mut self, expr: &TirExpr, live: &mut IndexSet<u32>, record: bool) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.read(*index, live, record),
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Local { index, .. } = &target.kind {
                    if record {
                        self.record_alias(*index, value, live);
                        self.let_sources
                            .entry(*index)
                            .or_default()
                            .push((**value).clone());
                        // A plain local reassignment repoints the whole
                        // binding rather than writing its old storage in
                        // place, so it always rebinds.
                        self.mark_local_mutated(*index, true, live);
                    }
                    live.swap_remove(index);
                    self.walk_expr(value, live, record);
                } else {
                    if record {
                        self.record_assign(target, live);
                    }
                    self.walk_persisting(value, live, record);
                    self.walk_expr(target, live, record);
                }
            }
            TirExprKind::Match { expr: scrut, arms } => {
                self.walk_match(scrut, arms, live, record);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut then_live = live.clone();
                self.walk_block(then_branch, &mut then_live, record);
                let mut else_live = live.clone();
                if let Some(eb) = else_branch {
                    self.walk_block(eb, &mut else_live, record);
                }
                *live = union(&then_live, &else_live);
                self.walk_expr(condition, live, record);
            }
            TirExprKind::Block(block) => self.walk_block(block, live, record),
            TirExprKind::LabeledBlock { label, block, .. } => {
                self.walk_labeled_block(label, block, live, record);
            }
            TirExprKind::Call {
                func,
                args,
                has_receiver,
                ..
            } => {
                let (receiver, siblings) = match has_receiver.then(|| args.split_first()).flatten()
                {
                    Some((receiver, rest)) => (Some(&receiver.expr), rest),
                    None => (None, args.as_slice()),
                };
                let mutated_receiver = receiver.filter(|_| {
                    self.mut_receiver_methods
                        .contains(&func.module_source, &func.name)
                });
                if record {
                    // A `&mut self` receiver mutates for the whole call, so it is
                    // what the other arguments are checked against, not one of
                    // them.
                    let exprs: Vec<&TirExpr> = siblings.iter().map(|a| &a.expr).collect();
                    self.mark_sibling_mut_aliases(&exprs, mutated_receiver.and_then(alias_root));
                    if let Some(receiver) = mutated_receiver {
                        self.record_call_mutation(func, receiver, live);
                    }
                }
                // A `&self` / `&mut self` receiver is a place the callee reads
                // through rather than a value it takes.
                let borrowing_receiver = receiver.is_some()
                    && self
                        .ref_receiver_methods
                        .contains(&func.module_source, &func.name);
                for (pos, arg) in args.iter().enumerate().rev() {
                    self.walk_call_arg(
                        &arg.expr,
                        Some(func),
                        pos,
                        borrowing_receiver && pos == 0,
                        live,
                        record,
                    );
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for (pos, arg) in args.iter().enumerate().rev() {
                    self.walk_call_arg(arg, None, pos, false, live, record);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                let stores = self.functor_stores(callee);
                if record {
                    let exprs: Vec<&TirExpr> = args.iter().collect();
                    self.mark_sibling_mut_aliases(&exprs, None);
                }
                for (pos, arg) in args.iter().enumerate().rev() {
                    self.walk_indirect_arg(arg, pos, &stores, live, record);
                }
                self.walk_expr(callee, live, record);
            }
            // A `&`/`&mut` outside a call argument persists past the borrow, so
            // the referent escapes and stays copied.
            TirExprKind::Unary {
                op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
                expr: place,
            } => {
                if record && matches!(op, TirUnaryOp::MutRef) {
                    self.record_mutation(place, live);
                }
                if let Some(r) = self.borrow_read(place, live, record)
                    && record
                {
                    self.mark_escaped(r, top_field_of(place));
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                if record {
                    let children: Vec<&TirExpr> = fields.iter().map(|f| &f.value).collect();
                    self.collect_place_moves(&children, live);
                }
                for f in fields.iter().rev() {
                    self.walk_persisting(&f.value, live, record);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                if record {
                    let children: Vec<&TirExpr> = elements.iter().collect();
                    self.collect_place_moves(&children, live);
                }
                for e in elements.iter().rev() {
                    self.walk_persisting(e, live, record);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                self.walk_persisting(p, live, record);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.walk_persisting(value, live, record);
            }
            // The body indexes locals of its own.
            TirExprKind::Closure { captures, .. } => {
                for c in captures {
                    live.insert(c.outer_index);
                    if record {
                        self.mark_escaped(c.outer_index, None);
                        self.mark_local_mutated(c.outer_index, false, live);
                        let at = self.consumed.entry(c.outer_index).or_default();
                        at.extend(live.iter().copied());
                    }
                }
            }
            // A scalar projection hands back bits, not the aggregate's storage,
            // so a later whole-value read is still the root's final use.
            TirExprKind::FieldAccess { .. }
            | TirExprKind::VariantPayload { .. }
            | TirExprKind::Index { .. }
                if is_scalar_type(expr.type_id, self.type_table) =>
            {
                self.borrow_read(expr, live, record);
            }
            // A projection hands on a piece of its root, so the root is read but
            // not taken. A deref is the same step, naming the referent.
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => {
                self.walk_place_base(inner, live, record);
            }
            TirExprKind::Index { expr: inner, index } => {
                self.walk_expr(index, live, record);
                self.walk_place_base(inner, live, record);
            }
            _ => {
                let mut children: Vec<&TirExpr> = Vec::new();
                collect_child_exprs(expr, &mut children);
                for child in children.into_iter().rev() {
                    self.walk_expr(child, live, record);
                }
            }
        }
    }
}

/// The referent a value names when it is a reference handed on as it stands.
/// `&place` is not one: [`Analyzer::walk_expr`] takes that spelling itself.
fn reference_escape(expr: &TirExpr, type_table: &TypeTable) -> Option<(u32, Option<u32>)> {
    if matches!(
        strip_casts(expr).kind,
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            ..
        }
    ) {
        return None;
    }
    if !is_reference_type(expr.type_id, type_table) {
        return None;
    }
    Some((alias_root(expr)?, top_field_of(expr)))
}

/// What a form hands to a position outliving it: its own value, or the arm,
/// tail and `break` values of a control form — whichever one runs.
fn yielded_escapes(expr: &TirExpr, type_table: &TypeTable, out: &mut Vec<(u32, Option<u32>)>) {
    match &expr.kind {
        TirExprKind::Match { arms, .. } => {
            for arm in arms {
                yielded_escapes(&arm.body, type_table, out);
            }
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            block_yielded_escapes(then_branch, type_table, out);
            if let Some(eb) = else_branch {
                block_yielded_escapes(eb, type_table, out);
            }
        }
        TirExprKind::Block(block) => block_yielded_escapes(block, type_table, out),
        TirExprKind::LabeledBlock { block, .. } => {
            block_yielded_escapes(block, type_table, out);
            BreakEscapes { type_table, out }.visit_block(block);
        }
        _ => out.extend(reference_escape(expr, type_table)),
    }
}

/// A block hands on its final statement's value.
fn block_yielded_escapes(
    block: &TirBlock,
    type_table: &TypeTable,
    out: &mut Vec<(u32, Option<u32>)>,
) {
    if let Some(TirStmtKind::Expr(e)) = block.stmts.last().map(|s| &s.kind) {
        yielded_escapes(e, type_table, out);
    }
}

/// What a labeled block's `break`s hand out of it, from anywhere inside. Which
/// label one targets is not distinguished; an outer one only over-counts.
struct BreakEscapes<'a> {
    type_table: &'a TypeTable,
    out: &'a mut Vec<(u32, Option<u32>)>,
}

impl TirRefVisitor for BreakEscapes<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Break { value: Some(v), .. } = &stmt.kind {
            yielded_escapes(v, self.type_table, self.out);
        }
        self.walk_stmt(stmt);
    }
}

/// Whether `expr` is a place chain bottoming out in a `&` / `&mut`, the shape a
/// match takes on a borrowed scrutinee.
fn is_borrowed_place(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            ..
        } => true,
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => is_borrowed_place(inner),
        _ => false,
    }
}

/// Locals `.capacity()` is called on directly: a `List` / `String` copy
/// right-sizes its backing storage, so only a binding whose own capacity is
/// read needs that right-sizing — one that is not can safely alias instead.
fn capacity_observed_locals(body: &TirBlock, type_table: &TypeTable) -> IndexSet<u32> {
    struct Scan<'a> {
        type_table: &'a TypeTable,
        found: IndexSet<u32>,
    }
    impl TirRefVisitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if let TirExprKind::Call { func, args, .. } = &expr.kind
                && func.name.ends_with("::capacity")
                && let Some(receiver) = args.first()
                && (self.type_table.is_list(receiver.expr.type_id)
                    || self.type_table.is_string(receiver.expr.type_id))
                && let Some(index) = alias_root(&receiver.expr)
            {
                self.found.insert(index);
            }
            self.walk_expr(expr);
        }
    }
    let mut scan = Scan {
        type_table,
        found: IndexSet::default(),
    };
    scan.visit_block(body);
    scan.found
}

pub(crate) fn alias_root(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. } => alias_root(inner),
        _ => None,
    }
}

/// The root of a pure struct-field projection chain. `None` where the place
/// goes through a deref, index, cast, or variant payload, which may alias more.
fn clean_root(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr: inner, .. } => clean_root(inner),
        _ => None,
    }
}

/// The field index a projection chain applies directly to its root local
/// (`base.top.f.g` → `top`). `None` for a whole local or any other shape.
fn top_field_of(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            if matches!(inner.kind, TirExprKind::Local { .. }) {
                Some(*field_index)
            } else {
                top_field_of(inner)
            }
        }
        _ => None,
    }
}

/// Classify a literal child as a materialization of an aggregate root: a bare
/// local takes the whole of it, a clean projection the field it names.
fn as_materialize(expr: &TirExpr) -> Option<(u32, Option<u32>)> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some((*index, None)),
        TirExprKind::FieldAccess { .. } => Some((clean_root(expr)?, Some(top_field_of(expr)?))),
        _ => None,
    }
}

/// Peel representation-preserving casts off a materialization: a newtype hands
/// over the storage it wraps (WEP 2026-01-29), as freshness already reads it.
pub fn strip_casts(mut expr: &TirExpr) -> &TirExpr {
    while let TirExprKind::Cast { expr: inner, .. } = &expr.kind {
        expr = inner;
    }
    expr
}

/// The immediate operand sub-expressions of `expr`, in evaluation order. The
/// control forms are the walker's own and never routed here.
fn collect_child_exprs<'e>(expr: &'e TirExpr, out: &mut Vec<&'e TirExpr>) {
    use TirExprKind as K;
    match &expr.kind {
        K::Binary { left, right, .. } => {
            out.push(left);
            out.push(right);
        }
        K::Unary { expr: inner, .. }
        | K::Cast { expr: inner, .. }
        | K::FieldAccess { expr: inner, .. }
        | K::VariantTag { expr: inner, .. }
        | K::VariantTest { expr: inner, .. }
        | K::VariantPayload { expr: inner, .. }
        | K::TupleSpread { expr: inner }
        | K::TupleZip { expr: inner }
        | K::TupleLen { expr: inner } => out.push(inner),
        K::GlobalVarSet { value, .. } => out.push(value),
        K::Index { expr: base, index } => {
            out.push(base);
            out.push(index);
        }
        K::Call { args, .. } => {
            for a in args {
                out.push(&a.expr);
            }
        }
        K::CmRawCall { args, .. } => {
            for a in args {
                out.push(a);
            }
        }
        K::IndirectCall { callee, args } => {
            out.push(callee);
            for a in args {
                out.push(a);
            }
        }
        K::StructLiteral { fields, .. } => {
            for f in fields {
                out.push(&f.value);
            }
        }
        K::TupleLiteral { elements } | K::ArrayLiteral { elements } => {
            for e in elements {
                out.push(e);
            }
        }
        K::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                out.push(p);
            }
        }
        K::TypePackExpansion { call_expr, .. } => out.push(call_expr),
        K::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    out.push(expr);
                }
            }
        }
        _ => {}
    }
}

fn is_scalar_type(type_id: tir::TypeId, type_table: &TypeTable) -> bool {
    type_table.is_primitive_like(type_id)
        || matches!(
            type_table.get(type_id),
            ResolvedType::Enum { .. } | ResolvedType::Unit
        )
}

/// A `&T` / `&mut T` parameter borrows the caller's storage, so it is never a
/// movable owned value. Everything else a function takes by value it owns.
fn union(a: &IndexSet<u32>, b: &IndexSet<u32>) -> IndexSet<u32> {
    let mut out = a.clone();
    for &id in b {
        out.insert(id);
    }
    out
}

/// Locals whose storage a move hands to a new owner. An immutable-source share
/// rooted at one of them keeps its copy: the new owner may be mutable.
pub fn compute_moved_roots(
    func: &TirFunction,
    move_eligible: &MoveEligible,
    func_moved_spans: Option<&IndexSet<Span>>,
) -> IndexSet<u32> {
    let Some(body) = &func.body else {
        return IndexSet::default();
    };
    let mut walker = MovedRoots {
        move_eligible,
        func_moved_spans,
        roots: IndexSet::default(),
    };
    walker.visit_block(body);
    walker.roots
}

struct MovedRoots<'a> {
    move_eligible: &'a MoveEligible,
    func_moved_spans: Option<&'a IndexSet<Span>>,
    roots: IndexSet<u32>,
}

impl TirRefVisitor for MovedRoots<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        let stripped = strip_casts(expr);
        let moved_place = self.move_eligible.place_spans.contains(&stripped.span);
        let moved_local = match &stripped.kind {
            TirExprKind::Local { index, .. } => {
                self.move_eligible.locals.contains(index)
                    || self
                        .func_moved_spans
                        .is_some_and(|spans| spans.contains(&stripped.span))
            }
            _ => false,
        };
        if (moved_place || moved_local)
            && let Some(root) = place::place_root(stripped)
        {
            self.roots.insert(root);
        }
        // A local reached only as a projection's base is no site of its own: the
        // fold decides on the projection above it. Counting it moves nothing and
        // costs the binding its read-only share.
        match &stripped.kind {
            TirExprKind::FieldAccess { expr: base, .. }
            | TirExprKind::VariantPayload { expr: base, .. }
                if is_local_place(base) => {}
            TirExprKind::Index { expr: base, index } if is_local_place(base) => {
                self.visit_expr(index);
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Whether `expr` is a bare local read, through the casts monomorphization
/// leaves.
fn is_local_place(expr: &TirExpr) -> bool {
    matches!(strip_casts(expr).kind, TirExprKind::Local { .. })
}
