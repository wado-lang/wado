//! Interprocedural may-escape analysis over monomorphized NIR.
//!
//! For each function parameter it answers: can the parameter's value be observed
//! outside the call frame? A parameter that cannot is a *confined* read: a caller
//! passing an argument to it need not deep-copy, because the callee neither
//! mutates it nor keeps an alias past the call.
//! [`ValueCopyElideRule`](super::value_copy_elide) reads this to strip such
//! call-argument `$value_copy$T` wrappers.
//!
//! Escape splits into two channels, tracked separately per parameter:
//!
//! - *return* — the parameter flows into the call's result. This does not by
//!   itself leave the caller's frame; the value lands in the result, whose own
//!   fate decides. So it feeds result taint, not an argument sink. (For elision
//!   it still counts: the result aliases the argument, so a returned parameter
//!   is not confined.)
//! - *side* — the parameter is written to storage that outlives the call: a
//!   global, an out-parameter pointee, a store, a closure capture, or another
//!   call that side-escapes it. This leaks the argument's identity regardless of
//!   the result, so it *is* an argument sink.
//!
//! Soundness is one-directional: the result must over-approximate the true
//! escape set — a parameter wrongly reported confined would let elision drop a
//! live copy and alias a leaked value (a miscompile). Every rule only adds
//! escape, unmodeled constructs default to escaping, and `$value_copy$T`
//! wrappers are transparent (elision may remove them). Only values whose type
//! can alias a source (aggregates and references) carry taint; a primitive
//! projection like `s.used` carries none.
//!
//! Builtins retain nothing, so their reference and by-value arguments stay
//! confined — except a store builtin (`array_set<T>`, …), which writes a
//! by-value aggregate element into a `&mut` aggregate the caller still holds
//! (a side escape). Their result is tainted by every argument, covering identity
//! builtins (`ref.as_non_null`, `array.new_fixed`).

use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::needs_value_copy;
use crate::nir::FuncId;
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// The two escape channels of one parameter.
#[derive(Clone, Default, PartialEq)]
struct ParamEscape {
    /// Flows into the function's return value.
    ret: Vec<bool>,
    /// Leaked to storage outliving the call (global, out-parameter, capture, …).
    side: Vec<bool>,
}

/// Per-parameter escape channels for every function with a body. A missing entry
/// (extern / builtin / not-yet-analyzed) answers "escapes" — the conservative
/// default.
#[derive(Default)]
pub(super) struct EscapeMap {
    funcs: IndexMap<FuncId, ParamEscape>,
    /// Functions whose result is a genuinely fresh value — it aliases no
    /// argument, receiver, global, or capture, so a copy of the result is a
    /// no-op. Lets the elider recover the copies inserted for call results.
    fresh_result: IndexSet<FuncId>,
}

impl EscapeMap {
    /// Whether parameter `param_index` (absolute: `self` is 0 for methods) of
    /// `func` may be observed outside the call — through either channel. Unknown
    /// functions and out-of-range indices default to `true`.
    pub(super) fn param_escapes(&self, func: FuncId, param_index: usize) -> bool {
        match self.funcs.get(&func) {
            Some(pe) => {
                pe.ret.get(param_index).copied().unwrap_or(true)
                    || pe.side.get(param_index).copied().unwrap_or(true)
            }
            None => true,
        }
    }

    /// Whether a call to `func` yields a genuinely fresh value, so copying its
    /// result is a no-op. Unknown functions default to `false`.
    pub(super) fn returns_fresh(&self, func: FuncId) -> bool {
        self.fresh_result.contains(&func)
    }

    /// Whether `e` is a fresh *rvalue* — it produces a new value that aliases
    /// nothing live, so `$value_copy$T(e)` is a no-op and can be stripped in any
    /// position. A bare local read is *not* fresh here (it aliases a live local);
    /// only constructions, copy clones, fresh-returning calls, and matches whose
    /// arms destructure a fresh scrutinee qualify.
    pub(super) fn rvalue_is_fresh(&self, body: &Body, e: ExprId, type_table: &TypeTable) -> bool {
        expr_is_fresh(body, e, &IndexSet::default(), type_table, &|id| {
            self.returns_fresh(id)
        })
    }
}

/// Classification of a callee, resolved once per function id.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// `core:builtin` / wasm-asset intrinsic — retains no GC reference.
    Builtin,
    /// A `$value_copy$T` helper — transparent (returns a fresh clone).
    ValueCopy,
    /// A normal function with a body — analyzed via its escape channels.
    HasBody,
    /// Extern / bodyless non-builtin (CM import, dispatch stub) — opaque.
    Opaque,
}

/// Compute per-parameter escape channels for every function in `project`.
pub(super) fn analyze_param_escape(
    project: &NirPackage,
    value_copy_ids: &IndexSet<FuncId>,
) -> EscapeMap {
    let type_table = project.type_table.borrow();
    let kinds = classify_functions(project, value_copy_ids);

    // Least fixpoint: start every channel optimistic (false) and only raise to
    // `true`, so the monotone rules converge.
    let mut funcs: IndexMap<FuncId, ParamEscape> = IndexMap::default();
    for func in &project.functions {
        let func = func.borrow();
        if func.body.is_some()
            && let Some(id) = func.id
        {
            let n = func.params.len();
            funcs.insert(
                id,
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
            let (Some(id), Some(body)) = (func.id, func.body.as_ref()) else {
                continue;
            };
            let ctx = Ctx {
                type_table: &type_table,
                kinds: &kinds,
                funcs: &funcs,
            };
            let mut pe = funcs[&id].clone();
            compute_escape(&ctx, body, func.return_type, &mut pe);
            if pe != funcs[&id] {
                funcs.insert(id, pe);
                changed = true;
            }
        }
    }

    // Least fixpoint over "the function returns a fresh value": a copy helper
    // clones; a builtin is fresh unless it reads storage aliased into a
    // reference argument (see `builtin_result_is_fresh`); a body function is
    // fresh when every return operand is fresh given the freshness of the callees
    // it returns. Start with the always-fresh callees and grow.
    let mut fresh_result: IndexSet<FuncId> = IndexSet::default();
    for func in &project.functions {
        let func = func.borrow();
        let Some(id) = func.id else { continue };
        let fresh = match kinds.get(&id) {
            Some(Kind::ValueCopy) => true,
            Some(Kind::Builtin) => builtin_result_is_fresh(&func.name),
            _ => false,
        };
        if fresh {
            fresh_result.insert(id);
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for func in &project.functions {
            let func = func.borrow();
            let (Some(id), Some(body)) = (func.id, func.body.as_ref()) else {
                continue;
            };
            if fresh_result.contains(&id) {
                continue;
            }
            let call_fresh = |c: FuncId| fresh_result.contains(&c);
            let fl = compute_fresh_locals(body, func.params.len() as u32, &type_table, &call_fresh);
            if function_result_is_fresh(body, &fl, &type_table, &call_fresh) {
                fresh_result.insert(id);
                changed = true;
            }
        }
    }

    EscapeMap {
        funcs,
        fresh_result,
    }
}

fn classify_functions(
    project: &NirPackage,
    value_copy_ids: &IndexSet<FuncId>,
) -> IndexMap<FuncId, Kind> {
    let mut kinds = IndexMap::default();
    for func in &project.functions {
        let func = func.borrow();
        let Some(id) = func.id else { continue };
        let kind = if value_copy_ids.contains(&id) {
            Kind::ValueCopy
        } else if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
            Kind::Builtin
        } else if func.body.is_some() {
            Kind::HasBody
        } else {
            Kind::Opaque
        };
        kinds.insert(id, kind);
    }
    kinds
}

/// Read-only context shared by the taint and sink walks of one function.
struct Ctx<'a> {
    type_table: &'a TypeTable,
    kinds: &'a IndexMap<FuncId, Kind>,
    funcs: &'a IndexMap<FuncId, ParamEscape>,
}

impl Ctx<'_> {
    fn kind(&self, id: FuncId) -> Kind {
        self.kinds.get(&id).copied().unwrap_or(Kind::Opaque)
    }

    /// Whether argument at `param_index` flows into `id`'s return value.
    fn callee_ret(&self, id: FuncId, param_index: usize) -> bool {
        match self.funcs.get(&id) {
            Some(pe) => pe.ret.get(param_index).copied().unwrap_or(true),
            None => true,
        }
    }

    /// Whether `id` leaks argument at `param_index` to lasting storage.
    fn callee_side(&self, id: FuncId, param_index: usize) -> bool {
        match self.funcs.get(&id) {
            Some(pe) => pe.side.get(param_index).copied().unwrap_or(true),
            None => true,
        }
    }
}

/// The per-parameter taint set: which parameters a value may carry the identity
/// of. Parameters are the seed roots (`0..n_params`), so a set of parameter
/// indices names exactly the roots whose escape a reaching sink implies.
type Taint = IndexSet<u32>;

/// Raise the return / side channels for every parameter whose value reaches the
/// corresponding sink. Runs the intra-function taint fixpoint first, then the
/// sink walk.
fn compute_escape(ctx: &Ctx, body: &Body, return_type: TypeId, pe: &mut ParamEscape) {
    let n_params = pe.ret.len() as u32;
    let taint = build_taint(ctx, body, n_params);

    let raise = |t: &Taint, bits: &mut [bool]| {
        for &p in t {
            if let Some(b) = bits.get_mut(p as usize) {
                *b = true;
            }
        }
    };

    // Defensive tail-return handling: if the root block ends in a bare value
    // expression (rather than an explicit `Return`) and the result type is an
    // aliasable value, treat it as a return sink.
    if needs_value_copy(return_type, ctx.type_table)
        && let Some(&last) = body.blocks[body.root].stmts.last()
        && let StmtKind::Expr(op) = &body.stmts[last].kind
    {
        raise(&taint_of(ctx, body, &taint, *op), &mut pe.ret);
    }

    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Stmt(s) => match &body.stmts[s].kind {
                // The break value of a labeled block can become the function
                // result; treat as a return sink (conservative).
                StmtKind::Return { value: Some(op) }
                | StmtKind::Break {
                    value: Some(op), ..
                } => {
                    raise(&taint_of(ctx, body, &taint, *op), &mut pe.ret);
                }
                _ => {}
            },
            NodeRef::Expr(e) => {
                side_expr(ctx, body, &taint, e, &mut |t| raise(t, &mut pe.side));
            }
            _ => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// Apply the *side*-escape sinks carried by a single expression node.
fn side_expr(
    ctx: &Ctx,
    body: &Body,
    taint: &IndexMap<u32, Taint>,
    e: ExprId,
    side: &mut impl FnMut(&Taint),
) {
    match &body.exprs[e].kind {
        ExprKind::GlobalVarSet { value, .. } => {
            side(&taint_of(ctx, body, taint, *value));
        }
        // A write into anything other than a bare local (a field, element, or
        // pointee) may reach caller-visible storage; treat the stored value as
        // escaping.
        ExprKind::Assign { target, value } => {
            if !matches!(body.exprs[*target].kind, ExprKind::Local { .. }) {
                side(&taint_of(ctx, body, taint, *value));
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            side(&taint_of(ctx, body, taint, *functor));
        }
        ExprKind::CmRawCall { args, .. } => {
            for a in args {
                side(&taint_of(ctx, body, taint, *a));
            }
        }
        ExprKind::IndirectCall { args, .. } => {
            for a in args {
                side(&taint_of(ctx, body, taint, *a));
            }
        }
        ExprKind::Call { func_id, args, .. } => {
            let ops = args.iter().map(|a| a.expr);
            side_call(ctx, body, taint, *func_id, None, ops, side);
        }
        ExprKind::MethodCall {
            func_id,
            receiver,
            args,
            ..
        } => {
            let ops = args.iter().map(|a| a.expr);
            side_call(ctx, body, taint, *func_id, Some(*receiver), ops, side);
        }
        _ => {}
    }
}

/// Side-escape sinks for a resolved call. `receiver` present ⇒ a method, so the
/// receiver is absolute parameter 0 and the `i`-th argument is parameter `i+1`.
/// An argument that only flows to the callee's *return* is not a side sink here
/// — that path is captured as result taint in [`taint_of`].
fn side_call(
    ctx: &Ctx,
    body: &Body,
    taint: &IndexMap<u32, Taint>,
    func_id: FuncId,
    receiver: Option<Operand>,
    args: impl Iterator<Item = Operand>,
    side: &mut impl FnMut(&Taint),
) {
    let operands: Vec<Operand> = receiver.into_iter().chain(args).collect();
    match ctx.kind(func_id) {
        Kind::ValueCopy => {}
        Kind::Builtin => builtin_side(ctx, body, taint, &operands, side),
        Kind::Opaque => {
            // Opaque callee: every argument may be retained.
            for op in operands {
                side(&taint_of(ctx, body, taint, op));
            }
        }
        Kind::HasBody => {
            for (i, op) in operands.into_iter().enumerate() {
                if ctx.callee_side(func_id, i) {
                    side(&taint_of(ctx, body, taint, op));
                }
            }
        }
    }
}

/// Builtin side rule: only a store builtin leaks, and only its by-value
/// aggregate elements, into a `&mut` aggregate the caller retains.
fn builtin_side(
    ctx: &Ctx,
    body: &Body,
    taint: &IndexMap<u32, Taint>,
    operands: &[Operand],
    side: &mut impl FnMut(&Taint),
) {
    let has_mut_aggregate = operands.iter().any(|op| {
        op.as_expr()
            .is_some_and(|e| is_mut_ref(body, e, ctx.type_table))
    });
    if !has_mut_aggregate {
        return;
    }
    for &op in operands {
        let Some(e) = op.as_expr() else { continue };
        // Shared-ref / `&mut` operands are the container and cursors, not stored
        // elements; only a by-value aggregate operand can be stored by identity.
        if is_ref_typed(body, e, ctx.type_table) {
            continue;
        }
        if needs_value_copy(body.exprs[e].type_id, ctx.type_table) {
            side(&taint_of(ctx, body, taint, op));
        }
    }
}

/// Intra-function taint fixpoint: propagate parameter identity across `let` /
/// assignment bindings until stable (loops need more than one forward pass).
fn build_taint(ctx: &Ctx, body: &Body, n_params: u32) -> IndexMap<u32, Taint> {
    let mut taint: IndexMap<u32, Taint> = IndexMap::default();
    for p in 0..n_params {
        taint.entry(p).or_default().insert(p);
    }

    let mut changed = true;
    while changed {
        changed = false;
        let mut stack = vec![NodeRef::Block(body.root)];
        while let Some(node) = stack.pop() {
            if let NodeRef::Stmt(s) = node
                && let Some((local, value)) = binding_target(body, s)
            {
                let t = taint_of(ctx, body, &taint, value);
                if merge_into(&mut taint, local, t) {
                    changed = true;
                }
            }
            body.for_each_child(node, |c| stack.push(c));
        }
    }
    taint
}

/// The `(local, value)` a statement binds — a `let` or a top-level
/// `local = value` assignment — if it targets a bare local.
fn binding_target(body: &Body, s: StmtId) -> Option<(u32, Operand)> {
    match &body.stmts[s].kind {
        StmtKind::Let {
            local_index, value, ..
        } => Some((*local_index, *value)),
        StmtKind::Expr(Operand::Expr(e)) => {
            if let ExprKind::Assign { target, value } = &body.exprs[*e].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
            {
                Some((*index, *value))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Merge `t` into `taint[local]`, returning whether it grew.
fn merge_into(taint: &mut IndexMap<u32, Taint>, local: u32, t: Taint) -> bool {
    if t.is_empty() {
        return false;
    }
    let slot = taint.entry(local).or_default();
    let before = slot.len();
    slot.extend(t);
    slot.len() != before
}

/// The set of parameters whose identity `op`'s value may carry.
fn taint_of(ctx: &Ctx, body: &Body, taint: &IndexMap<u32, Taint>, op: Operand) -> Taint {
    let Some(e) = op.as_expr() else {
        return Taint::default();
    };
    taint_of_expr(ctx, body, taint, e)
}

fn taint_of_expr(ctx: &Ctx, body: &Body, taint: &IndexMap<u32, Taint>, e: ExprId) -> Taint {
    // A value that cannot alias a source (a primitive result — a length, tag,
    // index, or byte) carries no parameter identity, even when projected from a
    // tainted aggregate (`s.used`). Only aggregate and reference results can.
    if !carries_identity(body.exprs[e].type_id, ctx.type_table) {
        return Taint::default();
    }
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => taint.get(index).cloned().unwrap_or_default(),
        // Transparent / projection: the value shares its inner's identity.
        ExprKind::Unary { expr, .. }
        | ExprKind::Cast { expr, .. }
        | ExprKind::FieldAccess { expr, .. }
        | ExprKind::VariantPayload { expr, .. }
        | ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. } => taint_of(ctx, body, taint, *expr),
        ExprKind::Binary { left, right, .. } => union(
            taint_of(ctx, body, taint, *left),
            taint_of(ctx, body, taint, *right),
        ),
        ExprKind::Index { expr, index } => union(
            taint_of(ctx, body, taint, *expr),
            taint_of(ctx, body, taint, *index),
        ),
        ExprKind::StructLiteral { fields, .. } => fields.iter().fold(Taint::default(), |acc, f| {
            union(acc, taint_of(ctx, body, taint, f.value))
        }),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.iter().fold(Taint::default(), |acc, el| {
                union(acc, taint_of(ctx, body, taint, *el))
            })
        }
        ExprKind::VariantConstruct { payload, .. } => payload
            .map(|p| taint_of(ctx, body, taint, p))
            .unwrap_or_default(),
        ExprKind::Call { func_id, args, .. } => {
            let ops = args.iter().map(|a| a.expr);
            call_result_taint(ctx, body, taint, *func_id, None, ops)
        }
        ExprKind::MethodCall {
            func_id,
            receiver,
            args,
            ..
        } => {
            let ops = args.iter().map(|a| a.expr);
            call_result_taint(ctx, body, taint, *func_id, Some(*receiver), ops)
        }
        // Control flow used as a value, and anything unmodeled: over-approximate
        // by taking every parameter read anywhere in the subtree. Sound (more
        // taint only ever means more escape).
        _ => subtree_local_taint(body, taint, e),
    }
}

/// The identity a call's result may carry: for a body callee, the argument at
/// each *return*-escaping parameter position; for a builtin or opaque callee,
/// every argument (covers identity builtins and unknown returns); for a copy
/// helper, its argument (transparency).
fn call_result_taint(
    ctx: &Ctx,
    body: &Body,
    taint: &IndexMap<u32, Taint>,
    func_id: FuncId,
    receiver: Option<Operand>,
    args: impl Iterator<Item = Operand>,
) -> Taint {
    let operands: Vec<Operand> = receiver.into_iter().chain(args).collect();
    match ctx.kind(func_id) {
        Kind::ValueCopy => operands
            .first()
            .map(|op| taint_of(ctx, body, taint, *op))
            .unwrap_or_default(),
        Kind::Builtin | Kind::Opaque => operands.iter().fold(Taint::default(), |acc, op| {
            union(acc, taint_of(ctx, body, taint, *op))
        }),
        Kind::HasBody => operands
            .into_iter()
            .enumerate()
            .filter(|(i, _)| ctx.callee_ret(func_id, *i))
            .fold(Taint::default(), |acc, (_, op)| {
                union(acc, taint_of(ctx, body, taint, op))
            }),
    }
}

/// Union of the taint of every `Local` read anywhere in `e`'s subtree.
fn subtree_local_taint(body: &Body, taint: &IndexMap<u32, Taint>, e: ExprId) -> Taint {
    let mut acc = Taint::default();
    let mut stack = vec![NodeRef::Expr(e)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Local { index, .. } = &body.exprs[id].kind
            && let Some(t) = taint.get(index)
        {
            acc.extend(t.iter().copied());
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    acc
}

// ──────────────────────────────────────────────────────────────────────────────
// Freshness analysis
// ──────────────────────────────────────────────────────────────────────────────

/// The locals of `body` (first `n_params` are parameters) that provably hold a
/// uniquely owned, fresh value. A parameter never qualifies — it aliases its
/// argument. Every other local qualifies only if it has at least one binding and
/// every value bound to it is fresh; mutation in place (`x.push(..)`) does not
/// rebind `x`, so it keeps a fresh value fresh.
fn compute_fresh_locals(
    body: &Body,
    n_params: u32,
    type_table: &TypeTable,
    call_fresh: &dyn Fn(FuncId) -> bool,
) -> IndexSet<u32> {
    let mut bindings: IndexMap<u32, Vec<Operand>> = IndexMap::default();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let Some((local, value)) = binding_target(body, s)
            && local >= n_params
        {
            bindings.entry(local).or_default().push(value);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    // Optimistic: every bound non-parameter local is fresh; remove any whose a
    // binding value is not fresh, to a fixpoint (a value may reference another
    // local whose freshness is still shrinking).
    let mut fresh: IndexSet<u32> = bindings.keys().copied().collect();
    let mut changed = true;
    while changed {
        changed = false;
        for (&local, values) in &bindings {
            if fresh.contains(&local)
                && !values
                    .iter()
                    .all(|v| operand_is_fresh(body, *v, &fresh, type_table, call_fresh))
            {
                fresh.swap_remove(&local);
                changed = true;
            }
        }
    }
    fresh
}

/// Whether every value `body` can return is fresh — the function's result
/// aliases none of its arguments, so a copy of a call to it is a no-op.
fn function_result_is_fresh(
    body: &Body,
    fresh_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    call_fresh: &dyn Fn(FuncId) -> bool,
) -> bool {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Return { value: Some(op) } | StmtKind::Break { value: Some(op), .. } =
                &body.stmts[s].kind
            && !operand_is_fresh(body, *op, fresh_locals, type_table, call_fresh)
        {
            return false;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    // A trailing bare expression is an implicit return.
    if let Some(&last) = body.blocks[body.root].stmts.last()
        && let StmtKind::Expr(op) = &body.stmts[last].kind
        && !operand_is_fresh(body, *op, fresh_locals, type_table, call_fresh)
    {
        return false;
    }
    true
}

/// Whether `op`'s value is fresh in the context of the locals in `fresh`. A
/// promoted constant is uniquely owned, hence fresh. A reference operand is
/// shared on copy, never deep-copied, so it never makes an enclosing
/// construction non-fresh.
fn operand_is_fresh(
    body: &Body,
    op: Operand,
    fresh: &IndexSet<u32>,
    type_table: &TypeTable,
    call_fresh: &dyn Fn(FuncId) -> bool,
) -> bool {
    let Some(e) = op.as_expr() else { return true };
    matches!(
        type_table.get(body.exprs[e].type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    ) || expr_is_fresh(body, e, fresh, type_table, call_fresh)
}

/// Whether `e`'s value shares storage with nothing outside `fresh`: a fresh
/// construction, a copy clone, a call whose result aliases no argument, a `fresh`
/// local, or a match all of whose arms yield fresh values. `fresh` grows through
/// a match with a fresh scrutinee, whose pattern bindings destructure unaliased
/// data. Projections, deref, cast, blocks, and unrecognized shapes stay
/// conservative (not fresh) — this is the NIR recovery counterpart of
/// `lower::plan::value_copy::analyze::is_fresh_in_context`, using the
/// interprocedural `call_fresh` the insertion side lacks.
fn expr_is_fresh(
    body: &Body,
    e: ExprId,
    fresh: &IndexSet<u32>,
    type_table: &TypeTable,
    call_fresh: &dyn Fn(FuncId) -> bool,
) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => fresh.contains(index),
        ExprKind::PackedArray(_) | ExprKind::EnumConstruct { .. } => true,
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .all(|f| operand_is_fresh(body, f.value, fresh, type_table, call_fresh)),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .all(|el| operand_is_fresh(body, *el, fresh, type_table, call_fresh)),
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_none_or(|p| operand_is_fresh(body, p, fresh, type_table, call_fresh))
        }
        // The callee's result aliases no argument (or it is a copy helper).
        ExprKind::Call { func_id, .. } | ExprKind::MethodCall { func_id, .. } => call_fresh(*func_id),
        // A match yields fresh iff every value-producing arm does. When the
        // scrutinee is fresh, an arm's pattern bindings destructure unaliased
        // data and are fresh too.
        ExprKind::Match { expr: scrut, arms } => {
            let scrut_fresh = operand_is_fresh(body, *scrut, fresh, type_table, call_fresh);
            arms.iter().all(|arm| {
                // A diverging arm (`=> return …`) is `Never`-typed, yields no value.
                if arm
                    .body
                    .as_expr()
                    .is_some_and(|be| body.exprs[be].type_id == crate::tir::TypeTable::NEVER)
                {
                    return true;
                }
                let mut arm_fresh = fresh.clone();
                if scrut_fresh {
                    collect_pattern_bindings(body, arm.pattern, &mut arm_fresh);
                }
                operand_is_fresh(body, arm.body, &arm_fresh, type_table, call_fresh)
            })
        }
        _ => false,
    }
}

/// Collect every local a NIR pattern binds.
fn collect_pattern_bindings(body: &Body, pat: crate::nir_arena::PatId, out: &mut IndexSet<u32>) {
    use crate::nir_arena::PatKind;
    match &body.pats[pat].kind {
        PatKind::Binding { local_index, .. } => {
            out.insert(*local_index);
        }
        PatKind::Tuple(subs, _) | PatKind::Or(subs) => {
            for &s in subs {
                collect_pattern_bindings(body, s, out);
            }
        }
        PatKind::Variant { bindings, .. } => {
            for &s in bindings {
                collect_pattern_bindings(body, s, out);
            }
        }
        PatKind::Struct { fields, .. } => {
            for f in fields {
                collect_pattern_bindings(body, f.pattern, out);
            }
        }
        PatKind::Wildcard
        | PatKind::Literal(_)
        | PatKind::Enum { .. }
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => {}
    }
}

fn union(mut a: Taint, b: Taint) -> Taint {
    a.extend(b);
    a
}

/// Whether a bodyless builtin returns a genuinely fresh value. A builtin has no
/// body to analyze, so its aliasing is modelled here. Almost every builtin
/// allocates or computes a fresh result; the sole exception is the array element
/// read, whose result is storage aliased into its container argument. A
/// primitive-element read is technically fresh (the element is returned by
/// value), but a primitive never carries a `value_copy` to strip, so there is
/// nothing to gain from distinguishing it — and a builtin's `return_type` is the
/// unsubstituted generic `T`, so the element type is not available here anyway.
fn builtin_result_is_fresh(name: &str) -> bool {
    name != "array_get"
}

/// Whether a value of `type_id` can carry another value's identity: an
/// aggregate that shares GC storage when aliased, or a reference to one.
/// Primitives (integers, floats, bools, bare enums, unit) cannot.
fn carries_identity(type_id: TypeId, type_table: &TypeTable) -> bool {
    needs_value_copy(type_id, type_table)
        || matches!(
            type_table.get(type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        )
}

/// Whether `e`'s type is `&mut T`.
fn is_mut_ref(body: &Body, e: ExprId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(body.exprs[e].type_id),
        ResolvedType::MutRef(_)
    )
}

/// Whether `e`'s type is any reference (`&T` / `&mut T`).
fn is_ref_typed(body: &Body, e: ExprId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(body.exprs[e].type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    )
}
