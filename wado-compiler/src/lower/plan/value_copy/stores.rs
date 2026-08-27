//! Interprocedural reference-storage analysis: which reference-parameter
//! *positions* a function may persist beyond its call. A least fixpoint over the
//! call graph, sound as long as `carries` over-approximates.
//!
//! A reference persists in one of two ways, tracked apart because only the first
//! is a property of the callee alone:
//!
//! - it reaches a global, or is written through a reference the caller owns
//!   ([`StoresFacts::escapes`]) — persists however the call is used;
//! - it reaches the return value ([`StoresFacts::into_result`]) — persists
//!   exactly as long as the caller keeps the result.
//!
//! Keeping them apart is what stops a transient carrier from poisoning its
//! source: `a.iter()` hands the iterator a reference to `a`, but a `.collect()`
//! that drops the iterator stores nothing, so the enclosing function does not
//! store `a`. Collapsing the two — as marking every carrying argument did —
//! makes every `&List` parameter a stored one the moment the body iterates it.
//!
//! What a *caller* must assume is the union: the returned reference may be kept.
//! That is what [`compute_stored_params`] publishes.

use super::callgraph::CallGraph;
use super::funcset::FuncKeyMap;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirParam, TirStmt, TirStmtKind,
    TirUnaryOp, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Per-function set of reference-parameter positions the function may store.
pub type StoredParams = FuncKeyMap<IndexSet<u32>>;

/// The two ways a reference parameter outlives its call. See the module doc.
#[derive(Clone, Default)]
struct StoresFacts {
    escapes: IndexSet<u32>,
    into_result: IndexSet<u32>,
}

impl StoresFacts {
    /// Absorb `other`, reporting whether anything grew.
    fn absorb(&mut self, other: &StoresFacts) -> bool {
        extend(&mut self.escapes, &other.escapes)
            | extend(&mut self.into_result, &other.into_result)
    }

    fn union(&self) -> IndexSet<u32> {
        let mut out = self.escapes.clone();
        for &p in &self.into_result {
            out.insert(p);
        }
        out
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

/// A callee's facts, in the current fixpoint iteration.
struct StoresOracle<'a> {
    computed: &'a FuncKeyMap<StoresFacts>,
    type_table: &'a TypeTable,
}

impl StoresOracle<'_> {
    /// Facts for a directly-called function. Unknown callee (not in the map — a
    /// bodyless / not-yet-computed function) → its declared positions are
    /// already folded into `computed` at seeding, so absence means "stores
    /// nothing known".
    fn direct(&self, func: &FunctionRef) -> StoresFacts {
        self.computed
            .get(&func.module_source, &func.name)
            .cloned()
            .unwrap_or_default()
    }

    /// Facts for an indirect (functor) callee, from its functor type's declared
    /// `stores`. A non-`Function` callee type is conservative: every position
    /// may escape.
    fn indirect(&self, callee: &TirExpr, arity: usize) -> StoresFacts {
        let positions: IndexSet<u32> =
            if let ResolvedType::Function { stores, .. } = self.type_table.get(callee.type_id) {
                stores.iter().copied().collect()
            } else {
                (0..u32::try_from(arity).unwrap()).collect()
            };
        StoresFacts {
            escapes: positions.clone(),
            into_result: positions,
        }
    }
}

pub fn compute_stored_params(project: &FlatPackage, call_graph: &CallGraph) -> StoredParams {
    let type_table = project.type_table.borrow();
    let mut computed: FuncKeyMap<StoresFacts> = FuncKeyMap::default();

    for func in &project.functions {
        let func = func.borrow();
        // A declared `stores[p]` says the reference persists, not where. A
        // value-returning body — the iterator and adapter shape, `iter()`,
        // `as_slice()` — hands it out with the result, and the walk finds any
        // further escape itself. With no result to hand it to, or no body to
        // read (`List::push` stores through a builtin), the strong reading is
        // the only sound one. See the known gap in WEP 2026-05-21 for what the
        // reading still rests on.
        let declared = declared_positions(&func);
        let hands_out_result =
            func.body.is_some() && !matches!(type_table.get(func.return_type), ResolvedType::Unit);
        let facts = StoresFacts {
            escapes: if hands_out_result {
                IndexSet::default()
            } else {
                declared.clone()
            },
            into_result: declared,
        };
        computed.insert(func.module_source.clone(), func.name.clone(), facts);
    }

    call_graph.solve(project, |id| {
        let func = project.functions[id as usize].borrow();
        let Some(body) = &func.body else {
            return false;
        };
        let found = {
            let oracle = StoresOracle {
                computed: &computed,
                type_table: &type_table,
            };
            function_stores_facts(body, &func.params, &oracle, &type_table)
        };
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

    let mut out = StoredParams::default();
    for func in &project.functions {
        let func = func.borrow();
        if let Some(facts) = computed.get(&func.module_source, &func.name) {
            out.insert(func.module_source.clone(), func.name.clone(), facts.union());
        }
    }

    drop(type_table);
    out
}

/// The reference-parameter positions named in a function's `stores[...]` clause.
fn declared_positions(func: &crate::tir::TirFunction) -> IndexSet<u32> {
    func.params
        .iter()
        .enumerate()
        .filter(|(_, p)| func.stores.contains(&p.name))
        .map(|(i, _)| u32::try_from(i).unwrap())
        .collect()
}

fn function_stores_facts(
    body: &TirBlock,
    params: &[TirParam],
    oracle: &StoresOracle,
    type_table: &TypeTable,
) -> StoresFacts {
    let mut carries: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    for (i, p) in params.iter().enumerate() {
        if is_reference_type(p.type_id, type_table) {
            carries
                .entry(p.local_index)
                .or_default()
                .insert(u32::try_from(i).unwrap());
        }
    }
    let mut walker = StoresWalker {
        carries,
        oracle,
        type_table,
        facts: StoresFacts::default(),
        grew: false,
    };
    // One walk propagates a carrier only as far forward as it appears; a loop
    // carrying a reference backwards needs another. Repeat until nothing grows.
    loop {
        walker.grew = false;
        walker.visit_block(body);
        if !walker.grew {
            break;
        }
    }
    walker.facts
}

struct StoresWalker<'a> {
    carries: IndexMap<u32, IndexSet<u32>>,
    oracle: &'a StoresOracle<'a>,
    type_table: &'a TypeTable,
    facts: StoresFacts,
    grew: bool,
}

impl StoresWalker<'_> {
    /// The parameter positions the value of `expr` carries: a reference derived
    /// from a parameter, or an aggregate holding one — copying a struct copies
    /// its reference fields, so a projection out of a carrier carries too.
    fn carries(&self, expr: &TirExpr) -> IndexSet<u32> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                self.carries.get(index).cloned().unwrap_or_default()
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => self.place_roots(place),
            // A projection hands back the reference it names; a projection to a
            // value reads data out of one, and copying data does not copy the
            // reference that found it.
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
            // Only the positions the callee routes into its result reach the
            // caller through the call's value; an escaping one is marked at the
            // call site instead. That routing is read off a body, and a builtin
            // has none: `a[i]`'s `array_get_ref` hands back a slot of its first
            // argument while declaring nothing, so a reference-typed result is
            // read as carrying every argument — the reading `List::index_ref`
            // and every wrapper of it depends on.
            TirExprKind::Call { func, args, .. } => {
                if is_reference_type(expr.type_id, self.type_table) {
                    return args.iter().flat_map(|a| self.carries(&a.expr)).collect();
                }
                let facts = self.oracle.direct(func);
                self.carried_args(args.iter().map(|a| &a.expr), &facts.into_result)
            }
            TirExprKind::IndirectCall { callee, args } => {
                let facts = self.oracle.indirect(callee, args.len());
                self.carried_args(args.iter(), &facts.into_result)
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
    ) -> IndexSet<u32> {
        args.enumerate()
            .filter(|(i, _)| positions.contains(&u32::try_from(*i).unwrap()))
            .flat_map(|(_, a)| self.carries(a))
            .collect()
    }

    /// The parameter positions a *place* (the operand of `&`) is rooted at,
    /// regardless of the place's own type — `&p.field` roots at `p`.
    fn place_roots(&self, place: &TirExpr) -> IndexSet<u32> {
        match &place.kind {
            TirExprKind::Local { index, .. } => {
                self.carries.get(index).cloned().unwrap_or_default()
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => self.place_roots(inner),
            _ => IndexSet::default(),
        }
    }

    /// The root local of a place, or `None` for a place this analysis cannot
    /// root (a call result, an rvalue).
    fn place_root(place: &TirExpr) -> Option<&TirExpr> {
        match &place.kind {
            TirExprKind::Local { .. } => Some(place),
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
        // Writing through a reference local lands in its referent, storage the
        // caller owns; writing into a local's own aggregate does not.
        match Self::place_root(target) {
            Some(root) if !is_reference_type(root.type_id, self.type_table) => {
                let TirExprKind::Local { index, .. } = &root.kind else {
                    unreachable!("place_root yields a Local")
                };
                self.carry_into(*index, &carried);
            }
            _ => self.escape(&carried),
        }
    }

    /// A call's arguments: an escaping position persists here, a result-bound
    /// one through the call's value (see [`StoresWalker::carries`]).
    fn call_args<'e>(&mut self, args: impl Iterator<Item = &'e TirExpr>, facts: &StoresFacts) {
        for (i, a) in args.enumerate() {
            if !facts.escapes.contains(&u32::try_from(i).unwrap()) {
                continue;
            }
            let carried = self.carries(a);
            self.escape(&carried);
        }
    }

    fn bind_pattern(&mut self, pattern: &crate::tir::TirPattern, source: &TirExpr) {
        let carried = self.carries(source);
        if carried.is_empty() {
            return;
        }
        let mut binds: IndexSet<u32> = IndexSet::default();
        super::analyze::collect_pattern_bindings(pattern, &mut binds);
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
                self.call_args(args.iter().map(|a| &a.expr), &facts);
            }
            TirExprKind::IndirectCall { callee, args } => {
                let facts = self.oracle.indirect(callee, args.len());
                self.call_args(args.iter(), &facts);
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

fn is_reference_type(type_id: crate::tir::TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    )
}
