//! Multi-value return ABI classification (NIR-level).
//!
//! Decides which aggregate-returning user functions should use the
//! multi-value Wasm return ABI (each tuple element / struct field a
//! separate Wasm result) instead of the default heap-struct ABI. The
//! decision is recorded on [`NirFunction.return_abi`] and consumed by
//! `wir_build`:
//! - `wir_build::functions` emits the multi-value Wasm signature on the
//!   function definition.
//! - `wir_build::translate::FunctionTranslator::try_emit_multi_value_let`
//!   rewrites a call site's `let __tmp = Call(f)` into
//!   `MultiValueLocalBind [__tmp_0, …] = Call(f)` with subsequent
//!   `FieldAccess` reads going to the split WIR locals directly.
//! - `wir_build::translate`'s `Return` arm unwraps the body's
//!   `StructNew` so the function's return pushes N values instead of
//!   wrapping them in a heap struct.
//!
//! Historically this work lived in `wir_optimize::sroa_variant_return`
//! (then named `sroa_return`), which pattern-matched on WIR `StructNew`
//! returns and `StructGet` call-site reads. Moving the decision to NIR
//! has three benefits:
//!
//! 1. **Better visibility**: NIR analysis sees `TupleLiteral` /
//!    `StructLiteral` / `FieldAccess` directly — high-level intent —
//!    rather than re-deriving it from low-level WIR shapes that may
//!    have already been rewritten by intermediate passes.
//! 2. **Cross-function context**: function ABI is a NIR-level concept,
//!    so inliner / DCE can use the marker (e.g. an inlined multi-value
//!    call can be fused without first guessing the ABI).
//! 3. **Foundation for stack-only types**: the NIR multi-value
//!    framework lays groundwork for further ABI work.
//!
//! ## Eligibility
//!
//! A function `f` is a candidate when **all** of the following hold:
//!
//! - `f` is not pinned (not exported, not used as `RefFunc`, not in an
//!   element table). Pinned functions have observable ABIs we can't
//!   change.
//! - `f`'s return type is one of:
//!   - `Tuple<T_0, …, T_n>` with `2 ≤ n ≤ 4`, every `T_i` eligible.
//!   - A user-defined struct with 2-4 fields, every field type
//!     eligible. Variant returns are *not* candidates here (the WIR
//!     `sroa_variant_return` pass handles those).
//! - Every `Return` statement in `f`'s body produces a fresh aggregate
//!   literal of the right shape (`TupleLiteral` for tuple returns,
//!   `StructLiteral` whose `struct_type` matches the return type for
//!   struct returns) with matching arity.
//! - Every call site of `f` is `let __tmp = Call(f); …` where the only
//!   uses of `__tmp` are `FieldAccess(LocalGet(__tmp), name)` (with
//!   `name` matching one of the aggregate's field names). Bare
//!   references, address-take, closure capture, and field assignment
//!   all disqualify the candidate.
//!
//! Candidates that survive both checks have their `return_abi` set to
//! `MultiValue { result_types, field_names }` carrying the per-field
//! NIR types and names.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{
    FunctionKind, NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind, NirStruct,
    ReturnAbi,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Per-candidate body-only info: the per-field NIR types and field names
/// (in declaration order). For tuples, `field_names` is `["0", "1", ...]`.
/// For user structs, the struct's actual field names.
#[derive(Clone, Debug)]
struct CandidateInfo {
    result_types: Vec<TypeId>,
    field_names: Vec<String>,
    /// Set of `field_names` for O(1) safe-pattern membership checks.
    field_name_set: IndexSet<String>,
}

/// If `expr` is a direct `Call(f)` or `MethodCall(f)` whose callee `f` is
/// a candidate in `candidate_names`, return that candidate's index.
/// Returns `None` for any wrapped / non-candidate / non-call shape.
///
/// Used by both the safe-bind detector (`Let { value: candidate_call }`)
/// and the escape detector (a candidate call appearing in any other
/// position is an escape — the result flows somewhere we can't rewrite).
fn candidate_call_idx(
    expr: &NirExpr,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
) -> Option<usize> {
    let func = match &expr.kind {
        NirExprKind::Call { func, .. } | NirExprKind::MethodCall { func, .. } => func,
        _ => return None,
    };
    candidate_names
        .get(&(func.name.clone(), func.module_source.clone()))
        .copied()
}

/// Walk the argument expressions of a (Method)Call so any tracked-local
/// escapes inside the args invalidate the right candidate. The call's
/// own ABI is already accepted by the caller (it's the safe `let __tmp =
/// Call` shape) — only the args need recursive validation.
fn walk_call_args_for_uses(
    expr: &NirExpr,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    match &expr.kind {
        NirExprKind::Call { args, .. } => {
            for arg in args {
                walk_expr_for_uses(&arg.expr, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            walk_expr_for_uses(receiver, candidate_names, candidates, invalid, tracked);
            for arg in args {
                walk_expr_for_uses(&arg.expr, candidate_names, candidates, invalid, tracked);
            }
        }
        _ => {}
    }
}

/// Classify aggregate-returning functions and set `return_abi` on those
/// whose every return statement and every call site permit the
/// multi-value ABI.
pub fn classify_multi_value_returns(project: &mut NirPackage) {
    let type_table = project.type_table.borrow();
    let structs = &project.structs;

    // Phase 1: candidate identification — body-only checks.
    let mut candidates: IndexMap<usize, CandidateInfo> = IndexMap::default();
    for (idx, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        if let Some(info) = candidate_info(&func, &type_table, structs) {
            candidates.insert(idx, info);
        }
    }
    if candidates.is_empty() {
        return;
    }

    // Phase 2: call-site validation. Cross-function: walk every function's
    // body, including the candidates themselves (a candidate calling
    // another candidate is fine). The check in `validate_uses_in_block`
    // disqualifies a candidate when even one of its call sites uses the
    // result outside the `FieldAccess(local, name)` pattern.
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
        let Some(body) = &func.body_block() else {
            continue;
        };
        validate_uses_in_block(body, &candidate_names, &candidates, &mut invalid);
    }

    // Phase 3: apply. Drop disqualified candidates, set ReturnAbi on the rest.
    drop(type_table);
    for (idx, info) in candidates {
        if invalid.contains(&idx) {
            continue;
        }
        let mut func = project.functions[idx].borrow_mut();
        func.return_abi = ReturnAbi::MultiValue {
            result_types: info.result_types,
            field_names: info.field_names,
        };
    }
}

/// Body-only candidate check. Returns `Some(info)` if the function is
/// body-eligible (return type is tuple-or-struct, arity 2-4, all fields
/// eligible, every return statement matches the shape).
fn candidate_info(
    func: &NirFunction,
    type_table: &TypeTable,
    structs: &[NirStruct],
) -> Option<CandidateInfo> {
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
    // Trait methods and synthesized closure functor `__call` methods are
    // excluded: they participate in dispatch infrastructure (effect
    // handlers, closure functors via the `Fn<…>` traits, vtable
    // indirect calls) whose canonical Wasm signature is a single-value
    // `(ref T)` baked into the dispatch type. Reshaping the impl method
    // to return multi-value would skew the signature against the vtable
    // slot it's installed into.
    if func.is_trait_method() || func.is_closure_call() {
        return None;
    }

    let bb = func.body_block();
    let body = bb.as_ref()?;
    let return_type = func.return_type;

    // Determine the aggregate shape and per-field info.
    let (result_types, field_names, is_struct_shape) =
        aggregate_field_info(return_type, type_table, structs)?;
    if !(2..=4).contains(&result_types.len()) {
        return None;
    }
    for &t in &result_types {
        if !is_eligible_field_type(t, type_table) {
            return None;
        }
    }

    // Every Return statement's value must be a fresh aggregate literal of
    // the right shape and matching arity. Implicit tail-expression
    // returns are normalised to explicit `Return` by the lowering, so a
    // body without any `Return` is fine — it returns unit (already
    // excluded by the aggregate check).
    let expected = ExpectedShape {
        arity: result_types.len(),
        struct_type: if is_struct_shape {
            Some(return_type)
        } else {
            None
        },
    };
    if !all_returns_match_shape(body, &expected) {
        return None;
    }

    let field_name_set: IndexSet<String> = field_names.iter().cloned().collect();
    Some(CandidateInfo {
        result_types,
        field_names,
        field_name_set,
    })
}

/// Inspect `return_type` and return per-field info if the return is a
/// tuple or a user-defined struct with 2..=4 fields. Third tuple element
/// indicates whether this is a struct (vs tuple).
fn aggregate_field_info(
    return_type: TypeId,
    type_table: &TypeTable,
    structs: &[NirStruct],
) -> Option<(Vec<TypeId>, Vec<String>, bool)> {
    if let Some(elems) = type_table.as_tuple(return_type) {
        let names: Vec<String> = (0..elems.len()).map(|i| i.to_string()).collect();
        return Some((elems, names, false));
    }
    // User-defined struct?
    let resolved = type_table.get(return_type);
    if let ResolvedType::Struct {
        name,
        module_source,
        ..
    } = resolved
    {
        let s = structs
            .iter()
            .find(|s| s.name == *name && s.module_source == *module_source)?;
        // Skip generic / template structs that shouldn't be in optimised NIR.
        if !s.type_params.is_empty() {
            return None;
        }
        let result_types: Vec<TypeId> = s.fields.iter().map(|f| f.type_id).collect();
        let field_names: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();
        return Some((result_types, field_names, true));
    }
    None
}

/// Whether a NIR type can occupy a slot of a multi-value Wasm result.
///
/// All concrete types Wasm GC supports as either a value type
/// (i32 / i64 / f32 / f64 / v128) or a `(ref T)` slot are eligible.
/// Type variables and pre-mono placeholders (`TypeParam`, `TypePack`,
/// `AssocTypeProjection`) shouldn't appear in optimised NIR's tuple
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

/// Per-candidate expected-shape info threaded through the body validator.
#[derive(Clone, Copy)]
struct ExpectedShape {
    arity: usize,
    /// `Some(struct_type_id)` when the candidate's return type is a user
    /// struct. `None` for tuple returns (which use `TupleLiteral`).
    struct_type: Option<TypeId>,
}

/// Check that every `return` statement in `block` (recursively, through
/// nested blocks) carries a fresh aggregate literal of the expected shape.
fn all_returns_match_shape(block: &NirBlock, expected: &ExpectedShape) -> bool {
    block
        .stmts
        .iter()
        .all(|stmt| stmt_returns_match(stmt, expected))
}

fn stmt_returns_match(stmt: &NirStmt, expected: &ExpectedShape) -> bool {
    match &stmt.kind {
        NirStmtKind::Return { value: None } => false, // void return on aggregate-return fn
        NirStmtKind::Return { value: Some(v) } => expr_returns_match(v, expected),
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            all_returns_match_shape(then_block, expected)
                && else_block
                    .as_ref()
                    .is_none_or(|b| all_returns_match_shape(b, expected))
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            all_returns_match_shape(body, expected)
        }
        // Let / Expr / Break / Continue — no explicit Return; recurse into
        // nested blocks via expression walks.
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            nested_returns_in_expr_match(value, expected)
        }
        NirStmtKind::Expr(e) => nested_returns_in_expr_match(e, expected),
        NirStmtKind::Break { value, .. } => value
            .as_ref()
            .is_none_or(|v| nested_returns_in_expr_match(v, expected)),
        NirStmtKind::Continue => true,
    }
}

/// Walk an expression tree, asserting every nested explicit `Return`
/// (e.g. inside a labelled block / match arm) carries a matching
/// aggregate literal.
fn nested_returns_in_expr_match(expr: &NirExpr, expected: &ExpectedShape) -> bool {
    struct Walker<'a> {
        expected: &'a ExpectedShape,
        ok: bool,
    }
    impl NirRefVisitor for Walker<'_> {
        fn visit_stmt(&mut self, stmt: &NirStmt) {
            if !self.ok {
                return;
            }
            if !stmt_returns_match(stmt, self.expected) {
                self.ok = false;
                return;
            }
            self.walk_stmt(stmt);
        }
    }
    let mut w = Walker { expected, ok: true };
    w.visit_expr(expr);
    w.ok
}

/// Whether the value of a `Return` statement is a fresh aggregate literal
/// of the expected shape and arity, possibly wrapped in control-flow
/// constructs (Block, If, Match, Switch) whose tail expressions are
/// themselves fresh literals. Wrapped forms are accepted because WIR
/// build's Return arm recursively unwraps leaf `StructNew`s in nested
/// blocks / conditionals when the function's `ReturnAbi::MultiValue` is
/// set.
fn expr_returns_match(expr: &NirExpr, expected: &ExpectedShape) -> bool {
    // Diverging arms (`_ => unreachable()`, etc.) carry control flow away
    // before the function-level Return is reached, so they don't need to
    // produce a fresh aggregate. Accept them so e.g. `pow10_coarse`'s
    // `_ => unreachable()` exhaustive-match branch isn't a disqualifier.
    if expr.type_id == TypeTable::NEVER {
        return true;
    }
    match &expr.kind {
        NirExprKind::TupleLiteral { elements } => {
            expected.struct_type.is_none() && elements.len() == expected.arity
        }
        NirExprKind::StructLiteral {
            struct_type,
            fields,
            ..
        } => match expected.struct_type {
            Some(t) => *struct_type == t && fields.len() == expected.arity,
            None => false,
        },
        NirExprKind::Block(b) => {
            // Plain block: only the tail expression flows out (no
            // `break` can target an unlabelled block from outside).
            all_returns_match_shape(b, expected) && block_tail_returns_match(b, expected)
        }
        NirExprKind::LabeledBlock { block: b, .. } => {
            // Labelled block: tail expression *and* every `break`
            // targeting this label flow out as the function's return
            // value. Without label-tracking we can't tell which inner
            // breaks target this block versus an outer one, so we
            // require *every* break-with-value inside the block to
            // carry a matching fresh literal — conservative but the
            // alternative is invalid Wasm at return-side unwrap time
            // when a non-literal break value flows past the
            // multi-value signature.
            //
            // TODO(optimizer): track break labels so we can distinguish
            // breaks that target *this* block (which must match the
            // expected shape) from breaks that target an enclosing
            // loop / labelled block (which leave the function via a
            // different path and don't constrain the return ABI). The
            // current over-requirement rejects multi-value for any
            // `return label: { … break label: f(x) … }` helper whose
            // breaks carry computed values.
            all_returns_match_shape(b, expected)
                && block_tail_returns_match(b, expected)
                && all_break_values_match_shape(b, expected)
        }
        NirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            all_returns_match_shape(then_branch, expected)
                && block_tail_returns_match(then_branch, expected)
                && else_branch.as_ref().is_none_or(|b| {
                    all_returns_match_shape(b, expected) && block_tail_returns_match(b, expected)
                })
        }
        NirExprKind::Match { arms, .. } => arms
            .iter()
            .all(|arm| expr_returns_match(&arm.body, expected)),
        NirExprKind::Switch { arms, default, .. } => {
            arms.iter().all(|arm| {
                all_returns_match_shape(arm, expected) && block_tail_returns_match(arm, expected)
            }) && all_returns_match_shape(default, expected)
                && block_tail_returns_match(default, expected)
        }
        // Anything else (Call result, FieldAccess, etc.) at the return
        // position would mean the function returns an opaque aggregate
        // value rather than constructing a fresh one — not a candidate
        // for the multi-value rewrite.
        _ => false,
    }
}

/// For a labelled block reached at return position, walk every nested
/// `Break { value: Some(v) }` (recursively, through inner blocks /
/// ifs / matches / switches / loops) and require `v` to match the
/// expected fresh-literal shape. Without label tracking we can't tell
/// which inner breaks target the outer labelled block versus an inner
/// loop / labelled block, so we conservatively require literal values
/// for every break. Functions whose inner breaks carry computed values
/// stay on the heap-struct ABI; that's the same trade-off as rejecting
/// `return match { … }` arms whose tail isn't a literal.
fn all_break_values_match_shape(block: &NirBlock, expected: &ExpectedShape) -> bool {
    block
        .stmts
        .iter()
        .all(|stmt| stmt_break_values_match(stmt, expected))
}

fn stmt_break_values_match(stmt: &NirStmt, expected: &ExpectedShape) -> bool {
    match &stmt.kind {
        NirStmtKind::Break { value: Some(v), .. } => {
            // The break value flows out as the labelled block's value.
            // It must itself be a fresh aggregate literal matching the
            // expected shape, AND its sub-expressions must not contain
            // further breaks with non-literal values.
            expr_returns_match(v, expected) && expr_break_values_match(v, expected)
        }
        NirStmtKind::Break { value: None, .. } | NirStmtKind::Continue => true,
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_break_values_match(condition, expected)
                && all_break_values_match_shape(then_block, expected)
                && else_block
                    .as_ref()
                    .is_none_or(|b| all_break_values_match_shape(b, expected))
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            all_break_values_match_shape(body, expected)
        }
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            expr_break_values_match(value, expected)
        }
        NirStmtKind::Expr(e) | NirStmtKind::Return { value: Some(e) } => {
            expr_break_values_match(e, expected)
        }
        NirStmtKind::Return { value: None } => true,
    }
}

fn expr_break_values_match(expr: &NirExpr, expected: &ExpectedShape) -> bool {
    struct Walker<'a> {
        expected: &'a ExpectedShape,
        ok: bool,
    }
    impl NirRefVisitor for Walker<'_> {
        fn visit_stmt(&mut self, stmt: &NirStmt) {
            if !self.ok {
                return;
            }
            if !stmt_break_values_match(stmt, self.expected) {
                self.ok = false;
                return;
            }
            self.walk_stmt(stmt);
        }
    }
    let mut w = Walker { expected, ok: true };
    w.visit_expr(expr);
    w.ok
}

/// For a block reached at return / arm-tail position, the trailing
/// expression (or break-value) must itself be a fresh aggregate literal
/// (or further-nested control flow that ends in one). Statements before
/// the tail are already validated by `all_returns_match_shape`.
fn block_tail_returns_match(block: &NirBlock, expected: &ExpectedShape) -> bool {
    let Some(last) = block.stmts.last() else {
        return false;
    };
    match &last.kind {
        NirStmtKind::Expr(e) => expr_returns_match(e, expected),
        NirStmtKind::Break { value: Some(v), .. } => expr_returns_match(v, expected),
        // An explicit Return at tail position is fine — `all_returns_match_shape`
        // already validates its value.
        NirStmtKind::Return { .. } => true,
        _ => false,
    }
}

/// Visitor that disqualifies candidate functions whose call sites use the
/// result in any way other than `let __tmp = Call(f); …
/// FieldAccess(LocalGet(__tmp), name) …`.
fn validate_uses_in_block(
    block: &NirBlock,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
) {
    // Track local indices that are bound to a candidate-call result in
    // the current scope. The scope ends at block exit.
    let mut tracked: IndexMap<u32, usize> = IndexMap::default();
    for stmt in &block.stmts {
        validate_stmt(stmt, candidate_names, candidates, invalid, &mut tracked);
    }
    // Block exit: any tracked locals that the visitor never confirmed
    // safe (via uses) are still considered safe — their absence of use
    // means elide_local will drop them.
}

fn validate_stmt(
    stmt: &NirStmt,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
    tracked: &mut IndexMap<u32, usize>,
) {
    match &stmt.kind {
        NirStmtKind::Let {
            local_index,
            value,
            is_mut,
            ..
        } => {
            // If the RHS is a direct call (free function or method) to a
            // candidate, start tracking this local. The call shape itself
            // is safe, but its arguments may still reference other tracked
            // locals — recurse into the args so any escapes there are
            // counted.
            if !*is_mut && let Some(candidate_idx) = candidate_call_idx(value, candidate_names) {
                tracked.insert(*local_index, candidate_idx);
                walk_call_args_for_uses(value, candidate_names, candidates, invalid, tracked);
                return;
            }
            // Any other RHS — walk it for both (a) nested candidate calls
            // in non-bind positions (the call's result flows somewhere we
            // can't rewrite) and (b) bare references to tracked locals.
            walk_expr_for_uses(value, candidate_names, candidates, invalid, tracked);
        }
        NirStmtKind::Expr(e) | NirStmtKind::Return { value: Some(e) } => {
            walk_expr_for_uses(e, candidate_names, candidates, invalid, tracked);
        }
        NirStmtKind::Return { value: None } | NirStmtKind::Continue => {}
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                walk_expr_for_uses(v, candidate_names, candidates, invalid, tracked);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            walk_expr_for_uses(condition, candidate_names, candidates, invalid, tracked);
            // Tracked locals propagate into nested blocks (lexical scope).
            // We clone the map so inner-block re-bindings don't leak out.
            let mut inner = tracked.clone();
            for stmt in &then_block.stmts {
                validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
            }
            if let Some(eb) = else_block {
                let mut inner = tracked.clone();
                for stmt in &eb.stmts {
                    validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
                }
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            let mut inner = tracked.clone();
            for stmt in &body.stmts {
                validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
            }
        }
        NirStmtKind::LetDestructure { value, .. } => {
            walk_expr_for_uses(value, candidate_names, candidates, invalid, tracked);
        }
    }
}

/// Walk an expression tree, validating any uses of tracked candidate-call
/// locals. A use disqualifies the candidate unless it's a
/// `FieldAccess(LocalGet(tracked), name)` access where `name` is one of
/// the candidate's known field names.
fn walk_expr_for_uses(
    expr: &NirExpr,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    match &expr.kind {
        // Safe access pattern: `FieldAccess(LocalGet(tracked), name)`
        // where `name` is one of the candidate's field names. Don't
        // recurse into the inner Local — that would mark it as a
        // bare reference.
        NirExprKind::FieldAccess {
            expr: source,
            field_name,
            ..
        } => {
            if let NirExprKind::Local { index, .. } = &source.kind
                && let Some(&candidate_idx) = tracked.get(index)
                && let Some(info) = candidates.get(&candidate_idx)
                && info.field_name_set.contains(field_name)
            {
                return;
            }
            walk_expr_for_uses(source, candidate_names, candidates, invalid, tracked);
        }
        // Bare local reference of a tracked candidate result → escape.
        NirExprKind::Local { index, .. } => {
            if let Some(&candidate_idx) = tracked.get(index) {
                invalid.insert(candidate_idx);
            }
        }
        // Direct candidate call in non-Let position → escape (the result
        // doesn't get bound and projected; it flows somewhere we can't
        // rewrite). Both `Call` and `MethodCall` can resolve to a
        // candidate after monomorphisation.
        NirExprKind::Call { func, args, .. } => {
            if let Some(&candidate_idx) =
                candidate_names.get(&(func.name.clone(), func.module_source.clone()))
            {
                invalid.insert(candidate_idx);
            }
            for arg in args {
                walk_expr_for_uses(&arg.expr, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::MethodCall {
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
            walk_expr_for_uses(receiver, candidate_names, candidates, invalid, tracked);
            for arg in args {
                walk_expr_for_uses(&arg.expr, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                walk_expr_for_uses(arg, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            walk_expr_for_uses(callee, candidate_names, candidates, invalid, tracked);
            for arg in args {
                walk_expr_for_uses(arg, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => {
            let mut inner = tracked.clone();
            for stmt in &b.stmts {
                validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
            }
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr_for_uses(condition, candidate_names, candidates, invalid, tracked);
            let mut inner = tracked.clone();
            for stmt in &then_branch.stmts {
                validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
            }
            if let Some(eb) = else_branch {
                let mut inner = tracked.clone();
                for stmt in &eb.stmts {
                    validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
                }
            }
        }
        NirExprKind::Match { expr: scrut, arms } => {
            walk_expr_for_uses(scrut, candidate_names, candidates, invalid, tracked);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    walk_expr_for_uses(g, candidate_names, candidates, invalid, tracked);
                }
                walk_expr_for_uses(&arm.body, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            walk_expr_for_uses(scrutinee, candidate_names, candidates, invalid, tracked);
            for arm in arms {
                let mut inner = tracked.clone();
                for stmt in &arm.stmts {
                    validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
                }
            }
            let mut inner = tracked.clone();
            for stmt in &default.stmts {
                validate_stmt(stmt, candidate_names, candidates, invalid, &mut inner);
            }
        }
        // For all other expressions (binary, unary, assign, struct/tuple
        // literals, etc.), walk every child with the same tracker so that
        // nested `LocalGet(p)` references are observed by the bare-Local
        // arm above and disqualify the candidate.
        _ => recurse_children(expr, candidate_names, candidates, invalid, tracked),
    }
}

/// Visit every direct child expression of `expr` with `walk_expr_for_uses`.
fn recurse_children(
    expr: &NirExpr,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    match &expr.kind {
        NirExprKind::Binary { left, right, .. } => {
            walk_expr_for_uses(left, candidate_names, candidates, invalid, tracked);
            walk_expr_for_uses(right, candidate_names, candidates, invalid, tracked);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. } => {
            walk_expr_for_uses(inner, candidate_names, candidates, invalid, tracked);
        }
        NirExprKind::Assign { target, value }
        | NirExprKind::Index {
            expr: target,
            index: value,
        } => {
            walk_expr_for_uses(target, candidate_names, candidates, invalid, tracked);
            walk_expr_for_uses(value, candidate_names, candidates, invalid, tracked);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                walk_expr_for_uses(&field.value, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                walk_expr_for_uses(elem, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                walk_expr_for_uses(p, candidate_names, candidates, invalid, tracked);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            walk_expr_for_uses(value, candidate_names, candidates, invalid, tracked);
        }
        // Leaf nodes — no nested expressions.
        NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => {}
        // Already handled in `walk_expr_for_uses` outer match.
        NirExprKind::FieldAccess { .. }
        | NirExprKind::Call { .. }
        | NirExprKind::MethodCall { .. }
        | NirExprKind::CmRawCall { .. }
        | NirExprKind::IndirectCall { .. }
        | NirExprKind::Block(_)
        | NirExprKind::LabeledBlock { .. }
        | NirExprKind::If { .. }
        | NirExprKind::Match { .. }
        | NirExprKind::Switch { .. } => {
            // Outer match should have routed these directly; if we reach
            // here something's wrong.
            unreachable!(
                "expression kind should have been handled by walk_expr_for_uses outer match: {expr:?}"
            )
        }
    }
}
