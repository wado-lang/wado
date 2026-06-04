//! Body globalization — hoist a constant, read-only aggregate `let` binding
//! out of a function body into a shared immutable module global.
//!
//! A constant-shaped aggregate bound by a `let` and only ever read is rebuilt
//! on every call (or loop iteration) — pure waste, since Wasm 3.0 GC can build
//! it once at instantiation. This pass detects such a binding and hoists it:
//!
//! ```text
//! fn f() { let xs = [10, 20, 30]; … xs.repr … }   // rebuilt per call
//!   ⇒
//! global __const_obj_0 = <lazy>;                   // built once
//! fn f() { __const_obj_0 = [10, 20, 30]; … global:__const_obj_0.repr … }
//! ```
//!
//! The hoisted global mirrors what `lower::plan::globals::extract` produces for
//! a non-const user global: a Wasm-mutable, Wado-immutable slot with a `null`
//! placeholder init (`lazy_init` / nullable), assigned once by an inline
//! `GlobalVarSet`. The existing [`crate::wir_optimize::const_global`] pass is
//! the single eager/lazy classifier — it sees the inline `GlobalVarSet` is
//! const-expressible, moves the value into the global's eager `init`, marks it
//! immutable, and drops the assignment. So this pass adds **no** new lowering;
//! it reuses the const-global machinery wholesale. If a value turns out not to
//! be WIR-const-expressible after all (e.g. a `String`'s `array.new_data`
//! repr), the global simply stays lazy-assigned each call — still correct.
//!
//! ## Soundness
//!
//! Two independent gates, both load-bearing:
//!
//! - **Closed const aggregate** ([`is_globalizable_const`]). The initializer
//!   must be a side-effect-free constant with no free locals: literals, nested
//!   `Struct` / `Tuple` / `Array` / `Enum` / `Variant` constructors, and the
//!   builder-temp block (`{ let __b = …; *__b }`) an array literal leaves.
//!   Strings (`array.new_data`), `BytesLiteral`, calls, and reads of other
//!   globals are excluded — a free local or side effect would make hoisting a
//!   value to module scope unsound, and a non-const value can't promote.
//!
//! - **Read-only** ([`is_readonly`]). Every use of the binding must be a
//!   borrowing / reading position. This is what blocked the first attempt: at
//!   `-O2`, `xs[i] = v` lowered to `array_set(xs.repr, …)`, an invisible
//!   mutation through the `.repr` projection. WEP-2026-06-02 Phase 1 gave the
//!   `builtin::array_*` ops `&` / `&mut` first parameters, so the mutation is
//!   now an explicit `&mut xs.repr` the gate rejects. Sharing the *whole*
//!   object means even a spine mutation (`push`) corrupts it, so — unlike
//!   `value_copy_demote`'s element-immutability check this gate is modelled on
//!   — any `&mut self` method, any `&mut` of a projection, and any assignment
//!   to the binding or a projection disqualifies it. A bare whole-value read
//!   in a consuming position (return, block tail, `let y = xs`, an aggregate
//!   element) is also rejected: the value-copy machinery may have elided the
//!   copy treating `xs` as a movable fresh local, which globalizing breaks.
//!   Crucially this includes passing the binding *by value* to a call — a
//!   builtin store (`array_set` / `array_fill`) stows the argument's reference
//!   into the array without a copy, so a shared global aliased in and later
//!   mutated would corrupt it (the bug that broke the zlib encoder tables in
//!   review). By-`&` borrows, field / index reads, and `&self` methods are the
//!   admitted shapes.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{
    FunctionRef, NirBlock, NirExpr, NirExprKind, NirFunction, NirGlobal, NirStmt, NirStmtKind,
    NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirMutVisitor, expr_mentions_local, is_local, strip_refs};
use crate::tir::{ResolvedType, TypeId, TypeTable};

type FuncKey = (ModuleSource, String);

/// A `let` binding selected for hoisting, identified by its owning function and
/// the bound local index. Resolved in an immutable analysis phase, applied in a
/// later mutation phase to avoid `RefCell` borrow conflicts.
struct Candidate {
    func_idx: usize,
    local_index: u32,
    ty: TypeId,
    module_source: ModuleSource,
}

pub fn globalize_const_objects(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.clone();
    let mut by_key: IndexMap<FuncKey, usize> = IndexMap::default();
    for (i, f) in project.functions.iter().enumerate() {
        let f = f.borrow();
        by_key.insert((f.module_source.clone(), f.name.clone()), i);
    }

    // Phase 1 — analysis (all immutable borrows). Collect every hoistable
    // binding. The read-only gate borrows callee functions immutably to read
    // `&mut self`-ness; stacking immutable borrows is fine even for a
    // self-recursive function.
    let gate = Gate {
        funcs: &project.functions,
        by_key: &by_key,
        type_table: &type_table,
    };
    let mut candidates: Vec<Candidate> = Vec::new();
    for (fi, f) in project.functions.iter().enumerate() {
        let f = f.borrow();
        if skip_function(&f) {
            continue;
        }
        let Some(body) = &f.body else { continue };
        collect_candidates(body, &gate, fi, &f.module_source, &mut candidates);
    }
    if candidates.is_empty() {
        return false;
    }

    // Phase 2 — mutation. Each candidate becomes a fresh global with a `null`
    // placeholder init; the binding becomes an inline `GlobalVarSet` and its
    // reads become `GlobalVarGet`. Number from the count of pre-existing
    // `__const_obj_*` globals rather than from `0`, so names stay unique even
    // if the pass is invoked more than once over a program's lifetime: a
    // per-call `0`-based counter would emit a second `__const_obj_0` with an
    // unrelated type, colliding two globals under one name (an invalid-Wasm
    // type mismatch). The pass runs once today, but this keeps it idempotent.
    let mut counter = project
        .globals
        .iter()
        .filter(|g| g.name.starts_with("__const_obj_"))
        .count();
    for cand in candidates {
        let name = format!("__const_obj_{counter}");
        counter += 1;

        let mut func = project.functions[cand.func_idx].borrow_mut();
        let body = func.body.as_mut().expect("candidate function has a body");
        // Rewrite reads first (the let's own value is const and references no
        // local index, so it is untouched), then replace the binding.
        let mut rw = ReadRewrite {
            local_index: cand.local_index,
            module_source: cand.module_source.clone(),
            name: name.clone(),
            ty: cand.ty,
        };
        rw.visit_block(body);
        replace_let_with_set(body, cand.local_index, &cand.module_source, &name);
        drop(func);

        project.globals.push(NirGlobal {
            name,
            ty: cand.ty,
            // A `null` placeholder; the real value is the inline `GlobalVarSet`
            // promoted by `wir_optimize::const_global`.
            initializer: NirExpr::new(
                NirExprKind::Null,
                cand.ty,
                crate::token::Span::new(0, 0, 1, 1),
            ),
            mutable: true,
            wado_mutable: false,
            is_pub: false,
            module_source: cand.module_source,
            span: crate::token::Span::new(0, 0, 1, 1),
            is_nullable: true,
            lazy_init: true,
            locals: Vec::new(),
        });
    }
    true
}

/// Skip synthesized init / CM-binding functions: their global assignments are
/// already the const-global machinery's domain, and we do not want to recurse
/// the optimization onto our own output.
fn skip_function(f: &NirFunction) -> bool {
    f.is_cm_binding
        || f.is_dispatch_wrapper
        || f.name.starts_with("__initialize")
        || f.value_copy_type().is_some()
}

// ---------------------------------------------------------------------------
// Phase 1 — candidate collection
// ---------------------------------------------------------------------------

fn collect_candidates(
    block: &NirBlock,
    gate: &Gate<'_>,
    func_idx: usize,
    module_source: &ModuleSource,
    out: &mut Vec<Candidate>,
) {
    for stmt in &block.stmts {
        if let NirStmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } = &stmt.kind
            && gate.is_reference_type(*type_id)
            && is_globalizable_const(value, &mut IndexSet::default())
            && contains_aggregate(value)
            && is_readonly(gate.func_body(func_idx), *local_index, gate)
        {
            out.push(Candidate {
                func_idx,
                local_index: *local_index,
                ty: *type_id,
                module_source: module_source.clone(),
            });
        }
        // Recurse into nested scopes so a binding inside an `if` / `loop` is
        // also considered.
        for inner in stmt_blocks(stmt) {
            collect_candidates(inner, gate, func_idx, module_source, out);
        }
    }
}

/// Iterate the sub-blocks a statement owns (for candidate recursion).
fn stmt_blocks(stmt: &NirStmt) -> Vec<&NirBlock> {
    match &stmt.kind {
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut v = vec![then_block];
            if let Some(eb) = else_block {
                v.push(eb);
            }
            v
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => vec![body],
        _ => Vec::new(),
    }
}

/// Recursively true when `expr` is a closed constant aggregate value: a
/// side-effect-free constant with no free local references. `bound` carries the
/// set of block-local indices introduced by an enclosing builder block (e.g.
/// the `__b` an array literal leaves), which are themselves const.
fn is_globalizable_const(expr: &NirExpr, bound: &mut IndexSet<u32>) -> bool {
    match &expr.kind {
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => true,
        NirExprKind::Local { index, .. } => bound.contains(index),
        NirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .all(|f| is_globalizable_const(&f.value, bound)),
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            elements.iter().all(|e| is_globalizable_const(e, bound))
        }
        NirExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .is_none_or(|p| is_globalizable_const(p, bound)),
        // Transparent value wrappers: a builder block yields `*__b`; numeric
        // casts and negation of a constant are constant.
        NirExprKind::Unary {
            op: NirUnaryOp::Deref | NirUnaryOp::Ref | NirUnaryOp::Neg,
            expr: inner,
        }
        | NirExprKind::Cast { expr: inner, .. } => is_globalizable_const(inner, bound),
        // The builder-temp block an array / list literal leaves:
        // `{ let mut __b = <const>; *__b }`. Every statement must be a `let`
        // binding a const value (added to `bound`) except the final value
        // expression.
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            block_is_const(block, bound)
        }
        _ => false,
    }
}

fn block_is_const(block: &NirBlock, bound: &mut IndexSet<u32>) -> bool {
    let Some((last, init)) = block.stmts.split_last() else {
        return false;
    };
    for stmt in init {
        let NirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
        else {
            return false;
        };
        if !is_globalizable_const(value, bound) {
            return false;
        }
        bound.insert(*local_index);
    }
    // The block's value is its trailing expression statement.
    match &last.kind {
        NirStmtKind::Expr(e) => is_globalizable_const(e, bound),
        _ => false,
    }
}

/// True when `expr` contains at least one aggregate constructor, so we only
/// globalize objects worth a global slot (not a bare `null` / scalar).
fn contains_aggregate(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::StructLiteral { .. }
        | NirExprKind::TupleLiteral { .. }
        | NirExprKind::ArrayLiteral { .. }
        | NirExprKind::VariantConstruct { .. } => true,
        NirExprKind::Unary { expr: inner, .. } | NirExprKind::Cast { expr: inner, .. } => {
            contains_aggregate(inner)
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            block.stmts.iter().any(|s| match &s.kind {
                NirStmtKind::Let { value, .. } | NirStmtKind::Expr(value) => {
                    contains_aggregate(value)
                }
                _ => false,
            })
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Read-only gate
// ---------------------------------------------------------------------------

struct Gate<'a> {
    funcs: &'a [Rc<RefCell<NirFunction>>],
    by_key: &'a IndexMap<FuncKey, usize>,
    type_table: &'a Rc<RefCell<TypeTable>>,
}

impl Gate<'_> {
    fn is_reference_type(&self, ty: TypeId) -> bool {
        !matches!(
            self.type_table.borrow().get(ty),
            ResolvedType::Primitive(_) | ResolvedType::Unit | ResolvedType::Never
        )
    }

    fn func_body(&self, idx: usize) -> std::cell::Ref<'_, NirFunction> {
        self.funcs[idx].borrow()
    }

    /// `Some(true)` when `func`'s `self` parameter is `&mut self`,
    /// `Some(false)` when it is `&self` / by-value, `None` when the callee is
    /// unresolvable (conservatively treated as mutating by the gate).
    fn callee_mutates_self(&self, func: &FunctionRef) -> Option<bool> {
        let idx = *self
            .by_key
            .get(&(func.module_source.clone(), func.name.clone()))?;
        let f = self.funcs[idx].borrow();
        let p0 = f.params.first()?;
        Some(matches!(
            self.type_table.borrow().get(p0.type_id),
            ResolvedType::MutRef(_)
        ))
    }
}

/// True when every use of local `idx` in `func`'s body keeps it immutable. The
/// binding's own `let` value is constant (no `idx` reference), so scanning the
/// whole body is safe.
fn is_readonly(func: std::cell::Ref<'_, NirFunction>, idx: u32, gate: &Gate<'_>) -> bool {
    match &func.body {
        Some(body) => body_readonly(body, idx, gate),
        None => true,
    }
}

fn body_readonly(block: &NirBlock, idx: u32, gate: &Gate<'_>) -> bool {
    block.stmts.iter().all(|s| stmt_readonly(s, idx, gate))
}

fn stmt_readonly(stmt: &NirStmt, idx: u32, gate: &Gate<'_>) -> bool {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => expr_readonly(value, idx, gate),
        NirStmtKind::Expr(e) => expr_readonly(e, idx, gate),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            value.as_ref().is_none_or(|v| expr_readonly(v, idx, gate))
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_readonly(condition, idx, gate)
                && body_readonly(then_block, idx, gate)
                && else_block
                    .as_ref()
                    .is_none_or(|eb| body_readonly(eb, idx, gate))
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            body_readonly(body, idx, gate)
        }
        NirStmtKind::LetDestructure { value, .. } => expr_readonly(value, idx, gate),
        NirStmtKind::Continue => true,
    }
}

fn expr_readonly(expr: &NirExpr, idx: u32, gate: &Gate<'_>) -> bool {
    match &expr.kind {
        // A bare whole-value read of the binding not intercepted by one of the
        // borrowing parents below is a consuming use (return, block tail,
        // `let y = xs`, aggregate element). Reject — globalizing would share a
        // value the value-copy machinery may have treated as a movable local.
        NirExprKind::Local { index, .. } => *index != idx,

        NirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let recv = strip_refs(receiver);
            if is_local(recv, idx) {
                // `xs.method()` — only a `&self` method is a read.
                if gate.callee_mutates_self(func) != Some(false) {
                    return false;
                }
            } else if expr_mentions_local(receiver, idx) {
                // The binding appears inside the receiver (`xs.field.method()`,
                // `xs[i].method()`). A `&mut self` method may mutate it.
                if gate.callee_mutates_self(func) != Some(false) {
                    return false;
                }
                if !expr_readonly(receiver, idx, gate) {
                    return false;
                }
            } else if !expr_readonly(receiver, idx, gate) {
                return false;
            }
            args.iter().all(|a| call_arg_readonly(&a.expr, idx, gate))
        }
        NirExprKind::Call { args, .. } => {
            args.iter().all(|a| call_arg_readonly(&a.expr, idx, gate))
        }
        NirExprKind::IndirectCall { callee, args } => {
            expr_readonly(callee, idx, gate) && args.iter().all(|a| call_arg_readonly(a, idx, gate))
        }

        // `&mut <…xs…>` — a mutable reference into the binding escapes.
        NirExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => !expr_mentions_local(inner, idx),

        // Pure scalar reads: `xs[i] + 1`, `k >= xs.used`, `xs[i] as i64`,
        // `-xs[i]`. The operands flow back through the read arms above; the
        // operator yields a fresh scalar that neither aliases nor mutates the
        // binding. Without this, a projection read buried in an expression — a
        // bounds check, a length comparison — falls to the conservative `_`
        // arm and blocks hoisting in straight-line bodies. (In a loop, `licm`
        // hoists such reads into their own `let`, which is why this only
        // surfaced for non-loop bindings.)
        NirExprKind::Binary { left, right, .. } => {
            expr_readonly(left, idx, gate) && expr_readonly(right, idx, gate)
        }
        NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::Unary {
            op: NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot,
            expr: inner,
        } => expr_readonly(inner, idx, gate),

        // Reads through projections: `xs.field`, `xs[i]`.
        NirExprKind::Index { expr: base, index } => {
            (is_local(base, idx) || expr_readonly(base, idx, gate))
                && expr_readonly(index, idx, gate)
        }
        NirExprKind::FieldAccess { expr: base, .. } => {
            is_local(base, idx) || expr_readonly(base, idx, gate)
        }

        // A write whose target touches the binding escapes.
        NirExprKind::Assign { target, value } => {
            !expr_mentions_local(target, idx) && expr_readonly(value, idx, gate)
        }

        // Block-bearing expressions: recurse through full statement lists so a
        // mutation hidden in a let-value of an expression-position block is not
        // missed (`expr_children` only yields a block's trailing exprs).
        NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => {
            body_readonly(b, idx, gate)
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_readonly(condition, idx, gate)
                && body_readonly(then_branch, idx, gate)
                && else_branch
                    .as_ref()
                    .is_none_or(|eb| body_readonly(eb, idx, gate))
        }
        NirExprKind::Match { expr: scrut, arms } => {
            expr_readonly(scrut, idx, gate)
                && arms.iter().all(|a| {
                    a.guard.as_ref().is_none_or(|g| expr_readonly(g, idx, gate))
                        && expr_readonly(&a.body, idx, gate)
                })
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_readonly(scrutinee, idx, gate)
                && arms.iter().all(|a| body_readonly(a, idx, gate))
                && body_readonly(default, idx, gate)
        }

        // Any other expression kind: the common borrowing / reading shapes are
        // handled above, so an appearance of the binding here is a
        // non-whitelisted (consuming or unknown) use. Reject conservatively.
        _ => !expr_mentions_local(expr, idx),
    }
}

/// A binding handed to a call as an argument. A `&` borrow is a read; `&mut`
/// escapes. Passing the binding *itself* by value (a bare `Local`) is a
/// consuming use and is rejected: a builtin store (`array_set` / `array_fill`)
/// stows the argument's reference into the array without a copy — the
/// value-copy machinery does not wrap a fresh-literal move — so a globalized
/// (shared) value would be aliased into the container and later mutated through
/// it. A bare `Local` of a *different* index is unrelated and fine.
fn call_arg_readonly(arg: &NirExpr, idx: u32, gate: &Gate<'_>) -> bool {
    match &arg.kind {
        NirExprKind::Local { index, .. } => *index != idx,
        NirExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => !expr_mentions_local(inner, idx),
        NirExprKind::Unary {
            op: NirUnaryOp::Ref,
            expr: inner,
        } => is_local(inner, idx) || expr_readonly(inner, idx, gate),
        _ => expr_readonly(arg, idx, gate),
    }
}

// ---------------------------------------------------------------------------
// Phase 2 — mutation
// ---------------------------------------------------------------------------

/// Rewrites every `Local(local_index)` read into a `GlobalVarGet`.
struct ReadRewrite {
    local_index: u32,
    module_source: ModuleSource,
    name: String,
    ty: TypeId,
}

impl NirMutVisitor for ReadRewrite {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        if let NirExprKind::Local { index, .. } = &expr.kind
            && *index == self.local_index
        {
            expr.kind = NirExprKind::GlobalVarGet {
                module_source: self.module_source.clone(),
                name: self.name.clone(),
            };
            // The global carries the binding's type; the read result type is
            // unchanged.
            expr.type_id = self.ty;
            return;
        }
        self.walk_expr(expr);
    }
}

/// Replace the `let local_index = value` statement with an inline
/// `GlobalVarSet(name, value)`, searching nested scopes.
fn replace_let_with_set(
    block: &mut NirBlock,
    local_index: u32,
    module_source: &ModuleSource,
    name: &str,
) -> bool {
    for stmt in &mut block.stmts {
        if let NirStmtKind::Let {
            local_index: li,
            value,
            ..
        } = &mut stmt.kind
            && *li == local_index
        {
            let span = stmt.span;
            let init = std::mem::replace(
                value,
                NirExpr::new(NirExprKind::Unit, TypeTable::UNIT, span),
            );
            let set = NirExpr::new(
                NirExprKind::GlobalVarSet {
                    module_source: module_source.clone(),
                    name: name.to_string(),
                    value: Box::new(init),
                },
                TypeTable::UNIT,
                span,
            );
            stmt.kind = NirStmtKind::Expr(set);
            return true;
        }
        for inner in stmt_blocks_mut(stmt) {
            if replace_let_with_set(inner, local_index, module_source, name) {
                return true;
            }
        }
    }
    false
}

fn stmt_blocks_mut(stmt: &mut NirStmt) -> Vec<&mut NirBlock> {
    match &mut stmt.kind {
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut v = vec![then_block];
            if let Some(eb) = else_block {
                v.push(eb);
            }
            v
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => vec![body],
        _ => Vec::new(),
    }
}
