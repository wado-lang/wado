//! `value_copy_demote` — demote a deep `$value_copy$T` of an `List<E>` to a
//! shallow spine copy when the binding's elements are provably never mutated
//! through it (nor through the source the copy reads from).
//!
//! `value_copy_elide` removes a `$value_copy$T` wrapper entirely when the
//! target is read-only — that aliases the binding to the source. When the
//! target is *spine*-mutated (`sort`, `push`, …) full elision is unsound, but
//! a shallow copy is still safe: the binding gets its own `repr` spine while
//! sharing the (immutable-through-this-handle) element objects. This pass
//! proves the element-immutability precondition and rewrites the call to a
//! synthesized shallow sibling helper that uses `array_clone_shallow`.
//!
//! The element-immutability analysis (`fn`s prefixed `analyze_`/`verify_`) is
//! shape-compatible with what `container_sroa` needs but currently kept
//! private to this module.
//!
//! TODO(optimizer): expose the element-immutability analysis so
//! `container_sroa` can replace its hardcoded method-shape whitelist
//! with this richer query, and so nested-`List<List<T>>` demotion can
//! reuse the recursive immutability proof. The recursion guard at
//! `is_element_immutable_method` returns `false` for any recursive
//! call site, suppressing self-recursive and mutually-recursive helpers.
//!
//! ## Arena traversal
//!
//! The pass reads and mutates the arena [`Body`] directly. The two
//! soundness-critical analyses (`ElementClean`, `ElementImmutable`) are
//! recursive arena walks over [`Body::for_each_child`], so every nested node —
//! including match-arm and destructure patterns, and `ConstantValue` pattern
//! sub-expressions — is covered exactly as the canonical tree visitor was.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::SeqField;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::nir::{FunctionKind, FunctionRef, NirFunction, NirParam, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeTable};

use super::arena_query::{expr_mentions_local, is_local, strip_refs};
use super::gate::{FunctionGate, FunctionId};
use cranelift_entity::EntityRef;

type FuncKey = (ModuleSource, String);

fn builtin_gname(func: &FunctionRef) -> Option<String> {
    func.builtin_name()
        .or_else(|| func.monomorphized_builtin_name())
}

/// Returns whether anything changed (a binding was retargeted or a shallow
/// specialization was added), so the optimizer's dirty-set gate can re-examine
/// the touched functions and accommodate the new ones.
pub fn demote_value_copies(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    // Map every function to its index for callee lookup.
    let mut by_key: IndexMap<FuncKey, usize> = IndexMap::default();
    for (i, f) in project.functions.iter().enumerate() {
        let f = f.borrow();
        by_key.insert((f.module_source.clone(), f.name.clone()), i);
    }

    // Identify `$value_copy$T` helpers whose body is an `List<E>` wrapper
    // copy: `return StructLiteral { repr: array_clone(v.repr), used: ... }`.
    let mut list_wrapper_copies: IndexSet<FuncKey> = IndexSet::default();
    for f in &project.functions {
        let f = f.borrow();
        if f.value_copy_type().is_some()
            && let Some(body) = &f.body
            && body_is_list_wrapper_copy(body)
        {
            list_wrapper_copies.insert((f.module_source.clone(), f.name.clone()));
        }
    }
    crate::compiler_trace!(
        "demote",
        "array-wrapper value-copy helpers: {}",
        list_wrapper_copies.len()
    );
    if list_wrapper_copies.is_empty() {
        return false;
    }

    let type_table = project.type_table.clone();
    let mut analyzer = Analyzer {
        funcs: &project.functions,
        by_key: &by_key,
        type_table: &type_table,
        eimm_memo: IndexMap::default(),
    };

    // Phase 1: per `(function, target-local)`, AND-combine the eligibility
    // of every value-copy-wrapper binding to that local. A local bound by
    // several wrapper copies is demoted only when *every* binding is
    // eligible — so the single `(fi, local)` key precisely drives the
    // rewrite, and a binding whose sibling is unsafe stays deep rather than
    // being collaterally demoted.
    let mut site_elig: IndexMap<(usize, u32), bool> = IndexMap::default();
    let mut site_key: IndexMap<(usize, u32), FuncKey> = IndexMap::default();
    for (fi, f) in project.functions.iter().enumerate() {
        let f = f.borrow();
        if f.value_copy_type().is_some() {
            continue;
        }
        let Some(body) = &f.body else {
            continue;
        };
        collect_sites(
            body,
            &list_wrapper_copies,
            &f.params,
            fi,
            &mut analyzer,
            &mut site_elig,
            &mut site_key,
        );
    }

    let mut demoted_keys: IndexSet<FuncKey> = IndexSet::default();
    for (loc, &elig) in &site_elig {
        if elig {
            demoted_keys.insert(site_key[loc].clone());
        }
    }
    crate::compiler_trace!("demote", "demoted helper keys: {}", demoted_keys.len());
    if demoted_keys.is_empty() {
        return false;
    }

    // Phase 2a: synthesize a shallow sibling helper per demoted deep helper.
    let mut shallow_name: IndexMap<FuncKey, String> = IndexMap::default();
    let mut new_funcs: Vec<Rc<RefCell<NirFunction>>> = Vec::new();
    for deep_key in &demoted_keys {
        let new_name = format!("{}$shallow", deep_key.1);
        // A prior fixed-point iteration may already have synthesized this
        // helper; reuse it rather than adding a duplicate-named function.
        if by_key.contains_key(&(deep_key.0.clone(), new_name.clone())) {
            shallow_name.insert(deep_key.clone(), new_name);
            continue;
        }
        let idx = by_key[deep_key];
        let mut shallow = project.functions[idx].borrow().clone();
        shallow.name.clone_from(&new_name);
        shallow.kind = FunctionKind::Regular;
        shallow.is_pub = false;
        shallow.is_export = false;
        if let Some(body) = &mut shallow.body {
            rewrite_array_clone_to_shallow(body);
        }
        shallow_name.insert(deep_key.clone(), new_name);
        new_funcs.push(Rc::new(RefCell::new(shallow)));
    }

    // Phase 2b: a pure mechanical rewrite — retarget exactly the
    // `(fi, local)` bindings marked eligible. No analysis here, so it needs
    // no `&project.functions` borrow and cannot conflict with the mutation.
    let mut touched: IndexSet<usize> = IndexSet::default();
    for ((fi, _), &elig) in &site_elig {
        if elig {
            touched.insert(*fi);
        }
    }
    let changed = !touched.is_empty() || !new_funcs.is_empty();
    for &fi in &touched {
        let mut f = project.functions[fi].borrow_mut();
        if let Some(body) = &mut f.body {
            retarget_block(body, fi, &site_elig, &list_wrapper_copies, &shallow_name);
        }
    }
    project.functions.extend(new_funcs);
    // Report the retargeted callers; the appended shallow specializations get a
    // fresh (dirty) gate slot via the gate's auto-grow on next access. The
    // retarget only redirects a `$value_copy$` call to its shallow twin, so the
    // caller's edge shifts but — as with `inline` — the staleness is quality,
    // not correctness.
    for fi in touched {
        gate.mark_changed(FunctionId::new(fi));
    }
    changed
}

// ---------------------------------------------------------------------------
// Reachable-node enumeration
// ---------------------------------------------------------------------------

/// Every `BlockId` reachable from the body root, in arbitrary order. Dead
/// blocks left by a prior in-place arena pass are skipped — matching the tree
/// bridge, which only ever materialized reachable nodes.
fn reachable_blocks(body: &Body) -> Vec<BlockId> {
    let mut out = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Block(b) = node {
            out.push(b);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

/// Every `ExprId` reachable from the body root, in arbitrary order.
fn reachable_exprs(body: &Body) -> Vec<ExprId> {
    let mut out = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            out.push(id);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

// ---------------------------------------------------------------------------
// Helper-shape detection / rewrite
// ---------------------------------------------------------------------------

fn body_is_list_wrapper_copy(body: &Body) -> bool {
    // `return StructLiteral { fields: [.., repr: Call(array_clone, ..), ..] }`
    for s in &body.blocks[body.root].stmts {
        if let StmtKind::Return { value: Some(v) } = &body.stmts[*s].kind
            && let ExprKind::StructLiteral { fields, .. } = &body.exprs[*v].kind
        {
            return fields.iter().any(|fld| {
                fld.name == SeqField::Backing.field_name()
                    && matches!(&body.exprs[fld.value].kind,
                        ExprKind::Call { func, .. }
                            if builtin_gname(func).as_deref() == Some("builtin::array_clone"))
            });
        }
    }
    false
}

/// Rewrite every reachable `builtin::array_clone` call in `body` to its
/// shallow sibling `array_clone_shallow`.
fn rewrite_array_clone_to_shallow(body: &mut Body) {
    for id in reachable_exprs(body) {
        if let ExprKind::Call { func, .. } = &mut body.exprs[id].kind
            && builtin_gname(func).as_deref() == Some("builtin::array_clone")
        {
            func.name = "array_clone_shallow".to_string();
            if let Some(mi) = &mut func.monomorph_info {
                mi.generic_name = "array_clone_shallow".to_string();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Call-site collection / retargeting
// ---------------------------------------------------------------------------

/// If the expression at `value` is a one-argument call to an array-wrapper
/// `$value_copy$T` helper, return that helper's key.
fn wrapper_call_key(body: &Body, value: ExprId, wrappers: &IndexSet<FuncKey>) -> Option<FuncKey> {
    if let ExprKind::Call { func, args, .. } = &body.exprs[value].kind
        && args.len() == 1
    {
        let key = (func.module_source.clone(), func.name.clone());
        if wrappers.contains(&key) {
            return Some(key);
        }
    }
    None
}

/// The `(value, target-local)` binding a statement establishes, when it is a
/// `let x = …` or a `x = …` whose target is a plain local.
fn stmt_binding(body: &Body, s: StmtId) -> Option<(ExprId, u32)> {
    match &body.stmts[s].kind {
        StmtKind::Let {
            local_index, value, ..
        } => Some((*value, *local_index)),
        StmtKind::Expr(e) => {
            if let ExprKind::Assign { target, value } = &body.exprs[*e].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
            {
                Some((*value, *index))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// For every `let x = $value_copy$T(arg)` (and `Assign` form) binding to a
/// value-copy-wrapper helper, AND-combine its eligibility into
/// `site_elig[(fi, x)]`. The per-`(fi, x)` key and the AND-combine are both
/// order-independent (a local's value-copy wrapper is fixed by its type), so a
/// flat sweep over every reachable statement matches the former depth-first
/// descent exactly.
fn collect_sites(
    body: &Body,
    wrappers: &IndexSet<FuncKey>,
    params: &[NirParam],
    fi: usize,
    an: &mut Analyzer,
    site_elig: &mut IndexMap<(usize, u32), bool>,
    site_key: &mut IndexMap<(usize, u32), FuncKey>,
) {
    for block in reachable_blocks(body) {
        let stmts = body.blocks[block].stmts.clone();
        for s in stmts {
            if let Some((value, target)) = stmt_binding(body, s)
                && let Some(key) = wrapper_call_key(body, value, wrappers)
            {
                let elig = demote_candidate(body, value, target, params, an);
                let loc = (fi, target);
                site_elig
                    .entry(loc)
                    .and_modify(|e| *e &= elig)
                    .or_insert(elig);
                site_key.entry(loc).or_insert(key);
            }
        }
    }
}

/// Walk the projections (`*`, `&`, `.field`, `[i]`, cast) that share storage
/// with their inner expression to the local the value-copy argument reads
/// from. `None` for a genuinely fresh rvalue (call result, literal,
/// constructor) — those are uniquely owned, so a shallow copy of them is
/// always safe.
fn arg_source_root(body: &Body, id: ExprId) -> Option<u32> {
    match &body.exprs[id].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => arg_source_root(body, *inner),
        _ => None,
    }
}

/// True when `idx` is a parameter declared as an immutable reference (`&T`):
/// the body cannot mutate `*idx` at all, so its elements are element-clean.
fn is_immutable_ref_param(
    params: &[NirParam],
    type_table: &Rc<RefCell<TypeTable>>,
    idx: u32,
) -> bool {
    params.iter().any(|p| {
        p.local_index == idx && matches!(type_table.borrow().get(p.type_id), ResolvedType::Ref(_))
    })
}

/// True when the binding `target_idx` and the source `value` reads from are
/// both element-clean — i.e. demoting `value` (an array-wrapper value copy)
/// to a shallow copy is observably equivalent to the deep copy.
fn demote_candidate(
    body: &Body,
    value: ExprId,
    target_idx: u32,
    params: &[NirParam],
    an: &mut Analyzer,
) -> bool {
    let arg0 = match &body.exprs[value].kind {
        ExprKind::Call { args, .. } => args[0].expr,
        _ => return false,
    };
    if !an.handle_is_element_clean(body, target_idx) {
        crate::compiler_trace!(
            "demote",
            "target local {} not element-clean — skip",
            target_idx
        );
        return false;
    }
    // The argument side: a fresh rvalue (`None` root) is uniquely owned;
    // an immutable-ref parameter cannot be mutated; otherwise every use of
    // the root local must itself be element-clean.
    match arg_source_root(body, arg0) {
        None => true,
        Some(root) => {
            let clean = is_immutable_ref_param(params, an.type_table, root)
                || an.arg_local_is_element_clean(body, root);
            if !clean {
                crate::compiler_trace!(
                    "demote",
                    "arg root local {} not element-clean — skip",
                    root
                );
            }
            clean
        }
    }
}

/// Phase 2b mechanical rewrite: for each `let x = …` / `x = …` binding whose
/// `(fi, x)` is marked eligible, retarget the value-copy-wrapper call to its
/// shallow sibling. Pure rewrite — no analysis, so it cannot conflict with the
/// `&mut` borrow of the function. Targets are gathered first (an immutable
/// sweep), then rewritten, since each retarget is independent.
fn retarget_block(
    body: &mut Body,
    fi: usize,
    site_elig: &IndexMap<(usize, u32), bool>,
    wrappers: &IndexSet<FuncKey>,
    shallow_name: &IndexMap<FuncKey, String>,
) {
    let mut targets: Vec<ExprId> = Vec::new();
    for block in reachable_blocks(body) {
        for s in &body.blocks[block].stmts {
            let target = match &body.stmts[*s].kind {
                StmtKind::Let { local_index, .. } => Some(*local_index),
                StmtKind::Expr(e) => match &body.exprs[*e].kind {
                    ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
                        ExprKind::Local { index, .. } => Some(*index),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            };
            if let Some(target) = target
                && site_elig.get(&(fi, target)) == Some(&true)
                && let Some(value) = stmt_binding_value(body, *s)
            {
                targets.push(value);
            }
        }
    }
    for id in targets {
        retarget_wrapper_call(body, id, wrappers, shallow_name);
    }
}

/// The binding value of a `let` / `Assign` statement.
fn stmt_binding_value(body: &Body, s: StmtId) -> Option<ExprId> {
    match &body.stmts[s].kind {
        StmtKind::Let { value, .. } => Some(*value),
        StmtKind::Expr(e) => {
            if let ExprKind::Assign { value, .. } = &body.exprs[*e].kind {
                Some(*value)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Rewrite a value-copy-wrapper call to its synthesized shallow sibling.
fn retarget_wrapper_call(
    body: &mut Body,
    value: ExprId,
    wrappers: &IndexSet<FuncKey>,
    shallow_name: &IndexMap<FuncKey, String>,
) {
    if let ExprKind::Call { func, args, .. } = &mut body.exprs[value].kind
        && args.len() == 1
    {
        let key = (func.module_source.clone(), func.name.clone());
        if wrappers.contains(&key)
            && let Some(new) = shallow_name.get(&key)
        {
            func.name.clone_from(new);
        }
    }
}

// ---------------------------------------------------------------------------
// Element-immutability analysis
// ---------------------------------------------------------------------------

struct Analyzer<'a> {
    funcs: &'a [Rc<RefCell<NirFunction>>],
    by_key: &'a IndexMap<FuncKey, usize>,
    type_table: &'a Rc<RefCell<TypeTable>>,
    eimm_memo: IndexMap<FuncKey, bool>,
}

impl Analyzer<'_> {
    /// True when `param0` of `func` (its `self`) is `&mut self`.
    fn callee_mutates_self(&self, key: &FuncKey) -> Option<bool> {
        let idx = *self.by_key.get(key)?;
        let f = self.funcs[idx].borrow();
        let p0 = f.params.first()?;
        Some(matches!(
            self.type_table.borrow().get(p0.type_id),
            ResolvedType::MutRef(_)
        ))
    }

    /// True when calling `key` (a `&mut self` method) cannot mutate any
    /// element of `self`. Memoized; recursion is conservatively `false`.
    fn is_method_element_immutable(&mut self, key: &FuncKey) -> bool {
        let mut visiting: IndexSet<FuncKey> = IndexSet::default();
        let r = self.verify(key, &mut visiting);
        crate::compiler_trace!("demote", "eimm({}) = {}", key.1, r);
        r
    }

    fn verify(&mut self, key: &FuncKey, visiting: &mut IndexSet<FuncKey>) -> bool {
        if let Some(&v) = self.eimm_memo.get(key) {
            return v;
        }
        if visiting.contains(key) {
            return false; // recursion guard — conservative
        }
        let Some(&idx) = self.by_key.get(key) else {
            crate::compiler_trace!("demote", "verify: callee {} not found in package", key.1);
            return false;
        };
        visiting.insert(key.clone());
        let body_opt = self.funcs[idx].borrow().body.clone();
        let result = match body_opt {
            Some(body) => {
                let mut tainted: IndexSet<u32> = IndexSet::default();
                tainted.insert(0); // local 0 == self
                let mut v = ElementImmutable {
                    analyzer: self,
                    tainted,
                    visiting,
                    clean: true,
                };
                v.visit_node(&body, NodeRef::Block(body.root));
                v.clean
            }
            None => false,
        };
        visiting.swap_remove(key);
        self.eimm_memo.insert(key.clone(), result);
        result
    }

    /// True when every use of local `idx` in `body` keeps the array's
    /// elements immutable: spine-only methods, index/field reads, by-value
    /// or `&` argument passing.
    fn handle_is_element_clean(&mut self, body: &Body, idx: u32) -> bool {
        let mut v = ElementClean {
            analyzer: self,
            idx,
            clean: true,
        };
        v.visit_node(body, NodeRef::Block(body.root));
        v.clean
    }

    /// The `arg` of a value-copy: element-clean when it is an immutable-ref
    /// parameter (the body cannot mutate `*arg` at all) or when every use is
    /// element-clean.
    fn arg_local_is_element_clean(&mut self, body: &Body, idx: u32) -> bool {
        self.handle_is_element_clean(body, idx)
    }
}

/// Element-cleanliness check for a single handle local `idx`: every use of it
/// must keep the array's elements immutable (spine-only methods, index/field
/// reads, by-value or `&` argument passing). Recurses through every nested
/// node — including match-arm and destructure patterns — via the arena
/// `for_each_child` walk; only the storage-sharing shapes are special-cased.
struct ElementClean<'a, 'b> {
    analyzer: &'b mut Analyzer<'a>,
    idx: u32,
    clean: bool,
}

impl ElementClean<'_, '_> {
    /// Default walk: descend into every id-bearing child. Statement / block /
    /// pattern nodes have no override, so they fall here; expression nodes
    /// dispatch to the override.
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if !self.clean {
            return;
        }
        if let NodeRef::Expr(id) = node {
            self.visit_expr(body, id);
            return;
        }
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            self.visit_node(body, c);
            if !self.clean {
                return;
            }
        }
    }

    fn walk_expr(&mut self, body: &Body, id: ExprId) {
        let mut kids = Vec::new();
        body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
        for c in kids {
            self.visit_node(body, c);
            if !self.clean {
                return;
            }
        }
    }

    fn visit_expr(&mut self, body: &Body, id: ExprId) {
        if !self.clean {
            return;
        }
        let idx = self.idx;
        match &body.exprs[id].kind {
            ExprKind::Local { index, .. } => {
                if *index == idx {
                    self.clean = false;
                }
            }
            ExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                let receiver = *receiver;
                let key = (func.module_source.clone(), func.name.clone());
                let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
                // The receiver auto-refs to `&self` / `&mut self`, so strip
                // the wrapping reference before matching the handle.
                let recv = strip_refs(body, receiver);
                if is_local(body, recv, idx) {
                    // Receiver is the handle itself: `x.method()`.
                    let safe = match self.analyzer.callee_mutates_self(&key) {
                        Some(false) => true, // &self
                        Some(true) => self.analyzer.is_method_element_immutable(&key),
                        None => false,
                    };
                    if !safe {
                        self.clean = false;
                        return;
                    }
                } else if expr_mentions_local(body, receiver, idx) {
                    // The handle appears inside the receiver — an element
                    // or field of it (`x[i].method()`, `x.get(i).method()`,
                    // `x.field.method()`). A `&mut self` method there may
                    // mutate an element, which a shallow copy would share.
                    // A `&self` method is a read; recurse to vet the
                    // receiver. (`x[i]` lowers to an `List::index` method
                    // call, not a bare `Index`, so a structural root check
                    // is not enough — match on the handle appearing at all.)
                    if self.analyzer.callee_mutates_self(&key) != Some(false) {
                        self.clean = false;
                        return;
                    }
                    self.visit_expr(body, receiver);
                    if !self.clean {
                        return;
                    }
                } else {
                    self.visit_expr(body, receiver);
                    if !self.clean {
                        return;
                    }
                }
                for a in args {
                    self.visit_call_arg(body, a);
                    if !self.clean {
                        return;
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
                for a in args {
                    self.visit_call_arg(body, a);
                    if !self.clean {
                        return;
                    }
                }
            }
            ExprKind::IndirectCall { callee, args } => {
                let callee = *callee;
                let args = args.clone();
                self.visit_expr(body, callee);
                if !self.clean {
                    return;
                }
                for a in args {
                    self.visit_call_arg(body, a);
                    if !self.clean {
                        return;
                    }
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                // `&mut x`, `&mut x[i]`, `&mut x.field` — a mutable
                // reference into the handle escapes our control.
                let inner = *inner;
                if expr_mentions_local(body, inner, idx) {
                    self.clean = false;
                }
            }
            ExprKind::Index { expr: base, index } => {
                // Index read: `x[i]` produces an element copy.
                let base = *base;
                let index = *index;
                if !is_local(body, base, idx) {
                    self.visit_expr(body, base);
                    if !self.clean {
                        return;
                    }
                }
                self.visit_expr(body, index);
            }
            ExprKind::FieldAccess { expr: base, .. } => {
                let base = *base;
                if !is_local(body, base, idx) {
                    self.visit_expr(body, base);
                }
            }
            ExprKind::Assign { target, value } => {
                // A write whose target touches `idx` escapes our control.
                let target = *target;
                let value = *value;
                if expr_mentions_local(body, target, idx) {
                    self.clean = false;
                    return;
                }
                self.visit_expr(body, value);
            }
            // Every other shape — operators, casts, aggregates, control flow,
            // and the patterns reached through it — is a plain read; recurse
            // with the canonical walk.
            _ => self.walk_expr(body, id),
        }
    }

    /// `idx` passed as a call argument: by-value or `&` is element-clean;
    /// `&mut idx` is not.
    fn visit_call_arg(&mut self, body: &Body, arg: ExprId) {
        let idx = self.idx;
        match &body.exprs[arg].kind {
            // Passing the handle by value hands the callee an independent
            // (deep) copy, so it cannot reach our elements.
            ExprKind::Local { .. } => {}
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                let inner = *inner;
                if expr_mentions_local(body, inner, idx) {
                    self.clean = false;
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: inner,
            } => {
                let inner = *inner;
                if !is_local(body, inner, idx) {
                    self.visit_expr(body, inner);
                }
            }
            _ => self.visit_expr(body, arg),
        }
    }
}

/// Element-immutability proof for a `&mut self` method: whether calling it can
/// mutate any element of `self`. Threads the analysis state as fields:
///
/// - `tainted` — locals that alias `self`'s storage (local 0 is `self`). The
///   set grows in statement order: a `let x = <self-derived>` (or assignment)
///   taints `x` for every *later* statement, so the order-sensitive update
///   lives in the `visit_stmt` override, after the value is visited.
/// - `visiting` — the recursion guard shared with [`Analyzer::verify`], so a
///   self-derived `&mut self` call recurses into the callee's own proof.
/// - `clean` — the running verdict; once falsified every visit short-circuits.
///
/// Only the storage-escaping shapes (`&mut` of self-derived, element
/// field/index writes, unsafe calls on self-derived receivers/args, indirect
/// calls of self-capturing closures) are special-cased; everything else
/// recurses through the arena `for_each_child` walk, which descends into
/// match-arm and destructure patterns *and* threads taint through
/// expression-position blocks.
struct ElementImmutable<'a, 'b, 'c> {
    analyzer: &'b mut Analyzer<'a>,
    tainted: IndexSet<u32>,
    visiting: &'c mut IndexSet<FuncKey>,
    clean: bool,
}

impl ElementImmutable<'_, '_, '_> {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if !self.clean {
            return;
        }
        match node {
            NodeRef::Stmt(s) => self.visit_stmt(body, s),
            NodeRef::Expr(e) => self.visit_expr(body, e),
            _ => {
                let mut kids = Vec::new();
                body.for_each_child(node, |c| kids.push(c));
                for c in kids {
                    self.visit_node(body, c);
                    if !self.clean {
                        return;
                    }
                }
            }
        }
    }

    fn walk_stmt(&mut self, body: &Body, s: StmtId) {
        let mut kids = Vec::new();
        body.for_each_child(NodeRef::Stmt(s), |c| kids.push(c));
        for c in kids {
            self.visit_node(body, c);
            if !self.clean {
                return;
            }
        }
    }

    fn walk_expr(&mut self, body: &Body, id: ExprId) {
        let mut kids = Vec::new();
        body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
        for c in kids {
            self.visit_node(body, c);
            if !self.clean {
                return;
            }
        }
    }

    fn visit_stmt(&mut self, body: &Body, s: StmtId) {
        if !self.clean {
            return;
        }
        match &body.stmts[s].kind {
            // `let x = <self-derived>` makes `x` alias self's storage for
            // every later statement. Verify the value first, then taint —
            // this order-sensitivity is why the statement visit is overridden.
            StmtKind::Let {
                local_index, value, ..
            } => {
                let local_index = *local_index;
                let value = *value;
                self.visit_expr(body, value);
                if self.clean
                    && is_self_derived(body, value, &self.tainted, self.analyzer.type_table)
                {
                    self.tainted.insert(local_index);
                }
            }
            StmtKind::Expr(e) => {
                let e = *e;
                self.visit_expr(body, e);
                if !self.clean {
                    return;
                }
                // Assign-form taint: `x = <self-derived>` aliases `x` to
                // self's storage thereafter, just like a `let` binding.
                if let ExprKind::Assign { target, value } = &body.exprs[e].kind {
                    let target = *target;
                    let value = *value;
                    if let ExprKind::Local { index, .. } = &body.exprs[target].kind {
                        let index = *index;
                        if is_self_derived(body, value, &self.tainted, self.analyzer.type_table) {
                            self.tainted.insert(index);
                        }
                    }
                }
            }
            // If / Loop / LabeledBlock / Return / Break / LetDestructure /
            // Continue carry no taint update of their own; the canonical walk
            // recurses through the shared `tainted` field.
            _ => self.walk_stmt(body, s),
        }
    }

    fn visit_expr(&mut self, body: &Body, id: ExprId) {
        if !self.clean {
            return;
        }
        let tt = self.analyzer.type_table;
        match &body.exprs[id].kind {
            // `&mut <self-derived>` would expose mutable element access.
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                let inner = *inner;
                if is_self_derived(body, inner, &self.tainted, tt) {
                    crate::compiler_trace!("demote", "verify reject: &mut of self-derived");
                    self.clean = false;
                    return;
                }
                self.walk_expr(body, id);
            }
            // Field/index write to a self-derived base other than `self`
            // itself (writing `self.repr`/`self.used` is a spine op; writing
            // `array_get(...).field` is an element mutation).
            ExprKind::Assign { target, value } => {
                let target = *target;
                let value = *value;
                let bad = match &body.exprs[target].kind {
                    ExprKind::FieldAccess { expr: base, .. } => {
                        let base = *base;
                        is_self_derived(body, base, &self.tainted, tt)
                            && !matches!(&body.exprs[base].kind, ExprKind::Local { index: 0, .. })
                    }
                    ExprKind::Index { expr: base, .. } => {
                        is_self_derived(body, *base, &self.tainted, tt)
                    }
                    _ => false,
                };
                if bad {
                    crate::compiler_trace!("demote", "verify reject: element field/index write");
                    self.clean = false;
                    return;
                }
                self.visit_expr(body, target);
                if !self.clean {
                    return;
                }
                self.visit_expr(body, value);
            }
            ExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                let receiver = *receiver;
                let key = (func.module_source.clone(), func.name.clone());
                let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
                // A call whose receiver is self-derived may mutate an element
                // unless the callee is known `&self` (cannot mutate) or a
                // verified element-immutable `&mut self` method. An
                // unresolvable callee is conservatively unsafe.
                if is_self_derived(body, receiver, &self.tainted, tt) {
                    let ok = match self.analyzer.callee_mutates_self(&key) {
                        Some(false) => true,
                        Some(true) => self.analyzer.verify(&key, self.visiting),
                        None => false,
                    };
                    if !ok {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: unsafe call on self-derived recv {}",
                            key.1
                        );
                        self.clean = false;
                        return;
                    }
                }
                self.visit_expr(body, receiver);
                if !self.clean {
                    return;
                }
                for a in args {
                    self.visit_call_arg(body, a);
                    if !self.clean {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: bad arg to method {}",
                            key.1
                        );
                        return;
                    }
                }
            }
            ExprKind::Call { func, args, .. } => {
                // No builtin intrinsic mutates an element's pointee
                // (`array_set` rewrites a spine slot; `array_get` / `select`
                // only forward values), so a self-derived arg to a builtin is
                // harmless. Opaque (non-builtin) calls still gate.
                let is_builtin = builtin_gname(func).is_some();
                let fname = func.name.clone();
                let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
                for a in args {
                    if is_builtin {
                        // The `array_*` intrinsics take `&` / `&mut` first
                        // parameters (WEP-2026-06-02 Phase 1). A `&mut
                        // self.repr` arg to a spine builtin is a spine
                        // handoff, not an element-mutating escape, so peel the
                        // reference wrapper before recursing.
                        let inner = match &body.exprs[a].kind {
                            ExprKind::Unary {
                                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                                expr,
                            } => *expr,
                            _ => a,
                        };
                        self.visit_expr(body, inner);
                    } else {
                        self.visit_call_arg(body, a);
                    }
                    if !self.clean {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: self-derived arg to call {}",
                            fname
                        );
                        return;
                    }
                }
            }
            ExprKind::IndirectCall { callee, args } => {
                // Invoking a closure that captured a self-derived value runs
                // its (unverified) body with access to `self`'s elements.
                let callee = *callee;
                let args = args.clone();
                if is_self_derived(body, callee, &self.tainted, tt) {
                    crate::compiler_trace!(
                        "demote",
                        "verify reject: indirect call of self-capturing closure"
                    );
                    self.clean = false;
                    return;
                }
                self.visit_expr(body, callee);
                if !self.clean {
                    return;
                }
                for a in args {
                    self.visit_call_arg(body, a);
                    if !self.clean {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: self-derived arg to indirect call"
                        );
                        return;
                    }
                }
            }
            // Every other shape — operators, casts, aggregates, control flow,
            // and the patterns reached through it — recurses via the canonical
            // walk, which descends into match-arm / destructure-pattern
            // `ConstantValue` sub-expressions.
            _ => self.walk_expr(body, id),
        }
    }

    /// A self-derived value handed to an opaque (non-spine-builtin) call may
    /// only flow through an immutable `&` borrow.
    fn visit_call_arg(&mut self, body: &Body, arg: ExprId) {
        if let ExprKind::Unary {
            op: NirUnaryOp::Ref,
            ..
        } = &body.exprs[arg].kind
        {
            self.visit_expr(body, arg);
            return;
        }
        if is_self_derived(body, arg, &self.tainted, self.analyzer.type_table) {
            self.clean = false;
            return;
        }
        self.visit_expr(body, arg);
    }
}

/// True when the expression at `id` produces a value that may alias `self`'s
/// storage — the tracked local itself, a projection of it, an `array_get`
/// element read of a tracked spine, or an aggregate / closure that captures
/// any such value.
///
/// A primitive-typed value is excluded: reading `self.used` (an `i32`)
/// produces an independent copy, so passing it around cannot reach `self`.
fn is_self_derived(
    body: &Body,
    id: ExprId,
    tainted: &IndexSet<u32>,
    tt: &Rc<RefCell<TypeTable>>,
) -> bool {
    if matches!(
        tt.borrow().get(body.exprs[id].type_id),
        ResolvedType::Primitive(_)
    ) {
        return false;
    }
    match &body.exprs[id].kind {
        ExprKind::Local { index, .. } => tainted.contains(index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. } => is_self_derived(body, *inner, tainted, tt),
        ExprKind::Call { func, args, .. } => {
            // `array_get(spine, _)` yields an element of the spine; other
            // array builtins (`array_clone`, `array_new`) produce fresh
            // storage and are not self-derived.
            builtin_gname(func).as_deref() == Some("builtin::array_get")
                && args
                    .first()
                    .is_some_and(|a| is_self_derived(body, a.expr, tainted, tt))
        }
        // An aggregate / closure that embeds a self-derived value carries
        // that aliasing storage. Tainting it lets the mutation checks below
        // (field write, `&mut`, `&mut self` call, indirect call) fire on the
        // aggregate / closure too.
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| is_self_derived(body, f.value, tainted, tt)),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .any(|e| is_self_derived(body, *e, tainted, tt)),
        ExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .is_some_and(|p| is_self_derived(body, *p, tainted, tt)),
        ExprKind::ClosureToCanonical { functor, .. } => {
            is_self_derived(body, *functor, tainted, tt)
        }
        _ => false,
    }
}
