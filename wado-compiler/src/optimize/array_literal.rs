//! Materialize `ExprKind::ArrayLiteral` from an `Array<T>` builder
//! sequence — an `array_new(N)` allocation followed by `N` `List::push`
//! calls.
//!
//! An array literal `[e0, …, eN-1] as List<T>` is lowered (via the
//! `SequenceLiteralBuilder` coercion and inlining) to:
//!
//! ```text
//! let mut __b = <init binding> List<T> { repr: array_new(N), used: 0 };
//! PLACE.push(e0);
//! PLACE.push(e1);
//! …
//! PLACE.push(eN-1);
//! ```
//!
//! `PLACE` is the bound local itself for a direct `List<T>` literal, or a
//! field of it for a custom `SequenceLiteralBuilder` whose builder wraps an
//! `List<T>` (e.g. `SeqVec { items: List<T> }`, `Bag { keys, values }`).
//! This pass recognizes that window — the `List<T> { repr: array_new(N),
//! used: 0 }` struct plus its `N` trailing `List::push` calls — and rewrites
//! the struct to `ExprKind::ArrayLiteral { elements }`, dropping the
//! pushes. The pushes need not be contiguous: inlining `push_literal` leaves
//! single-use element temps between them (see `pure_temp_binding`), and
//! pushes to distinct array fields may interleave (see `try_collapse_at`).
//!
//! `List::push` is identified by its [`CompilerItem::ListPush`] marker, not
//! by a canonical path, mirroring `string_push`. `array_new` is identified by
//! its builtin name.
//!
//! Runs *after* `inline` in the fixpoint loop: the `SequenceLiteralBuilder`
//! `new_literal` / `push_literal` / `build` methods (and, for wrapper builders,
//! the `push_literal → self.field.push` delegation) must be inlined first so
//! the raw `List<T> { array_new } + List::push` window is exposed. Giving
//! constant arrays this first-class, analyzable shape lets `cse`,
//! `const_fold`, bounds-check elimination, and constant globalization act on
//! them; `wir_build` lowers `ArrayLiteral` to `array.new_fixed`.
//!
//! Ported to the worklist rewrite engine (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a block-level
//! [`Rule`]: the builder window is a run of sibling statements, so it collapses
//! a block's statement list (`set_block_stmts`), reusing the existing element
//! expression ids (their statements are dropped, so the ids are moved, not
//! cloned). Nested blocks are separate worklist nodes processed bottom-up,
//! matching the old visitor's recurse-then-collapse order.

use crate::compiler_item::{CompilerItem, SeqField};
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;

use super::arena_query::{
    is_local_operand, is_pure_operand, operand_mentions_local, stmt_mentions_local,
};

/// The builtin generic name of the raw array allocation (`builtin::array_new`).
const ARRAY_NEW: &str = "array_new";

/// The `List<T>` struct's backing-array field and length-counter field.
const REPR_FIELD: &str = SeqField::Backing.field_name();
const USED_FIELD: &str = SeqField::Len.field_name();

/// Collect the mangled names of every `List<T>::push` monomorphization by
/// their shared [`CompilerItem::ListPush`] marker. Each element type produces
/// a distinct `NirFunction` (`List<i32>::push`, `List<String>::push`, …), so
/// call sites are matched by membership in this set rather than against one
/// reference.
pub(super) fn resolve_array_push_names(project: &NirPackage) -> IndexSet<String> {
    project
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            (func.compiler_item == Some(CompilerItem::ListPush)).then(|| func.name.clone())
        })
        .collect()
}

pub(super) struct Collapser<'a> {
    push_names: &'a IndexSet<String>,
}

impl<'a> Collapser<'a> {
    pub(super) fn new(push_names: &'a IndexSet<String>) -> Self {
        Self { push_names }
    }
}

impl Rule for Collapser<'_> {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let mut stmts = engine.body.blocks[id].stmts.clone();
        let mut changed = false;
        let mut i = 0;
        while i < stmts.len() {
            // The window is an init statement whose value embeds one or more
            // `List<T> { array_new(N), used: 0 }` structs, each consumed by a
            // run of `push` calls in the following statements.
            let consumed = self.try_collapse_at(engine, &stmts, i);
            if consumed > 0 {
                // Drop the window statements (pushes and resolved element
                // temps) that followed the init; their data moved into the
                // literal.
                stmts.drain(i + 1..i + 1 + consumed);
                changed = true;
            }
            i += 1;
        }
        if changed {
            engine.set_block_stmts(id, stmts);
        }
        changed
    }
}

impl Collapser<'_> {
    /// Try to collapse the builder window starting at `stmts[start]` (the init
    /// statement). Returns the number of following statements the window
    /// consumed — pushes plus any interleaved element temps — or 0 if no
    /// window matched. On success, the init statement's embedded `List<T>`
    /// structs are rewritten to `ArrayLiteral` in place.
    fn try_collapse_at(&self, engine: &mut Engine, stmts: &[StmtId], start: usize) -> usize {
        let body = &*engine.body;

        // Identify the local bound/assigned by the init statement.
        let Some(local) = init_local(body, stmts[start]) else {
            return 0;
        };

        // Collect the `List<T> { array_new(N), used: 0 }` structs reachable in
        // the init value, each with the access path (field chain) by which the
        // bound local reaches it. `[]` path = the local itself is the array.
        let mut targets = Vec::new();
        if let Some(value) = init_value(body, stmts[start]) {
            let mut const_env = IndexMap::default();
            collect_array_targets(body, value, &mut Vec::new(), &mut const_env, &mut targets);
        }
        if targets.is_empty() {
            return 0;
        }

        // Walk the following statements, gathering each target's push elements.
        // Pushes to different array fields may interleave (e.g. `Bag { keys,
        // values }`), and inlining `push_literal` leaves single-use temp
        // bindings (`let v = <element>; place.push(v)`) between the pushes —
        // these are resolved to their value and consumed with the window.
        // Collect each target's push elements *unresolved* (bare `Local(temp)`
        // for inlining's element temps); resolution happens after the window so
        // multi-use temps can be detected first.
        let mut pushes_per_target: Vec<Vec<Operand>> = vec![Vec::new(); targets.len()];
        let mut bindings: Vec<(u32, Operand)> = Vec::new();
        let mut consumed = 0;
        let mut all_done = false;
        // A single target keeps materialized elements in push order, so an
        // impure element temp can be moved into the literal without reordering
        // its side effect; multiple interleaved targets cannot (see
        // `temp_binding`).
        let allow_impure = targets.len() == 1;
        while start + 1 + consumed < stmts.len() && !all_done {
            let stmt = stmts[start + 1 + consumed];
            if let Some((path, element)) = self.match_push(body, stmt, local) {
                let Some(idx) = targets.iter().position(|t| t.path == path) else {
                    break;
                };
                pushes_per_target[idx].push(element);
                consumed += 1;
                all_done = pushes_per_target
                    .iter()
                    .zip(&targets)
                    .all(|(p, t)| p.len() == t.capacity);
            } else if let Some((local_index, value)) = temp_binding(body, stmt, allow_impure) {
                // A `let temp = value` for a fresh element temp; remember it so
                // a following push that reads `temp` resolves to `value`.
                bindings.push((local_index, value));
                consumed += 1;
            } else {
                break;
            }
        }

        // Every target must have received exactly its `array_new` capacity in
        // pushes; otherwise this is a genuinely growable array, not a literal.
        if !pushes_per_target
            .iter()
            .zip(&targets)
            .all(|(p, t)| p.len() == t.capacity)
        {
            return 0;
        }

        // Resolve each single-use temp binding into the one element that reads
        // it as a bare `Local`. A temp referenced by more than one element is
        // left unresolved: substituting it would clone its initializer into
        // every slot, duplicating evaluation of an impure value — so it is
        // caught by the read guard below and aborts the collapse.
        for (idx, value) in &bindings {
            let uses = pushes_per_target
                .iter()
                .flatten()
                .filter(|e| is_local_operand(body, **e, *idx))
                .count();
            if uses == 1 {
                for element in pushes_per_target.iter_mut().flatten() {
                    if is_local_operand(body, *element, *idx) {
                        *element = *value;
                    }
                }
            }
        }

        // Consuming the window drops the temp bindings whose values moved into
        // the literal. That is only sound if no dropped temp is still read —
        // neither after the window, nor inside an element (a temp left
        // unresolved above because it is multi-use, or referenced through a
        // sub-expression rather than a bare `Local`). Either residual read
        // would dangle the dropped binding, so bail.
        let rest = &stmts[start + 1 + consumed..];
        let reads_after = |idx: u32| rest.iter().any(|s| stmt_mentions_local(body, *s, idx));
        let reads_in_element = |idx: u32| {
            pushes_per_target
                .iter()
                .flatten()
                .any(|e| operand_mentions_local(body, *e, idx))
        };
        if bindings
            .iter()
            .any(|(idx, _)| reads_after(*idx) || reads_in_element(*idx))
        {
            return 0;
        }

        // Rewrite each `List<T> { array_new(N), used: 0 }` struct to the
        // materialized `ArrayLiteral`, reusing the element expression ids (the
        // push statements that owned them are dropped with the window). The
        // `body` immutable borrow ends here; the rewrite mutates the arena.
        for (target, elements) in targets.iter().zip(pushes_per_target) {
            let array_lit = ExprKind::ArrayLiteral { elements };
            engine.replace_expr_kind(target.struct_expr_id, array_lit);
        }
        consumed
    }

    /// Match a `PLACE.push(elem)` statement where `PLACE` roots at `local`.
    /// Returns the field path from `local` to the array and the pushed element.
    fn match_push(&self, body: &Body, stmt: StmtId, local: u32) -> Option<(Vec<u32>, Operand)> {
        let StmtKind::Expr(Operand::Expr(e)) = &body.stmts[stmt].kind else {
            return None;
        };
        let ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } = &body.exprs[*e].kind
        else {
            return None;
        };
        if !self.push_names.contains(&func.name) || args.len() != 1 {
            return None;
        }
        let path = place_path_operand(body, *receiver, local)?;
        Some((path, args[0].expr))
    }
}

/// If `stmt` is `let temp = value`, return the local index and the bound
/// value. Used to see through the element temps that inlining
/// `push_literal(value)` introduces (`let v = <element>; place.push(v)`).
///
/// `allow_impure` is set only when the window has a single array target. With
/// one target the materialized elements keep their original push order, so
/// moving an impure value into its element slot preserves both evaluation
/// count (the caller's read guards enforce single use) and order. With
/// multiple interleaved targets (e.g. `Bag { keys, values }`) the per-field
/// arrays materialize one after another, which would reorder side effects
/// across fields, so only pure temps may be resolved there.
fn temp_binding(body: &Body, stmt: StmtId, allow_impure: bool) -> Option<(u32, Operand)> {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } if allow_impure || is_pure_operand(body, *value) => Some((*local_index, *value)),
        _ => None,
    }
}

/// A detected `List<T> { repr: array_new(N), used: 0 }` struct, with the
/// arena id of the struct, the field path from the init's bound local, and its
/// `array_new` capacity.
struct ArrayTarget {
    struct_expr_id: ExprId,
    path: Vec<u32>,
    capacity: usize,
}

/// The local a `Let` binds or an `Assign`-to-local sets.
fn init_local(body: &Body, stmt: StmtId) -> Option<u32> {
    match &body.stmts[stmt].kind {
        StmtKind::Let { local_index, .. } => Some(*local_index),
        StmtKind::Expr(e) => match &body.exprs[e.as_expr()?].kind {
            ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
                ExprKind::Local { index, .. } => Some(*index),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn init_value(body: &Body, stmt: StmtId) -> Option<ExprId> {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. } => value.as_expr(),
        StmtKind::Expr(e) => match &body.exprs[e.as_expr()?].kind {
            ExprKind::Assign { value, .. } => value.as_expr(),
            _ => None,
        },
        _ => None,
    }
}

/// Walk an init value, recording each `List<T> { array_new(N), used: 0 }`
/// struct with the field path from the value's root. Descends through the
/// outer block tail (`{ …; *__b }` produced for direct literals) and through
/// wrapper `StructLiteral` fields.
fn collect_array_targets(
    body: &Body,
    expr: ExprId,
    path: &mut Vec<u32>,
    const_env: &mut IndexMap<u32, u64>,
    out: &mut Vec<ArrayTarget>,
) {
    match &body.exprs[expr].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            // The array struct may be a `let __b = <array>` binding (the `*__b`
            // direct-literal shape) or the block's tail expression (e.g.
            // `{ let capacity = 4; List { repr: array_new(capacity), used: 0 } }`,
            // where a dead `let capacity` survives operand promotion). Record each
            // immutable `let x = <const int>` into `const_env` first so a later
            // `array_new(x)` resolves through it, then recurse into every
            // statement's value — a non-struct candidate records no target, so
            // trying all is safe.
            for &s in &body.blocks[*block].stmts {
                let cand = match &body.stmts[s].kind {
                    StmtKind::Let {
                        local_index,
                        is_mut,
                        value,
                        ..
                    } => {
                        if !is_mut && let Some(n) = body.operand_const_int(*value) {
                            const_env.insert(*local_index, n);
                        }
                        *value
                    }
                    StmtKind::Expr(op) => *op,
                    _ => continue,
                };
                if let Some(e) = cand.as_expr() {
                    collect_array_targets(body, e, path, const_env, out);
                }
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            // Only collapse non-empty literals. A capacity-0 `array_new(0)` is
            // indistinguishable from a growable-array initialization (`let mut
            // v = []; v.push(…)`); collapsing it to a fixed 0-length
            // `array.new_fixed()` would break subsequent growth.
            if let Some(capacity) = match_list_struct(body, expr, const_env).filter(|&n| n > 0) {
                out.push(ArrayTarget {
                    struct_expr_id: expr,
                    path: path.clone(),
                    capacity,
                });
            } else {
                // A wrapper struct: recurse into each field, extending the path.
                let fields: Vec<(u32, ExprId)> = fields
                    .iter()
                    .filter_map(|f| f.value.as_expr().map(|e| (f.field_index, e)))
                    .collect();
                for (field_index, value) in fields {
                    path.push(field_index);
                    collect_array_targets(body, value, path, const_env, out);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

/// A constant `i32`/`u64` operand: a promoted `Operand::Value(Int)`, or a
/// skeleton `Local` read resolved through an enclosing immutable `let x =
/// <const>` recorded in `const_env`.
fn const_int(body: &Body, op: Operand, const_env: &IndexMap<u32, u64>) -> Option<u64> {
    if let Some(n) = body.operand_const_int(op) {
        return Some(n);
    }
    let e = op.as_expr()?;
    if let ExprKind::Local { index, .. } = &body.exprs[e].kind {
        return const_env.get(index).copied();
    }
    None
}

/// If `expr` is an `List<T> { repr: array_new(N), used: 0 }` struct, return N.
fn match_list_struct(body: &Body, expr: ExprId, const_env: &IndexMap<u32, u64>) -> Option<usize> {
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[expr].kind else {
        return None;
    };
    if fields.len() != 2 {
        return None;
    }
    let repr = fields.iter().find(|f| f.name == REPR_FIELD)?;
    let used = fields.iter().find(|f| f.name == USED_FIELD)?;
    if const_int(body, used.value, const_env) != Some(0) {
        return None;
    }
    let repr_expr = repr.value.as_expr()?;
    array_new_capacity(body, repr_expr, const_env)
}

/// If `expr` is a `builtin::array_new(N)` call with a constant `N`, return N.
fn array_new_capacity(body: &Body, expr: ExprId, const_env: &IndexMap<u32, u64>) -> Option<usize> {
    let ExprKind::Call { func, args, .. } = &body.exprs[expr].kind else {
        return None;
    };
    // The builtin reaches NIR either as a bare `array_new` (non-generic call)
    // or mangled (`…/array_new<u8>`) carrying its generic name on
    // `monomorph_info`; match the exact name in each form.
    let is_array_new = func.name == ARRAY_NEW
        || func
            .monomorph_info
            .as_ref()
            .is_some_and(|m| m.generic_name == ARRAY_NEW);
    if !is_array_new || args.len() != 1 {
        return None;
    }
    usize::try_from(const_int(body, args[0].expr, const_env)?).ok()
}

fn place_path_operand(body: &Body, op: Operand, local: u32) -> Option<Vec<u32>> {
    op.as_expr().and_then(|e| place_path(body, e, local))
}

/// If `receiver` is `local` reached through zero or more field accesses,
/// return the field-index path (`[]` for the bare local). The builder methods
/// take `&mut self`, so peel a leading reference.
fn place_path(body: &Body, receiver: ExprId, local: u32) -> Option<Vec<u32>> {
    let mut path = Vec::new();
    let mut cur = peel_ref(body, receiver);
    loop {
        match &body.exprs[cur].kind {
            ExprKind::Local { index, .. } if *index == local => {
                path.reverse();
                return Some(path);
            }
            ExprKind::FieldAccess {
                expr, field_index, ..
            } => {
                path.push(*field_index);
                match expr.as_expr() {
                    Some(e) => cur = e,
                    None => return None,
                }
            }
            _ => return None,
        }
    }
}

fn peel_ref(body: &Body, expr: ExprId) -> ExprId {
    match &body.exprs[expr].kind {
        ExprKind::Unary {
            op: crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef,
            expr: inner,
        } => inner.as_expr().map_or(expr, |e| peel_ref(body, e)),
        _ => expr,
    }
}
