//! Materialize `NirExprKind::ArrayLiteral` from an `Array<T>` builder
//! sequence — an `array_new(N)` allocation followed by `N` `Array::push`
//! calls.
//!
//! An array literal `[e0, …, eN-1] as Array<T>` is lowered (via the
//! `SequenceLiteralBuilder` coercion and inlining) to:
//!
//! ```text
//! let mut __b = <init binding> Array<T> { repr: array_new(N), used: 0 };
//! PLACE.push(e0);
//! PLACE.push(e1);
//! …
//! PLACE.push(eN-1);
//! ```
//!
//! `PLACE` is the bound local itself for a direct `Array<T>` literal, or a
//! field of it for a custom `SequenceLiteralBuilder` whose builder wraps an
//! `Array<T>` (e.g. `SeqVec { items: Array<T> }`, `Bag { keys, values }`).
//! This pass recognizes that window — the `Array<T> { repr: array_new(N),
//! used: 0 }` struct plus its `N` trailing `Array::push` calls — and rewrites
//! the struct to `NirExprKind::ArrayLiteral { elements }`, dropping the
//! pushes. The pushes need not be contiguous: inlining `push_literal` leaves
//! single-use element temps between them (see `pure_temp_binding`), and
//! pushes to distinct array fields may interleave (see `try_collapse_at`).
//!
//! `Array::push` is identified by its [`CompilerItem::ArrayPush`] marker, not
//! by a canonical path, mirroring `string_push`. `array_new` is identified by
//! its builtin name.
//!
//! Runs *after* `inline` in the fixpoint loop: the `SequenceLiteralBuilder`
//! `new_literal` / `push_literal` / `build` methods (and, for wrapper builders,
//! the `push_literal → self.field.push` delegation) must be inlined first so
//! the raw `Array<T> { array_new } + Array::push` window is exposed. Giving
//! constant arrays this first-class, analyzable shape lets `cse`,
//! `const_fold`, bounds-check elimination, and constant globalization act on
//! them; `wir_build` lowers `ArrayLiteral` to `array.new_fixed`.

use crate::compiler_item::CompilerItem;
use crate::hashmap::IndexSet;
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirOptVisitor, opt_walk_block};

/// The builtin generic name of the raw array allocation (`builtin::array_new`).
const ARRAY_NEW: &str = "array_new";

/// The `Array<T>` struct's backing-array field and length-counter field.
const REPR_FIELD: &str = "repr";
const USED_FIELD: &str = "used";

pub fn collapse_array_literals(project: &mut NirPackage) -> bool {
    let push_names = resolve_array_push_names(project);
    if push_names.is_empty() {
        return false;
    }
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = &mut func.body {
            let mut visitor = Collapser {
                push_names: &push_names,
            };
            changed |= visitor.visit_block(body);
        }
    }
    changed
}

/// Collect the mangled names of every `Array<T>::push` monomorphization by
/// their shared [`CompilerItem::ArrayPush`] marker. Each element type produces
/// a distinct `NirFunction` (`Array<i32>::push`, `Array<String>::push`, …), so
/// call sites are matched by membership in this set rather than against one
/// reference.
fn resolve_array_push_names(project: &NirPackage) -> IndexSet<String> {
    project
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            (func.compiler_item == Some(CompilerItem::ArrayPush)).then(|| func.name.clone())
        })
        .collect()
}

struct Collapser<'a> {
    push_names: &'a IndexSet<String>,
}

impl NirOptVisitor for Collapser<'_> {
    fn visit_block(&mut self, block: &mut NirBlock) -> bool {
        // Recurse into nested blocks first so inner literals collapse before
        // the outer statement vector is rewritten.
        let mut changed = opt_walk_block(self, block);
        changed |= self.collapse_in_stmts(&mut block.stmts);
        changed
    }
}

impl Collapser<'_> {
    /// Scan a statement list for `Array<T>` builder windows and collapse them
    /// in place.
    fn collapse_in_stmts(&self, stmts: &mut Vec<NirStmt>) -> bool {
        let mut changed = false;
        let mut i = 0;
        while i < stmts.len() {
            // The window is an init statement whose value embeds one or more
            // `Array<T> { array_new(N), used: 0 }` structs, each consumed by a
            // run of `push` calls in the following statements.
            let consumed = self.try_collapse_at(stmts, i);
            if consumed > 0 {
                // Drop the window statements (pushes and resolved element
                // temps) that followed the init; their data moved into the
                // literal.
                stmts.drain(i + 1..i + 1 + consumed);
                changed = true;
            }
            i += 1;
        }
        changed
    }

    /// Try to collapse the builder window starting at `stmts[start]` (the init
    /// statement). Returns the number of following statements the window
    /// consumed — pushes plus any interleaved element temps — or 0 if no
    /// window matched. On success, the init statement's embedded `Array<T>`
    /// structs are rewritten to `ArrayLiteral` in place.
    fn try_collapse_at(&self, stmts: &mut [NirStmt], start: usize) -> usize {
        // Identify the local bound/assigned by the init statement.
        let Some(local) = init_local(&stmts[start].kind) else {
            return 0;
        };

        // Collect the `Array<T> { array_new(N), used: 0 }` structs reachable in
        // the init value, each with the access path (field chain) by which the
        // bound local reaches it. `[]` path = the local itself is the array.
        let mut targets = Vec::new();
        if let Some(value) = init_value(&stmts[start].kind) {
            collect_array_targets(value, &mut Vec::new(), &mut targets);
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
        let mut pushes_per_target: Vec<Vec<NirExpr>> = vec![Vec::new(); targets.len()];
        let mut bindings: Vec<(u32, NirExpr)> = Vec::new();
        let mut consumed = 0;
        let mut all_done = false;
        // A single target keeps materialized elements in push order, so an
        // impure element temp can be moved into the literal without reordering
        // its side effect; multiple interleaved targets cannot (see
        // `temp_binding`).
        let allow_impure = targets.len() == 1;
        while start + 1 + consumed < stmts.len() && !all_done {
            let stmt = &stmts[start + 1 + consumed];
            if let Some((path, element)) = self.match_push(&stmt.kind, local) {
                let Some(idx) = targets.iter().position(|t| t.path == path) else {
                    break;
                };
                pushes_per_target[idx].push(element.clone());
                consumed += 1;
                all_done = pushes_per_target
                    .iter()
                    .zip(&targets)
                    .all(|(p, t)| p.len() == t.capacity);
            } else if let Some((local_index, value)) = temp_binding(&stmt.kind, allow_impure) {
                // A `let temp = value` for a fresh element temp; remember it so
                // a following push that reads `temp` resolves to `value`.
                bindings.push((local_index, value.clone()));
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
                .filter(|e| is_local(e, *idx))
                .count();
            if uses == 1 {
                for element in pushes_per_target.iter_mut().flatten() {
                    if is_local(element, *idx) {
                        *element = value.clone();
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
        let reads_after = |idx: u32| rest.iter().any(|s| stmt_reads_local(s, idx));
        let reads_in_element = |idx: u32| {
            pushes_per_target
                .iter()
                .flatten()
                .any(|e| expr_reads_local(e, idx))
        };
        if bindings
            .iter()
            .any(|(idx, _)| reads_after(*idx) || reads_in_element(*idx))
        {
            return 0;
        }

        // Rewrite each `Array<T> { array_new(N), used: 0 }` struct to the
        // materialized `ArrayLiteral`.
        if let Some(value) = init_value_mut(&mut stmts[start].kind) {
            let mut elements_by_path: Vec<(Vec<u32>, Vec<NirExpr>)> = targets
                .iter()
                .map(|t| t.path.clone())
                .zip(pushes_per_target)
                .collect();
            rewrite_array_targets(value, &mut Vec::new(), &mut elements_by_path);
        }
        consumed
    }

    /// Match a `PLACE.push(elem)` statement where `PLACE` roots at `local`.
    /// Returns the field path from `local` to the array and the pushed element.
    fn match_push<'e>(&self, kind: &'e NirStmtKind, local: u32) -> Option<(Vec<u32>, &'e NirExpr)> {
        let NirStmtKind::Expr(NirExpr {
            kind:
                NirExprKind::MethodCall {
                    receiver,
                    func,
                    args,
                    ..
                },
            ..
        }) = kind
        else {
            return None;
        };
        if !self.push_names.contains(&func.name) || args.len() != 1 {
            return None;
        }
        let path = place_path(receiver, local)?;
        Some((path, &args[0].expr))
    }
}

/// If `kind` is `let temp = value`, return the local index and the bound
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
fn temp_binding(kind: &NirStmtKind, allow_impure: bool) -> Option<(u32, &NirExpr)> {
    match kind {
        NirStmtKind::Let {
            local_index, value, ..
        } if allow_impure || crate::optimize::elide_local::is_pure_expr(value) => {
            Some((*local_index, value))
        }
        _ => None,
    }
}

/// Visitor that records whether a given local is read anywhere in a subtree.
struct LocalReads {
    local: u32,
    found: bool,
}

impl crate::nir_visitor::NirRefVisitor for LocalReads {
    fn visit_expr(&mut self, expr: &NirExpr) {
        if let NirExprKind::Local { index, .. } = &expr.kind
            && *index == self.local
        {
            self.found = true;
        }
        self.walk_expr(expr);
    }
}

/// Whether `stmt` reads `local` anywhere in its subtree.
fn stmt_reads_local(stmt: &NirStmt, local: u32) -> bool {
    use crate::nir_visitor::NirRefVisitor;
    let mut v = LocalReads {
        local,
        found: false,
    };
    v.visit_stmt(stmt);
    v.found
}

/// Whether `expr` reads `local` anywhere in its subtree.
fn expr_reads_local(expr: &NirExpr, local: u32) -> bool {
    use crate::nir_visitor::NirRefVisitor;
    let mut v = LocalReads {
        local,
        found: false,
    };
    v.visit_expr(expr);
    v.found
}

/// Whether `expr` is a bare `Local(index)` reference.
fn is_local(expr: &NirExpr, index: u32) -> bool {
    matches!(&expr.kind, NirExprKind::Local { index: i, .. } if *i == index)
}

/// A detected `Array<T> { repr: array_new(N), used: 0 }` struct, with the
/// field path from the init's bound local and its `array_new` capacity.
struct ArrayTarget {
    path: Vec<u32>,
    capacity: usize,
}

/// The local a `Let` binds or an `Assign`-to-local sets.
fn init_local(kind: &NirStmtKind) -> Option<u32> {
    match kind {
        NirStmtKind::Let { local_index, .. } => Some(*local_index),
        NirStmtKind::Expr(NirExpr {
            kind: NirExprKind::Assign { target, .. },
            ..
        }) => match &target.kind {
            NirExprKind::Local { index, .. } => Some(*index),
            _ => None,
        },
        _ => None,
    }
}

fn init_value(kind: &NirStmtKind) -> Option<&NirExpr> {
    match kind {
        NirStmtKind::Let { value, .. } => Some(value),
        NirStmtKind::Expr(NirExpr {
            kind: NirExprKind::Assign { value, .. },
            ..
        }) => Some(value),
        _ => None,
    }
}

fn init_value_mut(kind: &mut NirStmtKind) -> Option<&mut NirExpr> {
    match kind {
        NirStmtKind::Let { value, .. } => Some(value),
        NirStmtKind::Expr(NirExpr {
            kind: NirExprKind::Assign { value, .. },
            ..
        }) => Some(value),
        _ => None,
    }
}

/// Walk an init value, recording each `Array<T> { array_new(N), used: 0 }`
/// struct with the field path from the value's root. Descends through the
/// outer block tail (`{ …; *__b }` produced for direct literals) and through
/// wrapper `StructLiteral` fields.
fn collect_array_targets(expr: &NirExpr, path: &mut Vec<u32>, out: &mut Vec<ArrayTarget>) {
    match &expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            // The direct-literal block binds `__b` to the array and yields it
            // via a `*__b` / `__b` tail; the array struct is the let value.
            if let Some(value) = block.stmts.iter().find_map(|s| match &s.kind {
                NirStmtKind::Let { value, .. } => Some(value),
                _ => None,
            }) {
                collect_array_targets(value, path, out);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            // Only collapse non-empty literals. A capacity-0 `array_new(0)` is
            // indistinguishable from a growable-array initialization (`let mut
            // v = []; v.push(…)`); collapsing it to a fixed 0-length
            // `array.new_fixed()` would break subsequent growth.
            if let Some(capacity) = match_array_struct(&expr.kind).filter(|&n| n > 0) {
                out.push(ArrayTarget {
                    path: path.clone(),
                    capacity,
                });
            } else {
                // A wrapper struct: recurse into each field, extending the path.
                for field in fields {
                    path.push(field.field_index);
                    collect_array_targets(&field.value, path, out);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

/// If `kind` is an `Array<T> { repr: array_new(N), used: 0 }` struct, return N.
fn match_array_struct(kind: &NirExprKind) -> Option<usize> {
    let NirExprKind::StructLiteral { fields, .. } = kind else {
        return None;
    };
    if fields.len() != 2 {
        return None;
    }
    let repr = fields.iter().find(|f| f.name == REPR_FIELD)?;
    let used = fields.iter().find(|f| f.name == USED_FIELD)?;
    if !is_zero_int(&used.value) {
        return None;
    }
    array_new_capacity(&repr.value)
}

/// If `expr` is a `builtin::array_new(N)` call with a constant `N`, return N.
fn array_new_capacity(expr: &NirExpr) -> Option<usize> {
    let NirExprKind::Call { func, args, .. } = &expr.kind else {
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
    match &args[0].expr.kind {
        NirExprKind::IntLiteral { value, .. } => usize::try_from(*value).ok(),
        _ => None,
    }
}

fn is_zero_int(expr: &NirExpr) -> bool {
    matches!(&expr.kind, NirExprKind::IntLiteral { value, .. } if *value == 0)
}

/// Rewrite the `Array<T>` structs collected by [`collect_array_targets`] to
/// `ArrayLiteral`, matching by field path. Consumes the element vectors.
fn rewrite_array_targets(
    expr: &mut NirExpr,
    path: &mut Vec<u32>,
    elements_by_path: &mut [(Vec<u32>, Vec<NirExpr>)],
) {
    let is_array_struct = match_array_struct(&expr.kind).is_some();
    match &mut expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            if let Some(value) = block.stmts.iter_mut().find_map(|s| match &mut s.kind {
                NirStmtKind::Let { value, .. } => Some(value),
                _ => None,
            }) {
                rewrite_array_targets(value, path, elements_by_path);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            if is_array_struct {
                if let Some((_, elements)) = elements_by_path.iter_mut().find(|(p, _)| p == path) {
                    expr.kind = NirExprKind::ArrayLiteral {
                        elements: std::mem::take(elements),
                    };
                }
            } else {
                for field in fields {
                    path.push(field.field_index);
                    rewrite_array_targets(&mut field.value, path, elements_by_path);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

/// If `receiver` is `local` reached through zero or more field accesses,
/// return the field-index path (`[]` for the bare local). The builder methods
/// take `&mut self`, so peel a leading reference.
fn place_path(receiver: &NirExpr, local: u32) -> Option<Vec<u32>> {
    let mut path = Vec::new();
    let mut cur = peel_ref(receiver);
    loop {
        match &cur.kind {
            NirExprKind::Local { index, .. } if *index == local => {
                path.reverse();
                return Some(path);
            }
            NirExprKind::FieldAccess {
                expr, field_index, ..
            } => {
                path.push(*field_index);
                cur = expr;
            }
            _ => return None,
        }
    }
}

fn peel_ref(expr: &NirExpr) -> &NirExpr {
    match &expr.kind {
        NirExprKind::Unary {
            op: crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef,
            expr: inner,
        } => peel_ref(inner),
        _ => expr,
    }
}
