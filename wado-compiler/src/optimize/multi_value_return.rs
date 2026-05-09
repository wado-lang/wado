//! Multi-value return ABI classification (TIR-level).
//!
//! Decides which tuple-returning user functions should use the multi-value
//! Wasm return ABI (each tuple element a separate result) instead of the
//! default heap-struct ABI. The decision is recorded on
//! [`TirFunction.return_abi`] and consumed by `wir_build`:
//! - `wir_build::functions` emits the multi-value Wasm signature on the
//!   function definition.
//! - `wir_build::translate::FunctionTranslator::try_emit_multi_value_let`
//!   rewrites a call site's `let __tmp = Call(f)` into
//!   `MultiValueLocalBind [__tmp_0, …] = Call(f)` with subsequent
//!   `MultiValueProject` reads going to the split WIR locals directly.
//! - `wir_build::translate`'s `Return` arm unwraps the body's
//!   `MultiValueStructNew` so the function's return pushes N values
//!   instead of wrapping them in a heap struct.
//!
//! Historically this work lived in `wir_optimize::sroa_return`, which
//! pattern-matched on WIR `StructNew` returns and `StructGet` call-site
//! reads. Moving the decision to TIR has three benefits:
//!
//! 1. **Better visibility**: TIR analysis sees `MultiValueLiteral` /
//!    `MultiValueProject` directly — high-level intent — rather than
//!    re-deriving it from low-level WIR shapes that may have already been
//!    rewritten by intermediate passes.
//! 2. **Cross-function context**: function ABI is a TIR-level concept, so
//!    inliner / DCE can use the marker (e.g. an inlined multi-value call
//!    can be fused without first guessing the ABI).
//! 3. **Foundation for Stage 2**: stack-only types and tuple parameters
//!    (Phase 5 follow-up) need TIR-level ABI awareness; this pass lays
//!    the framework.
//!
//! ## Eligibility
//!
//! A function `f` is a candidate when **all** of the following hold:
//!
//! - `f` is not pinned (not exported, not used as `RefFunc`, not in an
//!   element table). Pinned functions have observable ABIs we can't change.
//! - `f`'s return type is `Tuple<T_0, …, T_n>` with `2 ≤ n ≤ 4`, and every
//!   `T_i` is an eligible scalar / non-nullable ref (matching
//!   `wir_optimize::sroa_return`'s rules — i.e. anything Wasm multi-value
//!   can carry on the stack).
//! - Every `Return` statement in `f`'s body produces a `MultiValueLiteral`
//!   of arity `n`. Variant returns are *not* candidates here (the WIR
//!   `sroa_return` variant path handles those).
//! - Every call site of `f` is `let __tmp = Call(f); …` where the only
//!   uses of `__tmp` are `MultiValueProject(LocalGet(__tmp), i)`. Bare
//!   references, address-take, closure capture, and field assignment all
//!   disqualify the candidate.
//!
//! Candidates that survive both checks have their `return_abi` set to
//! `MultiValue { result_types }` carrying the tuple element types.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    FunctionKind, ResolvedType, ReturnAbi, TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt,
    TirStmtKind, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// If `expr` is a direct `Call(f)` or `MethodCall(f)` whose callee `f` is
/// a candidate in `candidate_names`, return that candidate's index.
/// Returns `None` for any wrapped / non-candidate / non-call shape.
///
/// Used by both the safe-bind detector (`Let { value: candidate_call }`)
/// and the escape detector (a candidate call appearing in any other
/// position is an escape — the result flows somewhere we can't rewrite).
fn candidate_call_idx(
    expr: &TirExpr,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
) -> Option<usize> {
    let func = match &expr.kind {
        TirExprKind::Call { func, .. } | TirExprKind::MethodCall { func, .. } => func,
        _ => return None,
    };
    candidate_names
        .get(&(func.name.clone(), func.module_source.clone()))
        .copied()
}

/// Classify tuple-returning functions and set `return_abi` on those whose
/// every return statement and every call site permit the multi-value ABI.
pub fn classify_multi_value_returns(project: &mut FlatPackage) {
    let type_table = project.type_table.borrow();

    // Phase 1: candidate identification — body-only checks.
    let mut candidates: IndexMap<usize, Vec<TypeId>> = IndexMap::default();
    for (idx, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        if let Some(result_types) = candidate_result_types(&func, &type_table) {
            candidates.insert(idx, result_types);
        }
    }
    if candidates.is_empty() {
        return;
    }

    // Phase 2: call-site validation. Cross-function: walk every function's
    // body, including the candidates themselves (a candidate calling
    // another candidate is fine). The check in `validate_uses_in_block`
    // disqualifies a candidate when even one of its call sites uses the
    // result outside the `MultiValueProject(local)` pattern.
    //
    // Keyed by `(name, module_source)` — plain names collide across
    // modules (e.g. two `make_pair` in different `.wado` files would
    // otherwise share a key here, and a call site to one of them would
    // be erroneously charged to both candidate idxs).
    let candidate_names: IndexMap<(String, ModuleSource), usize> = candidates
        .keys()
        .map(|&idx| {
            let func = project.functions[idx].borrow();
            ((func.name.clone(), func.module_source.clone()), idx)
        })
        .collect();

    let mut invalid: IndexSet<usize> = IndexSet::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(body) = &func.body else { continue };
        validate_uses_in_block(body, &candidate_names, &mut invalid);
    }

    // Phase 3: apply. Drop disqualified candidates, set ReturnAbi on the rest.
    drop(type_table);
    for (idx, result_types) in candidates {
        if invalid.contains(&idx) {
            continue;
        }
        let mut func = project.functions[idx].borrow_mut();
        func.return_abi = ReturnAbi::MultiValue { result_types };
    }
}

/// Body-only candidate check. Returns `Some(result_types)` if the function
/// is body-eligible (return type and every return statement match).
fn candidate_result_types(func: &TirFunction, type_table: &TypeTable) -> Option<Vec<TypeId>> {
    // Restrict to ordinary (Regular) functions. Synthesised stubs
    // (`ValueCopy`, `FnCanonicalDispatch`) and the dispatch wrappers used
    // by effect handlers all have ABIs that are pinned by their callers,
    // so reshaping them via multi-value return would corrupt the runtime
    // contract.
    if !matches!(func.kind, FunctionKind::Regular) || func.is_dispatch_wrapper {
        return None;
    }
    // Skip CM / world-export wrappers — their ABI is observable at the
    // component model boundary. CM-binding wrappers also cross an ABI
    // boundary we shouldn't reshape.
    if func.is_export || func.is_cm_export || func.is_cm_binding {
        return None;
    }
    // Skip async functions — they deliver results via `task return`,
    // not a Wasm `return`, and the synthesised wrapper has its own
    // single-value return type (`unit`).
    if func.is_async {
        return None;
    }
    // Skip generics / impl-type-param polymorphic functions: those
    // shouldn't survive monomorphisation, but if a stub does linger
    // (e.g. an unreachable trait stub) its body's `Return` shapes are
    // not meaningful for the call-site rewrite.
    if func.has_real_type_params() || !func.impl_type_params.is_empty() {
        return None;
    }
    // Methods on regular `impl` blocks are eligible alongside free
    // functions: after monomorphisation a method call is a static
    // `MethodCall { func, .. }` whose `func` resolves the same way a
    // free-function `Call` does, and the call-site validation below
    // catches every shape that prevents rewriting (bare local references,
    // address-take, escape via call argument, etc.).
    //
    // Trait methods (`impl Trait for T`) and synthesized closure functor
    // `__call` methods are excluded: they participate in dispatch
    // infrastructure (effect handlers, closure functors via the `Fn<…>`
    // traits, vtable indirect calls) whose canonical Wasm signature is a
    // single-value `(ref T)` baked into the dispatch type. Reshaping the
    // impl method to return multi-value would skew the signature against
    // the vtable slot it's installed into.
    //
    // Closure functor `__call` methods are inherent methods syntactically
    // (`trait_name` is `None`) but are dispatched indirectly via the
    // `Fn<arity, ret>` canonical type, so `is_trait_method()` alone is not
    // enough — `is_closure_call()` filters them explicitly.
    //
    // A future refinement could detect "trait method whose every caller
    // is a static `MethodCall`, never an indirect dispatch" and let
    // those through, but the current bright line keeps the analysis
    // sound by construction.
    if func.is_trait_method() || func.is_closure_call() {
        return None;
    }

    let body = func.body.as_ref()?;

    // Return type must be a built-in tuple of 2-4 elements.
    let result_types = type_table.as_tuple(func.return_type)?;
    if !(2..=4).contains(&result_types.len()) {
        return None;
    }
    // Every element type must be eligible for Wasm multi-value carriage.
    for &t in &result_types {
        if !is_eligible_field_type(t, type_table) {
            return None;
        }
    }

    // Every Return statement's value must be a `MultiValueLiteral` of the
    // matching arity. Implicit tail-expression returns are normalised to
    // explicit `Return` by the lowering, so a body without any `Return`
    // is fine — it returns unit (already excluded by the tuple check).
    if !all_returns_are_multi_value_literal(body, result_types.len()) {
        return None;
    }

    Some(result_types)
}

/// Whether a TIR type can occupy a slot of a multi-value Wasm result.
///
/// All concrete types Wasm GC supports as either a value type
/// (i32 / i64 / f32 / f64 / v128) or a `(ref T)` slot are eligible.
/// Type variables and pre-mono placeholders (`TypeParam`, `TypePack`,
/// `AssocTypeProjection`) shouldn't appear in optimised TIR's tuple
/// positions — be conservative and reject them so the analysis stays
/// safe even if the invariant is ever broken.
fn is_eligible_field_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Primitive(_)
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::Newtype { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::Ref(_)
        | ResolvedType::MutRef(_)
        | ResolvedType::Reactive(_) => true,
        ResolvedType::Unit
        | ResolvedType::Never
        | ResolvedType::Function { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::Unknown
        | ResolvedType::Error => false,
    }
}

/// Check that every `return` statement in `block` (recursively, through
/// nested blocks) carries a `MultiValueLiteral` of `expected_arity`.
fn all_returns_are_multi_value_literal(block: &TirBlock, expected_arity: usize) -> bool {
    block
        .stmts
        .iter()
        .all(|stmt| stmt_returns_match(stmt, expected_arity))
}

fn stmt_returns_match(stmt: &TirStmt, expected_arity: usize) -> bool {
    match &stmt.kind {
        TirStmtKind::Return { value: None } => false, // void return on a tuple-return fn
        TirStmtKind::Return { value: Some(v) } => expr_returns_multi_value(v, expected_arity),
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            all_returns_are_multi_value_literal(then_block, expected_arity)
                && else_block
                    .as_ref()
                    .is_none_or(|b| all_returns_are_multi_value_literal(b, expected_arity))
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            all_returns_are_multi_value_literal(body, expected_arity)
        }
        TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            all_returns_are_multi_value_literal(then_block, expected_arity)
                && else_block
                    .as_ref()
                    .is_none_or(|b| all_returns_are_multi_value_literal(b, expected_arity))
        }
        // Let / Expr / Break / Continue / TaskReturn / VariadicForOf — no
        // explicit Return; recurse into nested blocks via expression walks.
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            nested_returns_in_expr_match(value, expected_arity)
        }
        TirStmtKind::Expr(e) => nested_returns_in_expr_match(e, expected_arity),
        TirStmtKind::Break { value, .. } => value
            .as_ref()
            .is_none_or(|v| nested_returns_in_expr_match(v, expected_arity)),
        TirStmtKind::Continue => true,
        TirStmtKind::TaskReturn { .. } | TirStmtKind::VariadicForOf { .. } => true,
    }
}

/// Walk an expression tree, asserting every nested explicit `Return`
/// (e.g. inside a labelled block / match arm) carries a matching
/// `MultiValueLiteral`.
fn nested_returns_in_expr_match(expr: &TirExpr, expected_arity: usize) -> bool {
    struct Walker {
        expected_arity: usize,
        ok: bool,
    }
    impl TirRefVisitor for Walker {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if !self.ok {
                return;
            }
            if !stmt_returns_match(stmt, self.expected_arity) {
                self.ok = false;
                return;
            }
            self.walk_stmt(stmt);
        }
    }
    let mut w = Walker {
        expected_arity,
        ok: true,
    };
    w.visit_expr(expr);
    w.ok
}

/// Whether the value of a `Return` statement is a tuple-shaped
/// `MultiValueLiteral` of the expected arity. Permits the value to be
/// wrapped in a labelled-block break-value position (matched by the
/// resolver for `[..spread]` rewrites and synthesis temps), since the
/// final result still flows out as the same expression.
fn expr_returns_multi_value(expr: &TirExpr, expected_arity: usize) -> bool {
    match &expr.kind {
        TirExprKind::MultiValueLiteral { elements } => elements.len() == expected_arity,
        TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } => {
            // The block's tail value is its last `Break { value }` or its
            // final expression statement — for our purposes it's enough that
            // *every* explicit Return inside still matches and that the
            // block's outer expression type is the tuple. We rely on the
            // type matching the function signature (already checked) and
            // recurse into the block to validate any nested Returns.
            all_returns_are_multi_value_literal(b, expected_arity)
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            all_returns_are_multi_value_literal(then_branch, expected_arity)
                && else_branch
                    .as_ref()
                    .is_none_or(|b| all_returns_are_multi_value_literal(b, expected_arity))
        }
        // `return match { … }`: every arm body's tail expression must
        // produce a `MultiValueLiteral` of matching arity. We approximate
        // by requiring every `Return` inside each arm body matches; the
        // arm's final tail expression is checked structurally (each arm
        // body is itself an expression).
        TirExprKind::Match { arms, .. } => arms
            .iter()
            .all(|arm| expr_returns_multi_value(&arm.body, expected_arity)),
        // `return switch { … }`: same idea as `Match`, but scrutinee
        // dispatch goes through indexed `Switch` arms (each a block).
        TirExprKind::Switch { arms, default, .. } => {
            arms.iter()
                .all(|arm| all_returns_are_multi_value_literal(arm, expected_arity))
                && all_returns_are_multi_value_literal(default, expected_arity)
        }
        // Anything else (Call result, FieldAccess, etc.) at the return
        // position would mean the function returns an opaque tuple value
        // rather than constructing a fresh one — not a candidate for the
        // multi-value rewrite (the value escapes the function as a single
        // ref through whatever computed it).
        _ => false,
    }
}

/// Visitor that disqualifies candidate functions whose call sites use the
/// result in any way other than `let __tmp = Call(f); …
/// MultiValueProject(LocalGet(__tmp), i) …`.
fn validate_uses_in_block(
    block: &TirBlock,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    invalid: &mut IndexSet<usize>,
) {
    // Track local indices that are bound to a candidate-call result in
    // the current scope. The scope ends at block exit.
    let mut tracked: IndexMap<u32, usize> = IndexMap::default();
    for stmt in &block.stmts {
        validate_stmt(stmt, candidate_names, invalid, &mut tracked);
    }
    // Block exit: any tracked locals that the visitor never confirmed
    // safe (via uses) are still considered safe — their absence of use
    // means elide_local will drop them.
}

fn validate_stmt(
    stmt: &TirStmt,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    invalid: &mut IndexSet<usize>,
    tracked: &mut IndexMap<u32, usize>,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index,
            value,
            is_mut,
            ..
        } => {
            // If the RHS is a direct call (free function or method) to a
            // candidate, start tracking this local. The bind itself is the
            // only safe shape, so we do not recurse into the Call's args.
            if !*is_mut && let Some(candidate_idx) = candidate_call_idx(value, candidate_names) {
                tracked.insert(*local_index, candidate_idx);
                return;
            }
            // Any other RHS — walk it for both (a) nested candidate calls
            // in non-bind positions (the call's result flows somewhere we
            // can't rewrite) and (b) bare references to tracked locals
            // (e.g. `let v = a.0` where `a` was bound from a candidate
            // earlier in the scope: the FieldAccess reads `a` as a single
            // ref, not as a multi-value tuple, so the candidate must
            // remain heap-resident).
            walk_expr_for_uses(value, candidate_names, invalid, tracked);
        }
        TirStmtKind::LetDestructure { value, .. } => {
            walk_expr_for_uses(value, candidate_names, invalid, tracked);
        }
        TirStmtKind::Expr(e) | TirStmtKind::Return { value: Some(e) } => {
            walk_expr_for_uses(e, candidate_names, invalid, tracked);
        }
        TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                walk_expr_for_uses(v, candidate_names, invalid, tracked);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            walk_expr_for_uses(condition, candidate_names, invalid, tracked);
            // Tracked locals propagate into nested blocks (lexical scope).
            // We clone the map so inner-block re-bindings don't leak out;
            // semantically tracked is about "this local at this point is
            // known to hold a candidate result".
            let mut inner = tracked.clone();
            for stmt in &then_block.stmts {
                validate_stmt(stmt, candidate_names, invalid, &mut inner);
            }
            if let Some(eb) = else_block {
                let mut inner = tracked.clone();
                for stmt in &eb.stmts {
                    validate_stmt(stmt, candidate_names, invalid, &mut inner);
                }
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            let mut inner = tracked.clone();
            for stmt in &body.stmts {
                validate_stmt(stmt, candidate_names, invalid, &mut inner);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            walk_expr_for_uses(scrutinee, candidate_names, invalid, tracked);
            let mut inner = tracked.clone();
            for stmt in &then_block.stmts {
                validate_stmt(stmt, candidate_names, invalid, &mut inner);
            }
            if let Some(eb) = else_block {
                let mut inner = tracked.clone();
                for stmt in &eb.stmts {
                    validate_stmt(stmt, candidate_names, invalid, &mut inner);
                }
            }
        }
        TirStmtKind::TaskReturn { .. } | TirStmtKind::VariadicForOf { .. } => {}
    }
}

/// Walk an expression tree, validating any uses of tracked candidate-call
/// locals. A use disqualifies the candidate unless it's a
/// `MultiValueProject(LocalGet(tracked))` access.
fn walk_expr_for_uses(
    expr: &TirExpr,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    match &expr.kind {
        // Safe access pattern: `MultiValueProject(LocalGet(tracked), …)`.
        // Don't recurse into the inner Local — that would mark it as a
        // bare reference.
        TirExprKind::MultiValueProject { source, .. } => {
            if matches!(&source.kind, TirExprKind::Local { index, .. }
                if tracked.contains_key(index))
            {
                return;
            }
            walk_expr_for_uses(source, candidate_names, invalid, tracked);
        }
        // Bare local reference of a tracked candidate result → escape.
        TirExprKind::Local { index, .. } => {
            if let Some(&candidate_idx) = tracked.get(index) {
                invalid.insert(candidate_idx);
            }
        }
        // Direct candidate call in non-Let position → escape (the result
        // doesn't get bound and projected; it flows somewhere we can't
        // rewrite). Both `Call` and `MethodCall` can resolve to a
        // candidate after monomorphisation.
        TirExprKind::Call { func, args, .. } => {
            if let Some(&candidate_idx) =
                candidate_names.get(&(func.name.clone(), func.module_source.clone()))
            {
                invalid.insert(candidate_idx);
            }
            for arg in args {
                walk_expr_for_uses(&arg.expr, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            if let Some(&candidate_idx) =
                candidate_names.get(&(func.name.clone(), func.module_source.clone()))
            {
                invalid.insert(candidate_idx);
            }
            walk_expr_for_uses(receiver, candidate_names, invalid, tracked);
            for arg in args {
                walk_expr_for_uses(&arg.expr, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                walk_expr_for_uses(arg, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            walk_expr_for_uses(callee, candidate_names, invalid, tracked);
            for arg in args {
                walk_expr_for_uses(arg, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } => {
            let mut inner = tracked.clone();
            for stmt in &b.stmts {
                validate_stmt(stmt, candidate_names, invalid, &mut inner);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr_for_uses(condition, candidate_names, invalid, tracked);
            let mut inner = tracked.clone();
            for stmt in &then_branch.stmts {
                validate_stmt(stmt, candidate_names, invalid, &mut inner);
            }
            if let Some(eb) = else_branch {
                let mut inner = tracked.clone();
                for stmt in &eb.stmts {
                    validate_stmt(stmt, candidate_names, invalid, &mut inner);
                }
            }
        }
        TirExprKind::Match { expr: scrut, arms } => {
            walk_expr_for_uses(scrut, candidate_names, invalid, tracked);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    walk_expr_for_uses(g, candidate_names, invalid, tracked);
                }
                walk_expr_for_uses(&arm.body, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            walk_expr_for_uses(scrutinee, candidate_names, invalid, tracked);
            for arm in arms {
                let mut inner = tracked.clone();
                for stmt in &arm.stmts {
                    validate_stmt(stmt, candidate_names, invalid, &mut inner);
                }
            }
            let mut inner = tracked.clone();
            for stmt in &default.stmts {
                validate_stmt(stmt, candidate_names, invalid, &mut inner);
            }
        }
        TirExprKind::Closure { body, .. } => {
            // Closure captures wouldn't refer to outer-tracked locals
            // through a `Local` arm — captures use `Capture { index }`.
            // But a closure body that itself calls a candidate must still
            // be walked for invalid call positions.
            walk_expr_for_uses(body, candidate_names, invalid, tracked);
        }
        // For all other expressions (binary, unary, assign, FieldAccess,
        // struct/tuple literals, etc.), walk every child with the same
        // tracker so that nested `LocalGet(p)` references are observed by
        // the bare-Local arm above and disqualify the candidate.
        // `FieldAccess { expr: LocalGet(p), .. }` is the most common form
        // — the user wrote `p.0` directly without destructuring, so the
        // tuple value flows out as a single struct ref.
        _ => recurse_children(expr, candidate_names, invalid, tracked),
    }
}

/// Visit every direct child expression of `expr` with `walk_expr_for_uses`.
/// Mirrors `tir_visitor::TirRefVisitor::walk_expr` but threads our
/// candidate / tracked state through.
fn recurse_children(
    expr: &TirExpr,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    match &expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            walk_expr_for_uses(left, candidate_names, invalid, tracked);
            walk_expr_for_uses(right, candidate_names, invalid, tracked);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
            walk_expr_for_uses(inner, candidate_names, invalid, tracked);
        }
        TirExprKind::Assign { target, value }
        | TirExprKind::Index {
            expr: target,
            index: value,
        } => {
            walk_expr_for_uses(target, candidate_names, invalid, tracked);
            walk_expr_for_uses(value, candidate_names, invalid, tracked);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                walk_expr_for_uses(&field.value, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::MultiValueLiteral { elements } => {
            for elem in elements {
                walk_expr_for_uses(elem, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                walk_expr_for_uses(p, candidate_names, invalid, tracked);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            walk_expr_for_uses(value, candidate_names, invalid, tracked);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let crate::tir::TirTemplatePart::Interpolation { expr: e, .. } = part {
                    walk_expr_for_uses(e, candidate_names, invalid, tracked);
                }
            }
        }
        TirExprKind::Resume { value } => {
            walk_expr_for_uses(value, candidate_names, invalid, tracked);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for b in bindings {
                walk_expr_for_uses(&b.handler, candidate_names, invalid, tracked);
            }
            let mut inner = tracked.clone();
            for stmt in &body.stmts {
                validate_stmt(stmt, candidate_names, invalid, &mut inner);
            }
        }
        // Leaf nodes — no nested expressions.
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        // Already handled in `walk_expr_for_uses` outer match.
        TirExprKind::MultiValueProject { .. }
        | TirExprKind::Call { .. }
        | TirExprKind::MethodCall { .. }
        | TirExprKind::CmRawCall { .. }
        | TirExprKind::IndirectCall { .. }
        | TirExprKind::Block(_)
        | TirExprKind::LabeledBlock { .. }
        | TirExprKind::If { .. }
        | TirExprKind::Match { .. }
        | TirExprKind::Switch { .. }
        | TirExprKind::Closure { .. } => {
            // Outer match should have routed these directly; if we reach
            // here something's wrong.
            unreachable!(
                "expression kind should have been handled by walk_expr_for_uses outer match: {expr:?}"
            )
        }
    }
}
