//! Interprocedural reference-retention analysis: which reference-parameter
//! *positions* a function may persist beyond its call. A least fixpoint over
//! the call graph, sound as long as `carries` over-approximates.
//!
//! Three channels, apart by where the reference lands:
//! [`RetentionFacts::escapes`] reaches somewhere the caller cannot see,
//! [`RetentionFacts::into_result`] reaches the return value, and
//! [`RetentionFacts::into_param`] reaches a parameter the caller named, which the
//! caller resolves against its own argument there. Why the first two cannot be
//! one set is in WEP 2026-05-21; [`compute_retention`] publishes the union,
//! which is what a reader with no argument list must assume.
//!
//! An indirect call reads [`FunctorRows`], which answers per call site where it
//! can name the function values that reach one, and per functor type — the join
//! over every expression minting a value of that type — where it cannot.

use super::callgraph::CallGraph;
use super::funcset::FuncKeyMap;
use super::is_reference_type;
use crate::compiler_trace;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::analyze;
use crate::tir::{
    BuiltinDeclarations, FunctionRef, ResolvedType, RetainSpec, TirBlock, TirExpr, TirExprKind,
    TirFunction, TirPattern, TirStmt, TirStmtKind, TirStruct, TirUnaryOp, TypeId, TypeTable,
    capture_source_locals,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;
use std::cell::RefCell;

/// Per-function set of reference-parameter positions the function may retain.
pub type RetainedParams = FuncKeyMap<IndexSet<u32>>;

/// The destination of a position the callee hands back with its result. The
/// caller resolves it against the local it binds the result to.
pub const RESULT: u32 = u32::MAX;

/// The three ways a reference parameter outlives its call. See the module doc.
#[derive(Clone, Default)]
struct RetentionFacts {
    escapes: IndexSet<u32>,
    into_result: IndexSet<u32>,
    /// Retained parameter to the parameters it lands in. A destination the
    /// caller can name bounds the retention: the caller resolves it against its
    /// own argument there, and only what lands somewhere it cannot see escapes.
    into_param: IndexMap<u32, IndexSet<u32>>,
    /// Positions whose claim is on what the referent's elements hold rather
    /// than on the reference itself (`elements_of = p`). An array of plain data
    /// hands on nothing, so the claim is gated on the argument's type at each
    /// call. Only a declaration states one; a body walk never does.
    elements: IndexSet<u32>,
}

impl RetentionFacts {
    /// Whether any channel claims `source`.
    fn claims(&self, source: u32) -> bool {
        self.escapes.contains(&source)
            || self.into_result.contains(&source)
            || self.into_param.contains_key(&source)
    }

    /// Absorb `other`, reporting whether anything grew.
    fn absorb(&mut self, other: &RetentionFacts) -> bool {
        let claimed_before: Vec<u32> = self
            .escapes
            .iter()
            .chain(self.into_result.iter())
            .chain(self.into_param.keys())
            .copied()
            .collect();
        let mut grew = extend(&mut self.escapes, &other.escapes)
            | extend(&mut self.into_result, &other.into_result);
        for (&source, destinations) in &other.into_param {
            grew |= extend(self.into_param.entry(source).or_default(), destinations);
        }
        // An element claim is the narrower one, so a position keeps it only
        // while every side claiming that position claims its elements.
        for &source in &other.elements {
            if !claimed_before.contains(&source) {
                grew |= self.elements.insert(source);
            }
        }
        for source in self.elements.iter().copied().collect::<Vec<_>>() {
            if other.claims(source) && !other.elements.contains(&source) {
                self.elements.swap_remove(&source);
                grew = true;
            }
        }
        grew
    }

    /// Where each position the caller can resolve lands: a parameter, or
    /// [`RESULT`]. A position the callee also keeps out of sight is left out:
    /// naming one of its destinations would say the reference goes no further
    /// than there, and it does.
    fn bounded(&self) -> IndexMap<u32, IndexSet<u32>> {
        self.union()
            .into_iter()
            .filter(|source| !self.escapes.contains(source))
            .map(|source| {
                let mut destinations = self.into_param.get(&source).cloned().unwrap_or_default();
                if self.into_result.contains(&source) {
                    destinations.insert(RESULT);
                }
                (source, destinations)
            })
            .collect()
    }

    /// Every position that outlives the call somehow — what a reader with no
    /// argument list to resolve a destination against must assume.
    fn union(&self) -> IndexSet<u32> {
        let mut out = self.escapes.clone();
        for &p in &self.into_result {
            out.insert(p);
        }
        for &p in self.into_param.keys() {
            out.insert(p);
        }
        out
    }
}

/// What a value hands on, split by how it does. `is` are the positions the
/// value is itself a reference into; `holds` are the positions something inside
/// it is a reference into. A value can do both — a `&Sink` whose `Sink` keeps a
/// reference to the same parameter — so the two are not disjoint, and what the
/// value carries altogether is their union.
///
/// The split is what an element claim reads: `elements_of = p` asks what the
/// referent's elements hold, which is `holds` and never `is`.
#[derive(Clone, Default)]
struct Carried {
    is: IndexSet<u32>,
    holds: IndexSet<u32>,
}

impl Carried {
    fn held(positions: IndexSet<u32>) -> Self {
        Carried {
            is: IndexSet::default(),
            holds: positions,
        }
    }

    fn all(&self) -> IndexSet<u32> {
        let mut out = self.is.clone();
        extend(&mut out, &self.holds);
        out
    }

    fn is_empty(&self) -> bool {
        self.is.is_empty() && self.holds.is_empty()
    }

    fn absorb(&mut self, other: &Carried) -> bool {
        extend(&mut self.is, &other.is) | extend(&mut self.holds, &other.holds)
    }
}

/// The place an argument names, past the `&`/`&mut` it is wrapped in.
fn unwrap_borrow(argument: &TirExpr) -> &TirExpr {
    match &argument.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr,
        } => expr,
        _ => argument,
    }
}

fn is_function_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(type_id), ResolvedType::Function { .. })
}

/// Which of a body's parameter positions take a function value, and the
/// function type each is read at.
type FunctorParams = IndexMap<u32, TypeId>;

/// The function type each of `func`'s parameters holds a value of.
///
/// A specialization declares such a parameter at the one closure's own type
/// while its body still reads it at the function type, so what the body reads
/// answers where the signature cannot.
fn functor_params_of(func: &TirFunction, type_table: &TypeTable) -> FunctorParams {
    let mut reads = FunctorLocalReads {
        type_table,
        at: IndexMap::default(),
    };
    if let Some(body) = &func.body {
        reads.visit_block(body);
    }
    func.params
        .iter()
        .enumerate()
        .filter_map(|(position, param)| {
            let position = u32::try_from(position).unwrap();
            if is_function_type(param.type_id, type_table) {
                return Some((position, param.type_id));
            }
            reads.at.get(&param.local_index).map(|&at| (position, at))
        })
        .collect()
}

/// The function type each local is read at, for a parameter whose declared type
/// no longer says. A local read at two such types answers with the first.
struct FunctorLocalReads<'a> {
    type_table: &'a TypeTable,
    at: IndexMap<u32, TypeId>,
}

impl TirRefVisitor for FunctorLocalReads<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Local { index, .. } = &expr.kind
            && is_function_type(expr.type_id, self.type_table)
        {
            self.at.entry(*index).or_insert(expr.type_id);
        }
        self.walk_expr_in_frame(expr);
    }
}

/// Insert every element of `src` into `dst`, reporting whether `dst` grew.
fn extend(dst: &mut IndexSet<u32>, src: &IndexSet<u32>) -> bool {
    let mut grew = false;
    for &p in src {
        grew |= dst.insert(p);
    }
    grew
}

/// Where a function value was minted: the function whose body holds the
/// expression, and where in that body. A synthesised expression carries no
/// distinct range, so two mints can share a key and are then read as one, which
/// only ever widens the answer.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MintId {
    owner: u32,
    span: Span,
}

/// The body a walk is analysing, as the function-typed parameters of that body
/// are named: a named function's, or one minted closure's.
#[derive(Clone, Copy)]
enum Owner {
    Named(u32),
    Mint(MintId),
}

impl Owner {
    /// The function whose body this one sits in, which is what a mint inside it
    /// is keyed by. A closure's mints belong to the function holding it.
    fn function(self) -> u32 {
        match self {
            Owner::Named(function) => function,
            Owner::Mint(id) => id.owner,
        }
    }

    fn site(self, position: u32) -> Site {
        match self {
            Owner::Named(function) => Site::Named(function, position),
            Owner::Mint(id) => Site::Mint(id, position),
        }
    }
}

/// A function-typed parameter position, of a named function or of a closure.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Site {
    Named(u32, u32),
    Mint(MintId, u32),
}

/// Whose parameters a call resolving to one mint feeds. A reference to a
/// declaration the call graph does not carry names no body, so an argument
/// handed to one is followed no further.
#[derive(Clone, Copy, PartialEq)]
enum MintTarget {
    Named(u32),
    Closure,
    Opaque,
}

/// What one minting expression says about the value it mints: whose parameters
/// a call resolving to it feeds, which of those take a function value, and what
/// the call keeps.
#[derive(Clone)]
struct MintRow {
    target: MintTarget,
    functor_params: IndexSet<u32>,
    facts: RetentionFacts,
}

/// One parameter namespace a call's arguments land in. `Unfollowed` is a body
/// this analysis does not walk, so what reaches it is not tracked.
#[derive(Clone, Copy)]
enum Landing {
    Named(u32),
    Closure(MintId),
    Unfollowed,
}

/// Which function values may reach a call. `Any` is every value of the callee's
/// type, which is what a functor row answers.
#[derive(Clone)]
enum Callees {
    Mints(IndexSet<MintId>),
    Any,
}

impl Default for Callees {
    fn default() -> Self {
        Callees::Mints(IndexSet::default())
    }
}

impl Callees {
    fn one(id: MintId) -> Self {
        let mut ids = IndexSet::default();
        ids.insert(id);
        Callees::Mints(ids)
    }

    fn absorb(&mut self, other: &Callees) -> bool {
        match (&mut *self, other) {
            (Callees::Any, _) => false,
            (_, Callees::Any) => {
                *self = Callees::Any;
                true
            }
            (Callees::Mints(mine), Callees::Mints(theirs)) => {
                let mut grew = false;
                for &id in theirs {
                    grew |= mine.insert(id);
                }
                grew
            }
        }
    }
}

/// What a call through a function value may keep.
///
/// A function value is minted in exactly two places — a reference to a named
/// function and a closure literal — and no function type crosses a Component
/// Model boundary, so joining the facts of every such expression of one type
/// bounds every call through a value of that type. A call site that can name
/// which of those values reach it joins only those instead.
#[derive(Default, Clone)]
pub struct FunctorRows {
    /// The functor types some expression in the package mints a value at,
    /// collected before the fixpoint so that "nothing mints this" and "not
    /// reached yet" stay apart: the first is read conservatively, the second is
    /// the bottom a least fixpoint starts from.
    minted: IndexSet<TypeId>,
    facts: IndexMap<TypeId, RetentionFacts>,
    /// Each mint's own facts, whose parameters a call resolving to it feeds,
    /// and which of those parameters take a function value.
    per_mint: IndexMap<MintId, MintRow>,
    /// Which values reach each function-typed parameter position.
    flow: IndexMap<Site, Callees>,
    /// Function types whose values reach a position this analysis stopped
    /// following. A parameter of such a type is read as holding any value of it.
    escaped: IndexSet<TypeId>,
    /// What each indirect call site keeps, by its callee expression. A reader
    /// outside the fixpoint has the expression and not the resolution, so this
    /// is where the per-site answer is published; two callee expressions sharing
    /// a key answer their join.
    at_call: IndexMap<(TypeId, Span), Retained>,
}

/// What a call keeps, and where the positions the caller can resolve land.
///
/// Every reading of a call gives one: the union with no destinations is the
/// answer a reader that cannot resolve a landing takes, and it is what a
/// position any contributing body keeps out of sight reads as.
#[derive(Default, Clone)]
pub struct Retained {
    kept: IndexSet<u32>,
    bounded: IndexMap<u32, IndexSet<u32>>,
}

impl Retained {
    fn of(facts: &RetentionFacts) -> Self {
        Retained {
            kept: facts.union(),
            bounded: facts.bounded(),
        }
    }

    /// Join with another reading of the same call: it keeps what either keeps,
    /// and a position stays bounded only where every reading that keeps it says
    /// where it landed. A reading that does not keep a position says nothing
    /// about it, which is what lets the fixpoint start from keeping nothing.
    fn join(&mut self, other: &Retained) {
        for (source, destinations) in &other.bounded {
            if self.kept.contains(source) {
                if let Some(mine) = self.bounded.get_mut(source) {
                    extend(mine, destinations);
                }
            } else {
                self.bounded.insert(*source, destinations.clone());
            }
        }
        self.bounded
            .retain(|source, _| !other.kept.contains(source) || other.bounded.contains_key(source));
        extend(&mut self.kept, &other.kept);
    }

    /// Every position the call keeps, for a reader with no landing to resolve.
    #[must_use]
    pub fn positions(&self) -> IndexSet<u32> {
        self.kept.clone()
    }

    #[must_use]
    pub fn keeps(&self, position: u32) -> bool {
        self.kept.contains(&position)
    }

    /// The parameter positions (or [`RESULT`]) the call puts `position` in, or
    /// `None` where it also keeps it somewhere the caller cannot name.
    #[must_use]
    pub fn destinations(&self, position: u32) -> Option<&IndexSet<u32>> {
        self.bounded.get(&position)
    }
}

impl FunctorRows {
    fn row(&self, callee_type: TypeId, arity: usize) -> RetentionFacts {
        if !self.minted.contains(&callee_type) {
            let every: IndexSet<u32> = (0..u32::try_from(arity).unwrap()).collect();
            return RetentionFacts {
                escapes: every.clone(),
                into_result: every,
                into_param: IndexMap::default(),
                elements: IndexSet::default(),
            };
        }
        self.facts.get(&callee_type).cloned().unwrap_or_default()
    }

    /// The facts of a call the walk resolved to `callees`. A resolved set joins
    /// exactly those mints; an unresolved one falls back to the type's row.
    fn row_of(&self, callees: &Callees, callee_type: TypeId, arity: usize) -> RetentionFacts {
        let Callees::Mints(ids) = callees else {
            return self.row(callee_type, arity);
        };
        if !self.minted.contains(&callee_type) {
            return self.row(callee_type, arity);
        }
        let mut out = RetentionFacts::default();
        for id in ids {
            if let Some(row) = self.per_mint.get(id) {
                out.absorb(&row.facts);
            }
        }
        out
    }

    /// What a value of `type_id` arriving at `site` may be. A type this analysis
    /// stopped following anywhere is read as any value of it.
    fn at_site(&self, site: Site, type_id: TypeId) -> Callees {
        if self.escaped.contains(&type_id) {
            return Callees::Any;
        }
        self.flow.get(&site).cloned().unwrap_or_default()
    }

    fn target(&self, id: MintId) -> Option<MintTarget> {
        self.per_mint.get(&id).map(|row| row.target)
    }

    /// Which of a minted closure's parameters take a function value.
    fn mint_functor_params(&self, id: MintId) -> Option<&IndexSet<u32>> {
        self.per_mint.get(&id).map(|row| &row.functor_params)
    }

    fn merge(&mut self, callee_type: TypeId, facts: &RetentionFacts) -> bool {
        self.facts.entry(callee_type).or_default().absorb(facts)
    }

    fn merge_mint(&mut self, id: MintId, row: &MintRow) -> bool {
        let entry = self.per_mint.entry(id).or_insert_with(|| MintRow {
            target: row.target,
            functor_params: row.functor_params.clone(),
            facts: RetentionFacts::default(),
        });
        // Two mints can share a key — a synthesised expression carries no
        // distinct range — and the fixpoint only terminates while every step
        // climbs. Disagreeing targets therefore join to the one that names no
        // body, rather than the later one replacing the earlier for ever.
        let mut grew = false;
        if entry.target != row.target && !matches!(entry.target, MintTarget::Opaque) {
            entry.target = MintTarget::Opaque;
            grew = true;
        }
        grew |= extend(&mut entry.functor_params, &row.functor_params);
        grew |= entry.facts.absorb(&row.facts);
        grew
    }

    fn flow_into(&mut self, site: Site, callees: &Callees) -> bool {
        self.flow.entry(site).or_default().absorb(callees)
    }

    /// What a call through `callee` may keep, for a reader outside the fixpoint.
    /// A site the walk did not publish falls back to the type's row.
    #[must_use]
    pub fn retained(&self, callee: &TirExpr, arity: usize) -> Retained {
        match self.at_call.get(&(callee.type_id, callee.span)) {
            Some(retained) => retained.clone(),
            None => Retained::of(&self.row(callee.type_id, arity)),
        }
    }
}

/// The functor types of every parameter a call from outside the package can
/// fill: an exported function carries values no call site here contributed, so
/// such a parameter is read as holding any value of its type.
fn unfollowed_functor_params(project: &FlatPackage, type_table: &TypeTable) -> IndexSet<TypeId> {
    let mut found = IndexSet::default();
    for func in &project.functions {
        let func = func.borrow();
        if !func.is_export {
            continue;
        }
        for at in functor_params_of(&func, type_table).values() {
            found.insert(*at);
        }
    }
    found
}

/// Every functor type a `Closure` or `FuncRef` expression mints a value at.
///
/// Read over the same tree the row's reader walks, which is what makes a
/// missing type mean "nothing mints this": a value has to be minted in a tree
/// to reach a call site in it, so a later phase minting one — a handler thunk
/// at closure lifting — cannot widen what an earlier reader already answered.
fn minted_functor_types(project: &FlatPackage, type_table: &TypeTable) -> IndexSet<TypeId> {
    let mut collector = MintedTypes {
        type_table,
        found: IndexSet::default(),
    };
    for func in &project.functions {
        let func = func.borrow();
        if let Some(body) = &func.body {
            collector.visit_block(body);
        }
    }
    collector.found
}

struct MintedTypes<'a> {
    type_table: &'a TypeTable,
    found: IndexSet<TypeId>,
}

impl TirRefVisitor for MintedTypes<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if matches!(
            expr.kind,
            TirExprKind::Closure { .. } | TirExprKind::FuncRef { .. }
        ) && matches!(
            self.type_table.get(expr.type_id),
            ResolvedType::Function { .. }
        ) {
            self.found.insert(expr.type_id);
        }
        self.walk_expr(expr);
    }
}

/// What one body's walk contributes beyond its own facts: the row of every
/// function value it mints, which values reach each callee's function-typed
/// parameters, the function types whose values it stopped following, and what
/// each of its indirect call sites keeps.
#[derive(Default)]
struct Contributions {
    mints: Vec<(MintId, TypeId, MintRow)>,
    flowed: Vec<(Site, Callees)>,
    escaping: IndexSet<TypeId>,
    at_call: Vec<((TypeId, Span), Retained)>,
}

impl Contributions {
    fn extend(&mut self, other: Contributions) {
        self.mints.extend(other.mints);
        self.flowed.extend(other.flowed);
        for type_id in other.escaping {
            self.escaping.insert(type_id);
        }
        self.at_call.extend(other.at_call);
    }
}

/// A callee's facts, in the current fixpoint iteration.
struct StoresOracle<'a> {
    computed: &'a FuncKeyMap<RetentionFacts>,
    builtins: &'a BuiltinDeclarations,
    rows: &'a FunctorRows,
    call_graph: &'a CallGraph,
    /// Which of each function's parameters take a function value, by the dense
    /// id the call graph uses.
    functor_params: &'a [FunctorParams],
}

impl StoresOracle<'_> {
    /// Facts for a directly-called function, falling back to link's snapshot:
    /// monomorphization drops the generic declaration a builtin's is read from.
    fn direct(&self, func: &FunctionRef) -> RetentionFacts {
        if let Some(facts) = self.computed.get(&func.module_source, &func.name) {
            return facts.clone();
        }
        let mut facts = RetentionFacts::default();
        for retain in self.builtins.retain_specs(func) {
            declare_retention(&mut facts, retain);
        }
        facts
    }

    /// Facts for an indirect (functor) callee: the join over the values the
    /// walk resolved it to, or the callee type's row where it resolved none.
    fn indirect(&self, callees: &Callees, callee: &TirExpr, arity: usize) -> RetentionFacts {
        self.rows.row_of(callees, callee.type_id, arity)
    }

    fn functor_params(&self, function: u32) -> &FunctorParams {
        &self.functor_params[function as usize]
    }
}

/// Where a retained position lands, for the positions a caller can resolve.
///
/// A position is here only when every channel claiming it names a parameter or
/// the result, so the reference goes nowhere the caller cannot see. One that
/// also escapes is absent, and a reader takes the union as before.
#[derive(Default)]
pub struct BoundedRetention {
    at: FuncKeyMap<IndexMap<u32, IndexSet<u32>>>,
}

impl BoundedRetention {
    /// The parameter positions (or [`RESULT`]) the callee puts position
    /// `source` in, or `None` where it also keeps it somewhere the caller
    /// cannot name.
    #[must_use]
    pub fn destinations(&self, func: &FunctionRef, source: usize) -> Option<&IndexSet<u32>> {
        self.at
            .get(&func.module_source, &func.name)?
            .get(&u32::try_from(source).ok()?)
    }
}

/// What the fixpoint publishes: the positions each function may keep, where the
/// bounded ones land, and the row each functor type carries.
pub struct RetentionSummary {
    pub retained_params: RetainedParams,
    pub bounded: BoundedRetention,
    pub rows: FunctorRows,
}

pub fn compute_retention(
    project: &FlatPackage,
    call_graph: &CallGraph,
    builtins: &BuiltinDeclarations,
) -> RetentionSummary {
    let type_table = project.type_table.borrow();
    let mut computed: FuncKeyMap<RetentionFacts> = FuncKeyMap::default();

    for func in &project.functions {
        let func = func.borrow();
        debug_assert!(
            func.retains.is_empty() || func.body.is_none(),
            "`{}` has a body and a `#[retain]`",
            func.name
        );
        computed.insert(
            func.module_source.clone(),
            func.name.clone(),
            declared_facts(&func),
        );
    }

    let carrying = RefCarrying::new(&project.structs, &type_table);
    let functor_params: Vec<FunctorParams> = project
        .functions
        .iter()
        .map(|func| functor_params_of(&func.borrow(), &type_table))
        .collect();
    let mut rows = FunctorRows {
        minted: minted_functor_types(project, &type_table),
        escaped: unfollowed_functor_params(project, &type_table),
        ..FunctorRows::default()
    };
    // A row is a whole-program join, so a function's facts depend on a minting
    // site the call graph gives no edge to. The worklist settles each round
    // against the rows it was given, and the round is repeated while any row
    // still grows; both lattices only ever gain positions, so this terminates.
    loop {
        let mut round = Contributions::default();
        call_graph.solve(project, |id| {
            let func = project.functions[id as usize].borrow();
            let Some(body) = &func.body else {
                return false;
            };
            let positions: Vec<(u32, TypeId)> = func
                .params
                .iter()
                .map(|p| (p.local_index, p.type_id))
                .collect();
            let (found, from_body) = {
                let oracle = StoresOracle {
                    computed: &computed,
                    builtins,
                    rows: &rows,
                    call_graph,
                    functor_params: &functor_params,
                };
                facts_of_body(
                    &positions,
                    &functor_params[id as usize],
                    None,
                    &oracle,
                    &type_table,
                    &carrying,
                    Owner::Named(id),
                    BodyRef::Block(body),
                )
            };
            round.extend(from_body);
            let mut merged = computed
                .get(&func.module_source, &func.name)
                .cloned()
                .unwrap_or_default();
            if !merged.absorb(&found) {
                return false;
            }
            computed.insert(func.module_source.clone(), func.name.clone(), merged);
            true
        });
        let mut grew = false;
        for (id, callee_type, row) in round.mints {
            grew |= rows.merge(callee_type, &row.facts);
            grew |= rows.merge_mint(id, &row);
        }
        for (site, callees) in round.flowed {
            grew |= rows.flow_into(site, &callees);
        }
        for type_id in round.escaping {
            grew |= rows.escaped.insert(type_id);
        }
        for (key, retained) in round.at_call {
            match rows.at_call.get_mut(&key) {
                Some(entry) => entry.join(&retained),
                None => {
                    rows.at_call.insert(key, retained);
                }
            }
        }
        if !grew {
            break;
        }
    }

    let mut retained_params = RetainedParams::default();
    let mut bounded = BoundedRetention::default();
    for func in &project.functions {
        let func = func.borrow();
        if let Some(facts) = computed.get(&func.module_source, &func.name) {
            compiler_trace!(
                "retention",
                "{} escapes={:?} into_result={:?} into_param={:?}",
                func.name,
                facts.escapes,
                facts.into_result,
                facts.into_param
            );
            retained_params.insert(func.module_source.clone(), func.name.clone(), facts.union());
            bounded.at.insert(
                func.module_source.clone(),
                func.name.clone(),
                facts.bounded(),
            );
        }
    }

    RetentionSummary {
        retained_params,
        bounded,
        rows,
    }
}

/// What one `#[retain(...)]` clause states, by parameter position. The single
/// reading of a clause, so a declaration cannot mean two things by it.
///
/// `into = q` names the destination, so the clause takes the bounded channel
/// alone. Without one the reference persists and the declaration does not say
/// where, so both unbounded channels take it: the result is one of the places
/// it could be. `elements_of = p` claims what the referent holds rather than
/// the reference, which each call gates on the argument's own type.
fn declare_retention(facts: &mut RetentionFacts, retain: &RetainSpec<usize>) {
    let source = u32::try_from(retain.source).unwrap();
    if retain.elements {
        facts.elements.insert(source);
    }
    if let Some(destination) = retain.into {
        facts
            .into_param
            .entry(source)
            .or_default()
            .insert(u32::try_from(destination).unwrap());
    } else {
        facts.escapes.insert(source);
        facts.into_result.insert(source);
    }
}

/// What a declaration's `#[retain(...)]` clauses state, by parameter position.
fn declared_facts(func: &TirFunction) -> RetentionFacts {
    let mut facts = RetentionFacts::default();
    for retain in func.retains_by_position() {
        declare_retention(&mut facts, &retain);
    }
    facts
}

/// Whether a value of a type can hold a reference, memoized per `TypeId`.
/// `List::push` takes `Sink { r: &Item }` by value: a parameter carries a
/// reference when its type holds one, not only when it is one.
pub struct RefCarrying<'a> {
    structs: &'a [TirStruct],
    type_table: &'a TypeTable,
    memo: RefCell<IndexMap<TypeId, bool>>,
}

impl<'a> RefCarrying<'a> {
    #[must_use]
    pub fn new(structs: &'a [TirStruct], type_table: &'a TypeTable) -> Self {
        Self {
            structs,
            type_table,
            memo: RefCell::new(IndexMap::default()),
        }
    }

    #[must_use]
    pub fn type_table(&self) -> &'a TypeTable {
        self.type_table
    }
}

impl RefCarrying<'_> {
    #[must_use]
    pub fn holds(&self, type_id: TypeId) -> bool {
        self.walk(type_id, &mut Vec::new()).0
    }

    /// The answer, and whether it was reached through a type still being
    /// walked. An answer that leans on an open cycle is not memoized: the
    /// enclosing walk may still find the reference the cycle could not.
    fn walk(&self, type_id: TypeId, open: &mut Vec<TypeId>) -> (bool, bool) {
        if let Some(&answer) = self.memo.borrow().get(&type_id) {
            return (answer, false);
        }
        if open.contains(&type_id) {
            return (false, true);
        }
        open.push(type_id);
        let (answer, cyclic) = self.members(type_id, open);
        open.pop();
        if !cyclic {
            self.memo.borrow_mut().insert(type_id, answer);
        }
        (answer, cyclic)
    }

    fn any(&self, types: Vec<TypeId>, open: &mut Vec<TypeId>) -> (bool, bool) {
        let mut cyclic = false;
        for t in types {
            let (answer, saw_cycle) = self.walk(t, open);
            cyclic |= saw_cycle;
            if answer {
                return (true, cyclic);
            }
        }
        (false, cyclic)
    }

    fn members(&self, type_id: TypeId, open: &mut Vec<TypeId>) -> (bool, bool) {
        match self.type_table.get(type_id) {
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) => (true, false),
            // What a type parameter stands for is not known here.
            ResolvedType::TypeParam { .. } | ResolvedType::AssocTypeProjection { .. } => {
                (true, false)
            }
            ResolvedType::Reactive(inner) | ResolvedType::BuiltinArray(inner) => {
                let inner = *inner;
                self.walk(inner, open)
            }
            ResolvedType::Newtype { base_type, .. } => {
                let base = *base_type;
                self.walk(base, open)
            }
            ResolvedType::Struct { def, type_args } => {
                let (def, type_args) = (*def, type_args.clone());
                let fields = self
                    .structs
                    .iter()
                    .find(|s| s.def == def && s.type_args == type_args)
                    .map(|s| s.fields.iter().map(|f| f.type_id).collect::<Vec<_>>());
                match fields {
                    Some(fields) => self.any(fields, open),
                    // A declaration the plan phase cannot see is read as
                    // holding one, which only widens the carrier set.
                    None => (true, false),
                }
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let (def, type_args) = (*def, type_args.clone());
                // A payload that is one of the template's parameters is the
                // argument there; any other still reads its parameters as unknown.
                let payloads = self.type_table.variant_template_cases(def).map(|cases| {
                    cases
                        .iter()
                        .map(|&(_, _, payload)| match self.type_table.get(payload) {
                            ResolvedType::TypeParam { index, .. } => type_args[*index as usize],
                            _ => payload,
                        })
                        .collect()
                });
                self.any(payloads.unwrap_or(type_args), open)
            }
            ResolvedType::Variant { def } => {
                let def = *def;
                match self.type_table.variant_template_cases(def) {
                    Some(cases) => {
                        let payloads = cases.iter().map(|(_, _, payload)| *payload).collect();
                        self.any(payloads, open)
                    }
                    // A declaration the plan phase cannot see is read as
                    // holding one, which only widens the carrier set.
                    None => (true, false),
                }
            }
            // What the pack stands for is not known here, as a type parameter
            // is not; an inference variable is the same question unanswered.
            ResolvedType::TypePack { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::Unknown
            | ResolvedType::Error => (true, false),
            // A functor is a value; the environment it closed over is not
            // reachable from a caller's parameter through its type. Neither is
            // anything reachable from the rest: they carry no member at all.
            ResolvedType::Function { .. }
            | ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. } => (false, false),
        }
    }
}

/// The tree a body walk starts from: a function's block, or the expression a
/// closure's body is. Naming it lets one body be walked by two visitors.
#[derive(Clone, Copy)]
enum BodyRef<'a> {
    Block(&'a TirBlock),
    Expr(&'a TirExpr),
}

impl BodyRef<'_> {
    fn walk(self, visitor: &mut impl TirRefVisitor) {
        match self {
            BodyRef::Block(block) => visitor.visit_block(block),
            BodyRef::Expr(expr) => visitor.visit_expr(expr),
        }
    }
}

/// Where a reference-typed local must point.
///
/// The carrier map is a may-set: a local carries a position when some
/// assignment derived from it. This is the must-set — a local is anchored only
/// where every assignment to it roots at one of this body's parameters — so a
/// write through an anchored local lands inside those parameters and nowhere
/// else, which is what lets the bounded channel take it instead of an escape.
#[derive(Default)]
struct Anchors {
    at: IndexMap<u32, IndexSet<u32>>,
}

impl Anchors {
    /// The positions a write through `local` must land in, or `None` where
    /// some assignment to it goes somewhere this walk cannot name.
    fn of(&self, local: u32) -> Option<&IndexSet<u32>> {
        self.at
            .get(&local)
            .filter(|positions| !positions.is_empty())
    }
}

/// Where one assignment's value came from, reduced to what anchoring needs.
#[derive(Clone, Copy)]
enum Source {
    Param(u32),
    Local(u32),
    /// A call result, an rvalue, a global — a root this walk cannot name.
    Unknown,
}

/// Every assignment to a local in one body, as `(local, source)`.
struct AnchorScan<'a> {
    param_of_local: &'a IndexMap<u32, u32>,
    writes: Vec<(u32, Source)>,
}

impl AnchorScan<'_> {
    fn source_of(&self, value: &TirExpr) -> Source {
        match StoresWalker::place_root(unwrap_borrow(value)) {
            Some((root, _)) => match self.param_of_local.get(&root) {
                Some(&position) => Source::Param(position),
                None => Source::Local(root),
            },
            None => Source::Unknown,
        }
    }

    fn bind(&mut self, local: u32, value: &TirExpr) {
        if self.param_of_local.contains_key(&local) {
            return;
        }
        let source = self.source_of(value);
        self.writes.push((local, source));
    }

    fn bind_pattern(&mut self, pattern: &TirPattern, value: &TirExpr) {
        let mut binds: IndexSet<u32> = IndexSet::default();
        analyze::collect_pattern_bindings(pattern, &mut binds);
        for b in binds {
            self.bind(b, value);
        }
    }
}

impl TirRefVisitor for AnchorScan<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => self.bind(*local_index, value),
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.bind_pattern(pattern, value);
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Local { index, .. } = &target.kind {
                    self.bind(*index, value);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                for arm in arms {
                    self.bind_pattern(&arm.pattern, scrutinee);
                }
            }
            _ => {}
        }
        self.walk_expr_in_frame(expr);
    }
}

/// Settle [`Anchors`] over one body's assignments. A local whose every
/// assignment resolves to a parameter is anchored at those positions; one with
/// an unnameable source, or one derived from a local that has one, is not.
fn anchors_of_body(param_of_local: &IndexMap<u32, u32>, body: BodyRef) -> Anchors {
    let mut scan = AnchorScan {
        param_of_local,
        writes: Vec::new(),
    };
    body.walk(&mut scan);

    let mut loose: IndexSet<u32> = IndexSet::default();
    for &(local, source) in &scan.writes {
        if matches!(source, Source::Unknown) {
            loose.insert(local);
        }
    }
    // A local derived from an unanchored one is unanchored too, and a local
    // nothing here assigns is a write this walk did not model. Both only
    // spread, so the loop settles.
    loop {
        let mut grew = false;
        for &(local, source) in &scan.writes {
            let Source::Local(root) = source else {
                continue;
            };
            let unmodelled = !scan.writes.iter().any(|(bound, _)| *bound == root);
            if (loose.contains(&root) || unmodelled) && loose.insert(local) {
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut anchors = Anchors::default();
    loop {
        let mut grew = false;
        for &(local, source) in &scan.writes {
            if loose.contains(&local) {
                continue;
            }
            let positions = match source {
                Source::Param(position) => {
                    let mut one = IndexSet::default();
                    one.insert(position);
                    one
                }
                Source::Local(root) => anchors.at.get(&root).cloned().unwrap_or_default(),
                Source::Unknown => continue,
            };
            grew |= extend(anchors.at.entry(local).or_default(), &positions);
        }
        if !grew {
            break;
        }
    }
    anchors
}

/// Walk one body over `params`, given as `(local index, type)` so a closure's
/// own parameters are read the same way a function's are. `tail` is the
/// expression a body without a `return` yields, which a closure has and a
/// function does not. `owner` names the body, which is how a function value
/// reaching one of its parameters is addressed. Returns the body's own facts
/// and what it contributes to the whole-program tables.
fn facts_of_body(
    params: &[(u32, TypeId)],
    functor_params: &FunctorParams,
    tail: Option<&TirExpr>,
    oracle: &StoresOracle,
    type_table: &TypeTable,
    carrying: &RefCarrying,
    owner: Owner,
    body: BodyRef,
) -> (RetentionFacts, Contributions) {
    let mut carries: IndexMap<u32, Carried> = IndexMap::default();
    let mut param_of_local: IndexMap<u32, u32> = IndexMap::default();
    let mut reaching: IndexMap<u32, Callees> = IndexMap::default();
    for (i, (local_index, type_id)) in params.iter().enumerate() {
        let position = u32::try_from(i).unwrap();
        param_of_local.insert(*local_index, position);
        let entry = carries.entry(*local_index).or_default();
        // A reference parameter is only what it points at. What its referent's
        // elements hold is the caller's to account for, not this body's — see
        // `retain_elements_of_a_local_view.wado`.
        if is_reference_type(*type_id, type_table) {
            entry.is.insert(position);
        } else if carrying.holds(*type_id) {
            entry.holds.insert(position);
        }
    }
    for (&position, &at) in functor_params {
        if let Some((local_index, _)) = params.get(position as usize) {
            let callees = oracle.rows.at_site(owner.site(position), at);
            reaching.insert(*local_index, callees);
        }
    }
    let anchors = anchors_of_body(&param_of_local, body);
    let mut walker = StoresWalker {
        carries,
        param_of_local,
        anchors,
        oracle,
        type_table,
        carrying,
        owner,
        reaching,
        facts: RetentionFacts::default(),
        contributions: Contributions::default(),
        grew: false,
    };
    // One walk propagates a carrier only as far forward as it appears; a loop
    // carrying a reference backwards needs another. Repeat until nothing grows.
    loop {
        walker.grew = false;
        // Each pass reads more than the one before, so only the last one's
        // contributions are kept: the earlier ones are subsets of it.
        walker.contributions = Contributions::default();
        body.walk(&mut walker);
        if let Some(tail) = tail {
            let carried = walker.carried(tail).all();
            walker.reaches_result(&carried);
        }
        if !walker.grew {
            break;
        }
    }
    (walker.facts, walker.contributions)
}

struct StoresWalker<'a> {
    carries: IndexMap<u32, Carried>,
    /// Which parameter position each parameter's local holds, so a write
    /// through one is recorded as landing there rather than out of sight.
    param_of_local: IndexMap<u32, u32>,
    /// Where a write through a reference-typed local must land, for the locals
    /// that hold a reference to one of this body's own parameters and nothing
    /// else. See [`Anchors`].
    anchors: Anchors,
    oracle: &'a StoresOracle<'a>,
    type_table: &'a TypeTable,
    carrying: &'a RefCarrying<'a>,
    /// The body being walked, which names both the mints inside it and the
    /// positions its own parameters occupy.
    owner: Owner,
    /// Which function values each function-typed local may hold.
    reaching: IndexMap<u32, Callees>,
    facts: RetentionFacts,
    contributions: Contributions,
    grew: bool,
}

impl StoresWalker<'_> {
    /// What the value of `expr` hands on, and how: a reference derived from a
    /// parameter, a value holding one, or both. See [`Carried`].
    fn carried(&self, expr: &TirExpr) -> Carried {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                self.carries.get(index).cloned().unwrap_or_default()
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => self.place_roots(place),
            // A projection reads out of a container, so what comes out is a
            // reference into the container where it is one, and holds what the
            // container holds where its own type can hold a reference.
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => {
                self.placed(expr.type_id, self.carried(inner).all())
            }
            TirExprKind::StructLiteral { fields, .. } => Carried::held(
                fields
                    .iter()
                    .flat_map(|f| self.carried(&f.value).all())
                    .collect(),
            ),
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                Carried::held(
                    elements
                        .iter()
                        .flat_map(|e| self.carried(e).all())
                        .collect(),
                )
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => Carried::held(self.carried(p).all()),
            TirExprKind::Closure { captures, .. } => Carried::held(
                capture_source_locals(captures)
                    .flat_map(|index| {
                        self.carries
                            .get(&index)
                            .map(Carried::all)
                            .unwrap_or_default()
                    })
                    .collect(),
            ),
            // Which positions a call routes to its result is read off a body,
            // and a builtin has none — `a[i]`'s `array_get_ref` hands back a
            // slot of its first argument while declaring nothing — so a
            // reference-typed result carries every argument.
            TirExprKind::Call { func, args, .. } => {
                let routed = if is_reference_type(expr.type_id, self.type_table) {
                    args.iter()
                        .flat_map(|a| self.carried(&a.expr).all())
                        .collect()
                } else {
                    let facts = self.oracle.direct(func);
                    self.carried_args(args.iter().map(|a| &a.expr), &facts.into_result, &facts)
                };
                self.placed(expr.type_id, routed)
            }
            TirExprKind::IndirectCall { callee, args } => {
                let facts = self
                    .oracle
                    .indirect(&self.reaching_of(callee), callee, args.len());
                let routed = self.carried_args(args.iter(), &facts.into_result, &facts);
                self.placed(expr.type_id, routed)
            }
            // A control form's value is the tail of whichever arm runs, plus
            // whatever a `break` hands out of a labeled block.
            TirExprKind::Block(block) => self.block_carries(block),
            TirExprKind::LabeledBlock { block, .. } => {
                let mut out = self.block_carries(block);
                let mut breaks = BreakScan {
                    walker: self,
                    found: Carried::default(),
                };
                breaks.walk_block(block);
                out.absorb(&breaks.found);
                out
            }
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                let mut out = self.block_carries(then_branch);
                if let Some(eb) = else_branch {
                    let e = self.block_carries(eb);
                    out.absorb(&e);
                }
                out
            }
            TirExprKind::Match { arms, .. } => {
                let mut out = Carried::default();
                for arm in arms {
                    let c = self.carried(&arm.body);
                    out.absorb(&c);
                }
                out
            }
            _ => Carried::default(),
        }
    }

    /// Where `positions`, reached through a value of `type_id`, belong. A
    /// reference is *into* them and holds nothing of its own until something is
    /// put there; any other value that can keep a reference holds them. A type
    /// that is neither hands on nothing, which is how reading plain data out of
    /// a reference stops the walk.
    fn placed(&self, type_id: TypeId, positions: IndexSet<u32>) -> Carried {
        let mut out = Carried::default();
        if positions.is_empty() {
            return out;
        }
        if is_reference_type(type_id, self.type_table) {
            out.is = positions;
        } else if self.carrying.holds(type_id) {
            out.holds = positions;
        }
        out
    }

    /// A block's value is its final statement's expression.
    fn block_carries(&self, block: &TirBlock) -> Carried {
        block
            .tail_expr()
            .map_or_else(Carried::default, |e| self.carried(e))
    }

    fn carried_args<'e>(
        &self,
        args: impl Iterator<Item = &'e TirExpr>,
        positions: &IndexSet<u32>,
        facts: &RetentionFacts,
    ) -> IndexSet<u32> {
        args.enumerate()
            .filter(|(i, _)| positions.contains(&u32::try_from(*i).unwrap()))
            .flat_map(|(i, a)| self.claimed(u32::try_from(i).unwrap(), a, facts))
            .collect()
    }

    /// What the argument at `position` hands the callee. A position claimed by
    /// `elements_of` asks after the referent's elements, so it reads what the
    /// argument holds and never what it is: a reference to a container is a
    /// carrier by pointing at the container, not by anything the container keeps.
    fn claimed(&self, position: u32, arg: &TirExpr, facts: &RetentionFacts) -> IndexSet<u32> {
        let carried = self.carried(arg);
        if facts.elements.contains(&position) {
            return carried.holds;
        }
        carried.all()
    }

    /// What a *place* (the operand of `&`) carries, read off its root —
    /// `&p.field` roots at `p`, whatever the field's own type.
    fn place_roots(&self, place: &TirExpr) -> Carried {
        Self::place_root(place)
            .and_then(|(local, _)| self.carries.get(&local).cloned())
            .unwrap_or_default()
    }

    /// The local a place is rooted at, with its type, or `None` for a place
    /// this analysis cannot root (a call result, an rvalue).
    fn place_root(place: &TirExpr) -> Option<(u32, TypeId)> {
        match &place.kind {
            TirExprKind::Local { index, .. } => Some((*index, place.type_id)),
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => Self::place_root(inner),
            _ => None,
        }
    }

    fn escape(&mut self, positions: &IndexSet<u32>) {
        self.grew |= extend(&mut self.facts.escapes, positions);
    }

    /// Which function values `expr` may evaluate to. A mint is itself, a local
    /// is what reached it, and anything else — a field, a capture, a call
    /// result — is every value of the type.
    fn reaching_of(&self, expr: &TirExpr) -> Callees {
        if !is_function_type(expr.type_id, self.type_table) {
            return Callees::default();
        }
        match &expr.kind {
            TirExprKind::Closure { .. } | TirExprKind::FuncRef { .. } => {
                Callees::one(self.mint_id(expr.span))
            }
            TirExprKind::Local { index, .. } => {
                self.reaching.get(index).cloned().unwrap_or(Callees::Any)
            }
            _ => Callees::Any,
        }
    }

    fn mint_id(&self, span: Span) -> MintId {
        MintId {
            owner: self.owner.function(),
            span,
        }
    }

    /// Give up on naming what a local holds.
    fn reaches_any(&mut self, local: u32) {
        self.grew |= self
            .reaching
            .entry(local)
            .or_default()
            .absorb(&Callees::Any);
    }

    /// A function value settling in a local: the local may be any of the values
    /// that reached it.
    fn reaches_local(&mut self, local: u32, value: &TirExpr) {
        if !is_function_type(value.type_id, self.type_table) {
            return;
        }
        let callees = self.reaching_of(value);
        self.grew |= self.reaching.entry(local).or_default().absorb(&callees);
    }

    /// Record what a function value minted here contributes to its row.
    fn mint(&mut self, span: Span, callee_type: TypeId, row: MintRow) {
        if is_function_type(callee_type, self.type_table) {
            self.contributions
                .mints
                .push((self.mint_id(span), callee_type, row));
        }
    }

    /// A closure literal: analyse its body over its own parameters, which
    /// occupy local indices `0..params.len()` in the closure's namespace, and a
    /// `Capture` reaches none of them. A body with no `return` yields its tail,
    /// so that is what reaches the result.
    fn mint_closure(
        &mut self,
        span: Span,
        callee_type: TypeId,
        params: &[(String, TypeId)],
        body: &TirExpr,
    ) {
        let positions: Vec<(u32, TypeId)> = params
            .iter()
            .enumerate()
            .map(|(i, (_, type_id))| (u32::try_from(i).unwrap(), *type_id))
            .collect();
        let functor_params: FunctorParams = positions
            .iter()
            .filter(|(_, type_id)| is_function_type(*type_id, self.type_table))
            .map(|(position, type_id)| (*position, *type_id))
            .collect();
        let (facts, nested) = facts_of_body(
            &positions,
            &functor_params,
            Some(body),
            self.oracle,
            self.type_table,
            self.carrying,
            Owner::Mint(self.mint_id(span)),
            BodyRef::Expr(body),
        );
        self.contributions.extend(nested);
        self.mint(
            span,
            callee_type,
            MintRow {
                target: MintTarget::Closure,
                functor_params: functor_params.keys().copied().collect(),
                facts,
            },
        );
    }

    /// Follow every function value this call hands over into the parameter it
    /// lands at. A callee this walk could not name leaves the argument's type
    /// unfollowed, so a parameter of that type is read as holding any value.
    fn functor_args_reach(&mut self, landings: Option<&[Landing]>, args: &[&TirExpr]) {
        let Some(landings) = landings else {
            for arg in args {
                if is_function_type(arg.type_id, self.type_table) {
                    self.contributions.escaping.insert(arg.type_id);
                }
            }
            return;
        };
        // Which positions carry a function value is the callee's to say, not the
        // argument's: a phase that rewrites a call can leave an argument spelled
        // at a type its parameter is not, and reading the parameter is what keeps
        // such a position from looking like one nothing reaches.
        let oracle = self.oracle;
        for landing in landings {
            let sites: Vec<Site> = match landing {
                Landing::Named(function) => oracle
                    .functor_params(*function)
                    .keys()
                    .map(|&position| Site::Named(*function, position))
                    .collect(),
                Landing::Closure(id) => oracle
                    .rows
                    .mint_functor_params(*id)
                    .into_iter()
                    .flatten()
                    .map(|&position| Site::Mint(*id, position))
                    .collect(),
                Landing::Unfollowed => {
                    for arg in args {
                        if is_function_type(arg.type_id, self.type_table) {
                            self.contributions.escaping.insert(arg.type_id);
                        }
                    }
                    continue;
                }
            };
            for site in sites {
                let position = match site {
                    Site::Named(_, position) | Site::Mint(_, position) => position,
                };
                let callees = match args.get(position as usize) {
                    Some(arg) => self.arg_callees(arg),
                    None => Callees::Any,
                };
                self.contributions.flowed.push((site, callees));
            }
        }
    }

    /// What an argument hands the parameter it lands at. Anything but a function
    /// value this walk can name is every value of the parameter's type.
    fn arg_callees(&self, arg: &TirExpr) -> Callees {
        if !is_function_type(arg.type_id, self.type_table) {
            return Callees::Any;
        }
        self.reaching_of(arg)
    }

    /// The parameter namespaces an indirect call's arguments may land in, or
    /// `None` where the callee is unresolved. A mint the rows do not carry yet
    /// is left out: the round that records it runs the whole walk again.
    fn landings(&self, callees: &Callees) -> Option<Vec<Landing>> {
        let Callees::Mints(ids) = callees else {
            return None;
        };
        Some(
            ids.iter()
                .filter_map(|&id| match self.oracle.rows.target(id) {
                    Some(MintTarget::Named(function)) => Some(Landing::Named(function)),
                    Some(MintTarget::Closure) => Some(Landing::Closure(id)),
                    Some(MintTarget::Opaque) => Some(Landing::Unfollowed),
                    None => None,
                })
                .collect(),
        )
    }

    fn reaches_result(&mut self, positions: &IndexSet<u32>) {
        self.grew |= extend(&mut self.facts.into_result, positions);
    }

    /// A local takes on a value: it carries what that value carries, the way
    /// the value carried it.
    fn rebind(&mut self, local: u32, carried: &Carried) {
        if carried.is_empty() {
            return;
        }
        self.grew |= self.carries.entry(local).or_default().absorb(carried);
    }

    /// A write into a local's own aggregate: whatever went in is held by the
    /// local rather than named by it.
    fn hold_into(&mut self, local: u32, positions: &IndexSet<u32>) {
        if positions.is_empty() {
            return;
        }
        self.grew |= extend(&mut self.carries.entry(local).or_default().holds, positions);
    }

    /// A write of `value` into `target`: through a reference the caller owns it
    /// is an escape; into a local's own storage it makes that local a carrier.
    fn write(&mut self, target: &TirExpr, value: &TirExpr) {
        let carried = self.carried(value);
        if carried.is_empty() {
            return;
        }
        // A bare local target rebinds the local — `cur = cur.next` retargets the
        // walker, it does not write through it — so even a reference-typed one
        // only becomes a carrier. Only a projection reaches a referent.
        if let TirExprKind::Local { index, .. } = &target.kind {
            self.rebind(*index, &carried);
            return;
        }
        self.lands_in_place(target, &carried.all());
    }

    /// Where a reference written into `place` ends up. Into a local's own
    /// aggregate it makes that local a carrier; through one of this body's own
    /// reference parameters, or a local anchored at some of them, it is a
    /// bounded retention the caller resolves; anywhere else the caller cannot
    /// see it.
    fn lands_in_place(&mut self, place: &TirExpr, carried: &IndexSet<u32>) {
        match Self::place_root(place) {
            Some((root, ty)) if !is_reference_type(ty, self.type_table) => {
                self.hold_into(root, carried);
            }
            Some((root, _)) => {
                if let Some(&destination) = self.param_of_local.get(&root) {
                    self.retained_into(destination, carried);
                } else if let Some(destinations) = self.anchors.of(root).cloned() {
                    for destination in destinations {
                        self.retained_into(destination, carried);
                    }
                } else {
                    self.escape(carried);
                }
            }
            None => self.escape(carried),
        }
    }

    /// The storage the argument at a callee's destination position names, read
    /// through the `&`/`&mut` the argument wraps it in.
    fn lands_in_argument(&mut self, argument: &TirExpr, carried: &IndexSet<u32>) {
        self.lands_in_place(unwrap_borrow(argument), carried);
    }

    fn retained_into(&mut self, destination: u32, carried: &IndexSet<u32>) {
        for &source in carried {
            self.grew |= self
                .facts
                .into_param
                .entry(source)
                .or_default()
                .insert(destination);
        }
    }

    /// What a call hands on. An argument at an escaping position leaves for
    /// good; one the callee puts into another of its own parameters lands in
    /// whatever this body passed there, so it is resolved against that argument
    /// rather than assumed unseen. A result-bound one reaches the caller
    /// through the call's value instead (see [`StoresWalker::carries`]).
    fn call_hands_on(&mut self, args: &[&TirExpr], facts: &RetentionFacts) {
        let carried = self.carried_args(args.iter().copied(), &facts.escapes, facts);
        self.escape(&carried);
        for (&source, destinations) in &facts.into_param {
            let Some(argument) = args.get(source as usize) else {
                continue;
            };
            let carried = self.claimed(source, argument, facts);
            if carried.is_empty() {
                continue;
            }
            for &destination in destinations {
                match args.get(destination as usize) {
                    Some(place) => self.lands_in_argument(place, &carried),
                    // A destination no argument fills is one this call cannot
                    // resolve, and what it holds is out of sight.
                    None => self.escape(&carried),
                }
            }
        }
    }

    /// Each binding names a part of `source`, so it carries what that part
    /// can at its own type, as a projection does.
    fn bind_pattern(&mut self, pattern: &TirPattern, source: &TirExpr) {
        let carried = self.carried(source).all();
        if carried.is_empty() {
            return;
        }
        analyze::for_each_pattern_binding(pattern, &mut |local_index, type_id| {
            let placed = self.placed(type_id, carried.clone());
            self.rebind(local_index, &placed);
        });
    }

    /// A pattern names a part of its scrutinee, which this walk does not follow
    /// for function values, so every local it binds may hold any of them.
    fn pattern_reaches_any(&mut self, pattern: &TirPattern) {
        let mut binds: IndexSet<u32> = IndexSet::default();
        analyze::collect_pattern_bindings(pattern, &mut binds);
        for b in binds {
            self.reaches_any(b);
        }
    }
}

/// Unions what every `break` inside a labeled block hands out of it. Which
/// label a break targets is not distinguished — an outer one only over-counts.
struct BreakScan<'a, 'w> {
    walker: &'a StoresWalker<'w>,
    found: Carried,
}

impl TirRefVisitor for BreakScan<'_, '_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Break { value: Some(v), .. } = &stmt.kind {
            let c = self.walker.carried(v);
            self.found.absorb(&c);
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        self.walk_expr_in_frame(expr);
    }
}

impl TirRefVisitor for StoresWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                let c = self.carried(value);
                self.rebind(*local_index, &c);
                self.reaches_local(*local_index, value);
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.bind_pattern(pattern, value);
                self.pattern_reaches_any(pattern);
            }
            TirStmtKind::Return { value: Some(v) } => {
                let c = self.carried(v).all();
                self.reaches_result(&c);
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Assign { target, value } => {
                self.write(target, value);
                match &target.kind {
                    TirExprKind::Local { index, .. } => self.reaches_local(*index, value),
                    // A function value written anywhere else is read back out of
                    // a place this walk does not follow, which answers `Any`.
                    _ => {
                        if is_function_type(value.type_id, self.type_table) {
                            self.contributions.escaping.insert(value.type_id);
                        }
                    }
                }
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                let c = self.carried(value).all();
                self.escape(&c);
            }
            TirExprKind::Match { expr: scrut, arms } => {
                for arm in arms {
                    self.bind_pattern(&arm.pattern, scrut);
                    self.pattern_reaches_any(&arm.pattern);
                }
            }
            TirExprKind::Call { func, args, .. } => {
                let facts = self.oracle.direct(func);
                let exprs: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                self.call_hands_on(&exprs, &facts);
                let landing = match self.oracle.call_graph.id_of(func) {
                    Some(id) => Landing::Named(id),
                    None => Landing::Unfollowed,
                };
                self.functor_args_reach(Some(&[landing]), &exprs);
            }
            TirExprKind::IndirectCall { callee, args } => {
                let callees = self.reaching_of(callee);
                let facts = self.oracle.indirect(&callees, callee, args.len());
                let exprs: Vec<&TirExpr> = args.iter().collect();
                self.call_hands_on(&exprs, &facts);
                let landings = self.landings(&callees);
                self.functor_args_reach(landings.as_deref(), &exprs);
                self.contributions
                    .at_call
                    .push(((callee.type_id, callee.span), Retained::of(&facts)));
            }
            // A reference to a function-typed local is a way to replace what it
            // holds that this walk does not read, so the local may hold any
            // function value from here on.
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => {
                if is_function_type(place.type_id, self.type_table)
                    && let TirExprKind::Local { index, .. } = &place.kind
                {
                    self.reaches_any(*index);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                let exprs: Vec<&TirExpr> = args.iter().collect();
                self.functor_args_reach(None, &exprs);
            }
            // A function value's own facts belong to its functor type, not to
            // the body that mints it. A capture the closure may replace is the
            // same hole a reference is, so the outer local widens too.
            TirExprKind::Closure {
                params,
                body,
                captures,
                ..
            } => {
                for index in capture_source_locals(captures) {
                    self.reaches_any(index);
                }
                self.mint_closure(expr.span, expr.type_id, params, body);
                return;
            }
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => {
                let referenced = FunctionRef {
                    module_source: module_source.clone(),
                    name: name.clone(),
                    template: None,
                    monomorph_info: None,
                    method_info: None,
                };
                let facts = self.oracle.direct(&referenced);
                let (target, functor_params) = match self.oracle.call_graph.id_of(&referenced) {
                    Some(id) => (
                        MintTarget::Named(id),
                        self.oracle.functor_params(id).keys().copied().collect(),
                    ),
                    None => (MintTarget::Opaque, IndexSet::default()),
                };
                self.mint(
                    expr.span,
                    expr.type_id,
                    MintRow {
                        target,
                        functor_params,
                        facts,
                    },
                );
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
