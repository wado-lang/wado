//! Whole-program points-to analysis: which storage each reference may name.
//!
//! Inclusion-based (Andersen), flow- and context-insensitive, one location per
//! local of each frame. A node's set is the storage the value held there can
//! reach through the references it carries; a local's node is also the
//! location of the local's own storage. A call flows each argument into its
//! parameter and the callee's return node into the call.
//!
//! A function value is a location: a closure's holds what it captured. A call
//! through a value is bound, as the solve finds them, to the functions it can
//! name: a closure's `$call`, which takes the closure as `self`, or a `FuncRef`
//! target. Effect handlers are already globals, closures and indirect calls
//! here, so their dispatch resolves the same way.
//!
//! What only code the analysis cannot see reaches merges into one escape node:
//! the arguments of a call to a function with no declaration here, of a call
//! through a function value with no body, and of an `Inspect` dispatch stub,
//! whose WIR body calls through its argument.
//!
//! The solve collapses each cycle of copies into one node, as every node on it
//! ends with one set, and merges a node fed by one copy alone into its source.

use std::cell::RefCell;
use std::ops::Range;

mod loc_set;

pub use loc_set::LocSet;

use super::analyze::collect_pattern_bindings;
use super::funcset::FuncIndex;
use super::is_reference_type;
use super::last_use::collect_child_exprs;
use super::place::could_write_through;
use super::{EnvFrame, NEEDS_COPY_DEPTH_LIMIT, resolve_param_in_env};
use crate::defs::DefId;
use crate::flat_package::FlatPackage;
use crate::graph::DisjointSets;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{
    BuiltinDeclaration, BuiltinDeclarations, CaptureSource, ClosureFunctor, FunctionKind,
    ResolvedType, RetainInto, ReturnConvention, StructFields, TirBlock, TirExpr, TirExprKind,
    TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
};
use crate::token::Span;
use crate::type_table::{TypeKey, TypeTable};

/// A frame: a function, or a global's initializer, whose locals have locations
/// of their own.
type Frame = u32;

/// The solved analysis. Sets name nodes that are locations: a frame's local,
/// a global, a function value, or the temporary a reference to a call's result
/// takes.
pub struct PointsTo {
    functions: FuncIndex,
    locals: IndexMap<(Frame, u32), u32>,
    /// The frame and local each location node is, the inverse of `locals`.
    owners: IndexMap<u32, (Frame, u32)>,
    pts: Vec<LocSet>,
    /// The node whose entry in `pts` each node's set is, flattened.
    rep: DisjointSets,
    /// The node the value each call argument hands its callee flows from, and
    /// the argument's span, keyed by the argument expression's address. Valid
    /// while no body is rewritten, as the fold, which computes and reads it,
    /// rewrites none.
    handed: IndexMap<usize, (u32, Span)>,
    /// The node naming the storage each assigned or referenced place denotes,
    /// keyed as `handed` is.
    written: IndexMap<usize, (u32, Span)>,
    /// What any call may write: [`Self::global_reach`].
    global_reach: IndexSet<u32>,
}

/// Record `node` for `expr` in `sites`, keyed by the expression's address.
fn note_site(sites: &mut IndexMap<usize, (u32, Span)>, expr: &TirExpr, node: u32) {
    sites.insert(std::ptr::from_ref(expr).addr(), (node, expr.span));
}

/// The node `expr` was recorded under in `sites`.
fn site_of(sites: &IndexMap<usize, (u32, Span)>, expr: &TirExpr) -> u32 {
    let &(node, span) = sites
        .get(&std::ptr::from_ref(expr).addr())
        .expect("the analysis records every site of every body it reads");
    assert_eq!(
        span, expr.span,
        "a site keyed by address moved after the analysis"
    );
    node
}

impl PointsTo {
    fn set(&self, node: u32) -> &LocSet {
        &self.pts[self.rep.root(node) as usize]
    }

    /// The frame of the function `module`/`name`, `None` for one the analysis
    /// did not read.
    #[must_use]
    pub fn frame(&self, module: &ModuleSource, name: &str) -> Option<Frame> {
        self.functions.id(module, name)
    }

    /// The location of `local`'s storage in `frame`.
    #[must_use]
    pub fn location(&self, frame: Frame, local: u32) -> Option<u32> {
        self.locals.get(&(frame, local)).copied()
    }

    /// The local of `frame` whose storage `node` is, if any.
    #[must_use]
    pub fn local_of(&self, frame: Frame, node: u32) -> Option<u32> {
        match self.owners.get(&node) {
            Some(&(f, local)) if f == frame => Some(local),
            _ => None,
        }
    }

    /// The storage the value in `local` can reach.
    #[must_use]
    pub fn local_targets(&self, frame: Frame, local: u32) -> IndexSet<u32> {
        self.location(frame, local)
            .map(|node| self.set(node).iter().collect())
            .unwrap_or_default()
    }

    /// Whether storage the value in `local` reaches, at any depth, may be
    /// written: it meets `written`, or [`Self::global_reach`].
    #[must_use]
    pub fn reaches_written(&self, frame: Frame, local: u32, written: &IndexSet<u32>) -> bool {
        let Some(node) = self.location(frame, local) else {
            return false;
        };
        let mut walked: IndexSet<u32> = IndexSet::default();
        let mut work: Vec<u32> = self.set(node).iter().collect();
        while let Some(loc) = work.pop() {
            if written.contains(&loc) || self.global_reach.contains(&loc) {
                return true;
            }
            if walked.insert(self.rep.root(loc)) {
                work.extend(self.set(loc).iter());
            }
        }
        false
    }

    /// The storage the call argument `arg` lets its callee write: what its
    /// value reaches where that holds a writable handle, and its own where a
    /// `&mut` parameter references it.
    #[must_use]
    pub fn handed(&self, arg: &TirExpr) -> &LocSet {
        self.set(site_of(&self.handed, arg))
    }

    /// The storage an assignment to, or a reference to, `place` denotes.
    #[must_use]
    pub fn written(&self, place: &TirExpr) -> &LocSet {
        self.set(site_of(&self.written, place))
    }

    /// `from` and every location reachable from it, short of
    /// [`Self::global_reach`]: what a write through any of it may reach that
    /// every reader of a write set does not already consult.
    #[must_use]
    pub fn reach_beyond_global(&self, from: impl IntoIterator<Item = u32>) -> IndexSet<u32> {
        self.reach_until(from, &self.global_reach)
    }

    /// `from` and every location reachable from it, not walking into `known`.
    fn reach_until(
        &self,
        from: impl IntoIterator<Item = u32>,
        known: &IndexSet<u32>,
    ) -> IndexSet<u32> {
        let mut out: IndexSet<u32> = IndexSet::default();
        // Locations merged into one node share its set, walked once.
        let mut walked: IndexSet<u32> = IndexSet::default();
        let mut work: Vec<u32> = from.into_iter().filter(|l| !known.contains(l)).collect();
        while let Some(loc) = work.pop() {
            if out.insert(loc) && walked.insert(self.rep.root(loc)) {
                work.extend(self.set(loc).iter().filter(|l| !known.contains(l)));
            }
        }
        out
    }

    /// What any call may write, whatever it is handed: the storage the
    /// globals reach, and what escaped to code the analysis cannot see.
    #[must_use]
    pub fn global_reach(&self) -> &IndexSet<u32> {
        &self.global_reach
    }
}

/// What a handle held in a value lets its holder do to another local's storage.
#[derive(Clone, Copy)]
enum Handle {
    /// Name it: a `&T`, a box, or a function value.
    Names,
    /// Write it: a `&mut T`, a writable box, or a function value, or one of
    /// those read through a `&T`.
    Writes,
}

/// The types a walk is inside of: a struct by its type, a variant by its
/// declaration and its arguments resolved to concrete types. A cycle back to
/// one adds no path to a handle.
type UnderWay = IndexSet<(Option<DefId>, Vec<TypeKey>)>;

/// `walk` inside `key`, or `false` where `key` is already under way.
fn guarded(
    key: (Option<DefId>, Vec<TypeKey>),
    under_way: &mut UnderWay,
    walk: impl FnOnce(&mut UnderWay) -> bool,
) -> bool {
    if !under_way.insert(key.clone()) {
        return false;
    }
    let answer = walk(under_way);
    under_way.swap_remove(&key);
    answer
}

/// Whether a value of a type holds a [`Handle`] at any depth. Everything else
/// is a value, which assignment copies.
struct HoldsHandle<'a> {
    type_table: &'a TypeTable,
    fields: &'a StructFields,
    handle: Handle,
    known: RefCell<IndexMap<TypeKey, bool>>,
}

impl<'a> HoldsHandle<'a> {
    fn new(type_table: &'a TypeTable, fields: &'a StructFields, handle: Handle) -> Self {
        Self {
            type_table,
            fields,
            handle,
            known: RefCell::default(),
        }
    }

    fn of(&self, ty: TypeId) -> bool {
        let key = self.type_table.type_key(ty);
        if let Some(&known) = self.known.borrow().get(&key) {
            return known;
        }
        let answer = self.walk(ty, None, &mut IndexSet::default(), 0);
        self.known.borrow_mut().insert(key, answer);
        answer
    }

    /// `env` is what the template parameters `ty` is written in take.
    fn walk(
        &self,
        ty: TypeId,
        env: Option<&EnvFrame<'_>>,
        under_way: &mut UnderWay,
        depth: usize,
    ) -> bool {
        if depth > NEEDS_COPY_DEPTH_LIMIT {
            return true;
        }
        let tt = self.type_table;
        let next = depth + 1;
        if tt.is_reference_shaped(ty) {
            let referent = match tt.get(ty) {
                ResolvedType::Ref(payload) => Some(*payload),
                ResolvedType::MutRef(_) => None,
                _ => tt.box_payload_of(ty).filter(|_| !tt.is_mut_box(ty)),
            };
            return match (self.handle, referent) {
                (Handle::Writes, Some(payload)) => self.walk(payload, env, under_way, next),
                _ => true,
            };
        }
        match tt.get(ty) {
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. } => false,
            ResolvedType::Newtype { base_type, .. } => self.walk(*base_type, env, under_way, next),
            ResolvedType::BuiltinArray(element) => self.walk(*element, env, under_way, next),
            ResolvedType::Struct { def, type_args } => {
                let Some(fields) = self.fields.get(&(*def, type_args.clone())) else {
                    return true;
                };
                guarded((None, vec![tt.type_key(ty)]), under_way, |under_way| {
                    fields
                        .iter()
                        .any(|f| self.walk(f.type_id, None, under_way, next))
                })
            }
            ResolvedType::Variant { def } => guarded((Some(*def), Vec::new()), under_way, |u| {
                self.cases(*def, None, u, next)
            }),
            ResolvedType::GenericInstance { def, type_args } => {
                if tt.is_tuple_def(*def) {
                    return type_args
                        .iter()
                        .any(|&a| self.walk(a, env, under_way, next));
                }
                if tt.variant_template_cases(*def).is_none() {
                    return true;
                }
                let frame = EnvFrame {
                    args: type_args,
                    family: &[],
                    parent: env,
                };
                // An instance whose arguments resolve is under way once, so a
                // variant recursing through its own instance ends there.
                let resolved: Option<Vec<TypeKey>> = type_args
                    .iter()
                    .map(|&a| resolve_param_in_env(a, env, tt).map(|c| tt.type_key(c)))
                    .collect();
                match resolved {
                    Some(args) => guarded((Some(*def), args), under_way, |u| {
                        self.cases(*def, Some(&frame), u, next)
                    }),
                    None => self.cases(*def, Some(&frame), under_way, next),
                }
            }
            ResolvedType::TypeParam { index, .. } => env
                .and_then(|frame| Some((*frame.args.get(*index as usize)?, frame.parent)))
                .is_none_or(|(arg, parent)| self.walk(arg, parent, under_way, next)),
            _ => true,
        }
    }

    fn cases(
        &self,
        def: DefId,
        env: Option<&EnvFrame<'_>>,
        under_way: &mut UnderWay,
        depth: usize,
    ) -> bool {
        self.type_table
            .variant_template_cases(def)
            .is_none_or(|cases| {
                cases
                    .iter()
                    .any(|&(_, _, p)| self.walk(p, env, under_way, depth))
            })
    }
}

/// How a parameter takes its argument.
#[derive(Clone, Copy)]
struct Slot {
    /// It takes the argument by reference, so the argument's own storage is
    /// handed too.
    by_ref: bool,
    /// It is a `&mut`, so the callee may write that storage.
    writes: bool,
}

impl Slot {
    /// A parameter the analysis knows nothing of.
    const UNKNOWN: Slot = Slot {
        by_ref: true,
        writes: true,
    };
}

/// What a callee is, as a call sees it.
struct Callee {
    params: Vec<u32>,
    slots: Vec<Slot>,
    /// A body the analysis reads, so its parameters and return carry the flow.
    analyzed: bool,
    /// A stub WIR build replaces with a call through a closure.
    dispatches: bool,
}

/// A function value's location: the function a call through it runs.
struct Target {
    /// The value naming the closure itself, which its `$call` takes as `self`.
    /// `None` for a plain function.
    this: Option<u32>,
    params: Vec<u32>,
    /// The node every argument flows into, for a function with no parameters
    /// the analysis sees.
    sink: Option<u32>,
    ret: u32,
}

/// What a call through a value hands each parameter.
enum Args {
    Each(Vec<Option<u32>>),
    /// Every parameter takes this node: a caller the analysis cannot see.
    Every(u32),
}

/// A call through the value in some node.
struct Site {
    args: Args,
    result: u32,
}

/// A node's state in a cycle search, Tarjan's.
#[derive(Clone, Copy, Default)]
struct Mark {
    /// The search that set it.
    search: u32,
    index: u32,
    low: u32,
    on_stack: bool,
}

/// What a node merged into another had carried along its own edges: every
/// location of `pts` but those in `pending`. Its loads, stores and calls sit
/// at these ranges of the merging node's lists.
struct Carried {
    pts: LocSet,
    pending: LocSet,
    loads: Range<usize>,
    stores: Range<usize>,
    sites: Range<usize>,
    copy_to: Vec<u32>,
}

#[derive(Default)]
struct Graph {
    pts: Vec<LocSet>,
    /// The node each node's set is held at, shared by the nodes merged into
    /// it: a cycle of copies, or a node fed by one copy alone.
    rep: DisjointSets,
    copy_to: Vec<Vec<u32>>,
    /// `loads[n]`: nodes receiving what each location in `pts[n]` holds.
    loads: Vec<Vec<u32>>,
    /// `stores[n]`: nodes whose set each location in `pts[n]` receives.
    stores: Vec<Vec<u32>>,
    /// `sites[n]`: calls through the value in `n`, bound to each target that
    /// reaches it.
    sites: Vec<Vec<Site>>,
    targets: IndexMap<u32, Target>,
    /// Locations whose value names no storage, which nothing flows into.
    inert: IndexSet<u32>,
    edges: IndexSet<(u32, u32)>,
    /// Copy edges found with equal sets at both ends, each searched for a
    /// cycle once: lazy cycle detection (Hardekopf and Lin, PLDI 2007).
    checked: IndexSet<(u32, u32)>,
    /// Each node's state in the latest cycle search that reached it.
    marks: Vec<Mark>,
    /// The number of cycle searches so far.
    search: u32,
    /// What each node gained since its edges last carried its set.
    delta: Vec<LocSet>,
    /// For each node merging others since it last settled, what each node
    /// merged had carried along its own edges, its own first.
    merged: IndexMap<u32, Vec<Carried>>,
    work: Vec<u32>,
}

impl Graph {
    fn node(&mut self) -> u32 {
        let id = self.rep.push();
        self.pts.push(LocSet::default());
        self.copy_to.push(Vec::new());
        self.loads.push(Vec::new());
        self.stores.push(Vec::new());
        self.sites.push(Vec::new());
        self.delta.push(LocSet::default());
        id
    }

    fn call_site(&mut self, callee: u32, site: Site) {
        let callee = self.find(callee);
        self.sites[callee as usize].push(site);
        let at = self.sites[callee as usize].len() - 1;
        let locs: Vec<u32> = self.pts[callee as usize].iter().collect();
        for loc in locs {
            self.bind(callee, at, loc);
        }
    }

    /// Connect call `at` through `callee` to `loc`, where `loc` is a function.
    fn bind(&mut self, callee: u32, at: usize, loc: u32) {
        let Some(target) = self.targets.get(&loc) else {
            return;
        };
        let site = &self.sites[callee as usize][at];
        let mut flows = vec![(target.ret, site.result)];
        if let (Some(sink), Args::Each(args)) = (target.sink, &site.args) {
            flows.extend(args.iter().flatten().map(|&arg| (arg, sink)));
        }
        let mut params = target.params.iter().copied();
        if let Some(this) = target.this
            && let Some(param) = params.next()
        {
            flows.push((this, param));
            if let Args::Every(from) = site.args {
                flows.push((from, param));
            }
        }
        match &site.args {
            Args::Each(args) => {
                for (arg, param) in args.iter().zip(params) {
                    if let Some(arg) = *arg {
                        flows.push((arg, param));
                    }
                }
            }
            Args::Every(from) => flows.extend(params.map(|param| (*from, param))),
        }
        for (from, to) in flows {
            self.copy(from, to);
        }
    }

    /// The node holding `node`'s set.
    fn find(&mut self, node: u32) -> u32 {
        self.rep.find(node)
    }

    fn add(&mut self, node: u32, loc: u32) {
        let node = self.find(node);
        if self.inert.contains(&node) {
            return;
        }
        if self.pts[node as usize].insert(loc) {
            if self.delta[node as usize].is_empty() {
                self.work.push(node);
            }
            self.delta[node as usize].insert(loc);
        }
    }

    /// Add `locs` to the set of `node`, a representative at the end of a copy
    /// edge.
    fn add_all(&mut self, node: u32, locs: &LocSet) {
        let n = node as usize;
        assert!(
            !self.inert.contains(&node),
            "`copy` makes no edge into an inert node"
        );
        let was_idle = self.delta[n].is_empty();
        self.pts[n].union_into(locs, &mut self.delta[n]);
        if was_idle && !self.delta[n].is_empty() {
            self.work.push(node);
        }
    }

    fn copy(&mut self, from: u32, to: u32) {
        let (from, to) = (self.find(from), self.find(to));
        // An inert node's set stays empty, so an edge at either end carries
        // nothing.
        if from == to
            || self.inert.contains(&from)
            || self.inert.contains(&to)
            || !self.edges.insert((from, to))
        {
            return;
        }
        self.copy_to[from as usize].push(to);
        let carried = std::mem::take(&mut self.pts[from as usize]);
        self.add_all(to, &carried);
        self.pts[from as usize] = carried;
    }

    fn load(&mut self, from: u32, to: u32) {
        let from = self.find(from);
        self.loads[from as usize].push(to);
        let locs: Vec<u32> = self.pts[from as usize].iter().collect();
        for loc in locs {
            self.copy(loc, to);
        }
    }

    fn store(&mut self, into: u32, from: u32) {
        let into = self.find(into);
        self.stores[into as usize].push(from);
        let locs: Vec<u32> = self.pts[into as usize].iter().collect();
        for loc in locs {
            self.copy(from, loc);
        }
    }

    /// Merge each node that only ever copies one other's set into that one,
    /// ahead of the solve: a temporary a value passes through holds what its
    /// source holds, and a cycle search along the pair would find none.
    fn substitute(&mut self) {
        // What may gain a location other than through its static copies: a
        // location (a store writes it), what a load or a call feeds, and what
        // a location is added to.
        let mut fed: IndexSet<u32> = self.inert.clone();
        for set in &self.pts {
            fed.extend(set.iter());
        }
        fed.extend(self.loads.iter().flatten().copied());
        for (&loc, target) in &self.targets {
            fed.insert(loc);
            fed.extend(target.this);
            fed.extend(target.params.iter().copied());
            fed.extend(target.sink);
            fed.insert(target.ret);
        }
        fed.extend(self.sites.iter().flatten().map(|site| site.result));
        let mut sources: IndexMap<u32, Option<u32>> = IndexMap::default();
        for &(from, to) in &self.edges {
            sources
                .entry(to)
                .and_modify(|only| *only = None)
                .or_insert(Some(from));
        }
        let mut roots: IndexSet<u32> = IndexSet::default();
        for (to, only) in sources {
            let Some(from) = only else { continue };
            let from = self.find(from);
            // A set holding more than its one source's took a location of its
            // own, by an address taken.
            if from == to
                || fed.contains(&to)
                || !self.pts[to as usize]
                    .iter()
                    .all(|loc| self.pts[from as usize].contains(loc))
            {
                continue;
            }
            self.absorb(from, to);
            roots.insert(from);
        }
        let roots: IndexSet<u32> = roots.into_iter().map(|root| self.find(root)).collect();
        for root in roots {
            self.settle(root);
        }
    }

    /// What `node`'s own edges have carried, and where they sit in its lists.
    fn carried(&mut self, node: u32) -> Carried {
        let n = node as usize;
        Carried {
            pts: self.pts[n].clone(),
            pending: std::mem::take(&mut self.delta[n]),
            loads: 0..self.loads[n].len(),
            stores: 0..self.stores[n].len(),
            sites: 0..self.sites[n].len(),
            copy_to: self.copy_to[n].clone(),
        }
    }

    /// Make `root` the node of `member`, leaving `root` to [`Self::settle`].
    fn absorb(&mut self, root: u32, member: u32) {
        let (r, m) = (root as usize, member as usize);
        self.rep.link(member, root);
        if !self.merged.contains_key(&root) {
            let own = self.carried(root);
            self.merged.insert(root, vec![own]);
        }
        let carried = match self.merged.swap_remove(&member) {
            Some(theirs) => {
                assert!(
                    self.delta[m].is_empty(),
                    "a merging node's delta is carried"
                );
                theirs
            }
            None => vec![self.carried(member)],
        };
        let (loads, stores, sites) = (
            self.loads[r].len(),
            self.stores[r].len(),
            self.sites[r].len(),
        );
        let shift = |range: Range<usize>, by: usize| range.start + by..range.end + by;
        self.merged[&root].extend(carried.into_iter().map(|c| Carried {
            loads: shift(c.loads, loads),
            stores: shift(c.stores, stores),
            sites: shift(c.sites, sites),
            ..c
        }));
        let pts = std::mem::take(&mut self.pts[m]);
        self.pts[r].union_into(&pts, &mut LocSet::default());
        let copy_to = std::mem::take(&mut self.copy_to[m]);
        self.copy_to[r].extend(copy_to);
        let loads = std::mem::take(&mut self.loads[m]);
        self.loads[r].extend(loads);
        let stores = std::mem::take(&mut self.stores[m]);
        self.stores[r].extend(stores);
        let sites = std::mem::take(&mut self.sites[m]);
        self.sites[r].extend(sites);
    }

    /// Point the edges `root` took from the nodes it absorbed at
    /// representatives, and carry along each merged node's own edges what they
    /// had not carried: the merged set's locations it lacked, and its delta.
    fn settle(&mut self, root: u32) {
        let r = root as usize;
        let mut copy_to: IndexSet<u32> = IndexSet::default();
        for to in std::mem::take(&mut self.copy_to[r]) {
            let to = self.find(to);
            if to != root && copy_to.insert(to) {
                self.edges.insert((root, to));
            }
        }
        self.copy_to[r] = copy_to.into_iter().collect();
        let carried = self
            .merged
            .swap_remove(&root)
            .expect("a settled node merged others");
        let missing: Vec<LocSet> = carried
            .iter()
            .map(|c| {
                let mut missing = c.pending.clone();
                self.pts[r].difference_into(&c.pts, &mut missing);
                missing
            })
            .collect();
        for (c, missing) in carried.into_iter().zip(missing) {
            if missing.is_empty() {
                continue;
            }
            self.carry(root, &missing, c.loads, c.stores, c.sites);
            for to in c.copy_to {
                let to = self.find(to);
                if to != root {
                    self.add_all(to, &missing);
                }
            }
        }
    }

    /// Carry `locs`, new in `node`'s set, along its loads, stores and calls in
    /// the ranges given.
    fn carry(
        &mut self,
        node: u32,
        locs: &LocSet,
        loads: Range<usize>,
        stores: Range<usize>,
        sites: Range<usize>,
    ) {
        let n = node as usize;
        for loc in locs.iter() {
            for i in loads.clone() {
                let to = self.loads[n][i];
                self.copy(loc, to);
            }
            for i in stores.clone() {
                let from = self.stores[n][i];
                self.copy(from, loc);
            }
            for at in sites.clone() {
                self.bind(node, at, loc);
            }
        }
    }

    fn solve(&mut self) {
        self.substitute();
        while let Some(node) = self.work.pop() {
            if self.find(node) != node {
                continue;
            }
            let delta = std::mem::take(&mut self.delta[node as usize]);
            let n = node as usize;
            let (loads, stores, sites) = (
                0..self.loads[n].len(),
                0..self.stores[n].len(),
                0..self.sites[n].len(),
            );
            self.carry(node, &delta, loads, stores, sites);
            let mut equal_ends = Vec::new();
            for i in 0..self.copy_to[n].len() {
                let to = self.find(self.copy_to[n][i]);
                if to == node {
                    continue;
                }
                self.add_all(to, &delta);
                let (from_set, to_set) = (&self.pts[n], &self.pts[to as usize]);
                // A node no copy leaves lies on no cycle.
                if from_set.len() == to_set.len()
                    && !self.copy_to[to as usize].is_empty()
                    && !self.checked.contains(&(node, to))
                    && from_set == to_set
                {
                    self.checked.insert((node, to));
                    equal_ends.push(to);
                }
            }
            for to in equal_ends {
                if self.find(to) == to {
                    self.collapse_cycles_from(to);
                }
            }
        }
    }

    /// Merge each cycle of copies reachable from `start` into one node: every
    /// node on a cycle ends up with the same set, which one node then holds.
    fn collapse_cycles_from(&mut self, start: u32) {
        // Tarjan's algorithm, iteratively, over marks reused across searches:
        // a mark from an earlier search is a node this one has not visited.
        self.search += 1;
        let search = self.search;
        self.marks.resize(self.pts.len(), Mark::default());
        let mut count = 0;
        let mut stack: Vec<u32> = Vec::new();
        let mut walk: Vec<(u32, usize)> = Vec::new();
        let mut cycles: Vec<Vec<u32>> = Vec::new();
        let mut next = Some(start);
        loop {
            if let Some(v) = next.take() {
                self.marks[v as usize] = Mark {
                    search,
                    index: count,
                    low: count,
                    on_stack: true,
                };
                count += 1;
                stack.push(v);
                walk.push((v, 0));
            }
            let Some(&(v, i)) = walk.last() else { break };
            if let Some(&to) = self.copy_to[v as usize].get(i) {
                walk.last_mut().expect("the walk is under way").1 += 1;
                let w = self.find(to);
                if w == v {
                    continue;
                }
                let mark = self.marks[w as usize];
                if mark.search != search {
                    next = Some(w);
                } else if mark.on_stack {
                    let low = &mut self.marks[v as usize].low;
                    *low = (*low).min(mark.index);
                }
                continue;
            }
            walk.pop();
            let Mark { index, low, .. } = self.marks[v as usize];
            if let Some(&(parent, _)) = walk.last() {
                let parent_low = &mut self.marks[parent as usize].low;
                *parent_low = (*parent_low).min(low);
            }
            if low == index {
                let at = stack
                    .iter()
                    .rposition(|&w| w == v)
                    .expect("a node leaves the stack only with its component");
                let cycle: Vec<u32> = stack.drain(at..).collect();
                for &w in &cycle {
                    self.marks[w as usize].on_stack = false;
                }
                if cycle.len() > 1 {
                    cycles.push(cycle);
                }
            }
        }
        for cycle in cycles {
            let root = cycle[0];
            for &member in &cycle[1..] {
                self.absorb(root, member);
            }
            self.settle(root);
        }
    }
}

struct Builder<'a> {
    type_table: &'a TypeTable,
    names: HoldsHandle<'a>,
    writes: HoldsHandle<'a>,
    local_types: IndexMap<(Frame, u32), TypeId>,
    builtins: &'a BuiltinDeclarations,
    callees: Vec<Callee>,
    functions: &'a FuncIndex,
    returns: Vec<u32>,
    g: Graph,
    locals: IndexMap<(Frame, u32), u32>,
    globals: IndexMap<(ModuleSource, String), u32>,
    escape: u32,
    handed: IndexMap<usize, (u32, Span)>,
    written: IndexMap<usize, (u32, Span)>,
    next_frame: Frame,
    frame: Frame,
    /// Where a `return` in the current frame sends its value.
    return_to: u32,
    labels: Vec<(String, u32)>,
    /// Each closure functor's `$call`, by functor id.
    closure_calls: Vec<Frame>,
    /// The value of a function the analysis has no body for: what a call
    /// through it hands escapes, and its result is what escapes.
    unseen_function: u32,
}

#[must_use]
pub fn compute(flat: &FlatPackage, fields: &StructFields, functors: &[ClosureFunctor]) -> PointsTo {
    let type_table = flat.type_table.borrow();
    let mut functions = FuncIndex::default();
    for (i, func) in flat.functions.iter().enumerate() {
        let func = func.borrow();
        let id = u32::try_from(i).expect("function count fits u32");
        functions.insert(func.module_source.clone(), func.name.clone(), id);
    }
    let local_types = flat
        .functions
        .iter()
        .enumerate()
        .flat_map(|(i, func)| {
            let frame = u32::try_from(i).expect("function count fits u32");
            let func = func.borrow();
            let params = func.params.iter().map(|p| (p.local_index, p.type_id));
            let locals = (0u32..).zip(func.locals.iter().map(|l| l.type_id));
            params
                .chain(locals)
                .map(|(local, ty)| ((frame, local), ty))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut b = Builder {
        type_table: &type_table,
        names: HoldsHandle::new(&type_table, fields, Handle::Names),
        writes: HoldsHandle::new(&type_table, fields, Handle::Writes),
        local_types,
        builtins: &flat.builtin_declarations,
        callees: Vec::new(),
        functions: &functions,
        returns: Vec::new(),
        g: Graph::default(),
        locals: IndexMap::default(),
        globals: IndexMap::default(),
        escape: 0,
        handed: IndexMap::default(),
        written: IndexMap::default(),
        next_frame: u32::try_from(flat.functions.len()).expect("function count fits u32"),
        frame: 0,
        return_to: 0,
        labels: Vec::new(),
        closure_calls: Vec::new(),
        unseen_function: 0,
    };
    b.escape = b.g.node();
    // Whatever escapes may reach, and be stored in, whatever else escapes.
    let reached = b.held_by(b.escape);
    b.g.copy(reached, b.escape);
    b.g.store(b.escape, b.escape);
    // And may call whatever function escapes, with whatever escapes.
    b.g.call_site(
        b.escape,
        Site {
            args: Args::Every(b.escape),
            result: b.escape,
        },
    );
    let unseen = b.g.node();
    b.g.targets.insert(
        unseen,
        Target {
            this: None,
            params: Vec::new(),
            sink: Some(b.escape),
            ret: b.escape,
        },
    );
    b.unseen_function = b.naming(unseen);
    for (i, func) in flat.functions.iter().enumerate() {
        let func = func.borrow();
        let frame = u32::try_from(i).expect("function count fits u32");
        let params = func
            .params
            .iter()
            .map(|p| b.local(frame, p.local_index))
            .collect();
        let slots = func
            .params
            .iter()
            .map(|p| Slot {
                by_ref: type_table.is_reference_shaped(p.type_id),
                writes: p.is_mut_ref || could_write_through(p.type_id, &type_table),
            })
            .collect();
        let dispatches = matches!(func.kind, FunctionKind::FnCanonicalDispatch { .. });
        b.callees.push(Callee {
            params,
            slots,
            analyzed: func.body.is_some() && !dispatches,
            dispatches,
        });
        let ret = b.g.node();
        b.returns.push(ret);
    }
    b.closure_calls = functors
        .iter()
        .map(|functor| {
            let call = functor.call_method.borrow();
            functions
                .id(&call.module_source, &call.name)
                .expect("closure planning adds each `$call` to the package")
        })
        .collect();
    for global in &flat.globals {
        let node = b.global(&global.module_source, &global.name);
        b.frame = b.next_frame;
        b.next_frame += 1;
        b.return_to = node;
        if let Some(value) = b.eval(global.init.slot_expr()) {
            b.g.copy(value, node);
        }
    }
    for (i, func) in flat.functions.iter().enumerate() {
        let func = func.borrow();
        let Some(body) = &func.body else { continue };
        b.frame = u32::try_from(i).expect("function count fits u32");
        b.return_to = b.returns[i];
        if let Some(value) = b.block_value(body) {
            b.g.copy(value, b.return_to);
        }
    }
    b.g.solve();
    // Code the analysis cannot see may run under any call and write what
    // escaped to it, as any call may write what a global reaches.
    let global_nodes: Vec<u32> = b.globals.values().copied().chain([b.escape]).collect();
    let mut rep = std::mem::take(&mut b.g.rep);
    rep.flatten();
    let pts = std::mem::take(&mut b.g.pts);
    let locals = std::mem::take(&mut b.locals);
    let owners = locals.iter().map(|(&key, &node)| (node, key)).collect();
    let mut result = PointsTo {
        functions: FuncIndex::default(),
        locals,
        owners,
        pts,
        rep,
        handed: std::mem::take(&mut b.handed),
        written: std::mem::take(&mut b.written),
        global_reach: IndexSet::default(),
    };
    result.global_reach = result.reach_until(global_nodes, &IndexSet::default());
    drop(b);
    result.functions = functions;
    result
}

impl Builder<'_> {
    fn local(&mut self, frame: Frame, local: u32) -> u32 {
        if let Some(&node) = self.locals.get(&(frame, local)) {
            return node;
        }
        let node = self.g.node();
        self.locals.insert((frame, local), node);
        if let Some(&ty) = self.local_types.get(&(frame, local))
            && !self.names.of(ty)
        {
            self.g.inert.insert(node);
        }
        node
    }

    fn global(&mut self, module: &ModuleSource, name: &str) -> u32 {
        if let Some(&node) = self.globals.get(&(module.clone(), name.to_string())) {
            return node;
        }
        let node = self.g.node();
        self.globals
            .insert((module.clone(), name.to_string()), node);
        node
    }

    /// A function value running `frame`'s function: a location holding what
    /// the closure captured, which a closure's `$call` takes as `self`.
    fn function_value(&mut self, frame: Frame, is_closure: bool, captured: Vec<u32>) -> u32 {
        let object = self.g.node();
        for c in captured {
            self.g.copy(c, object);
        }
        let value = self.naming(object);
        let target = Target {
            this: is_closure.then_some(value),
            params: self.callees[frame as usize].params.clone(),
            sink: None,
            ret: self.returns[frame as usize],
        };
        self.g.targets.insert(object, target);
        value
    }

    /// A node holding the union of `values`, `None` where there is nothing.
    fn join(&mut self, values: impl IntoIterator<Item = Option<u32>>) -> Option<u32> {
        let values: Vec<u32> = values.into_iter().flatten().collect();
        match values.as_slice() {
            [] => None,
            [one] => Some(*one),
            _ => {
                let node = self.g.node();
                for v in values {
                    self.g.copy(v, node);
                }
                Some(node)
            }
        }
    }

    /// A node naming the location `loc` alone.
    fn naming(&mut self, loc: u32) -> u32 {
        let node = self.g.node();
        self.g.add(node, loc);
        node
    }

    /// A node holding what the locations in `reference`'s set hold.
    fn held_by(&mut self, reference: u32) -> u32 {
        let node = self.g.node();
        self.g.load(reference, node);
        node
    }

    fn deref(&mut self, reference: Option<u32>) -> Option<u32> {
        reference.map(|r| self.held_by(r))
    }

    /// The node whose set is the storage `expr` names, evaluating what it
    /// reads, or `None` for an expression no storage holds, which is then left
    /// unevaluated.
    fn place(&mut self, expr: &TirExpr) -> Option<u32> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                let loc = self.local(self.frame, *index);
                Some(self.naming(loc))
            }
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                let loc = self.global(module_source, name);
                Some(self.naming(loc))
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => Some(self.referent(inner)),
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => self.projected(inner),
            TirExprKind::Index { expr: inner, index } => {
                let place = self.projected(inner)?;
                self.eval(index);
                Some(place)
            }
            _ => None,
        }
    }

    /// The storage a projection over `base` names: `base`'s own, or what it
    /// refers to where it is a reference read through.
    fn projected(&mut self, base: &TirExpr) -> Option<u32> {
        if self.type_table.is_reference_shaped(base.type_id) {
            Some(self.referent(base))
        } else {
            self.place(base)
        }
    }

    /// The storage the reference `reference` evaluates to names.
    fn referent(&mut self, reference: &TirExpr) -> u32 {
        self.eval(reference).unwrap_or_else(|| self.g.node())
    }

    /// The storage a reference to, or an assignment to, `expr` names: its place,
    /// or a temporary holding its value, as `get().field = v` writes.
    fn referenced(&mut self, expr: &TirExpr) -> u32 {
        let place = self.place(expr).unwrap_or_else(|| {
            let temp = self.g.node();
            if let Some(value) = self.eval(expr) {
                self.g.copy(value, temp);
            }
            self.naming(temp)
        });
        note_site(&mut self.written, expr, place);
        place
    }

    /// Evaluate `expr` once: its value, and the storage it names where it is
    /// a place.
    fn operand(&mut self, expr: &TirExpr) -> (Option<u32>, Option<u32>) {
        let Some(place) = self.place(expr) else {
            return (self.eval(expr), None);
        };
        // A local's node holds its value already; reading it evaluates nothing.
        let value = if matches!(expr.kind, TirExprKind::Local { .. }) {
            self.eval(expr)
        } else {
            self.names.of(expr.type_id).then(|| self.held_by(place))
        };
        (value, Some(place))
    }

    /// Evaluate `arg` for a slot that may take it by reference: its value, and
    /// its own storage where `by_ref` takes a value-typed place by reference. A
    /// reference argument names the storage in its value already.
    ///
    /// Recorded for the walk is what the callee can write through it: the
    /// value where its type holds a writable handle, and its own storage where
    /// the slot is a `&mut`.
    fn hand(&mut self, arg: &TirExpr, slot: Slot) -> Option<u32> {
        let (value, own) = if slot.by_ref && !self.type_table.is_reference_shaped(arg.type_id) {
            self.operand(arg)
        } else {
            (self.eval(arg), None)
        };
        let writable_value = value.filter(|_| self.writes.of(arg.type_id));
        let writable_own = own.filter(|_| slot.writes);
        let recorded = self
            .join([writable_value, writable_own])
            .unwrap_or_else(|| self.g.node());
        note_site(&mut self.handed, arg, recorded);
        self.join([value, own])
    }

    fn call(&mut self, id: Frame, args: &mut dyn Iterator<Item = &TirExpr>) -> Option<u32> {
        let callee = &self.callees[id as usize];
        let slots = callee.slots.clone();
        if callee.dispatches {
            return self.opaque_call(&slots, args);
        }
        if !callee.analyzed {
            return self.local_merge(&slots, args);
        }
        let params = callee.params.clone();
        for (i, arg) in args.enumerate() {
            let value = self.hand(arg, slots[i]);
            if let Some(value) = value {
                self.g.copy(value, params[i]);
            }
        }
        Some(self.returns[id as usize])
    }

    /// A call the analysis cannot follow into: everything handed escapes, and
    /// the result is what escapes. A parameter `slots` does not cover may
    /// take its argument by reference and write.
    fn opaque_call(
        &mut self,
        slots: &[Slot],
        args: &mut dyn Iterator<Item = &TirExpr>,
    ) -> Option<u32> {
        for (i, arg) in args.enumerate() {
            let slot = slots.get(i).copied().unwrap_or(Slot::UNKNOWN);
            if let Some(v) = self.hand(arg, slot) {
                self.g.copy(v, self.escape);
            }
        }
        Some(self.escape)
    }

    /// How each parameter of a value of type `callee` takes its argument.
    fn fn_slots(&self, callee: TypeId) -> Vec<Slot> {
        let tt = self.type_table;
        let ResolvedType::Function { params, .. } = tt.get(tt.peel_refs_and_box(callee)) else {
            unreachable!("an indirect call's callee is a function value");
        };
        params
            .iter()
            .map(|&p| Slot {
                by_ref: tt.is_reference_shaped(p),
                writes: could_write_through(p, tt),
            })
            .collect()
    }

    /// A body-less callee: what any argument carries or names, or what that
    /// storage holds, may be stored through a `&mut` parameter and returned.
    fn local_merge(
        &mut self,
        slots: &[Slot],
        args: &mut dyn Iterator<Item = &TirExpr>,
    ) -> Option<u32> {
        let carried = self.g.node();
        let written = self.g.node();
        for (i, arg) in args.enumerate() {
            let Some(handed) = self.hand(arg, slots[i]) else {
                continue;
            };
            self.g.copy(handed, carried);
            if slots[i].writes {
                self.g.copy(handed, written);
            }
        }
        let held = self.held_by(carried);
        self.g.copy(held, carried);
        self.g.store(written, carried);
        Some(carried)
    }

    /// A builtin, as its declaration states: the result is part of the
    /// arguments it names, and each retained argument — or what it points to
    /// — lands where the declaration says.
    fn builtin_call(
        &mut self,
        decl: &BuiltinDeclaration,
        returns_reference: bool,
        args: &[&TirExpr],
    ) -> Option<u32> {
        let handed: Vec<Option<u32>> = args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                self.hand(
                    arg,
                    Slot {
                        by_ref: decl.reference_params.contains(&i),
                        writes: decl.mut_params.contains(&i),
                    },
                )
            })
            .collect();
        let result = self.g.node();
        // A part of what a reference names is held there: a value read out of
        // it carries what that storage holds, a reference into it names it.
        if let Some(ReturnConvention::PartOf(params)) = &decl.returns {
            for &p in params {
                let Some(h) = handed[p] else { continue };
                if self.type_table.is_reference_shaped(args[p].type_id) && !returns_reference {
                    self.g.load(h, result);
                } else {
                    self.g.copy(h, result);
                }
            }
        }
        for spec in &decl.retains {
            let Some(source) = handed[spec.source] else {
                continue;
            };
            let kept = if spec.elements {
                self.held_by(source)
            } else {
                source
            };
            let into: Vec<usize> = match spec.into {
                RetainInto::Result => {
                    self.g.copy(kept, result);
                    Vec::new()
                }
                RetainInto::Param(p) => vec![p],
                RetainInto::Anywhere => {
                    self.g.copy(kept, result);
                    self.g.copy(kept, self.escape);
                    decl.mut_params.iter().copied().collect()
                }
            };
            for dst in into.into_iter().filter_map(|p| handed[p]) {
                self.g.store(dst, kept);
            }
        }
        Some(result)
    }

    fn assign(&mut self, target: &TirExpr, value: Option<u32>) {
        if let TirExprKind::Local { index, .. } = &target.kind {
            let node = self.local(self.frame, *index);
            if let Some(v) = value {
                self.g.copy(v, node);
            }
        } else {
            let place = self.referenced(target);
            if let Some(v) = value {
                self.g.store(place, v);
            }
        }
    }

    /// Bind `pattern` to `scrutinee`, evaluated as [`Self::operand`] to `value`
    /// and `own`.
    fn bind_pattern(
        &mut self,
        pattern: &TirPattern,
        scrutinee: &TirExpr,
        (value, own): (Option<u32>, Option<u32>),
    ) {
        let mut binds: IndexSet<u32> = IndexSet::default();
        collect_pattern_bindings(pattern, &mut binds);
        if binds.is_empty() {
            return;
        }
        // A binding takes part of the value, or what it refers to, or — lifted
        // to a reference to the scrutinee — the scrutinee's own storage.
        let referent = if self.type_table.is_reference_shaped(scrutinee.type_id) {
            self.deref(value)
        } else {
            None
        };
        for local in binds {
            let node = self.local(self.frame, local);
            for v in [value, referent, own].into_iter().flatten() {
                self.g.copy(v, node);
            }
        }
    }

    fn block_value(&mut self, block: &TirBlock) -> Option<u32> {
        let mut tail = None;
        let last = block.stmts.len().saturating_sub(1);
        for (i, stmt) in block.stmts.iter().enumerate() {
            let value = self.stmt(stmt);
            if i == last {
                tail = value;
            }
        }
        tail
    }

    fn stmt(&mut self, stmt: &TirStmt) -> Option<u32> {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                let v = self.eval(value);
                let node = self.local(self.frame, *local_index);
                if let Some(v) = v {
                    self.g.copy(v, node);
                }
                None
            }
            TirStmtKind::Expr(e) => self.eval(e),
            TirStmtKind::Return { value } => {
                if let Some(v) = value.as_ref().and_then(|v| self.eval(v)) {
                    self.g.copy(v, self.return_to);
                }
                None
            }
            TirStmtKind::TaskReturn { value } => {
                if let Some(v) = self.eval(value) {
                    self.g.copy(v, self.return_to);
                }
                None
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.eval(condition);
                self.block_value(then_block);
                if let Some(b) = else_block {
                    self.block_value(b);
                }
                None
            }
            TirStmtKind::Loop { body } => {
                self.block_value(body);
                None
            }
            TirStmtKind::Break { label, value } => {
                let v = value.as_ref().and_then(|v| self.eval(v));
                if let (Some(v), Some(label)) = (v, label)
                    && let Some(&(_, node)) = self.labels.iter().rev().find(|(l, _)| l == label)
                {
                    self.g.copy(v, node);
                }
                None
            }
            TirStmtKind::Continue => None,
            TirStmtKind::LabeledBlock { block, .. } => {
                self.block_value(block);
                None
            }
            TirStmtKind::LetDestructure { pattern, value } => {
                let scrutinee = self.operand(value);
                self.bind_pattern(pattern, value, scrutinee);
                None
            }
            TirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.eval(iterable);
                self.block_value(body);
                None
            }
        }
    }

    /// The node holding what `expr`'s value can reach, `None` for a value
    /// that carries nothing. Walks every subexpression for the flow it makes.
    fn eval(&mut self, expr: &TirExpr) -> Option<u32> {
        let value = self.eval_kind(expr);
        if !self.names.of(expr.type_id) {
            return None;
        }
        value
    }

    /// What a part of `whole` carries: what `whole` carries, or where `whole`
    /// is a reference read through, what the storage it names holds.
    fn part_of(&mut self, whole: &TirExpr) -> Option<u32> {
        let v = self.eval(whole);
        if self.type_table.is_reference_shaped(whole.type_id) {
            self.deref(v)
        } else {
            v
        }
    }

    fn eval_kind(&mut self, expr: &TirExpr) -> Option<u32> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => Some(self.local(self.frame, *index)),
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => Some(self.global(module_source, name)),
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => {
                let node = self.global(module_source, name);
                if let Some(v) = self.eval(value) {
                    self.g.copy(v, node);
                }
                None
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => Some(self.referenced(place)),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => {
                let v = self.eval(inner);
                self.deref(v)
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => self.part_of(inner),
            TirExprKind::Cast { expr: inner, .. } => {
                let v = self.eval(inner);
                if self.type_table.is_reference_shaped(inner.type_id)
                    && !is_reference_type(expr.type_id, self.type_table)
                {
                    self.deref(v)
                } else {
                    v
                }
            }
            TirExprKind::Index { expr: inner, index } => {
                self.eval(index);
                self.part_of(inner)
            }
            TirExprKind::Assign { target, value } => {
                let v = self.eval(value);
                self.assign(target, v);
                None
            }
            TirExprKind::Call { func, args, .. } => {
                let mut args = args.iter().map(|a| &a.expr);
                if let Some(decl) = self.builtins.get(&**func) {
                    let args: Vec<&TirExpr> = args.collect();
                    let returns_reference = self.type_table.is_reference_shaped(expr.type_id);
                    return self.builtin_call(decl, returns_reference, &args);
                }
                match self.functions.id(&func.module_source, &func.name) {
                    Some(id) => self.call(id, &mut args),
                    None => self.opaque_call(&[], &mut args),
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                let slots = self.fn_slots(callee.type_id);
                let v = self
                    .hand(
                        callee,
                        Slot {
                            by_ref: false,
                            writes: false,
                        },
                    )
                    .expect("a function value names the function it runs");
                let args = args
                    .iter()
                    .zip(slots)
                    .map(|(arg, slot)| self.hand(arg, slot))
                    .collect();
                let result = self.g.node();
                self.g.call_site(
                    v,
                    Site {
                        args: Args::Each(args),
                        result,
                    },
                );
                Some(result)
            }
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => Some(match self.functions.id(module_source, name) {
                Some(id) if self.callees[id as usize].analyzed => {
                    self.function_value(id, false, Vec::new())
                }
                // No body carries the flow from its parameters.
                _ => self.unseen_function,
            }),
            TirExprKind::Block(block) => self.block_value(block),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.eval(condition);
                let t = self.block_value(then_branch);
                let e = else_branch.as_ref().and_then(|b| self.block_value(b));
                self.join([t, e])
            }
            TirExprKind::Match { expr: scrut, arms } => {
                let scrutinee = self.operand(scrut);
                let mut results = Vec::new();
                for arm in arms {
                    self.bind_pattern(&arm.pattern, scrut, scrutinee);
                    if let Some(guard) = &arm.guard {
                        self.eval(guard);
                    }
                    results.push(self.eval(&arm.body));
                }
                self.join(results)
            }
            TirExprKind::LabeledBlock { label, block, .. } => {
                let node = self.g.node();
                self.labels.push((label.clone(), node));
                let tail = self.block_value(block);
                self.labels.pop();
                if let Some(t) = tail {
                    self.g.copy(t, node);
                }
                Some(node)
            }
            TirExprKind::WithHandler { bindings, body, .. } => {
                for binding in bindings {
                    if let Some(v) = self.eval(&binding.handler) {
                        self.g.copy(v, self.escape);
                    }
                }
                self.block_value(body)
            }
            TirExprKind::Closure {
                captures,
                functor_id,
                ..
            } => {
                let functor = functor_id.expect("closure planning numbers every closure");
                let call = self.closure_calls[functor as usize];
                let mut held = Vec::new();
                for capture in captures {
                    match capture.source {
                        CaptureSource::Local(index) => {
                            let node = self.local(self.frame, index);
                            // A capture by reference holds the local's storage.
                            if self.type_table.is_reference_shaped(capture.type_id) {
                                held.push(self.naming(node));
                            }
                            held.push(node);
                        }
                        // What the enclosing `$call` captured: its `self` holds it.
                        CaptureSource::Capture(_) => {
                            let this = self.callees[self.frame as usize].params[0];
                            held.push(self.held_by(this));
                        }
                    }
                }
                Some(self.function_value(call, true, held))
            }
            TirExprKind::Capture { .. } => {
                unreachable!("closure planning rewrites a capture into a read of `self`")
            }
            TirExprKind::VariadicTupleComprehension {
                iterable,
                destructure,
                body,
                ..
            } => {
                self.eval(iterable);
                for stmt in destructure {
                    self.stmt(stmt);
                }
                self.eval(body)
            }
            _ => {
                let mut children = Vec::new();
                collect_child_exprs(expr, &mut children);
                let values: Vec<Option<u32>> = children.into_iter().map(|c| self.eval(c)).collect();
                self.join(values)
            }
        }
    }
}
