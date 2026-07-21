//! Interprocedural reference-storage analysis: which reference-parameter
//! *positions* a function may persist beyond its call.
//!
//! A reference escapes only when a *reference-typed* value derived from a
//! parameter reaches a persistence sink — returned, placed in an aggregate /
//! global, stored through a reference, or passed to a callee at a position that
//! callee stores. A value read (`*p`, a value-typed `p.field`) copies data, not
//! the reference, so it never escapes. The result is a least fixpoint over the
//! call graph, seeded conservatively: a bodyless callee (extern / builtin / CM
//! import) cannot retain a guest reference across its boundary, so it stores its
//! declared positions only.
//!
//! Soundness rests on `carries` being an over-approximation of "this expression
//! is a reference into a parameter": every unmodelled reference-producing shape
//! falls to the conservative `_ => {}` only when the expression is *not*
//! reference-typed, so a genuinely reference-typed derivation is always tracked.

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

/// A callee's stored positions, in the current fixpoint iteration.
struct StoresOracle<'a> {
    computed: &'a StoredParams,
    type_table: &'a TypeTable,
}

impl StoresOracle<'_> {
    /// Positions a directly-called function stores. Unknown callee (not in the
    /// map — a bodyless / not-yet-computed function) → its declared positions
    /// are already folded into `computed` at seeding, so absence means "stores
    /// nothing known".
    fn direct(&self, func: &FunctionRef) -> IndexSet<u32> {
        self.computed
            .get(&func.module_source, &func.name)
            .cloned()
            .unwrap_or_default()
    }

    /// Positions an indirect (functor) callee may store, from its functor type's
    /// declared `stores`. A non-`Function` callee type is conservative: every
    /// position may be stored.
    fn indirect(&self, callee: &TirExpr, arity: usize) -> Option<IndexSet<u32>> {
        match self.type_table.get(callee.type_id) {
            ResolvedType::Function { stores, .. } => Some(stores.iter().copied().collect()),
            _ => Some((0..u32::try_from(arity).unwrap()).collect()),
        }
    }
}

pub fn compute_stored_params(project: &FlatPackage) -> StoredParams {
    let type_table = project.type_table.borrow();
    let mut computed: StoredParams = FuncKeyMap::default();

    // Seed: every function stores at least its declared reference positions (a
    // bodyless callee stores exactly those). `f.stores` names map to positions.
    for func in &project.functions {
        let func = func.borrow();
        let declared = declared_positions(&func);
        computed.insert(func.module_source.clone(), func.name.clone(), declared);
    }

    let call_graph = CallGraph::build(project);
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
            function_stored_params(body, &func.params, &oracle, &type_table)
        };
        let current = computed
            .get(&func.module_source, &func.name)
            .cloned()
            .unwrap_or_default();
        if found.iter().all(|p| current.contains(p)) {
            return false;
        }
        let mut merged = current;
        for p in found {
            merged.insert(p);
        }
        computed.insert(func.module_source.clone(), func.name.clone(), merged);
        true
    });

    drop(type_table);
    computed
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

fn function_stored_params(
    body: &TirBlock,
    params: &[TirParam],
    oracle: &StoresOracle,
    type_table: &TypeTable,
) -> IndexSet<u32> {
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
        stored: IndexSet::default(),
    };
    walker.visit_block(body);
    walker.stored
}

struct StoresWalker<'a> {
    carries: IndexMap<u32, IndexSet<u32>>,
    oracle: &'a StoresOracle<'a>,
    type_table: &'a TypeTable,
    stored: IndexSet<u32>,
}

impl StoresWalker<'_> {
    /// The parameter positions the reference produced by `expr` carries. Non
    /// reference-typed expressions carry nothing (a value read copies data, not
    /// the reference).
    fn carries(&self, expr: &TirExpr) -> IndexSet<u32> {
        if !is_reference_type(expr.type_id, self.type_table) {
            return IndexSet::default();
        }
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.carries.get(index).cloned().unwrap_or_default(),
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => self.place_roots(place),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => self.carries(inner),
            // A reference-typed result may borrow any argument.
            TirExprKind::Call { args, .. } => {
                args.iter().flat_map(|a| self.carries(&a.expr)).collect()
            }
            TirExprKind::MethodCall { receiver, args, .. } => std::iter::once(&**receiver)
                .chain(args.iter().map(|a| &a.expr))
                .flat_map(|e| self.carries(e))
                .collect(),
            TirExprKind::IndirectCall { args, .. } => {
                args.iter().flat_map(|a| self.carries(a)).collect()
            }
            _ => IndexSet::default(),
        }
    }

    /// The parameter positions a *place* (the operand of `&`) is rooted at,
    /// regardless of the place's own type — `&p.field` roots at `p`.
    fn place_roots(&self, place: &TirExpr) -> IndexSet<u32> {
        match &place.kind {
            TirExprKind::Local { index, .. } => self.carries.get(index).cloned().unwrap_or_default(),
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => self.place_roots(inner),
            _ => IndexSet::default(),
        }
    }

    fn mark(&mut self, positions: IndexSet<u32>) {
        self.stored.extend(positions);
    }

    /// A call/method/indirect argument at `pos`: a carried reference escapes iff
    /// the callee stores that position.
    fn arg(&mut self, arg: &TirExpr, pos: usize, stores: &Option<IndexSet<u32>>) {
        let carried = self.carries(arg);
        if carried.is_empty() {
            return;
        }
        let escapes = match stores {
            Some(s) => s.contains(&u32::try_from(pos).unwrap()),
            None => true,
        };
        if escapes {
            self.mark(carried);
        }
    }
}

impl TirRefVisitor for StoresWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                let c = self.carries(value);
                if !c.is_empty() {
                    self.carries.entry(*local_index).or_default().extend(c);
                }
            }
            TirStmtKind::Return { value: Some(v) } => {
                let c = self.carries(v);
                self.mark(c);
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Assign { target, value } => {
                let c = self.carries(value);
                if !c.is_empty() {
                    match &target.kind {
                        TirExprKind::Local { index, .. } => {
                            self.carries.entry(*index).or_default().extend(c);
                        }
                        _ => self.mark(c),
                    }
                }
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                let c = self.carries(value);
                self.mark(c);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    let c = self.carries(&f.value);
                    self.mark(c);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for e in elements {
                    let c = self.carries(e);
                    self.mark(c);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                let c = self.carries(p);
                self.mark(c);
            }
            TirExprKind::Call { func, args, .. } => {
                let stores = Some(self.oracle.direct(func));
                for (i, a) in args.iter().enumerate() {
                    self.arg(&a.expr, i, &stores);
                }
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                let stores = Some(self.oracle.direct(func));
                self.arg(receiver, 0, &stores);
                for (i, a) in args.iter().enumerate() {
                    self.arg(&a.expr, i + 1, &stores);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                let stores = self.oracle.indirect(callee, args.len());
                for (i, a) in args.iter().enumerate() {
                    self.arg(a, i, &stores);
                }
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
