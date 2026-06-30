//! Per-function alias analysis feeding the engine [`ValueGraph`] builder.
//!
//! [`build_alias_info`] computes the [`crate::niri::AliasInfo`] (`aliased`,
//! `untrackable`, `alias_groups`); [`builder_alias_sets`] wraps it with the
//! mutable-escape analysis ([`build_mut_escaped`]) so every engine-driven pass
//! feeds the [`ValueGraph`] the alias view it needs to bound heap-write
//! invalidation. The walk seeds `aliased` from the function's stable
//! `address_taken_locals` / `stores_aliased_locals` annotations plus a body
//! scan for transient inlined-in copies, builds the union-find of
//! reference-typed `let dst = src` aliases, and lifts `stores_aliased_locals`
//! verbatim into `untrackable`.
//!
//! TODO(optimizer): plumb the callee's `stores` annotation into
//! `AliasCollector` so a `Ref` / `MutRef` on a local that flows into a
//! `stores`-free callee no longer marks the local aliased. The current
//! unconditional mark over-approximates for the common
//! `(&self).field` / `(&mut self).field = ...` single-call patterns.
//!
//! [`ValueGraph`]: crate::nir_value_graph

use cranelift_entity::SecondaryMap;

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FuncId, NirUnaryOp};
use crate::nir_arena::{Body, ExprKind, NodeRef, Operand, StmtKind};
use crate::nir_package::NirPackage;
use crate::niri::{AliasInfo, LocalSet};
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Apply `f` to every node reachable from the body root, parents before
/// children. Visiting every node and matching on `Stmt` / `Expr` gives the
/// alias collectors the coverage they need.
fn walk_all(body: &Body, node: NodeRef, f: &mut impl FnMut(&Body, NodeRef)) {
    f(body, node);
    body.for_each_child(node, |c| walk_all(body, c, f));
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
    // Per-node hook fused into the walk; its returned local is marked aliased.
    mut extra_aliased: impl FnMut(&Body, NodeRef) -> Option<u32>,
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
        if let Some(r) = extra_aliased(body, node) {
            aliased.insert(r);
        }
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

/// Per-[`FuncId`] first-parameter type. Lets the mutable-escape scan decide
/// whether a method call takes `&self` (immutable, cannot mutate the receiver)
/// or `&mut self` from the callee's declared signature, without a whole-program
/// mod-ref analysis. Keyed by the callee's id (read off the call node), so no
/// `full_name` is resolved per lookup. Build once per pass over all functions.
pub(super) type FirstParamTypes = SecondaryMap<FuncId, Option<TypeId>>;

pub(super) fn first_param_types(project: &NirPackage) -> FirstParamTypes {
    let mut map = FirstParamTypes::new();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let (Some(id), Some(first_param)) = (func.id, func.params.first()) {
            map[id] = Some(first_param.type_id);
        }
    }
    map
}

/// The `aliased`, `untrackable`, and `mut_escaped` local sets the `ValueGraph`
/// builder needs, as plain `IndexSet`s. Wraps [`build_alias_info`] (for
/// `aliased` / `untrackable`) with the mutable-escape analysis (`mut_escaped`).
///
/// `mut_escaped` ⊆ `aliased` is the subset a call can actually mutate, derived
/// subtractively: a local is dropped only when its type is transitively free of
/// shared mutable state ([`CallImmutability::is_call_immutable`]) *and* it has
/// no syntactic mutable escape ([`collect_mut_escaped_node`]). Field-*write*
/// granularity uses the full `aliased`; only call effects consult `mut_escaped`.
pub(super) fn builder_alias_sets(
    body: &Body,
    locals: &[crate::nir::NirLocal],
    address_taken_locals: &IndexSet<u32>,
    stores_aliased_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
    call_immutability: &CallImmutability,
) -> (IndexSet<u32>, IndexSet<u32>, IndexSet<u32>) {
    // A mutating method call `recv.m(…)` (`&mut self`) aliases `recv` implicitly:
    // the NIR receiver is a bare `Local` with no `&mut recv` node, so
    // `collect_aliased_node` misses it and the value graph would forward `recv`'s
    // pre-call fields across the call (`x.bump(); x.value` — a miscompile). Mark
    // the receiver root via the `build_alias_info` hook, sharing its walk.
    let info = build_alias_info(
        body,
        locals,
        address_taken_locals,
        stores_aliased_locals,
        type_table,
        |body, node| {
            if let NodeRef::Expr(e) = node
                && let ExprKind::MethodCall {
                    receiver, func_id, ..
                } = &body.exprs[e].kind
                && let Some(re) = receiver.as_expr()
                && method_mutates_receiver(
                    body,
                    re,
                    *func_id,
                    first_param_types,
                    type_table,
                    true,
                    Some(call_immutability),
                )
            {
                projection_root_local(body, re)
            } else {
                None
            }
        },
    );
    let aliased: IndexSet<u32> = info.aliased.iter().collect();
    let mut_escaped = build_mut_escaped(
        body,
        locals,
        &aliased,
        stores_aliased_locals,
        type_table,
        first_param_types,
        call_immutability,
        &info.alias_groups,
    );
    (aliased, info.untrackable.iter().collect(), mut_escaped)
}

/// Compute the per-function `mut_escaped` set subtractively from `aliased`.
///
/// 1. Collect the body's *syntactic* mutable-escape sites
///    ([`collect_mut_escaped_node`]) plus `stores_aliased_locals` (an inlined
///    `stores`-annotated callee stashed the reference, mutability unknown).
/// 2. Keep every `aliased` local whose type is not provably call-immutable
///    *or* that has a syntactic mutable escape; drop the rest (provably
///    immutable: no callee can mutate them).
/// 3. Close the result over `alias_groups` (the union-find
///    [`build_alias_info`] already computed), so that mutating one member of an
///    alias group (a `let dst = src` reference copy, or two same-pointee
///    reference params) clobbers the whole group's fields.
fn build_mut_escaped(
    body: &Body,
    locals: &[crate::nir::NirLocal],
    aliased: &IndexSet<u32>,
    stores_aliased_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
    call_immutability: &CallImmutability,
    alias_groups: &IndexMap<u32, IndexSet<u32>>,
) -> IndexSet<u32> {
    let mut syntactic_mut: IndexSet<u32> = stores_aliased_locals.iter().copied().collect();
    walk_all(body, NodeRef::Block(body.root), &mut |body, node| {
        collect_mut_escaped_node(
            body,
            node,
            type_table,
            first_param_types,
            call_immutability,
            &mut syntactic_mut,
        );
    });
    let local_type = |idx: u32| locals.get(idx as usize).map(|l| l.type_id);
    // Keep a local in `mut_escaped` unless it is provably immutable across
    // calls — no syntactic mutable escape AND a transitively-immutable type.
    let mut esc: IndexSet<u32> = aliased
        .iter()
        .copied()
        .filter(|&v| {
            syntactic_mut.contains(&v)
                || local_type(v).is_none_or(|t| !call_immutability.is_call_immutable(t))
        })
        .collect();
    // Close over the reference alias groups: if any group member is mutably
    // escaped, every member's pointee may be mutated through it.
    if !alias_groups.is_empty() {
        let seed: Vec<u32> = esc.iter().copied().collect();
        for v in seed {
            if let Some(group) = alias_groups.get(&v) {
                for &member in group {
                    esc.insert(member);
                }
            }
        }
    }
    esc
}

/// Transitive "a value of this type cannot be mutated by a callee that receives
/// it by value or by immutable reference" predicate, memoised per pass.
///
/// A value escaping into a call is mutable through that call only if it carries
/// *shared mutable state*: a `&mut T`, a `Box<T>` / `List<T>` cell, a raw
/// `Array<T>`, a resource handle, a reactive cell, or a variant/generic/unknown
/// shape we cannot see into. A type free of all of those — primitives, plain
/// value structs of such fields (e.g. `i128 { low: i64, high: i64 }`), enums,
/// flags, and immutable `&T` references — is deep-copied on pass-by-value and
/// cannot be written through an immutable reference, so no callee can change
/// the fields the `ValueGraph` forwards. (Cross-handle mutation of a `&T`
/// pointee is handled separately by the alias-group closure in
/// [`build_mut_escaped`].)
///
/// Owns a struct-field map cloned from `project.structs`, so it borrows only
/// `type_table` for the pass's lifetime.
pub(super) struct CallImmutability<'a> {
    type_table: &'a TypeTable,
    struct_fields: IndexMap<(String, ModuleSource), Vec<TypeId>>,
    box_name: String,
    list_name: String,
    memo: std::cell::RefCell<IndexMap<TypeId, bool>>,
    /// Per-[`FuncId`]: the function's body provably writes through its receiver
    /// (param 0). Boxing erases the `&self` / `&mut self` distinction — both
    /// become a `Box<T>` param (see `lower/plan/boxing`) — so a receiver's
    /// mutability cannot be read off its type; only the callee body distinguishes
    /// `bump(&mut self) { *self = … }` from `fmt(&self)`. Computed by fixpoint.
    receiver_mutating: SecondaryMap<FuncId, bool>,
    /// Per-[`FuncId`]: the function has a body (so a not-mutating verdict is
    /// *proven*, not unknown). A callee that does not resolve here (extern /
    /// builtin / unstamped) stays conservatively mutating.
    has_body: SecondaryMap<FuncId, bool>,
}

impl<'a> CallImmutability<'a> {
    pub(super) fn new(project: &NirPackage, type_table: &'a TypeTable) -> Self {
        let struct_fields = project
            .structs
            .iter()
            .map(|s| {
                (
                    (s.name.clone(), s.module_source.clone()),
                    s.fields.iter().map(|f| f.type_id).collect(),
                )
            })
            .collect();
        let items = type_table.compiler_items();
        let box_name = items
            .struct_name(crate::compiler_item::CompilerItem::Box)
            .to_string();
        let list_name = items
            .struct_name(crate::compiler_item::CompilerItem::List)
            .to_string();
        let first_param_types = first_param_types(project);
        let (receiver_mutating, has_body) =
            compute_receiver_mutating(project, type_table, &first_param_types);
        Self {
            type_table,
            struct_fields,
            box_name,
            list_name,
            memo: std::cell::RefCell::default(),
            receiver_mutating,
            has_body,
        }
    }

    /// Whether the callee `func_id` writes through its receiver. `None` for a
    /// callee with no body (extern / builtin) — the caller falls back to its
    /// conservative default.
    pub(super) fn method_writes_receiver(&self, func_id: FuncId) -> Option<bool> {
        self.has_body[func_id].then(|| self.receiver_mutating[func_id])
    }

    pub(super) fn is_call_immutable(&self, type_id: TypeId) -> bool {
        self.walk(type_id, &mut Vec::new())
    }

    fn walk(&self, type_id: TypeId, stack: &mut Vec<TypeId>) -> bool {
        if let Some(&cached) = self.memo.borrow().get(&type_id) {
            return cached;
        }
        // A struct can only reach itself through a `Box`/`List`/reference
        // field (value types cannot be infinite-sized), all of which return
        // `false` before recursing, so a cycle implies an immutable shape.
        if stack.contains(&type_id) {
            return true;
        }
        stack.push(type_id);
        let result = match self.type_table.get(type_id) {
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            // `&T` is immutable: a callee cannot write through it. The pointee
            // being mutated through a *different* handle is the alias-group
            // closure's concern, not this type-shape check.
            | ResolvedType::Ref(_) => true,
            ResolvedType::Newtype { base_type, .. } => {
                let base = *base_type;
                self.walk(base, stack)
            }
            ResolvedType::Struct {
                name,
                module_source,
                base_name,
                ..
            } => {
                let is_box_or_list = base_name.as_deref() == Some(self.box_name.as_str())
                    || base_name.as_deref() == Some(self.list_name.as_str());
                if is_box_or_list {
                    false
                } else {
                    let key = (name.clone(), module_source.clone());
                    match self.struct_fields.get(&key) {
                        Some(fields) => fields.iter().all(|&f| self.walk(f, stack)),
                        // Unknown layout (not in the registry) — conservative.
                        None => false,
                    }
                }
            }
            // `&mut T`, `Box`/`List` (as `GenericInstance`), raw arrays,
            // resources, reactive cells, variants, generics, type params, and
            // unknowns may all carry shared mutable state — conservative.
            _ => false,
        };
        stack.pop();
        self.memo.borrow_mut().insert(type_id, result);
        result
    }
}

use super::arena_query::projection_root_local;

/// Call exprs that **mutate no caller local**: a free `Call` or `&self`
/// `MethodCall` whose every argument is safe — not `mut`, an immutable `&`
/// borrow, or a by-value value of a call-immutable type (a deep copy the callee
/// cannot reach back through). The value graph can skip a `mut_escaped`
/// receiver's per-call bump for these, so a field read keeps its version across a
/// pure accessor (`arr.len()` does not split `arr.used`'s version) — the
/// precondition for promoting a spliced `FieldAccess` (WEP: the field-bound
/// recovery). Unknown callees stay impure for a `MethodCall` receiver
/// (`method_mutates_receiver`'s conservative default); a free `Call` is pure when
/// its args are all safe, since a callee can only mutate what it is handed
/// mutably.
pub(super) fn pure_calls(
    body: &Body,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
    call_immutability: &CallImmutability,
) -> IndexSet<crate::nir_arena::ExprId> {
    let arg_safe = |arg: &crate::nir_arena::ArenaCallArg| -> bool {
        if arg.is_mut {
            return false;
        }
        let Some(ae) = arg.expr.as_expr() else {
            return true; // a promoted constant operand mutates nothing
        };
        match &body.exprs[ae].kind {
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                ..
            } => true,
            ExprKind::Unary {
                op: NirUnaryOp::MutRef | NirUnaryOp::Deref,
                ..
            } => false,
            _ => call_immutability.is_call_immutable(body.exprs[ae].type_id),
        }
    };
    // Walk reachable nodes only: `body.exprs` also holds orphaned nodes left by
    // earlier promotion, whose stale `type_id`s are no longer in the table.
    let mut out = IndexSet::default();
    walk_all(body, NodeRef::Block(body.root), &mut |body, node| {
        let NodeRef::Expr(e) = node else {
            return;
        };
        let pure = match &body.exprs[e].kind {
            ExprKind::Call { args, .. } => args.iter().all(&arg_safe),
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } => {
                receiver.as_expr().is_some_and(|re| {
                    !method_mutates_receiver(
                        body,
                        re,
                        *func_id,
                        first_param_types,
                        type_table,
                        true,
                        Some(call_immutability),
                    )
                }) && args.iter().all(&arg_safe)
            }
            _ => false,
        };
        if pure {
            out.insert(e);
        }
    });
    out
}

/// Whether a `recv.m(...)` call may mutate its receiver's pointee: the receiver
/// expression is itself a `&mut`, or the callee's declared first param is
/// `&mut self`. `conservative_on_unknown` decides a callee absent from
/// `first_param_types` — `alias`'s mutable-escape scan passes `true` (assume it
/// mutates), while `copy_prop`'s copy-propagation guard passes `false` (it has
/// its own receiver-type guards). Shared so the two analyses agree on the
/// known-callee verdict.
pub(super) fn method_mutates_receiver(
    body: &Body,
    receiver: crate::nir_arena::ExprId,
    func_id: FuncId,
    first_param_types: &FirstParamTypes,
    type_table: &TypeTable,
    conservative_on_unknown: bool,
    call_immutability: Option<&CallImmutability>,
) -> bool {
    let receiver_is_mut_ref = matches!(
        type_table.get(body.exprs[receiver].type_id),
        ResolvedType::MutRef(_)
    );
    // The receiver type alone cannot tell `&self` from `&mut self` once boxing
    // has made both a `Box<T>` param, so consult the body-derived verdict: a
    // method that writes through its receiver mutates it, one that does not is
    // proven safe. A bodyless callee (`None` verdict) falls back to the legacy
    // `MutRef`-param check (still valid for a genuine `&mut Struct` receiver) or
    // the conservative default. `call_immutability` is `None` for the
    // copy-propagation caller, which keeps its own type-based receiver guard.
    let callee_mutates = match call_immutability.and_then(|ci| ci.method_writes_receiver(func_id)) {
        Some(writes) => writes,
        None => match first_param_types[func_id] {
            Some(tp) => matches!(type_table.get(tp), ResolvedType::MutRef(_)),
            None => conservative_on_unknown,
        },
    };
    receiver_is_mut_ref || callee_mutates
}

/// Fixpoint over the call graph: the set of methods whose body provably writes
/// through their receiver (param 0), plus the set of methods that have a body
/// at all. A method writes through its receiver when the body assigns into a
/// projection of param 0 (`self.f = …`, `*self = …`, `self[i] = …`), takes a
/// `&mut` of such a place, passes one as a `mut` call argument, or calls a
/// receiver-mutating method on such a place. The last clause is the fixpoint:
/// `outer(&self) { self.inner() }` mutates its receiver iff `inner` does, so we
/// iterate until the set stops growing. Conservative on every bodyless callee.
fn compute_receiver_mutating(
    project: &NirPackage,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) -> (SecondaryMap<FuncId, bool>, SecondaryMap<FuncId, bool>) {
    let mut has_body: SecondaryMap<FuncId, bool> = SecondaryMap::new();
    let mut p0_of: SecondaryMap<FuncId, Option<u32>> = SecondaryMap::new();
    for func_rc in &project.functions {
        let f = func_rc.borrow();
        let Some(id) = f.id else { continue };
        if f.body.is_some()
            && let Some(p0) = f.params.first()
        {
            has_body[id] = true;
            p0_of[id] = Some(p0.local_index);
        }
    }
    // Walk each body once to extract a fixpoint-invariant summary: whether it
    // writes through its receiver directly, and the bodied callees it invokes on
    // a self projection (the only verdicts that vary as `mutating` grows). The
    // fixpoint then iterates these summaries with cheap integer lookups instead
    // of re-walking every body each round.
    struct Summary {
        id: FuncId,
        direct: bool,
        pending: Vec<FuncId>,
    }
    let mut summaries: Vec<Summary> = Vec::new();
    for func_rc in &project.functions {
        let f = func_rc.borrow();
        let Some(id) = f.id else { continue };
        let (Some(body), Some(p0)) = (f.body.as_ref(), p0_of[id]) else {
            continue;
        };
        let (direct, pending) =
            summarize_receiver_writes(body, p0, &has_body, first_param_types, type_table);
        summaries.push(Summary {
            id,
            direct,
            pending,
        });
    }
    let mut mutating: SecondaryMap<FuncId, bool> = SecondaryMap::new();
    for s in &summaries {
        if s.direct {
            mutating[s.id] = true;
        }
    }
    loop {
        let mut changed = false;
        for s in &summaries {
            if mutating[s.id] {
                continue;
            }
            if s.pending.iter().any(|&c| mutating[c]) {
                mutating[s.id] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (mutating, has_body)
}

/// One walk of `body` extracting param-0's receiver-write summary:
///
/// - `direct`: a fixpoint-invariant write through param-0's projection root
///   `p0` — `self.f = …` / `*self = …`, `&mut <self projection>`, a `mut` call
///   argument rooted at self, or a receiver-projecting method call into a
///   bodyless / unstamped callee that mutates its receiver (verdict fixed by the
///   callee's declared first-param type, conservative when unknown).
/// - `pending`: ids of bodied callees invoked on a self projection. Their verdict
///   is `mutating[id]`, resolved by the fixpoint as the set grows. `direct`
///   short-circuits the walk; `pending` is collected only while no direct write
///   has been seen.
fn summarize_receiver_writes(
    body: &Body,
    p0: u32,
    has_body: &SecondaryMap<FuncId, bool>,
    first_param_types: &FirstParamTypes,
    type_table: &TypeTable,
) -> (bool, Vec<FuncId>) {
    let projects_p0 = |e: crate::nir_arena::ExprId| -> bool {
        super::arena_query::projection_root_local(body, e) == Some(p0)
    };
    let mut direct = false;
    let mut pending: Vec<FuncId> = Vec::new();
    walk_all(body, NodeRef::Block(body.root), &mut |body, node| {
        if direct {
            return;
        }
        let NodeRef::Expr(e) = node else { return };
        match &body.exprs[e].kind {
            // A write to a projection of self: `self.f = …`, `*self = …`.
            ExprKind::Assign { target, .. } if projects_p0(*target) => direct = true,
            // `&mut <self projection>` hands a mutable place out.
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } if inner.as_expr().is_some_and(projects_p0) => direct = true,
            ExprKind::Call { args, .. } => {
                if args
                    .iter()
                    .any(|a| a.is_mut && a.expr.as_expr().is_some_and(projects_p0))
                {
                    direct = true;
                }
            }
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } => {
                if receiver.as_expr().is_some_and(projects_p0) {
                    // self-rooted receiver: mutated iff the callee mutates its
                    // own receiver. A bodied callee (stamped id with a body) →
                    // defer to the fixpoint; an unstamped / bodyless callee →
                    // conservative via its declared first-param type.
                    if has_body[*func_id] {
                        pending.push(*func_id);
                    } else {
                        let inner_mut = match first_param_types[*func_id] {
                            Some(tp) => matches!(type_table.get(tp), ResolvedType::MutRef(_)),
                            None => true,
                        };
                        if inner_mut {
                            direct = true;
                        }
                    }
                }
                if args
                    .iter()
                    .any(|a| a.is_mut && a.expr.as_expr().is_some_and(projects_p0))
                {
                    direct = true;
                }
            }
            _ => {}
        }
    });
    (direct, pending)
}

/// Flag the roots of mutable-escape sites in a single node:
///
/// - `&mut v` — the inner place's root is mutably referenced.
/// - `f(…, mut arg, …)` — a mut-ref call argument's root.
/// - `recv.m(…)` where `m` takes `&mut self` (or the receiver expression is
///   already a `&mut`), plus the method's own mut-ref arguments. Receiver
///   mutability comes from the callee's declared first-param type
///   (`first_param_types`); an unknown callee is treated conservatively as
///   mutating its receiver.
///
/// Immutable `&v` is deliberately *not* a mutable escape: a `&self` receiver
/// or `&T` argument cannot mutate the pointee, so the local's fields survive
/// across the call.
fn collect_mut_escaped_node(
    body: &Body,
    node: NodeRef,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
    call_immutability: &CallImmutability,
    out: &mut IndexSet<u32>,
) {
    let NodeRef::Expr(e) = node else {
        return;
    };
    match &body.exprs[e].kind {
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            // A promoted `Operand::Value` inner has no projection root local.
            if let Some(ie) = inner.as_expr()
                && let Some(r) = projection_root_local(body, ie)
            {
                out.insert(r);
            }
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                // A `mut` arg is a place; a promoted constant references no local.
                if arg.is_mut
                    && let Some(e) = arg.expr.as_expr()
                    && let Some(r) = projection_root_local(body, e)
                {
                    out.insert(r);
                }
            }
        }
        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            // Unknown callee (builtin / extern not in the project) → assume it
            // may mutate the receiver (`conservative_on_unknown = true`). A
            // promoted-value receiver carries no local root, so it aliases
            // nothing.
            if let Some(re) = receiver.as_expr()
                && method_mutates_receiver(
                    body,
                    re,
                    *func_id,
                    first_param_types,
                    type_table,
                    true,
                    Some(call_immutability),
                )
                && let Some(r) = projection_root_local(body, re)
            {
                out.insert(r);
            }
            for arg in args {
                if arg.is_mut
                    && let Some(r) = arg
                        .expr
                        .as_expr()
                        .and_then(|e| projection_root_local(body, e))
                {
                    out.insert(r);
                }
            }
        }
        _ => {}
    }
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
        // Look through a `type T = U` newtype to its base, so two references to
        // the same object whose pointee surfaces as a newtype still share a key
        // (and thus an alias group). Otherwise `&Newtype` / `&mut Newtype` to
        // one object would not be grouped, and `mut_escaped`'s group closure
        // could leave an immutable handle's fields stale across a mutating call.
        ResolvedType::Newtype { base_type, .. } => {
            reference_pointee_struct_key(*base_type, type_table)
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
                && let Some(ve) = value.as_expr()
                && let ExprKind::Local { index: src, .. } = &body.exprs[ve].kind
                && type_creates_alias(body.exprs[ve].type_id, type_table)
            {
                edges.push((*local_index, *src));
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::Assign { target, value } = &body.exprs[e].kind
                && let ExprKind::Local { index: dst, .. } = &body.exprs[*target].kind
                && let Some(ve) = value.as_expr()
                && let ExprKind::Local { index: src, .. } = &body.exprs[ve].kind
                && type_creates_alias(body.exprs[ve].type_id, type_table)
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
                if let Some(src) = value.as_expr().and_then(local) {
                    out.insert(*local_index);
                    out.insert(src);
                }
            }
            // `dst = src` (Assign Local→Local) — same aliasing.
            StmtKind::Expr(Operand::Expr(expr)) => {
                if let ExprKind::Assign { target, value } = &body.exprs[*expr].kind
                    && let Some(dst) = local(*target)
                    && let Some(src) = value.as_expr().and_then(local)
                {
                    out.insert(dst);
                    out.insert(src);
                }
            }
            StmtKind::Expr(Operand::Value(_)) => {}
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
                if let Some(index) = inner.as_expr().and_then(local) {
                    out.insert(index);
                }
            }
            // Calls with mut args may stash the reference — alias.
            ExprKind::Call { args, .. } => {
                for arg in args {
                    // A `mut` arg is a place (never a promoted constant); a
                    // constant arg references no local.
                    if arg.is_mut
                        && let Some(e) = arg.expr.as_expr()
                        && let Some(index) = local(e)
                    {
                        out.insert(index);
                    }
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref: receiver may be passed as `&mut self`.
                if let Some(index) = receiver.as_expr().and_then(local) {
                    out.insert(index);
                }
                for arg in args {
                    if arg.is_mut
                        && let Some(index) = arg.expr.as_expr().and_then(local)
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
                    if let Some(index) = field.value.as_expr().and_then(local) {
                        out.insert(index);
                    }
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for &elem in elements {
                    if let Some(index) = elem.as_expr().and_then(local) {
                        out.insert(index);
                    }
                }
            }
            ExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                if let Some(index) = p.as_expr().and_then(local) {
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
            | ExprKind::PackedArray(_)
            | ExprKind::Dead
            | ExprKind::EnumConstruct { .. } => {}
        },
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
}
