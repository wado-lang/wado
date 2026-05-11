//! Per-function alias analysis used by const-fold's field-knowledge
//! tracking.
//!
//! [`build_alias_info`] computes the [`crate::tiri::AliasInfo`] that
//! [`crate::tiri::Interpreter`] consults whenever the const-fold
//! visitor calls `bind_field` / `invalidate_field` /
//! `invalidate_aliased_fields`. The structure of this module mirrors
//! the original `field_forward` pass (issue #1009) — a flow-
//! insensitive walk that seeds the `aliased` set from the function's
//! stable annotations plus a body scan for transient inlined-in
//! copies, builds the union-find of reference-typed `let dst = src`
//! aliases, and lifts the `stores_aliased_locals` set verbatim into
//! `untrackable`.
//!
//! [`build_value_copy_helpers`] bundles the synthesized
//! `$value_copy$T<id>` helpers exposed by
//! [`crate::nir::NirFunction::value_copy_type`] into the lookup the
//! visitor uses to recognize `Call(helper, [arg])` shapes that
//! transfer field knowledge across the one-level shallow value-copy
//! helpers (see `lower::value_copy::synthesize`).
//!
//! [`recognize_value_copy`] is the single-call recognizer.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::tiri::AliasInfo;

/// Build the `(module_source, func_name) → struct type id` map of
/// synthesized `$value_copy$T<id>` helpers. The const-fold visitor
/// uses the map to recognize `Call(helper, [arg])` shapes that
/// transfer field knowledge from `arg` to the binding's target.
pub(super) fn build_value_copy_helpers(
    project: &NirPackage,
) -> IndexMap<(ModuleSource, String), TypeId> {
    project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            f.value_copy_type()
                .map(|t| ((f.module_source.clone(), f.name.clone()), t))
        })
        .collect()
}

/// Compute per-function alias annotations for a function body.
///
/// Returns an [`AliasInfo`] populated as follows:
///
/// - `aliased`: seeds from `address_taken_locals` ∪
///   `stores_aliased_locals`, then augmented with locals whose
///   aliasing is visible only inside `body` (transient inlined-in
///   copies, captures, struct-field-stores). The seeded sets persist
///   across optimization iterations, so subsequent passes (`ref_elim`,
///   SROA) erasing the syntactic markers can't make us forget the
///   alias.
/// - `untrackable`: mirrors `stores_aliased_locals` exactly. An
///   inlined `stores`-annotated callee has stashed the reference
///   somewhere the analyzer cannot see, so any later read may
///   observe a mutation we never witnessed. The const-fold visitor
///   refuses to record fields for these locals (matches the OLD
///   WIR-level `const_forward` conservatism).
/// - `alias_groups`: union-find over reference-typed `let dst = src`
///   Local→Local copies in `body` (`Box<T>`, `Array<T>`, `&T`,
///   `&mut T`). Used to widen field-assignment invalidation: writing
///   `dst.field = …` drops the same field on every alias.
pub(super) fn build_alias_info(
    body: &NirBlock,
    address_taken_locals: &IndexSet<u32>,
    stores_aliased_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> AliasInfo {
    let mut aliased = address_taken_locals.clone();
    for idx in stores_aliased_locals {
        aliased.insert(*idx);
    }
    let untrackable = stores_aliased_locals.clone();
    collect_aliased_in_block(body, &mut aliased);
    let alias_groups = collect_alias_groups(body, type_table);
    AliasInfo {
        aliased,
        untrackable,
        alias_groups,
    }
}

/// Recognize `Call(helper, [arg])` where `helper` is a synthesized
/// `$value_copy$T<id>` registered in the helpers map. Returns the
/// argument expression so the caller can copy `arg`'s field
/// knowledge to the binding's target.
pub(super) fn recognize_value_copy<'a>(
    expr: &'a NirExpr,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> Option<&'a NirExpr> {
    let NirExprKind::Call { func, args, .. } = &expr.kind else {
        return None;
    };
    if args.len() != 1 {
        return None;
    }
    helpers
        .get(&(func.module_source.clone(), func.name.clone()))
        .map(|_| &args[0].expr)
}

// ──────────────────────────────────────────────────────────────────────────────
// Aliasing collection
// ──────────────────────────────────────────────────────────────────────────────

/// Build the alias-group map. Two locals end up in the same group
/// when they're connected by a chain of `let dst = src` Local→Local
/// copies of a reference-typed value (`Box<T>`, `Array<T>`, `&T`,
/// `&mut T`). For value-semantic types (plain structs, variants),
/// `let dst = src` will later be wrapped in `$value_copy$T(src)` by
/// the value-copy synthesis pass — `dst` is then a fresh allocation
/// and does not share storage with `src`, so we don't connect them.
///
/// The group is used to widen field-assignment invalidation: writing
/// `dst.field = ...` invalidates the same field of every alias.
fn collect_alias_groups(body: &NirBlock, type_table: &TypeTable) -> IndexMap<u32, IndexSet<u32>> {
    let mut edges: Vec<(u32, u32)> = Vec::new();
    collect_alias_edges_in_block(body, type_table, &mut edges);
    if edges.is_empty() {
        return IndexMap::default();
    }
    // Union-find via simple parent pointers; locals are sparse u32s.
    let mut parent: IndexMap<u32, u32> = IndexMap::default();
    fn find(parent: &mut IndexMap<u32, u32>, x: u32) -> u32 {
        let p = *parent.get(&x).unwrap_or(&x);
        if p == x {
            x
        } else {
            let r = find(parent, p);
            parent.insert(x, r);
            r
        }
    }
    for (a, b) in edges {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let ra = find(&mut parent, a);
        let rb = find(&mut parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let keys: Vec<u32> = parent.keys().copied().collect();
    let mut groups: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    for k in &keys {
        let r = find(&mut parent, *k);
        groups.entry(r).or_default().insert(*k);
    }
    let mut out: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    for set in groups.into_values() {
        for &k in &set {
            out.insert(k, set.clone());
        }
    }
    out
}

fn collect_alias_edges_in_block(
    block: &NirBlock,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    for stmt in &block.stmts {
        collect_alias_edges_in_stmt(stmt, type_table, edges);
    }
}

/// True when assigning a value of `type_id` from one local to another
/// produces aliasing — both names refer to the same heap object. This
/// is the case for reference types (`Box<T>`, `Array<T>`, `&T`,
/// `&mut T`). Value-semantic types (plain structs, variants) are
/// turned into a `$value_copy$T(src)` wrapper post-loop, so during
/// the loop a `let dst = src` edge between two value-typed locals
/// would over-merge groups that should stay separate.
///
/// `Box<T>` and `Array<T>` may surface either as `GenericInstance`
/// (pre-monomorphization) or as concrete monomorphized `Struct`
/// records carrying the original generic name in `base_name`.
fn type_creates_alias(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Ref { .. } => true,
        ResolvedType::GenericInstance { name, .. } if name == "Box" || name == "Array" => true,
        ResolvedType::Struct { base_name, .. }
            if base_name.as_deref() == Some("Box") || base_name.as_deref() == Some("Array") =>
        {
            true
        }
        _ => false,
    }
}

fn collect_alias_edges_in_stmt(
    stmt: &NirStmt,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    match &stmt.kind {
        NirStmtKind::Let {
            local_index, value, ..
        } => {
            if let NirExprKind::Local { index: src, .. } = &value.kind
                && type_creates_alias(value.type_id, type_table)
            {
                edges.push((*local_index, *src));
            }
            collect_alias_edges_in_expr(value, type_table, edges);
        }
        NirStmtKind::LetDestructure { value, .. } => {
            collect_alias_edges_in_expr(value, type_table, edges);
        }
        NirStmtKind::Expr(expr) => {
            if let NirExprKind::Assign { target, value } = &expr.kind
                && let NirExprKind::Local { index: dst, .. } = &target.kind
                && let NirExprKind::Local { index: src, .. } = &value.kind
                && type_creates_alias(value.type_id, type_table)
            {
                edges.push((*dst, *src));
            }
            collect_alias_edges_in_expr(expr, type_table, edges);
        }
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_alias_edges_in_expr(v, type_table, edges);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_alias_edges_in_expr(condition, type_table, edges);
            collect_alias_edges_in_block(then_block, type_table, edges);
            if let Some(eb) = else_block {
                collect_alias_edges_in_block(eb, type_table, edges);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            collect_alias_edges_in_block(body, type_table, edges);
        }
        NirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_alias_edges_in_expr(scrutinee, type_table, edges);
            collect_alias_edges_in_block(then_block, type_table, edges);
            if let Some(eb) = else_block {
                collect_alias_edges_in_block(eb, type_table, edges);
            }
        }
        _ => {}
    }
}

fn collect_alias_edges_in_expr(
    expr: &NirExpr,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    expr_for_each_child(expr, &mut |child| {
        collect_alias_edges_in_expr(child, type_table, edges);
    });
}

/// Walk `expr`'s direct sub-expressions. Used to recurse without
/// duplicating the case list at every call site.
fn expr_for_each_child(expr: &NirExpr, f: &mut dyn FnMut(&NirExpr)) {
    match &expr.kind {
        NirExprKind::Assign { target, value } => {
            f(target);
            f(value);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => f(inner),
        NirExprKind::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        NirExprKind::Call { args, .. } | NirExprKind::MethodCall { args, .. } => {
            if let NirExprKind::MethodCall { receiver, .. } = &expr.kind {
                f(receiver);
            }
            for arg in args {
                f(&arg.expr);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                f(arg);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            f(callee);
            for arg in args {
                f(arg);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => f(functor),
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            f(inner);
            f(index);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            for stmt in &block.stmts {
                stmt_for_each_child(stmt, f);
            }
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            f(condition);
            for stmt in &then_branch.stmts {
                stmt_for_each_child(stmt, f);
            }
            if let Some(eb) = else_branch {
                for stmt in &eb.stmts {
                    stmt_for_each_child(stmt, f);
                }
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                f(&field.value);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                f(elem);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                f(p);
            }
        }
        NirExprKind::Closure { body, .. } => f(body),
        NirExprKind::Match { expr: inner, arms } => {
            f(inner);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    f(g);
                }
                f(&arm.body);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => f(value),
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            f(scrutinee);
            for arm in arms {
                for stmt in &arm.stmts {
                    stmt_for_each_child(stmt, f);
                }
            }
            for stmt in &default.stmts {
                stmt_for_each_child(stmt, f);
            }
        }
        NirExprKind::Local { .. }
        | NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => {}
        NirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        NirExprKind::WithHandler { .. } | NirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

fn stmt_for_each_child(stmt: &NirStmt, f: &mut dyn FnMut(&NirExpr)) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => f(value),
        NirStmtKind::Expr(e) => f(e),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                f(v);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            f(condition);
            for s in &then_block.stmts {
                stmt_for_each_child(s, f);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    stmt_for_each_child(s, f);
                }
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            for s in &body.stmts {
                stmt_for_each_child(s, f);
            }
        }
        NirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            f(scrutinee);
            for s in &then_block.stmts {
                stmt_for_each_child(s, f);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    stmt_for_each_child(s, f);
                }
            }
        }
        _ => {}
    }
}

/// Augment `out` with body-visible aliasing markers. Used in
/// addition to the function's stable `address_taken_locals` /
/// `stores_aliased_locals` to catch transient aliasings introduced
/// by inlining. Conservative — false positives only cost missed
/// optimizations.
fn collect_aliased_in_block(block: &NirBlock, out: &mut IndexSet<u32>) {
    for stmt in &block.stmts {
        collect_aliased_in_stmt(stmt, out);
    }
}

fn collect_aliased_in_stmt(stmt: &NirStmt, out: &mut IndexSet<u32>) {
    match &stmt.kind {
        // `let dst = src` (Local→Local copy) → both share storage.
        NirStmtKind::Let {
            local_index, value, ..
        } => {
            if let NirExprKind::Local { index: src, .. } = &value.kind {
                out.insert(*local_index);
                out.insert(*src);
            }
            collect_aliased_in_expr(value, out);
        }
        NirStmtKind::LetDestructure { value, .. } => collect_aliased_in_expr(value, out),
        NirStmtKind::Expr(expr) => {
            // `dst = src` (Assign Local→Local) — same aliasing.
            if let NirExprKind::Assign { target, value } = &expr.kind
                && let NirExprKind::Local { index: dst, .. } = &target.kind
                && let NirExprKind::Local { index: src, .. } = &value.kind
            {
                out.insert(*dst);
                out.insert(*src);
            }
            collect_aliased_in_expr(expr, out);
        }
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_aliased_in_expr(v, out);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            collect_aliased_in_block(body, out);
        }
        NirStmtKind::IfLet {
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

fn collect_aliased_in_expr(expr: &NirExpr, out: &mut IndexSet<u32>) {
    match &expr.kind {
        // `&local` or `&mut local` escapes a reference. The OLD
        // WIR-level pass distinguished by `stores` annotation, but
        // at TIR we don't have a callee-level view here — be
        // conservative and treat any Ref/MutRef on a Local as
        // alias-creating.
        NirExprKind::Unary { op, expr: inner } => {
            if matches!(op, NirUnaryOp::MutRef | NirUnaryOp::Ref)
                && let NirExprKind::Local { index, .. } = &inner.kind
            {
                out.insert(*index);
            }
            collect_aliased_in_expr(inner, out);
        }
        // Calls with mut args may stash the reference — alias.
        NirExprKind::Call { args, .. } | NirExprKind::MethodCall { args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let NirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    out.insert(*index);
                }
                collect_aliased_in_expr(&arg.expr, out);
            }
            if let NirExprKind::MethodCall { receiver, .. } = &expr.kind {
                // Auto-ref: receiver may be passed as `&mut self`.
                if let NirExprKind::Local { index, .. } = &receiver.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(receiver, out);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_aliased_in_expr(arg, out);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            collect_aliased_in_expr(callee, out);
            for arg in args {
                collect_aliased_in_expr(arg, out);
            }
        }
        NirExprKind::Closure { captures, body, .. } => {
            for capture in captures {
                out.insert(capture.outer_index);
            }
            collect_aliased_in_expr(body, out);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            collect_aliased_in_block(block, out);
        }
        NirExprKind::If {
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
        NirExprKind::Match { expr: inner, arms } => {
            collect_aliased_in_expr(inner, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    collect_aliased_in_expr(g, out);
                }
                collect_aliased_in_expr(&arm.body, out);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_aliased_in_expr(functor, out);
        }
        NirExprKind::Assign { target, value } => {
            collect_aliased_in_expr(target, out);
            collect_aliased_in_expr(value, out);
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_aliased_in_expr(left, out);
            collect_aliased_in_expr(right, out);
        }
        NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => {
            collect_aliased_in_expr(inner, out);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_aliased_in_expr(inner, out);
            collect_aliased_in_expr(index, out);
        }
        // Locals stored as field values of a fresh aggregate become
        // reachable through that aggregate; future reads through the
        // aggregate (including via captured-closure access or stored
        // references) may modify them. Mark aliased.
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                if let NirExprKind::Local { index, .. } = &field.value.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(&field.value, out);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                if let NirExprKind::Local { index, .. } = &elem.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(elem, out);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                if let NirExprKind::Local { index, .. } = &p.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(p, out);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => collect_aliased_in_expr(value, out),
        NirExprKind::Switch {
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
