//! Move and share eligibility for the value-copy fold (WEP 2026-05-21), from one
//! backward liveness walk per body plus small auxiliary scans. A handler
//! reads twice, so skips.

use super::analyze::is_owned_value;
use super::confine::ConfinedParams;
use super::funcset::FuncKeySet;
use super::observed::ObservedFields;
use super::ownership::OwnedCalls;
use super::points_to::{LocSet, PointsTo};
use super::reach::StorageReach;
use super::retention::{CallRetention, FunctorRows, RESULT, Retained};
use super::{is_reference_type, is_scalar_type, needs_value_copy};
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::place::field_owner;
use crate::lower::plan::value_copy::{ValueCopyPlan, analyze, modref, place};
use crate::tir;
use crate::tir::{
    FunctionRef, LetStorage, TirBlock, TirExpr, TirExprKind, TirFunction, TirMatchArm, TirPattern,
    TirStmt, TirStmtKind, TirUnaryOp, TypeId, capture_source_locals,
};
use crate::tir_visitor::{TirRefVisitor, for_each_block_result};
use crate::type_table::TypeTable;
use std::rc::Rc;

/// What a move can retire in one body.
#[derive(Default)]
pub struct MoveEligible {
    /// Whole locals whose every read is final. A single read that moves is
    /// marked on its node instead ([`tir::ExprMarks::moves`]).
    pub locals: IndexSet<u32>,
}

/// What one backward walk over a body decides: which locals a move can retire,
/// and which read-only bindings may alias the storage they were read out of.
/// What it decides of a single expression it marks on the node
/// ([`tir::ExprMarks`]):
///
/// - `moves` on a field or whole-value materialization that aliases a dead
///   aggregate out at a literal, and on a whole-local value read no path reads
///   again after, of a local no kept reference pins, and not reaching storage
///   the same call writes;
/// - `written` on a by-value argument whose storage the call's evaluation may
///   write before the callee is done with it;
/// - `in_place` on an assignment that writes into the storage its place holds.
#[derive(Default)]
pub struct Ownership {
    pub move_eligible: MoveEligible,
    /// Locals whose binding copy is elided by sharing the source storage:
    /// `row = self.rows[0]; row.len(); self.rows[0].push(x)`.
    pub share_eligible: IndexSet<u32>,
}

/// Decide `func`'s moves and shares together, both being readings of the one
/// liveness this walk computes over the body as lowering sees it, synthesized
/// reads included. A final read moves unless a kept reference pins its local,
/// its callers pass it uncopied, or it names a match scrutinee's storage.
pub fn analyze_ownership(
    func: &TirFunction,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
    resolver: &Resolver<'_>,
    plan: &ValueCopyPlan,
    points_to: &PointsTo,
    reach: &StorageReach<'_>,
) -> Ownership {
    let Some(body) = &func.body else {
        return Ownership::default();
    };
    if has_unsupported_form(body) {
        return Ownership::default();
    }
    let mut_receiver_methods = &plan.mut_receiver_methods;
    let shared_params = plan.confined_params.shared_locals(func);

    let mut all_locals: IndexSet<u32> = (0..func.local_count).collect();
    // Guard against a local_count that lags a grown local set.
    let mut scan = MaxLocal { max: 0 };
    scan.visit_block(body);
    for i in 0..=scan.max {
        all_locals.insert(i);
    }

    let mut a = Analyzer {
        calls: &plan.retention,
        params: func.params.iter().map(|p| p.local_index).collect(),
        shared_params: &shared_params,
        confined_params: &plan.confined_params,
        pending_bounded: Vec::new(),
        value_holds: Vec::new(),
        handed_away: IndexSet::default(),
        contents_escaped: IndexSet::default(),
        result_kept: None,
        functor_rows: &plan.functor_rows,
        mut_receiver_methods,
        ref_receiver_methods: &plan.ref_receiver_methods,
        returns_receiver_alias: &plan.returns_receiver_alias,
        mod_ref: &plan.mod_ref,
        resolver,
        type_table,
        non_final: IndexSet::default(),
        aliases_live: IndexSet::default(),
        alias_sites: Vec::new(),
        ref_escaped: IndexMap::default(),
        written_through_escape: IndexSet::default(),
        let_sources: IndexMap::default(),
        match_sources: Vec::new(),
        pending_writes: Vec::new(),
        handed: Vec::new(),
        points_to,
        frame: points_to
            .frame(&func.module_source, &func.name)
            .expect("the fold computes points-to over every body it analyzes"),
        exits: Vec::new(),
        all_locals,
        place_cands: Vec::new(),
        declared_owned: IndexSet::default(),
        share_sources: IndexMap::default(),
        consumed: IndexMap::default(),
        final_reads: Vec::new(),
        mutations: Vec::new(),
        assign_sites: Vec::new(),
        reach,
        returns_owned: &plan.returns_owned,
        list_element_assigns: &plan.list_element_assigns,
    };
    let mut live = IndexSet::default();
    a.walk_block(body, &mut live, true);
    let paths = a.alias_paths();
    let released = released_edges(&a.alias_sites);
    a.resolve_alias_chains(&paths, &released);
    a.resolve_write_checks(func, &paths, &released);
    let reference_copies = a.reference_copies(func, type_table);
    let move_reach = a.move_reach(&paths);
    // Each step either settles a bounded retention against a destination the
    // other has just marked, or there is nothing left for either to learn.
    loop {
        a.propagate_escapes_to_referents(func, type_table);
        if !a.resolve_pending_bounded(&paths, &reference_copies, &move_reach) {
            break;
        }
    }

    a.mark_in_place_assigns(
        func,
        &paths,
        &reference_copies,
        &plan.observed_fields,
        &ReferencePoints::of(body, plan),
    );

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
                && !storage_shared(&paths, &released, site.base, site.top, None, &site.live)
        })
        .collect();
    let place_move_bases: IndexSet<u32> = moved_places.iter().map(|site| site.base).collect();
    for site in &moved_places {
        site.node.mark(|marks| marks.moves = true);
    }
    // A match binding is storage its scrutinee still owns, and whether the
    // scrutinee is dead there is the place moves' question, not this one.
    let match_bound: IndexSet<u32> = a.match_sources.iter().map(|(b, _)| *b).collect();
    for &(read, local) in &a.final_reads {
        if !shared_params.contains(&local)
            && !match_bound.contains(&local)
            && !a.place_escaped(local, None)
            && !read.marks().written
        {
            read.mark(|marks| marks.moves = true);
        }
    }

    Ownership {
        move_eligible: MoveEligible { locals: owned },
        share_eligible: a.share_eligible(body, &paths, &place_move_bases),
    }
}

use super::place::{Names, Place as AccessPath, Resolver, Selector};

/// Where a by-value argument's storage lives, for the write check.
enum Storage {
    Local(u32),
    /// Storage the reference in this local points to.
    Referent(u32),
    /// Storage this frame cannot name, which anyone may write.
    Unnamed,
}

/// A by-value argument rooted at `root`, checked once the alias chains are
/// whole.
struct WriteCheck<'a> {
    root: u32,
    /// The argument, under any newtype casts.
    arg: &'a TirExpr,
    /// The locations written while the callee holds the argument, beyond
    /// [`PointsTo::global_reach`].
    written: Rc<IndexSet<u32>>,
}

/// A binding that may share the storage it was read out of.
struct ShareSource {
    path: AccessPath,
    /// The binding's type, which says what storage a write must reach to
    /// change its value.
    binds: TypeId,
}

/// A write this body makes, and the locals live where it runs. A binding absent
/// from `live` cannot observe the write: nothing reads it afterwards.
struct Mutation<'a> {
    path: AccessPath,
    lands: Lands<'a>,
    /// The type of the place written, where the write names one: a write
    /// through a reference changes only storage a value of it reaches.
    written: Option<TypeId>,
    /// `p = x` repoints `p`; the value an earlier binding took out of `p` keeps
    /// the storage it already had. Only a write *inside* that value disturbs it.
    rebinds_place: bool,
    live: IndexSet<u32>,
}

/// The storage a write lands in, as points-to resolves it: `path` alone names
/// a reference's own slot where the write goes through it.
#[derive(Clone, Copy)]
enum Lands<'a> {
    /// The local's own location.
    Local(u32),
    /// What a place, or a handle a call writes through, denotes.
    Place(&'a LocSet),
}

/// A materialization that may take its aggregate's storage rather than copy it,
/// and the locals live where it runs.
struct PlaceMove<'a> {
    base: u32,
    /// The one selector the materialization reaches through, `None` for the
    /// whole aggregate.
    top: Option<u32>,
    node: &'a TirExpr,
    live: IndexSet<u32>,
}

/// An assignment to a place of an in-place type, and the locals live after it:
/// the storage it would repoint stays reachable through a holder still readable
/// there (WEP 2026-10-13).
struct AssignSite<'a> {
    /// The `Assign`, or the `List::index_assign` call.
    node: &'a TirExpr,
    /// The local the place is rooted at, `None` where no local roots it (a
    /// field of a call's result, `xs[i].f`), so no reference to it can be ruled
    /// out.
    root: Option<u32>,
    /// The first field the place reaches through, `None` for the whole root.
    top: Option<u32>,
    /// The last field the place names, as `(owner, index)`, `None` for a root.
    last: Option<(TypeId, u32)>,
    /// Whether the place lies past a reference, so its storage is the caller's.
    through_ref: bool,
    /// Whether the site assigns an element of the `List` the place names
    /// (`xs[i] = v`) rather than the place itself.
    element: bool,
    /// The writes the assignment recorded, which stop repointing if it writes
    /// in place.
    mutations: std::ops::Range<usize>,
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
///
/// Both ends ask what each name holds now, so a `released` edge is walked
/// through without counting its source: that name is rebound before it is
/// read again, and then holds other storage.
fn storage_shared(
    paths: &IndexMap<u32, Vec<AccessPath>>,
    released: &IndexSet<(u32, u32)>,
    root: u32,
    top: Option<u32>,
    taker: Option<u32>,
    readers: &IndexSet<u32>,
) -> bool {
    let mut others = readers.clone();
    if let Some(taker) = taker {
        others.swap_remove(&taker);
    }
    let source = readable_paths(paths, released, &std::iter::once(root).collect());
    let reads = readable_paths(paths, released, &others);
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
    // A path through a reference ends in storage its root only points to, so `m` may be
    // `read`'s own storage under another name.
    if m.path.through_ref {
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
        // A path through a reference names the slot holding the reference, not what it
        // points at, so it yields no referent.
        if let Some(Names::Place(place)) = resolver.binding(local)
            && place.root != local
            && !place.through_ref
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

impl<'a> Analyzer<'a> {
    /// The locals holding storage of their own, however often read. A greatest
    /// fixpoint: every sourced local, less those whose source is not owned.
    fn owned_locals(
        &self,
        func: &TirFunction,
        oracle: &OwnedCalls,
        type_table: &TypeTable,
    ) -> IndexSet<u32> {
        // A by-value parameter is owned, the caller having copied or moved it
        // in, unless the caller passes it uncopied. A reference local never is.
        let owned_params = func
            .params
            .iter()
            .map(|p| p.local_index)
            .filter(|l| !self.shared_params.contains(l));
        let mut fresh: IndexSet<u32> = self
            .let_sources
            .keys()
            .copied()
            .chain(self.declared_owned.iter().copied())
            .chain(self.match_sources.iter().map(|(l, _)| *l))
            .chain(owned_params)
            .filter(|idx| !is_reference_local(func, *idx, type_table))
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
            && !self.ref_escaped.contains_key(&local)
    }

    /// The read-only bindings that may alias the storage they were read out of.
    /// Every rule below is stated in WEP 2026-05-21, _Sharing_.
    fn share_eligible(
        &self,
        body: &TirBlock,
        paths: &IndexMap<u32, Vec<AccessPath>>,
        place_move_bases: &IndexSet<u32>,
    ) -> IndexSet<u32> {
        // A call's result that keeps an argument (`s.into_iter()`, `s[0..<2]`)
        // reads that argument's storage for as long as the result is live.
        let mut paths = paths.clone();
        for (root, _, holders) in &self.pending_bounded {
            for &holder in holders {
                paths
                    .entry(holder)
                    .or_default()
                    .push(AccessPath::local(*root));
            }
        }
        let paths = &paths;
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
        let Some(ShareSource { path, binds }) = self.share_sources.get(&local) else {
            return false;
        };
        // `let r = p; p = x;` leaves `r` holding what `p` gave up, so a rebind of
        // the place is never itself a conflict. Every other mutation must be
        // unreachable.
        let share_safe = self.mutations.iter().zip(&inputs.at_write).all(|(m, r)| {
            !r.contains(&local)
                || write_cannot_reach(m, path)
                || m.written
                    .is_some_and(|written| !self.reach.reaches(written, *binds, true))
        });
        let root_given_away = inputs
            .consumed_reach
            .get(&path.root)
            .is_some_and(|reach| reach.contains(&local));
        let moved_out =
            self.consumed.contains_key(&local) || inputs.place_move_bases.contains(&local);
        path.root != local
            // A source reached through a reference is written by whatever owns
            // its referent, at a root the scan above never sees, as is a root a
            // `&mut` outlives.
            && !path.through_ref
            && !self.written_through_escape.contains(&path.root)
            // A reference to the binding kept out of this body's sight reads its
            // storage after any write the scan above weighs.
            && !self.ref_escaped.contains_key(&local)
            // A share skips a copy's right-sizing: a binding whose capacity is read keeps its copy.
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
        // `Taken` bindings `let_sources` leaves out.
        for (local, source) in &self.share_sources {
            edge(*local, source.path.clone());
        }
        paths
    }

    /// The place an alias edge reads, as far as the selectors can be trusted: a
    /// chain through a reference ends in storage its root only points to, so it names
    /// the whole root instead.
    fn read_place(&self, source: &TirExpr) -> Option<AccessPath> {
        match self.source_path(source) {
            Some(path) if !path.through_ref => Some(path),
            Some(path) => Some(AccessPath::local(path.root)),
            None => alias_root(source).map(AccessPath::local),
        }
    }

    /// Record what a call writes through `handle`, the place its argument `arg`
    /// refers to, receiver or not: the fields the callee names, re-rooted at the
    /// handle's own path.
    fn record_call_mutation(
        &mut self,
        func: &tir::FunctionRef,
        arg: &TirExpr,
        handle: &TirExpr,
        position: u32,
        live: &IndexSet<u32>,
    ) {
        let lands = Lands::Place(self.points_to.handed(arg));
        let writes = self.mod_ref.writes(&func.module_source, &func.name);
        let owner = field_owner(handle.type_id, self.type_table);
        let Names::Place(path) = self.resolver.names(handle) else {
            self.record_write(handle, lands, false, live);
            return;
        };
        if writes.is_opaque() || !writes.re_rootable_at(position, owner) {
            self.record_write(handle, lands, false, live);
            return;
        }
        for field in writes.fields_of(position, owner) {
            let mut written = path.clone();
            written.selectors.push(Selector::Field {
                owner,
                index: field,
            });
            self.mutations.push(Mutation {
                path: written,
                lands,
                written: Some(handle.type_id),
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
        let lands = Lands::Place(self.points_to.written(place));
        self.record_write(place, lands, false, live);
    }

    /// Note `assign`, writing `target`, for [`ExprMarks::in_place`] where
    /// [`can_write_in_place`]; its writes are those recorded since `start`.
    fn note_assign_site(
        &mut self,
        assign: &'a TirExpr,
        target: &TirExpr,
        start: usize,
        live: &IndexSet<u32>,
    ) {
        if can_write_in_place(assign, self.type_table) {
            self.note_site(assign, target, false, start, live);
        }
    }

    /// Note `call`, a `List::index_assign` on `receiver`, for
    /// [`ExprMarks::in_place`] when the elements are of an in-place type.
    fn note_element_site(
        &mut self,
        call: &'a TirExpr,
        func: &tir::FunctionRef,
        receiver: &TirExpr,
        start: usize,
        live: &IndexSet<u32>,
    ) {
        if self
            .list_element_assigns
            .contains(&func.module_source, &func.name)
            && self
                .type_table
                .as_list(receiver.type_id)
                .is_some_and(|element| self.type_table.is_in_place_aggregate(element))
        {
            self.note_site(call, receiver, true, start, live);
            // A list reached through a reference (`*capture` in a closure) is
            // the referent's, which no field of this body owns.
            if matches!(
                place::unwrap_ref(receiver).kind,
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    ..
                }
            ) && let Some(site) = self.assign_sites.last_mut()
            {
                site.through_ref = true;
                site.last = None;
            }
        }
    }

    fn note_site(
        &mut self,
        node: &'a TirExpr,
        target: &TirExpr,
        element: bool,
        start: usize,
        live: &IndexSet<u32>,
    ) {
        let site = match self.resolver.names(target) {
            Names::Place(path) => AssignSite {
                node,
                root: Some(path.root),
                top: match path.selectors.first() {
                    Some(Selector::Field { index, .. }) => Some(*index),
                    _ => None,
                },
                last: match path.selectors.last() {
                    Some(Selector::Field { owner, index }) => Some((*owner, *index)),
                    _ => None,
                },
                through_ref: path.through_ref,
                element,
                mutations: start..self.mutations.len(),
                live: live.clone(),
            },
            // A place the resolver cannot follow is answered as the caller's.
            Names::Value | Names::Unknown => {
                let last = match &target.kind {
                    TirExprKind::FieldAccess {
                        expr, field_index, ..
                    } => Some((field_owner(expr.type_id, self.type_table), *field_index)),
                    _ => None,
                };
                AssignSite {
                    node,
                    root: place::place_root(target),
                    top: None,
                    last,
                    through_ref: true,
                    element,
                    mutations: start..self.mutations.len(),
                    live: live.clone(),
                }
            }
        };
        self.assign_sites.push(site);
    }

    fn record_assign(&mut self, place: &TirExpr, live: &IndexSet<u32>) {
        let rebinds = self.rebinds_place(place);
        let lands = Lands::Place(self.points_to.written(place));
        self.record_write(place, lands, rebinds, live);
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

    fn record_write(
        &mut self,
        place: &TirExpr,
        lands: Lands<'a>,
        rebinds_place: bool,
        live: &IndexSet<u32>,
    ) {
        if let Names::Place(path) = self.resolver.names(place) {
            self.mutations.push(Mutation {
                path,
                lands,
                written: Some(place.type_id),
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
                // A root the write cannot land in only produced the handle:
                // `view.chars().count()` advances the iterator, not `view`.
                if let Lands::Place(landed) = lands
                    && !path.through_ref
                    && let Some(location) = self.points_to.location(self.frame, path.root)
                    && !landed.contains(location)
                {
                    continue;
                }
                self.mutations.push(Mutation {
                    path,
                    lands,
                    written: Some(place.type_id),
                    rebinds_place: false,
                    live: live.clone(),
                });
            }
        }
    }

    fn mark_local_mutated(&mut self, index: u32, rebinds_place: bool, live: &IndexSet<u32>) {
        self.mutations.push(Mutation {
            path: AccessPath::local(index),
            lands: Lands::Local(index),
            written: None,
            rebinds_place,
            live: live.clone(),
        });
    }

    /// The access path a binding's value projects: a direct place, or a
    /// receiver-aliasing accessor call whose receiver / first arg is a place.
    fn source_path(&self, value: &TirExpr) -> Option<AccessPath> {
        // Sharing storage a call named rests on the argument it named being
        // a reference: `through_ref` is what makes a write through the referent's
        // owner refuse the share. A by-value argument is referenced by nobody, so its path
        // carries no such flag and a later write would go unseen —
        // `let s = black_box(y); y.a = …` must copy, as `let s = y` does.
        if self.resolver.hands_back_by_value(value) {
            return None;
        }
        if let Names::Place(p) = self.resolver.names(value) {
            return Some(p);
        }
        match &value.kind {
            TirExprKind::Call { func, args, .. }
                if self
                    .returns_receiver_alias
                    .contains(&func.module_source, &func.name) =>
            {
                match self.resolver.names(place::arg_at(func, args, 0)) {
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
    readable_paths(paths, &IndexSet::default(), live)
        .into_iter()
        .map(|p| p.root)
        .collect()
}

/// The places a live set can still read: each live local as a whole, and every
/// place its value was taken out of, the walked selectors carried along. A chain
/// longer than [`PATH_DEPTH`] keeps the shorter place, which widens it. A
/// `released` edge `(binding, source)` carries the chain on past `source`
/// without reading `source` itself.
fn readable_paths(
    paths: &IndexMap<u32, Vec<AccessPath>>,
    released: &IndexSet<(u32, u32)>,
    live: &IndexSet<u32>,
) -> Vec<AccessPath> {
    let mut out: Vec<AccessPath> = Vec::new();
    let mut walked: Vec<AccessPath> = Vec::new();
    let mut work: Vec<(AccessPath, bool)> =
        live.iter().map(|&l| (AccessPath::local(l), true)).collect();
    while let Some((place, read)) = work.pop() {
        if read && !out.contains(&place) {
            out.push(place.clone());
        }
        if walked.contains(&place) {
            continue;
        }
        for source in paths.get(&place.root).into_iter().flatten() {
            let mut up = source.clone();
            if up.selectors.len() + place.selectors.len() <= PATH_DEPTH {
                up.selectors.extend(place.selectors.iter().copied());
            }
            let read = !released.contains(&(place.root, up.root));
            work.push((up, read));
        }
        walked.push(place);
    }
    out
}

/// Alias edges `(binding, source)` whose source is dead after every binding
/// that reads it: that name is rebound before it is read again, so it no
/// longer holds the storage the binding took.
fn released_edges(alias_sites: &[(u32, u32, IndexSet<u32>)]) -> IndexSet<(u32, u32)> {
    let held: IndexSet<(u32, u32)> = alias_sites
        .iter()
        .filter(|(_, source, live)| live.contains(source))
        .map(|(binding, source, _)| (*binding, *source))
        .collect();
    alias_sites
        .iter()
        .map(|(binding, source, _)| (*binding, *source))
        .filter(|edge| !held.contains(edge))
        .collect()
}

fn is_reference_local(func: &TirFunction, local: u32, type_table: &TypeTable) -> bool {
    func.locals
        .get(local as usize)
        .is_some_and(|l| is_reference_type(l.type_id, type_table))
}

/// Every local holding what `destinations` hold: they themselves, and whatever
/// one of `holds` hands a holder's value on to.
fn holders_of(destinations: &[u32], holds: &[(u32, u32)]) -> IndexSet<u32> {
    let mut out: IndexSet<u32> = destinations.iter().copied().collect();
    let mut work: Vec<u32> = destinations.to_vec();
    while let Some(holder) = work.pop() {
        for &(from, to) in holds {
            if from == holder && out.insert(to) {
                work.push(to);
            }
        }
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

/// Whether `assign` stores a value of an in-place type, so that it writes in
/// place where a reference holds its storage. A move emptying an affine place
/// stores `null`, which repoints: the object it held is the destination's now.
/// An affine struct or list is an object, whose stores resource drop synthesis
/// has already written into it where its place was not null, so one left is
/// to a null place, and repoints too (WEP: Affine Types).
#[must_use]
pub fn can_write_in_place(assign: &TirExpr, type_table: &TypeTable) -> bool {
    let TirExprKind::Assign { target, value } = &assign.kind else {
        unreachable!("an assign site is an `Assign`");
    };
    let ty = target.type_id;
    let affine_object = type_table.holds_affine_resource(ty) && type_table.as_tuple(ty).is_none();
    !matches!(value.kind, TirExprKind::Null)
        && !affine_object
        && type_table.is_in_place_aggregate(ty)
}

/// Where a forward walk over a body first takes a reference to each local, and
/// where each assignment site runs. An assignment to a local no reference can
/// precede writes storage nothing else reaches, so it repoints.
///
/// A reference is taken only where an expression names its local: by `&`, as
/// the receiver of a `&self` / `&mut self` method, or by a closure's capture.
/// What a repeated body references counts from where it starts, since the next
/// turn brings it before every site the body runs.
#[derive(Default)]
struct ReferencePoints {
    first: IndexMap<u32, usize>,
    /// Each assignment site's position, by the node's address: the walks that
    /// ask hold the same body by reference, so the node is its own key.
    sites: IndexMap<*const TirExpr, usize>,
}

impl ReferencePoints {
    fn of(body: &TirBlock, plan: &ValueCopyPlan) -> Self {
        let mut walk = ReferenceWalk {
            plan,
            position: 0,
            repeated_from: None,
            points: ReferencePoints::default(),
        };
        walk.visit_block(body);
        walk.points
    }

    /// Whether a reference to `local` may exist where the assignment `site`
    /// runs.
    fn referenced_at(&self, local: u32, site: &TirExpr) -> bool {
        let at = self
            .sites
            .get(&std::ptr::from_ref(site))
            .expect("the walk meets every assignment site");
        self.first.get(&local).is_some_and(|first| first <= at)
    }
}

/// Each statement and expression takes the next position as the walk enters it.
struct ReferenceWalk<'a> {
    plan: &'a ValueCopyPlan,
    position: usize,
    /// Where the outermost body around the walk that runs again starts.
    repeated_from: Option<usize>,
    points: ReferencePoints,
}

impl ReferenceWalk<'_> {
    fn reference(&mut self, local: u32) {
        // Positions only grow, and a repeated body starts before every position
        // in it, so the first reference the walk meets is the earliest.
        let at = self.repeated_from.unwrap_or(self.position);
        self.points.first.entry(local).or_insert(at);
    }

    fn walk_repeated(&mut self, walk: impl FnOnce(&mut Self)) {
        let outer = self.repeated_from;
        self.repeated_from = outer.or(Some(self.position));
        walk(self);
        self.repeated_from = outer;
    }
}

impl TirRefVisitor for ReferenceWalk<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        self.position += 1;
        if let TirStmtKind::Loop { .. } | TirStmtKind::VariadicForOf { .. } = &stmt.kind {
            self.walk_repeated(|walk| walk.walk_stmt(stmt));
        } else {
            self.walk_stmt(stmt);
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        self.position += 1;
        match &expr.kind {
            // A closure body is a frame of its own; its captures are this
            // frame's locals, and it holds them.
            TirExprKind::Closure { captures, .. } => {
                for local in capture_source_locals(captures) {
                    self.reference(local);
                }
                return;
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => {
                if let Some(root) = place::place_root(place) {
                    self.reference(root);
                }
            }
            TirExprKind::Call { func, .. }
                if self
                    .plan
                    .ref_receiver_methods
                    .contains(&func.module_source, &func.name) =>
            {
                if let Some(root) = expr
                    .kind
                    .call_receiver()
                    .and_then(|(receiver, _)| place::place_root(receiver))
                {
                    self.reference(root);
                }
            }
            TirExprKind::VariadicTupleComprehension { .. }
            | TirExprKind::TypePackExpansion { .. } => {
                self.walk_repeated(|walk| walk.walk_expr(expr));
                return;
            }
            _ => {}
        }
        self.walk_expr(expr);
        let is_site = match &expr.kind {
            TirExprKind::Assign { .. } => true,
            TirExprKind::Call { func, .. } => self
                .plan
                .list_element_assigns
                .contains(&func.module_source, &func.name),
            _ => false,
        };
        if is_site {
            self.points
                .sites
                .insert(std::ptr::from_ref(expr), self.position);
        }
    }
}

/// Mark the assignments that write in place in a body the ownership walk does
/// not read, answered without liveness: every one but to a place rooted at a
/// local of this frame that no reference reaches yet, and to that a field some
/// reference anywhere may observe.
pub fn mark_unanalyzed_in_place_assigns(
    func: &TirFunction,
    type_table: &TypeTable,
    plan: &ValueCopyPlan,
) {
    struct Scan<'a> {
        func: &'a TirFunction,
        type_table: &'a TypeTable,
        observed: &'a ObservedFields,
        list_element_assigns: &'a FuncKeySet,
        references: ReferencePoints,
    }
    impl Scan<'_> {
        /// Whether assigning `place` at `site` writes in place, as the walk
        /// decides it without liveness. Past a reference, the caller's
        /// references decide, through the field `owner` holding the place's
        /// storage, and with no such field anything may hold it.
        fn writes_in_place(
            &self,
            place: &TirExpr,
            owner: Option<(TypeId, u32)>,
            site: &TirExpr,
        ) -> bool {
            match place::place_root(place) {
                None => true,
                Some(root) if is_reference_local(self.func, root, self.type_table) => {
                    owner.is_none_or(|(ty, field)| self.observed.contains(ty, field))
                }
                Some(root) => self.references.referenced_at(root, site),
            }
        }

        fn owning_field(&self, place: &TirExpr) -> Option<(TypeId, u32)> {
            match &place.kind {
                TirExprKind::FieldAccess {
                    expr, field_index, ..
                } => Some((field_owner(expr.type_id, self.type_table), *field_index)),
                _ => None,
            }
        }
    }
    impl TirRefVisitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &TirExpr) {
            match &expr.kind {
                // A closure body is a frame of its own; its captures are this
                // frame's locals, and it holds them.
                TirExprKind::Closure { .. } => return,
                TirExprKind::Call { func, .. }
                    if self
                        .list_element_assigns
                        .contains(&func.module_source, &func.name) =>
                {
                    let (receiver, _) = expr
                        .kind
                        .call_receiver()
                        .expect("`index_assign` takes its list as the receiver");
                    let in_place_elements = self
                        .type_table
                        .as_list(receiver.type_id)
                        .is_some_and(|element| self.type_table.is_in_place_aggregate(element));
                    let list = place::unwrap_ref(receiver);
                    if in_place_elements
                        && self.writes_in_place(list, self.owning_field(list), expr)
                    {
                        expr.mark(|marks| marks.in_place = true);
                    }
                }
                TirExprKind::Assign { target, .. }
                    if can_write_in_place(expr, self.type_table)
                        && self.writes_in_place(target, self.owning_field(target), expr) =>
                {
                    expr.mark(|marks| marks.in_place = true);
                }
                _ => {}
            }
            self.walk_expr(expr);
        }
    }
    let Some(body) = &func.body else {
        return;
    };
    let mut scan = Scan {
        func,
        type_table,
        observed: &plan.observed_fields,
        list_element_assigns: &plan.list_element_assigns,
        references: ReferencePoints::of(body, plan),
    };
    scan.visit_block(body);
}

/// Forms that re-enter this frame and read a local twice, so the whole function
/// falls back to copies. A closure does not: it reaches the frame by capture.
pub fn has_unsupported_form(body: &TirBlock) -> bool {
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
            if matches!(expr.kind, TirExprKind::WithHandler { .. }) {
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
    /// Which parameter positions each named callee may persist a reference to
    /// (position 0 is the receiver), and where it puts the ones it bounds, so
    /// an argument list on hand resolves the retention to locals this body
    /// owns. Elsewhere a `&`/`&mut` is transient.
    calls: &'a CallRetention,
    /// This body's parameter locals. A retention landing in one leaves the
    /// frame, whatever the callee does with it.
    params: IndexSet<u32>,
    /// The by-value parameter locals this body's callers pass uncopied.
    shared_params: &'a IndexSet<u32>,
    confined_params: &'a ConfinedParams,
    /// `(referent root, field, destination roots)` for each bounded retention,
    /// answered once every escape this body makes is known.
    pending_bounded: Vec<(u32, Option<u32>, Vec<u32>)>,
    /// `(local, destination roots)` for each by-value argument a callee keeps
    /// in locals of this body: whatever the local holds, they hold too.
    value_holds: Vec<(u32, Vec<u32>)>,
    /// Locals whose value went where no alias chain follows it — a literal, a
    /// result, a callee keeping it out of sight — so whatever it holds is
    /// readable anywhere.
    handed_away: IndexSet<u32>,
    /// Locals what whose references lead to went out of sight, and not their
    /// own storage: a call keeping a reference's referent's references, not the
    /// reference ([`Kept::Contents`]).
    contents_escaped: IndexSet<u32>,
    /// What keeps the value being walked, taken by the call that produces it:
    /// where that call hands a reference back, the reference is kept the same way.
    result_kept: Option<Kept>,
    /// What a call through a function value of each functor type keeps — the
    /// answer an indirect call reads, where no callee name is available.
    functor_rows: &'a FunctorRows,
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
    non_final: IndexSet<u32>,
    aliases_live: IndexSet<u32>,
    /// Each binding, the root its value was read out of, and what is live there.
    alias_sites: Vec<(u32, u32, IndexSet<u32>)>,
    /// Locals a reference outlives, by the fields it reaches. Such a local may
    /// be read through that reference after a move, so it stays copied.
    ref_escaped: IndexMap<u32, FieldEscape>,
    /// Locals a `&mut` outlives. The write through it is recorded where the
    /// `&mut` is taken, but lands wherever the holder runs it.
    written_through_escape: IndexSet<u32>,
    let_sources: IndexMap<u32, Vec<TirExpr>>,
    match_sources: Vec<(u32, TirExpr)>,
    /// Resolved once the alias chains are complete.
    pending_writes: Vec<WriteCheck<'a>>,
    /// What every value handed to a call reaches, in walk order, so a check
    /// reads what the later arguments handed on.
    handed: Vec<&'a LocSet>,
    points_to: &'a PointsTo,
    /// This body's frame in `points_to`.
    frame: u32,
    exits: Vec<Exit>,
    all_locals: IndexSet<u32>,
    /// Place-level move sites found at literals, filtered after the walk.
    place_cands: Vec<PlaceMove<'a>>,
    /// Locals bound by a `LetStorage::Taken` `let` — storage handed over by the
    /// binding's producer, so owned without a source to prove it.
    declared_owned: IndexSet<u32>,
    /// The place each `let` reads its value out of, for the share rule.
    share_sources: IndexMap<u32, ShareSource>,
    /// Locals read in a value position, each with the locals live where that
    /// happens. A projection base and a reference's referent consume nothing.
    consumed: IndexMap<u32, IndexSet<u32>>,
    /// Those reads no path reads the local again after, with the local.
    final_reads: Vec<(&'a TirExpr, u32)>,
    /// Every write this body makes, with the locals live where it runs.
    mutations: Vec<Mutation<'a>>,
    /// Assignments to a place of an in-place type, decided once every reference
    /// this body takes is known.
    assign_sites: Vec<AssignSite<'a>>,
    /// Whether a call's result can hold the storage an argument refers to.
    reach: &'a StorageReach<'a>,
    returns_owned: &'a FuncKeySet,
    list_element_assigns: &'a FuncKeySet,
}

/// What a call does with a reference handed at one argument position.
#[derive(Clone)]
enum Kept {
    /// Nothing: the reference ends with the call.
    Transient,
    /// Something the caller cannot name, so the referent is pinned for the rest
    /// of the frame.
    Frame,
    /// Only these locals of this body, so the referent is pinned only where one
    /// of them is still readable.
    Into(Vec<u32>),
    /// Only what the argument's references lead to, in these locals: they hold
    /// what the argument holds, and none of its own storage.
    Through(Vec<u32>),
    /// Only what the argument's references lead to, somewhere the caller
    /// cannot name: what the argument holds is readable for the rest of the
    /// frame, and its own storage is not. The types of the values that escape,
    /// where known, decide the same for what the argument was itself made of.
    Contents(Option<Vec<TypeId>>),
}

impl Kept {
    /// What keeping an argument both ways keeps of it.
    fn join(self, other: Kept) -> Kept {
        match (self, other) {
            (Kept::Frame, _) | (_, Kept::Frame) => Kept::Frame,
            (Kept::Transient, kept) | (kept, Kept::Transient) => kept,
            (Kept::Contents(a), Kept::Contents(b)) => Kept::Contents(merge_contents(a, b)),
            (Kept::Contents(c), Kept::Through(_)) | (Kept::Through(_), Kept::Contents(c)) => {
                Kept::Contents(c)
            }
            // Its own storage held here and its contents out of sight is no one
            // answer this can give.
            (Kept::Contents(_), Kept::Into(_)) | (Kept::Into(_), Kept::Contents(_)) => Kept::Frame,
            (Kept::Through(mut a), Kept::Through(b)) => {
                a.extend(b);
                Kept::Through(a)
            }
            (Kept::Into(mut a) | Kept::Through(mut a), Kept::Into(b) | Kept::Through(b)) => {
                a.extend(b);
                Kept::Into(a)
            }
        }
    }
}

/// Contents escaping as either description: any value where either says so.
fn merge_contents(a: Option<Vec<TypeId>>, b: Option<Vec<TypeId>>) -> Option<Vec<TypeId>> {
    let (mut a, b) = (a?, b?);
    for ty in b {
        if !a.contains(&ty) {
            a.push(ty);
        }
    }
    Some(a)
}

/// Which fields of a local an escaped reference reaches. A whole-local or imprecise
/// reference covers every field; a clean projection names the ones it takes.
enum FieldEscape {
    Whole,
    Fields(IndexSet<u32>),
}

impl<'a> Analyzer<'a> {
    /// A read in a value position: the whole local is taken, so the value can
    /// leave this binding, and where that happens decides who still sees it.
    fn read(&mut self, index: u32, read: &'a TirExpr, live: &mut IndexSet<u32>, record: bool) {
        self.read_taking(index, read, true, live, record);
    }

    /// [`Analyzer::read`], where `takes` says whether the value leaves this
    /// binding. One handed to a confined parameter does not: the callee holds it
    /// shared and copies before any write.
    fn read_taking(
        &mut self,
        index: u32,
        read: &'a TirExpr,
        takes: bool,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if record {
            if takes {
                let at = self.consumed.entry(index).or_default();
                at.extend(live.iter().copied());
            }
            if !live.contains(&index) {
                self.final_reads.push((read, index));
            }
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
    fn walk_place_base(&mut self, expr: &'a TirExpr, live: &mut IndexSet<u32>, record: bool) {
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

    /// Record that a persisting reference reaches `field` of `root`. `None` is the
    /// whole local or an imprecise projection, escaping every field.
    fn mark_escaped(&mut self, root: u32, field: Option<u32>) {
        let entry = self
            .ref_escaped
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

    /// Whether a persisting reference reaches the place `root.field` (`None` field =
    /// a whole-value move, blocked by any escaped field).
    fn place_escaped(&self, root: u32, field: Option<u32>) -> bool {
        match self.ref_escaped.get(&root) {
            None => false,
            Some(FieldEscape::Whole) => true,
            Some(FieldEscape::Fields(fs)) => match field {
                None => !fs.is_empty(),
                Some(f) => fs.contains(&f),
            },
        }
    }

    /// A `&place` / `&mut place`. The referent stays live, but a reference takes no
    /// value, so it never ends the local's final use. Returns the referent.
    fn ref_read(
        &mut self,
        place: &'a TirExpr,
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
            | TirExprKind::Unary { expr: inner, .. } => self.ref_read(inner, live, record),
            TirExprKind::Index { expr: inner, index } => {
                self.walk_expr(index, live, record);
                self.ref_read(inner, live, record)
            }
            // A call handing back a projection of an argument (`index_value`)
            // is a place of that argument's root.
            TirExprKind::Call { .. }
                if let Names::Place(projected) = self.resolver.names(place) =>
            {
                self.walk_expr(place, live, record);
                live.insert(projected.root);
                Some(projected.root)
            }
            // A reference to a non-place (a fresh temporary) escapes nothing.
            _ => {
                self.walk_expr(place, live, record);
                None
            }
        }
    }

    fn mutates_receiver(&self, func: &FunctionRef) -> bool {
        self.mut_receiver_methods
            .contains(&func.module_source, &func.name)
    }

    /// What `call`, a named call of `arity` arguments, keeps of them. A result
    /// that cannot hold a reference hands none back.
    fn retained_by(&self, callee: &FunctionRef, arity: usize, call: &TirExpr) -> Retained {
        self.calls.of_call(
            callee,
            arity,
            !is_scalar_type(call.type_id, self.type_table),
        )
    }

    /// What a call keeps of each argument, off what `retained` says of its
    /// positions. `result` is the call's result type and whether the callee
    /// returns it fresh, where known.
    fn kept_through(
        &self,
        retained: &Retained,
        args: &[&TirExpr],
        result: &Kept,
        result_type: Option<(TypeId, bool)>,
    ) -> Vec<Kept> {
        (0..args.len())
            .map(|pos| {
                let position = u32::try_from(pos).unwrap();
                if !retained.keeps(position) {
                    return Kept::Transient;
                }
                let arg = args[pos];
                let Some(escape) = retained.escape(position) else {
                    return self.kept_at(
                        args,
                        arg,
                        result,
                        result_type,
                        retained.destinations(position),
                    );
                };
                // Out of sight, the argument is pinned where what escapes can
                // reach its own storage; otherwise only what it holds escapes.
                let out_of_sight = match &escape.types {
                    None => Kept::Frame,
                    Some(types) => {
                        let escaping: Vec<(TypeId, bool)> =
                            types.iter().map(|&t| (t, true)).collect();
                        self.contents_kept(arg, &escaping, Some(types.iter().copied().collect()))
                    }
                };
                let landed = if escape.landings.is_empty() {
                    Kept::Transient
                } else {
                    self.kept_at(args, arg, result, result_type, Some(&escape.landings))
                };
                out_of_sight.join(landed)
            })
            .collect()
    }

    /// What landing at `destinations` keeps of `arg`.
    fn kept_at(
        &self,
        args: &[&TirExpr],
        arg: &TirExpr,
        result: &Kept,
        result_type: Option<(TypeId, bool)>,
        destinations: Option<&IndexSet<u32>>,
    ) -> Kept {
        let landing = |into_result: bool| {
            self.landing(
                args,
                arg,
                result,
                result_type.map(|(ty, _)| ty),
                destinations,
                into_result,
            )
        };
        // What a reference hands over is the storage it names; any other value lands
        // in the result whole.
        let Some((ty, fresh)) =
            result_type.filter(|_| is_reference_type(arg.type_id, self.type_table))
        else {
            return landing(true);
        };
        if self.reach.reaches(ty, arg.type_id, !fresh) {
            return landing(true);
        }
        if !self.reach.reaches_through(ty, arg.type_id, !fresh) {
            return landing(false);
        }
        // The result holds none of what the reference names, only what that
        // storage's references lead to. Kept anywhere else too, the reference pins
        // as a whole.
        match (landing(false), landing(true)) {
            (Kept::Transient, Kept::Into(locals)) => Kept::Through(locals),
            (Kept::Transient, Kept::Frame) => Kept::Contents(None),
            (Kept::Transient, contents @ Kept::Contents(_)) => contents,
            (_, all) => all,
        }
    }

    /// What values `escaping` out of sight keep of `arg`, each with whether it
    /// may hold storage by value: its own storage where one can reach it, else
    /// what it holds, described by `contents`.
    fn contents_kept(
        &self,
        arg: &TirExpr,
        escaping: &[(TypeId, bool)],
        contents: Option<Vec<TypeId>>,
    ) -> Kept {
        if escaping
            .iter()
            .any(|&(ty, by_value)| self.reach.reaches(ty, arg.type_id, by_value))
        {
            Kept::Frame
        } else if escaping
            .iter()
            .any(|&(ty, by_value)| self.reach.reaches_through(ty, arg.type_id, by_value))
        {
            Kept::Contents(contents)
        } else {
            Kept::Transient
        }
    }

    /// A kept position read as a pin: where the destinations are all locals of
    /// this body, or a result kept no longer, it lasts only as long as they do,
    /// and otherwise the frame. A result that cannot hold the argument is no
    /// destination.
    fn landing(
        &self,
        args: &[&TirExpr],
        arg: &TirExpr,
        result: &Kept,
        result_type: Option<TypeId>,
        destinations: Option<&IndexSet<u32>>,
        into_result: bool,
    ) -> Kept {
        let Some(destinations) = destinations.filter(|d| !d.is_empty()) else {
            return Kept::Frame;
        };
        let mut locals = Vec::new();
        let mut contents: Option<Option<Vec<TypeId>>> = None;
        for &position in destinations {
            if position == RESULT {
                if !into_result {
                    continue;
                }
                // A result whose holders take only what its references lead
                // to may lead to the argument itself, which it holds.
                match result {
                    Kept::Transient => {}
                    Kept::Frame => return Kept::Frame,
                    Kept::Into(holders) | Kept::Through(holders) => {
                        locals.extend(holders.iter().copied());
                    }
                    // The result's contents escape: the argument goes with them
                    // as far as what escapes reaches it.
                    Kept::Contents(escaped) => {
                        let escaping: Vec<(TypeId, bool)> = match (escaped, result_type) {
                            (Some(types), _) => types.iter().map(|&t| (t, true)).collect(),
                            (None, Some(ty)) => vec![(ty, false)],
                            (None, None) => return Kept::Frame,
                        };
                        match self.contents_kept(arg, &escaping, escaped.clone()) {
                            Kept::Frame => return Kept::Frame,
                            Kept::Contents(c) => {
                                contents = Some(match contents {
                                    Some(prior) => merge_contents(prior, c),
                                    None => c,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                continue;
            }
            let root = usize::try_from(position)
                .ok()
                .and_then(|pos| args.get(pos))
                .and_then(|arg| alias_root(arg))
                .and_then(|root| self.frame_local(root));
            let Some(root) = root else {
                return Kept::Frame;
            };
            locals.push(root);
        }
        match (contents, locals.is_empty()) {
            (Some(contents), true) => Kept::Contents(contents),
            (Some(_), false) => Kept::Frame,
            (None, true) => Kept::Transient,
            (None, false) => Kept::Into(locals),
        }
    }

    /// `root`, where a value landing in it stays in this frame. A landing in a
    /// parameter leaves the frame, so the caller's caller is the one that could
    /// bound it.
    fn frame_local(&self, root: u32) -> Option<u32> {
        (!self.params.contains(&root)).then_some(root)
    }

    /// What storing a value in `local` keeps of it.
    fn held_in(&self, local: u32) -> Kept {
        self.frame_local(local)
            .map_or(Kept::Frame, |local| Kept::Into(vec![local]))
    }

    /// [`Self::pin`] for a `&place` / `&mut place` argument naming `root`.
    fn pin_ref(&mut self, op: TirUnaryOp, root: u32, place: &TirExpr, kept: &Kept) {
        if op == TirUnaryOp::MutRef
            && !matches!(kept, Kept::Transient | Kept::Through(_) | Kept::Contents(_))
        {
            self.written_through_escape.insert(root);
        }
        self.pin(root, top_field_of(place), kept);
    }

    /// Pin `root.field` for as long as what the call keeps it in is readable.
    fn pin(&mut self, root: u32, field: Option<u32>, kept: &Kept) {
        match kept {
            Kept::Transient => {}
            Kept::Frame => self.mark_escaped(root, field),
            Kept::Into(destinations) => {
                self.pending_bounded
                    .push((root, field, destinations.clone()));
            }
            Kept::Through(destinations) => self.value_holds.push((root, destinations.clone())),
            Kept::Contents(_) => {
                self.contents_escaped.insert(root);
            }
        }
    }

    /// Answer each deferred bounded retention against the liveness and the
    /// escapes now known, and say whether that marked anything new.
    /// What hands a holder's value on to another local, and the storage
    /// readable anywhere: a local a reference outlives, or whose value went out
    /// of sight, and everything its value was read out of.
    fn holder_graph(
        &self,
        paths: &IndexMap<u32, Vec<AccessPath>>,
        reference_copies: &[(u32, u32)],
    ) -> (Vec<(u32, u32)>, IndexSet<u32>) {
        let holds = self
            .pending_bounded
            .iter()
            .map(|(root, _, destinations)| (*root, destinations))
            .chain(self.value_holds.iter().map(|(local, d)| (*local, d)))
            .flat_map(|(from, destinations)| destinations.iter().map(move |to| (from, *to)))
            .chain(reference_copies.iter().copied())
            .collect();
        // A local whose contents escaped holds what it holds anywhere, which is
        // all a holder is asked; its own storage it keeps.
        let anywhere: IndexSet<u32> = self
            .handed_away
            .iter()
            .chain(self.ref_escaped.keys())
            .chain(&self.contents_escaped)
            .copied()
            .collect();
        (holds, readable_storage(paths, &anywhere))
    }

    /// Mark the assignments that write in place, each turning the writes it
    /// recorded from a repoint into a write inside the storage. Every place asks
    /// the references this body takes to it before the assignment: one a holder
    /// outlives, or one whose holder is readable after the assignment. A place
    /// past a reference is the caller's as well, so it also asks the references
    /// other bodies may hold across the call that reaches this one.
    fn mark_in_place_assigns(
        &mut self,
        func: &TirFunction,
        paths: &IndexMap<u32, Vec<AccessPath>>,
        reference_copies: &[(u32, u32)],
        observed: &ObservedFields,
        references: &ReferencePoints,
    ) {
        let (holds, readable_anywhere) = self.holder_graph(paths, reference_copies);
        for site in &self.assign_sites {
            let Some(root) = site.root else {
                site.node.mark(|marks| marks.in_place = true);
                for m in site.mutations.clone() {
                    self.mutations[m].rebinds_place = false;
                }
                continue;
            };
            let external = site.through_ref || is_reference_local(func, root, self.type_table);
            if !external && !references.referenced_at(root, site.node) {
                continue;
            }
            let readable = readable_storage(paths, &site.live);
            // Which of the root's fields a reference still holds after the site:
            // `None` stands for the whole root, or a reference too imprecise to
            // name a field.
            let held: IndexSet<Option<u32>> = self
                .pending_bounded
                .iter()
                .filter(|(pinned, _, destinations)| {
                    *pinned == root
                        && holders_of(destinations, &holds).iter().any(|holder| {
                            readable.contains(holder) || readable_anywhere.contains(holder)
                        })
                })
                .map(|(_, field, _)| *field)
                .chain(self.escaped_fields(root))
                .collect();
            // A reference naming no field may be one taken through a call
            // (`&hs[0].p`, an iterator) that holds any field, so it counts for
            // every place under the root. An element of a `List` this body
            // received by reference may be held by its caller.
            let in_place = match site.top {
                None => !held.is_empty(),
                Some(top) => held.contains(&Some(top)) || held.contains(&None),
            } || (external
                && site.last.map_or(site.element, |(owner, field)| {
                    observed.contains(owner, field)
                }));
            if in_place {
                site.node.mark(|marks| marks.in_place = true);
                for m in site.mutations.clone() {
                    self.mutations[m].rebinds_place = false;
                }
            }
        }
    }

    /// The fields of `root` a reference that outlives the frame reaches, `None`
    /// for the whole root.
    fn escaped_fields(&self, root: u32) -> Vec<Option<u32>> {
        match self.ref_escaped.get(&root) {
            None => Vec::new(),
            Some(FieldEscape::Whole) => vec![None],
            Some(FieldEscape::Fields(fields)) => fields.iter().map(|f| Some(*f)).collect(),
        }
    }

    fn resolve_pending_bounded(
        &mut self,
        paths: &IndexMap<u32, Vec<AccessPath>>,
        reference_copies: &[(u32, u32)],
        move_reach: &IndexMap<u32, Vec<IndexSet<u32>>>,
    ) -> bool {
        let (holds, readable_anywhere) = self.holder_graph(paths, reference_copies);
        let mut marked = false;
        let mut unsettled = Vec::new();
        for (root, field, destinations) in std::mem::take(&mut self.pending_bounded) {
            let holders = holders_of(&destinations, &holds);
            let reaches = |storage: &IndexSet<u32>| holders.iter().any(|h| storage.contains(h));
            let held = reaches(&readable_anywhere)
                || move_reach
                    .get(&root)
                    .is_some_and(|sites| sites.iter().any(reaches));
            if held {
                self.mark_escaped(root, field);
                marked = true;
            } else {
                unsettled.push((root, field, destinations));
            }
        }
        self.pending_bounded = unsettled;
        marked
    }

    /// For each root a bounded retention pins, the storage still readable at
    /// each place it would be moved out of.
    fn move_reach(
        &self,
        paths: &IndexMap<u32, Vec<AccessPath>>,
    ) -> IndexMap<u32, Vec<IndexSet<u32>>> {
        let roots: IndexSet<u32> = self
            .pending_bounded
            .iter()
            .map(|(root, ..)| *root)
            .collect();
        roots
            .into_iter()
            .map(|root| {
                let sites = self
                    .consumed
                    .get(&root)
                    .into_iter()
                    .chain(
                        self.place_cands
                            .iter()
                            .filter(|site| site.base == root)
                            .map(|site| &site.live),
                    )
                    .map(|live| readable_storage(paths, live))
                    .collect();
                (root, sites)
            })
            .collect()
    }

    /// `(source, copy)` for each reference local bound out of a place rooted at
    /// `source`. The resolver names such a copy by its referent rather than by
    /// `source`, so no alias chain leads back from the copy to it.
    fn reference_copies(&self, func: &TirFunction, type_table: &TypeTable) -> Vec<(u32, u32)> {
        self.let_sources
            .iter()
            .flat_map(|(local, sources)| sources.iter().map(move |s| (*local, s)))
            .chain(self.match_sources.iter().map(|(local, s)| (*local, s)))
            .filter(|(local, _)| is_reference_local(func, *local, type_table))
            .filter_map(|(local, source)| Some((alias_root(source)?, local)))
            .collect()
    }

    /// An indirect-call argument. Nothing here names the body that will run, so
    /// the row the call site resolved says what this position leaves behind:
    /// through a functor nothing mints a retaining value for, a reference is as
    /// transient as it is through a named callee that keeps nothing.
    fn walk_indirect_arg(
        &mut self,
        arg: &'a TirExpr,
        kept: &Kept,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if let TirExprKind::Unary {
            op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
            expr: place,
        } = &arg.kind
        {
            self.walk_ref(*op, place, kept, live, record);
        } else {
            self.walk_kept(arg, kept, false, live, record);
        }
    }

    /// A `&place` / `&mut place` whose reference is kept as `kept` says. The write
    /// a `&mut` makes is recorded here, wherever its holder runs it.
    fn walk_ref(
        &mut self,
        op: TirUnaryOp,
        place: &'a TirExpr,
        kept: &Kept,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if record && op == TirUnaryOp::MutRef {
            self.record_mutation(place, live);
        }
        self.hold_ref(op, place, kept, live, record);
    }

    /// [`Self::walk_ref`] for a caller that records the write itself. A
    /// reference to a call's result keeps that temporary as `kept` says.
    fn hold_ref(
        &mut self,
        op: TirUnaryOp,
        place: &'a TirExpr,
        kept: &Kept,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if record && is_call(place) {
            self.result_kept = Some(kept.clone());
        }
        if let Some(r) = self.ref_read(place, live, record)
            && record
        {
            self.pin_ref(op, r, place, kept);
        }
        assert!(
            self.result_kept.is_none(),
            "the call producing a referenced temporary takes what keeps it"
        );
    }

    /// One call argument. A `&`/`&mut` is transient unless the callee stores
    /// that position; `ref_receiver` marks the one it reads through,
    /// whose writes the call records.
    fn walk_call_arg(
        &mut self,
        arg: &'a TirExpr,
        callee: Option<(&FunctionRef, u32)>,
        kept: &Kept,
        ref_receiver: bool,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        let TirExprKind::Unary {
            op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
            expr: place,
        } = &arg.kind
        else {
            if let TirExprKind::Local { index, .. } = &arg.kind
                && !ref_receiver
                && callee.is_some_and(|(c, position)| {
                    self.confined_params.is_confined(c, position as usize)
                })
            {
                if record {
                    self.hand_on(arg, kept);
                }
                self.read_taking(*index, arg, false, live, record);
                return;
            }
            self.walk_kept(arg, kept, ref_receiver, live, record);
            return;
        };
        // Which fields a `&mut` argument is written through is the callee's
        // own answer. Only one this walk cannot name writes the whole place.
        let Some((c, position)) = callee else {
            self.walk_ref(*op, place, kept, live, record);
            return;
        };
        if record && !ref_receiver && *op == TirUnaryOp::MutRef {
            self.record_call_mutation(c, arg, place, position, live);
        }
        // A call handing its receiver back as its result keeps the reference in
        // that result, which the walk follows as a place.
        let handed_back = position == 0
            && self
                .returns_receiver_alias
                .contains(&c.module_source, &c.name);
        let op = if handed_back { TirUnaryOp::Ref } else { *op };
        self.hold_ref(op, place, kept, live, record);
    }

    /// A value handed to a position that keeps it as `kept` says. A reference
    /// pins its referent; any other value hands on what it holds. `&place` is
    /// left to [`Analyzer::walk_ref`], which knows the field it references.
    fn hand_on(&mut self, expr: &TirExpr, kept: &Kept) {
        let mut yielded = Vec::new();
        yielded_values(expr, self.type_table, &mut yielded);
        for value in yielded {
            if let Some((root, field)) = value.reference {
                self.pin(root, field, kept);
            } else if let Some(root) = value.root {
                match kept {
                    Kept::Transient => {}
                    Kept::Frame => {
                        self.handed_away.insert(root);
                    }
                    Kept::Contents(_) => {
                        self.contents_escaped.insert(root);
                    }
                    Kept::Into(destinations) | Kept::Through(destinations) => {
                        self.value_holds.push((root, destinations.clone()));
                    }
                }
            }
        }
    }

    /// Walk `value`, handed to a position that keeps it as `kept` says. A call
    /// producing it keeps what it hands back in its result the same way, and a
    /// literal building it keeps its elements so.
    /// `place_base` walks a receiver read through rather than taken.
    fn walk_kept(
        &mut self,
        value: &'a TirExpr,
        kept: &Kept,
        place_base: bool,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if record {
            self.hand_on(value, kept);
            if is_call(value) || (!place_base && is_aggregate_literal(value)) {
                self.result_kept = Some(kept.clone());
            }
        }
        if place_base {
            self.walk_place_base(value, live, record);
        } else {
            self.walk_expr(value, live, record);
        }
        assert!(
            self.result_kept.is_none(),
            "the call producing a kept value takes what keeps it"
        );
    }

    /// Walk an element of a literal that is kept as `kept` says.
    fn walk_element(
        &mut self,
        element: &'a TirExpr,
        kept: &Kept,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if let TirExprKind::Unary {
            op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
            expr: place,
        } = &element.kind
        {
            self.walk_ref(*op, place, kept, live, record);
        } else {
            self.walk_kept(element, kept, false, live, record);
        }
    }

    /// Walk `expr` where what it yields outlives it, so a reference there pins
    /// its referent as `&place` would, and any other value hands on what it
    /// holds, through whichever arm yields it.
    fn walk_persisting(&mut self, expr: &'a TirExpr, live: &mut IndexSet<u32>, record: bool) {
        self.walk_kept(expr, &Kept::Frame, false, live, record);
    }

    /// Walk the value a `let` or a whole-local assignment stores in `local`.
    /// A reference or a call result stored there lasts as long as `local` is
    /// readable, so it pins its referent only that long. A place is followed
    /// by the alias chains; anything else a branch yields persists.
    fn walk_stored_value(
        &mut self,
        local: u32,
        value: &'a TirExpr,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        let stripped = strip_casts(value);
        match &stripped.kind {
            TirExprKind::Unary {
                op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
                expr: place,
            } => {
                let kept = self.held_in(local);
                self.walk_ref(*op, place, &kept, live, record);
            }
            _ if alias_root(stripped).is_some() => self.walk_expr(value, live, record),
            _ if is_call(value) => {
                let kept = self.held_in(local);
                self.walk_kept(value, &kept, false, live, record);
            }
            _ => self.walk_persisting(value, live, record),
        }
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
    fn resolve_alias_chains(
        &mut self,
        paths: &IndexMap<u32, Vec<AccessPath>>,
        released: &IndexSet<(u32, u32)>,
    ) {
        for (local, root, live) in std::mem::take(&mut self.alias_sites) {
            if storage_shared(paths, released, root, None, Some(local), &live) {
                self.aliases_live.insert(local);
            }
        }
    }

    /// Resolve the deferred write checks: a by-value argument whose storage the
    /// call's evaluation may write keeps its copy. A local's storage is
    /// reached through the alias chains as well as its own location; storage a
    /// reference points to, through what the reference names.
    fn resolve_write_checks(
        &mut self,
        func: &TirFunction,
        paths: &IndexMap<u32, Vec<AccessPath>>,
        released: &IndexSet<(u32, u32)>,
    ) {
        let checks = std::mem::take(&mut self.pending_writes);
        if checks.is_empty() {
            return;
        }
        let frame = self.frame;
        let pt = self.points_to;
        let global = pt.global_reach();
        let references: Vec<(u32, IndexSet<u32>)> = self
            .all_locals
            .iter()
            .filter(|&&l| is_reference_local(func, l, self.type_table))
            .map(|&l| (l, pt.local_targets(frame, l)))
            .collect();
        // What any call may write, whatever it is handed.
        let always: IndexSet<u32> = self
            .all_locals
            .iter()
            .copied()
            .filter(|&l| pt.location(frame, l).is_some_and(|n| global.contains(&n)))
            .chain(
                references
                    .iter()
                    .filter(|(_, named)| named.iter().any(|n| global.contains(n)))
                    .map(|(l, _)| *l),
            )
            .collect();
        for check in checks {
            let written = if is_reference_local(func, check.root, self.type_table) {
                pt.reaches_written(frame, check.root, &check.written)
            } else {
                let targets: IndexSet<u32> = always
                    .iter()
                    .copied()
                    .chain(check.written.iter().filter_map(|&n| pt.local_of(frame, n)))
                    .chain(
                        references
                            .iter()
                            .filter(|(_, named)| named.iter().any(|n| check.written.contains(n)))
                            .map(|(l, _)| *l),
                    )
                    .collect();
                storage_shared(paths, released, check.root, None, None, &targets)
            };
            if written {
                self.aliases_live.insert(check.root);
                check.arg.mark(|marks| marks.written = true);
            }
        }
    }

    /// Defer the check of call argument `arg` against what the call's
    /// evaluation may write while the callee holds it (wado-lang/wado#1544):
    /// whatever the values handed to a call since `handed_from` reach, `own`
    /// aside, and the writes recorded since `call_writes`, which the later
    /// arguments make after `arg` is taken.
    fn defer_write_check(
        &mut self,
        arg: &'a TirExpr,
        call_writes: usize,
        handed_from: usize,
        own: usize,
    ) {
        let is_ref = matches!(
            &arg.kind,
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                ..
            }
        );
        // Only a value a copy would wrap has a copy to keep.
        if is_ref || !needs_value_copy(arg.type_id, self.type_table) {
            return;
        }
        let stripped = strip_casts(arg);
        let mut storages = Vec::new();
        self.storage_roots(arg, &mut storages);
        if storages.is_empty() {
            return;
        }
        let written = Rc::new(self.written_since(call_writes, handed_from, own));
        for storage in storages {
            match storage {
                Storage::Local(root) => self.pending_writes.push(WriteCheck {
                    root,
                    arg: stripped,
                    written: Rc::clone(&written),
                }),
                Storage::Referent(reference) => {
                    if self
                        .points_to
                        .reaches_written(self.frame, reference, &written)
                    {
                        stripped.mark(|marks| marks.written = true);
                    }
                }
                Storage::Unnamed => stripped.mark(|marks| marks.written = true),
            }
        }
    }

    /// The locations a call's evaluation may write beyond
    /// [`PointsTo::global_reach`].
    fn written_since(&self, call_writes: usize, handed_from: usize, own: usize) -> IndexSet<u32> {
        let frame = self.frame;
        let pt = self.points_to;
        let mut sources: Vec<u32> = Vec::new();
        for (i, handed) in self.handed[handed_from..].iter().enumerate() {
            if handed_from + i != own {
                sources.extend(handed.iter());
            }
        }
        let mut written = pt.reach_beyond_global(sources);
        for m in &self.mutations[call_writes..] {
            match m.lands {
                Lands::Local(local) => {
                    written.insert(
                        pt.location(frame, local)
                            .expect("the analysis gives every local a body writes a location"),
                    );
                }
                Lands::Place(set) => written.extend(set.iter()),
            }
        }
        written
    }

    /// Note what `args` hand a call.
    fn hand(&mut self, args: &[&TirExpr]) {
        for arg in args {
            self.handed.push(self.points_to.handed(arg));
        }
    }

    /// The storage each value `expr` may hand on.
    fn storage_roots(&self, expr: &TirExpr, out: &mut Vec<Storage>) {
        yielded_leaves(
            expr,
            self.type_table,
            &mut |leaf| match self.resolver.names(leaf) {
                Names::Value => {}
                Names::Place(place) if place.through_ref => out.push(Storage::Referent(place.root)),
                Names::Place(place) => out.push(Storage::Local(place.root)),
                Names::Unknown => match (alias_root(leaf), &projected_from(leaf).kind) {
                    (Some(root), _) => out.push(Storage::Local(root)),
                    // A named call hands back storage of its own or reached
                    // through its arguments, and so does a part of it.
                    (None, TirExprKind::Call { args, .. }) => {
                        for a in args {
                            self.storage_roots(place::unwrap_ref(&a.expr), out);
                        }
                    }
                    (None, _) => out.push(Storage::Unnamed),
                },
            },
        );
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
    fn collect_place_moves(&mut self, children: &[&'a TirExpr], live_out: &IndexSet<u32>) {
        let mut mats: IndexMap<u32, Vec<(Option<u32>, &'a TirExpr)>> = IndexMap::default();
        let mut conflict: IndexSet<u32> = IndexSet::default();
        for child in children {
            let child = strip_casts(child);
            if let Some((base, top)) =
                as_materialize(child).or_else(|| self.accessor_materialize(child, &mut conflict))
            {
                mats.entry(base).or_default().push((top, child));
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
            for &(top, node) in sites {
                self.place_cands.push(PlaceMove {
                    base: *base,
                    top,
                    node,
                    live: live_out.clone(),
                });
            }
        }
    }

    /// A call handing back a field of a local it only reads through `&`, as
    /// `fn raw(&self) -> Array<T> { return self.repr; }` does: it materializes
    /// that field as `local.repr` would. Its other arguments go to `conflict`.
    fn accessor_materialize(
        &self,
        call: &TirExpr,
        conflict: &mut IndexSet<u32>,
    ) -> Option<(u32, Option<u32>)> {
        let TirExprKind::Call { func, args, .. } = &call.kind else {
            return None;
        };
        let Names::Place(place) = self.resolver.names(call) else {
            return None;
        };
        if place.through_ref || !place.field_addressable() {
            return None;
        }
        let param = self.resolver.return_path(func)?.param;
        let TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: referent,
        } = &place::arg_at(func, args, param).kind
        else {
            return None;
        };
        let retained = self.retained_by(func, args.len(), call);
        if clean_root(referent) != Some(place.root) || retained.keeps(u32::try_from(param).unwrap())
        {
            return None;
        }
        for (pos, a) in args.iter().enumerate() {
            if pos != param {
                self.scan_call_arg_place_use(&a.expr, Some(&retained), pos, conflict);
            }
        }
        Some((
            place.root,
            place.selectors.first().and_then(|s| s.field_index()),
        ))
    }

    /// Mark `conflict` for any use of an aggregate root that mutates or moves its
    /// storage. A read-only reference and a deep copy leave no live alias.
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
            TirExprKind::Call { func, args, .. } if let (Some(receiver), _) = args.split() => {
                let retained = self.retained_by(func, args.len(), expr);
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
                            && !self.mutates_receiver(func)
                            && !retained.keeps(0);
                        if !read_only {
                            conflict.insert(base);
                        }
                    }
                    None => self.scan_place_uses(recv_place, conflict),
                }
                for (pos, a) in args.rest_positioned() {
                    self.scan_call_arg_place_use(&a.expr, Some(&retained), pos, conflict);
                }
            }
            TirExprKind::Call { func, args, .. } => {
                let retained = self.retained_by(func, args.len(), expr);
                for (pos, a) in args.iter().enumerate() {
                    self.scan_call_arg_place_use(&a.expr, Some(&retained), pos, conflict);
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
                for index in capture_source_locals(captures) {
                    conflict.insert(index);
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
        retained: Option<&Retained>,
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
                    if retained.is_some_and(|r| r.keeps(u32::try_from(pos).unwrap())) {
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

    /// A run of `let`s each taking a distinct field of one aggregate, as a
    /// destructuring lowers to, hands its fields over as one literal would: each
    /// field is read only by its own `let`, so the aggregate is dead after all
    /// of them rather than after each.
    fn walk_block(&mut self, block: &'a TirBlock, live: &mut IndexSet<u32>, record: bool) {
        let stmts = &block.stmts;
        let mut end = stmts.len();
        while end > 0 {
            // A pass that records nothing walks the run as it walks any `let`s.
            let run = record.then(|| field_let_run(stmts, end)).flatten();
            let start = run.unwrap_or(end - 1);
            if run.is_some() {
                let mut live_out = live.clone();
                let mut values = Vec::new();
                for stmt in &stmts[start..end] {
                    let TirStmtKind::Let {
                        local_index, value, ..
                    } = &stmt.kind
                    else {
                        unreachable!("a field run holds only `let`s");
                    };
                    live_out.swap_remove(local_index);
                    values.push(value);
                }
                self.collect_place_moves(&values, &live_out);
            }
            for stmt in stmts[start..end].iter().rev() {
                self.walk_stmt(stmt, live, record, run.is_none());
            }
            end = start;
        }
    }

    /// `place_moves`: whether a `let` records its own place-move candidate,
    /// rather than the field run holding it.
    fn walk_stmt(
        &mut self,
        stmt: &'a TirStmt,
        live: &mut IndexSet<u32>,
        record: bool,
        place_moves: bool,
    ) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                value,
                storage,
                ..
            } => {
                if record {
                    if *storage == LetStorage::Taken {
                        self.declared_owned.insert(*local_index);
                    } else {
                        self.record_alias(*local_index, value, live);
                        self.let_sources
                            .entry(*local_index)
                            .or_default()
                            .push(value.clone());
                    }
                    if let Some(path) = self.source_path(value) {
                        self.share_sources.insert(
                            *local_index,
                            ShareSource {
                                path,
                                binds: value.type_id,
                            },
                        );
                    }
                }
                live.swap_remove(local_index);
                if record && place_moves {
                    self.collect_place_moves(&[value], live);
                }
                self.walk_stored_value(*local_index, value, live, record);
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                assert!(
                    alias_root(value).is_none(),
                    "a destructured place lowers to one `Let` per binding"
                );
                self.kill_pattern(pattern, live);
                self.walk_persisting(value, live, record);
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
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
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
        block: &'a TirBlock,
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
    fn walk_loop(&mut self, body: &'a TirBlock, live: &mut IndexSet<u32>, record: bool) {
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
        scrut: &'a TirExpr,
        arms: &'a [TirMatchArm],
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
            for arm in arms {
                analyze::for_each_pattern_binding(&arm.pattern, &mut |b, binds| {
                    self.match_sources.push((b, scrut.clone()));
                    // An arm binding is its scrutinee's storage under a second
                    // name, so the share rule reads the path the resolver gives.
                    if let Some(Names::Place(path)) = self.resolver.binding(b) {
                        self.share_sources.insert(b, ShareSource { path, binds });
                    }
                });
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
        let binds: Vec<u32> = arm_binds.into_iter().flatten().collect();
        self.walk_scrutinee(scrut, &binds, live, record);
    }

    /// A place scrutinee over a `&` / `&mut` is held by what the arms bind, so
    /// the referent stays move-eligible wherever none of that is readable.
    fn walk_scrutinee(
        &mut self,
        scrut: &'a TirExpr,
        binds: &[u32],
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if let Some((op, place)) = referenced_place(scrut) {
            if record && op == TirUnaryOp::MutRef {
                self.record_mutation(place, live);
            }
            // The walk goes through the projections over the reference as well.
            if let Some(r) = self.ref_read(scrut, live, record)
                && record
                && !binds.is_empty()
            {
                self.pin_ref(op, r, place, &Kept::Into(binds.to_vec()));
            }
        } else if alias_root(scrut).is_some() {
            // A `match` projects its scrutinee rather than taking it: the arm
            // bindings are the reads, and each decides its own copy.
            self.walk_place_base(scrut, live, record);
        } else {
            self.walk_persisting(scrut, live, record);
        }
    }

    /// Close the escape set over reference bindings: an escaped reference
    /// carries the root it was taken over. Deferred, the walk being backward.
    fn propagate_escapes_to_referents(&mut self, func: &TirFunction, type_table: &TypeTable) {
        let mut work: Vec<u32> = self.ref_escaped.keys().copied().collect();
        let mut seen: IndexSet<u32> = work.iter().copied().collect();
        while let Some(local) = work.pop() {
            if !is_reference_local(func, local, type_table) {
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

    fn walk_expr(&mut self, expr: &'a TirExpr, live: &mut IndexSet<u32>, record: bool) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.read(*index, expr, live, record),
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Local { index, .. } = &target.kind {
                    if record {
                        self.record_alias(*index, value, live);
                        self.let_sources
                            .entry(*index)
                            .or_default()
                            .push((**value).clone());
                        // A whole-local assignment repoints the binding unless
                        // `mark_in_place_assigns` finds a holder of its storage.
                        let start = self.mutations.len();
                        self.mark_local_mutated(*index, true, live);
                        self.note_assign_site(expr, target, start, live);
                    }
                    live.swap_remove(index);
                    self.walk_stored_value(*index, value, live, record);
                } else {
                    if record {
                        let start = self.mutations.len();
                        self.record_assign(target, live);
                        self.note_assign_site(expr, target, start, live);
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
            TirExprKind::Call { func, args, .. } => {
                let result = self.result_kept.take().unwrap_or(Kept::Frame);
                let receiver = args.receiver().map(|r| &r.expr);
                let mutated_receiver = receiver.filter(|_| self.mutates_receiver(func));
                let exprs: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                let call_writes = self.mutations.len();
                let handed_from = self.handed.len();
                if record {
                    self.hand(&exprs);
                    if let Some(receiver) = mutated_receiver {
                        let start = self.mutations.len();
                        self.record_call_mutation(func, receiver, receiver, 0, live);
                        self.note_element_site(expr, func, receiver, start, live);
                    }
                }
                // A `&self` / `&mut self` receiver is a place the callee reads
                // through rather than a value it takes.
                let ref_receiver = receiver.is_some()
                    && self
                        .ref_receiver_methods
                        .contains(&func.module_source, &func.name);
                let kept = if record {
                    let fresh = self.returns_owned.contains(&func.module_source, &func.name);
                    self.kept_through(
                        &self.retained_by(func, args.len(), expr),
                        &exprs,
                        &result,
                        Some((expr.type_id, fresh)),
                    )
                } else {
                    Vec::new()
                };
                for (pos, arg) in args.iter().enumerate().rev() {
                    // A `&mut self` receiver mutates for the whole call, so it is
                    // what the other arguments are checked against, not one of
                    // them.
                    if record && !(pos == 0 && mutated_receiver.is_some()) {
                        self.defer_write_check(
                            &arg.expr,
                            call_writes,
                            handed_from,
                            handed_from + pos,
                        );
                    }
                    self.walk_call_arg(
                        &arg.expr,
                        Some((func, pos as u32)),
                        kept.get(pos).unwrap_or(&Kept::Transient),
                        ref_receiver && pos == 0,
                        live,
                        record,
                    );
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args.iter().rev() {
                    self.walk_call_arg(arg, None, &Kept::Transient, false, live, record);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                let result = self.result_kept.take().unwrap_or(Kept::Frame);
                let call_writes = self.mutations.len();
                let exprs: Vec<&TirExpr> = args.iter().collect();
                let handed_from = self.handed.len();
                let kept = if record {
                    // The closure called hands on what it captured.
                    self.hand(&[&**callee]);
                    self.hand(&exprs);
                    let retained = self.functor_rows.retained(callee, args.len());
                    self.kept_through(&retained, &exprs, &result, None)
                } else {
                    Vec::new()
                };
                for (pos, arg) in args.iter().enumerate().rev() {
                    if record {
                        // The callee is noted first, then each argument.
                        self.defer_write_check(
                            arg,
                            call_writes,
                            handed_from,
                            handed_from + 1 + pos,
                        );
                    }
                    self.walk_indirect_arg(
                        arg,
                        kept.get(pos).unwrap_or(&Kept::Transient),
                        live,
                        record,
                    );
                }
                self.walk_expr(callee, live, record);
            }
            // A `&`/`&mut` neither passed to a call nor stored in a local may
            // persist anywhere, so the referent escapes and stays copied.
            TirExprKind::Unary {
                op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
                expr: place,
            } => self.walk_ref(*op, place, &Kept::Frame, live, record),
            TirExprKind::StructLiteral { fields, .. } => {
                let kept = self.result_kept.take().unwrap_or(Kept::Frame);
                if record {
                    let children: Vec<&TirExpr> = fields.iter().map(|f| &f.value).collect();
                    self.collect_place_moves(&children, live);
                }
                for f in fields.iter().rev() {
                    self.walk_element(&f.value, &kept, live, record);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                let kept = self.result_kept.take().unwrap_or(Kept::Frame);
                if record {
                    let children: Vec<&TirExpr> = elements.iter().collect();
                    self.collect_place_moves(&children, live);
                }
                for e in elements.iter().rev() {
                    self.walk_element(e, &kept, live, record);
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
                let sources: Vec<u32> = capture_source_locals(captures).collect();
                for index in sources {
                    live.insert(index);
                    if record {
                        self.mark_escaped(index, None);
                        self.mark_local_mutated(index, false, live);
                        let at = self.consumed.entry(index).or_default();
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
                self.ref_read(expr, live, record);
            }
            // A newtype cast hands over the storage it wraps, so a local under
            // one is taken as the bare local would be.
            TirExprKind::Cast { .. } if local_read(expr).is_some() => {
                self.walk_expr(strip_casts(expr), live, record);
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
            // A test reads its operand and hands on nothing of it.
            TirExprKind::VariantTag { expr: inner, .. }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::TupleLen { expr: inner } => {
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

/// One value a form hands on: the referent it pins where it is a reference
/// handed on as it stands, and the local it was read out of.
struct Yielded {
    reference: Option<(u32, Option<u32>)>,
    root: Option<u32>,
}

/// What a form hands to a position outliving it, one [`Yielded`] per
/// [`yielded_leaves`] leaf.
fn yielded_values(expr: &TirExpr, type_table: &TypeTable, out: &mut Vec<Yielded>) {
    yielded_leaves(expr, type_table, &mut |leaf| {
        out.push(Yielded {
            reference: reference_escape(leaf, type_table),
            root: alias_root(leaf),
        });
    });
}

/// Hand `leaf` each expression whose value a form hands on: its own, or the
/// arm, tail and `break` values of a control form — whichever one runs — past
/// the casts and spreads that pass a value through unchanged.
fn yielded_leaves(expr: &TirExpr, type_table: &TypeTable, leaf: &mut dyn FnMut(&TirExpr)) {
    let expr = strip_casts(expr);
    match &expr.kind {
        TirExprKind::Match { arms, .. } => {
            for arm in arms {
                yielded_leaves(&arm.body, type_table, leaf);
            }
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            block_yielded_leaves(then_branch, type_table, leaf);
            if let Some(eb) = else_branch {
                block_yielded_leaves(eb, type_table, leaf);
            }
        }
        TirExprKind::Block(_) | TirExprKind::LabeledBlock { .. } => {
            for_each_block_result(expr, &mut |result| yielded_leaves(result, type_table, leaf));
        }
        TirExprKind::TupleSpread { expr: inner } => yielded_leaves(inner, type_table, leaf),
        // A scalar read out of a local carries none of its storage.
        _ if is_scalar_type(expr.type_id, type_table) => {}
        _ => leaf(expr),
    }
}

/// A block hands on its final statement's value.
fn block_yielded_leaves(block: &TirBlock, type_table: &TypeTable, leaf: &mut dyn FnMut(&TirExpr)) {
    if let Some(e) = block.tail_expr() {
        yielded_leaves(e, type_table, leaf);
    }
}

/// What a chain of projections is taken over.
fn projected_from(mut expr: &TirExpr) -> &TirExpr {
    while let Some(base) = place::projection_base(expr) {
        expr = base;
    }
    expr
}

/// The reference a place chain bottoms out in, the shape a match takes on a
/// referenced scrutinee: its op and the place it takes.
fn referenced_place(expr: &TirExpr) -> Option<(TirUnaryOp, &TirExpr)> {
    match &expr.kind {
        TirExprKind::Unary {
            op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
            expr: place,
        } => Some((*op, place)),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => referenced_place(inner),
        _ => None,
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

/// The start of the run of at least two `let`s ending before `end` that each
/// take a distinct field of one local, none binding that local.
fn field_let_run(stmts: &[TirStmt], end: usize) -> Option<usize> {
    let field_let = |stmt: &TirStmt| match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => match as_materialize(strip_casts(value)) {
            Some((base, Some(top))) if base != *local_index => Some((base, top)),
            _ => None,
        },
        _ => None,
    };
    let (base, top) = field_let(&stmts[end - 1])?;
    let mut tops = vec![top];
    let mut start = end - 1;
    while start > 0
        && let Some((b, t)) = field_let(&stmts[start - 1])
        && b == base
        && !tops.contains(&t)
    {
        tops.push(t);
        start -= 1;
    }
    (start + 1 < end).then_some(start)
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

/// Whether `expr` is a call, past the casts that hand its result on unchanged.
fn is_call(expr: &TirExpr) -> bool {
    matches!(
        strip_casts(expr).kind,
        TirExprKind::Call { .. } | TirExprKind::IndirectCall { .. }
    )
}

fn is_aggregate_literal(expr: &TirExpr) -> bool {
    matches!(
        expr.kind,
        TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::ArrayLiteral { .. }
    )
}

/// The immediate operand sub-expressions of `expr`, in evaluation order. The
/// control forms are the walker's own and never routed here.
pub(super) fn collect_child_exprs<'e>(expr: &'e TirExpr, out: &mut Vec<&'e TirExpr>) {
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
        _ => {}
    }
}

fn union(a: &IndexSet<u32>, b: &IndexSet<u32>) -> IndexSet<u32> {
    let mut out = a.clone();
    for &id in b {
        out.insert(id);
    }
    out
}

impl MoveEligible {
    /// Whether `value` is a move: a whole local read at its final use, or a
    /// materialization aliasing out of a dead aggregate. A newtype cast hands
    /// over the same storage, so it does not hide the read underneath it.
    pub fn is_move(&self, value: &TirExpr) -> bool {
        let value = strip_casts(value);
        if value.marks().moves {
            return true;
        }
        matches!(&value.kind, TirExprKind::Local { index, .. } if self.locals.contains(index))
    }

    /// The value locals every read in `value` moves. Two whole-local moves of
    /// one local lie on disjoint paths, as each is a read no path reads again
    /// after, and place moves on one path take distinct fields of a dead
    /// aggregate, so what runs hands over storage nothing reaches afterward. A
    /// reference moved hands over the reference, not what it names.
    pub fn moved_reads(&self, value: &TirExpr, type_table: &TypeTable) -> IndexSet<u32> {
        let mut reads: IndexMap<u32, bool> = IndexMap::default();
        let mut sites = ReadSites {
            moves: self,
            read: |local: u32, local_type: Option<TypeId>, moved: bool| {
                let moved = moved
                    && local_type.is_some_and(|t| {
                        needs_value_copy(t, type_table) && !place::is_reference(t, type_table)
                    });
                reads
                    .entry(local)
                    .and_modify(|all| *all &= moved)
                    .or_insert(moved);
            },
        };
        sites.visit_expr(value);
        reads
            .into_iter()
            .filter_map(|(local, all)| all.then_some(local))
            .collect()
    }

    /// Locals whose storage a move hands to a new owner. An immutable-source
    /// share rooted at one of them keeps its copy: the new owner may be mutable.
    pub fn roots(&self, func: &TirFunction) -> IndexSet<u32> {
        let mut roots = IndexSet::default();
        if let Some(body) = &func.body {
            let mut sites = ReadSites {
                moves: self,
                read: |local: u32, _: Option<TypeId>, moved: bool| {
                    if moved {
                        roots.insert(local);
                    }
                },
            };
            sites.visit_block(body);
        }
        roots
    }
}

/// The reads of locals a walk makes, each at the site that decides whether it
/// hands the local's storage over: a bare local, a projection straight off one,
/// or a deeper place that moves. `read` gets the local, its type where the site
/// names it, and whether the site is a move. A closure's body indexes locals of
/// its own, and a capture reads the one it names without moving it.
struct ReadSites<'a, F> {
    moves: &'a MoveEligible,
    read: F,
}

impl<F: FnMut(u32, Option<TypeId>, bool)> TirRefVisitor for ReadSites<'_, F> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        let stripped = strip_casts(expr);
        let moved = self.moves.is_move(stripped);
        match &stripped.kind {
            TirExprKind::Closure { captures, .. } => {
                for index in capture_source_locals(captures) {
                    (self.read)(index, None, false);
                }
            }
            TirExprKind::Local { index, .. } => (self.read)(*index, Some(stripped.type_id), moved),
            TirExprKind::FieldAccess { expr: base, .. }
            | TirExprKind::VariantPayload { expr: base, .. }
                if let Some((local, local_type)) = local_read(base) =>
            {
                (self.read)(local, Some(local_type), moved);
            }
            TirExprKind::Index { expr: base, index }
                if let Some((local, local_type)) = local_read(base) =>
            {
                (self.read)(local, Some(local_type), moved);
                self.visit_expr(index);
            }
            _ => {
                if moved && let Some(root) = place::place_root(stripped) {
                    (self.read)(root, None, true);
                }
                self.walk_expr(stripped);
            }
        }
    }
}

/// The local `expr` reads whole, and its type, through the casts
/// monomorphization leaves.
fn local_read(expr: &TirExpr) -> Option<(u32, TypeId)> {
    let expr = strip_casts(expr);
    match expr.kind {
        TirExprKind::Local { index, .. } => Some((index, expr.type_id)),
        _ => None,
    }
}
