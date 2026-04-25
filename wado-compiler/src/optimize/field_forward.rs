//! TIR-level struct field constant forwarding.
//!
//! Tracks per-local field values (constants and aliased locals) along
//! straight-line code, propagating them through:
//!
//! - `let local = StructLiteral { ... }` — record each forwardable field
//! - `let dst = $value_copy$T(src)` — recognize calls to
//!   `FunctionKind::ValueCopy` helpers and copy `src`'s field knowledge
//!   to `dst`. This is the TIR replacement for the WIR-level
//!   `WirInstr::ValueCopy` arm in `wir_optimize::const_forward`.
//! - `let dst = local` — copy `local`'s knowledge to `dst`
//!
//! Replaces field reads (`local.field`) with the recorded value when
//! known. Invalidates entries on field assignment, full reassignment,
//! address-take, capture, or call args that may mutate the local.
//!
//! Runs inside the optimization loop so that newly-exposed StructLiteral
//! / `$value_copy$T<id>` patterns from inlining or synthesis cascade
//! into further folding.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId};

/// `(local_index, field_name)` → forwardable value (constant literal or
/// `Local` reference).
type FieldKey = (u32, String);

/// Per-function field-value knowledge tracked along straight-line code.
#[derive(Default, Clone)]
struct FieldKnowledge {
    /// Known field values. Stored expressions are always *forwardable*
    /// (see [`is_forwardable`]) so substituting them at a use site
    /// doesn't change semantics.
    fields: IndexMap<FieldKey, TirExpr>,
    /// Locals that may be aliased — either both sides of a `let dst =
    /// src` Local→Local copy (which makes them share storage for
    /// reference-typed values like `Box<T>` or post-elide value
    /// types), or whose `&mut` reference escapes. Field knowledge is
    /// never recorded for aliased locals because mutations through
    /// any alias would invalidate it without our seeing it.
    aliased: IndexSet<u32>,
}

impl FieldKnowledge {
    /// Record forwardable fields from a `StructLiteral { f0: e0, ... }`
    /// assigned to `local_index`. Skipped when `local_index` is in the
    /// aliased set — its fields may be modified through aliases.
    fn record_struct_literal(
        &mut self,
        local_index: u32,
        fields: &[crate::tir::TirStructField],
    ) {
        if self.aliased.contains(&local_index) {
            return;
        }
        for field in fields {
            if is_forwardable(&field.value) {
                self.fields
                    .insert((local_index, field.name.clone()), field.value.clone());
            }
        }
    }

    /// Copy every recorded field of `src` to `dst`. Skipped when `dst`
    /// is aliased (its fields could be modified via the alias source).
    fn copy_from(&mut self, src: u32, dst: u32) {
        if self.aliased.contains(&dst) {
            return;
        }
        let copies: Vec<(String, TirExpr)> = self
            .fields
            .iter()
            .filter_map(|((idx, name), val)| {
                if *idx == src {
                    Some((name.clone(), val.clone()))
                } else {
                    None
                }
            })
            .collect();
        for (name, val) in copies {
            self.fields.insert((dst, name), val);
        }
    }

    /// Invalidate all knowledge about `local_index` — the local was
    /// fully reassigned, captured, or had its address taken with mut
    /// access. Also drops entries whose stored value references the
    /// reassigned local, which would otherwise read stale data.
    fn invalidate_local(&mut self, local_index: u32) {
        self.fields.retain(|(idx, _), val| {
            if *idx == local_index {
                return false;
            }
            if let TirExprKind::Local { index: src_idx, .. } = &val.kind
                && *src_idx == local_index
            {
                return false;
            }
            true
        });
    }

    /// Invalidate just `(local_index, field)` — the field was assigned
    /// directly via `local.field = expr`.
    fn invalidate_field(&mut self, local_index: u32, field_name: &str) {
        self.fields
            .swap_remove(&(local_index, field_name.to_string()));
    }

    /// Look up a recorded value for `local_index.field_name`.
    fn get(&self, local_index: u32, field_name: &str) -> Option<&TirExpr> {
        self.fields.get(&(local_index, field_name.to_string()))
    }

    /// Drop all recorded knowledge. Used at control-flow boundaries
    /// where conservatively invalidating is simpler than tracking the
    /// modified set.
    fn clear(&mut self) {
        self.fields.clear();
    }
}

/// Returns `Some(type_id)` when this expression is a synthesized
/// `$value_copy$T<id>(arg)` call whose callee was registered as a
/// `FunctionKind::ValueCopy` helper.
fn value_copy_call_arg<'a>(
    expr: &'a TirExpr,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> Option<&'a TirExpr> {
    let TirExprKind::Call { func, args, .. } = &expr.kind else {
        return None;
    };
    if args.len() != 1 {
        return None;
    }
    helpers
        .get(&(func.module_source.clone(), func.name.clone()))
        .map(|_| &args[0].expr)
}

/// True when an expression is safe to forward into a use site —
/// substituting it preserves semantics regardless of the surrounding
/// state. Mirrors the WIR-level `is_forwardable` predicate.
fn is_forwardable(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::Local { .. }
    )
}

pub fn forward_struct_field_constants(project: &mut FlatPackage) -> bool {
    let helpers: IndexMap<(ModuleSource, String), TypeId> = project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            f.value_copy_type()
                .map(|t| ((f.module_source.clone(), f.name.clone()), t))
        })
        .collect();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let func_name = func.name.clone();
        let module = func.module_source.clone();
        let Some(ref mut body) = func.body else {
            continue;
        };
        let aliased = collect_aliased_locals(body);
        let _ = (&func_name, &module);
        let mut known = FieldKnowledge {
            aliased,
            ..Default::default()
        };
        changed |= forward_in_block(body, &mut known, &helpers);
    }
    changed
}

/// Pre-pass: collect every local whose storage may be observed
/// through more than one name, and is therefore unsafe to record
/// field knowledge for. Conservative — false positives only cost
/// missed optimizations.
fn collect_aliased_locals(body: &TirBlock) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    collect_aliased_in_block(body, &mut out);
    out
}

fn collect_aliased_in_block(block: &TirBlock, out: &mut IndexSet<u32>) {
    for stmt in &block.stmts {
        collect_aliased_in_stmt(stmt, out);
    }
}

fn collect_aliased_in_stmt(stmt: &TirStmt, out: &mut IndexSet<u32>) {
    match &stmt.kind {
        // `let dst = src` (Local→Local copy) → both share storage.
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if let TirExprKind::Local { index: src, .. } = &value.kind {
                out.insert(*local_index);
                out.insert(*src);
            }
            collect_aliased_in_expr(value, out);
        }
        TirStmtKind::LetDestructure { value, .. } => collect_aliased_in_expr(value, out),
        TirStmtKind::Expr(expr) => {
            // `dst = src` (Assign Local→Local) — same aliasing.
            if let TirExprKind::Assign { target, value } = &expr.kind
                && let TirExprKind::Local { index: dst, .. } = &target.kind
                && let TirExprKind::Local { index: src, .. } = &value.kind
            {
                out.insert(*dst);
                out.insert(*src);
            }
            collect_aliased_in_expr(expr, out);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_aliased_in_expr(v, out);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_aliased_in_expr(condition, out);
            collect_aliased_in_block(then_block, out);
            if let Some(eb) = else_block {
                collect_aliased_in_block(eb, out);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_aliased_in_block(body, out);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_aliased_in_expr(scrutinee, out);
            collect_aliased_in_block(then_block, out);
            if let Some(eb) = else_block {
                collect_aliased_in_block(eb, out);
            }
        }
        _ => {}
    }
}

fn collect_aliased_in_expr(expr: &TirExpr, out: &mut IndexSet<u32>) {
    match &expr.kind {
        // `&local` or `&mut local` escapes a reference. The OLD
        // WIR-level pass distinguished by `stores` annotation, but at
        // TIR we don't have a callee-level view here — be conservative
        // and treat any Ref/MutRef on a Local as alias-creating.
        TirExprKind::Unary { op, expr: inner } => {
            if matches!(
                op,
                crate::tir::TirUnaryOp::MutRef | crate::tir::TirUnaryOp::Ref
            ) && let TirExprKind::Local { index, .. } = &inner.kind
            {
                out.insert(*index);
            }
            collect_aliased_in_expr(inner, out);
        }
        // Calls with mut args may stash the reference — alias.
        TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let TirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    out.insert(*index);
                }
                collect_aliased_in_expr(&arg.expr, out);
            }
            if let TirExprKind::MethodCall { receiver, .. } = &expr.kind {
                // Auto-ref: receiver may be passed as `&mut self`.
                if let TirExprKind::Local { index, .. } = &receiver.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(receiver, out);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_aliased_in_expr(arg, out);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_aliased_in_expr(callee, out);
            for arg in args {
                collect_aliased_in_expr(arg, out);
            }
        }
        TirExprKind::Closure { captures, body, .. } => {
            for capture in captures {
                out.insert(capture.outer_index);
            }
            collect_aliased_in_expr(body, out);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_aliased_in_block(block, out);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_aliased_in_expr(condition, out);
            collect_aliased_in_block(then_branch, out);
            if let Some(eb) = else_branch {
                collect_aliased_in_block(eb, out);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_aliased_in_expr(inner, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    collect_aliased_in_expr(g, out);
                }
                collect_aliased_in_expr(&arm.body, out);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_aliased_in_expr(functor, out);
        }
        TirExprKind::Assign { target, value } => {
            collect_aliased_in_expr(target, out);
            collect_aliased_in_expr(value, out);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_aliased_in_expr(left, out);
            collect_aliased_in_expr(right, out);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            collect_aliased_in_expr(inner, out);
        }
        TirExprKind::Index { expr: inner, index, .. } => {
            collect_aliased_in_expr(inner, out);
            collect_aliased_in_expr(index, out);
        }
        // Locals stored as field values of a fresh aggregate become
        // reachable through that aggregate; future reads through the
        // aggregate (including via captured-closure access or stored
        // references) may modify them. Mark aliased.
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                if let TirExprKind::Local { index, .. } = &field.value.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(&field.value, out);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                if let TirExprKind::Local { index, .. } = &elem.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(elem, out);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                if let TirExprKind::Local { index, .. } = &p.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(p, out);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => collect_aliased_in_expr(value, out),
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_aliased_in_expr(scrutinee, out);
            for arm in arms {
                collect_aliased_in_block(arm, out);
            }
            collect_aliased_in_block(default, out);
        }
        _ => {}
    }
}

/// Update `known` after a `let local = value` binding has been
/// processed. Records the field knowledge produced by recognized RHS
/// shapes and copies through `$value_copy$T(local)` calls.
fn update_knowledge_from_let(
    local_index: u32,
    value: &TirExpr,
    known: &mut FieldKnowledge,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) {
    // Recognize chained `$value_copy$T(...)` wrappers so a single Let
    // introduces the underlying source's knowledge.
    let inner = match value_copy_call_arg(value, helpers) {
        Some(arg) => arg,
        None => value,
    };
    match &inner.kind {
        TirExprKind::StructLiteral { fields, .. } => {
            known.record_struct_literal(local_index, fields);
        }
        TirExprKind::Local { index: src, .. } => {
            known.copy_from(*src, local_index);
        }
        _ => {}
    }
}

/// Update `known` after a top-level `Expr(stmt)` has been processed —
/// typically an `Assign { target, value }` or a method-call expression
/// that may mutate a local.
fn update_knowledge_from_expr_stmt(
    expr: &TirExpr,
    known: &mut FieldKnowledge,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) {
    if let TirExprKind::Assign { target, value } = &expr.kind {
        match &target.kind {
            TirExprKind::Local { index, .. } => {
                known.invalidate_local(*index);
                update_knowledge_from_let(*index, value, known, helpers);
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => match &inner.kind {
                TirExprKind::Local { index, .. } => {
                    known.invalidate_field(*index, field_name);
                    if is_forwardable(value) {
                        known
                            .fields
                            .insert((*index, field_name.clone()), (**value).clone());
                    }
                }
                // Anything more complex than `local.field = expr`
                // (e.g. `(*p).field = ...` or `q.outer.inner = ...`)
                // could mutate aliased state we don't track. Fall back
                // to clearing all knowledge.
                _ => known.clear(),
            },
            // Writes through Deref / Index / etc. may alias arbitrary
            // locals; conservatively clear.
            _ => known.clear(),
        }
    }
}

/// Walk an expression, replacing `local.field` reads when `known`
/// records a forwardable value, and conservatively invalidating
/// locals passed to calls or used as `&mut` targets.
fn forward_in_expr(
    expr: &mut TirExpr,
    known: &mut FieldKnowledge,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> bool {
    // Try to fold `local.field` here itself.
    if let TirExprKind::FieldAccess {
        expr: inner,
        field_name,
        ..
    } = &expr.kind
        && let TirExprKind::Local { index, .. } = &inner.kind
        && let Some(known_val) = known.get(*index, field_name)
    {
        let span = expr.span;
        let mut new_expr = known_val.clone();
        new_expr.span = span;
        *expr = new_expr;
        return true;
    }
    let mut changed = false;
    match &mut expr.kind {
        TirExprKind::Local { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::Assign { target, value } => {
            // Walk `value` normally — that side is read.
            changed |= forward_in_expr(value, known, helpers);
            // For `target`, the OUTER expression is an lvalue (write
            // position) and must not be folded. Only its sub-expressions
            // (the receiver of a FieldAccess, the indexee of an Index)
            // are read positions. Walk those without touching the outer
            // shape.
            match &mut target.kind {
                TirExprKind::FieldAccess { expr: inner, .. }
                | TirExprKind::Index { expr: inner, .. } => {
                    changed |= forward_in_expr(inner, known, helpers);
                }
                _ => {}
            }
            // Invalidate based on target shape. Unrecognized lvalue
            // shapes (e.g. `*self = ...` writing through a Deref) may
            // mutate any aliased local, so fall back to clearing all
            // knowledge.
            match &target.kind {
                TirExprKind::Local { index, .. } => {
                    known.invalidate_local(*index);
                }
                TirExprKind::FieldAccess {
                    expr: inner,
                    field_name,
                    ..
                } => match &inner.kind {
                    TirExprKind::Local { index, .. } => {
                        known.invalidate_field(*index, field_name);
                    }
                    _ => known.clear(),
                },
                _ => known.clear(),
            }
        }
        TirExprKind::Unary { op, expr: inner } => {
            changed |= forward_in_expr(inner, known, helpers);
            if matches!(op, crate::tir::TirUnaryOp::MutRef)
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                known.invalidate_local(*index);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            changed |= forward_in_expr(left, known, helpers);
            changed |= forward_in_expr(right, known, helpers);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, helpers);
                if arg.is_mut
                    && let TirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    known.invalidate_local(*index);
                }
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(arg, known, helpers);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= forward_in_expr(receiver, known, helpers);
            // Auto-ref hides &mut self, so be conservative: any local
            // receiver may have been mutated by the call.
            if let TirExprKind::Local { index, .. } = &receiver.kind {
                known.invalidate_local(*index);
            }
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, helpers);
                if arg.is_mut
                    && let TirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    known.invalidate_local(*index);
                }
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            changed |= forward_in_expr(callee, known, helpers);
            for arg in args {
                changed |= forward_in_expr(arg, known, helpers);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= forward_in_expr(functor, known, helpers);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::Cast { expr: inner, .. } => {
            changed |= forward_in_expr(inner, known, helpers);
        }
        TirExprKind::Index { expr: inner, index, .. } => {
            changed |= forward_in_expr(inner, known, helpers);
            changed |= forward_in_expr(index, known, helpers);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            // Conservative: blocks may re-execute via labels; clear
            // outer knowledge and start fresh inside.
            known.clear();
            let mut inner = FieldKnowledge::default();
            changed |= forward_in_block(block, &mut inner, helpers);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= forward_in_expr(condition, known, helpers);
            let mut then_known = known.clone();
            changed |= forward_in_block(then_branch, &mut then_known, helpers);
            if let Some(eb) = else_branch {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, helpers);
            }
            known.clear();
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= forward_in_expr(&mut field.value, known, helpers);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                changed |= forward_in_expr(elem, known, helpers);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= forward_in_expr(p, known, helpers);
            }
        }
        TirExprKind::Closure { body, .. } => {
            // Closure body executes in its own scope — clear and walk.
            known.clear();
            let mut inner = FieldKnowledge::default();
            changed |= forward_in_expr(body, &mut inner, helpers);
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= forward_in_expr(inner, known, helpers);
            for arm in arms {
                let mut arm_known = known.clone();
                if let Some(guard) = &mut arm.guard {
                    changed |= forward_in_expr(guard, &mut arm_known, helpers);
                }
                changed |= forward_in_expr(&mut arm.body, &mut arm_known, helpers);
            }
            known.clear();
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= forward_in_expr(value, known, helpers);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            changed |= forward_in_expr(expr, known, helpers);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= forward_in_expr(scrutinee, known, helpers);
            for arm in arms {
                let mut arm_known = known.clone();
                changed |= forward_in_block(arm, &mut arm_known, helpers);
            }
            let mut def_known = known.clone();
            changed |= forward_in_block(default, &mut def_known, helpers);
            known.clear();
        }
        _ => {}
    }
    changed
}

fn forward_in_block(
    block: &mut TirBlock,
    known: &mut FieldKnowledge,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= forward_in_stmt(stmt, known, helpers);
    }
    changed
}

fn forward_in_stmt(
    stmt: &mut TirStmt,
    known: &mut FieldKnowledge,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> bool {
    let mut changed = false;
    match &mut stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            changed |= forward_in_expr(value, known, helpers);
            // Even when this Let re-binds an existing local index (rare
            // — typically each Let introduces a fresh index), drop any
            // stale entries first so the snapshot below sees only the
            // values produced by `value`.
            known.invalidate_local(*local_index);
            update_knowledge_from_let(*local_index, value, known, helpers);
        }
        TirStmtKind::LetDestructure { value, .. } => {
            changed |= forward_in_expr(value, known, helpers);
        }
        TirStmtKind::Expr(expr) => {
            changed |= forward_in_expr(expr, known, helpers);
            update_knowledge_from_expr_stmt(expr, known, helpers);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                changed |= forward_in_expr(v, known, helpers);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            changed |= forward_in_expr(condition, known, helpers);
            // Conservative: drop knowledge before each branch and after
            // the merge. Per-branch tracking inside the branch body is
            // still useful for chained patterns.
            let mut then_known = known.clone();
            changed |= forward_in_block(then_block, &mut then_known, helpers);
            if let Some(eb) = else_block {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, helpers);
            }
            known.clear();
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            // Loop bodies can re-execute and re-assign anything; drop
            // outer knowledge and start fresh inside.
            known.clear();
            let mut inner = FieldKnowledge::default();
            changed |= forward_in_block(body, &mut inner, helpers);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            changed |= forward_in_expr(scrutinee, known, helpers);
            let mut then_known = known.clone();
            changed |= forward_in_block(then_block, &mut then_known, helpers);
            if let Some(eb) = else_block {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, helpers);
            }
            known.clear();
        }
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => {}
    }
    changed
}
