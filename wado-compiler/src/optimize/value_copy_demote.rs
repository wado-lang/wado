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

use crate::nir::{FunctionKind, FunctionRef, NirFunction, NirParam, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::arena_query::{expr_mentions_local, is_local, reachable_blocks, strip_refs};
use super::gate::{FunctionGate, GatedPass};
use crate::nir::FuncId;
use cranelift_entity::EntityRef;

/// A function's canonical [`FuncId`]: the wrapper / demoted / shallow sets key
/// on it, and a call site is matched by its stamped `func_id`.
type FuncKey = FuncId;

fn builtin_gname(func: &FunctionRef) -> Option<String> {
    func.builtin_name()
        .or_else(|| func.monomorphized_builtin_name())
}

/// Returns whether anything changed (a binding was retargeted or a shallow
/// specialization was added), so the optimizer's dirty-set gate can re-examine
/// the touched functions and accommodate the new ones.
pub fn demote_value_copies(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    // Intern the `array_clone_shallow` builtin the synthesized twins call, so
    // those calls are born resolved. One id serves all instantiations (the key
    // ignores type args, which ride the node).
    let array_clone_shallow_id = project.intern_extern(&FunctionRef {
        module_source: crate::module_source::ModuleSource::builtin(),
        name: "array_clone_shallow".to_string(),
        monomorph_info: None,
        method_info: None,
    });
    // Callee identity by `func_id` (descriptor table built once from the records,
    // borrow-safe, indexed by `func_id.index()` == store position). A call site's
    // identity (builtin name, wrapper membership, callee key) is read through its
    // stamped `func_id`, not the call node's `FunctionRef`. Built after the
    // `array_clone_shallow` intern so it includes that stub; the later synthesis
    // (Phase 2a) appends shallow twins, but no recognizer reads those by id.
    let descriptors = super::dce::build_callee_descriptors(project);

    // Identify `$value_copy$T` helpers whose body is an `List<E>` wrapper
    // copy: `return StructLiteral { repr: array_clone(v.repr), used: ... }`.
    // Helpers whose element type can leak a payload alias through a pattern
    // binding are never demoted — see `element_may_leak_payload_alias`.
    let mut list_wrapper_copies: IndexSet<FuncKey> = IndexSet::default();
    {
        let type_table = project.type_table.borrow();
        for f in &project.functions {
            let f = f.borrow();
            if let Some(copy_type) = f.value_copy_type()
                && let Some(body) = &f.body
                && body_is_list_wrapper_copy(body, &descriptors)
                && let Some(id) = f.id
                && !list_element_may_leak_payload_alias(copy_type, project, &type_table)
            {
                list_wrapper_copies.insert(id);
            }
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
        descriptors: &descriptors,
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
    for fid in gate.dirty_funcs(GatedPass::ValueCopyDemote, project.functions.len()) {
        let fi = fid.index();
        let f = project.functions[fi].borrow();
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
            demoted_keys.insert(site_key[loc]);
        }
    }
    crate::compiler_trace!("demote", "demoted helper keys: {}", demoted_keys.len());
    if demoted_keys.is_empty() {
        return false;
    }

    // Phase 2a: synthesize a shallow sibling helper per demoted deep helper.
    // Each twin is born resolved: minted a `FuncId` at its eventual store
    // position and registered in the reverse index, so its callers stamp the
    // twin's id directly (`shallow_id`).
    let mut shallow_id: IndexMap<FuncKey, FuncId> = IndexMap::default();
    let mut new_funcs: Vec<Rc<RefCell<NirFunction>>> = Vec::new();
    let mut next = project.next_func_id().index();
    for &deep_key in &demoted_keys {
        // The shallow twin's identity mirrors the deep helper's (a clone with a
        // `$shallow` name), so its `func_index` key reuses the deep helper's
        // `monomorph_info` / `method_info`.
        let (new_name, existing_ref) = {
            let deep = project.functions[deep_key.index()].borrow();
            let new_name = format!("{}$shallow", deep.name);
            let existing_ref = FunctionRef {
                module_source: deep.module_source.clone(),
                name: new_name.clone(),
                monomorph_info: deep.monomorph_info.clone(),
                method_info: deep.method_info.clone(),
            };
            (new_name, existing_ref)
        };
        // A prior fixed-point iteration may already have synthesized this
        // helper; reuse it (and its id) rather than adding a duplicate.
        if let Some(id) = project.func_id_of(&existing_ref) {
            shallow_id.insert(deep_key, id);
            continue;
        }
        let mut shallow = project.functions[deep_key.index()].borrow().clone();
        shallow.name.clone_from(&new_name);
        shallow.kind = FunctionKind::Regular;
        shallow.visibility = crate::ast::Visibility::Private;
        shallow.is_export = false;
        let id = FuncId::new(next);
        next += 1;
        shallow.id = Some(id);
        if let Some(body) = &mut shallow.body {
            rewrite_array_clone_to_shallow(body, array_clone_shallow_id, &descriptors);
        }
        let key = FunctionRef::from_resolved(&shallow, shallow.module_source.clone()).function_id();
        project.func_index.insert(key, id);
        shallow_id.insert(deep_key, id);
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
            retarget_block(body, fi, &site_elig, &list_wrapper_copies, &shallow_id);
        }
    }
    project.functions.extend(new_funcs);
    // Report the retargeted callers; the appended shallow specializations get a
    // fresh dirty gate slot via the gate's auto-grow on first access. Retarget
    // shifts a caller's call edges (a `$value_copy$` call → its shallow twin),
    // which only costs propagation precision, not correctness.
    for fi in touched {
        gate.mark_changed(FuncId::new(fi));
    }
    changed
}

// ---------------------------------------------------------------------------
// Reachable-node enumeration
// ---------------------------------------------------------------------------

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

fn body_is_list_wrapper_copy(body: &Body, descriptors: &[FunctionRef]) -> bool {
    // `return StructLiteral { fields: [.., repr: Call(array_clone, ..), ..] }`
    for s in &body.blocks[body.root].stmts {
        if let StmtKind::Return { value: Some(v) } = &body.stmts[*s].kind
            && let Some(ve) = v.as_expr()
            && let ExprKind::StructLiteral { fields, .. } = &body.exprs[ve].kind
        {
            return fields.iter().any(|fld| {
                fld.name == SeqField::Backing.field_name()
                    && fld.value.as_expr().is_some_and(|fe| {
                        matches!(&body.exprs[fe].kind,
                            ExprKind::Call { func_id, .. }
                                if is_array_clone(*func_id, descriptors))
                    })
            });
        }
    }
    false
}

/// Whether a shallow spine copy of a `List` of this wrapper type could be
/// mutated through a shared element. A `match` payload binding aliases a
/// variant's payload storage instead of copying it (`if let Some(v) = &mut
/// xs[i]` reaches the element in place through a synthesized box), a flow the
/// element-cleanliness analysis cannot see. Any element that transitively
/// carries a copy-needing variant is therefore ineligible for demotion.
fn list_element_may_leak_payload_alias(
    copy_type: TypeId,
    project: &NirPackage,
    type_table: &TypeTable,
) -> bool {
    let element = match type_table.get(copy_type) {
        ResolvedType::GenericInstance { type_args, .. } => match type_args.first() {
            Some(elem) => *elem,
            None => return true,
        },
        // `String`'s wrapper copy has a `u8` element; other shapes that reach
        // here without a type argument stay conservatively deep.
        ResolvedType::Struct { .. } => return false,
        _ => return true,
    };
    contains_copy_needing_variant(element, project, type_table, 0)
}

fn contains_copy_needing_variant(
    type_id: TypeId,
    project: &NirPackage,
    type_table: &TypeTable,
    depth: usize,
) -> bool {
    if depth > 16 {
        return true;
    }
    match type_table.get(type_id) {
        ResolvedType::Variant { .. } => {
            crate::lower::plan::value_copy::needs_value_copy(type_id, type_table)
        }
        ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        } => {
            if type_table
                .variant_template_cases(name, module_source)
                .is_some()
            {
                return crate::lower::plan::value_copy::needs_value_copy(type_id, type_table);
            }
            if type_args
                .iter()
                .any(|arg| contains_copy_needing_variant(*arg, project, type_table, depth + 1))
            {
                return true;
            }
            struct_fields_contain_copy_needing_variant(type_id, project, type_table, depth)
        }
        ResolvedType::Struct { .. } => {
            struct_fields_contain_copy_needing_variant(type_id, project, type_table, depth)
        }
        ResolvedType::BuiltinArray(elem) => {
            contains_copy_needing_variant(*elem, project, type_table, depth + 1)
        }
        _ => false,
    }
}

fn struct_fields_contain_copy_needing_variant(
    type_id: TypeId,
    project: &NirPackage,
    type_table: &TypeTable,
    depth: usize,
) -> bool {
    let mangled = type_table.mangle_type_name(type_id);
    project
        .structs
        .iter()
        .filter(|s| s.name == mangled)
        .any(|s| {
            s.fields.iter().any(|f| {
                contains_copy_needing_variant(f.type_id, project, type_table, depth + 1)
            })
        })
}

/// Whether the call's stamped `func_id` resolves to `builtin::array_clone`.
fn is_array_clone(func_id: FuncId, descriptors: &[FunctionRef]) -> bool {
    builtin_gname(super::dce::callee_descriptor(descriptors, func_id)).as_deref()
        == Some("builtin::array_clone")
}

/// Rewrite every reachable `builtin::array_clone` call in `body` to its
/// shallow sibling `array_clone_shallow`.
fn rewrite_array_clone_to_shallow(
    body: &mut Body,
    array_clone_shallow_id: FuncId,
    descriptors: &[FunctionRef],
) {
    for id in reachable_exprs(body) {
        if let ExprKind::Call { func_id, .. } = &mut body.exprs[id].kind
            && is_array_clone(*func_id, descriptors)
        {
            // Retarget to the shallow sibling by id; codegen reads the callee
            // descriptor from `func_id` (`store[id]`), so no node `FunctionRef`
            // rewrite is needed. The key ignores type args, so one id serves all.
            *func_id = array_clone_shallow_id;
        }
    }
}

// ---------------------------------------------------------------------------
// Call-site collection / retargeting
// ---------------------------------------------------------------------------

/// If the expression at `value` is a one-argument call to an array-wrapper
/// `$value_copy$T` helper, return that helper's key.
fn wrapper_call_key(body: &Body, value: ExprId, wrappers: &IndexSet<FuncKey>) -> Option<FuncKey> {
    if let ExprKind::Call { func_id, args, .. } = &body.exprs[value].kind
        && args.len() == 1
        && wrappers.contains(func_id)
    {
        return Some(*func_id);
    }
    None
}

/// The `(value, target-local)` binding a statement establishes, when it is a
/// `let x = …` or a `x = …` whose target is a plain local.
fn stmt_binding(body: &Body, s: StmtId) -> Option<(ExprId, u32)> {
    match &body.stmts[s].kind {
        StmtKind::Let {
            local_index, value, ..
        } => value.as_expr().map(|e| (e, *local_index)),
        StmtKind::Expr(Operand::Expr(e)) => {
            if let ExprKind::Assign { target, value } = &body.exprs[*e].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
            {
                value.as_expr().map(|e| (e, *index))
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
        | ExprKind::VariantPayload { expr: inner, .. } => {
            inner.as_expr().and_then(|ie| arg_source_root(body, ie))
        }
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
    // A promoted `Operand::Value` arg is a constant: a fresh, uniquely-owned
    // rvalue with no root local — the `None` (clean) case.
    match arg0.as_expr().and_then(|e| arg_source_root(body, e)) {
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
    shallow_id: &IndexMap<FuncKey, FuncId>,
) {
    let mut targets: Vec<ExprId> = Vec::new();
    for block in reachable_blocks(body) {
        for s in &body.blocks[block].stmts {
            let target = match &body.stmts[*s].kind {
                StmtKind::Let { local_index, .. } => Some(*local_index),
                StmtKind::Expr(Operand::Expr(e)) => match &body.exprs[*e].kind {
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
        retarget_wrapper_call(body, id, wrappers, shallow_id);
    }
}

/// The binding value of a `let` / `Assign` statement.
fn stmt_binding_value(body: &Body, s: StmtId) -> Option<ExprId> {
    match &body.stmts[s].kind {
        // A promoted-constant binding has no skeleton value to demote.
        StmtKind::Let { value, .. } => value.as_expr(),
        StmtKind::Expr(Operand::Expr(e)) => {
            if let ExprKind::Assign { value, .. } = &body.exprs[*e].kind {
                value.as_expr()
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
    shallow_id: &IndexMap<FuncKey, FuncId>,
) {
    if let ExprKind::Call { func_id, args, .. } = &mut body.exprs[value].kind
        && args.len() == 1
        && wrappers.contains(func_id)
        && let Some(&shallow) = shallow_id.get(func_id)
    {
        // Retarget to the shallow twin by id; codegen reads the callee descriptor
        // from `func_id` (`store[id]`), so no node `FunctionRef` rewrite is needed
        // (born resolved; the twin was interned in Phase 2a).
        *func_id = shallow;
    }
}

// ---------------------------------------------------------------------------
// Element-immutability analysis
// ---------------------------------------------------------------------------

struct Analyzer<'a> {
    funcs: &'a [Rc<RefCell<NirFunction>>],
    descriptors: &'a [FunctionRef],
    type_table: &'a Rc<RefCell<TypeTable>>,
    eimm_memo: IndexMap<FuncKey, bool>,
}

impl Analyzer<'_> {
    /// True when `param0` of the callee (its `self`) is `&mut self`. `key` is the
    /// callee's `FuncId` (== store position).
    fn callee_mutates_self(&self, key: FuncKey) -> Option<bool> {
        let f = self.funcs.get(key.index())?.borrow();
        let p0 = f.params.first()?;
        Some(matches!(
            self.type_table.borrow().get(p0.type_id),
            ResolvedType::MutRef(_)
        ))
    }

    /// True when calling `key` (a `&mut self` method) cannot mutate any
    /// element of `self`. Memoized; recursion is conservatively `false`.
    fn is_method_element_immutable(&mut self, key: FuncKey) -> bool {
        let mut visiting: IndexSet<FuncKey> = IndexSet::default();
        let r = self.verify(key, &mut visiting);
        crate::compiler_trace!("demote", "eimm({}) = {}", key.index(), r);
        r
    }

    fn verify(&mut self, key: FuncKey, visiting: &mut IndexSet<FuncKey>) -> bool {
        if let Some(&v) = self.eimm_memo.get(&key) {
            return v;
        }
        if visiting.contains(&key) {
            return false; // recursion guard — conservative
        }
        let Some(func_rc) = self.funcs.get(key.index()) else {
            crate::compiler_trace!(
                "demote",
                "verify: callee {} not found in package",
                key.index()
            );
            return false;
        };
        visiting.insert(key);
        let body_opt = func_rc.borrow().body.clone();
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
        visiting.swap_remove(&key);
        self.eimm_memo.insert(key, result);
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
                func_id,
                args,
                ..
            } => {
                let receiver = *receiver;
                let callee = *func_id;
                let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                // A promoted constant receiver is never the demote candidate
                // handle and has nothing to vet; only a skeleton receiver does.
                if let Some(recv_e) = receiver.as_expr() {
                    // The receiver auto-refs to `&self` / `&mut self`, so strip
                    // the wrapping reference before matching the handle.
                    let recv = strip_refs(body, recv_e);
                    if is_local(body, recv, idx) {
                        // Receiver is the handle itself: `x.method()`.
                        let safe = match self.analyzer.callee_mutates_self(callee) {
                            Some(false) => true, // &self
                            Some(true) => self.analyzer.is_method_element_immutable(callee),
                            None => false,
                        };
                        if !safe {
                            self.clean = false;
                            return;
                        }
                    } else if expr_mentions_local(body, recv_e, idx) {
                        // The handle appears inside the receiver — an element
                        // or field of it (`x[i].method()`, `x.get(i).method()`,
                        // `x.field.method()`). A `&mut self` method there may
                        // mutate an element, which a shallow copy would share.
                        // A `&self` method is a read; recurse to vet the
                        // receiver. (`x[i]` lowers to an `List::index` method
                        // call, not a bare `Index`, so a structural root check
                        // is not enough — match on the handle appearing at all.)
                        if self.analyzer.callee_mutates_self(callee) != Some(false) {
                            self.clean = false;
                            return;
                        }
                        self.visit_expr(body, recv_e);
                        if !self.clean {
                            return;
                        }
                    } else {
                        self.visit_expr(body, recv_e);
                        if !self.clean {
                            return;
                        }
                    }
                }
                for a in args {
                    if let Some(ae) = a.as_expr() {
                        self.visit_call_arg(body, ae);
                        if !self.clean {
                            return;
                        }
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                for a in args {
                    let Some(a) = a.as_expr() else { continue };
                    self.visit_call_arg(body, a);
                    if !self.clean {
                        return;
                    }
                }
            }
            ExprKind::IndirectCall { callee, args } => {
                let callee = *callee;
                let args = args.clone();
                if let Some(ce) = callee.as_expr() {
                    self.visit_expr(body, ce);
                    if !self.clean {
                        return;
                    }
                }
                for a in args {
                    let Some(ae) = a.as_expr() else { continue };
                    self.visit_call_arg(body, ae);
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
                // A promoted `Operand::Value` inner is a constant; it mentions no
                // local, so it cannot escape `idx`.
                if inner
                    .as_expr()
                    .is_some_and(|ie| expr_mentions_local(body, ie, idx))
                {
                    self.clean = false;
                }
            }
            ExprKind::Index { expr: base, index } => {
                // Index read `x[i]` produces an element copy.
                let base = *base;
                let index = *index;
                if let Some(be) = base.as_expr()
                    && !is_local(body, be, idx)
                {
                    self.visit_expr(body, be);
                    if !self.clean {
                        return;
                    }
                }
                if let Some(ie) = index.as_expr() {
                    self.visit_expr(body, ie);
                }
            }
            ExprKind::FieldAccess { expr: base, .. } => {
                let base = *base;
                if let Some(be) = base.as_expr()
                    && !is_local(body, be, idx)
                {
                    self.visit_expr(body, be);
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
                if let Some(ve) = value.as_expr() {
                    self.visit_expr(body, ve);
                }
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
                // A promoted `Operand::Value` inner is a constant; it mentions no
                // local, so it cannot escape `idx`.
                if inner
                    .as_expr()
                    .is_some_and(|ie| expr_mentions_local(body, ie, idx))
                {
                    self.clean = false;
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: inner,
            } => {
                if let Some(ie) = inner.as_expr()
                    && !is_local(body, ie, idx)
                {
                    self.visit_expr(body, ie);
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
                if let Some(ve) = value.as_expr() {
                    self.visit_expr(body, ve);
                    if self.clean
                        && is_self_derived(
                            body,
                            ve,
                            &self.tainted,
                            self.analyzer.type_table,
                            self.analyzer.descriptors,
                        )
                    {
                        self.tainted.insert(local_index);
                    }
                }
            }
            StmtKind::Expr(Operand::Expr(e)) => {
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
                        if is_self_derived_operand(
                            body,
                            value,
                            &self.tainted,
                            self.analyzer.type_table,
                            self.analyzer.descriptors,
                        ) {
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
                if is_self_derived_operand(
                    body,
                    inner,
                    &self.tainted,
                    tt,
                    self.analyzer.descriptors,
                ) {
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
                        // A promoted base is never self-derived, so the guard
                        // short-circuits before the node lookup.
                        is_self_derived_operand(
                            body,
                            base,
                            &self.tainted,
                            tt,
                            self.analyzer.descriptors,
                        ) && base.as_expr().is_some_and(|be| {
                            !matches!(&body.exprs[be].kind, ExprKind::Local { index: 0, .. })
                        })
                    }
                    ExprKind::Index { expr: base, .. } => is_self_derived_operand(
                        body,
                        *base,
                        &self.tainted,
                        tt,
                        self.analyzer.descriptors,
                    ),
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
                if let Some(ve) = value.as_expr() {
                    self.visit_expr(body, ve);
                }
            }
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } => {
                let receiver = *receiver;
                let callee = *func_id;
                let args: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
                // A call whose receiver is self-derived may mutate an element
                // unless the callee is known `&self` (cannot mutate) or a
                // verified element-immutable `&mut self` method. An
                // unresolvable callee is conservatively unsafe.
                if is_self_derived_operand(
                    body,
                    receiver,
                    &self.tainted,
                    tt,
                    self.analyzer.descriptors,
                ) {
                    let ok = match self.analyzer.callee_mutates_self(callee) {
                        Some(false) => true,
                        Some(true) => self.analyzer.verify(callee, self.visiting),
                        None => false,
                    };
                    if !ok {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: unsafe call on self-derived recv {}",
                            callee.index()
                        );
                        self.clean = false;
                        return;
                    }
                }
                if let Some(re) = receiver.as_expr() {
                    self.visit_expr(body, re);
                    if !self.clean {
                        return;
                    }
                }
                for a in args {
                    self.visit_call_arg(body, a);
                    if !self.clean {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: bad arg to method {}",
                            callee.index()
                        );
                        return;
                    }
                }
            }
            ExprKind::Call { func_id, args, .. } => {
                // No builtin intrinsic mutates an element's pointee
                // (`array_set` rewrites a spine slot; `array_get` / `select`
                // only forward values), so a self-derived arg to a builtin is
                // harmless. Opaque (non-builtin) calls still gate.
                let callee = super::dce::callee_descriptor(self.analyzer.descriptors, *func_id);
                let is_builtin = builtin_gname(callee).is_some();
                let fname = callee.name.clone();
                let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                for a in args {
                    let Some(a) = a.as_expr() else { continue };
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
                            _ => a.into(),
                        };
                        if let Some(ie) = inner.as_expr() {
                            self.visit_expr(body, ie);
                        }
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
                if is_self_derived_operand(
                    body,
                    callee,
                    &self.tainted,
                    tt,
                    self.analyzer.descriptors,
                ) {
                    crate::compiler_trace!(
                        "demote",
                        "verify reject: indirect call of self-capturing closure"
                    );
                    self.clean = false;
                    return;
                }
                if let Some(ce) = callee.as_expr() {
                    self.visit_expr(body, ce);
                    if !self.clean {
                        return;
                    }
                }
                for a in args {
                    let Some(ae) = a.as_expr() else { continue };
                    self.visit_call_arg(body, ae);
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
        if is_self_derived(
            body,
            arg,
            &self.tainted,
            self.analyzer.type_table,
            self.analyzer.descriptors,
        ) {
            self.clean = false;
            return;
        }
        self.visit_expr(body, arg);
    }
}

/// [`is_self_derived`] for an operand: a promoted constant is never self-derived.
fn is_self_derived_op(
    body: &Body,
    op: Operand,
    tainted: &IndexSet<u32>,
    tt: &Rc<RefCell<TypeTable>>,
    descriptors: &[FunctionRef],
) -> bool {
    op.as_expr()
        .is_some_and(|e| is_self_derived(body, e, tainted, tt, descriptors))
}

fn is_self_derived_operand(
    body: &Body,
    op: Operand,
    tainted: &IndexSet<u32>,
    tt: &Rc<RefCell<TypeTable>>,
    descriptors: &[FunctionRef],
) -> bool {
    op.as_expr()
        .is_some_and(|e| is_self_derived(body, e, tainted, tt, descriptors))
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
    descriptors: &[FunctionRef],
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
        | ExprKind::Unary { expr: inner, .. } => {
            is_self_derived_op(body, *inner, tainted, tt, descriptors)
        }
        ExprKind::Call { func_id, args, .. } => {
            // `array_get(spine, _)` yields an element of the spine; other
            // array builtins (`array_clone`, `array_new`) produce fresh
            // storage and are not self-derived.
            builtin_gname(super::dce::callee_descriptor(descriptors, *func_id)).as_deref()
                == Some("builtin::array_get")
                && args
                    .first()
                    .is_some_and(|a| is_self_derived_op(body, a.expr, tainted, tt, descriptors))
        }
        // An aggregate / closure that embeds a self-derived value carries
        // that aliasing storage. Tainting it lets the mutation checks below
        // (field write, `&mut`, `&mut self` call, indirect call) fire on the
        // aggregate / closure too.
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| is_self_derived_op(body, f.value, tainted, tt, descriptors)),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .any(|e| is_self_derived_op(body, *e, tainted, tt, descriptors)),
        ExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .is_some_and(|p| is_self_derived_op(body, *p, tainted, tt, descriptors)),
        ExprKind::ClosureToCanonical { functor, .. } => {
            is_self_derived_op(body, *functor, tainted, tt, descriptors)
        }
        _ => false,
    }
}
