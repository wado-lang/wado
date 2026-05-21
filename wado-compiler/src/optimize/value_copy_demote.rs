//! `value_copy_demote` — demote a deep `$value_copy$T` of an `Array<E>` to a
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
//! written to be reused by future passes (e.g. a `container_sroa` migration).

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::nir::{
    FunctionKind, FunctionRef, NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind,
    NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeTable};

type FuncKey = (ModuleSource, String);

fn builtin_gname(func: &FunctionRef) -> Option<String> {
    func.builtin_name()
        .or_else(|| func.monomorphized_builtin_name())
}

pub fn demote_value_copies(project: &mut NirPackage) {
    // Map every function to its index for callee lookup.
    let mut by_key: IndexMap<FuncKey, usize> = IndexMap::default();
    for (i, f) in project.functions.iter().enumerate() {
        let f = f.borrow();
        by_key.insert((f.module_source.clone(), f.name.clone()), i);
    }

    // Identify `$value_copy$T` helpers whose body is an `Array<E>` wrapper
    // copy: `return StructLiteral { repr: array_clone(v.repr), used: ... }`.
    let mut array_wrapper_copies: IndexSet<FuncKey> = IndexSet::default();
    for f in &project.functions {
        let f = f.borrow();
        if f.value_copy_type().is_some()
            && let Some(body) = &f.body
            && body_is_array_wrapper_copy(body)
        {
            array_wrapper_copies.insert((f.module_source.clone(), f.name.clone()));
        }
    }
    crate::compiler_trace!(
        "demote",
        "array-wrapper value-copy helpers: {}",
        array_wrapper_copies.len()
    );
    if array_wrapper_copies.is_empty() {
        return;
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
        let Some(body) = &f.body else { continue };
        collect_sites(
            body,
            &array_wrapper_copies,
            body,
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
        return;
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
    for fi in touched {
        let mut f = project.functions[fi].borrow_mut();
        if let Some(body) = &mut f.body {
            retarget_block(body, fi, &site_elig, &array_wrapper_copies, &shallow_name);
        }
    }
    project.functions.extend(new_funcs);
}

// ---------------------------------------------------------------------------
// Helper-shape detection / rewrite
// ---------------------------------------------------------------------------

fn body_is_array_wrapper_copy(body: &NirBlock) -> bool {
    // `return StructLiteral { fields: [.., repr: Call(array_clone, ..), ..] }`
    for stmt in &body.stmts {
        if let NirStmtKind::Return { value: Some(v) } = &stmt.kind
            && let NirExprKind::StructLiteral { fields, .. } = &v.kind
        {
            return fields.iter().any(|fld| {
                fld.name == "repr"
                    && matches!(&fld.value.kind,
                        NirExprKind::Call { func, .. }
                            if builtin_gname(func).as_deref() == Some("builtin::array_clone"))
            });
        }
    }
    false
}

fn rewrite_array_clone_to_shallow(body: &mut NirBlock) {
    for stmt in &mut body.stmts {
        rewrite_stmt(stmt);
    }
}

fn rewrite_stmt(stmt: &mut NirStmt) {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::Expr(value) => rewrite_expr(value),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_expr(v);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_expr(condition);
            rewrite_array_clone_to_shallow(then_block);
            if let Some(eb) = else_block {
                rewrite_array_clone_to_shallow(eb);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_array_clone_to_shallow(body);
        }
        NirStmtKind::LetDestructure { value, .. } => rewrite_expr(value),
        NirStmtKind::Continue => {}
    }
}

fn rewrite_expr(expr: &mut NirExpr) {
    if let NirExprKind::Call { func, .. } = &mut expr.kind
        && builtin_gname(func).as_deref() == Some("builtin::array_clone")
    {
        func.name = "array_clone_shallow".to_string();
        if let Some(mi) = &mut func.monomorph_info {
            mi.generic_name = "array_clone_shallow".to_string();
        }
    }
    for child in expr_children_mut(expr) {
        rewrite_expr(child);
    }
}

// ---------------------------------------------------------------------------
// Call-site collection / retargeting
// ---------------------------------------------------------------------------

/// If `value` is a one-argument call to an array-wrapper `$value_copy$T`
/// helper, return that helper's key.
fn wrapper_call_key(value: &NirExpr, wrappers: &IndexSet<FuncKey>) -> Option<FuncKey> {
    if let NirExprKind::Call { func, args, .. } = &value.kind
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
fn stmt_binding(stmt: &NirStmt) -> Option<(&NirExpr, u32)> {
    match &stmt.kind {
        NirStmtKind::Let {
            local_index, value, ..
        } => Some((value, *local_index)),
        NirStmtKind::Expr(e) => {
            if let NirExprKind::Assign { target, value } = &e.kind
                && let NirExprKind::Local { index, .. } = target.kind
            {
                Some((value, index))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Walk `block`; for every `let x = $value_copy$T(arg)` (and `Assign` form)
/// binding to a value-copy-wrapper helper, AND-combine its eligibility into
/// `site_elig[(fi, x)]`. Every nested block is visited exactly once.
fn collect_sites(
    block: &NirBlock,
    wrappers: &IndexSet<FuncKey>,
    fn_body: &NirBlock,
    params: &[crate::nir::NirParam],
    fi: usize,
    an: &mut Analyzer,
    site_elig: &mut IndexMap<(usize, u32), bool>,
    site_key: &mut IndexMap<(usize, u32), FuncKey>,
) {
    for stmt in &block.stmts {
        if let Some((value, target)) = stmt_binding(stmt)
            && let Some(key) = wrapper_call_key(value, wrappers)
        {
            let elig = demote_candidate(value, target, fn_body, params, an);
            let loc = (fi, target);
            site_elig
                .entry(loc)
                .and_modify(|e| *e &= elig)
                .or_insert(elig);
            site_key.entry(loc).or_insert(key);
        }
        // Descend into every sub-block one level down, exactly once.
        let mut subs: Vec<&NirBlock> = Vec::new();
        match &stmt.kind {
            NirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                subs.push(then_block);
                if let Some(eb) = else_block {
                    subs.push(eb);
                }
            }
            NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
                subs.push(body);
            }
            _ => {}
        }
        for e in stmt_own_exprs(stmt) {
            direct_blocks_in_expr(e, &mut subs);
        }
        for sub in subs {
            collect_sites(sub, wrappers, fn_body, params, fi, an, site_elig, site_key);
        }
    }
}

/// The expressions a statement owns directly (not the contents of its
/// sub-blocks).
fn stmt_own_exprs(stmt: &NirStmt) -> Vec<&NirExpr> {
    match &stmt.kind {
        NirStmtKind::Let { value, .. }
        | NirStmtKind::Expr(value)
        | NirStmtKind::LetDestructure { value, .. } => vec![value],
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => value.iter().collect(),
        NirStmtKind::If { condition, .. } => vec![condition],
        NirStmtKind::Loop { .. } | NirStmtKind::LabeledBlock { .. } | NirStmtKind::Continue => {
            vec![]
        }
    }
}

/// Collect the top-most `NirBlock`s embedded in `expr` (an `if`/`block`/
/// `match`/`switch` rvalue), without descending into the blocks themselves.
fn direct_blocks_in_expr<'a>(expr: &'a NirExpr, out: &mut Vec<&'a NirBlock>) {
    match &expr.kind {
        NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => {
            out.push(b);
            return;
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            direct_blocks_in_expr(condition, out);
            out.push(then_branch);
            if let Some(eb) = else_branch {
                out.push(eb);
            }
            return;
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            direct_blocks_in_expr(scrutinee, out);
            for a in arms {
                out.push(a);
            }
            out.push(default);
            return;
        }
        _ => {}
    }
    for child in expr_children(expr) {
        direct_blocks_in_expr(child, out);
    }
}

/// Walk the projections (`*`, `&`, `.field`, `[i]`, cast) that share storage
/// with their inner expression to the local the value-copy argument reads
/// from. `None` for a genuinely fresh rvalue (call result, literal,
/// constructor) — those are uniquely owned, so a shallow copy of them is
/// always safe.
fn arg_source_root(expr: &NirExpr) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Index { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => arg_source_root(inner),
        _ => None,
    }
}

/// True when `idx` is a parameter declared as an immutable reference (`&T`):
/// the body cannot mutate `*idx` at all, so its elements are element-clean.
fn is_immutable_ref_param(
    params: &[crate::nir::NirParam],
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
    value: &NirExpr,
    target_idx: u32,
    fn_body: &NirBlock,
    params: &[crate::nir::NirParam],
    an: &mut Analyzer,
) -> bool {
    let NirExprKind::Call { args, .. } = &value.kind else {
        return false;
    };
    if !an.handle_is_element_clean(fn_body, target_idx) {
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
    match arg_source_root(&args[0].expr) {
        None => true,
        Some(root) => {
            let clean = is_immutable_ref_param(params, an.type_table, root)
                || an.arg_local_is_element_clean(fn_body, root);
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

/// Phase 2b mechanical rewrite: visit every nested block, and for each
/// `let x = …` / `x = …` binding whose `(fi, x)` is marked eligible, retarget
/// the value-copy-wrapper call to its shallow sibling. Pure rewrite — no
/// analysis, so it cannot conflict with the `&mut` borrow of the function.
fn retarget_block(
    block: &mut NirBlock,
    fi: usize,
    site_elig: &IndexMap<(usize, u32), bool>,
    wrappers: &IndexSet<FuncKey>,
    shallow_name: &IndexMap<FuncKey, String>,
) {
    for stmt in &mut block.stmts {
        let target = match &stmt.kind {
            NirStmtKind::Let { local_index, .. } => Some(*local_index),
            NirStmtKind::Expr(e) => match &e.kind {
                NirExprKind::Assign { target, .. } => match target.kind {
                    NirExprKind::Local { index, .. } => Some(index),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        if let Some(target) = target
            && site_elig.get(&(fi, target)) == Some(&true)
            && let Some(value) = stmt_binding_value_mut(stmt)
        {
            retarget_wrapper_call(value, wrappers, shallow_name);
        }
        for sub in stmt_subblocks_mut(stmt) {
            retarget_block(sub, fi, site_elig, wrappers, shallow_name);
        }
    }
}

/// The mutable binding value of a `let` / `Assign` statement.
fn stmt_binding_value_mut(stmt: &mut NirStmt) -> Option<&mut NirExpr> {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => Some(value),
        NirStmtKind::Expr(e) => {
            if let NirExprKind::Assign { value, .. } = &mut e.kind {
                Some(value)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Rewrite a value-copy-wrapper call to its synthesized shallow sibling.
fn retarget_wrapper_call(
    value: &mut NirExpr,
    wrappers: &IndexSet<FuncKey>,
    shallow_name: &IndexMap<FuncKey, String>,
) {
    if let NirExprKind::Call { func, args, .. } = &mut value.kind
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

/// Every `NirBlock` one level down from `stmt` — statement-level branches
/// and blocks embedded in the statement's own expressions.
fn stmt_subblocks_mut(stmt: &mut NirStmt) -> Vec<&mut NirBlock> {
    let mut out: Vec<&mut NirBlock> = Vec::new();
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. }
        | NirStmtKind::Expr(value)
        | NirStmtKind::LetDestructure { value, .. } => blocks_in_expr_mut(value, &mut out),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                blocks_in_expr_mut(v, &mut out);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            blocks_in_expr_mut(condition, &mut out);
            out.push(then_block);
            if let Some(eb) = else_block {
                out.push(eb);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            out.push(body);
        }
        NirStmtKind::Continue => {}
    }
    out
}

/// `&mut` mirror of [`direct_blocks_in_expr`]. The classify-then-borrow split
/// keeps `expr` borrowed mutably on exactly one path (the block-bearing match
/// arms *or* the `expr_children_mut` recursion, never both).
fn blocks_in_expr_mut<'a>(expr: &'a mut NirExpr, out: &mut Vec<&'a mut NirBlock>) {
    let block_bearing = matches!(
        &expr.kind,
        NirExprKind::Block(_)
            | NirExprKind::LabeledBlock { .. }
            | NirExprKind::If { .. }
            | NirExprKind::Switch { .. }
    );
    if block_bearing {
        match &mut expr.kind {
            NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => out.push(b),
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                blocks_in_expr_mut(condition, out);
                out.push(then_branch);
                if let Some(eb) = else_branch {
                    out.push(eb);
                }
            }
            NirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                blocks_in_expr_mut(scrutinee, out);
                for a in arms {
                    out.push(a);
                }
                out.push(default);
            }
            _ => unreachable!("block_bearing checked above"),
        }
    } else {
        for child in expr_children_mut(expr) {
            blocks_in_expr_mut(child, out);
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
                self.verify_block(&body, &mut tainted, visiting)
            }
            None => false,
        };
        visiting.swap_remove(key);
        self.eimm_memo.insert(key.clone(), result);
        result
    }

    fn verify_block(
        &mut self,
        block: &NirBlock,
        tainted: &mut IndexSet<u32>,
        visiting: &mut IndexSet<FuncKey>,
    ) -> bool {
        for stmt in &block.stmts {
            if !self.verify_stmt(stmt, tainted, visiting) {
                return false;
            }
        }
        true
    }

    fn verify_stmt(
        &mut self,
        stmt: &NirStmt,
        tainted: &mut IndexSet<u32>,
        visiting: &mut IndexSet<FuncKey>,
    ) -> bool {
        match &stmt.kind {
            NirStmtKind::Let {
                local_index, value, ..
            } => {
                if !self.verify_expr(value, tainted, visiting) {
                    return false;
                }
                if is_self_derived(value, tainted, self.type_table) {
                    tainted.insert(*local_index);
                }
                true
            }
            NirStmtKind::Expr(e) => {
                if !self.verify_expr(e, tainted, visiting) {
                    return false;
                }
                // Assign-form taint: `x = <self-derived>` makes `x` alias
                // self's storage thereafter, just like a `let` binding.
                if let NirExprKind::Assign { target, value } = &e.kind
                    && let NirExprKind::Local { index, .. } = target.kind
                    && is_self_derived(value, tainted, self.type_table)
                {
                    tainted.insert(index);
                }
                true
            }
            NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => value
                .as_ref()
                .map(|v| self.verify_expr(v, tainted, visiting))
                .unwrap_or(true),
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.verify_expr(condition, tainted, visiting)
                    && self.verify_block(then_block, tainted, visiting)
                    && else_block
                        .as_ref()
                        .map(|eb| self.verify_block(eb, tainted, visiting))
                        .unwrap_or(true)
            }
            NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
                self.verify_block(body, tainted, visiting)
            }
            NirStmtKind::LetDestructure { value, .. } => self.verify_expr(value, tainted, visiting),
            NirStmtKind::Continue => true,
        }
    }

    fn verify_expr(
        &mut self,
        expr: &NirExpr,
        tainted: &IndexSet<u32>,
        visiting: &mut IndexSet<FuncKey>,
    ) -> bool {
        match &expr.kind {
            // `&mut <self-derived>` would expose mutable element access.
            NirExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if is_self_derived(inner, tainted, self.type_table) {
                    crate::compiler_trace!("demote", "verify reject: &mut of self-derived");
                    return false;
                }
            }
            // Field write to a self-derived base other than `self` itself
            // (writing `self.repr`/`self.used` is a spine op; writing
            // `array_get(...).field` is an element mutation).
            NirExprKind::Assign { target, value } => {
                let bad = match &target.kind {
                    NirExprKind::FieldAccess { expr: base, .. } => {
                        is_self_derived(base, tainted, self.type_table)
                            && !matches!(base.kind, NirExprKind::Local { index: 0, .. })
                    }
                    NirExprKind::Index { expr: base, .. } => {
                        is_self_derived(base, tainted, self.type_table)
                    }
                    _ => false,
                };
                if bad {
                    crate::compiler_trace!("demote", "verify reject: element field/index write");
                    return false;
                }
                return self.verify_expr(target, tainted, visiting)
                    && self.verify_expr(value, tainted, visiting);
            }
            NirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                let key = (func.module_source.clone(), func.name.clone());
                // A call whose receiver is self-derived may mutate an
                // element unless the callee is known `&self` (cannot
                // mutate) or a verified element-immutable `&mut self`
                // method. An unresolvable callee is conservatively unsafe.
                if is_self_derived(receiver, tainted, self.type_table) {
                    let ok = match self.callee_mutates_self(&key) {
                        Some(false) => true,
                        Some(true) => self.verify(&key, visiting),
                        None => false,
                    };
                    if !ok {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: unsafe call on self-derived recv {}",
                            key.1
                        );
                        return false;
                    }
                }
                if !self.verify_expr(receiver, tainted, visiting) {
                    return false;
                }
                for a in args {
                    if !self.verify_call_arg(&a.expr, tainted, visiting) {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: bad arg to method {}",
                            key.1
                        );
                        return false;
                    }
                }
                return true;
            }
            NirExprKind::Call { func, args, .. } => {
                // No builtin intrinsic mutates an element's pointee
                // (`array_set` rewrites a spine slot; `array_get` / `select`
                // only forward values), so a self-derived arg to a builtin
                // is harmless. Opaque (non-builtin) calls still gate.
                let is_builtin = builtin_gname(func).is_some();
                for a in args {
                    if is_builtin {
                        if !self.verify_expr(&a.expr, tainted, visiting) {
                            return false;
                        }
                    } else if !self.verify_call_arg(&a.expr, tainted, visiting) {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: self-derived arg to call {}",
                            func.name
                        );
                        return false;
                    }
                }
                return true;
            }
            NirExprKind::IndirectCall { callee, args } => {
                // Invoking a closure that captured a self-derived value runs
                // its (unverified) body with access to `self`'s elements.
                if is_self_derived(callee, tainted, self.type_table) {
                    crate::compiler_trace!(
                        "demote",
                        "verify reject: indirect call of self-capturing closure"
                    );
                    return false;
                }
                if !self.verify_expr(callee, tainted, visiting) {
                    return false;
                }
                for a in args {
                    if !self.verify_call_arg(a, tainted, visiting) {
                        crate::compiler_trace!(
                            "demote",
                            "verify reject: self-derived arg to indirect call"
                        );
                        return false;
                    }
                }
                return true;
            }
            _ => {}
        }
        for child in expr_children(expr) {
            if !self.verify_expr(child, tainted, visiting) {
                return false;
            }
        }
        true
    }

    /// A self-derived value handed to an opaque (non-spine-builtin) call may
    /// only flow through an immutable `&` borrow.
    fn verify_call_arg(
        &mut self,
        arg: &NirExpr,
        tainted: &IndexSet<u32>,
        visiting: &mut IndexSet<FuncKey>,
    ) -> bool {
        if let NirExprKind::Unary {
            op: NirUnaryOp::Ref,
            ..
        } = &arg.kind
        {
            return self.verify_expr(arg, tainted, visiting);
        }
        if is_self_derived(arg, tainted, self.type_table) {
            return false;
        }
        self.verify_expr(arg, tainted, visiting)
    }

    /// True when every use of local `idx` in `fn_body` keeps the array's
    /// elements immutable: spine-only methods, index/field reads, by-value
    /// or `&` argument passing.
    fn handle_is_element_clean(&mut self, fn_body: &NirBlock, idx: u32) -> bool {
        self.handle_block(fn_body, idx)
    }

    fn handle_block(&mut self, block: &NirBlock, idx: u32) -> bool {
        for stmt in &block.stmts {
            if !self.handle_stmt(stmt, idx) {
                return false;
            }
        }
        true
    }

    fn handle_stmt(&mut self, stmt: &NirStmt, idx: u32) -> bool {
        match &stmt.kind {
            NirStmtKind::Let { value, .. } | NirStmtKind::Expr(value) => {
                self.handle_expr(value, idx)
            }
            NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => value
                .as_ref()
                .map(|v| self.handle_expr(v, idx))
                .unwrap_or(true),
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.handle_expr(condition, idx)
                    && self.handle_block(then_block, idx)
                    && else_block
                        .as_ref()
                        .map(|eb| self.handle_block(eb, idx))
                        .unwrap_or(true)
            }
            NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
                self.handle_block(body, idx)
            }
            NirStmtKind::LetDestructure { value, .. } => self.handle_expr(value, idx),
            NirStmtKind::Continue => true,
        }
    }

    /// Returns false if `expr` contains a use of `Local(idx)` that is not a
    /// recognized element-clean use.
    fn handle_expr(&mut self, expr: &NirExpr, idx: u32) -> bool {
        match &expr.kind {
            NirExprKind::Local { index, .. } => *index != idx,
            NirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                // The receiver auto-refs to `&self` / `&mut self`, so strip
                // the wrapping reference before matching the handle.
                let recv = strip_refs(receiver);
                let key = (func.module_source.clone(), func.name.clone());
                if is_local(recv, idx) {
                    // Receiver is the handle itself: `x.method()`.
                    let safe = match self.callee_mutates_self(&key) {
                        Some(false) => true, // &self
                        Some(true) => self.is_method_element_immutable(&key),
                        None => false,
                    };
                    if !safe {
                        return false;
                    }
                } else if expr_mentions_local(receiver, idx) {
                    // The handle appears inside the receiver — an element
                    // or field of it (`x[i].method()`, `x.get(i).method()`,
                    // `x.field.method()`). A `&mut self` method there may
                    // mutate an element, which a shallow copy would share.
                    // A `&self` method is a read; recurse to vet the
                    // receiver. (`x[i]` lowers to an `Array::index` method
                    // call, not a bare `Index`, so a structural root check
                    // is not enough — match on the handle appearing at all.)
                    if self.callee_mutates_self(&key) != Some(false) {
                        return false;
                    }
                    if !self.handle_expr(receiver, idx) {
                        return false;
                    }
                } else if !self.handle_expr(receiver, idx) {
                    return false;
                }
                args.iter().all(|a| self.handle_call_arg(&a.expr, idx))
            }
            NirExprKind::Call { args, .. } => {
                args.iter().all(|a| self.handle_call_arg(&a.expr, idx))
            }
            NirExprKind::IndirectCall { callee, args } => {
                self.handle_expr(callee, idx) && args.iter().all(|a| self.handle_call_arg(a, idx))
            }
            NirExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                // `&mut x`, `&mut x[i]`, `&mut x.field` — a mutable
                // reference into the handle escapes our control.
                !expr_mentions_local(inner, idx)
            }
            NirExprKind::Index { expr: base, index } => {
                // Index read: `x[i]` produces an element copy.
                let base_ok = is_local(base, idx) || self.handle_expr(base, idx);
                base_ok && self.handle_expr(index, idx)
            }
            NirExprKind::FieldAccess { expr: base, .. } => {
                is_local(base, idx) || self.handle_expr(base, idx)
            }
            NirExprKind::Assign { target, value } => {
                // A write whose target touches `idx` escapes our control.
                if expr_mentions_local(target, idx) {
                    return false;
                }
                self.handle_expr(value, idx)
            }
            _ => {
                for child in expr_children(expr) {
                    if !self.handle_expr(child, idx) {
                        return false;
                    }
                }
                true
            }
        }
    }

    /// `idx` passed as a call argument: by-value or `&` is element-clean;
    /// `&mut idx` is not.
    fn handle_call_arg(&mut self, arg: &NirExpr, idx: u32) -> bool {
        match &arg.kind {
            // Passing the handle by value hands the callee an independent
            // (deep) copy, so it cannot reach our elements.
            NirExprKind::Local { .. } => true,
            NirExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => !expr_mentions_local(inner, idx),
            NirExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: inner,
            } => is_local(inner, idx) || self.handle_expr(inner, idx),
            _ => self.handle_expr(arg, idx),
        }
    }

    /// The `arg` of a value-copy: element-clean when it is an immutable-ref
    /// parameter (the body cannot mutate `*arg` at all) or when every use is
    /// element-clean.
    fn arg_local_is_element_clean(&mut self, fn_body: &NirBlock, idx: u32) -> bool {
        self.handle_is_element_clean(fn_body, idx)
    }
}

fn is_local(expr: &NirExpr, idx: u32) -> bool {
    matches!(&expr.kind, NirExprKind::Local { index, .. } if *index == idx)
}

/// Strip outer auto-ref / deref wrappers from a method-call receiver.
fn strip_refs(expr: &NirExpr) -> &NirExpr {
    match &expr.kind {
        NirExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => strip_refs(inner),
        _ => expr,
    }
}

fn expr_mentions_local(expr: &NirExpr, idx: u32) -> bool {
    if is_local(expr, idx) {
        return true;
    }
    expr_children(expr).any(|c| expr_mentions_local(c, idx))
}

/// True when `expr` produces a value that may alias `self`'s storage — the
/// tracked local itself, a projection of it, an `array_get` element read of a
/// tracked spine, or an aggregate / closure that captures any such value.
///
/// A primitive-typed value is excluded: reading `self.used` (an `i32`)
/// produces an independent copy, so passing it around cannot reach `self`.
fn is_self_derived(expr: &NirExpr, tainted: &IndexSet<u32>, tt: &Rc<RefCell<TypeTable>>) -> bool {
    if matches!(tt.borrow().get(expr.type_id), ResolvedType::Primitive(_)) {
        return false;
    }
    match &expr.kind {
        NirExprKind::Local { index, .. } => tainted.contains(index),
        NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Index { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::Unary { expr: inner, .. } => is_self_derived(inner, tainted, tt),
        NirExprKind::Call { func, args, .. } => {
            // `array_get(spine, _)` yields an element of the spine; other
            // array builtins (`array_clone`, `array_new`) produce fresh
            // storage and are not self-derived.
            builtin_gname(func).as_deref() == Some("builtin::array_get")
                && args
                    .first()
                    .is_some_and(|a| is_self_derived(&a.expr, tainted, tt))
        }
        // An aggregate / closure that embeds a self-derived value carries
        // that aliasing storage. Tainting it lets the mutation checks below
        // (field write, `&mut`, `&mut self` call, indirect call) fire on the
        // aggregate / closure too.
        NirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| is_self_derived(&f.value, tainted, tt)),
        NirExprKind::TupleLiteral { elements } => {
            elements.iter().any(|e| is_self_derived(e, tainted, tt))
        }
        NirExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .is_some_and(|p| is_self_derived(p, tainted, tt)),
        NirExprKind::ClosureToCanonical { functor, .. } => is_self_derived(functor, tainted, tt),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Generic child traversal
// ---------------------------------------------------------------------------

fn expr_children(expr: &NirExpr) -> Box<dyn Iterator<Item = &NirExpr> + '_> {
    use NirExprKind::{
        Assign, Binary, Block, BoolLiteral, BytesLiteral, Call, Cast, CharLiteral,
        ClosureToCanonical, CmRawCall, EnumConstruct, FieldAccess, FloatLiteral, GlobalVarGet,
        GlobalVarSet, If, Index, IndirectCall, IntLiteral, LabeledBlock, Local, Match, MethodCall,
        Null, StringLiteral, StructLiteral, Switch, TupleLiteral, Unary, Unit, VariantConstruct,
        VariantPayload, VariantTag, VariantTest,
    };
    match &expr.kind {
        Binary { left, right, .. } => Box::new([left.as_ref(), right.as_ref()].into_iter()),
        Unary { expr: e, .. }
        | Cast { expr: e, .. }
        | FieldAccess { expr: e, .. }
        | VariantTag { expr: e }
        | VariantTest { expr: e, .. }
        | VariantPayload { expr: e, .. }
        | GlobalVarSet { value: e, .. } => Box::new(std::iter::once(e.as_ref())),
        Assign { target, value } => Box::new([target.as_ref(), value.as_ref()].into_iter()),
        Index { expr: e, index } => Box::new([e.as_ref(), index.as_ref()].into_iter()),
        Call { args, .. } => Box::new(args.iter().map(|a| &a.expr)),
        CmRawCall { args, .. } => Box::new(args.iter()),
        MethodCall { receiver, args, .. } => {
            Box::new(std::iter::once(receiver.as_ref()).chain(args.iter().map(|a| &a.expr)))
        }
        IndirectCall { callee, args } => {
            Box::new(std::iter::once(callee.as_ref()).chain(args.iter()))
        }
        ClosureToCanonical { functor, .. } => Box::new(std::iter::once(functor.as_ref())),
        Block(b) | LabeledBlock { block: b, .. } => Box::new(block_exprs(b)),
        If {
            condition,
            then_branch,
            else_branch,
        } => Box::new(
            std::iter::once(condition.as_ref())
                .chain(block_exprs(then_branch))
                .chain(else_branch.iter().flat_map(block_exprs)),
        ),
        Match { expr: e, arms } => Box::new(
            std::iter::once(e.as_ref()).chain(
                arms.iter()
                    .flat_map(|a| a.guard.iter().chain(std::iter::once(&a.body))),
            ),
        ),
        StructLiteral { fields, .. } => Box::new(fields.iter().map(|f| &f.value)),
        TupleLiteral { elements } => Box::new(elements.iter()),
        VariantConstruct { payload, .. } => {
            Box::new(payload.iter().map(std::convert::AsRef::as_ref))
        }
        Switch {
            scrutinee,
            arms,
            default,
            ..
        } => Box::new(
            std::iter::once(scrutinee.as_ref())
                .chain(arms.iter().flat_map(block_exprs))
                .chain(block_exprs(default)),
        ),
        IntLiteral { .. }
        | FloatLiteral { .. }
        | BoolLiteral(_)
        | CharLiteral(_)
        | StringLiteral(_)
        | BytesLiteral(_)
        | Null
        | Unit
        | Local { .. }
        | GlobalVarGet { .. }
        | EnumConstruct { .. } => Box::new(std::iter::empty()),
    }
}

fn block_exprs(block: &NirBlock) -> impl Iterator<Item = &NirExpr> {
    block.stmts.iter().flat_map(stmt_exprs)
}

fn stmt_exprs(stmt: &NirStmt) -> Box<dyn Iterator<Item = &NirExpr> + '_> {
    match &stmt.kind {
        NirStmtKind::Let { value, .. }
        | NirStmtKind::Expr(value)
        | NirStmtKind::LetDestructure { value, .. } => Box::new(std::iter::once(value)),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => Box::new(value.iter()),
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => Box::new(
            std::iter::once(condition)
                .chain(block_exprs(then_block))
                .chain(else_block.iter().flat_map(block_exprs)),
        ),
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            Box::new(block_exprs(body))
        }
        NirStmtKind::Continue => Box::new(std::iter::empty()),
    }
}

fn expr_children_mut(expr: &mut NirExpr) -> Vec<&mut NirExpr> {
    use NirExprKind::{
        Assign, Binary, Block, BoolLiteral, BytesLiteral, Call, Cast, CharLiteral,
        ClosureToCanonical, CmRawCall, EnumConstruct, FieldAccess, FloatLiteral, GlobalVarGet,
        GlobalVarSet, If, Index, IndirectCall, IntLiteral, LabeledBlock, Local, Match, MethodCall,
        Null, StringLiteral, StructLiteral, Switch, TupleLiteral, Unary, Unit, VariantConstruct,
        VariantPayload, VariantTag, VariantTest,
    };
    match &mut expr.kind {
        Binary { left, right, .. }
        | Assign {
            target: left,
            value: right,
        }
        | Index {
            expr: left,
            index: right,
        } => vec![left.as_mut(), right.as_mut()],
        Unary { expr: e, .. }
        | Cast { expr: e, .. }
        | FieldAccess { expr: e, .. }
        | VariantTag { expr: e }
        | VariantTest { expr: e, .. }
        | VariantPayload { expr: e, .. }
        | GlobalVarSet { value: e, .. }
        | ClosureToCanonical { functor: e, .. } => vec![e.as_mut()],
        Call { args, .. } => args.iter_mut().map(|a| &mut a.expr).collect(),
        CmRawCall { args, .. } => args.iter_mut().collect(),
        MethodCall { receiver, args, .. } => std::iter::once(receiver.as_mut())
            .chain(args.iter_mut().map(|a| &mut a.expr))
            .collect(),
        IndirectCall { callee, args } => std::iter::once(callee.as_mut())
            .chain(args.iter_mut())
            .collect(),
        Block(b) | LabeledBlock { block: b, .. } => block_exprs_mut(b),
        If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut v = vec![condition.as_mut()];
            v.extend(block_exprs_mut(then_branch));
            if let Some(eb) = else_branch {
                v.extend(block_exprs_mut(eb));
            }
            v
        }
        Match { expr: e, arms } => {
            let mut v = vec![e.as_mut()];
            for a in arms {
                if let Some(g) = &mut a.guard {
                    v.push(g);
                }
                v.push(&mut a.body);
            }
            v
        }
        StructLiteral { fields, .. } => fields.iter_mut().map(|f| &mut f.value).collect(),
        TupleLiteral { elements } => elements.iter_mut().collect(),
        VariantConstruct { payload, .. } => payload
            .iter_mut()
            .map(std::convert::AsMut::as_mut)
            .collect(),
        Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut v = vec![scrutinee.as_mut()];
            for a in arms {
                v.extend(block_exprs_mut(a));
            }
            v.extend(block_exprs_mut(default));
            v
        }
        IntLiteral { .. }
        | FloatLiteral { .. }
        | BoolLiteral(_)
        | CharLiteral(_)
        | StringLiteral(_)
        | BytesLiteral(_)
        | Null
        | Unit
        | Local { .. }
        | GlobalVarGet { .. }
        | EnumConstruct { .. } => vec![],
    }
}

fn block_exprs_mut(block: &mut NirBlock) -> Vec<&mut NirExpr> {
    block.stmts.iter_mut().flat_map(stmt_exprs_mut).collect()
}

fn stmt_exprs_mut(stmt: &mut NirStmt) -> Vec<&mut NirExpr> {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. }
        | NirStmtKind::Expr(value)
        | NirStmtKind::LetDestructure { value, .. } => vec![value],
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            value.iter_mut().collect()
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut v = vec![condition];
            v.extend(block_exprs_mut(then_block));
            if let Some(eb) = else_block {
                v.extend(block_exprs_mut(eb));
            }
            v
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            block_exprs_mut(body)
        }
        NirStmtKind::Continue => vec![],
    }
}
