#![allow(dead_code)] // WIP: helpers wired up by follow-up commits
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
use crate::hashmap::IndexMap;
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
}

impl FieldKnowledge {
    /// Record forwardable fields from a `StructLiteral { f0: e0, ... }`
    /// assigned to `local_index`.
    fn record_struct_literal(
        &mut self,
        local_index: u32,
        fields: &[crate::tir::TirStructField],
    ) {
        for field in fields {
            if is_forwardable(&field.value) {
                self.fields
                    .insert((local_index, field.name.clone()), field.value.clone());
            }
        }
    }

    /// Copy every recorded field of `src` to `dst`.
    fn copy_from(&mut self, src: u32, dst: u32) {
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
        let Some(ref mut body) = func.body else {
            continue;
        };
        let mut known = FieldKnowledge::default();
        changed |= forward_in_block(body, &mut known, &helpers);
    }
    changed
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
            } => {
                if let TirExprKind::Local { index, .. } = &inner.kind {
                    known.invalidate_field(*index, field_name);
                    if is_forwardable(value) {
                        known
                            .fields
                            .insert((*index, field_name.clone()), (**value).clone());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Walk an expression, replacing `local.field` reads when `known`
/// records a forwardable value, and conservatively invalidating
/// locals passed to calls or used as `&mut` targets.
fn forward_in_expr(
    expr: &mut TirExpr,
    known: &mut FieldKnowledge,
    _helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> bool {
    // First, try to fold `local.field` here itself.
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
    // TODO: recurse into children + invalidate on aliasing operations.
    false
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
