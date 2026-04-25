//! TIR-level struct field constant forwarding.
//!
//! Tracks per-local field constants along straight-line code, propagating
//! them through:
//!
//! - `let local = StructLiteral { ... }` — record each forwardable field
//! - `let dst = $value_copy$T(src)` — recognize calls to
//!   `FunctionKind::ValueCopy` helpers (synthesized in `lower::value_copy`)
//!   and copy `src`'s field knowledge to `dst`, which is a fresh deep copy
//!   carrying the same field values. This is the TIR replacement for the
//!   WIR-level `WirInstr::ValueCopy` arm in `wir_optimize::const_forward`.
//! - `let dst = local` — copy `local`'s knowledge to `dst` (only meaningful
//!   for reference-typed locals; value-typed copies always go through the
//!   `$value_copy$T(src)` wrapper above).
//!
//! Replaces field reads (`local.field`) with the recorded value when
//! known. Invalidates entries on field assignment, full reassignment,
//! address-take, capture, or call args that may mutate the local.
//!
//! Runs inside the optimization loop so that newly-exposed `StructLiteral`
//! / `$value_copy$T<id>` patterns from inlining cascade into further
//! folding.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId, TypeTable,
};

/// `(local_index, field_name)` → forwardable constant value. Only
/// literals are recorded — see [`is_forwardable`] for why `Local` is
/// excluded.
type FieldKey = (u32, String);

/// Per-function field-value knowledge tracked along straight-line code.
#[derive(Default, Clone)]
struct FieldKnowledge {
    /// Known field values. Stored expressions are always *forwardable*
    /// (see [`is_forwardable`]) so substituting them at a use site
    /// doesn't change semantics.
    fields: IndexMap<FieldKey, TirExpr>,
    /// Locals that may be aliased — both sides of a `let dst = src`
    /// Local→Local copy of a reference-typed value (`Box<T>`,
    /// `Array<T>`, `&T`, `&mut T`), `&` / `&mut` references that
    /// escape, and locals captured by closures. Value-typed
    /// `let dst = src` is wrapped in `$value_copy$T(src)` by the
    /// lower phase, so it doesn't create aliasing here. Field
    /// knowledge IS recorded for aliased locals
    /// (`record_struct_literal` / `copy_from` never gate on aliasing);
    /// the flow-sensitive walk drops their entries at every
    /// side-effect boundary (call, dereferenced write, etc.) where an
    /// unseen alias could have mutated the storage.
    aliased: IndexSet<u32>,
    /// Locals whose aliasing is *untrackable*: an inlined
    /// `stores`-annotated call has stashed their reference somewhere
    /// the optimizer can't see, so subsequent reads might observe
    /// mutations from arbitrary points. Field knowledge is **never**
    /// recorded for these locals; that's the conservative behavior
    /// the OLD WIR-level `const_forward` had for stores-passed args.
    untrackable: IndexSet<u32>,
    /// Maps each local to its full alias group — every other local
    /// that shares the same underlying storage. Populated only by
    /// `let dst = src` Local→Local copies, which create true storage
    /// aliasing for reference types (`Box<T>`, `Array<T>`, etc.).
    /// `&local` / `&mut local` and struct-field captures put the
    /// local in [`Self::aliased`] without merging groups — they
    /// expose access through the receiver but don't bind two named
    /// locals to the same slot. Lookup returns an empty set for
    /// solo locals.
    alias_groups: IndexMap<u32, IndexSet<u32>>,
}

impl FieldKnowledge {
    /// Record forwardable fields from a `StructLiteral { f0: e0, ... }`
    /// assigned to `local_index`. Skipped for *untrackable* locals
    /// (the conservative-aliasing case where an inlined stores call
    /// makes any later read potentially observe an unseen mutation).
    /// Plain `aliased` locals do still get recorded; the flow-
    /// sensitive walk drops them at side-effect points.
    fn record_struct_literal(&mut self, local_index: u32, fields: &[crate::tir::TirStructField]) {
        if self.untrackable.contains(&local_index) {
            return;
        }
        for field in fields {
            if is_forwardable(&field.value) {
                self.fields
                    .insert((local_index, field.name.clone()), field.value.clone());
            }
        }
    }

    /// Copy every recorded field of `src` to `dst`. Skipped when
    /// `dst` is *untrackable* (see [`Self::untrackable`]).
    fn copy_from(&mut self, src: u32, dst: u32) {
        if self.untrackable.contains(&dst) {
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

    /// Drop knowledge for every aliased local. Called at side-effect
    /// boundaries (calls, dereferenced writes) where some external
    /// code could have mutated the storage through an alias.
    fn invalidate_aliased(&mut self) {
        if self.aliased.is_empty() {
            return;
        }
        let aliased = self.aliased.clone();
        self.fields.retain(|(idx, _), _| !aliased.contains(idx));
    }

    /// Invalidate all knowledge about `local_index` — the local was
    /// fully reassigned, captured, or had its address taken with mut
    /// access.
    fn invalidate_local(&mut self, local_index: u32) {
        self.fields.retain(|(idx, _), _| *idx != local_index);
    }

    /// Invalidate just `(local_index, field)` — the field was assigned
    /// directly via `local.field = expr`.
    fn invalidate_field(&mut self, local_index: u32, field_name: &str) {
        self.fields
            .swap_remove(&(local_index, field_name.to_string()));
        if let Some(group) = self.alias_groups.get(&local_index).cloned() {
            for other in &group {
                if *other == local_index {
                    continue;
                }
                self.fields.swap_remove(&(*other, field_name.to_string()));
            }
        }
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

/// True when `expr` (positioned as a struct/tuple/variant field
/// value) lets the freshly built aggregate capture access to an
/// already-aliased local. The check is conservative: any explicit
/// `Ref(Local)` / `MutRef(Local)` plus any plain `Local(idx)` whose
/// `idx` is in the aliased set counts. The plain-Local case covers
/// implicit reference coercion at field-store sites (e.g. assigning
/// `p: Pair` into a `pair: &Pair` field).
fn value_captures_alias(expr: &TirExpr, aliased: &IndexSet<u32>) -> bool {
    match &expr.kind {
        TirExprKind::Unary { op, expr: inner } => {
            (matches!(
                op,
                crate::tir::TirUnaryOp::Ref | crate::tir::TirUnaryOp::MutRef
            ) && matches!(inner.kind, TirExprKind::Local { .. }))
                || value_captures_alias(inner, aliased)
        }
        TirExprKind::Local { index, .. } => aliased.contains(index),
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            value_captures_alias(inner, aliased)
        }
        _ => false,
    }
}

/// True when an expression is safe to forward into a use site —
/// substituting it preserves semantics regardless of the surrounding
/// state. Restricted to literals: a `Local` substitution would read
/// the local's *current* value at the use site, which differs from
/// the snapshot the field captured if the local was mutated in
/// between via any path the analyzer doesn't fully track (mutation
/// through a `&mut`-passed callee, dereferenced write through an
/// alias, …). The WIR-level `const_forward` admitted `Local` because
/// its caller had already proven the local immutable; that proof is
/// not available to a flow-sensitive TIR pass.
fn is_forwardable(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
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
    let type_table = project.type_table.clone();
    let type_table = type_table.borrow();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        // Seed with the function's stable aliasing annotations: any
        // local that ever had its address taken (`&x` / `&mut x`) and
        // any local stored across a `stores`-annotated callee. These
        // sets persist across optimization iterations, so subsequent
        // passes (ref_elim, SROA) erasing the syntactic markers won't
        // make us forget the alias.
        let mut aliased = func.address_taken_locals.clone();
        for idx in &func.stores_aliased_locals {
            aliased.insert(*idx);
        }
        // `stores_aliased_locals` is the strictly stronger
        // "untrackable" set: an inlined stores callee has stashed the
        // reference somewhere the analyzer cannot see, so any later
        // read may observe a mutation we never witnessed. Refuse to
        // record fields for these locals (matches the OLD WIR-level
        // const_forward conservatism).
        let untrackable = func.stores_aliased_locals.clone();
        let Some(ref mut body) = func.body else {
            continue;
        };
        // Augment `aliased` with locals whose aliasing is visible only
        // in the current body (e.g. inlined-in copies). Conservative —
        // extra entries only mean missed optimizations.
        collect_aliased_in_block(body, &mut aliased);
        let alias_groups = collect_alias_groups(body, &type_table);
        let mut known = FieldKnowledge {
            aliased,
            untrackable,
            alias_groups,
            ..Default::default()
        };
        changed |= forward_in_block(body, &mut known, &helpers);
    }
    changed
}

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
fn collect_alias_groups(body: &TirBlock, type_table: &TypeTable) -> IndexMap<u32, IndexSet<u32>> {
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
    block: &TirBlock,
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
    stmt: &TirStmt,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if let TirExprKind::Local { index: src, .. } = &value.kind
                && type_creates_alias(value.type_id, type_table)
            {
                edges.push((*local_index, *src));
            }
            collect_alias_edges_in_expr(value, type_table, edges);
        }
        TirStmtKind::LetDestructure { value, .. } => {
            collect_alias_edges_in_expr(value, type_table, edges);
        }
        TirStmtKind::Expr(expr) => {
            if let TirExprKind::Assign { target, value } = &expr.kind
                && let TirExprKind::Local { index: dst, .. } = &target.kind
                && let TirExprKind::Local { index: src, .. } = &value.kind
                && type_creates_alias(value.type_id, type_table)
            {
                edges.push((*dst, *src));
            }
            collect_alias_edges_in_expr(expr, type_table, edges);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_alias_edges_in_expr(v, type_table, edges);
            }
        }
        TirStmtKind::If {
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
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_alias_edges_in_block(body, type_table, edges);
        }
        TirStmtKind::IfLet {
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
    expr: &TirExpr,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    expr_for_each_child(expr, &mut |child| {
        collect_alias_edges_in_expr(child, type_table, edges);
    });
}

/// Walk `expr`'s direct sub-expressions. Used to recurse without
/// duplicating the case list at every call site.
fn expr_for_each_child(expr: &TirExpr, f: &mut dyn FnMut(&TirExpr)) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            f(target);
            f(value);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => f(inner),
        TirExprKind::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
            if let TirExprKind::MethodCall { receiver, .. } = &expr.kind {
                f(receiver);
            }
            for arg in args {
                f(&arg.expr);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                f(arg);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            f(callee);
            for arg in args {
                f(arg);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => f(functor),
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            f(inner);
            f(index);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            for stmt in &block.stmts {
                stmt_for_each_child(stmt, f);
            }
        }
        TirExprKind::If {
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
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                f(&field.value);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                f(elem);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                f(p);
            }
        }
        TirExprKind::Closure { body, .. } => f(body),
        TirExprKind::Match { expr: inner, arms } => {
            f(inner);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    f(g);
                }
                f(&arm.body);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => f(value),
        TirExprKind::Switch {
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
        _ => {}
    }
}

fn stmt_for_each_child(stmt: &TirStmt, f: &mut dyn FnMut(&TirExpr)) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => f(value),
        TirStmtKind::Expr(e) => f(e),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                f(v);
            }
        }
        TirStmtKind::If {
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
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            for s in &body.stmts {
                stmt_for_each_child(s, f);
            }
        }
        TirStmtKind::IfLet {
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

/// Augment `out` with body-visible aliasing markers. Used in addition
/// to the function's stable `address_taken_locals` /
/// `stores_aliased_locals` to catch transient aliasings introduced by
/// inlining. Conservative — false positives only cost missed
/// optimizations.
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
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
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
                // `local.field = expr` is precise: only `local.field`
                // changes. Other aliased locals don't share storage
                // for this specific field — they may share the
                // underlying object, but writing through `local`
                // names a specific field-slot we tracked separately.
                TirExprKind::Local { index, .. } => {
                    known.invalidate_field(*index, field_name);
                    if is_forwardable(value) && !known.untrackable.contains(index) {
                        known
                            .fields
                            .insert((*index, field_name.clone()), (**value).clone());
                    }
                }
                // Anything more complex than `local.field = expr`
                // (e.g. `(*p).field = ...` or `q.outer.inner = ...`)
                // could mutate aliased state we don't track. Drop
                // every aliased local's fields — non-aliased locals
                // are out of reach for the unknown receiver.
                _ => known.invalidate_aliased(),
            },
            // Writes through Deref / Index / etc. may alias any
            // aliased local; non-aliased locals are unreachable.
            _ => known.invalidate_aliased(),
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
            // Invalidate based on target shape. `local.field = ...`
            // invalidates precisely (only that field of that local).
            // Anything more complex (e.g. `*self = ...`, `arr[i] =
            // ...`) is opaque about which storage it hits — fall back
            // to invalidating every aliased local.
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
                    _ => known.invalidate_aliased(),
                },
                _ => known.invalidate_aliased(),
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
        TirExprKind::Call { func, args, .. } => {
            let is_value_copy = args.len() == 1
                && helpers
                    .get(&(func.module_source.clone(), func.name.clone()))
                    .is_some();
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, helpers);
                if arg.is_mut
                    && let TirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    known.invalidate_local(*index);
                }
            }
            // The callee may have mutated any of its captured /
            // stored / aliased operands. Drop their fields. Skip for
            // synthesized `$value_copy$T(arg)` helpers — those are
            // pure shallow copies that don't mutate `arg` and the
            // caller (update_knowledge_from_let) wants to copy field
            // knowledge from `arg` to the binding's target.
            if !is_value_copy {
                known.invalidate_aliased();
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(arg, known, helpers);
            }
            known.invalidate_aliased();
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= forward_in_expr(receiver, known, helpers);
            // Auto-ref hides &mut self, so the local receiver may
            // have been mutated by the call.
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
            known.invalidate_aliased();
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            changed |= forward_in_expr(callee, known, helpers);
            for arg in args {
                changed |= forward_in_expr(arg, known, helpers);
            }
            // Indirect callee is unknown — closures may capture and
            // mutate any aliased local.
            known.invalidate_aliased();
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
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            changed |= forward_in_expr(inner, known, helpers);
            changed |= forward_in_expr(index, known, helpers);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            // Plain Block / LabeledBlock are sequential scopes (not
            // re-entry points): outer knowledge flows in, statements
            // update it in order, and any `break label: value` exits
            // the scope — there's no loop-back. Walk the body with
            // the *same* `known` so subsequent reads inside the
            // block can use values established outside, then drop
            // the post-block knowledge: a break could have skipped
            // some interior writes that would otherwise have
            // invalidated entries.
            changed |= forward_in_block(block, known, helpers);
            known.clear();
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
            let mut captures_alias = false;
            for field in fields {
                changed |= forward_in_expr(&mut field.value, known, helpers);
                if value_captures_alias(&field.value, &known.aliased) {
                    captures_alias = true;
                }
            }
            // The freshly constructed aggregate captures access to any
            // aliased local that appears in a field value (whether
            // explicitly via `&x` or implicitly via type-coerced
            // `Local`). Subsequent code reaching the aggregate could
            // mutate those locals through the captured handle.
            if captures_alias {
                known.invalidate_aliased();
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            let mut captures_alias = false;
            for elem in elements {
                changed |= forward_in_expr(elem, known, helpers);
                if value_captures_alias(elem, &known.aliased) {
                    captures_alias = true;
                }
            }
            if captures_alias {
                known.invalidate_aliased();
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= forward_in_expr(p, known, helpers);
                if value_captures_alias(p, &known.aliased) {
                    known.invalidate_aliased();
                }
            }
        }
        TirExprKind::Closure { body, .. } => {
            // Closure body executes in its own scope — clear and walk.
            known.clear();
            let mut inner = FieldKnowledge {
                aliased: known.aliased.clone(),
                untrackable: known.untrackable.clone(),
                alias_groups: known.alias_groups.clone(),
                ..Default::default()
            };
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
        TirStmtKind::Loop { body } => {
            // Loop bodies can re-execute and re-assign anything; drop
            // outer knowledge and start fresh inside.
            known.clear();
            let mut inner = FieldKnowledge {
                aliased: known.aliased.clone(),
                untrackable: known.untrackable.clone(),
                alias_groups: known.alias_groups.clone(),
                ..Default::default()
            };
            changed |= forward_in_block(body, &mut inner, helpers);
        }
        TirStmtKind::LabeledBlock { block: body, .. } => {
            // LabeledBlock is a sequential scope (not a re-entry
            // point): outer knowledge flows in. Walk in-place; drop
            // post-block knowledge to be safe about interior breaks.
            changed |= forward_in_block(body, known, helpers);
            known.clear();
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
