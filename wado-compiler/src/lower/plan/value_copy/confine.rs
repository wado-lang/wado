//! Interprocedural confinement analysis over monomorphized TIR
//! (WEP 2026-05-21, value-copy client).
//!
//! A by-value parameter is *confined* when the callee neither returns it nor
//! leaks it to storage that outlives the call. Passing a still-live source into
//! a confined parameter needs no defensive copy: the callee either only reads it
//! (a non-`mut` parameter safely shares the caller's storage) or copies it on
//! entry (a `mut` parameter takes its own copy), so the caller's value is never
//! observably perturbed.
//!
//! This is the caller-side, single-phase replacement for `optimize::escape`'s
//! `param_escapes`. The fold skips the copy precisely rather than inserting it
//! everywhere and recovering it in a later elision pass (the now-deleted
//! `optimize::value_copy_elide`).
//!
//! It runs before the boxing plan (`lower::plan::plan`), so `&mut T` / `&T`
//! references are still distinguishable — boxing collapses both onto `Box<T>`.
//! The `$value_copy$T` wraps do not exist yet either (the fold inserts them), so
//! unlike `optimize::escape` this analysis never has to see through a copy
//! helper.
//!
//! Escape is tracked in two channels per parameter, mirroring `optimize::escape`:
//!
//! - *return* — the parameter flows into a value the function can return. The
//!   result then aliases the argument, so a returned parameter is not confined.
//! - *side* — the parameter is written to storage outliving the call (a global,
//!   a field/element/pointee write, a closure capture, or an argument to another
//!   call that side-escapes it).
//!
//! Soundness is one-directional: the analysis must *over*-approximate escape. A
//! parameter wrongly reported confined would drop a live copy and alias a leaked
//! value — a miscompile. So every rule only adds escape, unmodelled constructs
//! default to escaping, and a function whose body defies the model (a closure,
//! effect handler, or `resume` that can re-observe a captured parameter) has all
//! its parameters marked escaping.

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

/// Per-parameter confinement bits for every function the fold may call. A
/// missing entry (or out-of-range index) answers "not confined" — the
/// conservative default that keeps a copy.
pub struct ConfinedParams {
    map: IndexMap<FunctionId, Vec<bool>>,
}

impl ConfinedParams {
    /// Whether parameter `param_index` (absolute: the receiver of a method is 0)
    /// of `func` is confined — neither returned nor leaked, so a caller passing a
    /// still-live value into it needs no defensive copy.
    pub fn is_confined(&self, func: &FunctionRef, param_index: usize) -> bool {
        self.map
            .get(&func_key(&func.module_source, &func.name))
            .and_then(|bits| bits.get(param_index))
            .copied()
            .unwrap_or(false)
    }
}

/// The two escape channels of one parameter. A parameter is confined iff neither
/// channel is raised.
#[derive(Clone, Default, PartialEq)]
struct ParamEscape {
    ret: Vec<bool>,
    side: Vec<bool>,
}

/// Classification of a callee, resolved once per function.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A core / wasm-asset intrinsic — retains no reference to a by-value arg.
    Builtin,
    /// A `$value_copy$T` helper — transparent (returns a fresh clone).
    ValueCopy,
    /// A normal function with a body — analyzed via its escape channels.
    HasBody,
    /// Extern / bodyless non-builtin (CM import, dispatch stub) — opaque.
    Opaque,
}

/// Compute per-parameter confinement for every function in `project`.
pub fn compute_confined_params(project: &FlatPackage) -> ConfinedParams {
    let type_table = project.type_table.borrow();
    let kinds = classify_functions(project);

    // Least fixpoint: start every channel optimistic (false) and only raise to
    // `true`, so the monotone rules converge.
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
            // A body that captures parameters into a closure, an effect handler,
            // or a `resume` can re-observe them past the call: mark every
            // parameter escaping and skip the precise walk.
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

/// Read-only context shared by the taint and sink walks of one function.
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

    /// Whether argument at `param_index` flows into `func`'s return value.
    fn callee_ret(&self, func: &FunctionRef, param_index: usize) -> bool {
        match self.funcs.get(&func_key(&func.module_source, &func.name)) {
            Some(pe) => pe.ret.get(param_index).copied().unwrap_or(true),
            None => true,
        }
    }

    /// Whether `func` leaks argument at `param_index` to lasting storage.
    fn callee_side(&self, func: &FunctionRef, param_index: usize) -> bool {
        match self.funcs.get(&func_key(&func.module_source, &func.name)) {
            Some(pe) => pe.side.get(param_index).copied().unwrap_or(true),
            None => true,
        }
    }
}

/// The per-parameter taint set: which parameters a value may carry the identity
/// of. Parameters are the seed roots, so a set of parameter indices names
/// exactly the roots whose escape a reaching sink implies.
type Taint = IndexSet<u32>;

/// Raise the return / side channels for every parameter whose value reaches a
/// sink. Runs the intra-function taint fixpoint first, then the sink walk.
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

/// Walks the body raising escape channels: `return` / `break` / `task return`
/// values feed the return channel; global sets, non-local writes, closure
/// captures, and call arguments to side-escaping callee parameters feed the side
/// channel.
struct SinkWalker<'a> {
    ctx: &'a Ctx<'a>,
    taint: &'a IndexMap<u32, Taint>,
    pe: &'a mut ParamEscape,
    return_type: TypeId,
}

impl SinkWalker<'_> {
    fn raise_ret(&mut self, op: &TirExpr) {
        // A returned aliasable value taints the return channel; a primitive
        // return carries no parameter identity.
        if needs_value_copy(self.return_type, self.ctx.type_table)
            || matches!(
                self.ctx.type_table.get(self.return_type),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        {
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
            // A write into anything other than a bare local (a field, element,
            // or pointee) may reach caller-visible storage.
            TirExprKind::Assign { target, value } => {
                if !matches!(target.kind, TirExprKind::Local { .. }) {
                    self.raise_side(value);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for a in args {
                    self.raise_side(a);
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
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
    /// Side-escape sinks for a resolved call. An argument that only flows to the
    /// callee's *return* is not a side sink — that path is captured as result
    /// taint in [`taint_of`].
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

    /// Builtin side rule: only a store builtin leaks, and only its by-value
    /// aggregate elements, into a `&mut` aggregate the caller retains.
    fn raise_builtin_sides(&mut self, operands: &[&TirExpr]) {
        let has_mut_aggregate = operands
            .iter()
            .any(|op| is_mut_ref(op, self.ctx.type_table));
        if !has_mut_aggregate {
            return;
        }
        for op in operands {
            // Shared-ref / `&mut` operands are the container and cursors, not
            // stored elements; only a by-value aggregate operand can be stored
            // by identity.
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

/// Intra-function taint fixpoint: propagate parameter identity across `let` /
/// assignment bindings until stable (loops need more than one forward pass).
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

/// Collects every `(local, value)` a `let` or a bare `local = value` assignment
/// binds, so the taint fixpoint can propagate through them.
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

/// The set of parameters whose identity `expr`'s value may carry.
fn taint_of(ctx: &Ctx, taint: &IndexMap<u32, Taint>, expr: &TirExpr) -> Taint {
    // A value that cannot alias a source (a primitive result — a length, tag,
    // index, or byte) carries no parameter identity, even when projected from a
    // tainted aggregate (`s.used`).
    if !carries_identity(expr.type_id, ctx.type_table) {
        return Taint::default();
    }
    match &expr.kind {
        TirExprKind::Local { index, .. } => taint.get(index).cloned().unwrap_or_default(),
        // Transparent / projection: the value shares its inner's identity.
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
        // Control flow used as a value, and anything unmodelled: over-approximate
        // by taking every parameter read anywhere in the subtree. Sound (more
        // taint only ever means more escape).
        _ => subtree_local_taint(taint, expr),
    }
}

/// The identity a call's result may carry.
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

/// Union of the taint of every `Local` read anywhere in `expr`'s subtree.
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

/// Whether the body contains a construct that can re-observe a parameter outside
/// the call frame: a closure capture, an effect handler, or a `resume`.
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

/// Whether a value of `type_id` can carry another value's identity: an aggregate
/// that shares GC storage when aliased, or a reference to one.
fn carries_identity(type_id: TypeId, type_table: &TypeTable) -> bool {
    needs_value_copy(type_id, type_table)
        || matches!(
            type_table.get(type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        )
}

/// Whether a value is `&mut T`. This analysis runs before the boxing plan
/// collapses `&mut T` / `&T` onto `Box<T>`, so the reference types and the
/// `&`/`&mut` operators are still distinguishable.
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

/// Whether a value is any reference (`&T` / `&mut T`).
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
