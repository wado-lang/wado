//! Which GC-heap objects a call may read or write: [`HeapEffectsCache`]
//! summarises each function over the call graph, [`HeapFrame`] one body's objects.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use cranelift_entity::EntityRef;

use crate::builtin_facts::{SideEffect, Storage};
use crate::defs::DefId;
use crate::graph::{DisjointSets, RankedSweep, strongly_connected_components};
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::RefKind;
use crate::nir::StructFieldTypes;
use crate::nir::{FuncRef, NirFunction, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtKind};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{ValueId, ValueKind};
use crate::tir::{
    BuiltinDeclaration, BuiltinDeclarations, ObjectTypes, ResolvedType, RetainInto, RetainSpec,
    ReturnConvention, TypeId,
};
use crate::type_table::TypeTable;

use super::arena_query::holds_reference;
use super::body_memo::BodyMemo;
use crate::lower::plan::value_copy::needs_value_copy;
use crate::parallel::{Executor, lock};

/// How many depths below a parameter's own objects a [`Prov`] tells apart: a
/// parameter's objects are at depth 0, and what one load from depth `d`
/// reaches is at depth `d + 1`. The deepest stands for itself and every depth
/// past it.
const DEPTHS: usize = 8;
const DEEPEST: usize = DEPTHS - 1;

/// How many parameters a [`Prov`] tells apart; the last stands for every later
/// one.
const PARAM_SLOTS: usize = 32;

fn param_slot(index: usize) -> usize {
    index.min(PARAM_SLOTS - 1)
}

/// Where a class's objects may come from: a parameter's objects, or what a
/// chain of loads from them reaches, a bit per parameter at each depth; or a
/// global. None means allocated here.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Prov {
    elsewhere: bool,
    params: [u32; DEPTHS],
}

impl Prov {
    const ELSEWHERE: Self = Self {
        elsewhere: true,
        params: [0; DEPTHS],
    };

    fn param(index: usize) -> Self {
        let mut prov = Self::default();
        prov.params[0] = 1 << param_slot(index);
        prov
    }

    fn is_fresh(self) -> bool {
        !self.elsewhere && self.params.iter().all(|&bits| bits == 0)
    }

    fn join(&mut self, other: Self) {
        self.elsewhere |= other.elsewhere;
        for (mine, theirs) in self.params.iter_mut().zip(other.params) {
            *mine |= theirs;
        }
    }

    fn contains(self, other: Self) -> bool {
        let mut joined = self;
        joined.join(other);
        joined == self
    }

    /// What the objects one load from these reaches may come from.
    fn loaded(self) -> Self {
        let mut out = Self {
            elsewhere: self.elsewhere,
            params: [0; DEPTHS],
        };
        out.params[1..].copy_from_slice(&self.params[..DEEPEST]);
        out.params[DEEPEST] |= self.params[DEEPEST];
        out
    }

    /// The origins of `self` that `other` lacks.
    fn without(self, other: Self) -> Self {
        let mut out = Self {
            elsewhere: self.elsewhere && !other.elsewhere,
            params: self.params,
        };
        for (mine, theirs) in out.params.iter_mut().zip(other.params) {
            *mine &= !theirs;
        }
        out
    }

    /// Each parameter and depth the objects may come from.
    fn sources(self) -> impl Iterator<Item = (usize, usize)> {
        (0..DEPTHS).flat_map(move |depth| {
            let mut bits = self.params[depth];
            std::iter::from_fn(move || {
                (bits != 0).then(|| {
                    let index = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    (index, depth)
                })
            })
        })
    }

    /// The parameters the objects may come from, at any depth.
    fn params_at_any_depth(self) -> u32 {
        self.params.iter().fold(0, |all, &bits| all | bits)
    }
}

/// The object types an access reaches through each parameter at each depth,
/// and elsewhere. Kept per parameter so a call charges a callee's write to the
/// argument it goes through: `format(t)` writing through its own fresh
/// `Formatter` writes nothing `t` reaches. Kept per depth so a write to a
/// list's spine is told from one inside its elements, whose types a recursive
/// element type shares.
#[derive(Clone, Default, PartialEq, Debug)]
struct Access {
    through_params: Vec<[ObjectTypes; DEPTHS]>,
    elsewhere: ObjectTypes,
}

impl Access {
    /// Each depth below parameter `index` the access reaches something at.
    fn depths(&self, index: usize) -> impl Iterator<Item = (usize, &ObjectTypes)> {
        self.through_params
            .get(param_slot(index))
            .into_iter()
            .flat_map(|depths| depths.iter().enumerate())
            .filter(|(_, keys)| !keys.is_empty())
    }

    /// What the access reaches through parameter `index`, at any depth.
    fn through_any_depth(&self, index: usize) -> ObjectTypes {
        let mut out = ObjectTypes::default();
        for (_, keys) in self.depths(index) {
            out.union(keys);
        }
        out
    }

    fn through_mut(&mut self, index: usize, depth: usize) -> &mut ObjectTypes {
        if self.through_params.len() <= index {
            self.through_params.resize(index + 1, Default::default());
        }
        &mut self.through_params[index][depth]
    }

    fn record(&mut self, prov: Prov, keys: &ObjectTypes) {
        for (i, depth) in prov.sources() {
            self.through_mut(i, depth).union(keys);
        }
        if prov.elsewhere {
            self.elsewhere.union(keys);
        }
    }

    fn record_meet(&mut self, prov: Prov, a: &ObjectTypes, b: &ObjectTypes) {
        for (i, depth) in prov.sources() {
            self.through_mut(i, depth).union_meet(a, b);
        }
        if prov.elsewhere {
            self.elsewhere.union_meet(a, b);
        }
    }

    fn union(&mut self, other: &Access) {
        for (i, depths) in other.through_params.iter().enumerate() {
            for (depth, keys) in depths.iter().enumerate() {
                self.through_mut(i, depth).union(keys);
            }
        }
        self.elsewhere.union(&other.elsewhere);
    }
}

/// What each depth of a chain of objects may come to hold beyond what the depth
/// above it implies: the objects themselves first, then what one load from
/// them reaches, and so on.
#[derive(Clone, Default, PartialEq, Debug)]
struct Levels {
    provs: Vec<Prov>,
    /// The chain runs past [`DEEPEST`] with origins the last depth does not
    /// imply, so the last one stands for every depth past it.
    open: bool,
}

impl Levels {
    /// The levels of the chain from `start`'s class in `classes`.
    fn of(classes: &Classes, start: u32) -> Self {
        let mut raw = Vec::with_capacity(DEPTHS);
        let mut class = Some(classes.root(start));
        while raw.len() < DEPTHS
            && let Some(c) = class
        {
            raw.push(classes.prov_of(c));
            class = classes.loaded_from(c);
        }
        let mut open = false;
        if let Some(deeper) = class {
            let mut held = Prov::default();
            for c in classes.chain(deeper) {
                held.join(classes.prov_of(c));
            }
            open = !held.without(deepened(raw[DEEPEST])).is_fresh();
            if open {
                raw[DEEPEST].join(held);
            }
        }
        let mut provs: Vec<Prov> = (0..raw.len())
            .map(|d| {
                let implied = d
                    .checked_sub(1)
                    .map_or(Prov::default(), |up| raw[up].loaded());
                raw[d].without(implied)
            })
            .collect();
        if !open {
            while provs.last().is_some_and(|p| p.is_fresh()) {
                provs.pop();
            }
        }
        Self { provs, open }
    }

    fn join(&mut self, other: &Levels) {
        if self.provs.len() < other.provs.len() {
            self.provs.resize(other.provs.len(), Prov::default());
        }
        for (mine, theirs) in self.provs.iter_mut().zip(&other.provs) {
            mine.join(*theirs);
        }
        self.open |= other.open;
    }
}

/// The fields a function may write, by object type. A type mapped to `None`
/// may have any field written: a store to the whole object, or to an element.
#[derive(Clone, Default, PartialEq, Debug)]
struct FieldWrites {
    any: bool,
    by_type: IndexMap<TypeId, Option<IndexSet<u32>>>,
}

impl FieldWrites {
    fn record(&mut self, key: TypeId, field: Option<u32>) {
        if self.any {
            return;
        }
        let entry = self
            .by_type
            .entry(key)
            .or_insert_with(|| Some(IndexSet::default()));
        match (entry, field) {
            (Some(fields), Some(f)) => {
                fields.insert(f);
            }
            (entry, _) => *entry = None,
        }
    }

    fn record_types(&mut self, keys: &ObjectTypes) {
        if keys.is_any() {
            self.any = true;
            self.by_type.clear();
            return;
        }
        for k in keys.keys() {
            self.record(k, None);
        }
    }

    fn union(&mut self, other: &FieldWrites) {
        if other.any {
            self.record_types(&ObjectTypes::everything());
            return;
        }
        for (&key, fields) in &other.by_type {
            match fields {
                Some(fields) => {
                    for &f in fields {
                        self.record(key, Some(f));
                    }
                }
                None => self.record(key, None),
            }
        }
    }

    fn may_write(&self, key: TypeId, field: u32) -> bool {
        self.any
            || self
                .by_type
                .get(&key)
                .is_some_and(|fields| fields.as_ref().is_none_or(|fields| fields.contains(&field)))
    }
}

/// What one function does to objects that existed before it was called, and
/// where the references it was given may end up.
#[derive(Clone, Default, PartialEq, Debug)]
struct Summary {
    reads: Access,
    writes: Access,
    /// Which fields of those objects `writes` may write.
    field_writes: FieldWrites,
    /// What the result may share.
    ret: Levels,
    /// What each parameter's objects may come to hold.
    into_params: Vec<Levels>,
    /// What globals may come to hold.
    into_elsewhere: Prov,
}

impl Summary {
    fn join(&mut self, other: &Summary) {
        self.reads.union(&other.reads);
        self.writes.union(&other.writes);
        self.field_writes.union(&other.field_writes);
        self.ret.join(&other.ret);
        if self.into_params.len() < other.into_params.len() {
            self.into_params
                .resize(other.into_params.len(), Levels::default());
        }
        for (mine, theirs) in self.into_params.iter_mut().zip(&other.into_params) {
            mine.join(theirs);
        }
        self.into_elsewhere.join(other.into_elsewhere);
    }

    fn access(&self, effect: Effect) -> &Access {
        match effect {
            Effect::Read => &self.reads,
            Effect::Write => &self.writes,
        }
    }
}

/// How a callee is summarised.
#[derive(Debug, PartialEq)]
enum Callee {
    Body,
    /// A `core:builtin` declaration.
    Builtin(Box<BuiltinDeclaration>),
    /// No body, and nothing declared, `opaque` or `black_box`: it may read,
    /// write and keep anything it is handed. One that `suspends` also reads
    /// and writes anything held elsewhere, since other tasks run meanwhile.
    Opaque {
        suspends: bool,
    },
}

/// One function as the solver sees it.
#[derive(Debug, PartialEq)]
struct FunctionEntry {
    callee: Callee,
    closure: bool,
    /// The functions its reachable calls name.
    calls: IndexSet<usize>,
    calls_indirect: bool,
}

impl FunctionEntry {
    fn of(f: &NirFunction, declarations: &BuiltinDeclarations) -> Self {
        let mut calls = IndexSet::default();
        let mut calls_indirect = false;
        if let Some(body) = &f.body {
            body.for_each_reachable_node(|node| {
                let NodeRef::Expr(e) = node else { return };
                if let ExprKind::Call { func_id, .. } = &body.exprs[e].kind {
                    calls.insert(func_id.index());
                }
                calls_indirect |= matches!(body.exprs[e].kind, ExprKind::IndirectCall { .. });
            });
        }
        Self {
            callee: classify_callee(f, declarations),
            closure: f.body.is_some() && f.is_closure_call(),
            calls,
            calls_indirect,
        }
    }

    /// What a caller reads of this entry besides its summary.
    fn shape(&self) -> (std::mem::Discriminant<Callee>, bool) {
        (std::mem::discriminant(&self.callee), self.closure)
    }
}

/// What the summaries read of the types beyond the type table, which only
/// grows while they are kept.
#[derive(Default, PartialEq)]
struct Layouts {
    struct_fields: StructFieldTypes,
}

impl Layouts {
    fn of(project: &NirPackage) -> Self {
        Self {
            struct_fields: StructFieldTypes::new(&project.structs),
        }
    }
}

/// The call-graph-wide summaries, kept across passes no DCE separates and
/// re-solved only where a function's write count moved since. A DCE removes
/// types, and the summaries read the type table as only growing.
#[derive(Default)]
pub(super) struct HeapEffectsCache {
    layouts: Layouts,
    functions: BodyMemo<FunctionEntry>,
    summaries: Vec<Summary>,
    /// Each body's unlinked frame, built by the first summary that needs it
    /// since the body was last written.
    frames: Vec<OnceLock<HeapFrame>>,
    /// The join over every closure body, which is what an indirect call runs.
    indirect: Summary,
    /// Filled during a sweep by whichever visit asks first; its values depend
    /// only on the type, so the order changes nothing.
    reach_memo: Mutex<IndexMap<TypeId, Arc<ObjectTypes>>>,
    /// [`HeapEffects::copy_shares`]'s answers, filled as `reach_memo` is.
    copy_shares_memo: Mutex<IndexMap<TypeId, bool>>,
    /// Where `assert_one_settled` resumes its rotation.
    #[cfg(debug_assertions)]
    cursor: usize,
}

impl HeapEffectsCache {
    /// The summaries of `project` as it stands.
    pub(super) fn effects<'t>(
        &'t mut self,
        project: &NirPackage,
        type_table: &'t TypeTable,
        exec: &Executor,
    ) -> HeapEffects<'t> {
        self.refresh(project, type_table, exec);
        HeapEffects {
            type_table,
            cache: self,
        }
    }

    fn refresh(&mut self, project: &NirPackage, type_table: &TypeTable, exec: &Executor) {
        let layouts = Layouts::of(project);
        if self.layouts != layouts {
            *self = Self {
                layouts,
                ..Self::default()
            };
        }
        let len = project.functions.len();
        let mut edited = vec![false; len];
        // Whether a node's answer to its callers moved; the last is the indirect one.
        let mut moved = vec![false; len + 1];
        let declarations = &project.builtin_declarations;
        self.functions.refresh_observing(
            project,
            exec,
            |f| FunctionEntry::of(f, declarations),
            |i, old, new| {
                edited[i] = true;
                moved[i] = old.is_none_or(|old| old.shape() != new.shape());
            },
        );
        self.summaries.resize_with(len, Summary::default);
        self.frames.resize_with(len, OnceLock::new);
        for (frame, &edited) in self.frames.iter_mut().zip(&edited) {
            if edited {
                *frame = OnceLock::new();
            }
        }
        if edited.contains(&true) {
            self.solve(&project.functions, type_table, exec, &edited, &mut moved);
        }
        #[cfg(debug_assertions)]
        self.assert_one_settled(&project.functions, type_table);
    }

    /// The least fixpoint over the call graph, callees first; a component whose
    /// inputs all stand is the fixpoint already. Components no path joins are
    /// solved together, in ranked sweeps whose batches each run in parallel,
    /// so the outcome does not depend on the thread count.
    fn solve(
        &mut self,
        functions: &[FuncRef],
        type_table: &TypeTable,
        exec: &Executor,
        edited: &[bool],
        moved: &mut [bool],
    ) {
        let indirect = self.functions.facts().len();
        let successors = self.successors();
        let components = strongly_connected_components(&successors);
        let mut component_of = vec![0; successors.len()];
        for (c, members) in components.iter().enumerate() {
            for &n in members {
                component_of[n] = c;
            }
        }
        let mut sweep = RankedSweep::new(&successors, &component_of);
        // A component's depth is one past its deepest callee component's, so
        // one depth's components reach none of each other.
        let mut depth = vec![0; components.len()];
        let mut by_depth: Vec<Vec<usize>> = Vec::new();
        for (c, members) in components.iter().enumerate() {
            depth[c] = members
                .iter()
                .flat_map(|&n| &successors[n])
                .map(|&s| component_of[s])
                .filter(|&d| d != c)
                .map(|d| {
                    assert!(d < c, "a callee's component must precede its caller's");
                    depth[d] + 1
                })
                .max()
                .unwrap_or(0);
            if by_depth.len() <= depth[c] {
                by_depth.resize_with(depth[c] + 1, Vec::new);
            }
            by_depth[depth[c]].push(c);
        }
        for level in by_depth {
            let pending: Vec<usize> = level
                .into_iter()
                .filter(|&c| {
                    components[c].iter().any(|&n| {
                        (n != indirect && edited[n]) || successors[n].iter().any(|&s| moved[s])
                    })
                })
                .flat_map(|c| components[c].iter().copied())
                .collect();
            let before: Vec<(usize, Summary)> = pending
                .iter()
                .map(|&n| (n, std::mem::take(self.summary_mut(n))))
                .collect();
            sweep.solve(&pending, |batch| {
                let this = &*self;
                let fresh = exec.map(batch, |&n| this.summarize(functions, type_table, n, true));
                let mut changed = Vec::new();
                for (&n, summary) in batch.iter().zip(fresh) {
                    if summary != *self.summary_mut(n) {
                        *self.summary_mut(n) = summary;
                        changed.push(n);
                    }
                }
                changed
            });
            for (n, before) in before {
                moved[n] |= *self.summary_mut(n) != before;
            }
        }
    }

    /// Each node's callees, the node past the last function standing for what
    /// every indirect call runs.
    fn successors(&self) -> Vec<Vec<usize>> {
        let entries = self.functions.facts();
        let indirect = entries.len();
        let mut successors: Vec<Vec<usize>> = entries
            .iter()
            .map(|f| match f.callee {
                Callee::Body => f
                    .calls
                    .iter()
                    .copied()
                    .chain(f.calls_indirect.then_some(indirect))
                    .collect(),
                Callee::Builtin(_) | Callee::Opaque { .. } => Vec::new(),
            })
            .collect();
        successors.push(self.closure_bodies().collect());
        successors
    }

    fn closure_bodies(&self) -> impl Iterator<Item = usize> + '_ {
        let entries = self.functions.facts();
        (0..entries.len()).filter(|&i| entries[i].closure)
    }

    fn summary_mut(&mut self, n: usize) -> &mut Summary {
        if n == self.summaries.len() {
            &mut self.indirect
        } else {
            &mut self.summaries[n]
        }
    }

    /// What node `n` gives its callers, read against the summaries as they
    /// stand, from its cached frame or, where `cached` is false, a fresh one.
    fn summarize(
        &self,
        functions: &[FuncRef],
        type_table: &TypeTable,
        n: usize,
        cached: bool,
    ) -> Summary {
        let entries = self.functions.facts();
        if n == entries.len() {
            let mut indirect = Summary::default();
            for c in self.closure_bodies() {
                indirect.join(&self.summaries[c]);
            }
            return indirect;
        }
        match entries[n].callee {
            Callee::Body => {
                let effects = HeapEffects {
                    type_table,
                    cache: self,
                };
                let f = functions[n].borrow();
                let body = f.body.as_ref().expect("a `Callee::Body` has a body");
                let build = || {
                    let params: Vec<u32> = f.params.iter().map(|p| p.local_index).collect();
                    HeapFrame::unlinked(&effects, body, &params)
                };
                let fresh;
                let frame = if cached {
                    self.frames[n].get_or_init(build)
                } else {
                    fresh = build();
                    &fresh
                };
                let classes = frame.link(frame.classes.clone(), &effects, body);
                frame.summary(&classes, &effects, body)
            }
            Callee::Builtin(_) | Callee::Opaque { .. } => Summary::default(),
        }
    }

    /// A summary the solver left behind a moved callee, or a cached frame its
    /// body outgrew, no longer answers for its function. One node per refresh,
    /// rotating, keeps the check cheap.
    #[cfg(debug_assertions)]
    fn assert_one_settled(&mut self, functions: &[FuncRef], type_table: &TypeTable) {
        let nodes = self.summaries.len() + 1;
        self.cursor %= nodes;
        let n = self.cursor;
        self.cursor += 1;
        let summary = self.summarize(functions, type_table, n, false);
        assert!(
            summary == *self.summary_mut(n),
            "the heap-effect summary of node {n} went stale"
        );
    }
}

/// The call-graph-wide summaries every [`HeapFrame`] reads.
#[derive(Clone, Copy)]
pub(super) struct HeapEffects<'t> {
    pub(super) type_table: &'t TypeTable,
    cache: &'t HeapEffectsCache,
}

impl HeapEffects<'_> {
    /// Whether what `ty` holds may be a reference: an array's elements, or
    /// anything else's contents. When `copied`, the elements are value-copied,
    /// which hands on only what such a copy shares.
    fn elements_hold_reference(&self, ty: TypeId, copied: bool) -> bool {
        let tt = self.type_table;
        let ResolvedType::BuiltinArray(element) = tt.get(tt.strip_handles(ty)) else {
            return true;
        };
        holds_reference(tt, *element) && (!copied || self.copy_shares(*element))
    }

    /// Whether a value copy of `ty` may hold an object its source holds: one
    /// it hands on as is, such as a reference's referent, a closure's
    /// environment or an affine value.
    fn copy_shares(&self, ty: TypeId) -> bool {
        if let Some(&hit) = lock(&self.cache.copy_shares_memo).get(&ty) {
            return hit;
        }
        let answer = self.copy_shares_walk(ty, &mut IndexSet::default());
        lock(&self.cache.copy_shares_memo).insert(ty, answer);
        answer
    }

    fn copy_shares_walk(&self, ty: TypeId, seen: &mut IndexSet<TypeId>) -> bool {
        let tt = self.type_table;
        let ty = tt.representation_head(ty);
        if !holds_reference(tt, ty) {
            return false;
        }
        if !needs_value_copy(ty, tt) {
            return true;
        }
        if !seen.insert(ty) {
            return false;
        }
        match tt.get(ty) {
            ResolvedType::Tuple(elems) => elems.iter().any(|&e| self.copy_shares_walk(e, seen)),
            ResolvedType::Struct { .. } => self
                .struct_field_types(ty)
                .is_none_or(|fields| fields.iter().any(|&f| self.copy_shares_walk(f, seen))),
            ResolvedType::BuiltinArray(element) => self.copy_shares_walk(*element, seen),
            ResolvedType::Variant { def, type_args } => {
                tt.variant_template_cases(*def).is_none_or(|cases| {
                    cases.iter().any(|&(_, _, payload)| {
                        if !tt.contains_type_param(payload) {
                            return self.copy_shares_walk(payload, seen);
                        }
                        tt.type_param_id(payload)
                            .and_then(|id| type_args.get(id.index as usize))
                            .is_none_or(|&arg| self.copy_shares_walk(arg, seen))
                    })
                })
            }
            _ => true,
        }
    }

    /// Whether the retain `r` of `declaration` may carry a reference out of
    /// `args`.
    fn retains_reference(
        &self,
        body: &Body,
        declaration: &BuiltinDeclaration,
        r: &RetainSpec<usize>,
        source: Option<Operand>,
    ) -> bool {
        let copied = matches!(declaration.facts.storage, Storage::CopiesArgs);
        !r.elements
            || source.is_none_or(|a| self.elements_hold_reference(body.operand_type(a), copied))
    }

    /// Every object type `call` may write that its caller can reach: through
    /// an argument, or held elsewhere.
    fn call_writes(&self, body: &Body, call: ExprId) -> ObjectTypes {
        if !self.returns(body, call) {
            return ObjectTypes::default();
        }
        let (target, args) = call_parts(self, body, call);
        let mut out = target.elsewhere(Effect::Write).into_owned();
        if out.is_any() {
            return out;
        }
        for (j, &a) in args.iter().enumerate() {
            let ty = body.operand_type(a);
            match target {
                Target::Summary(t) => {
                    out.union_meet(&t.writes.through_any_depth(j), &self.reach(ty));
                }
                Target::Builtin(declaration) if declaration.mut_params.contains(&j) => {
                    out.union(&builtin_touches(self, declaration, ty).0);
                }
                Target::Builtin(_) => {}
                Target::Opaque { .. } => out.union(&self.reach(ty)),
            }
        }
        out
    }

    /// Whether the function at `index` may write an object of type `ty` it did
    /// not allocate, where code after the write may read it.
    pub(super) fn function_may_write(&self, index: usize, ty: TypeId) -> bool {
        let writes = &self.cache.summaries[index].writes;
        self.type_table.heap_object_key(ty).is_none_or(|key| {
            writes
                .through_params
                .iter()
                .flatten()
                .any(|keys| keys.contains(key))
                || writes.elsewhere.contains(key)
        })
    }

    /// Whether `call` may write field `field` of a `key` object it did not
    /// allocate. [`HeapFrame::call_may`] says whether it may write the object.
    pub(super) fn call_may_write_field(
        &self,
        body: &Body,
        call: ExprId,
        key: TypeId,
        field: u32,
    ) -> bool {
        if !self.returns(body, call) {
            return false;
        }
        match call_target(self, body, call) {
            Target::Summary(t) => t.field_writes.may_write(key, field),
            Target::Builtin(_) | Target::Opaque { .. } => true,
        }
    }

    /// Whether `call` may return. One typed `!` traps or runs forever, since a
    /// handler resumes its caller rather than unwinding past it, so no code
    /// after it observes what it writes.
    fn returns(&self, body: &Body, call: ExprId) -> bool {
        !self.type_table.is_never(body.exprs[call].type_id)
    }

    /// [`Self::call_writes`] of every call `body` reaches.
    pub(super) fn body_call_writes(&self, body: &Body) -> IndexMap<ExprId, ObjectTypes> {
        let mut out = IndexMap::default();
        body.for_each_reachable_node(|node| {
            if let NodeRef::Expr(e) = node
                && matches!(
                    body.exprs[e].kind,
                    ExprKind::Call { .. } | ExprKind::IndirectCall { .. }
                )
            {
                out.insert(e, self.call_writes(body, e));
            }
        });
        out
    }

    /// Every object type an element of the list `ty` may reach, or `None` where
    /// `ty` is no list.
    pub(super) fn element_reach(&self, ty: TypeId) -> Option<Arc<ObjectTypes>> {
        let tt = self.type_table;
        let ty = tt.strip_handles(ty);
        let ResolvedType::Struct { type_args, .. } = tt.get(ty) else {
            return None;
        };
        let [element] = type_args.as_slice() else {
            return None;
        };
        tt.is_list(ty).then(|| self.reach(*element))
    }

    /// Every object type a value of `ty` may reach, itself included.
    pub(super) fn reach(&self, ty: TypeId) -> Arc<ObjectTypes> {
        if let Some(hit) = lock(&self.cache.reach_memo).get(&ty) {
            return Arc::clone(hit);
        }
        let mut out = ObjectTypes::default();
        let mut seen = IndexSet::default();
        self.reach_into(ty, &mut seen, &mut out);
        let out = Arc::new(out);
        lock(&self.cache.reach_memo).insert(ty, Arc::clone(&out));
        out
    }

    fn reach_into(&self, ty: TypeId, seen: &mut IndexSet<TypeId>, out: &mut ObjectTypes) {
        if out.is_any() {
            return;
        }
        let tt = self.type_table;
        let ty = tt.strip_handles(ty);
        if !seen.insert(ty) {
            return;
        }
        match tt.get(ty) {
            ResolvedType::Tuple(elems) => {
                out.insert(ty);
                for &elem in elems {
                    self.reach_into(elem, seen, out);
                }
            }
            ResolvedType::Struct { .. } => {
                out.insert(ty);
                if let Some(fields) = self.struct_field_types(ty) {
                    for &f in fields {
                        self.reach_into(f, seen, out);
                    }
                } else {
                    out.set_any();
                }
            }
            ResolvedType::Variant { def, type_args } => {
                out.insert(ty);
                match tt.variant_template_cases(*def) {
                    Some(cases) => {
                        for &(_, _, payload) in cases {
                            self.reach_template(payload, *def, type_args, seen, out);
                        }
                    }
                    None => {
                        out.set_any();
                    }
                }
            }
            ResolvedType::BuiltinArray(element) => {
                out.insert(ty);
                self.reach_into(*element, seen, out);
            }
            // A closure's environment holds whatever it captured.
            ResolvedType::Function { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::AssocParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::PackElement { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => {
                out.set_any();
            }
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::Flags { .. } => {}
            ResolvedType::Ref(payload) | ResolvedType::MutRef(payload) => {
                out.insert(tt.cell_key(ty));
                self.reach_into(*payload, seen, out);
            }
            ResolvedType::Newtype { .. } => {
                unreachable!("strip_handles removes every newtype")
            }
            ResolvedType::InferVar(var) => panic!("{var} reached heap-effect analysis"),
        }
    }

    /// A variant case's payload, written over `variant`'s parameters.
    fn reach_template(
        &self,
        payload: TypeId,
        variant: DefId,
        args: &[TypeId],
        seen: &mut IndexSet<TypeId>,
        out: &mut ObjectTypes,
    ) {
        let tt = self.type_table;
        if !tt.contains_type_param(payload) {
            self.reach_into(payload, seen, out);
            return;
        }
        let arg_of = |param: TypeId| {
            let id = tt.type_param_id(param)?;
            assert_eq!(
                id.owner, variant,
                "a payload names its variant's parameters"
            );
            args.get(id.index as usize).copied()
        };
        match tt.get(payload) {
            ResolvedType::TypeParam { .. } => match arg_of(payload) {
                Some(arg) => self.reach_into(arg, seen, out),
                None => {
                    out.set_any();
                }
            },
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) => {
                let pointee = tt.peel_refs(payload);
                if !matches!(tt.get(pointee), ResolvedType::TypeParam { .. }) {
                    out.set_any();
                    return;
                }
                let Some(arg) = arg_of(pointee) else {
                    out.set_any();
                    return;
                };
                // A table without the instance holds no value of it, so no
                // cell its layers would be.
                let reached = instantiated_reference(tt, payload, arg).unwrap_or(arg);
                self.reach_into(reached, seen, out);
            }
            _ => {
                out.set_any();
            }
        }
    }

    /// Whether a value of type `part` is what one load from an object of type
    /// `holder` may read: an element of an array or a tuple, or a field of a
    /// struct.
    fn loads_as(&self, holder: TypeId, part: TypeId) -> bool {
        let tt = self.type_table;
        let part = referent(tt, part);
        let is_part = |ty: &TypeId| referent(tt, *ty) == part;
        let holder = referent(tt, holder);
        match tt.get(holder) {
            ResolvedType::BuiltinArray(element) => is_part(element),
            ResolvedType::Tuple(elements) => elements.iter().any(is_part),
            ResolvedType::Struct { .. } => self
                .struct_field_types(holder)
                .is_some_and(|fields| fields.iter().any(is_part)),
            _ => false,
        }
    }

    /// A struct's field types, the instance of a generic one included.
    fn struct_field_types(&self, ty: TypeId) -> Option<&[TypeId]> {
        self.cache
            .layouts
            .struct_fields
            .of_type(ty, self.type_table)
    }

    /// The type of field `field` of the struct `ty` names, where it is known.
    fn pattern_field_type(&self, ty: Option<TypeId>, field: usize) -> Option<TypeId> {
        let ty = self.type_table.strip_handles(ty?);
        self.struct_field_types(ty)?.get(field).copied()
    }

    fn tuple_element_type(&self, ty: Option<TypeId>, index: usize) -> Option<TypeId> {
        let ty = self.type_table.strip_handles(ty?);
        self.type_table.as_tuple(ty)?.get(index).copied()
    }
}

/// `payload`, a chain of references to a type parameter, with `arg` for the
/// parameter, if the table holds it.
fn instantiated_reference(tt: &TypeTable, payload: TypeId, arg: TypeId) -> Option<TypeId> {
    match tt.get(payload) {
        ResolvedType::Ref(inner) => {
            tt.find_ref(RefKind::Shared, instantiated_reference(tt, *inner, arg)?)
        }
        ResolvedType::MutRef(inner) => {
            tt.find_ref(RefKind::Mut, instantiated_reference(tt, *inner, arg)?)
        }
        _ => Some(arg),
    }
}

fn classify_callee(f: &NirFunction, declarations: &BuiltinDeclarations) -> Callee {
    if f.body.is_some() {
        return Callee::Body;
    }
    match declarations.get(f) {
        Some(declaration) => match declaration.facts.side_effect {
            SideEffect::Listed { .. } => Callee::Builtin(Box::new(declaration.clone())),
            SideEffect::Opaque | SideEffect::BlackBox => Callee::Opaque {
                suspends: declaration.facts.suspend,
            },
        },
        None => Callee::Opaque { suspends: true },
    }
}

/// Whether a call is asked about what it reads or what it writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Effect {
    Read,
    Write,
}

/// The node an operand's objects belong to, where it has one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OperandNode {
    /// A scalar, or an object allocated where it is read: nothing else shares it.
    None,
    Node(u32),
    /// Built after the frame, so no class answers for it.
    Unknown,
}

impl OperandNode {
    fn node(self) -> Option<u32> {
        match self {
            OperandNode::Node(n) => Some(n),
            OperandNode::None | OperandNode::Unknown => None,
        }
    }
}

enum Keys {
    One(TypeId),
    Set(Arc<ObjectTypes>),
}

impl Keys {
    fn of(effects: &HeapEffects, ty: Option<TypeId>) -> Self {
        ty.and_then(|t| effects.type_table.heap_object_key(t))
            .map_or_else(|| Keys::Set(Arc::new(ObjectTypes::everything())), Keys::One)
    }

    fn contains(&self, key: TypeId) -> bool {
        match self {
            Keys::One(k) => *k == key,
            Keys::Set(set) => set.contains(key),
        }
    }

    fn meets(&self, set: &ObjectTypes) -> bool {
        match self {
            Keys::One(k) => set.contains(*k),
            Keys::Set(mine) => mine.meets(set),
        }
    }

    /// Whether some key is in `self`, `a` and `b` alike.
    fn meets_both(&self, a: &ObjectTypes, b: &ObjectTypes) -> bool {
        match self {
            Keys::One(k) => a.contains(*k) && b.contains(*k),
            Keys::Set(mine) => {
                let mut both = ObjectTypes::default();
                both.union_meet(a, b);
                mine.meets(&both)
            }
        }
    }
}

/// One read or write of an object: where it happens, through which operand,
/// and which field, where it names one.
struct AccessSite {
    effect: Effect,
    node: u32,
    keys: Keys,
    site: NodeRef,
    receiver: Option<Operand>,
    field: Option<u32>,
}

/// A union-find over a frame's nodes, where a class holds what flows together
/// and its root carries the origins reaching it, and the class a load from its
/// objects reaches: a field, an element or a payload of theirs, or what a cell
/// holds. A reference that is no cell is its referent's own handle, so it is in
/// its referent's class.
#[derive(Clone, Default)]
struct Classes {
    sets: DisjointSets,
    prov: Vec<Prov>,
    /// At a root, the node of the class a load reaches, or [`NO_NODE`] where
    /// nothing was loaded or stored there.
    loads: Vec<u32>,
}

/// Some objects a call or a store reaches: a class, or objects below the
/// deepest class `under` a chain of loads has in the frame, which come from
/// where `prov` says.
#[derive(Clone, Copy)]
enum Level {
    Class(u32),
    Beyond { under: u32, prov: Prov },
}

impl Classes {
    fn new(len: usize) -> Self {
        Self {
            sets: DisjointSets::new(len),
            prov: vec![Prov::default(); len],
            loads: vec![NO_NODE; len],
        }
    }

    fn push(&mut self) -> u32 {
        self.prov.push(Prov::default());
        self.loads.push(NO_NODE);
        self.sets.push()
    }

    fn root(&self, n: u32) -> u32 {
        self.sets.root(n)
    }

    /// The origins of `n`'s class.
    fn prov_of(&self, n: u32) -> Prov {
        self.prov[self.root(n) as usize]
    }

    /// Join `a`'s class and `b`'s, and so what a load reaches from each.
    fn unify(&mut self, a: u32, b: u32) {
        let mut pending = vec![(a, b)];
        while let Some((a, b)) = pending.pop() {
            let Some((absorbed, kept)) = self.sets.union(b, a) else {
                continue;
            };
            let absorbed_prov = self.prov[absorbed as usize];
            self.prov[kept as usize].join(absorbed_prov);
            let moved = self.loads[absorbed as usize];
            if moved != NO_NODE {
                match self.loads[kept as usize] {
                    NO_NODE => self.loads[kept as usize] = moved,
                    held => pending.push((held, moved)),
                }
            }
        }
    }

    fn unify_nodes(&mut self, a: OperandNode, b: OperandNode) {
        if let (OperandNode::Node(a), OperandNode::Node(b)) = (a, b) {
            self.unify(a, b);
        }
    }

    /// The class a load from `n`'s objects reaches, made where there is none.
    fn load(&mut self, n: u32) -> u32 {
        let r = self.sets.find(n);
        match self.loads[r as usize] {
            NO_NODE => {
                let loaded = self.push();
                self.loads[r as usize] = loaded;
                loaded
            }
            loaded => self.sets.find(loaded),
        }
    }

    /// [`Self::load`] of an operand's node, where it has one.
    fn load_operand(&mut self, n: OperandNode) -> OperandNode {
        match n {
            OperandNode::Node(n) => OperandNode::Node(self.load(n)),
            other => other,
        }
    }

    /// The class `depth` loads from `n`'s objects reach, made where there is
    /// none.
    fn load_n(&mut self, n: u32, depth: usize) -> u32 {
        (0..depth).fold(n, |n, _| self.load(n))
    }

    /// Make every load below `n`'s class reach that class: what an access at
    /// [`DEEPEST`] stands for, or what nothing says the depth of.
    fn collapse(&mut self, n: u32) {
        let loaded = self.load(n);
        self.unify(n, loaded);
    }

    /// The class a load from `n`'s objects reaches, where the frame has one.
    fn loaded_from(&self, n: u32) -> Option<u32> {
        let loaded = self.loads[self.root(n) as usize];
        (loaded != NO_NODE).then(|| self.root(loaded))
    }

    /// The classes `n`'s objects are in or reach, `n`'s first.
    fn chain(&self, n: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut class = Some(self.root(n));
        while let Some(c) = class
            && !chain.contains(&c)
        {
            chain.push(c);
            class = self.loaded_from(c);
        }
        chain
    }

    /// The objects `depth` loads from `n`'s reach, and where `below`, every
    /// object a load from them reaches.
    fn levels(&self, n: u32, depth: usize, below: bool) -> Vec<Level> {
        let mut class = self.root(n);
        for reached in 0..depth {
            let Some(loaded) = self.loaded_from(class) else {
                let prov = (reached..depth).fold(self.prov_of(class), |p, _| p.loaded());
                let prov = if below { deepened(prov) } else { prov };
                return vec![Level::Beyond { under: class, prov }];
            };
            class = loaded;
        }
        if !below {
            return vec![Level::Class(class)];
        }
        let chain = self.chain(class);
        let last = *chain.last().expect("a chain holds its start");
        let mut levels: Vec<Level> = chain.into_iter().map(Level::Class).collect();
        if self.loaded_from(last).is_none() {
            levels.push(Level::Beyond {
                under: last,
                prov: deepened(self.prov_of(last).loaded()),
            });
        }
        levels
    }

    fn level_prov(&self, level: Level) -> Prov {
        match level {
            Level::Class(c) => self.prov_of(c),
            Level::Beyond { prov, .. } => prov,
        }
    }

    /// Hand each class's origins on to what a load from it reaches.
    fn settle(&mut self) {
        let mut pending: Vec<u32> = (0..self.loads.len() as u32)
            .filter(|&n| self.root(n) == n && !self.prov[n as usize].is_fresh())
            .collect();
        while let Some(n) = pending.pop() {
            let Some(loaded) = self.loaded_from(n) else {
                continue;
            };
            let handed = self.prov[n as usize].loaded();
            if !self.prov[loaded as usize].contains(handed) {
                self.prov[loaded as usize].join(handed);
                pending.push(loaded);
            }
        }
    }
}

/// `prov` joined with what every chain of loads from objects of its origins
/// reaches.
fn deepened(mut prov: Prov) -> Prov {
    loop {
        let next = prov.loaded();
        if prov.contains(next) {
            return prov;
        }
        prov.join(next);
    }
}

/// One body's objects as [`Classes`] over its locals and expressions.
pub(super) struct HeapFrame {
    classes: Classes,
    local_nodes: Vec<u32>,
    /// Locals whose definition the walk reached. A caller may query a body it
    /// is midway through rewriting: LICM reads a hoist's local before linking
    /// the hoist's `let` in.
    defined: IndexSet<u32>,
    /// Some local was read with no definition the walk reached. Such a local
    /// joins the `ELSEWHERE` class, which may then hold an object of any class,
    /// fresh ones included.
    partial: bool,
    /// Each parameter's node, in parameter order.
    params: Vec<u32>,
    expr_base: u32,
    expr_count: usize,
    /// Accesses to classify once every class is final.
    accesses: Vec<AccessSite>,
    calls: Vec<ExprId>,
    /// Each call's result node and its span of `call_args`, in `calls` order.
    call_nodes: Vec<(OperandNode, Range<usize>)>,
    /// Each argument, in the callee's parameter order, with its node.
    call_args: Vec<(Operand, OperandNode)>,
    /// Assignment targets: written, not read.
    targets: IndexSet<ExprId>,
}

/// A body's [`HeapFrame`], built on the first query against the body as it
/// stands then.
pub(super) struct LazyHeapFrame<'e, 't> {
    pub(super) effects: &'e HeapEffects<'t>,
    params: Vec<u32>,
    frame: OnceCell<HeapFrame>,
}

impl<'e, 't> LazyHeapFrame<'e, 't> {
    /// For a body whose parameters are the locals `params`.
    pub(super) fn new(effects: &'e HeapEffects<'t>, params: Vec<u32>) -> Self {
        Self {
            effects,
            params,
            frame: OnceCell::new(),
        }
    }

    pub(super) fn get(&self, body: &Body) -> &HeapFrame {
        self.frame
            .get_or_init(|| HeapFrame::new(self.effects, body, &self.params))
    }
}

const NO_NODE: u32 = u32::MAX;
const ELSEWHERE: u32 = 0;
const RET: u32 = 1;

impl HeapFrame {
    /// The classes of `body`, whose parameters are the locals `params`.
    pub(super) fn new(effects: &HeapEffects, body: &Body, params: &[u32]) -> Self {
        let mut frame = Self::unlinked(effects, body, params);
        let unlinked = std::mem::take(&mut frame.classes);
        frame.classes = frame.link(unlinked, effects, body);
        frame
    }

    /// The frame before [`Self::link`]: what the body alone joins, read from
    /// nothing in the summaries, so it stands as long as the body does.
    fn unlinked(effects: &HeapEffects, body: &Body, params: &[u32]) -> Self {
        let expr_count = body.exprs.len();
        let expr_base = 2;
        let mut frame = Self {
            classes: Classes::new(expr_base as usize + expr_count),
            local_nodes: Vec::new(),
            defined: IndexSet::default(),
            partial: false,
            params: Vec::with_capacity(params.len()),
            expr_base,
            expr_count,
            accesses: Vec::new(),
            calls: Vec::new(),
            call_nodes: Vec::new(),
            call_args: Vec::new(),
            targets: IndexSet::default(),
        };
        frame.classes.prov[ELSEWHERE as usize] = Prov::ELSEWHERE;
        // A global reaches only what other globals may.
        frame.classes.loads[ELSEWHERE as usize] = ELSEWHERE;
        for (k, &local) in params.iter().enumerate() {
            let n = frame.define_local(local);
            frame.classes.prov[n as usize].join(Prov::param(k));
            frame.params.push(n);
        }
        let mut nodes = Vec::new();
        body.for_each_reachable_node(|node| nodes.push(node));
        for node in nodes {
            let mut seen_values = IndexSet::default();
            body.for_each_operand(node, |op| {
                if let Some(v) = op.as_value() {
                    frame.value_reads(effects, body, node, v, &mut seen_values);
                }
            });
            match node {
                NodeRef::Expr(e) => frame.expr(effects, body, e),
                NodeRef::Stmt(s) => match &body.stmts[s].kind {
                    StmtKind::Let {
                        local_index, value, ..
                    } => {
                        let l = frame.define_local(*local_index);
                        if let Some(value) = *value {
                            frame.unify_op(effects, body, l, value);
                        }
                    }
                    StmtKind::Return { value: Some(v) } => frame.unify_op(effects, body, RET, *v),
                    StmtKind::LetDestructure { pattern, value, .. } => {
                        let scrutinee = frame.operand_node(effects, body, *value);
                        let ty = body.operand_type(*value);
                        frame.pattern(effects, body, *pattern, scrutinee, Some(ty));
                    }
                    StmtKind::Expr(_)
                    | StmtKind::Return { value: None }
                    | StmtKind::If { .. }
                    | StmtKind::Loop { .. }
                    | StmtKind::Break { .. }
                    | StmtKind::Continue
                    | StmtKind::LabeledBlock { .. } => {}
                },
                NodeRef::Block(_) | NodeRef::Pat(_) => {}
            }
        }
        if let Some(tail) = body.block_tail(body.root()) {
            frame.unify_op(effects, body, RET, tail);
        }
        for i in 0..frame.local_nodes.len() {
            let n = frame.local_nodes[i];
            if n != NO_NODE && !frame.defined.contains(&(i as u32)) {
                frame.unify(n, ELSEWHERE);
                frame.partial = true;
            }
        }
        frame
    }

    /// This frame's `classes` once each call joins what it makes share, as its
    /// callee answers now.
    fn link(&self, mut classes: Classes, effects: &HeapEffects, body: &Body) -> Classes {
        for (&e, (result, span)) in self.calls.iter().zip(&self.call_nodes) {
            Self::call_flows(
                &mut classes,
                effects,
                body,
                e,
                *result,
                &self.call_args[span.clone()],
            );
        }
        classes.sets.flatten();
        classes.settle();
        classes
    }

    fn local_node(&mut self, local: u32) -> u32 {
        let i = local as usize;
        if i >= self.local_nodes.len() {
            self.local_nodes.resize(i + 1, NO_NODE);
        }
        if self.local_nodes[i] == NO_NODE {
            self.local_nodes[i] = self.classes.push();
        }
        self.local_nodes[i]
    }

    fn define_local(&mut self, local: u32) -> u32 {
        self.defined.insert(local);
        self.local_node(local)
    }

    fn expr_node(&self, e: ExprId) -> Option<u32> {
        (e.index() < self.expr_count).then(|| self.expr_base + e.index() as u32)
    }

    fn unify(&mut self, a: u32, b: u32) {
        self.classes.unify(a, b);
    }

    fn unify_op(&mut self, effects: &HeapEffects, body: &Body, n: u32, op: Operand) {
        if let OperandNode::Node(m) = self.operand_node(effects, body, op) {
            self.unify(n, m);
        }
    }

    fn unify_nodes(&mut self, a: OperandNode, b: OperandNode) {
        self.classes.unify_nodes(a, b);
    }

    /// The node for an operand while building.
    fn operand_node(&mut self, effects: &HeapEffects, body: &Body, op: Operand) -> OperandNode {
        match op {
            Operand::Expr(e) => {
                if !holds_reference(effects.type_table, body.exprs[e].type_id) {
                    return OperandNode::None;
                }
                self.expr_node(e)
                    .map_or(OperandNode::Unknown, OperandNode::Node)
            }
            Operand::Value(v) => self.value_node(effects, body, v),
        }
    }

    /// The node for a promoted value while building: a field read is a load
    /// from its receiver, and a merge joins its inputs.
    fn value_node(&mut self, effects: &HeapEffects, body: &Body, v: ValueId) -> OperandNode {
        if !value_holds_reference(effects, body, v) {
            return OperandNode::None;
        }
        let join = |frame: &mut Self, inputs: &[ValueId]| {
            let mut joined = OperandNode::None;
            for &input in inputs {
                let n = frame.value_node(effects, body, input);
                frame.unify_nodes(joined, n);
                if joined == OperandNode::None {
                    joined = n;
                }
            }
            joined
        };
        match *body.values.kind(v) {
            ValueKind::Opaque(oid) => {
                OperandNode::Node(self.local_node(body.values.promoted_opaque_local(oid)))
            }
            ValueKind::FieldAccess { receiver, .. } => {
                let receiver = self.value_node(effects, body, receiver);
                self.classes.load_operand(receiver)
            }
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                self.value_node(effects, body, operand)
            }
            ValueKind::Binary { lhs, rhs, .. } => join(self, &[lhs, rhs]),
            ValueKind::Select { then, else_, .. } => join(self, &[then, else_]),
            ValueKind::LoopPhi { entry, body_iter } => join(self, &[entry, body_iter]),
            ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::UnitCase { .. }
            | ValueKind::Const(..) => OperandNode::None,
        }
    }

    /// The class a load from `op`'s objects reaches, where `op` has any.
    fn operand_load(&mut self, effects: &HeapEffects, body: &Body, op: Operand) -> OperandNode {
        let n = self.operand_node(effects, body, op);
        self.classes.load_operand(n)
    }

    /// Join `n` with what a load from `op` reaches.
    fn unify_load(&mut self, effects: &HeapEffects, body: &Body, n: u32, op: Operand) {
        if let OperandNode::Node(m) = self.operand_load(effects, body, op) {
            self.unify(n, m);
        }
    }

    /// Join what a load from `n` reaches with `op`.
    fn store_into(&mut self, effects: &HeapEffects, body: &Body, n: u32, op: Operand) {
        if let OperandNode::Node(m) = self.operand_node(effects, body, op) {
            let slot = self.classes.load(n);
            self.unify(slot, m);
        }
    }

    /// Record the field reads inside a promoted value.
    fn value_reads(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        site: NodeRef,
        v: ValueId,
        seen: &mut IndexSet<ValueId>,
    ) {
        if !seen.insert(v) {
            return;
        }
        match body.values.kind(v) {
            ValueKind::FieldAccess {
                receiver,
                field_index,
                ..
            } => {
                let (receiver, field) = (*receiver, *field_index);
                let keys = Keys::of(effects, body.values.type_of(receiver));
                self.record(
                    effects,
                    body,
                    Effect::Read,
                    keys,
                    site,
                    Operand::Value(receiver),
                    Some(field),
                );
                self.value_reads(effects, body, site, receiver, seen);
            }
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                self.value_reads(effects, body, site, *operand, seen);
            }
            ValueKind::Binary { lhs, rhs, .. } => {
                let (lhs, rhs) = (*lhs, *rhs);
                self.value_reads(effects, body, site, lhs, seen);
                self.value_reads(effects, body, site, rhs, seen);
            }
            ValueKind::Select { cond, then, else_ } => {
                let (cond, then, else_) = (*cond, *then, *else_);
                self.value_reads(effects, body, site, cond, seen);
                self.value_reads(effects, body, site, then, seen);
                self.value_reads(effects, body, site, else_, seen);
            }
            ValueKind::LoopPhi { entry, body_iter } => {
                let (entry, body_iter) = (*entry, *body_iter);
                self.value_reads(effects, body, site, entry, seen);
                self.value_reads(effects, body, site, body_iter, seen);
            }
            ValueKind::Opaque(_)
            | ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::UnitCase { .. }
            | ValueKind::Const(..) => {}
        }
    }

    /// Record an access at `site` to the object `receiver` names.
    fn access(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        effect: Effect,
        site: ExprId,
        receiver: Operand,
        field: Option<u32>,
    ) {
        let keys = Keys::of(effects, Some(body.operand_type(receiver)));
        self.record(
            effects,
            body,
            effect,
            keys,
            NodeRef::Expr(site),
            receiver,
            field,
        );
    }

    fn record(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        effect: Effect,
        keys: Keys,
        site: NodeRef,
        receiver: Operand,
        field: Option<u32>,
    ) {
        if let OperandNode::Node(node) = self.operand_node(effects, body, receiver) {
            self.accesses.push(AccessSite {
                effect,
                node,
                keys,
                site,
                receiver: Some(receiver),
                field,
            });
        }
    }

    fn expr(&mut self, effects: &HeapEffects, body: &Body, e: ExprId) {
        let node = self.expr_node(e).expect("built over this body");
        let yields = holds_reference(effects.type_table, body.exprs[e].type_id);
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => {
                if yields {
                    let l = self.local_node(*index);
                    self.unify(node, l);
                }
            }
            ExprKind::GlobalVarGet { .. } => {
                if yields {
                    self.unify(node, ELSEWHERE);
                }
            }
            ExprKind::GlobalVarSet { value, .. } => self.unify_op(effects, body, ELSEWHERE, *value),
            ExprKind::Unary { op, expr: inner } => {
                if *op == NirUnaryOp::Deref && !self.targets.contains(&e) {
                    self.access(effects, body, Effect::Read, e, *inner, None);
                }
                if yields {
                    self.unify_op(effects, body, node, *inner);
                }
            }
            ExprKind::Binary { left, right, .. } => {
                if yields {
                    self.unify_op(effects, body, node, *left);
                    self.unify_op(effects, body, node, *right);
                }
            }
            ExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                if yields {
                    self.unify_op(effects, body, node, *inner);
                    // A cast that names a different object than its operand,
                    // as a payload is to its variant, keeps no depth apart.
                    let tt = effects.type_table;
                    let object = |ty| tt.heap_object_key(referent(tt, ty));
                    if object(body.operand_type(*inner)) != object(*target_type) {
                        self.classes.collapse(node);
                    }
                }
            }
            ExprKind::ClosureToCanonical { functor: inner, .. } => {
                if yields {
                    self.unify_op(effects, body, node, *inner);
                }
            }
            ExprKind::Assign { target, value } => self.assign(effects, body, *target, *value),
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                if !self.targets.contains(&e) {
                    self.access(effects, body, Effect::Read, e, *inner, Some(*field_index));
                }
                if yields {
                    self.unify_load(effects, body, node, *inner);
                }
            }
            ExprKind::Index { expr: inner, .. } | ExprKind::VariantPayload { expr: inner, .. } => {
                if !self.targets.contains(&e) {
                    self.access(effects, body, Effect::Read, e, *inner, None);
                }
                if yields {
                    self.unify_load(effects, body, node, *inner);
                }
            }
            ExprKind::VariantTag { expr: inner } | ExprKind::VariantTest { expr: inner, .. } => {
                self.access(effects, body, Effect::Read, e, *inner, None);
            }
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.store_into(effects, body, node, f.value);
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for &el in elements {
                    self.store_into(effects, body, node, el);
                }
            }
            ExprKind::VariantConstruct { payload, .. } => {
                self.store_into(effects, body, node, *payload);
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                if yields {
                    for b in std::iter::once(*then_branch).chain(*else_branch) {
                        if let Some(tail) = body.block_tail(b) {
                            self.unify_op(effects, body, node, tail);
                        }
                    }
                }
            }
            ExprKind::Switch { arms, default, .. } => {
                if yields {
                    for &b in arms.iter().chain(std::iter::once(default)) {
                        if let Some(tail) = body.block_tail(b) {
                            self.unify_op(effects, body, node, tail);
                        }
                    }
                }
            }
            ExprKind::Match { expr, arms } => {
                let scrutinee = self.operand_node(effects, body, *expr);
                let ty = body.operand_type(*expr);
                for arm in arms {
                    self.pattern(effects, body, arm.pattern, scrutinee, Some(ty));
                    if yields {
                        self.unify_op(effects, body, node, arm.body);
                    }
                }
            }
            ExprKind::LabeledBlock { .. } => {
                if yields {
                    for exit in body.block_exits(e).into_iter().flatten().flatten() {
                        self.unify_op(effects, body, node, exit);
                    }
                }
            }
            ExprKind::Call { .. } | ExprKind::IndirectCall { .. } => {
                let result = self.operand_node(effects, body, Operand::Expr(e));
                let start = self.call_args.len();
                for a in call_operands(body, e) {
                    let n = self.operand_node(effects, body, a);
                    self.call_args.push((a, n));
                }
                self.calls.push(e);
                self.call_nodes.push((result, start..self.call_args.len()));
            }
            ExprKind::CmRawCall { .. }
            | ExprKind::EnumConstruct { .. }
            | ExprKind::PackedArray(_)
            | ExprKind::Dead => {}
        }
    }

    fn assign(&mut self, effects: &HeapEffects, body: &Body, target: ExprId, value: Operand) {
        self.targets.insert(target);
        let target_node = self.operand_node(effects, body, Operand::Expr(target));
        let value_node = self.operand_node(effects, body, value);
        self.unify_nodes(target_node, value_node);
        let place = &body.exprs[target].kind;
        if let ExprKind::Local { index, .. } = place {
            self.defined.insert(*index);
            return;
        }
        let field = if let ExprKind::FieldAccess { field_index, .. } = place {
            Some(*field_index)
        } else {
            None
        };
        if let ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } = place
        {
            let slot = self.operand_load(effects, body, *inner);
            self.unify_nodes(slot, value_node);
            self.access(effects, body, Effect::Write, target, *inner, field);
        } else if let ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } = place
        {
            let receiver = self.operand_node(effects, body, *inner);
            self.unify_nodes(receiver, value_node);
            self.access(effects, body, Effect::Write, target, *inner, None);
        } else if let OperandNode::Node(node) = target_node {
            self.accesses.push(AccessSite {
                effect: Effect::Write,
                node,
                keys: Keys::of(effects, None),
                site: NodeRef::Expr(target),
                receiver: None,
                field: None,
            });
        }
    }

    fn pattern(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        pat: PatId,
        scrutinee: OperandNode,
        ty: Option<TypeId>,
    ) {
        let read = |frame: &mut Self, ty: Option<TypeId>| {
            if let OperandNode::Node(node) = scrutinee {
                frame.accesses.push(AccessSite {
                    effect: Effect::Read,
                    node,
                    keys: Keys::of(effects, ty),
                    site: NodeRef::Pat(pat),
                    receiver: None,
                    field: None,
                });
            }
        };
        match &body.pats[pat].kind {
            PatKind::Binding {
                local_index,
                type_id,
                ..
            } => {
                let l = self.define_local(*local_index);
                if let OperandNode::Node(n) = scrutinee {
                    // A binding that holds the part in a cell, or reads it out
                    // of one, is a depth apart from it.
                    let tt = effects.type_table;
                    let is_cell = |t| tt.cell_payload(t).is_some();
                    if ty.is_none_or(|t| is_cell(t) != is_cell(*type_id)) {
                        self.classes.collapse(n);
                    }
                    self.unify(l, n);
                }
            }
            PatKind::Tuple(elements, _) => {
                read(self, ty);
                let element = self.classes.load_operand(scrutinee);
                for (i, &p) in elements.iter().enumerate() {
                    let sub = effects.tuple_element_type(ty, i);
                    self.pattern(effects, body, p, element, sub);
                }
            }
            PatKind::Struct {
                struct_type,
                fields,
                ..
            } => {
                read(self, Some(*struct_type));
                let field = self.classes.load_operand(scrutinee);
                for f in fields {
                    let sub =
                        effects.pattern_field_type(Some(*struct_type), f.field_index as usize);
                    self.pattern(effects, body, f.pattern, field, sub);
                }
            }
            PatKind::Variant {
                bindings,
                payload_type,
                ..
            } => {
                read(self, ty);
                let payload = self.classes.load_operand(scrutinee);
                if let [one] = bindings.as_slice() {
                    self.pattern(effects, body, *one, payload, Some(*payload_type));
                } else {
                    let element = self.classes.load_operand(payload);
                    for (i, &p) in bindings.iter().enumerate() {
                        let sub = effects.tuple_element_type(Some(*payload_type), i);
                        self.pattern(effects, body, p, element, sub);
                    }
                }
            }
            PatKind::Or(alternatives) => {
                for &p in alternatives {
                    self.pattern(effects, body, p, scrutinee, ty);
                }
            }
            PatKind::Wildcard
            | PatKind::Literal(_)
            | PatKind::Enum { .. }
            | PatKind::ConstantValue { .. }
            | PatKind::Range { .. } => {}
        }
    }

    /// Join what a call makes share: its result with what the callee returns,
    /// and each argument with what the callee stores into it.
    fn call_flows(
        classes: &mut Classes,
        effects: &HeapEffects,
        body: &Body,
        e: ExprId,
        result: OperandNode,
        args: &[(Operand, OperandNode)],
    ) {
        let node = |j: usize| args.get(j).and_then(|&(_, n)| n.node());
        let result = result.node();
        // The arguments a [`Prov`] parameter bit stands for.
        let nodes_of = |bit: usize| {
            let end = if bit == PARAM_SLOTS - 1 {
                args.len()
            } else {
                bit + 1
            };
            (bit..end).filter_map(node)
        };
        let tt = effects.type_table;
        match call_target(effects, body, e) {
            Target::Summary(s) => {
                let held = |classes: &mut Classes, at: Option<u32>, levels: &Levels| {
                    let Some(at) = at else { return };
                    for (depth, prov) in levels.provs.iter().enumerate() {
                        if prov.is_fresh() {
                            continue;
                        }
                        let slot = classes.load_n(at, depth);
                        for (j, below) in prov.sources() {
                            for n in nodes_of(j) {
                                let source = classes.load_n(n, below);
                                if below == DEEPEST {
                                    classes.collapse(source);
                                }
                                classes.unify(slot, source);
                            }
                        }
                        if prov.elsewhere {
                            classes.unify(slot, ELSEWHERE);
                        }
                    }
                    if levels.open {
                        let deepest = classes.load_n(at, DEEPEST);
                        classes.collapse(deepest);
                    }
                };
                held(classes, result, &s.ret);
                for (k, levels) in s.into_params.iter().enumerate().take(args.len()) {
                    held(classes, node(k), levels);
                }
                for (j, below) in s.into_elsewhere.sources() {
                    for n in nodes_of(j) {
                        let source = classes.load_n(n, below);
                        classes.unify(source, ELSEWHERE);
                    }
                }
            }
            Target::Builtin(declaration) => {
                for r in &declaration.retains {
                    let (Some(&(source, _)), Some(n)) = (args.get(r.source), node(r.source)) else {
                        continue;
                    };
                    if !effects.retains_reference(body, declaration, r, Some(source)) {
                        continue;
                    }
                    let held = if r.elements {
                        contents(classes, tt, body.operand_type(source), n)
                    } else {
                        n
                    };
                    match r.into {
                        RetainInto::Param(q) => {
                            if let Some(d) = node(q) {
                                let slot = contents(classes, tt, body.operand_type(args[q].0), d);
                                classes.unify(slot, held);
                            }
                        }
                        RetainInto::Result => {
                            let Some(result) = result else { continue };
                            // A copy of the source shares what it shares at
                            // the depths the source holds it.
                            if matches!(declaration.facts.storage, Storage::CopiesArgs)
                                && !r.elements
                            {
                                classes.unify(result, held);
                            } else {
                                let slot = classes.load(result);
                                classes.unify(slot, held);
                            }
                        }
                        RetainInto::Anywhere => {
                            if let Some(result) = result {
                                classes.unify(result, held);
                            }
                            classes.unify(held, ELSEWHERE);
                        }
                    }
                }
                let Some(result) = result else { return };
                for j in 0..args.len() {
                    let Some(n) = node(j) else { continue };
                    if !returns_part_of(declaration, j) {
                        continue;
                    }
                    let declared = declaration.returns.is_some();
                    if declared && !declaration.reference_params.contains(&j) {
                        classes.unify(result, n);
                        continue;
                    }
                    // Through a reference, the result is what one load from
                    // the argument reads where the types say so: not through a
                    // cell, which may be the part or a snapshot holding it, and
                    // not `replace`, which hands back the referent itself.
                    let (holder, part) = (body.operand_type(args[j].0), body.exprs[e].type_id);
                    let is_cell = |t| tt.cell_payload(t).is_some();
                    if declared
                        && !is_cell(holder)
                        && !is_cell(part)
                        && effects.loads_as(holder, part)
                    {
                        let loaded = classes.load(n);
                        classes.unify(result, loaded);
                    } else {
                        classes.collapse(n);
                        classes.unify(result, n);
                    }
                }
            }
            Target::Opaque { .. } => {
                let mut joined = result;
                for n in (0..args.len()).filter_map(node) {
                    classes.collapse(n);
                    match joined {
                        Some(j) => classes.unify(j, n),
                        None => joined = Some(n),
                    }
                }
            }
        }
    }

    /// The summary this frame's body gives its callers, once its calls join
    /// `classes`.
    fn summary(&self, classes: &Classes, effects: &HeapEffects, body: &Body) -> Summary {
        let mut s = Summary {
            ret: Levels::of(classes, RET),
            into_params: self
                .params
                .iter()
                .map(|&n| Levels::of(classes, n))
                .collect(),
            into_elsewhere: classes.prov_of(ELSEWHERE),
            ..Summary::default()
        };
        for AccessSite {
            effect,
            node,
            keys,
            field,
            ..
        } in &self.accesses
        {
            let prov = classes.prov_of(*node);
            if *effect == Effect::Write && !prov.is_fresh() {
                match keys {
                    Keys::One(k) => s.field_writes.record(*k, *field),
                    Keys::Set(set) => s.field_writes.record_types(set),
                }
            }
            let access = match effect {
                Effect::Read => &mut s.reads,
                Effect::Write => &mut s.writes,
            };
            match keys {
                Keys::One(k) => access.record(prov, &ObjectTypes::one(*k)),
                Keys::Set(set) => access.record(prov, set),
            }
        }
        for (&call, (_, span)) in self.calls.iter().zip(&self.call_nodes) {
            let args = &self.call_args[span.clone()];
            call_summary(classes, effects, body, call, args, &mut s);
        }
        s
    }

    /// The class root an operand's objects belong to, without joining anything.
    fn lookup(&self, effects: &HeapEffects, body: &Body, op: Operand) -> OperandNode {
        let tt = effects.type_table;
        match op {
            Operand::Expr(e) => {
                if !holds_reference(tt, body.exprs[e].type_id) {
                    return OperandNode::None;
                }
                self.expr_node(e).map_or(OperandNode::Unknown, |n| {
                    OperandNode::Node(self.classes.root(n))
                })
            }
            Operand::Value(v) => self.value_lookup(effects, body, v),
        }
    }

    /// [`Self::lookup`] for a promoted value.
    fn value_lookup(&self, effects: &HeapEffects, body: &Body, v: ValueId) -> OperandNode {
        if !value_holds_reference(effects, body, v) {
            return OperandNode::None;
        }
        let join = |inputs: &[ValueId]| {
            let mut joined = OperandNode::None;
            for &input in inputs {
                match (joined, self.value_lookup(effects, body, input)) {
                    (_, OperandNode::None) => {}
                    (OperandNode::None, n) => joined = n,
                    (a, b) if a == b => {}
                    _ => return OperandNode::Unknown,
                }
            }
            joined
        };
        match *body.values.kind(v) {
            ValueKind::Opaque(oid) => self
                .local_root(body.values.promoted_opaque_local(oid))
                .map_or(OperandNode::Unknown, OperandNode::Node),
            ValueKind::FieldAccess { receiver, .. } => {
                match self.value_lookup(effects, body, receiver) {
                    OperandNode::Node(r) => self
                        .classes
                        .loaded_from(r)
                        .map_or(OperandNode::Unknown, OperandNode::Node),
                    other => other,
                }
            }
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                self.value_lookup(effects, body, operand)
            }
            ValueKind::Binary { lhs, rhs, .. } => join(&[lhs, rhs]),
            ValueKind::Select { then, else_, .. } => join(&[then, else_]),
            ValueKind::LoopPhi { entry, body_iter } => join(&[entry, body_iter]),
            ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::UnitCase { .. }
            | ValueKind::Const(..) => OperandNode::None,
        }
    }

    fn local_root(&self, local: u32) -> Option<u32> {
        let n = *self.local_nodes.get(local as usize)?;
        (n != NO_NODE).then(|| self.classes.root(n))
    }

    /// What the objects class `h` holds reach, `None` standing for anything.
    fn reach_of(&self, h: Option<u32>) -> Option<Reach> {
        let chain = self.classes.chain(h?);
        let foreign = chain.iter().any(|&c| !self.classes.prov_of(c).is_fresh());
        let elsewhere = self.classes.root(ELSEWHERE);
        Some(Reach {
            anything: self.partial && chain.contains(&elsewhere),
            chain,
            foreign,
            elsewhere: self.partial.then_some(elsewhere),
        })
    }

    /// Whether locals `a` and `b` may hold one object.
    pub(super) fn locals_may_share(&self, a: u32, b: u32) -> bool {
        let Some(a) = self.local_root(a) else {
            return true;
        };
        meets(
            &self.classes,
            self.reach_of(self.local_root(b)).as_ref(),
            Level::Class(a),
        )
    }

    /// Whether `call` may read or write an object of type `key` that the
    /// object `local` holds is, or reaches.
    pub(super) fn call_may(
        &self,
        effects: &HeapEffects,
        body: &Body,
        call: ExprId,
        effect: Effect,
        key: TypeId,
        local: u32,
    ) -> bool {
        self.call_may_besides(effects, body, call, effect, key, local, |_| false)
    }

    /// [`Self::call_may`], leaving out the arguments `answered` accepts: the
    /// ones a caller accounts for on its own.
    pub(super) fn call_may_besides(
        &self,
        effects: &HeapEffects,
        body: &Body,
        call: ExprId,
        effect: Effect,
        key: TypeId,
        local: u32,
        answered: impl Fn(Operand) -> bool,
    ) -> bool {
        self.call_may_keys(
            effects,
            body,
            call,
            effect,
            &Keys::One(key),
            self.reach_of(self.local_root(local)).as_ref(),
            &answered,
        )
    }

    fn call_may_keys(
        &self,
        effects: &HeapEffects,
        body: &Body,
        call: ExprId,
        effect: Effect,
        keys: &Keys,
        reach: Option<&Reach>,
        answered: &impl Fn(Operand) -> bool,
    ) -> bool {
        if effect == Effect::Write && !effects.returns(body, call) {
            return false;
        }
        let h_escapes = reach.is_none_or(|r| r.foreign || r.anything);
        let (target, args) = call_parts(effects, body, call);
        if h_escapes && keys.meets(&target.elsewhere(effect)) {
            return true;
        }
        args.iter().enumerate().any(|(j, &a)| {
            if answered(a) {
                return false;
            }
            let n = match self.lookup(effects, body, a) {
                OperandNode::None => return false,
                OperandNode::Unknown => None,
                OperandNode::Node(n) => Some(n),
            };
            let met = |depth: usize, below: bool| {
                n.is_none_or(|n| {
                    self.classes
                        .levels(n, depth, below)
                        .into_iter()
                        .any(|level| meets(&self.classes, reach, level))
                })
            };
            let ty = body.operand_type(a);
            match target {
                Target::Summary(t) => {
                    let reached = effects.reach(ty);
                    t.access(effect).depths(j).any(|(depth, types)| {
                        keys.meets_both(types, &reached) && met(depth, depth == DEEPEST)
                    })
                }
                Target::Builtin(declaration) => {
                    let (touched, deep) = builtin_touches(effects, declaration, ty);
                    (effect == Effect::Read || declaration.mut_params.contains(&j))
                        && keys.meets(&touched)
                        && met(0, deep)
                }
                Target::Opaque { .. } => keys.meets(&effects.reach(ty)) && met(0, true),
            }
        })
    }

    /// Whether a store, or a call through an argument `answered` rejects, may
    /// write a `keys` object that `local` holds or reaches.
    pub(super) fn written(
        &self,
        effects: &HeapEffects,
        body: &Body,
        keys: &Arc<ObjectTypes>,
        local: u32,
        answered: impl Fn(Operand) -> bool,
    ) -> bool {
        self.written_in(effects, body, keys, self.local_root(local), &answered)
    }

    /// [`Self::written`] for the objects operand `op` holds or reaches.
    pub(super) fn operand_written(
        &self,
        effects: &HeapEffects,
        body: &Body,
        keys: &Arc<ObjectTypes>,
        op: Operand,
    ) -> bool {
        let h = match self.lookup(effects, body, op) {
            OperandNode::None => return false,
            OperandNode::Unknown => None,
            OperandNode::Node(n) => Some(n),
        };
        self.written_in(effects, body, keys, h, &|_| false)
    }

    /// Whether every object operand `op` holds is allocated in this body and
    /// stays in it: none comes from a parameter or a global, and none reaches
    /// the result or anything the body did not allocate.
    pub(super) fn operand_confined(&self, effects: &HeapEffects, body: &Body, op: Operand) -> bool {
        let OperandNode::Node(h) = self.lookup(effects, body, op) else {
            return false;
        };
        let returned = self.classes.chain(RET);
        !self.partial
            && self
                .classes
                .chain(h)
                .iter()
                .all(|&c| self.classes.prov_of(c).is_fresh() && !returned.contains(&c))
    }

    fn written_in(
        &self,
        effects: &HeapEffects,
        body: &Body,
        keys: &Arc<ObjectTypes>,
        h: Option<u32>,
        answered: &impl Fn(Operand) -> bool,
    ) -> bool {
        let wanted = Keys::Set(Arc::clone(keys));
        let reach = self.reach_of(h);
        self.accesses.iter().any(|a| {
            a.effect == Effect::Write
                && a.keys.meets(keys)
                && meets(&self.classes, reach.as_ref(), Level::Class(a.node))
        }) || self.calls.iter().any(|&call| {
            self.call_may_keys(
                effects,
                body,
                call,
                Effect::Write,
                &wanted,
                reach.as_ref(),
                answered,
            )
        })
    }

    /// Whether objects `local` holds, once it also holds what `other` holds,
    /// stay reachable after the body returns: through its result, a global,
    /// or a parameter they did not come from.
    pub(super) fn outlives_sharing(&self, local: u32, other: u32) -> bool {
        let (Some(a), Some(b)) = (self.local_root(local), self.local_root(other)) else {
            return true;
        };
        let (a, b) = (self.classes.chain(a), self.classes.chain(b));
        let mut prov = Prov::default();
        for &c in a.iter().chain(&b) {
            prov.join(self.classes.prov_of(c));
        }
        let mut outside = self.classes.chain(RET);
        outside.push(self.classes.root(ELSEWHERE));
        let escaped = |chain: &[u32]| chain.iter().any(|c| outside.contains(c));
        !prov.is_fresh()
            && (escaped(&a) || escaped(&b) || prov.params_at_any_depth().count_ones() > 1)
    }

    /// Whether an access at a site `at` accepts, other than one through a
    /// receiver `own` accepts, may reach `field` of the `key` object `local` holds.
    pub(super) fn accessed_besides(
        &self,
        writes_only: bool,
        at: impl Fn(NodeRef) -> bool,
        (key, field): (TypeId, u32),
        local: u32,
        own: impl Fn(Operand) -> bool,
    ) -> bool {
        let reach = self.reach_of(self.local_root(local));
        self.accesses.iter().any(|a| {
            (!writes_only || a.effect == Effect::Write)
                && at(a.site)
                && a.keys.contains(key)
                && a.field.is_none_or(|f| f == field)
                && !(a.field.is_some() && a.receiver.is_some_and(&own))
                && meets(&self.classes, reach.as_ref(), Level::Class(a.node))
        })
    }

    /// Whether a store or a call at a site `at` accepts may put a new object on
    /// the place `local` then `path` names (see [`field_path`]).
    pub(super) fn place_replaced(
        &self,
        effects: &HeapEffects,
        body: &Body,
        local: u32,
        path: &[(TypeId, u32)],
        at: impl Fn(NodeRef) -> bool,
    ) -> bool {
        path.iter().any(|&(holder, field)| {
            let Some(key) = effects.type_table.heap_object_key(holder) else {
                return true;
            };
            self.accessed_besides(true, &at, (key, field), local, |_| false)
                || self.calls.iter().any(|&call| {
                    at(NodeRef::Expr(call))
                        && self.call_may(effects, body, call, Effect::Write, key, local)
                })
        })
    }
}

/// The classes one class's objects are in or reach.
struct Reach {
    chain: Vec<u32>,
    /// Some class on the chain holds objects from outside the body.
    foreign: bool,
    /// The chain reaches the `ELSEWHERE` class of a partial frame, which may
    /// hold objects of any class.
    anything: bool,
    /// The `ELSEWHERE` class of a partial frame.
    elsewhere: Option<u32>,
}

/// Whether objects at `level` may be ones `reach` (`None`: anything) covers:
/// on its chain, from outside the body where the chain is too, or in the
/// `ELSEWHERE` class of a partial frame.
fn meets(classes: &Classes, reach: Option<&Reach>, level: Level) -> bool {
    let Some(reach) = reach else {
        return true;
    };
    reach.anything
        || match level {
            Level::Class(c) => {
                let c = classes.root(c);
                reach.chain.contains(&c)
                    || (reach.foreign && !classes.prov_of(c).is_fresh())
                    || reach.elsewhere == Some(c)
            }
            // Below a class on the chain is reached through it.
            Level::Beyond { under, prov } => {
                reach.chain.contains(&classes.root(under)) || (reach.foreign && !prov.is_fresh())
            }
        }
}

/// The class the elements of `n`, an array of type `ty`, are in. What a value
/// of any other type holds is at no depth a declaration states, so every depth
/// of it is made one class.
fn contents(classes: &mut Classes, tt: &TypeTable, ty: TypeId, n: u32) -> u32 {
    if matches!(tt.get(referent(tt, ty)), ResolvedType::BuiltinArray(_)) {
        classes.load(n)
    } else {
        classes.collapse(n);
        n
    }
}

/// `ty` through its references and newtypes.
fn referent(tt: &TypeTable, mut ty: TypeId) -> TypeId {
    loop {
        match tt.get(ty) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => ty = *inner,
            ResolvedType::Newtype { .. } => ty = tt.representation_head(ty),
            _ => return ty,
        }
    }
}

fn value_holds_reference(effects: &HeapEffects, body: &Body, v: ValueId) -> bool {
    body.values
        .type_of(v)
        .is_none_or(|ty| holds_reference(effects.type_table, ty))
}

/// The expression a field chain starts from, and each object on the chain with
/// the field taken from it.
pub(super) fn field_path(body: &Body, mut e: ExprId) -> Option<(ExprId, Vec<(TypeId, u32)>)> {
    let mut path = Vec::new();
    while let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[e].kind
    {
        let inner = inner.as_expr()?;
        path.push((body.exprs[inner].type_id, *field_index));
        e = inner;
    }
    Some((e, path))
}

/// What a builtin touches through an argument of type `ty`, and whether at
/// every depth below it. An `Array<T>` is reached as the array, its length and
/// element slots, unless the declaration copies what the elements reach or
/// states nothing of its storage; anything else is reached whole.
fn builtin_touches(
    effects: &HeapEffects,
    declaration: &BuiltinDeclaration,
    ty: TypeId,
) -> (Arc<ObjectTypes>, bool) {
    let reaches_elements = matches!(
        declaration.facts.storage,
        Storage::CopiesArgs | Storage::Opaque
    );
    let tt = effects.type_table;
    let is_array = matches!(tt.get(tt.strip_handles(ty)), ResolvedType::BuiltinArray(_));
    if reaches_elements || !is_array {
        return (effects.reach(ty), true);
    }
    let array = tt
        .heap_object_key(ty)
        .map_or_else(ObjectTypes::everything, ObjectTypes::one);
    (Arc::new(array), false)
}

/// Visit the local each opaque leaf of the promoted value `v` reads.
pub(super) fn for_each_leaf_local(body: &Body, v: ValueId, f: &mut impl FnMut(u32)) {
    let mut stack = vec![v];
    let mut seen = IndexSet::default();
    while let Some(v) = stack.pop() {
        if !seen.insert(v) {
            continue;
        }
        match body.values.kind(v) {
            ValueKind::Opaque(oid) => f(body.values.promoted_opaque_local(*oid)),
            ValueKind::FieldAccess { receiver, .. } => stack.push(*receiver),
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                stack.push(*operand);
            }
            ValueKind::Binary { lhs, rhs, .. } => stack.extend([*lhs, *rhs]),
            ValueKind::Select { then, else_, .. } => stack.extend([*then, *else_]),
            ValueKind::LoopPhi { entry, body_iter } => stack.extend([*entry, *body_iter]),
            ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::UnitCase { .. }
            | ValueKind::Const(..) => {}
        }
    }
}

/// Whether a builtin's result may be part of its argument `j`.
fn returns_part_of(declaration: &BuiltinDeclaration, j: usize) -> bool {
    match &declaration.returns {
        Some(ReturnConvention::Owned) => false,
        Some(ReturnConvention::PartOf(params)) => params.contains(&j),
        None => true,
    }
}

/// What a call runs, as far as its heap effects go.
enum Target<'s> {
    Summary(&'s Summary),
    Builtin(&'s BuiltinDeclaration),
    Opaque { suspends: bool },
}

impl Target<'_> {
    /// The object types held elsewhere that the call may `effect`: every type
    /// where other tasks run before it returns, and what a body's calls reach.
    fn elsewhere(&self, effect: Effect) -> Cow<'_, ObjectTypes> {
        let suspends = match self {
            Target::Summary(t) => return Cow::Borrowed(&t.access(effect).elsewhere),
            Target::Builtin(declaration) => declaration.facts.suspend,
            Target::Opaque { suspends } => *suspends,
        };
        Cow::Owned(if suspends {
            ObjectTypes::everything()
        } else {
            ObjectTypes::default()
        })
    }
}

/// Add what `call`, whose arguments are `args`, does to objects the caller did
/// not allocate.
fn call_summary(
    classes: &Classes,
    effects: &HeapEffects,
    body: &Body,
    call: ExprId,
    args: &[(Operand, OperandNode)],
    s: &mut Summary,
) {
    let target = call_target(effects, body, call);
    let mut unobserved = Access::default();
    let mut unobserved_fields = FieldWrites::default();
    let Summary {
        reads,
        writes,
        field_writes,
        ..
    } = s;
    let (writes, field_writes) = if effects.returns(body, call) {
        (writes, field_writes)
    } else {
        (&mut unobserved, &mut unobserved_fields)
    };
    reads.elsewhere.union(&target.elsewhere(Effect::Read));
    let written_elsewhere = target.elsewhere(Effect::Write);
    writes.elsewhere.union(&written_elsewhere);
    match target {
        Target::Summary(t) => field_writes.union(&t.field_writes),
        Target::Builtin(_) | Target::Opaque { .. } => field_writes.record_types(&written_elsewhere),
    }
    for (j, &(a, node)) in args.iter().enumerate() {
        let OperandNode::Node(n) = node else {
            continue;
        };
        let foreign = |depth: usize, below: bool| {
            classes
                .levels(n, depth, below)
                .into_iter()
                .map(|level| classes.level_prov(level))
                .filter(|prov| !prov.is_fresh())
        };
        let ty = body.operand_type(a);
        match target {
            Target::Summary(t) => {
                let reach = effects.reach(ty);
                for (theirs, mine) in [(&t.reads, &mut *reads), (&t.writes, &mut *writes)] {
                    for (depth, types) in theirs.depths(j) {
                        for prov in foreign(depth, depth == DEEPEST) {
                            mine.record_meet(prov, types, &reach);
                        }
                    }
                }
            }
            Target::Builtin(declaration) => {
                let (touched, deep) = builtin_touches(effects, declaration, ty);
                for prov in foreign(0, deep) {
                    reads.record(prov, &touched);
                    if declaration.mut_params.contains(&j) {
                        writes.record(prov, &touched);
                        field_writes.record_types(&touched);
                    }
                }
            }
            Target::Opaque { .. } => {
                let reach = effects.reach(ty);
                for prov in foreign(0, true) {
                    reads.record(prov, &reach);
                    writes.record(prov, &reach);
                    field_writes.record_types(&reach);
                }
            }
        }
    }
}

/// The call's target and its arguments in the callee's parameter order; an
/// indirect call's closure environment is parameter 0.
fn call_parts<'s>(effects: &'s HeapEffects, body: &Body, e: ExprId) -> (Target<'s>, Vec<Operand>) {
    (call_target(effects, body, e), call_operands(body, e))
}

fn call_target<'s>(effects: &'s HeapEffects, body: &Body, e: ExprId) -> Target<'s> {
    let cache = effects.cache;
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, .. } => match cache
            .functions
            .facts()
            .get(func_id.index())
            .map(|f| &f.callee)
        {
            Some(Callee::Body) => Target::Summary(&cache.summaries[func_id.index()]),
            Some(Callee::Builtin(declaration)) => Target::Builtin(declaration),
            Some(&Callee::Opaque { suspends }) => Target::Opaque { suspends },
            None => Target::Opaque { suspends: true },
        },
        ExprKind::IndirectCall { .. } => Target::Summary(&cache.indirect),
        other => panic!("call_target on a non-call {other:?}"),
    }
}

fn call_operands(body: &Body, e: ExprId) -> Vec<Operand> {
    match &body.exprs[e].kind {
        ExprKind::Call { args, .. } => args.iter().map(|a| a.expr).collect(),
        ExprKind::IndirectCall { callee, args, .. } => std::iter::once(*callee)
            .chain(args.iter().copied())
            .collect(),
        other => panic!("call_operands on a non-call {other:?}"),
    }
}
