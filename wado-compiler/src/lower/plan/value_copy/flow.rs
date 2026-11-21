//! What each function does with the storage its caller hands it: the one
//! resolver every value-copy question about a call reads.
//!
//! A body is solved on a graph of its own (inclusion-based, flow-insensitive),
//! where each parameter names locations standing for its caller's world: the
//! parameter's own storage ([`Iface::Own`]), what its value names
//! ([`Iface::Arg`]), and everything reachable past that ([`Iface::Deep`]).
//! What the solved graph says of those locations, of the result, and of what
//! code out of sight reaches ([`Iface::Escape`]) is the function's [`Summary`]:
//! which interface location may hold which once the call returns. A call
//! instantiates its callee's summary over its own arguments, so a callee's
//! answer is read per call site rather than joined over every caller.
//!
//! Every form of call resolves here once: a direct call to the callee's
//! summary, a builtin to its declaration, a declaration without a body to what
//! its parameters let it do, and a call through a function value to each
//! function that value may run. A closure's `$call` takes the closure as its
//! first parameter like any receiver. A value whose closure type names its
//! `$call` is bound to that `$call`; one the caller supplies may run any
//! function minted at the call's type.
//!
//! Storage a call creates is one location per call site ([`Iface::Fresh`]), so
//! the summaries form a finite lattice and the fixpoint over the call graph
//! ends, recursion included.

use super::analyze::collect_pattern_bindings;
use super::funcset::{FuncIndex, FuncKeySet};
use super::last_use::collect_child_exprs;
use super::points_to::{Args, Graph, Handle, HoldsHandle, Site, Slot, Target, retains_storage};
use crate::compiler_trace;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{
    BuiltinDeclaration, BuiltinDeclarations, CaptureSource, ClosureFunctor, FunctionKind,
    GlobalRole, ResolvedType, RetainInto, ReturnConvention, StructFields, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
};
use crate::type_table::TypeTable;
use std::collections::VecDeque;

/// A location a call's caller can name, as its callee's summary speaks of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Iface {
    /// The storage of the argument at a by-value position.
    Own(u32),
    /// What the argument at a position names: its referent, for a reference.
    Arg(u32),
    /// Everything reachable from [`Iface::Arg`] past that.
    Deep(u32),
    /// What code out of sight may reach: a global, or a callee unseen.
    Escape,
    /// Storage the call created.
    Fresh,
}

/// Where a fact of a [`Summary`] lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Holder {
    /// The call's result names it.
    Result,
    /// The call's result is its storage, handed out uncopied: `return place`
    /// leaves the copy to the caller.
    Returned,
    /// The storage of this interface location holds it.
    At(Iface),
}

/// What a call leaves behind: each holder may hold each source once the call
/// returns. A fact that holds of any call (an argument's storage holds what its
/// value names) is left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Summary {
    holds: IndexSet<(Holder, Iface)>,
}

impl Summary {
    /// A call nothing describes: every argument escapes, and the result is
    /// what escapes.
    fn unseen(arity: usize) -> Self {
        let mut holds: IndexSet<(Holder, Iface)> = IndexSet::default();
        for i in 0..u32::try_from(arity).unwrap() {
            for source in [Iface::Own(i), Iface::Arg(i), Iface::Deep(i)] {
                holds.insert((Holder::At(Iface::Escape), source));
            }
        }
        holds.insert((Holder::Result, Iface::Escape));
        holds.insert((Holder::Returned, Iface::Escape));
        Summary { holds }
    }

    /// Whether every value the call returns is storage it created or took by
    /// value that nothing else reaches, so the caller takes it over without a
    /// copy. A parameter its callers pass uncopied is the caller's to rule out.
    fn returns_owned(&self) -> bool {
        self.holds.iter().all(|&(holder, source)| {
            holder != Holder::Returned || matches!(source, Iface::Fresh | Iface::Own(_))
        })
    }
}

/// Every function's summary, by its position in `flat.functions`.
pub struct Flow {
    summaries: Vec<Summary>,
    /// Each body's locals whose storage a reference outliving the frame may
    /// name: one its caller can reach or one out of sight. One the result
    /// reaches leaves with the result, read in the frame only by what holds it
    /// there, which liveness answers for.
    outlived: Vec<IndexSet<u32>>,
    functions: FuncIndex,
}

impl Flow {
    /// The locals of `func` whose storage a reference outliving its frame may
    /// name, so its storage must not leave with a move.
    #[must_use]
    pub fn outlived(&self, func: &TirFunction) -> &IndexSet<u32> {
        &self.outlived[self.id(&func.module_source, &func.name) as usize]
    }

    /// Whether a call to `module`/`name` hands over storage nothing else
    /// reaches. A function this resolver did not read answers `false`.
    #[must_use]
    pub fn returns_owned(&self, module: &ModuleSource, name: &str) -> bool {
        self.functions
            .id(module, name)
            .is_some_and(|id| self.summaries[id as usize].returns_owned())
    }

    fn id(&self, module: &ModuleSource, name: &str) -> u32 {
        self.functions
            .id(module, name)
            .expect("every function of the package is numbered")
    }
}

/// What the resolver knows of the package while it solves one body.
struct Package<'a> {
    type_table: &'a TypeTable,
    names: HoldsHandle<'a>,
    builtins: &'a BuiltinDeclarations,
    functions: FuncIndex,
    callees: Vec<Callee>,
    /// Each closure functor's `$call`, by functor id.
    closure_calls: Vec<u32>,
    /// Each `$call`, by the type of the closure it takes as `self`.
    calls_by_receiver: IndexMap<TypeId, u32>,
    /// The functions a value of a function type may run, by the type's shape.
    minted: IndexMap<String, IndexSet<u32>>,
    handler_slots: FuncKeySet,
}

struct Callee {
    slots: Vec<Slot>,
    analyzed: bool,
    dispatches: bool,
    is_closure_call: bool,
    /// A `return Some(place)` hands the place out uncopied, as `return place`
    /// does ([`super::hands_out_payload`]).
    hands_out_payload: bool,
}

#[must_use]
pub fn compute(
    flat: &FlatPackage,
    fields: &StructFields,
    functors: &[ClosureFunctor],
    hands_out_payload: &dyn Fn(&TirFunction) -> bool,
) -> Flow {
    let type_table = flat.type_table.borrow();
    let mut functions = FuncIndex::default();
    for (id, func) in (0u32..).zip(&flat.functions) {
        let func = func.borrow();
        functions.insert(func.module_source.clone(), func.name.clone(), id);
    }
    let callees = flat
        .functions
        .iter()
        .map(|func| {
            let func = func.borrow();
            Callee {
                slots: func
                    .params
                    .iter()
                    .map(|p| Slot {
                        by_ref: type_table.is_reference(p.type_id),
                        writes: type_table.is_mut_ref(p.type_id),
                    })
                    .collect(),
                analyzed: func.body.is_some()
                    && !matches!(func.kind, FunctionKind::FnCanonicalDispatch { .. }),
                dispatches: matches!(func.kind, FunctionKind::FnCanonicalDispatch { .. }),
                is_closure_call: func.is_closure_call(),
                hands_out_payload: hands_out_payload(&func),
            }
        })
        .collect();
    let id_of = |func: &TirFunction| {
        functions
            .id(&func.module_source, &func.name)
            .expect("every function of the package is numbered")
    };
    let closure_calls = functors
        .iter()
        .map(|functor| id_of(&functor.call_method.borrow()))
        .collect();
    let calls_by_receiver = flat
        .functions
        .iter()
        .filter_map(|func| {
            let func = func.borrow();
            let receiver = func.params.first().filter(|_| func.is_closure_call())?;
            Some((receiver.type_id, id_of(&func)))
        })
        .collect();
    let mut package = Package {
        type_table: &type_table,
        names: HoldsHandle::new(&type_table, fields, Handle::Names),
        builtins: &flat.builtin_declarations,
        functions,
        callees,
        closure_calls,
        calls_by_receiver,
        minted: IndexMap::default(),
        handler_slots: FuncKeySet::default(),
    };
    for global in &flat.globals {
        if global.role == GlobalRole::HandlerSlot {
            package
                .handler_slots
                .insert(global.module_source.clone(), global.name.clone());
        }
    }
    package.minted = minted_by_shape(flat, &package);
    let n = flat.functions.len();
    let mut summaries = vec![Summary::default(); n];
    let mut outlived: Vec<IndexSet<u32>> = vec![IndexSet::default(); n];
    let mut readers: Vec<IndexSet<u32>> = vec![IndexSet::default(); n];
    let mut queue: VecDeque<u32> = (0..u32::try_from(n).unwrap())
        .filter(|&id| package.callees[id as usize].analyzed)
        .collect();
    let mut queued: Vec<bool> = (0..n).map(|i| package.callees[i].analyzed).collect();
    while let Some(id) = queue.pop_front() {
        queued[id as usize] = false;
        let func = flat.functions[id as usize].borrow();
        let solved = summarize(&package, &summaries, &func);
        for callee in solved.read {
            readers[callee as usize].insert(id);
        }
        outlived[id as usize] = solved.outlived;
        let summary = solved.summary;
        if summary != summaries[id as usize] {
            summaries[id as usize] = summary;
            for &reader in &readers[id as usize] {
                if !queued[reader as usize] {
                    queued[reader as usize] = true;
                    queue.push_back(reader);
                }
            }
        }
    }
    for (func, (summary, outlived)) in flat.functions.iter().zip(summaries.iter().zip(&outlived)) {
        compiler_trace!(
            "flow",
            "{} holds={:?} outlived={:?}",
            func.borrow().name,
            summary.holds,
            outlived
        );
    }
    Flow {
        summaries,
        outlived,
        functions: package.functions,
    }
}

/// What solving one body found.
struct Solved {
    summary: Summary,
    outlived: IndexSet<u32>,
    /// The functions whose summaries it read.
    read: IndexSet<u32>,
}

/// The structural key of a function type: two values of one shape may run the
/// same functions, whichever id the table interned each under.
fn shape(type_id: TypeId, type_table: &TypeTable) -> String {
    type_table.mangle_type_arg_for_generic(type_table.peel_refs(type_id))
}

/// The functions a value of each function shape may run: every closure and
/// every function taken by name, at the type the expression minting it has.
fn minted_by_shape(flat: &FlatPackage, package: &Package<'_>) -> IndexMap<String, IndexSet<u32>> {
    struct Mints<'p, 'a> {
        package: &'p Package<'a>,
        out: IndexMap<String, IndexSet<u32>>,
    }
    impl crate::tir_visitor::TirRefVisitor for Mints<'_, '_> {
        fn visit_expr(&mut self, expr: &TirExpr) {
            let minted = match &expr.kind {
                TirExprKind::Closure {
                    functor_id: Some(functor),
                    ..
                } => Some(self.package.closure_calls[*functor as usize]),
                TirExprKind::FuncRef {
                    module_source,
                    name,
                    ..
                } => self.package.functions.id(module_source, name),
                _ => None,
            };
            if let Some(id) = minted {
                self.out
                    .entry(shape(expr.type_id, self.package.type_table))
                    .or_default()
                    .insert(id);
            }
            self.walk_expr(expr);
        }
    }
    let mut mints = Mints {
        package,
        out: IndexMap::default(),
    };
    for func in &flat.functions {
        if let Some(body) = &func.borrow().body {
            crate::tir_visitor::TirRefVisitor::visit_block(&mut mints, body);
        }
    }
    for global in &flat.globals {
        crate::tir_visitor::TirRefVisitor::visit_expr(&mut mints, global.init.slot_expr());
    }
    mints.out
}

/// An expression's identity while the body holding it is solved.
fn key(expr: &TirExpr) -> usize {
    std::ptr::from_ref(expr).addr()
}

/// The parameter locations of the body being solved.
struct ParamLocs {
    own: u32,
    arg: u32,
    deep: u32,
    by_value: bool,
}

/// Solve `func`'s body and read off its summary.
fn summarize(package: &Package<'_>, summaries: &[Summary], func: &TirFunction) -> Solved {
    let body = func.body.as_ref().expect("only a body is summarized");
    let mut b = Body::new(package, summaries, func);
    let params: Vec<ParamLocs> = func
        .params
        .iter()
        .map(|p| {
            let own = b.local(p.local_index);
            let arg = b.g.node();
            let deep = b.g.node();
            b.g.add(own, arg);
            b.g.add(arg, deep);
            b.g.add(deep, deep);
            b.g.external.extend([arg, deep]);
            ParamLocs {
                own,
                arg,
                deep,
                by_value: !package.type_table.is_reference(p.type_id),
            }
        })
        .collect();
    for (p, locs) in func.params.iter().zip(&params) {
        if let Some(&call) = package.calls_by_receiver.get(&p.type_id) {
            b.bind_closure_value(locs.arg, call);
        }
    }
    if let Some(value) = b.block_value(body) {
        b.g.copy(value, b.result);
    }
    if let Some(tail) = body.tail_expr() {
        b.hand_back(tail);
    }
    b.g.solve();
    let mut iface: IndexMap<u32, Iface> = IndexMap::default();
    for (i, locs) in (0u32..).zip(&params) {
        if locs.by_value {
            iface.insert(locs.own, Iface::Own(i));
        }
        iface.insert(locs.arg, Iface::Arg(i));
        iface.insert(locs.deep, Iface::Deep(i));
    }
    let escaped = b.g.solved(b.escape);
    for &loc in &escaped {
        iface.entry(loc).or_insert(Iface::Escape);
    }
    let of = |loc: u32| iface.get(&loc).copied().unwrap_or(Iface::Fresh);
    let mut holds: IndexSet<(Holder, Iface)> = IndexSet::default();
    for loc in b.g.solved(b.result) {
        holds.insert((Holder::Result, of(loc)));
    }
    for &loc in &escaped {
        let source = of(loc);
        if source != Iface::Escape {
            holds.insert((Holder::At(Iface::Escape), source));
        }
    }
    // What each interface location, and each location the call created that
    // one of them reaches, holds once the body has run.
    // A reference parameter's own cell is the callee's, which no caller names.
    let mut work: Vec<u32> = params
        .iter()
        .flat_map(|l| [l.by_value.then_some(l.own), Some(l.arg), Some(l.deep)])
        .flatten()
        .chain(b.g.solved(b.result))
        .collect();
    let mut seen: IndexSet<u32> = IndexSet::default();
    while let Some(loc) = work.pop() {
        if !seen.insert(loc) || of(loc) == Iface::Escape {
            continue;
        }
        let holder = of(loc);
        for held in b.g.solved(loc) {
            let source = of(held);
            let trivial = match (holder, source) {
                (Iface::Own(i), Iface::Arg(j)) | (Iface::Arg(i), Iface::Deep(j)) => i == j,
                (Iface::Deep(i), Iface::Deep(j)) => i == j,
                _ => false,
            };
            if !trivial {
                holds.insert((Holder::At(holder), source));
            }
            if source == Iface::Fresh {
                work.push(held);
            }
        }
    }
    let owner: IndexMap<u32, u32> = b.locals.iter().map(|(&local, &loc)| (loc, local)).collect();
    let mut outlived: IndexSet<u32> = IndexSet::default();
    let mut work: Vec<u32> = escaped.clone();
    for l in &params {
        work.extend([l.arg, l.deep]);
        if l.by_value {
            work.extend(b.g.solved(l.own));
        }
    }
    let mut seen: IndexSet<u32> = IndexSet::default();
    while let Some(loc) = work.pop() {
        if !seen.insert(loc) {
            continue;
        }
        if let Some(&local) = owner.get(&loc) {
            outlived.insert(local);
        }
        work.extend(b.g.solved(loc));
    }
    // Storage a reference outliving the frame names is no caller's to take
    // over, wherever it was created.
    for loc in b.g.solved(b.returned) {
        let source = match of(loc) {
            Iface::Fresh | Iface::Own(_) if seen.contains(&loc) => Iface::Escape,
            source => source,
        };
        holds.insert((Holder::Returned, source));
    }
    holds.sort_unstable();
    Solved {
        summary: Summary { holds },
        outlived,
        read: b.read,
    }
}

/// One body's graph under construction.
struct Body<'p, 'a> {
    package: &'p Package<'a>,
    summaries: &'p [Summary],
    g: Graph,
    locals: IndexMap<u32, u32>,
    local_types: IndexMap<u32, TypeId>,
    /// Where a `return` sends its value.
    result: u32,
    /// The storage a `return` hands out uncopied.
    returned: u32,
    hands_out_payload: bool,
    /// What escapes to code out of sight: its set is every location that may.
    escape: u32,
    /// What a handler slot holds: code out of sight may run it while it is
    /// installed, but nothing keeps it past the installing frame.
    installed: u32,
    /// Each enclosing labeled block's value, and the storage it hands out.
    labels: Vec<(String, u32, u32)>,
    /// The value each expression evaluated to, by its address.
    values: IndexMap<usize, Option<u32>>,
    /// The storage each call's result is, by the call's address.
    call_storage: IndexMap<usize, u32>,
    /// The storage each labeled block's `break`s hand out, by its address.
    label_storage: IndexMap<usize, u32>,
    /// The parameter `self` of a `$call`, which a nested closure's captures of
    /// captures read.
    this: Option<u32>,
    /// The functions whose summaries the body read.
    read: IndexSet<u32>,
    /// The function location running each function, for a call through it.
    function_locations: IndexMap<u32, u32>,
    /// The function location running every function of a shape.
    shape_locations: IndexMap<String, u32>,
}

impl<'p, 'a> Body<'p, 'a> {
    fn new(package: &'p Package<'a>, summaries: &'p [Summary], func: &TirFunction) -> Self {
        let mut g = Graph::default();
        let result = g.node();
        let returned = g.node();
        let escape = g.node();
        let installed = g.node();
        let id = package
            .functions
            .id(&func.module_source, &func.name)
            .expect("every function of the package is numbered");
        let local_types = func
            .params
            .iter()
            .map(|p| (p.local_index, p.type_id))
            .chain((0u32..).zip(func.locals.iter().map(|l| l.type_id)))
            .collect();
        let mut b = Body {
            package,
            summaries,
            g,
            locals: IndexMap::default(),
            local_types,
            result,
            returned,
            hands_out_payload: package.callees[id as usize].hands_out_payload,
            escape,
            installed,
            labels: Vec::new(),
            values: IndexMap::default(),
            call_storage: IndexMap::default(),
            label_storage: IndexMap::default(),
            this: None,
            read: IndexSet::default(),
            function_locations: IndexMap::default(),
            shape_locations: IndexMap::default(),
        };
        // Whatever escapes may reach, and be stored in, whatever else escapes,
        // and may be called with it.
        for world in [escape, installed] {
            let reached = b.held_by(world);
            b.g.copy(reached, world);
            b.g.store(world, escape);
            b.g.call_site(
                world,
                Site {
                    args: Args::Every(escape),
                    result: escape,
                    fallback: None,
                },
            );
        }
        // A caller's handler is installed when this body runs.
        b.g.copy(escape, installed);
        if func.is_closure_call() {
            let first = func.params.first().expect("a `$call` takes `self`");
            b.this = Some(b.local(first.local_index));
        }
        b
    }

    fn local(&mut self, local: u32) -> u32 {
        if let Some(&node) = self.locals.get(&local) {
            return node;
        }
        let node = self.g.node();
        self.locals.insert(local, node);
        if let Some(&ty) = self.local_types.get(&local)
            && !self.package.names.of(ty)
        {
            self.g.inert.insert(node);
        }
        node
    }

    /// The node standing for a global, both its storage and its value.
    fn global(&self, module: &ModuleSource, name: &str) -> u32 {
        if self.package.handler_slots.contains(module, name) {
            self.installed
        } else {
            self.escape
        }
    }

    fn naming(&mut self, loc: u32) -> u32 {
        let node = self.g.node();
        self.g.add(node, loc);
        node
    }

    fn held_by(&mut self, reference: u32) -> u32 {
        let node = self.g.node();
        self.g.load(reference, node);
        node
    }

    fn deref(&mut self, reference: Option<u32>) -> Option<u32> {
        reference.map(|r| self.held_by(r))
    }

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

    /// Instantiate `callee`'s summary over `args`, the value each position
    /// hands it and its own storage where the position takes a place, into
    /// `result`. The storage the call creates is one location of this site.
    fn apply(
        &mut self,
        callee: u32,
        args: &[(Option<u32>, Option<u32>)],
        result: u32,
        storage: Option<u32>,
    ) {
        self.read.insert(callee);
        let summaries = self.summaries;
        self.apply_summary(&summaries[callee as usize], args, result, storage);
    }

    /// [`Self::apply`] with `summary`; the result's own storage lands in
    /// `storage`.
    fn apply_summary(
        &mut self,
        summary: &Summary,
        args: &[(Option<u32>, Option<u32>)],
        result: u32,
        storage: Option<u32>,
    ) {
        if summary.holds.is_empty() {
            return;
        }
        let fresh = self.g.node();
        let mut deep: IndexMap<u32, u32> = IndexMap::default();
        let mut node_of = |b: &mut Self, iface: Iface| -> Option<u32> {
            match iface {
                Iface::Own(i) => args.get(i as usize).and_then(|a| a.1),
                Iface::Arg(i) => args.get(i as usize).and_then(|a| a.0),
                Iface::Deep(i) => {
                    let arg = args.get(i as usize).and_then(|a| a.0)?;
                    Some(*deep.entry(i).or_insert_with(|| {
                        let node = b.held_by(arg);
                        b.g.load(node, node);
                        node
                    }))
                }
                Iface::Escape => Some(b.escape),
                Iface::Fresh => Some(b.naming(fresh)),
            }
        };
        for &(holder, source) in &summary.holds {
            let Some(source) = node_of(self, source) else {
                continue;
            };
            match holder {
                Holder::Result => self.g.copy(source, result),
                Holder::Returned => {
                    if let Some(storage) = storage {
                        self.g.copy(source, storage);
                    }
                }
                Holder::At(Iface::Escape) => self.g.copy(source, self.escape),
                Holder::At(at) => {
                    if let Some(into) = node_of(self, at) {
                        self.g.store(into, source);
                    }
                }
            }
        }
    }

    /// A function location running `callee`, whose call instantiates its
    /// summary: `self` first for a closure's `$call`.
    fn function_location(&mut self, callee: u32) -> u32 {
        if let Some(&loc) = self.function_locations.get(&callee) {
            return loc;
        }
        let loc = self.g.node();
        self.function_locations.insert(callee, loc);
        let (params, ret) = self.target_nodes(self.package.callees[callee as usize].slots.len());
        self.instantiate_target(callee, &params, ret);
        self.g.targets.insert(
            loc,
            Target {
                this: None,
                params,
                sink: None,
                ret,
            },
        );
        loc
    }

    fn target_nodes(&mut self, arity: usize) -> (Vec<u32>, u32) {
        let params: Vec<u32> = (0..arity).map(|_| self.g.node()).collect();
        let ret = self.g.node();
        (params, ret)
    }

    /// Instantiate `callee` over the parameter nodes of a function location.
    fn instantiate_target(&mut self, callee: u32, params: &[u32], ret: u32) {
        let args: Vec<(Option<u32>, Option<u32>)> =
            params.iter().map(|&p| (Some(p), None)).collect();
        if self.package.callees[callee as usize].analyzed {
            self.apply(callee, &args, ret, None);
        } else {
            self.apply_summary(&Summary::unseen(params.len()), &args, ret, None);
        }
    }

    /// Bind calls through the value in location `value` to the `$call`
    /// `call`, which takes the closure as `self`.
    fn bind_closure_value(&mut self, value: u32, call: u32) {
        let arity = self.package.callees[call as usize].slots.len();
        let (params, ret) = self.target_nodes(arity);
        self.instantiate_target(call, &params, ret);
        let this = self.naming(value);
        self.g.targets.insert(
            value,
            Target {
                this: Some(this),
                params,
                sink: None,
                ret,
            },
        );
    }

    /// The function location running every function a value of `callee`'s
    /// shape may run.
    fn shape_location(&mut self, callee_type: TypeId) -> Option<u32> {
        let key = shape(callee_type, self.package.type_table);
        if let Some(&loc) = self.shape_locations.get(&key) {
            return Some(loc);
        }
        let ResolvedType::Function { params, .. } = self
            .package
            .type_table
            .get(self.package.type_table.peel_refs(callee_type))
        else {
            return None;
        };
        let arity = params.len();
        let loc = self.g.node();
        self.shape_locations.insert(key.clone(), loc);
        let this = self.g.node();
        let (params, ret) = self.target_nodes(arity);
        let minted = self.package.minted.get(&key).cloned().unwrap_or_default();
        for callee in minted {
            if self.package.callees[callee as usize].is_closure_call {
                let with_self: Vec<u32> = std::iter::once(this)
                    .chain(params.iter().copied())
                    .collect();
                self.instantiate_target(callee, &with_self, ret);
            } else {
                self.instantiate_target(callee, &params, ret);
            }
        }
        let mut all = vec![this];
        all.extend(params);
        self.g.targets.insert(
            loc,
            Target {
                this: Some(this),
                params: all,
                sink: None,
                ret,
            },
        );
        Some(loc)
    }

    fn place(&mut self, expr: &TirExpr) -> Option<u32> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                let loc = self.local(*index);
                Some(self.naming(loc))
            }
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => Some(self.global(module_source, name)),
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

    fn projected(&mut self, base: &TirExpr) -> Option<u32> {
        if self.package.type_table.is_reference(base.type_id) {
            Some(self.referent(base))
        } else {
            self.place(base)
        }
    }

    fn referent(&mut self, reference: &TirExpr) -> u32 {
        self.eval(reference).unwrap_or_else(|| self.g.node())
    }

    fn referenced(&mut self, expr: &TirExpr) -> u32 {
        self.place(expr).unwrap_or_else(|| {
            let temp = self.g.node();
            if let Some(value) = self.eval(expr) {
                self.g.copy(value, temp);
            }
            self.naming(temp)
        })
    }

    fn operand(&mut self, expr: &TirExpr) -> (Option<u32>, Option<u32>) {
        let Some(place) = self.place(expr) else {
            return (self.eval(expr), None);
        };
        let value = if matches!(expr.kind, TirExprKind::Local { .. }) {
            self.eval(expr)
        } else {
            self.package
                .names
                .of(expr.type_id)
                .then(|| self.held_by(place))
        };
        (value, Some(place))
    }

    /// What `arg` hands a position taking it as `slot`: the value, and its own
    /// storage where the argument is a place. A by-reference position over a
    /// value-typed place takes the place itself as what it names.
    fn hand(&mut self, arg: &TirExpr, slot: Slot) -> (Option<u32>, Option<u32>) {
        if slot.by_ref && !self.package.type_table.is_reference(arg.type_id) {
            let (value, own) = self.operand(arg);
            return (own.or(value), own);
        }
        if matches!(arg.kind, TirExprKind::Local { .. }) {
            return self.operand(arg);
        }
        (self.eval(arg), None)
    }

    fn call(&mut self, callee: u32, args: &[&TirExpr], storage: u32) -> Option<u32> {
        let info = &self.package.callees[callee as usize];
        let slots = info.slots.clone();
        let handed: Vec<(Option<u32>, Option<u32>)> = args
            .iter()
            .enumerate()
            .map(|(i, arg)| self.hand(arg, slots.get(i).copied().unwrap_or(Slot::UNKNOWN)))
            .collect();
        let result = self.g.node();
        let info = &self.package.callees[callee as usize];
        if info.dispatches {
            self.apply_summary(
                &Summary::unseen(handed.len()),
                &handed,
                result,
                Some(storage),
            );
        } else if !info.analyzed {
            self.local_merge(&slots, &handed, result);
            self.g.copy(self.escape, storage);
        } else {
            self.apply(callee, &handed, result, Some(storage));
        }
        Some(result)
    }

    /// A call out of sight: everything handed escapes, the result is what
    /// escapes.
    fn opaque_call(&mut self, args: &[&TirExpr], storage: u32) -> Option<u32> {
        let handed: Vec<(Option<u32>, Option<u32>)> =
            args.iter().map(|a| self.hand(a, Slot::UNKNOWN)).collect();
        let result = self.g.node();
        self.apply_summary(
            &Summary::unseen(handed.len()),
            &handed,
            result,
            Some(storage),
        );
        Some(result)
    }

    /// A declaration without a body: what any argument carries or names, or
    /// what that holds, may be stored through a `&mut` position and returned.
    fn local_merge(&mut self, slots: &[Slot], handed: &[(Option<u32>, Option<u32>)], result: u32) {
        let written = self.g.node();
        for (i, &(value, _)) in handed.iter().enumerate() {
            let Some(value) = value else { continue };
            self.g.copy(value, result);
            if slots.get(i).is_some_and(|s| s.writes) {
                self.g.copy(value, written);
            }
        }
        let held = self.held_by(result);
        self.g.copy(held, result);
        self.g.store(written, result);
    }

    fn builtin_call(
        &mut self,
        decl: &BuiltinDeclaration,
        args: &[&TirExpr],
        storage: u32,
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
                .0
            })
            .collect();
        let result = self.g.node();
        if let Some(ReturnConvention::PartOf(params)) = &decl.returns {
            for &p in params {
                let through_reference = self.package.type_table.is_reference(args[p].type_id)
                    || decl.reference_params.contains(&p);
                // A value handed back is the argument's own storage, or part of
                // what a reference names.
                if !decl.returns_reference {
                    let part = if through_reference {
                        handed[p]
                    } else {
                        self.storage_of(args[p])
                    };
                    if let Some(part) = part {
                        self.g.copy(part, storage);
                    }
                }
                let Some(h) = handed[p] else { continue };
                if self.package.type_table.is_reference(args[p].type_id) && !decl.returns_reference {
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
            if !retains_storage(spec, args[spec.source].type_id, self.package.type_table) {
                continue;
            }
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

    fn indirect_call(&mut self, callee: &TirExpr, args: &[TirExpr], storage: u32) -> Option<u32> {
        let tt = self.package.type_table;
        let declared = match &callee.kind {
            TirExprKind::Local { index, .. } => self.local_types.get(index).copied(),
            _ => None,
        }
        .unwrap_or(callee.type_id);
        let value = self.eval(callee).unwrap_or_else(|| self.g.node());
        if let Some(&call) = self.package.calls_by_receiver.get(&declared) {
            let slots = self.package.callees[call as usize].slots.clone();
            let mut handed = vec![(Some(value), None)];
            handed.extend(
                args.iter()
                    .zip(slots.iter().skip(1))
                    .map(|(arg, &slot)| self.hand(arg, slot)),
            );
            let result = self.g.node();
            self.apply(call, &handed, result, Some(storage));
            return Some(result);
        }
        let ResolvedType::Function { params, .. } = tt.get(tt.peel_refs(callee.type_id)) else {
            unreachable!("an indirect call's callee is a function value");
        };
        let slots: Vec<Slot> = params
            .iter()
            .map(|&p| Slot {
                by_ref: tt.is_reference(p),
                writes: tt.is_mut_ref(p),
            })
            .collect();
        let handed: Vec<Option<u32>> = args
            .iter()
            .zip(slots)
            .map(|(arg, slot)| self.hand(arg, slot).0)
            .collect();
        self.shape_storage(callee.type_id, value, &handed, storage);
        let fallback = self.shape_location(callee.type_id);
        let result = self.g.node();
        self.g.call_site(
            value,
            Site {
                args: Args::Each(handed),
                result,
                fallback,
            },
        );
        Some(result)
    }

    /// The storage a call through a value of `callee_type` hands back: what
    /// any function of that shape hands back, over these arguments.
    fn shape_storage(
        &mut self,
        callee_type: TypeId,
        receiver: u32,
        handed: &[Option<u32>],
        storage: u32,
    ) {
        let key = shape(callee_type, self.package.type_table);
        let minted = self.package.minted.get(&key).cloned().unwrap_or_default();
        for callee in minted {
            let info = &self.package.callees[callee as usize];
            if !info.analyzed {
                self.g.copy(self.escape, storage);
                continue;
            }
            let receiver = info.is_closure_call.then_some(receiver);
            let args: Vec<(Option<u32>, Option<u32>)> = receiver
                .into_iter()
                .chain(handed.iter().flatten().copied())
                .map(|node| (Some(node), None))
                .collect();
            self.read.insert(callee);
            let returned = Summary {
                holds: self.summaries[callee as usize]
                    .holds
                    .iter()
                    .copied()
                    .filter(|&(holder, _)| holder == Holder::Returned)
                    .collect(),
            };
            let result = self.g.node();
            self.apply_summary(&returned, &args, result, Some(storage));
        }
    }

    /// The storage `expr`'s value is, where that is storage some other
    /// expression may still reach: a place, or what a call hands back
    /// uncopied. `None` for a value made fresh. Reads what evaluating `expr`
    /// recorded, so `expr` must have been evaluated.
    fn storage_of(&mut self, expr: &TirExpr) -> Option<u32> {
        let tt = self.package.type_table;
        let value_of = |b: &Self, e: &TirExpr| b.values.get(&key(e)).copied().flatten();
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                let loc = self.local(*index);
                Some(self.naming(loc))
            }
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => Some(self.global(module_source, name)),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => value_of(self, inner),
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => {
                if tt.is_reference(inner.type_id) {
                    value_of(self, inner)
                } else {
                    self.storage_of(inner)
                }
            }
            TirExprKind::Call { .. } | TirExprKind::IndirectCall { .. } => {
                self.call_storage.get(&key(expr)).copied()
            }
            TirExprKind::Block(block) => block.tail_expr().and_then(|t| self.storage_of(t)),
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                let t = then_branch.tail_expr().and_then(|t| self.storage_of(t));
                let e = else_branch
                    .as_ref()
                    .and_then(|b| b.tail_expr())
                    .and_then(|t| self.storage_of(t));
                self.join([t, e])
            }
            TirExprKind::Match { arms, .. } => {
                let arms: Vec<Option<u32>> =
                    arms.iter().map(|arm| self.storage_of(&arm.body)).collect();
                self.join(arms)
            }
            TirExprKind::LabeledBlock { block, .. } => {
                let tail = block.tail_expr().and_then(|t| self.storage_of(t));
                let breaks = self.label_storage.get(&key(expr)).copied();
                self.join([tail, breaks])
            }
            TirExprKind::VariantConstruct {
                payload: Some(payload),
                ..
            } if self.hands_out_payload => self.storage_of(payload),
            _ => None,
        }
    }

    /// `return value` hands `value`'s storage out uncopied.
    fn hand_back(&mut self, value: &TirExpr) {
        if let Some(storage) = self.storage_of(value) {
            self.g.copy(storage, self.returned);
        }
    }

    fn assign(&mut self, target: &TirExpr, value: Option<u32>) {
        if let TirExprKind::Local { index, .. } = &target.kind {
            let node = self.local(*index);
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
        let referent = if self.package.type_table.is_reference(scrutinee.type_id) {
            self.deref(value)
        } else {
            None
        };
        for local in binds {
            let node = self.local(local);
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
                let v = value.as_ref().and_then(|value| self.eval(value));
                let node = self.local(*local_index);
                if let Some(v) = v {
                    self.g.copy(v, node);
                }
                None
            }
            TirStmtKind::Expr(e) => self.eval(e),
            TirStmtKind::Return { value: Some(value) } | TirStmtKind::TaskReturn { value } => {
                if let Some(v) = self.eval(value) {
                    self.g.copy(v, self.result);
                }
                self.hand_back(value);
                None
            }
            TirStmtKind::Return { value: None } => None,
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
                let Some(label) = label else { return None };
                let Some(&(_, node, storage)) =
                    self.labels.iter().rev().find(|(l, _, _)| l == label)
                else {
                    return None;
                };
                if let Some(v) = v {
                    self.g.copy(v, node);
                }
                if let Some(held) = value.as_ref().and_then(|v| self.storage_of(v)) {
                    self.g.copy(held, storage);
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

    fn eval(&mut self, expr: &TirExpr) -> Option<u32> {
        let value = self
            .eval_kind(expr)
            .filter(|_| self.package.names.of(expr.type_id));
        self.values.insert(key(expr), value);
        value
    }

    fn part_of(&mut self, whole: &TirExpr) -> Option<u32> {
        let v = self.eval(whole);
        if self.package.type_table.is_reference(whole.type_id) {
            self.deref(v)
        } else {
            v
        }
    }

    fn eval_kind(&mut self, expr: &TirExpr) -> Option<u32> {
        let tt = self.package.type_table;
        match &expr.kind {
            TirExprKind::Local { index, .. } => Some(self.local(*index)),
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => Some(self.global(module_source, name)),
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => {
                if let Some(v) = self.eval(value) {
                    let global = self.global(module_source, name);
                    self.g.copy(v, global);
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
                if tt.is_reference(inner.type_id) && !tt.is_reference(expr.type_id) {
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
                let storage = self.g.node();
                self.call_storage.insert(key(expr), storage);
                let args: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                if let Some(decl) = self.package.builtins.get(&**func) {
                    return self.builtin_call(decl, &args, storage);
                }
                match self.package.functions.id(&func.module_source, &func.name) {
                    Some(id) => self.call(id, &args, storage),
                    None => self.opaque_call(&args, storage),
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                let storage = self.g.node();
                self.call_storage.insert(key(expr), storage);
                self.indirect_call(callee, args, storage)
            }
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => {
                let loc = match self.package.functions.id(module_source, name) {
                    Some(id) => self.function_location(id),
                    None => {
                        let loc = self.g.node();
                        self.g.targets.insert(
                            loc,
                            Target {
                                this: None,
                                params: Vec::new(),
                                sink: Some(self.escape),
                                ret: self.escape,
                            },
                        );
                        loc
                    }
                };
                Some(self.naming(loc))
            }
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
                let storage = self.g.node();
                self.label_storage.insert(key(expr), storage);
                self.labels.push((label.clone(), node, storage));
                let tail = self.block_value(block);
                self.labels.pop();
                if let Some(t) = tail {
                    self.g.copy(t, node);
                }
                Some(node)
            }
            TirExprKind::WithHandler { .. } => {
                unreachable!("effect dispatch synthesis desugars every `with`")
            }
            TirExprKind::Closure {
                captures,
                functor_id,
                ..
            } => {
                let functor = functor_id.expect("closure planning numbers every closure");
                let call = self.package.closure_calls[functor as usize];
                let object = self.g.node();
                for capture in captures {
                    match capture.source {
                        CaptureSource::Local(index) => {
                            let node = self.local(index);
                            if tt.is_reference(capture.type_id) {
                                let named = self.naming(node);
                                self.g.copy(named, object);
                            }
                            self.g.copy(node, object);
                        }
                        CaptureSource::Capture(_) => {
                            let this = self.this.expect("a capture of a capture is in a `$call`");
                            let held = self.held_by(this);
                            self.g.copy(held, object);
                        }
                    }
                }
                self.bind_closure_value(object, call);
                Some(self.naming(object))
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
