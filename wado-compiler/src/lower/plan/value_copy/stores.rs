//! Interprocedural reference-storage analysis: which reference-parameter
//! *positions* a function may persist beyond its call. A least fixpoint over
//! the call graph, sound as long as `carries` over-approximates.
//!
//! Three channels, apart by where the reference lands:
//! [`StoresFacts::escapes`] reaches somewhere the caller cannot see,
//! [`StoresFacts::into_result`] reaches the return value, and
//! [`StoresFacts::into_param`] reaches a parameter the caller named, which the
//! caller resolves against its own argument there. Why the first two cannot be
//! one set is in WEP 2026-05-21; [`compute_stored_params`] publishes the union,
//! which is what a reader with no argument list must assume.
//!
//! An indirect call reads [`FunctorRows`], whose answer for a functor type is
//! the join over every expression that mints a function value of that type.

use super::callgraph::CallGraph;
use super::funcset::FuncKeyMap;
use super::is_reference_type;
use super::ownership::BuiltinDeclarations;
use crate::compiler_trace;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::analyze;
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;
use std::cell::RefCell;

/// Per-function set of reference-parameter positions the function may store.
pub type StoredParams = FuncKeyMap<IndexSet<u32>>;

/// The three ways a reference parameter outlives its call. See the module doc.
#[derive(Clone, Default)]
struct StoresFacts {
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

impl StoresFacts {
    /// Whether any channel claims `source`.
    fn claims(&self, source: u32) -> bool {
        self.escapes.contains(&source)
            || self.into_result.contains(&source)
            || self.into_param.contains_key(&source)
    }

    /// Absorb `other`, reporting whether anything grew.
    fn absorb(&mut self, other: &StoresFacts) -> bool {
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
                self.elements.insert(source);
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

/// Insert every element of `src` into `dst`, reporting whether `dst` grew.
fn extend(dst: &mut IndexSet<u32>, src: &IndexSet<u32>) -> bool {
    let mut grew = false;
    for &p in src {
        grew |= dst.insert(p);
    }
    grew
}

/// What a call through a function value of each functor type may keep.
///
/// A function value is minted in exactly two places — a reference to a named
/// function and a closure literal — and no function type crosses a Component
/// Model boundary, so joining the facts of every such expression of one type
/// bounds every call through a value of that type without a points-to analysis.
#[derive(Default, Clone)]
pub struct FunctorRows {
    /// The functor types some expression in the package mints a value at,
    /// collected before the fixpoint so that "nothing mints this" and "not
    /// reached yet" stay apart: the first is read conservatively, the second is
    /// the bottom a least fixpoint starts from.
    minted: IndexSet<TypeId>,
    facts: IndexMap<TypeId, StoresFacts>,
}

impl FunctorRows {
    fn row(&self, callee_type: TypeId, arity: usize) -> StoresFacts {
        if !self.minted.contains(&callee_type) {
            let every: IndexSet<u32> = (0..u32::try_from(arity).unwrap()).collect();
            return StoresFacts {
                escapes: every.clone(),
                into_result: every,
                into_param: IndexMap::default(),
                elements: IndexSet::default(),
            };
        }
        self.facts.get(&callee_type).cloned().unwrap_or_default()
    }

    fn merge(&mut self, callee_type: TypeId, facts: &StoresFacts) -> bool {
        self.facts.entry(callee_type).or_default().absorb(facts)
    }

    /// The positions a call through `callee_type` may keep, for a reader
    /// outside the fixpoint.
    #[must_use]
    pub fn retained(&self, callee_type: TypeId, arity: usize) -> IndexSet<u32> {
        self.row(callee_type, arity).union()
    }
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

/// A callee's facts, in the current fixpoint iteration.
struct StoresOracle<'a> {
    computed: &'a FuncKeyMap<StoresFacts>,
    builtins: &'a BuiltinDeclarations,
    rows: &'a FunctorRows,
}

impl StoresOracle<'_> {
    /// Facts for a directly-called function, falling back to link's snapshot:
    /// monomorphization drops the generic declaration a builtin's is read from.
    fn direct(&self, func: &FunctionRef) -> StoresFacts {
        if let Some(facts) = self.computed.get(&func.module_source, &func.name) {
            return facts.clone();
        }
        let mut facts = StoresFacts::default();
        for retain in self.builtins.retain_specs(func) {
            let source = u32::try_from(retain.source).unwrap();
            if retain.elements {
                facts.elements.insert(source);
            }
            match retain.into {
                Some(destination) => {
                    facts
                        .into_param
                        .entry(source)
                        .or_default()
                        .insert(u32::try_from(destination).unwrap());
                }
                None => {
                    facts.escapes.insert(source);
                }
            }
        }
        facts
    }

    /// Facts for an indirect (functor) callee, read off the row its callee type
    /// carries.
    fn indirect(&self, callee_type: TypeId, arity: usize) -> StoresFacts {
        self.rows.row(callee_type, arity)
    }
}

/// What the fixpoint publishes: the positions each function may keep, and the
/// row each functor type carries.
pub struct StoresSummary {
    pub stored_params: StoredParams,
    pub rows: FunctorRows,
}

pub fn compute_stored_params(
    project: &FlatPackage,
    call_graph: &CallGraph,
    builtins: &BuiltinDeclarations,
) -> StoresSummary {
    let type_table = project.type_table.borrow();
    let mut computed: FuncKeyMap<StoresFacts> = FuncKeyMap::default();

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

    let carrying = RefCarrying {
        structs: &project.structs,
        type_table: &type_table,
        memo: RefCell::new(IndexMap::default()),
    };
    let mut rows = FunctorRows {
        minted: minted_functor_types(project, &type_table),
        facts: IndexMap::default(),
    };
    // A row is a whole-program join, so a function's facts depend on a minting
    // site the call graph gives no edge to. The worklist settles each round
    // against the rows it was given, and the round is repeated while any row
    // still grows; both lattices only ever gain positions, so this terminates.
    loop {
        let mut minted: Vec<(TypeId, StoresFacts)> = Vec::new();
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
                };
                facts_of_body(&positions, None, &oracle, &type_table, &carrying, |w| {
                    w.visit_block(body);
                })
            };
            minted.extend(from_body);
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
        for (callee_type, facts) in minted {
            grew |= rows.merge(callee_type, &facts);
        }
        if !grew {
            break;
        }
    }

    let mut stored_params = StoredParams::default();
    for func in &project.functions {
        let func = func.borrow();
        if let Some(facts) = computed.get(&func.module_source, &func.name) {
            compiler_trace!(
                "stores",
                "{} escapes={:?} into_result={:?} into_param={:?}",
                func.name,
                facts.escapes,
                facts.into_result,
                facts.into_param
            );
            stored_params.insert(func.module_source.clone(), func.name.clone(), facts.union());
        }
    }

    StoresSummary {
        stored_params,
        rows,
    }
}

/// What a declaration's `#[retain(...)]` clauses state, by parameter position.
///
/// `into = q` names the destination, so the clause takes the bounded channel
/// alone. Without one the reference persists and the declaration does not say
/// where, so both unbounded channels take it: the result is one of the places
/// it could be. `elements_of = p` claims what the referent holds rather than
/// the reference, which each call gates on the argument's own type.
fn declared_facts(func: &TirFunction) -> StoresFacts {
    let position = |name: &str| {
        func.params
            .iter()
            .position(|p| p.name == name)
            .map(|i| u32::try_from(i).unwrap())
    };
    let mut facts = StoresFacts::default();
    for retain in &func.retains {
        let Some(source) = position(&retain.source) else {
            continue;
        };
        if retain.elements {
            facts.elements.insert(source);
        }
        if let Some(destination) = retain.into.as_deref().and_then(position) {
            facts
                .into_param
                .entry(source)
                .or_default()
                .insert(destination);
        } else {
            facts.escapes.insert(source);
            facts.into_result.insert(source);
        }
    }
    facts
}

/// Whether a value of a type can hold a reference, memoized per `TypeId`.
/// `List::push` takes `Sink { r: &Item }` by value: a parameter carries a
/// reference when its type holds one, not only when it is one.
struct RefCarrying<'a> {
    structs: &'a [TirStruct],
    type_table: &'a TypeTable,
    memo: RefCell<IndexMap<TypeId, bool>>,
}

impl RefCarrying<'_> {
    fn holds(&self, type_id: TypeId) -> bool {
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
                let payloads = self
                    .type_table
                    .variant_template_cases(def)
                    .map(|cases| cases.iter().map(|(_, _, payload)| *payload).collect());
                match payloads {
                    Some(payloads) => {
                        let (from_cases, cyclic) = self.any(payloads, open);
                        if from_cases {
                            return (true, cyclic);
                        }
                        let (from_args, more) = self.any(type_args, open);
                        (from_args, cyclic | more)
                    }
                    None => self.any(type_args, open),
                }
            }
            // A functor is a value; the environment it closed over is not
            // reachable from a caller's parameter through its type.
            _ => (false, false),
        }
    }
}

/// Walk one body over `params`, given as `(local index, type)` so a closure's
/// own parameters are read the same way a function's are. `tail` is the
/// expression a body without a `return` yields, which a closure has and a
/// function does not. Returns the body's own facts, and what every function
/// value it mints contributes to that value's functor row.
fn facts_of_body(
    params: &[(u32, TypeId)],
    tail: Option<&TirExpr>,
    oracle: &StoresOracle,
    type_table: &TypeTable,
    carrying: &RefCarrying,
    walk: impl Fn(&mut StoresWalker),
) -> (StoresFacts, Vec<(TypeId, StoresFacts)>) {
    let mut carries: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    let mut param_of_local: IndexMap<u32, u32> = IndexMap::default();
    for (i, (local_index, type_id)) in params.iter().enumerate() {
        let position = u32::try_from(i).unwrap();
        param_of_local.insert(*local_index, position);
        if carrying.holds(*type_id) {
            carries.entry(*local_index).or_default().insert(position);
        }
    }
    let mut walker = StoresWalker {
        carries,
        param_of_local,
        oracle,
        type_table,
        carrying,
        facts: StoresFacts::default(),
        minted: Vec::new(),
        grew: false,
    };
    // One walk propagates a carrier only as far forward as it appears; a loop
    // carrying a reference backwards needs another. Repeat until nothing grows.
    loop {
        walker.grew = false;
        walk(&mut walker);
        if let Some(tail) = tail {
            let carried = walker.carries(tail);
            walker.reaches_result(&carried);
        }
        if !walker.grew {
            break;
        }
    }
    (walker.facts, walker.minted)
}

struct StoresWalker<'a> {
    carries: IndexMap<u32, IndexSet<u32>>,
    /// Which parameter position each parameter's local holds, so a write
    /// through one is recorded as landing there rather than out of sight.
    param_of_local: IndexMap<u32, u32>,
    oracle: &'a StoresOracle<'a>,
    type_table: &'a TypeTable,
    carrying: &'a RefCarrying<'a>,
    facts: StoresFacts,
    /// What each function value this body mints contributes to its functor row.
    minted: Vec<(TypeId, StoresFacts)>,
    grew: bool,
}

impl StoresWalker<'_> {
    /// The parameter positions the value of `expr` carries: a reference derived
    /// from a parameter, or an aggregate holding one.
    fn carries(&self, expr: &TirExpr) -> IndexSet<u32> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                self.carries.get(index).cloned().unwrap_or_default()
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => self.place_roots(place),
            // A projection to a value reads data out of a reference; copying
            // data does not copy the reference that found it.
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => {
                if is_reference_type(expr.type_id, self.type_table) {
                    self.carries(inner)
                } else {
                    IndexSet::default()
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                fields.iter().flat_map(|f| self.carries(&f.value)).collect()
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                elements.iter().flat_map(|e| self.carries(e)).collect()
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => self.carries(p),
            TirExprKind::Closure { captures, .. } => captures
                .iter()
                .flat_map(|c| {
                    self.carries
                        .get(&c.outer_index)
                        .cloned()
                        .unwrap_or_default()
                })
                .collect(),
            // Which positions a call routes to its result is read off a body,
            // and a builtin has none — `a[i]`'s `array_get_ref` hands back a
            // slot of its first argument while declaring nothing — so a
            // reference-typed result carries every argument.
            TirExprKind::Call { func, args, .. } => {
                if is_reference_type(expr.type_id, self.type_table) {
                    return args.iter().flat_map(|a| self.carries(&a.expr)).collect();
                }
                let facts = self.oracle.direct(func);
                self.carried_args(args.iter().map(|a| &a.expr), &facts.into_result, &facts)
            }
            TirExprKind::IndirectCall { callee, args } => {
                let facts = self.oracle.indirect(callee.type_id, args.len());
                self.carried_args(args.iter(), &facts.into_result, &facts)
            }
            // A control form's value is the tail of whichever arm runs, plus
            // whatever a `break` hands out of a labeled block.
            TirExprKind::Block(block) => self.block_carries(block),
            TirExprKind::LabeledBlock { block, .. } => {
                let mut out = self.block_carries(block);
                let mut breaks = BreakScan {
                    walker: self,
                    found: IndexSet::default(),
                };
                breaks.walk_block(block);
                extend(&mut out, &breaks.found);
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
                    extend(&mut out, &e);
                }
                out
            }
            TirExprKind::Match { arms, .. } => arms
                .iter()
                .flat_map(|arm| self.carries(&arm.body))
                .collect(),
            _ => IndexSet::default(),
        }
    }

    /// A block's value is its final statement's expression.
    fn block_carries(&self, block: &TirBlock) -> IndexSet<u32> {
        match block.stmts.last().map(|s| &s.kind) {
            Some(TirStmtKind::Expr(e)) => self.carries(e),
            _ => IndexSet::default(),
        }
    }

    fn carried_args<'e>(
        &self,
        args: impl Iterator<Item = &'e TirExpr>,
        positions: &IndexSet<u32>,
        facts: &StoresFacts,
    ) -> IndexSet<u32> {
        args.enumerate()
            .filter(|(i, _)| positions.contains(&u32::try_from(*i).unwrap()))
            .flat_map(|(i, a)| self.claimed(u32::try_from(i).unwrap(), a, facts))
            .collect()
    }

    /// What the argument at `position` hands the callee. A position claimed by
    /// `elements_of` hands on what the referent's elements hold: nothing at all
    /// where they cannot hold a reference, and never the parameter the argument
    /// is rooted at, which is a carrier by being a reference to the container
    /// rather than by anything the container holds.
    fn claimed(&self, position: u32, arg: &TirExpr, facts: &StoresFacts) -> IndexSet<u32> {
        if !facts.elements.contains(&position) {
            return self.carries(arg);
        }
        if !self.elements_carry(arg.type_id) {
            return IndexSet::default();
        }
        let mut carried = self.carries(arg);
        if let Some(&position) = Self::place_root(unwrap_borrow(arg))
            .and_then(|(root, _)| self.param_of_local.get(&root))
        {
            carried.swap_remove(&position);
        }
        carried
    }

    /// Whether what a value of `type_id` holds can be a reference, past the
    /// reference the argument itself is.
    fn elements_carry(&self, type_id: TypeId) -> bool {
        let inner = match self.type_table.get(type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => type_id,
        };
        self.carrying.holds(inner)
    }

    /// What a *place* (the operand of `&`) carries, read off its root —
    /// `&p.field` roots at `p`, whatever the field's own type.
    fn place_roots(&self, place: &TirExpr) -> IndexSet<u32> {
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

    /// Record what a function value minted here contributes to its row.
    fn mint(&mut self, callee_type: TypeId, facts: StoresFacts) {
        if matches!(
            self.type_table.get(callee_type),
            ResolvedType::Function { .. }
        ) {
            self.minted.push((callee_type, facts));
        }
    }

    /// A closure literal: analyse its body over its own parameters, which
    /// occupy local indices `0..params.len()` in the closure's namespace, and a
    /// `Capture` reaches none of them. A body with no `return` yields its tail,
    /// so that is what reaches the result.
    fn mint_closure(&mut self, callee_type: TypeId, params: &[(String, TypeId)], body: &TirExpr) {
        let positions: Vec<(u32, TypeId)> = params
            .iter()
            .enumerate()
            .map(|(i, (_, type_id))| (u32::try_from(i).unwrap(), *type_id))
            .collect();
        let (facts, nested) = facts_of_body(
            &positions,
            Some(body),
            self.oracle,
            self.type_table,
            self.carrying,
            |walker| walker.visit_expr(body),
        );
        self.minted.extend(nested);
        self.mint(callee_type, facts);
    }

    fn reaches_result(&mut self, positions: &IndexSet<u32>) {
        self.grew |= extend(&mut self.facts.into_result, positions);
    }

    fn carry_into(&mut self, local: u32, positions: &IndexSet<u32>) {
        if positions.is_empty() {
            return;
        }
        self.grew |= extend(self.carries.entry(local).or_default(), positions);
    }

    /// A write of `value` into `target`: through a reference the caller owns it
    /// is an escape; into a local's own storage it makes that local a carrier.
    fn write(&mut self, target: &TirExpr, value: &TirExpr) {
        let carried = self.carries(value);
        if carried.is_empty() {
            return;
        }
        // A bare local target rebinds the local — `cur = cur.next` retargets the
        // walker, it does not write through it — so even a reference-typed one
        // only becomes a carrier. Only a projection reaches a referent.
        if let TirExprKind::Local { index, .. } = &target.kind {
            self.carry_into(*index, &carried);
            return;
        }
        self.lands_in_place(target, &carried);
    }

    /// Where a reference written into `place` ends up. Into a local's own
    /// aggregate it makes that local a carrier; through one of this body's own
    /// reference parameters it is a bounded retention the caller resolves;
    /// anywhere else the caller cannot see it.
    fn lands_in_place(&mut self, place: &TirExpr, carried: &IndexSet<u32>) {
        match Self::place_root(place) {
            Some((root, ty)) if !is_reference_type(ty, self.type_table) => {
                self.carry_into(root, carried);
            }
            Some((root, _)) => match self.param_of_local.get(&root) {
                Some(&destination) => self.retained_into(destination, carried),
                None => self.escape(carried),
            },
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
    fn call_hands_on(&mut self, args: &[&TirExpr], facts: &StoresFacts) {
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

    fn bind_pattern(&mut self, pattern: &TirPattern, source: &TirExpr) {
        let carried = self.carries(source);
        if carried.is_empty() {
            return;
        }
        let mut binds: IndexSet<u32> = IndexSet::default();
        analyze::collect_pattern_bindings(pattern, &mut binds);
        for b in binds {
            self.carry_into(b, &carried);
        }
    }
}

/// Unions what every `break` inside a labeled block hands out of it. Which
/// label a break targets is not distinguished — an outer one only over-counts.
struct BreakScan<'a, 'w> {
    walker: &'a StoresWalker<'w>,
    found: IndexSet<u32>,
}

impl TirRefVisitor for BreakScan<'_, '_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Break { value: Some(v), .. } = &stmt.kind {
            extend(&mut self.found, &self.walker.carries(v));
        }
        self.walk_stmt(stmt);
    }
}

impl TirRefVisitor for StoresWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                let c = self.carries(value);
                self.carry_into(*local_index, &c);
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.bind_pattern(pattern, value);
            }
            TirStmtKind::Return { value: Some(v) } => {
                let c = self.carries(v);
                self.reaches_result(&c);
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Assign { target, value } => self.write(target, value),
            TirExprKind::GlobalVarSet { value, .. } => {
                let c = self.carries(value);
                self.escape(&c);
            }
            TirExprKind::Match { expr: scrut, arms } => {
                for arm in arms {
                    self.bind_pattern(&arm.pattern, scrut);
                }
            }
            TirExprKind::Call { func, args, .. } => {
                let facts = self.oracle.direct(func);
                let exprs: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                self.call_hands_on(&exprs, &facts);
            }
            TirExprKind::IndirectCall { callee, args } => {
                let facts = self.oracle.indirect(callee.type_id, args.len());
                let exprs: Vec<&TirExpr> = args.iter().collect();
                self.call_hands_on(&exprs, &facts);
            }
            // A function value's own facts belong to its functor type, not to
            // the body that mints it. The closure body is analysed in its own
            // parameter namespace, so the default walk must not descend into it
            // with this body's carriers.
            TirExprKind::Closure { params, body, .. } => {
                self.mint_closure(expr.type_id, params, body);
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
                    monomorph_info: None,
                    method_info: None,
                };
                let facts = self.oracle.direct(&referenced);
                self.mint(expr.type_id, facts);
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
