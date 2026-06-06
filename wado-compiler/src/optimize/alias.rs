//! Per-function alias analysis used by const-fold's field-knowledge
//! tracking.
//!
//! [`build_alias_info`] computes the [`crate::niri::AliasInfo`] that
//! [`crate::niri::Interpreter`] consults whenever the const-fold
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
//! helpers (see `lower::plan::value_copy::synthesize`).
//!
//! [`recognize_value_copy_a`] is the single-call recognizer.
//!
//! TODO(optimizer): plumb the callee's `stores` annotation into
//! `AliasCollector` so a `Ref` / `MutRef` on a local that flows into a
//! `stores`-free callee no longer marks the local aliased. The current
//! unconditional mark over-approximates for the common
//! `(&self).field` / `(&mut self).field = ...` single-call patterns
//! and blocks const-fold's field-knowledge tracking across inlined
//! `&self` shadows.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprKind, NodeRef, StmtKind};
use crate::nir_package::NirPackage;
use crate::niri::{AliasInfo, LocalSet};
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Apply `f` to every node reachable from the body root, parents before
/// children. The arena counterpart of the tree [`crate::nir_visitor`] walk the
/// alias collectors used; visiting every node and matching on `Stmt` / `Expr`
/// gives the same coverage.
fn walk_all(body: &Body, node: NodeRef, f: &mut impl FnMut(&Body, NodeRef)) {
    f(body, node);
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        walk_all(body, c, f);
    }
}

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
///   Local→Local copies in `body` (`Box<T>`, `List<T>`, `&T`,
///   `&mut T`). Used to widen field-assignment invalidation: writing
///   `dst.field = …` drops the same field on every alias.
pub(super) fn build_alias_info(
    body: &Body,
    locals: &[crate::nir::NirLocal],
    address_taken_locals: &IndexSet<u32>,
    stores_aliased_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> AliasInfo {
    // Seed dense bitsets sized to the function's local count; local indices
    // are dense (`0..locals.len()`), so membership stays hash-free.
    let mut aliased = LocalSet::with_capacity(locals.len());
    for &idx in address_taken_locals {
        aliased.insert(idx);
    }
    for &idx in stores_aliased_locals {
        aliased.insert(idx);
    }
    let mut untrackable = LocalSet::with_capacity(locals.len());
    for &idx in stores_aliased_locals {
        untrackable.insert(idx);
    }
    walk_all(body, NodeRef::Block(body.root), &mut |body, node| {
        collect_aliased_node(body, node, &mut aliased);
    });
    // Reference parameters/locals pointing at the same struct may alias the
    // same heap object (Wado references alias, no borrow checker), so a write
    // through one must invalidate field knowledge of the others. Treat them as
    // a mutual alias group, and mark them `aliased` so a call boundary (opaque
    // mutation through any handle) drops their fields too.
    let same_pointee_edges = same_pointee_reference_edges(locals, type_table);
    for &(a, b) in &same_pointee_edges {
        aliased.insert(a);
        aliased.insert(b);
    }
    let alias_groups = collect_alias_groups(body, type_table, &same_pointee_edges);
    AliasInfo {
        aliased,
        untrackable,
        alias_groups,
    }
}

/// Recognize `Call(helper, [arg])` where `helper` is a synthesized
/// `$value_copy$T<id>` registered in the helpers map, reading the arena
/// body. Returns the argument expression id so the caller can copy
/// `arg`'s field knowledge to the binding's target.
pub(super) fn recognize_value_copy_a(
    body: &crate::nir_arena::Body,
    e: crate::nir_arena::ExprId,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> Option<crate::nir_arena::ExprId> {
    let crate::nir_arena::ExprKind::Call { func, args, .. } = &body.exprs[e].kind else {
        return None;
    };
    if args.len() != 1 {
        return None;
    }
    helpers
        .get(&(func.module_source.clone(), func.name.clone()))
        .map(|_| args[0].expr)
}

// ──────────────────────────────────────────────────────────────────────────────
// Alias group analysis (union-find over reference-typed copies)
// ──────────────────────────────────────────────────────────────────────────────

/// Build the alias-group map. Two locals end up in the same group
/// when they're connected by a chain of `let dst = src` Local→Local
/// copies of a reference-typed value (`Box<T>`, `List<T>`, `&T`,
/// `&mut T`). For value-semantic types (plain structs, variants),
/// `let dst = src` will later be wrapped in `$value_copy$T(src)` by
/// the value-copy synthesis pass — `dst` is then a fresh allocation
/// and does not share storage with `src`, so we don't connect them.
///
/// The group is used to widen field-assignment invalidation: writing
/// `dst.field = ...` invalidates the same field of every alias.
/// The struct identity a reference type points at, stripping `Ref`/`MutRef`.
/// `None` for non-reference types or references to non-struct pointees
/// (primitives, boxed primitives) whose fields const-fold never tracks.
fn reference_pointee_struct_key(
    type_id: TypeId,
    type_table: &TypeTable,
) -> Option<(String, ModuleSource)> {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            reference_pointee_struct_key(*inner, type_table)
        }
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => Some((name.clone(), module_source.clone())),
        _ => None,
    }
}

/// Edges connecting reference locals (`func.locals` is indexed by local index,
/// params included) that point at the same struct. Two such references may
/// alias the same heap object, so a write through one must widen invalidation
/// to the others. Connected as a star to each pointee's first-seen local.
fn same_pointee_reference_edges(
    locals: &[crate::nir::NirLocal],
    type_table: &TypeTable,
) -> Vec<(u32, u32)> {
    let mut rep: IndexMap<(String, ModuleSource), u32> = IndexMap::default();
    let mut edges = Vec::new();
    for (i, l) in locals.iter().enumerate() {
        let Some(key) = reference_pointee_struct_key(l.type_id, type_table) else {
            continue;
        };
        match rep.get(&key) {
            Some(&r) => edges.push((r, i as u32)),
            None => {
                rep.insert(key, i as u32);
            }
        }
    }
    edges
}

fn collect_alias_groups(
    body: &Body,
    type_table: &TypeTable,
    extra_edges: &[(u32, u32)],
) -> IndexMap<u32, IndexSet<u32>> {
    let mut edges: Vec<(u32, u32)> = extra_edges.to_vec();
    walk_all(body, NodeRef::Block(body.root), &mut |body, node| {
        collect_alias_edges_node(body, node, type_table, &mut edges);
    });
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

/// True when assigning a value of `type_id` from one local to another
/// produces aliasing — both names refer to the same heap object. This
/// is the case for reference types (`Box<T>`, `List<T>`, `&T`,
/// `&mut T`). Value-semantic types (plain structs, variants) are
/// turned into a `$value_copy$T(src)` wrapper post-loop, so during
/// the loop a `let dst = src` edge between two value-typed locals
/// would over-merge groups that should stay separate.
///
/// `Box<T>` and `List<T>` may surface either as `GenericInstance`
/// (pre-monomorphization) or as concrete monomorphized `Struct`
/// records carrying the original generic name in `base_name`.
fn type_creates_alias(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Ref(_) => true,
        ResolvedType::GenericInstance { name, .. } if name == "Box" || name == "List" => true,
        ResolvedType::Struct { base_name, .. }
            if base_name.as_deref() == Some("Box") || base_name.as_deref() == Some("List") =>
        {
            true
        }
        ResolvedType::Primitive(_)
        | ResolvedType::Unit
        | ResolvedType::Never
        | ResolvedType::MutRef(_)
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::Function { .. }
        | ResolvedType::Reactive(_)
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::Newtype { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Unknown
        | ResolvedType::Error => false,
    }
}

/// Record one alias-group edge if `node` is a reference-typed Local→Local
/// `let dst = src` or `dst = src`.
fn collect_alias_edges_node(
    body: &Body,
    node: NodeRef,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::Let {
                local_index, value, ..
            } = &body.stmts[s].kind
                && let ExprKind::Local { index: src, .. } = &body.exprs[*value].kind
                && type_creates_alias(body.exprs[*value].type_id, type_table)
            {
                edges.push((*local_index, *src));
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::Assign { target, value } = &body.exprs[e].kind
                && let ExprKind::Local { index: dst, .. } = &body.exprs[*target].kind
                && let ExprKind::Local { index: src, .. } = &body.exprs[*value].kind
                && type_creates_alias(body.exprs[*value].type_id, type_table)
            {
                edges.push((*dst, *src));
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Body-visible aliasing (transient inlined-in copies, captures, struct stores)
// ──────────────────────────────────────────────────────────────────────────────

/// Augments the seeded `aliased` set with body-visible aliasing markers for a
/// single arena node. Conservative — false positives only cost missed
/// optimizations.
fn collect_aliased_node(body: &Body, node: NodeRef, out: &mut LocalSet) {
    let local = |id: crate::nir_arena::ExprId| -> Option<u32> {
        match &body.exprs[id].kind {
            ExprKind::Local { index, .. } => Some(*index),
            _ => None,
        }
    };
    match node {
        NodeRef::Stmt(s) => match &body.stmts[s].kind {
            // `let dst = src` (Local→Local copy) → both share storage.
            StmtKind::Let {
                local_index, value, ..
            } => {
                if let Some(src) = local(*value) {
                    out.insert(*local_index);
                    out.insert(src);
                }
            }
            // `dst = src` (Assign Local→Local) — same aliasing.
            StmtKind::Expr(expr) => {
                if let ExprKind::Assign { target, value } = &body.exprs[*expr].kind
                    && let Some(dst) = local(*target)
                    && let Some(src) = local(*value)
                {
                    out.insert(dst);
                    out.insert(src);
                }
            }
            StmtKind::LetDestructure { .. }
            | StmtKind::Return { .. }
            | StmtKind::Break { .. }
            | StmtKind::If { .. }
            | StmtKind::Loop { .. }
            | StmtKind::LabeledBlock { .. }
            | StmtKind::Continue => {}
        },
        NodeRef::Expr(e) => match &body.exprs[e].kind {
            // `&local` or `&mut local` escapes a reference. The OLD
            // WIR-level pass distinguished by `stores` annotation, but
            // at NIR we don't have a callee-level view here — be
            // conservative and treat any Ref/MutRef on a Local as
            // alias-creating.
            ExprKind::Unary {
                op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
                expr: inner,
            } => {
                if let Some(index) = local(*inner) {
                    out.insert(index);
                }
            }
            // Calls with mut args may stash the reference — alias.
            ExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let Some(index) = local(arg.expr)
                    {
                        out.insert(index);
                    }
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref: receiver may be passed as `&mut self`.
                if let Some(index) = local(*receiver) {
                    out.insert(index);
                }
                for arg in args {
                    if arg.is_mut
                        && let Some(index) = local(arg.expr)
                    {
                        out.insert(index);
                    }
                }
            }
            // Locals stored as field values of a fresh aggregate become
            // reachable through that aggregate; future reads through the
            // aggregate (including via captured-closure access or stored
            // references) may modify them. Mark aliased.
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    if let Some(index) = local(field.value) {
                        out.insert(index);
                    }
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for &elem in elements {
                    if let Some(index) = local(elem) {
                        out.insert(index);
                    }
                }
            }
            ExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                if let Some(index) = local(*p) {
                    out.insert(index);
                }
            }
            ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Cast { .. }
            | ExprKind::CmRawCall { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::ClosureToCanonical { .. }
            | ExprKind::Block(_)
            | ExprKind::LabeledBlock { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. }
            | ExprKind::Assign { .. }
            | ExprKind::Index { .. }
            | ExprKind::FieldAccess { .. }
            | ExprKind::VariantTag { .. }
            | ExprKind::VariantTest { .. }
            | ExprKind::VariantPayload { .. }
            | ExprKind::VariantConstruct { payload: None, .. }
            | ExprKind::GlobalVarSet { .. }
            | ExprKind::Local { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::IntLiteral { .. }
            | ExprKind::FloatLiteral { .. }
            | ExprKind::StringLiteral(_)
            | ExprKind::BytesLiteral(_)
            | ExprKind::BoolLiteral(_)
            | ExprKind::CharLiteral(_)
            | ExprKind::Null
            | ExprKind::Unit
            | ExprKind::EnumConstruct { .. } => {}
        },
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
}
