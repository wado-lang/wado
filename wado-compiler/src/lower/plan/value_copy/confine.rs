//! Interprocedural confinement analysis over pre-boxing TIR (WEP 2026-05-21).
//!
//! A by-value parameter is *confined* when the callee neither returns it nor
//! leaks it to storage outliving the call, so a caller passing a still-live
//! value into it needs no defensive copy. Two escape channels per parameter —
//! `ret` (flows into a returned value) and `side` (written to lasting storage) —
//! reach a least fixpoint; a parameter is confined iff neither is raised. The
//! analysis over-approximates escape: unmodelled constructs and functions with a
//! closure / handler / `resume` body mark every parameter escaping.

use super::needs_value_copy;
use super::ownership::func_key;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::FunctionId;
use crate::tir::{
    FunctionKind, FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Per-parameter confinement bits. A missing entry answers "not confined".
pub struct ConfinedParams {
    map: IndexMap<FunctionId, Vec<bool>>,
}

impl ConfinedParams {
    pub fn is_confined(&self, func: &FunctionRef, param_index: usize) -> bool {
        self.map
            .get(&func_key(&func.module_source, &func.name))
            .and_then(|bits| bits.get(param_index))
            .copied()
            .unwrap_or(false)
    }
}

#[derive(Clone, Default, PartialEq)]
struct ParamEscape {
    ret: Vec<bool>,
    side: Vec<bool>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Builtin,
    ValueCopy,
    HasBody,
    Opaque,
}

pub fn compute_confined_params(project: &FlatPackage) -> ConfinedParams {
    let type_table = project.type_table.borrow();
    let kinds = classify_functions(project);

    let mut funcs: IndexMap<FunctionId, ParamEscape> = IndexMap::default();
    for func in &project.functions {
        let func = func.borrow();
        if func.body.is_some() {
            let n = func.params.len();
            funcs.insert(
                func_key(&func.module_source, &func.name),
                ParamEscape {
                    ret: vec![false; n],
                    side: vec![false; n],
                },
            );
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for func in &project.functions {
            let func = func.borrow();
            let Some(body) = &func.body else {
                continue;
            };
            let key = func_key(&func.module_source, &func.name);
            let ctx = Ctx {
                type_table: &type_table,
                kinds: &kinds,
                funcs: &funcs,
            };
            let mut pe = funcs[&key].clone();
            if body_defies_model(body) {
                for b in pe.ret.iter_mut().chain(pe.side.iter_mut()) {
                    *b = true;
                }
            } else {
                compute_escape(&ctx, body, func.return_type, &mut pe);
            }
            if pe != funcs[&key] {
                funcs.insert(key, pe);
                changed = true;
            }
        }
    }

    let map = funcs
        .into_iter()
        .map(|(key, pe)| {
            let bits = pe
                .ret
                .iter()
                .zip(&pe.side)
                .map(|(r, s)| !*r && !*s)
                .collect();
            (key, bits)
        })
        .collect();
    ConfinedParams { map }
}

fn classify_functions(project: &FlatPackage) -> IndexMap<FunctionId, Kind> {
    let mut kinds = IndexMap::default();
    for func in &project.functions {
        let func = func.borrow();
        let key = func_key(&func.module_source, &func.name);
        let kind = if matches!(func.kind, FunctionKind::ValueCopy { .. }) {
            Kind::ValueCopy
        } else if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
            Kind::Builtin
        } else if func.body.is_some() {
            Kind::HasBody
        } else {
            Kind::Opaque
        };
        kinds.insert(key, kind);
    }
    kinds
}

struct Ctx<'a> {
    type_table: &'a TypeTable,
    kinds: &'a IndexMap<FunctionId, Kind>,
    funcs: &'a IndexMap<FunctionId, ParamEscape>,
}

impl Ctx<'_> {
    fn kind(&self, func: &FunctionRef) -> Kind {
        self.kinds
            .get(&func_key(&func.module_source, &func.name))
            .copied()
            .unwrap_or(Kind::Opaque)
    }

    fn callee_ret(&self, func: &FunctionRef, param_index: usize) -> bool {
        match self.funcs.get(&func_key(&func.module_source, &func.name)) {
            Some(pe) => pe.ret.get(param_index).copied().unwrap_or(true),
            None => true,
        }
    }

    fn callee_side(&self, func: &FunctionRef, param_index: usize) -> bool {
        match self.funcs.get(&func_key(&func.module_source, &func.name)) {
            Some(pe) => pe.side.get(param_index).copied().unwrap_or(true),
            None => true,
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
}

impl TirRefVisitor for SinkWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Return { value: Some(op) }
            | TirStmtKind::Break {
                value: Some(op), ..
            } => self.raise_ret(op),
            TirStmtKind::TaskReturn { value } => self.raise_ret(value),
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::GlobalVarSet { value, .. } => self.raise_side(value),
            TirExprKind::Assign { target, value } => {
                if !matches!(target.kind, TirExprKind::Local { .. }) {
                    self.raise_side(value);
                }
            }
            TirExprKind::CmRawCall { args, .. } | TirExprKind::IndirectCall { args, .. } => {
                for a in args {
                    self.raise_side(a);
                }
            }
            TirExprKind::Call { func, args, .. } => {
                let operands: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                self.raise_call_sides(func, &operands);
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                let operands: Vec<&TirExpr> = std::iter::once(receiver.as_ref())
                    .chain(args.iter().map(|a| &a.expr))
                    .collect();
                self.raise_call_sides(func, &operands);
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

impl SinkWalker<'_> {
    fn raise_call_sides(&mut self, func: &FunctionRef, operands: &[&TirExpr]) {
        match self.ctx.kind(func) {
            Kind::ValueCopy => {}
            Kind::Builtin => self.raise_builtin_sides(operands),
            Kind::Opaque => {
                for op in operands {
                    self.raise_side(op);
                }
            }
            Kind::HasBody => {
                for (i, op) in operands.iter().enumerate() {
                    if self.ctx.callee_side(func, i) {
                        self.raise_side(op);
                    }
                }
            }
        }
    }

    /// A store builtin leaks its by-value aggregate operands into a `&mut`
    /// aggregate the caller retains.
    fn raise_builtin_sides(&mut self, operands: &[&TirExpr]) {
        let has_mut_aggregate = operands
            .iter()
            .any(|op| is_mut_ref(op, self.ctx.type_table));
        if !has_mut_aggregate {
            return;
        }
        for op in operands {
            if is_ref_typed(op, self.ctx.type_table) {
                continue;
            }
            if needs_value_copy(op.type_id, self.ctx.type_table) {
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

    let mut changed = true;
    while changed {
        changed = false;
        let mut collector = BindingWalker {
            targets: Vec::new(),
        };
        collector.visit_block(body);
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

impl TirRefVisitor for BindingWalker {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
        {
            self.targets.push((*local_index, value.clone()));
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Assign { target, value } = &expr.kind
            && let TirExprKind::Local { index, .. } = &target.kind
        {
            self.targets.push((*index, (**value).clone()));
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
        TirExprKind::TupleLiteral { elements } => {
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
        TirExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            let operands: Vec<&TirExpr> = std::iter::once(receiver.as_ref())
                .chain(args.iter().map(|a| &a.expr))
                .collect();
            call_result_taint(ctx, taint, func, &operands)
        }
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
        Kind::Builtin | Kind::Opaque => operands.iter().fold(Taint::default(), |acc, op| {
            union(acc, taint_of(ctx, taint, op))
        }),
        Kind::HasBody => operands
            .iter()
            .enumerate()
            .filter(|(i, _)| ctx.callee_ret(func, *i))
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

fn body_defies_model(body: &TirBlock) -> bool {
    struct Scan {
        found: bool,
    }
    impl TirRefVisitor for Scan {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if matches!(
                expr.kind,
                TirExprKind::Closure { .. }
                    | TirExprKind::WithHandler { .. }
                    | TirExprKind::Resume { .. }
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

fn is_mut_ref(expr: &TirExpr, type_table: &TypeTable) -> bool {
    matches!(type_table.get(expr.type_id), ResolvedType::MutRef(_))
        || matches!(
            &expr.kind,
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                ..
            }
        )
}

fn is_ref_typed(expr: &TirExpr, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(expr.type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    ) || matches!(
        &expr.kind,
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            ..
        }
    )
}

fn union(mut a: Taint, b: Taint) -> Taint {
    a.extend(b);
    a
}
