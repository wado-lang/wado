//! Read-only seed walker for the fold's value-copy decision.
//!
//! The fold (`lower::translate`) emits a `$value_copy$T(...)` wrap
//! directly at each wrap site, using the shared predicates exported
//! here ([`should_wrap`], [`is_fresh_value`], [`is_source_immutable`]).
//! [`collect_seed_types`] walks every function with the same
//! predicates to feed [`super::synthesize::synthesize_helpers`].

use super::funcset::FuncKeySet;
use super::ownership::OwnedCalls;
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirMatchArm, TirPattern, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Every `TypeId` the fold will wrap in `$value_copy$T(...)`, plus
/// element types of `array_clone::<T>(...)` calls that codegen
/// routes through the same helper.
///
/// Runs before the return-convention fixpoint, so it uses a conservative
/// oracle (only builtins are owned). That over-collects seed types — a helper
/// the precise fold never calls is dead-code-eliminated — but never misses one.
pub fn collect_seed_types(project: &FlatPackage) -> IndexSet<TypeId> {
    let type_table = project.type_table.borrow();
    let no_owned = FuncKeySet::default();
    let no_self_proj = FuncKeySet::default();
    let oracle = OwnedCalls::new(&no_owned, &no_self_proj);
    let mut walker = SeedWalker {
        type_table: &type_table,
        oracle: &oracle,
        out: IndexSet::default(),
        immutable_locals: IndexSet::default(),
    };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        walker.immutable_locals = func
            .locals
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_mut)
            .map(|(i, _)| u32::try_from(i).unwrap())
            .collect();
        if let Some(ref body) = func.body {
            walker.visit_block(body);
        }
    }
    walker.out
}

struct SeedWalker<'a> {
    type_table: &'a TypeTable,
    oracle: &'a OwnedCalls<'a>,
    out: IndexSet<TypeId>,
    immutable_locals: IndexSet<u32>,
}

impl SeedWalker<'_> {
    fn record_if_wrap(&mut self, expr: &TirExpr) {
        self.record_wrap_target(expr, expr.type_id);
    }

    /// Seed the helper for what the fold *writes*, which differs from the
    /// value's own type wherever the site writes through something.
    fn record_wrap_target(&mut self, value: &TirExpr, dest: TypeId) {
        if should_wrap_into(value, dest, self.type_table, self.oracle) {
            self.out.insert(dest);
        }
    }

    fn record_array_clone_element(&mut self, expr: &TirExpr) {
        if let Some(t) = super::array_clone_element_type_arg(expr)
            && super::needs_value_copy(t, self.type_table)
        {
            self.out.insert(t);
        }
    }
}

impl TirRefVisitor for SeedWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                value,
                type_id,
                skip_value_copy,
                ..
            } => {
                // Seed every candidate: whether an immutable source keeps its
                // copy depends on per-function move analysis this walk cannot
                // see. An unused helper is dead code `dce` removes.
                if !*skip_value_copy {
                    self.record_wrap_target(value, *type_id);
                }
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                // The copy lands on the temp `lower_let_pattern` mints, so
                // the helper it needs is that temp's.
                self.record_wrap_target(
                    value,
                    crate::lower::translate::pattern::pattern_temp_type(
                        pattern,
                        value.type_id,
                        self.type_table,
                    ),
                );
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        self.record_array_clone_element(expr);
        match &expr.kind {
            // Lowering binds a `Match` scrutinee to a temp that takes the copy,
            // in any position. Nothing here sees it, so seed its type.
            TirExprKind::Match {
                expr: scrutinee, ..
            } => self.record_if_wrap(scrutinee),
            TirExprKind::Call { args, .. } => {
                // Every by-value argument is copied — value semantics: passing
                // a value to a function deep-copies it. `should_wrap` already
                // excludes references (`&T` / `&mut T`), fresh values, and
                // non-copy types, so a `&mut` arg is not copied.
                for arg in args {
                    self.record_if_wrap(&arg.expr);
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args {
                    self.record_if_wrap(arg);
                }
            }
            TirExprKind::Assign { target, value } => {
                // A whole-local rebind (`x = v`) and a whole-value deref-assign
                // (`*ref = v`, lowered by `try_expand_deref_aggregate_assign`)
                // both replace the value, so the RHS needs a defensive copy.
                // Field / index writes mutate an existing slot in place and
                // don't.
                let replaces_whole_value = matches!(
                    &target.kind,
                    TirExprKind::Local { .. }
                        | TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            ..
                        }
                );
                if replaces_whole_value {
                    self.record_if_wrap(value);
                    // `try_expand_deref_aggregate_assign` copies the RHS as
                    // the referent's type, which a coercion can widen.
                    self.record_wrap_target(value, target.type_id);
                }
            }
            // An aggregate literal stores each element / field by value, so a
            // non-fresh aggregate element is deep-copied into the fresh literal.
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.record_if_wrap(&field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    self.record_if_wrap(element);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(payload),
                ..
            } => {
                self.record_if_wrap(payload);
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Shape predicate shared with the fold. Site-specific gating
/// (e.g. `skip_value_copy`, `is_source_immutable` for `Let`, the
/// `Local`-target check for `Assign`) is the caller's job.
pub fn should_wrap(expr: &TirExpr, type_table: &TypeTable, oracle: &OwnedCalls) -> bool {
    should_wrap_into(expr, expr.type_id, type_table, oracle)
}

/// [`should_wrap`] where the value lands in a destination of type `dest`: the
/// type test is the destination's, since `let { x, y } = &p` writes a `Point`
/// temp out of a `&Point`.
pub fn should_wrap_into(
    expr: &TirExpr,
    dest: TypeId,
    type_table: &TypeTable,
    oracle: &OwnedCalls,
) -> bool {
    super::needs_value_copy(dest, type_table)
        && !is_copy_value_call(expr)
        && !is_fresh_value(expr, oracle, type_table)
}

/// Avoid re-wrapping the `copy_value::<NestedT>(...)` markers
/// `synthesize_helpers` plants inside helper bodies.
fn is_copy_value_call(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::Call { func, .. }
            if func.module_source.is_core_builtin()
                && crate::tir::matches_builtin(&func.name, func.monomorph_info.as_ref(), "copy_value")
    )
}

/// A fresh (owned) expression does not alias existing data, so no defensive
/// copy is needed. `oracle` decides a call's return convention interprocedurally.
pub fn is_fresh_value(expr: &TirExpr, oracle: &OwnedCalls, type_table: &TypeTable) -> bool {
    is_owned_value(expr, &IndexSet::default(), oracle, type_table)
}

/// One step down the spine a returned value is wrapped in.
fn variant_payload(expr: &TirExpr) -> Option<&TirExpr> {
    match &expr.kind {
        TirExprKind::VariantConstruct {
            payload: Some(inner),
            ..
        } => Some(inner),
        _ => None,
    }
}

/// What a `return` actually delivers. `return` is no wrap site, so
/// `return place` hands a borrow out for the caller to materialize, and
/// `hands_out_payload` ([`super::hands_out_payload`]) makes
/// `return Some(place)` do the same.
///
/// Its three readers must agree, or the payload aliases undefended or copies
/// twice: [`translate`](crate::lower::translate) skips the copy at the
/// construction, and [`ownership`](super::ownership) judges the same payload for
/// the return convention and for the receiver-alias set.
pub fn returned_value(expr: &TirExpr, hands_out_payload: bool) -> &TirExpr {
    if !hands_out_payload {
        return expr;
    }
    let mut expr = expr;
    while let Some(inner) = variant_payload(expr) {
        expr = inner;
    }
    expr
}

/// Whether a returned value carries no storage at all — an empty variant case
/// (`None`) or a null. Nothing can be read or written through one, so it neither
/// confirms nor contradicts what the function's other returns name.
pub fn carries_no_storage(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::Null
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::VariantConstruct { payload: None, .. }
    )
}

/// The spans of the constructions [`returned_value`] descends through, so the
/// fold can recognize one it is converting.
pub fn returned_variant_spans(body: &TirBlock) -> IndexSet<crate::token::Span> {
    struct Walker {
        spans: IndexSet<crate::token::Span>,
    }
    impl TirRefVisitor for Walker {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if let TirStmtKind::Return { value: Some(v) } = &stmt.kind {
                let mut expr = v;
                while let Some(inner) = variant_payload(expr) {
                    self.spans.insert(expr.span);
                    expr = inner;
                }
            }
            self.walk_stmt(stmt);
        }
    }
    let mut walker = Walker {
        spans: IndexSet::default(),
    };
    walker.visit_block(body);
    walker.spans
}

/// Whether `expr` produces an *owned* value in the context of the owned locals
/// in `fresh_locals` — a value that aliases nothing the caller can still reach,
/// so consuming it into an owner is a move. A call is owned iff its callee
/// returns owned (`oracle`), the caller-side, single-phase replacement for the
/// old interprocedural escape recovery: the accessor `index_value(self: &List,
/// i) -> T { return self.repr[i] }` returns a borrowed projection of `&self`
/// (wado-lang/wado#1527), so it is *not* owned and its result is copied at a
/// materialization — but never at a mutable-place use, which is not a
/// materialization, so `arr[i].field.push(x)` keeps its element aliased.
pub(crate) fn is_owned_value(
    expr: &TirExpr,
    fresh_locals: &IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    match &expr.kind {
        // A string / bytes literal lowers to a fresh `StructLiteral` over a
        // packed array (`translate::seq_literal`), so each evaluation
        // materializes its own storage.
        TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::StructLiteral { .. }
        | TirExprKind::TupleLiteral { .. }
        | TirExprKind::ArrayLiteral { .. }
        | TirExprKind::TupleSpread { .. }
        | TirExprKind::TupleZip { .. }
        | TirExprKind::TupleLen { .. }
        | TirExprKind::TypePackExpansion { .. }
        | TirExprKind::Null => true,
        // A call is owned iff its callee returns an owned value. A core builtin
        // allocates or computes a fresh result — except `array_get_value`, the element
        // read that aliases its container — handled inside `oracle.is_owned`. A
        // raw CM call lifts a fresh value across the ABI boundary. A callee that
        // instead returns a projection of its receiver / first argument
        // (`build(&self) -> List { return *self }`) yields a fresh value exactly
        // when that receiver is itself fresh, so `[1, 2, 3]`'s builder — a fresh
        // block-local finalized by `.build()` — is not defensively copied.
        TirExprKind::Call { func, args, .. } => {
            oracle.is_owned(func)
                || (oracle.returns_self_projection(func)
                    && args
                        .first()
                        .is_some_and(|a| is_owned_value(&a.expr, fresh_locals, oracle, type_table)))
        }
        TirExprKind::CmRawCall { .. } => true,
        // Every callable value is a closure functor by lowering time, so an
        // indirect call is owned when every `__call` of this return type is
        // (`compute_indirect_owned_returns`). Without that verdict — inside the
        // fixpoint the verdict is derived from — it stays borrowed.
        TirExprKind::IndirectCall { .. } => oracle.indirect_is_owned(expr.type_id),
        TirExprKind::VariantConstruct { .. } | TirExprKind::EnumConstruct { .. } => true,
        TirExprKind::Local { index, .. } => fresh_locals.contains(index),
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => is_owned_value(inner, fresh_locals, oracle, type_table),
        TirExprKind::LabeledBlock { label, block, .. } => {
            block_breaks_are_fresh(label, block, fresh_locals, oracle, type_table)
        }
        // A block's value is its tail expression, and an `if`'s is the tail of
        // whichever branch runs — owned exactly when those tails are, the same
        // rule `Match` follows. `let resp = if let … { handler(…) } else { … }`
        // is the shape that needs it: without this the binding is classified
        // borrowed and every later field read is deep-copied.
        TirExprKind::Block(block) => block_tail_is_owned(block, fresh_locals, oracle, type_table),
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            let Some(else_branch) = else_branch else {
                return false;
            };
            block_tail_is_owned(then_branch, fresh_locals, oracle, type_table)
                && block_tail_is_owned(else_branch, fresh_locals, oracle, type_table)
        }
        TirExprKind::Match { expr: scrut, arms } => {
            match_result_is_fresh(scrut, arms, fresh_locals, oracle, type_table)
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        // A cast reinterprets a value without creating an alias, so it is owned
        // exactly when its operand is (`[] as List<i32>`, a fresh literal cast).
        | TirExprKind::Cast { expr: inner, .. } => {
            is_owned_value(inner, fresh_locals, oracle, type_table)
        }
        _ => false,
    }
}

/// A `match` yields an owned value when every value-producing arm yields one.
/// Divergent arms (`Never`-typed body: `=> return …`, `=> panic()`) contribute
/// no value and are skipped. When the scrutinee is owned, an arm's pattern
/// bindings destructure unaliased data, so they are owned too — this is what
/// makes `let x = f()?` (which desugars to `match f() { Ok(v) => v, Err(e) =>
/// return Err(e) }`) copy-free when `f()` returns an owned value.
fn match_result_is_fresh(
    scrut: &TirExpr,
    arms: &[TirMatchArm],
    fresh_locals: &IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    let scrut_fresh = is_owned_value(scrut, fresh_locals, oracle, type_table);
    let mut saw_value_arm = false;
    for arm in arms {
        if type_table.is_never(arm.body.type_id) {
            continue;
        }
        saw_value_arm = true;
        let mut arm_fresh = fresh_locals.clone();
        if scrut_fresh {
            collect_pattern_bindings(&arm.pattern, &mut arm_fresh);
        }
        if !is_owned_value(&arm.body, &arm_fresh, oracle, type_table) {
            return false;
        }
    }
    saw_value_arm
}

/// Collect every local a pattern binds, so a fresh scrutinee's destructured
/// parts can be treated as fresh in the arm body.
pub(crate) fn collect_pattern_bindings(pattern: &TirPattern, out: &mut IndexSet<u32>) {
    match pattern {
        TirPattern::Binding { local_index, .. } => {
            out.insert(*local_index);
        }
        TirPattern::Tuple(subs, _) | TirPattern::Variant { bindings: subs, .. } => {
            for sub in subs {
                collect_pattern_bindings(sub, out);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for field in fields {
                collect_pattern_bindings(&field.pattern, out);
            }
        }
        TirPattern::Or(alts) => {
            for alt in alts {
                collect_pattern_bindings(alt, out);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
    }
}

/// Whether the value a block delivers by falling off its end is owned.
///
/// The value is the block's tail expression statement; `let`s ahead of it seed
/// the fresh set exactly as they do inside a labeled block. A block that
/// diverges instead (`return` / `break` as the last statement) delivers no
/// value, so it cannot make the result borrowed — the caller's other branch, or
/// the enclosing `Match` arm rule, decides.
fn block_tail_is_owned(
    block: &TirBlock,
    parent_fresh: &IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    let mut fresh_locals = parent_fresh.clone();
    let Some((last, init)) = block.stmts.split_last() else {
        return false;
    };
    for stmt in init {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && is_owned_value(value, &fresh_locals, oracle, type_table)
        {
            fresh_locals.insert(*local_index);
        }
    }
    match &last.kind {
        TirStmtKind::Expr(expr) => is_owned_value(expr, &fresh_locals, oracle, type_table),
        TirStmtKind::Return { .. } | TirStmtKind::Break { .. } => true,
        _ => false,
    }
}

fn block_breaks_are_fresh(
    label: &str,
    block: &TirBlock,
    parent_fresh: &IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    let mut found = false;
    let mut fresh_locals = parent_fresh.clone();
    if scan_block_for_breaks(
        label,
        block,
        &mut found,
        &mut fresh_locals,
        oracle,
        type_table,
    ) {
        found
    } else {
        false
    }
}

fn scan_block_for_breaks(
    label: &str,
    block: &TirBlock,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    for stmt in &block.stmts {
        if !scan_stmt_for_breaks(label, stmt, found, fresh_locals, oracle, type_table) {
            return false;
        }
    }
    true
}

fn scan_stmt_for_breaks(
    label: &str,
    stmt: &TirStmt,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    match &stmt.kind {
        // A `skip_value_copy` binding takes its source's storage over, so it
        // owns the value whatever the source expression looks like.
        TirStmtKind::Let {
            local_index,
            value,
            skip_value_copy,
            ..
        } => {
            if *skip_value_copy || is_owned_value(value, fresh_locals, oracle, type_table) {
                fresh_locals.insert(*local_index);
            }
            true
        }
        TirStmtKind::Break {
            label: Some(l),
            value: Some(v),
        } if l == label => {
            *found = true;
            is_owned_value(v, fresh_locals, oracle, type_table)
        }
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            if !scan_block_for_breaks(label, then_block, found, fresh_locals, oracle, type_table) {
                return false;
            }
            if let Some(eb) = else_block
                && !scan_block_for_breaks(label, eb, found, fresh_locals, oracle, type_table)
            {
                return false;
            }
            true
        }
        TirStmtKind::Loop { body } => {
            scan_block_for_breaks(label, body, found, fresh_locals, oracle, type_table)
        }
        TirStmtKind::Expr(expr) => {
            scan_expr_for_breaks(label, expr, found, fresh_locals, oracle, type_table)
        }
        _ => true,
    }
}

fn scan_expr_for_breaks(
    label: &str,
    expr: &TirExpr,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    match &expr.kind {
        TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
            scan_block_for_breaks(label, block, found, fresh_locals, oracle, type_table)
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            if !scan_block_for_breaks(label, then_branch, found, fresh_locals, oracle, type_table) {
                return false;
            }
            if let Some(eb) = else_branch
                && !scan_block_for_breaks(label, eb, found, fresh_locals, oracle, type_table)
            {
                return false;
            }
            true
        }
        _ => true,
    }
}

/// An immutable destination binding can alias an immutable-rooted
/// source without a defensive copy.
///
/// Immutability of the *binding* is not immutability of the storage, so the
/// caller also checks the root against
/// [`super::last_use::compute_moved_roots`].
pub fn is_source_immutable(
    expr: &TirExpr,
    immutable_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    ref_targets: &super::last_use::RefTargets,
) -> bool {
    source_root(expr, type_table, ref_targets).is_some_and(|r| immutable_locals.contains(&r))
}

/// The local an immutable-source chain is rooted at, or `None` for a shape
/// [`is_source_immutable`] does not accept.
///
/// A projection through a reference continues at the place it borrows: the root
/// local's immutability binds the reference, not the storage behind it. An
/// unresolvable one names storage this body does not own, and answers nothing.
pub fn source_root(
    expr: &TirExpr,
    type_table: &TypeTable,
    ref_targets: &super::last_use::RefTargets,
) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            if matches!(
                type_table.get(inner.type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            ) {
                return ref_targets.referent_root(inner);
            }
            source_root(inner, type_table, ref_targets)
        }
        _ => None,
    }
}
