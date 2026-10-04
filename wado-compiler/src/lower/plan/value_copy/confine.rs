//! Interprocedural confinement analysis over pre-boxing TIR (WEP 2026-05-21).
//!
//! A by-value parameter is *confined* when the callee keeps nothing of it past
//! the call, so a caller passing a still-live value into it needs no defensive
//! copy. The callee then holds it borrowed, not owned, and copies before any
//! write. One it returns is confined too where the callee never takes it over:
//! the result is then a projection of the argument, and the caller copies it
//! only where it takes the result over itself, as it would any borrowed value.
//!
//! Three channels per parameter reach a least fixpoint: `ret` (flows into a
//! returned value), `side` (written to lasting storage) and `taken` (bound to
//! an owner the body holds, where borrowing it would move the copy into the
//! callee rather than save it). A parameter is confined iff it is not declared
//! `mut`, `side` is not raised, and `ret` and `taken` are not both. The
//! analysis over-approximates escape: unmodelled constructs, a closure's
//! captures, and a handler / `resume` body mark the parameters they reach.

use super::analyze::{collect_pattern_bindings, passes_through};
use super::callgraph::CallGraph;
use super::funcset::FuncKeyMap;
use super::needs_value_copy;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    BuiltinDeclarations, FunctionKind, FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind,
    TirFunction, TirParam, TirPattern, TirStmt, TirStmtKind, TypeId, TypeTable,
    capture_source_locals,
};
use crate::tir_visitor::TirRefVisitor;

/// Per-parameter confinement bits. A missing entry answers "not confined".
pub struct ConfinedParams {
    map: FuncKeyMap<Vec<bool>>,
}

impl ConfinedParams {
    pub fn is_confined(&self, func: &FunctionRef, param_index: usize) -> bool {
        self.map
            .get(&func.module_source, &func.name)
            .and_then(|bits| bits.get(param_index))
            .copied()
            .unwrap_or(false)
    }

    /// The locals of `func`'s confined parameters, which its callers pass
    /// uncopied and it therefore holds borrowed.
    pub fn borrowed_locals(&self, func: &TirFunction) -> IndexSet<u32> {
        let Some(bits) = self.map.get(&func.module_source, &func.name) else {
            return IndexSet::default();
        };
        func.params
            .iter()
            .zip(bits)
            .filter(|(_, confined)| **confined)
            .map(|(p, _)| p.local_index)
            .collect()
    }
}

#[derive(Clone, Default, PartialEq)]
struct ParamEscape {
    ret: Vec<bool>,
    side: Vec<bool>,
    taken: Vec<bool>,
    /// A `mut` parameter, which every caller hands a copy of: the callee owns
    /// it whatever it does with it.
    declared_mut: Vec<bool>,
}

impl ParamEscape {
    fn new(params: &[TirParam]) -> Self {
        let n = params.len();
        Self {
            ret: vec![false; n],
            side: vec![false; n],
            taken: vec![false; n],
            declared_mut: params.iter().map(|p| p.is_mut).collect(),
        }
    }

    fn confined_at(&self, i: usize) -> bool {
        !(self.declared_mut[i] || self.side[i] || self.ret[i] && self.taken[i])
    }

    fn confined(&self) -> Vec<bool> {
        (0..self.side.len()).map(|i| self.confined_at(i)).collect()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A body-less function with a declaration link snapshot.
    Declared,
    ValueCopy,
    HasBody,
    /// A body-less function nothing describes: a dispatch stub.
    Opaque,
}

pub fn compute_confined_params(
    project: &FlatPackage,
    call_graph: &CallGraph,
    builtins: &BuiltinDeclarations,
) -> ConfinedParams {
    let type_table = project.type_table.borrow();
    let kinds = classify_functions(project);

    let mut funcs: FuncKeyMap<ParamEscape> = FuncKeyMap::default();
    for func in &project.functions {
        let func = func.borrow();
        if func.body.is_some() {
            funcs.insert(
                func.module_source.clone(),
                func.name.clone(),
                ParamEscape::new(&func.params),
            );
        }
    }

    call_graph.solve(project, |id| {
        let func = project.functions[id as usize].borrow();
        let Some(body) = &func.body else {
            return false;
        };
        let ctx = Ctx {
            type_table: &type_table,
            builtins,
            kinds: &kinds,
            funcs: &funcs,
        };
        let current = funcs.get(&func.module_source, &func.name).unwrap();
        let mut pe = current.clone();
        if body_defies_model(body) {
            for b in pe.ret.iter_mut().chain(&mut pe.side).chain(&mut pe.taken) {
                *b = true;
            }
        } else {
            compute_escape(&ctx, body, func.return_type, &mut pe);
        }
        if &pe == funcs.get(&func.module_source, &func.name).unwrap() {
            false
        } else {
            funcs.insert(func.module_source.clone(), func.name.clone(), pe);
            true
        }
    });

    ConfinedParams {
        map: funcs.map_values(|pe| pe.confined()),
    }
}

/// The functions this scan reads a body of, or knows to keep nothing. A
/// body-less one is classified at the call, off its declaration.
fn classify_functions(project: &FlatPackage) -> FuncKeyMap<Kind> {
    let mut kinds = FuncKeyMap::default();
    for func in &project.functions {
        let func = func.borrow();
        let kind = if matches!(func.kind, FunctionKind::ValueCopy { .. }) {
            Kind::ValueCopy
        } else if func.body.is_some() {
            Kind::HasBody
        } else {
            continue;
        };
        kinds.insert(func.module_source.clone(), func.name.clone(), kind);
    }
    kinds
}

struct Ctx<'a> {
    type_table: &'a TypeTable,
    builtins: &'a BuiltinDeclarations,
    kinds: &'a FuncKeyMap<Kind>,
    funcs: &'a FuncKeyMap<ParamEscape>,
}

impl Ctx<'_> {
    /// A body-less callee reads its declaration wherever it comes from — a
    /// `core:builtin`, a CM import, a Wasm asset — so a monomorphized instance
    /// absent from the table is found by the generic name link snapshot.
    fn kind(&self, func: &FunctionRef) -> Kind {
        if let Some(kind) = self.kinds.get(&func.module_source, &func.name) {
            return *kind;
        }
        if self.builtins.get(func).is_some() {
            Kind::Declared
        } else {
            Kind::Opaque
        }
    }

    /// The fixpoint's entry for a callee [`Self::kind`] answers `HasBody` for:
    /// every function with a body has one.
    fn escape_of(&self, func: &FunctionRef) -> &ParamEscape {
        self.funcs
            .get(&func.module_source, &func.name)
            .expect("a callee with a body has a fixpoint entry")
    }

    /// One escape channel of a `HasBody` callee's parameter.
    fn callee_escape(
        &self,
        func: &FunctionRef,
        param_index: usize,
        channel: impl Fn(&ParamEscape) -> &[bool],
    ) -> bool {
        channel(self.escape_of(func))[param_index]
    }

    /// Whether `operand`, at `param_index`, outlives this call. A value-copy
    /// helper keeps nothing, a builtin keeps what its `#[storage]` implies — and
    /// through a reference only elements that carry an identity — a body
    /// answers from the fixpoint, and a callee this scan cannot read keeps all.
    fn callee_keeps(&self, func: &FunctionRef, param_index: usize, operand: &TirExpr) -> bool {
        match self.kind(func) {
            Kind::ValueCopy => false,
            Kind::Declared => self.builtins.retain_specs(func).any(|r| {
                r.source == param_index
                    && (!r.elements || holds_identity(operand.type_id, self.type_table))
            }),
            Kind::HasBody => self.callee_escape(func, param_index, |pe| &pe.side),
            Kind::Opaque => true,
        }
    }

    /// Whether the argument at `param_index` reaches the callee uncopied, so
    /// passing a parameter there does not take it over.
    fn callee_borrows(&self, func: &FunctionRef, param_index: usize) -> bool {
        match self.kind(func) {
            Kind::ValueCopy => true,
            Kind::Declared => passes_through(self.builtins, func, param_index),
            Kind::HasBody => self.escape_of(func).confined_at(param_index),
            Kind::Opaque => false,
        }
    }
}

/// Parameter indices whose identity a value may carry.
type Taint = IndexSet<u32>;

fn compute_escape(ctx: &Ctx, body: &TirBlock, return_type: TypeId, pe: &mut ParamEscape) {
    let n_params = pe.ret.len() as u32;
    let taint = build_taint(ctx, body, n_params);
    let mut raiser = SinkWalker {
        ctx,
        taint: &taint,
        pe,
        return_type,
    };
    raiser.visit_block(body);
}

struct SinkWalker<'a> {
    ctx: &'a Ctx<'a>,
    taint: &'a IndexMap<u32, Taint>,
    pe: &'a mut ParamEscape,
    return_type: TypeId,
}

impl SinkWalker<'_> {
    fn raise_ret(&mut self, op: &TirExpr) {
        if carries_identity(self.return_type, self.ctx.type_table) {
            let t = taint_of(self.ctx, self.taint, op);
            raise(&t, &mut self.pe.ret);
        }
    }

    fn raise_side(&mut self, op: &TirExpr) {
        let t = taint_of(self.ctx, self.taint, op);
        raise(&t, &mut self.pe.side);
    }

    /// `op` lands in an owner the body holds. A reference or a plain value
    /// takes nothing over.
    fn raise_taken(&mut self, op: &TirExpr) {
        if needs_value_copy(op.type_id, self.ctx.type_table) {
            let t = taint_of(self.ctx, self.taint, op);
            raise(&t, &mut self.pe.taken);
        }
    }
}

impl TirRefVisitor for SinkWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Return { value: Some(op) }
            | TirStmtKind::Break {
                value: Some(op), ..
            } => self.raise_ret(op),
            TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
                self.raise_taken(value);
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::GlobalVarSet { value, .. } => self.raise_side(value),
            TirExprKind::Assign { target, value } => {
                if matches!(target.kind, TirExprKind::Local { .. }) {
                    self.raise_taken(value);
                } else {
                    self.raise_side(value);
                }
            }
            TirExprKind::CmRawCall { args, .. } | TirExprKind::IndirectCall { args, .. } => {
                for a in args {
                    self.raise_side(a);
                    self.raise_taken(a);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.raise_taken(&f.value);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                for el in elements {
                    self.raise_taken(el);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => self.raise_taken(p),
            TirExprKind::Call { func, args, .. } => {
                let operands: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                self.raise_call_sides(func, &operands);
                // A call that never returns is a failure path, where a copy
                // costs nothing that matters.
                if !self.ctx.type_table.is_never(expr.type_id) {
                    for (i, op) in operands.iter().enumerate() {
                        if !self.ctx.callee_borrows(func, i) {
                            self.raise_taken(op);
                        }
                    }
                }
            }
            // The body indexes locals of its own.
            TirExprKind::Closure { captures, .. } => {
                for index in capture_source_locals(captures) {
                    let t = self.taint.get(&index).cloned().unwrap_or_default();
                    raise(&t, &mut self.pe.side);
                }
                return;
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

impl SinkWalker<'_> {
    fn raise_call_sides(&mut self, func: &FunctionRef, operands: &[&TirExpr]) {
        for (i, op) in operands.iter().enumerate() {
            if self.ctx.callee_keeps(func, i, op) {
                self.raise_side(op);
            }
        }
    }
}

fn raise(t: &Taint, bits: &mut [bool]) {
    for &p in t {
        if let Some(b) = bits.get_mut(p as usize) {
            *b = true;
        }
    }
}

fn build_taint(ctx: &Ctx, body: &TirBlock, n_params: u32) -> IndexMap<u32, Taint> {
    let mut taint: IndexMap<u32, Taint> = IndexMap::default();
    for p in 0..n_params {
        taint.entry(p).or_default().insert(p);
    }

    let mut collector = BindingWalker {
        targets: Vec::new(),
    };
    collector.visit_block(body);

    let mut changed = true;
    while changed {
        changed = false;
        for (local, value) in &collector.targets {
            let t = taint_of(ctx, &taint, value);
            if merge_into(&mut taint, *local, t) {
                changed = true;
            }
        }
    }
    taint
}

struct BindingWalker {
    targets: Vec<(u32, TirExpr)>,
}

impl BindingWalker {
    /// A pattern binding carries what it was destructured out of.
    fn bind_pattern(&mut self, pattern: &TirPattern, value: &TirExpr) {
        let mut locals = IndexSet::default();
        collect_pattern_bindings(pattern, &mut locals);
        for local in locals {
            self.targets.push((local, value.clone()));
        }
    }
}

impl TirRefVisitor for BindingWalker {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => self.targets.push((*local_index, value.clone())),
            TirStmtKind::LetDestructure { pattern, value, .. } => self.bind_pattern(pattern, value),
            TirStmtKind::Expr(_)
            | TirStmtKind::Return { .. }
            | TirStmtKind::If { .. }
            | TirStmtKind::Loop { .. }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue
            | TirStmtKind::LabeledBlock { .. }
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::VariadicForOf { .. } => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        if matches!(expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        if let TirExprKind::Assign { target, value } = &expr.kind
            && let TirExprKind::Local { index, .. } = &target.kind
        {
            self.targets.push((*index, (**value).clone()));
        }
        if let TirExprKind::Match {
            expr: scrutinee,
            arms,
        } = &expr.kind
        {
            for arm in arms {
                self.bind_pattern(&arm.pattern, scrutinee);
            }
        }
        self.walk_expr(expr);
    }
}

fn merge_into(taint: &mut IndexMap<u32, Taint>, local: u32, t: Taint) -> bool {
    if t.is_empty() {
        return false;
    }
    let slot = taint.entry(local).or_default();
    let before = slot.len();
    slot.extend(t);
    slot.len() != before
}

fn taint_of(ctx: &Ctx, taint: &IndexMap<u32, Taint>, expr: &TirExpr) -> Taint {
    if !carries_identity(expr.type_id, ctx.type_table) {
        return Taint::default();
    }
    match &expr.kind {
        TirExprKind::Local { index, .. } => taint.get(index).cloned().unwrap_or_default(),
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. } => taint_of(ctx, taint, inner),
        TirExprKind::Binary { left, right, .. } => {
            union(taint_of(ctx, taint, left), taint_of(ctx, taint, right))
        }
        TirExprKind::Index { expr: inner, index } => {
            union(taint_of(ctx, taint, inner), taint_of(ctx, taint, index))
        }
        TirExprKind::StructLiteral { fields, .. } => {
            fields.iter().fold(Taint::default(), |acc, f| {
                union(acc, taint_of(ctx, taint, &f.value))
            })
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
            elements.iter().fold(Taint::default(), |acc, el| {
                union(acc, taint_of(ctx, taint, el))
            })
        }
        TirExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .map(|p| taint_of(ctx, taint, p))
            .unwrap_or_default(),
        TirExprKind::Call { func, args, .. } => {
            let operands: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
            call_result_taint(ctx, taint, func, &operands)
        }
        TirExprKind::Closure { captures, .. } => capture_source_locals(captures)
            .fold(Taint::default(), |acc, index| {
                union(acc, taint.get(&index).cloned().unwrap_or_default())
            }),
        _ => subtree_local_taint(taint, expr),
    }
}

fn call_result_taint(
    ctx: &Ctx,
    taint: &IndexMap<u32, Taint>,
    func: &FunctionRef,
    operands: &[&TirExpr],
) -> Taint {
    match ctx.kind(func) {
        Kind::ValueCopy => operands
            .first()
            .map(|op| taint_of(ctx, taint, op))
            .unwrap_or_default(),
        Kind::Declared | Kind::Opaque => operands.iter().fold(Taint::default(), |acc, op| {
            union(acc, taint_of(ctx, taint, op))
        }),
        Kind::HasBody => operands
            .iter()
            .enumerate()
            .filter(|(i, _)| ctx.callee_escape(func, *i, |pe| &pe.ret))
            .fold(Taint::default(), |acc, (_, op)| {
                union(acc, taint_of(ctx, taint, op))
            }),
    }
}

fn subtree_local_taint(taint: &IndexMap<u32, Taint>, expr: &TirExpr) -> Taint {
    struct Walk<'a> {
        taint: &'a IndexMap<u32, Taint>,
        acc: Taint,
    }
    impl TirRefVisitor for Walk<'_> {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if let TirExprKind::Closure { captures, .. } = &expr.kind {
                for index in capture_source_locals(captures) {
                    if let Some(t) = self.taint.get(&index) {
                        self.acc.extend(t.iter().copied());
                    }
                }
                return;
            }
            if let TirExprKind::Local { index, .. } = &expr.kind
                && let Some(t) = self.taint.get(index)
            {
                self.acc.extend(t.iter().copied());
            }
            self.walk_expr(expr);
        }
    }
    let mut w = Walk {
        taint,
        acc: Taint::default(),
    };
    w.visit_expr(expr);
    w.acc
}

/// A handler or `resume` re-enters this frame, so nothing it holds can be
/// tracked.
fn body_defies_model(body: &TirBlock) -> bool {
    struct Scan {
        found: bool,
    }
    impl TirRefVisitor for Scan {
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

fn carries_identity(type_id: TypeId, type_table: &TypeTable) -> bool {
    needs_value_copy(type_id, type_table)
        || matches!(
            type_table.get(type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        )
}

/// Whether what the reference `type_id` points to hands on an identity: an
/// array's elements, or any other referent itself. Plain data hands on nothing.
fn holds_identity(type_id: TypeId, type_table: &TypeTable) -> bool {
    let referent = type_table.peel_refs(type_id);
    match type_table.get(referent) {
        ResolvedType::BuiltinArray(element) => carries_identity(*element, type_table),
        _ => carries_identity(referent, type_table),
    }
}

fn union(mut a: Taint, b: Taint) -> Taint {
    a.extend(b);
    a
}
