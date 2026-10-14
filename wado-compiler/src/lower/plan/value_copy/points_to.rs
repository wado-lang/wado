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
//! the arguments of a call to a function with no declaration here, and those
//! of an `Inspect` dispatch stub, whose WIR body calls through its argument.

use std::cell::RefCell;

use super::analyze::collect_pattern_bindings;
use super::funcset::FuncIndex;
use super::is_reference_type;
use super::last_use::collect_child_exprs;
use super::place::could_write_through;
use super::{EnvFrame, NEEDS_COPY_DEPTH_LIMIT, resolve_param_in_env};
use crate::defs::DefId;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{
    BuiltinDeclaration, BuiltinDeclarations, CaptureSource, ClosureFunctor, FunctionKind,
    ResolvedType, RetainInto, ReturnConvention, StructFields, TirBlock, TirExpr, TirExprKind,
    TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeKey, TypeTable,
};
use crate::token::Span;

/// A frame: a function, or a global's initializer, whose locals have locations
/// of their own.
type Frame = u32;

/// The solved analysis. Sets name nodes that are locations: a frame's local,
/// a global, a function value, or the temporary a borrow of a call's result
/// takes.
pub struct PointsTo {
    functions: FuncIndex,
    locals: IndexMap<(Frame, u32), u32>,
    /// The frame and local each location node is, the inverse of `locals`.
    owners: IndexMap<u32, (Frame, u32)>,
    pts: Vec<IndexSet<u32>>,
    /// The node the value each call argument hands its callee flows from, and
    /// the argument's span, keyed by the argument expression's address. Valid
    /// while no body is rewritten, as the fold, which computes and reads it,
    /// rewrites none.
    handed: IndexMap<usize, (u32, Span)>,
    /// What any call may write: [`Self::global_reach`].
    global_reach: IndexSet<u32>,
}

impl PointsTo {
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
            .map(|node| self.pts[node as usize].clone())
            .unwrap_or_default()
    }

    /// Whether the storage the value in `local` reaches may be written: it
    /// meets `written`, or [`Self::global_reach`].
    #[must_use]
    pub fn reaches_written(&self, frame: Frame, local: u32, written: &IndexSet<u32>) -> bool {
        self.location(frame, local).is_some_and(|node| {
            self.pts[node as usize]
                .iter()
                .any(|n| written.contains(n) || self.global_reach.contains(n))
        })
    }

    /// The storage the call argument `arg` lets its callee write: what its
    /// value reaches where that holds a writable handle, and its own where a
    /// `&mut` parameter borrows it.
    #[must_use]
    pub fn handed(&self, arg: &TirExpr) -> &IndexSet<u32> {
        let &(node, span) = self
            .handed
            .get(&std::ptr::from_ref(arg).addr())
            .expect("the analysis hands every argument of every call it reads");
        assert_eq!(
            span, arg.span,
            "an argument keyed by address moved after the analysis"
        );
        &self.pts[node as usize]
    }

    /// `from` and every location reachable from it: what a write through any of
    /// it may reach.
    #[must_use]
    pub fn reach(&self, from: impl IntoIterator<Item = u32>) -> IndexSet<u32> {
        let mut out: IndexSet<u32> = IndexSet::default();
        let mut work: Vec<u32> = from.into_iter().collect();
        while let Some(loc) = work.pop() {
            if out.insert(loc) {
                work.extend(self.pts[loc as usize].iter().copied());
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
    /// It borrows the argument, so the argument's own storage is handed too.
    borrows: bool,
    /// It is a `&mut`, so the callee may write that storage.
    writes: bool,
}

impl Slot {
    /// A parameter the analysis knows nothing of.
    const UNKNOWN: Slot = Slot {
        borrows: true,
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

#[derive(Default)]
struct Graph {
    pts: Vec<IndexSet<u32>>,
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
    delta: Vec<Vec<u32>>,
    work: Vec<u32>,
}

impl Graph {
    fn node(&mut self) -> u32 {
        let id = u32::try_from(self.pts.len()).expect("node count fits u32");
        self.pts.push(IndexSet::default());
        self.copy_to.push(Vec::new());
        self.loads.push(Vec::new());
        self.stores.push(Vec::new());
        self.sites.push(Vec::new());
        self.delta.push(Vec::new());
        id
    }

    fn call_site(&mut self, callee: u32, site: Site) {
        self.sites[callee as usize].push(site);
        let at = self.sites[callee as usize].len() - 1;
        let locs: Vec<u32> = self.pts[callee as usize].iter().copied().collect();
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

    fn add(&mut self, node: u32, loc: u32) {
        if self.inert.contains(&node) {
            return;
        }
        if self.pts[node as usize].insert(loc) {
            if self.delta[node as usize].is_empty() {
                self.work.push(node);
            }
            self.delta[node as usize].push(loc);
        }
    }

    fn copy(&mut self, from: u32, to: u32) {
        if from == to || !self.edges.insert((from, to)) {
            return;
        }
        self.copy_to[from as usize].push(to);
        let carried: Vec<u32> = self.pts[from as usize].iter().copied().collect();
        for loc in carried {
            self.add(to, loc);
        }
    }

    fn load(&mut self, from: u32, to: u32) {
        self.loads[from as usize].push(to);
        let locs: Vec<u32> = self.pts[from as usize].iter().copied().collect();
        for loc in locs {
            self.copy(loc, to);
        }
    }

    fn store(&mut self, into: u32, from: u32) {
        self.stores[into as usize].push(from);
        let locs: Vec<u32> = self.pts[into as usize].iter().copied().collect();
        for loc in locs {
            self.copy(from, loc);
        }
    }

    fn solve(&mut self) {
        while let Some(node) = self.work.pop() {
            let delta = std::mem::take(&mut self.delta[node as usize]);
            let n = node as usize;
            for &loc in &delta {
                for i in 0..self.loads[n].len() {
                    let to = self.loads[n][i];
                    self.copy(loc, to);
                }
                for i in 0..self.stores[n].len() {
                    let from = self.stores[n][i];
                    self.copy(from, loc);
                }
                for at in 0..self.sites[n].len() {
                    self.bind(node, at, loc);
                }
            }
            for i in 0..self.copy_to[n].len() {
                let to = self.copy_to[n][i];
                for &loc in &delta {
                    self.add(to, loc);
                }
            }
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
        next_frame: u32::try_from(flat.functions.len()).expect("function count fits u32"),
        frame: 0,
        return_to: 0,
        labels: Vec::new(),
        closure_calls: Vec::new(),
        unseen_function: 0,
    };
    b.escape = b.g.node();
    // Whatever escapes may reach, and be stored in, whatever else escapes.
    let reached = b.g.node();
    b.g.load(b.escape, reached);
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
    b.unseen_function = b.g.node();
    b.g.add(b.unseen_function, unseen);
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
                borrows: type_table.is_reference_shaped(p.type_id),
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
    let pts = std::mem::take(&mut b.g.pts);
    let locals = std::mem::take(&mut b.locals);
    let owners = locals.iter().map(|(&key, &node)| (node, key)).collect();
    let mut result = PointsTo {
        functions: FuncIndex::default(),
        locals,
        owners,
        pts,
        handed: std::mem::take(&mut b.handed),
        global_reach: IndexSet::default(),
    };
    result.global_reach = result.reach(global_nodes);
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
        let value = self.g.node();
        self.g.add(value, object);
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

    fn deref(&mut self, reference: Option<u32>) -> Option<u32> {
        let reference = reference?;
        let node = self.g.node();
        self.g.load(reference, node);
        Some(node)
    }

    /// The node whose set is the storage `place` names.
    fn place(&mut self, place: &TirExpr) -> u32 {
        match &place.kind {
            TirExprKind::Local { index, .. } => {
                let loc = self.local(self.frame, *index);
                let node = self.g.node();
                self.g.add(node, loc);
                node
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => self.eval(inner).unwrap_or_else(|| self.g.node()),
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => self.projected(inner),
            TirExprKind::Index { expr: inner, index } => {
                self.eval(index);
                self.projected(inner)
            }
            _ => {
                // A temporary holding the value, which the borrow names.
                let temp = self.g.node();
                if let Some(value) = self.eval(place) {
                    self.g.copy(value, temp);
                }
                let node = self.g.node();
                self.g.add(node, temp);
                node
            }
        }
    }

    /// The storage a projection over `base` names: `base`'s own, or what it
    /// refers to where it is a reference read through.
    fn projected(&mut self, base: &TirExpr) -> u32 {
        if self.type_table.is_reference_shaped(base.type_id) {
            self.eval(base).unwrap_or_else(|| self.g.node())
        } else {
            self.place(base)
        }
    }

    fn is_place(expr: &TirExpr) -> bool {
        match &expr.kind {
            TirExprKind::Local { .. } => true,
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => Self::is_place(inner),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                ..
            } => true,
            _ => false,
        }
    }

    /// Evaluate `arg` for a slot that may borrow it: its value, and its own
    /// storage where `borrows` takes a value-typed place by reference. A
    /// reference argument names the storage in its value already.
    ///
    /// Recorded for the walk is what the callee can write through it: the
    /// value where its type holds a writable handle, and its own storage where
    /// the slot is a `&mut`.
    fn hand(&mut self, arg: &TirExpr, slot: Slot) -> Option<u32> {
        let value = self.eval(arg);
        let own = (slot.borrows
            && Self::is_place(arg)
            && !self.type_table.is_reference_shaped(arg.type_id))
        .then(|| self.place_of(arg));
        let writable_value = value.filter(|_| self.writes.of(arg.type_id));
        let writable_own = own.filter(|_| slot.writes);
        let recorded = self
            .join([writable_value, writable_own])
            .unwrap_or_else(|| self.g.node());
        self.handed
            .insert(std::ptr::from_ref(arg).addr(), (recorded, arg.span));
        self.join([value, own])
    }

    /// [`Self::place`] without re-evaluating the subexpressions `eval` walked.
    fn place_of(&mut self, place: &TirExpr) -> u32 {
        match &place.kind {
            TirExprKind::Local { index, .. } => {
                let loc = self.local(self.frame, *index);
                let node = self.g.node();
                self.g.add(node, loc);
                node
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => {
                if self.type_table.is_reference_shaped(inner.type_id) {
                    self.value_of(inner)
                } else {
                    self.place_of(inner)
                }
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => self.value_of(inner),
            _ => self.g.node(),
        }
    }

    /// The value a place-shaped expression holds, without its side effects.
    fn value_of(&mut self, expr: &TirExpr) -> u32 {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.local(self.frame, *index),
            _ => {
                let place = self.place_of(expr);
                let node = self.g.node();
                self.g.load(place, node);
                node
            }
        }
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
    /// borrow and write.
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
                borrows: tt.is_reference_shaped(p),
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
        let held = self.g.node();
        self.g.load(carried, held);
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
                let writes = decl.mut_params.contains(&i);
                self.hand(
                    arg,
                    Slot {
                        borrows: writes,
                        writes,
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
                let node = self.g.node();
                self.g.load(source, node);
                node
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
        match &target.kind {
            TirExprKind::Local { index, .. } => {
                let node = self.local(self.frame, *index);
                if let Some(v) = value {
                    self.g.copy(v, node);
                }
            }
            _ => {
                let place = self.place(target);
                if let Some(v) = value {
                    self.g.store(place, v);
                }
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &TirPattern, scrutinee: &TirExpr, value: Option<u32>) {
        let mut binds: IndexSet<u32> = IndexSet::default();
        collect_pattern_bindings(pattern, &mut binds);
        if binds.is_empty() {
            return;
        }
        // A binding takes part of the value, or what it refers to, or — lifted
        // to a borrow of the scrutinee — the scrutinee's own storage.
        let referent = if self.type_table.is_reference_shaped(scrutinee.type_id) {
            self.deref(value)
        } else {
            None
        };
        let own = Self::is_place(scrutinee).then(|| self.place_of(scrutinee));
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
                let v = self.eval(value);
                self.bind_pattern(pattern, value, v);
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
            } => Some(self.place(place)),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => {
                let v = self.eval(inner);
                self.deref(v)
            }
            // A part read through a reference is held in what it refers to.
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                let v = self.eval(inner);
                if self.type_table.is_reference_shaped(inner.type_id) {
                    self.deref(v)
                } else {
                    v
                }
            }
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
                let v = self.eval(inner);
                if self.type_table.is_reference_shaped(inner.type_id) {
                    self.deref(v)
                } else {
                    v
                }
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
                            borrows: false,
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
                Some(id) => self.function_value(id, false, Vec::new()),
                None => self.unseen_function,
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
                let v = self.eval(scrut);
                let mut results = Vec::new();
                for arm in arms {
                    self.bind_pattern(&arm.pattern, scrut, v);
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
                                let own = self.g.node();
                                self.g.add(own, node);
                                held.push(own);
                            }
                            held.push(node);
                        }
                        // What the enclosing `$call` captured: its `self` holds it.
                        CaptureSource::Capture(_) => {
                            let this = self.callees[self.frame as usize].params[0];
                            let node = self.g.node();
                            self.g.load(this, node);
                            held.push(node);
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
