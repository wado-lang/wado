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
//! [`recognize_value_copy`] is the single-call recognizer.
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
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::nir_visitor::NirRefVisitor;
use crate::niri::{AliasInfo, LocalSet};
use crate::tir::{ResolvedType, TypeId, TypeTable};

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
    body: &NirBlock,
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
    {
        let mut collector = AliasCollector { out: &mut aliased };
        collector.visit_block(body);
    }
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
    body: &NirBlock,
    type_table: &TypeTable,
    extra_edges: &[(u32, u32)],
) -> IndexMap<u32, IndexSet<u32>> {
    let mut edges: Vec<(u32, u32)> = extra_edges.to_vec();
    {
        let mut collector = AliasEdgeCollector {
            type_table,
            edges: &mut edges,
        };
        collector.visit_block(body);
    }
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

struct AliasEdgeCollector<'a> {
    type_table: &'a TypeTable,
    edges: &'a mut Vec<(u32, u32)>,
}

impl NirRefVisitor for AliasEdgeCollector<'_> {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        if let NirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let NirExprKind::Local { index: src, .. } = &value.kind
            && type_creates_alias(value.type_id, self.type_table)
        {
            self.edges.push((*local_index, *src));
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        if let NirExprKind::Assign { target, value } = &expr.kind
            && let NirExprKind::Local { index: dst, .. } = &target.kind
            && let NirExprKind::Local { index: src, .. } = &value.kind
            && type_creates_alias(value.type_id, self.type_table)
        {
            self.edges.push((*dst, *src));
        }
        self.walk_expr(expr);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Body-visible aliasing (transient inlined-in copies, captures, struct stores)
// ──────────────────────────────────────────────────────────────────────────────

/// Augments the seeded `aliased` set with body-visible aliasing
/// markers. Conservative — false positives only cost missed
/// optimizations.
struct AliasCollector<'a> {
    out: &'a mut LocalSet,
}

impl NirRefVisitor for AliasCollector<'_> {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        match &stmt.kind {
            // `let dst = src` (Local→Local copy) → both share storage.
            NirStmtKind::Let {
                local_index, value, ..
            } => {
                if let NirExprKind::Local { index: src, .. } = &value.kind {
                    self.out.insert(*local_index);
                    self.out.insert(*src);
                }
            }
            NirStmtKind::Expr(expr) => {
                // `dst = src` (Assign Local→Local) — same aliasing.
                if let NirExprKind::Assign { target, value } = &expr.kind
                    && let NirExprKind::Local { index: dst, .. } = &target.kind
                    && let NirExprKind::Local { index: src, .. } = &value.kind
                {
                    self.out.insert(*dst);
                    self.out.insert(*src);
                }
            }
            NirStmtKind::LetDestructure { .. }
            | NirStmtKind::Return { .. }
            | NirStmtKind::Break { .. }
            | NirStmtKind::If { .. }
            | NirStmtKind::Loop { .. }
            | NirStmtKind::LabeledBlock { .. }
            | NirStmtKind::Continue => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            // `&local` or `&mut local` escapes a reference. The OLD
            // WIR-level pass distinguished by `stores` annotation, but
            // at NIR we don't have a callee-level view here — be
            // conservative and treat any Ref/MutRef on a Local as
            // alias-creating.
            NirExprKind::Unary {
                op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
                expr: inner,
            } => {
                if let NirExprKind::Local { index, .. } = &inner.kind {
                    self.out.insert(*index);
                }
            }
            // Calls with mut args may stash the reference — alias.
            NirExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.out.insert(*index);
                    }
                }
            }
            NirExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref: receiver may be passed as `&mut self`.
                if let NirExprKind::Local { index, .. } = &receiver.kind {
                    self.out.insert(*index);
                }
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.out.insert(*index);
                    }
                }
            }
            // Locals stored as field values of a fresh aggregate become
            // reachable through that aggregate; future reads through the
            // aggregate (including via captured-closure access or stored
            // references) may modify them. Mark aliased.
            NirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    if let NirExprKind::Local { index, .. } = &field.value.kind {
                        self.out.insert(*index);
                    }
                }
            }
            NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
                for elem in elements {
                    if let NirExprKind::Local { index, .. } = &elem.kind {
                        self.out.insert(*index);
                    }
                }
            }
            NirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                if let NirExprKind::Local { index, .. } = &p.kind {
                    self.out.insert(*index);
                }
            }
            NirExprKind::Unary { .. }
            | NirExprKind::Binary { .. }
            | NirExprKind::Cast { .. }
            | NirExprKind::CmRawCall { .. }
            | NirExprKind::IndirectCall { .. }
            | NirExprKind::ClosureToCanonical { .. }
            | NirExprKind::Block(_)
            | NirExprKind::LabeledBlock { .. }
            | NirExprKind::If { .. }
            | NirExprKind::Match { .. }
            | NirExprKind::Switch { .. }
            | NirExprKind::Assign { .. }
            | NirExprKind::Index { .. }
            | NirExprKind::FieldAccess { .. }
            | NirExprKind::VariantTag { .. }
            | NirExprKind::VariantTest { .. }
            | NirExprKind::VariantPayload { .. }
            | NirExprKind::VariantConstruct { payload: None, .. }
            | NirExprKind::GlobalVarSet { .. }
            | NirExprKind::Local { .. }
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit
            | NirExprKind::EnumConstruct { .. } => {}
        }
        self.walk_expr(expr);
    }
}
