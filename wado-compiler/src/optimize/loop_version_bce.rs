//! Loop-versioned bounds-check elimination, for the checks
//! `condition_implication` cannot statically prove: the loop becomes
//! `if H < B { <clone, checks deleted> } else { <original> }`, sound by
//! per-iteration transitivity from the guard's `i <= H`. A fast arm that is
//! exactly `a[i] = CONST; i += 1` then collapses to one [`try_fill_idiom`].
//!
//! A check may index past the guard variable, `a[i + k]` with `k` a constant
//! or a loop-invariant local, and the checks may bound against different
//! lengths: each distinct `i + k < B` adds its own residual term, which also
//! keeps `B - k` and `i + k` clear of the wrap. An `i32` index may scale `i`
//! too, `a[i * s + k]` with `s` a positive constant or an invariant local: the
//! row walk of a matrix stored row-major. The guard may offset `i` as well,
//! `i + g < H`, as a loop taking two elements a step writes.
//!
//! Where the entry value reads as no constant, the check's floor joins the
//! residual too: `i >= FLOOR` at the version point, carried to every later
//! value by a step the residual proves non-negative and clear of the wrap.

mod reload;

use std::ops::ControlFlow;
use std::sync::Arc;

use cranelift_entity::EntityRef;

use crate::call_args::CallArgs;
use crate::compiler_item::SeqField;
use crate::nir::{FuncParts, FunctionRef, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, Body, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::parallel::Executor;
use crate::primitive::PrimitiveType;
use crate::tir::TypeId;
use crate::token::Span;
use crate::type_table::TypeTable;

use super::alias::{CallImmutability, builder_alias_sets, first_param_types};
use super::arena_query::{block_contains_loop, has_break_to};
use super::bounds;
use super::condition_implication::{
    Binds, BoundKey, Conjunct, FieldPath, InductionStep, VAR_OFFSET_MAX_DEPTH, break_guard_cmp,
    build_copy_bindings, capture_block_binding, check_conjuncts, eliminate_condition,
    induction_entry, induction_step, is_seq_length, modifies_any_root, negated_operand,
    panic_guard_check, parse_bound, parse_break_guard_head, parse_const_i64, parse_var_offset,
    peel_capture_block, resolve, resolve_panic_ids, stmt_modifies,
};
use super::const_branch_prune::BranchPruneRule;
use super::dce::{DescriptorCache, callee_descriptor};
use super::elide_local::ElideRule;
use super::gate::FunctionGate;
use super::heap_effect::{HeapEffectsCache, LazyHeapFrame};
use crate::module_source::ModuleSource;
use crate::nir::FuncId;
use crate::nir_arena;
use crate::optimize::arena_query::{
    binary_parts, expr_node_may_trap_typed, is_pure_operand, local_written_by,
    mentions_local_except, operand_local,
};
use crate::optimize::mod_ref::{CallFacts, summarize};

/// A versionable loop: guard `var CMP bound` with in-body panic checks
/// `var + shift < B`, `B` and the shift's local and stride loop-invariant
/// locals of the guard's type (or constants).
struct Plan {
    /// Block holding the `Loop` statement.
    parent: BlockId,
    /// The `Loop` statement to version.
    loop_stmt: StmtId,
    /// The loop body block.
    loop_body: BlockId,
    /// Index of the guard statement within the loop body.
    guard_idx: usize,
    /// Guard variable `i`.
    var: u32,
    /// Guard bound `H`: a local, or a constant.
    bound: BoundKey,
    /// `true` for `i <= H` (residual `H < B`), `false` for `i < H`
    /// (residual `H <= B`).
    guard_le: bool,
    /// The constant `g` a guard `i + g < H` adds to `i`, `0` for none.
    guard_offset: i64,
    /// Whether `var`'s type is unsigned, which the residual compares as.
    unsigned: bool,
    /// The distinct limits the deleted checks put on `var`, one residual term
    /// apiece.
    limits: Vec<Limit>,
    /// The floor the checks demand, when it is the residual's to establish.
    floor: Option<RuntimeFloor>,
    /// Where a [`BoundKey::Field`] limit's bound is reloaded.
    reload: Option<Reload>,
}

/// A bound the loop reloads: see [`reload`]. The bound's own local is the
/// [`BoundKey::Field`] limit's root.
struct Reload {
    /// Each statement of the original loop after which the fast loop re-tests
    /// the bound, with its block.
    points: Vec<(BlockId, StmtId)>,
}

/// What a check makes of the guard variable before comparing: `var * stride +
/// local + konst`. `konst` is non-negative, and the residual's terms on
/// `local` and the stride make them so too (see [`limit_terms`]), which keeps
/// `var + shift` from lying below `var`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Shift {
    stride: Option<BoundKey>,
    local: Option<u32>,
    konst: i64,
}

impl Shift {
    const NONE: Shift = Shift {
        stride: None,
        local: None,
        konst: 0,
    };

    fn plus(self, c: i64) -> Option<Shift> {
        let konst = self.konst.checked_add(c)?;
        (konst >= 0).then_some(Shift { konst, ..self })
    }
}

/// The limit one check puts on the guard variable: `var + shift < bound`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Limit {
    shift: Shift,
    /// A local, or a constant.
    bound: BoundKey,
}

/// A floor the residual tests at the version point, with the step that carries
/// it: non-negative, and leaving `H` a step below `type_max` so none wraps.
struct RuntimeFloor {
    demanded: i64,
    step: InductionStep,
    type_max: u64,
}

/// The two blocks [`apply_version`] produces for a versioned loop: the version
/// `if`'s then-block (holding exactly the fast `Loop` stmt) and the fast loop's
/// body. The guard fields the fill idiom needs live on the paired [`Plan`].
struct FastArm {
    version_if: StmtId,
    then_block: BlockId,
    fast_body: BlockId,
}

/// Version eligible loops in every function. Returns whether anything changed.
pub(super) fn version_loops(
    project: &mut NirPackage,
    cache: &mut DescriptorCache,
    exec: &Arc<Executor>,
) -> bool {
    let fill_id = project.intern_extern(&FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "array_fill".to_string(),
        monomorph_info: None,
        method_info: None,
    });
    let descriptors = cache.descriptors(project);
    let panic_ids = resolve_panic_ids(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    // Only a body with a loop is versioned, so a program with none pays nothing.
    if !project
        .functions
        .iter()
        .any(|f| f.borrow().body.as_ref().is_some_and(body_contains_loop))
    {
        return false;
    }
    let (summaries, builtins) = summarize(project, exec);
    let type_table = project.type_table.borrow();
    let first_param_types = first_param_types(project);
    let call_immutability = CallImmutability::new(project, &type_table, exec);
    let gate = FunctionGate::new(project, exec);
    let mut heap = HeapEffectsCache::default();
    let effects = heap.effects(project, &type_table, &gate);
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let Some(body_ref) = func.body.as_ref() else {
            continue;
        };
        if !body_contains_loop(body_ref) {
            continue;
        }
        let FuncParts {
            body,
            locals,
            params,
            address_taken_locals,
            ..
        } = func.parts();
        let frame = LazyHeapFrame::new(&effects, params.iter().map(|p| p.local_index).collect());
        let body = body.expect("checked above");
        let alias = builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let mut engine = Engine::new(body, &mut buffers, locals, params);
        engine.set_alias_sets(alias);
        engine.set_value_graph_type_table(&type_table);
        engine.set_panic_callee_ids(&panic_ids);
        engine.set_pure_builtin_callees(&pure_builtin_callees);

        let binds = build_copy_bindings(&engine);
        let mut loops: Vec<(BlockId, StmtId, BlockId)> = Vec::new();
        collect_loops(engine.body, engine.body.root(), &mut loops);
        let plans: Vec<Plan> = loops
            .into_iter()
            .filter_map(|(parent, loop_stmt, loop_body)| {
                analyze_loop(
                    &engine,
                    &binds,
                    &type_table,
                    &frame,
                    parent,
                    loop_stmt,
                    loop_body,
                )
            })
            .collect();
        if plans.is_empty() {
            continue;
        }

        let fast_arms: Vec<FastArm> = plans
            .iter()
            .map(|plan| apply_version(&mut engine, &binds, &type_table, plan))
            .collect();

        // The splice invalidates licm's loop-entry snapshots (cloned loops
        // have no entry; the original moved under a new guard). An absent
        // entry is sound; clear like `inline` does for non-preserving splices.
        if let Some(vg) = engine.body.value_graph.as_mut() {
            vg.loop_entry_values.clear();
        }

        // Sweep the deleted checks' residue: the `if false { panic }` shells
        // and any now-write-only condition temps. The proofs are taken after
        // the splice, so the fast arm's cloned calls carry theirs.
        let proofs = bounds::analyze(engine.body, &type_table, |fid| builtins[fid.index()]).proofs;
        let calls = CallFacts {
            effects: &summaries.effects,
            proofs: &proofs,
        };
        let elide_rule = ElideRule::new(calls);
        let prune_rule = BranchPruneRule;
        let rules: [&dyn Rule; 2] = [&prune_rule, &elide_rule];
        engine.run(&rules);

        // Fill idiom over the cleaned fast arms; sweep again if it fired.
        let mut filled = false;
        for (plan, arm) in plans.iter().zip(&fast_arms) {
            if plan.reload.is_none() {
                filled |= try_fill_idiom(&mut engine, &binds, descriptors, fill_id, plan, arm);
            }
        }
        if filled {
            engine.run(&rules);
        }
        changed = true;
    }
    changed
}

/// Whether any statement in `body` is a `Loop` (cheap pre-filter).
fn body_contains_loop(body: &nir_arena::Body) -> bool {
    body.stmts
        .iter()
        .any(|(_, s)| matches!(s.kind, StmtKind::Loop { .. }))
}

/// Collect every `Loop` statement reachable from `block` with its parent
/// block, recursing through nested statement and expression blocks.
fn collect_loops(
    body: &nir_arena::Body,
    block: BlockId,
    out: &mut Vec<(BlockId, StmtId, BlockId)>,
) {
    for i in 0..body.blocks[block].stmts.len() {
        let s = body.blocks[block].stmts[i];
        if let StmtKind::Loop { body: lb } = &body.stmts[s].kind {
            out.push((block, s, *lb));
        }
        // Recurse into every nested block (loop bodies, if arms, expression
        // blocks) via the generic child walk.
        body.walk_nodes_under::<()>(NodeRef::Stmt(s), |n| {
            if let NodeRef::Block(b) = n {
                collect_loops(body, b, out);
                return ControlFlow::Continue(false);
            }
            ControlFlow::Continue(true)
        });
    }
}

/// Parse the loop-head guard `if !(var CMP bound) { break }`, or the
/// comparison turned round as in `if var >= bound { break }` (skipping
/// leading `let`s), through both the skeleton and promoted-operand forms, or
/// `var + g < bound` with `g` a positive constant. Returns `(guard_idx, var,
/// bound, guard_le, g)`, the bound a local or a constant.
fn parse_loop_guard(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
) -> Option<(usize, u32, BoundKey, bool, i64)> {
    let (guard_idx, cond) = parse_break_guard_head(engine, loop_body)?;
    let (var, off, h, op) = break_guard_cmp(engine, binds, cond)?;
    let h = match h {
        BoundKey::Field(..) => guard_bound_local(engine, binds, loop_body, cond)?,
        BoundKey::Local(_) | BoundKey::Const(_) => h,
    };
    if off < 0 || (off > 0 && op != NirBinaryOp::Lt) {
        return None;
    }
    let guard_le = match op {
        NirBinaryOp::LtEq => true,
        NirBinaryOp::Lt => false,
        NirBinaryOp::Add
        | NirBinaryOp::Sub
        | NirBinaryOp::Mul
        | NirBinaryOp::Div
        | NirBinaryOp::Mod
        | NirBinaryOp::Eq
        | NirBinaryOp::NotEq
        | NirBinaryOp::Gt
        | NirBinaryOp::GtEq
        | NirBinaryOp::And
        | NirBinaryOp::Or
        | NirBinaryOp::BitAnd
        | NirBinaryOp::BitOr
        | NirBinaryOp::BitXor
        | NirBinaryOp::Shl
        | NirBinaryOp::Shr
        | NirBinaryOp::NotIdentical => return None,
    };
    Some((guard_idx, var, h, guard_le, off))
}

/// The local a guard compares against, as written. [`break_guard_cmp`] reads a
/// `let` of a field (`let per_row = b.per_row`) as the field, which the
/// residual cannot re-read, where the guard reads the local.
fn guard_bound_local(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
    cond: Operand,
) -> Option<BoundKey> {
    let cmp = negated_operand(engine, binds, cond).unwrap_or(cond);
    let (_, _, right) = binary_parts(engine.body, resolve(engine, binds, cmp))?;
    match parse_check_bound(engine, binds, loop_body, right)? {
        (bound @ BoundKey::Local(_), 0) => Some(bound),
        _ => None,
    }
}

/// A versionable in-body check over the guard variable.
struct Check {
    holder: NodeRef,
    cond: Operand,
    limit: Limit,
    /// The strongest constant floor the check demands of `var`, or `i64::MIN`
    /// when it demands none. A floor under `var + shift` is demanded of `var`
    /// itself, which the shift's non-negativity makes sufficient.
    floor: i64,
}

/// Collect every versionable check nested anywhere in `node`.
fn collect_checks_in_node(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
    node: NodeRef,
    var: u32,
    out: &mut Vec<Check>,
) {
    engine.body.for_each_node_under(node, |n| {
        if let Some(cond) = panic_guard_check(engine, n)
            && let Some(check) = parse_versionable_check(engine, binds, loop_body, n, cond, var)
        {
            out.push(check);
        }
    });
}

/// Read one panic guard as a versionable check: exactly one upper-bound
/// conjunct `var + shift < B` over the guard variable, and any number of
/// constant floors under that same index — the shape `assert 0 <= i < n`
/// lowers to — or under `var + k` with `k` a constant up to the shift's own,
/// which lies between `var` and the index and so wraps no more than it does.
/// Any other conjunct refuses the check, so versioning never deletes a
/// condition it has only half proved.
fn parse_versionable_check(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
    holder: NodeRef,
    cond: Operand,
    var: u32,
) -> Option<Check> {
    let mut limit = None;
    let mut floors = Vec::new();
    for part in check_conjuncts(engine, binds, cond)? {
        match part {
            Conjunct::Lt(left, right) => {
                if limit.is_some() {
                    return None;
                }
                limit = Some(parse_limit(engine, binds, loop_body, left, right, var)?);
            }
            Conjunct::AtLeast(fvar, foff, f) => {
                floors.push((local_shift(engine, binds, fvar, var)?.plus(foff)?, f));
            }
        }
    }
    let limit = limit?;
    let mut floor = i64::MIN;
    for (shift, f) in floors {
        let below_index =
            shift.stride.is_none() && shift.local.is_none() && shift.konst <= limit.shift.konst;
        if shift != limit.shift && !below_index {
            return None;
        }
        floor = floor.max(f);
    }
    // A strided check demands `var >= 0` too: with the residual's
    // `stride >= 1`, `var * stride` then lies at or above `var`, clear of the
    // wrap.
    if limit.shift.stride.is_some() {
        floor = floor.max(0);
    }
    Some(Check {
        holder,
        cond,
        limit,
        floor,
    })
}

/// Read `op` (through copy temps) as `var + shift`: a `+` tree, in any
/// association, of `var` or `var * stride` once, at most one other local, and
/// constants.
fn parse_shift(engine: &Engine, binds: &Binds, op: Operand, var: u32) -> Option<Shift> {
    let mut terms = Terms::default();
    terms.add_operand(engine, binds, op, var, 0)?;
    terms.shift()
}

/// [`parse_shift`] of the local `l`.
fn local_shift(engine: &Engine, binds: &Binds, l: u32, var: u32) -> Option<Shift> {
    let mut terms = Terms::default();
    terms.add_local(engine, binds, l, var, 0)?;
    terms.shift()
}

/// The terms of a `+` tree [`parse_shift`] has read so far.
#[derive(Default)]
struct Terms {
    var: bool,
    /// What `var` is multiplied by, when it is.
    stride: Option<BoundKey>,
    local: Option<u32>,
    konst: i64,
}

impl Terms {
    const VAR: Terms = Terms {
        var: true,
        stride: None,
        local: None,
        konst: 0,
    };

    fn shift(self) -> Option<Shift> {
        let local = Shift {
            stride: self.stride,
            local: self.local,
            konst: 0,
        };
        self.var.then_some(local)?.plus(self.konst)
    }

    /// `var * stride`, the stride a positive constant or a local other than
    /// `var` (the caller proves it loop-invariant).
    fn scaled(
        engine: &Engine,
        binds: &Binds,
        var_side: Operand,
        stride_side: Operand,
        var: u32,
        depth: u32,
    ) -> Option<Terms> {
        let mut inner = Terms::default();
        inner.add_operand(engine, binds, var_side, var, depth + 1)?;
        let exactly_var =
            inner.var && inner.stride.is_none() && inner.local.is_none() && inner.konst == 0;
        if !exactly_var {
            return None;
        }
        let stride = match parse_const_i64(engine, binds, stride_side) {
            Some(c) if c >= 1 => BoundKey::Const(c),
            Some(_) => return None,
            None => match operand_local(engine.body, stride_side)? {
                l if l == var => return None,
                l => BoundKey::Local(l),
            },
        };
        Some(Terms {
            stride: Some(stride),
            ..Terms::VAR
        })
    }

    fn add_operand(
        &mut self,
        engine: &Engine,
        binds: &Binds,
        op: Operand,
        var: u32,
        depth: u32,
    ) -> Option<()> {
        if depth >= VAR_OFFSET_MAX_DEPTH {
            return None;
        }
        if let Some(c) = parse_const_i64(engine, binds, op) {
            self.konst = self.konst.checked_add(c)?;
            return Some(());
        }
        if let Some(l) = operand_local(engine.body, op) {
            return self.add_local(engine, binds, l, var, depth);
        }
        match binary_parts(engine.body, resolve(engine, binds, op))? {
            (left, NirBinaryOp::Add, right) => {
                self.add_operand(engine, binds, left, var, depth + 1)?;
                self.add_operand(engine, binds, right, var, depth + 1)
            }
            (left, NirBinaryOp::Mul, right) => self.merge(
                Terms::scaled(engine, binds, left, right, var, depth)
                    .or_else(|| Terms::scaled(engine, binds, right, left, var, depth))?,
            ),
            _ => None,
        }
    }

    /// A copy temp whose value holds `var` is read through. Any other local is
    /// a term of its own, read where the residual is evaluated, so the caller
    /// checks that it holds still across the loop.
    fn add_local(
        &mut self,
        engine: &Engine,
        binds: &Binds,
        l: u32,
        var: u32,
        depth: u32,
    ) -> Option<()> {
        if l == var {
            return self.merge(Terms::VAR);
        }
        if let Some(&bound) = binds.get(&l) {
            let mut inner = Terms::default();
            if inner
                .add_operand(engine, binds, bound, var, depth + 1)
                .is_some()
                && inner.var
            {
                return self.merge(inner);
            }
        }
        self.merge(Terms {
            local: Some(l),
            ..Terms::default()
        })
    }

    fn merge(&mut self, other: Terms) -> Option<()> {
        if (self.var && other.var) || (self.local.is_some() && other.local.is_some()) {
            return None;
        }
        self.var |= other.var;
        self.stride = self.stride.or(other.stride);
        self.local = self.local.or(other.local);
        self.konst = self.konst.checked_add(other.konst)?;
        Some(())
    }
}

/// The most residual terms one loop is versioned on, each evaluated at every
/// entry to the loop.
const MAX_LIMITS: usize = 8;

/// Parse a check bound as a constant (the length of a list whose size is
/// known), or else as a direct local read, without resolving through copy
/// bindings. A local bound must stay the loop-invariant local itself
/// (typically a licm-hoisted `arr.used` preheader local): the residual
/// re-reads it before the loop, and a `binds`-resolved form (the underlying
/// field read) would compare a different representation. A local present as
/// a `let` initializer target is single-assignment by [`build_copy_bindings`]'
/// definition; in-loop re-bindings are rejected by `subtree_redefines`.
///
/// A local the loop binds anew each iteration to the length of a list or
/// string local is read through, as that length: a [`reload`] bound.
///
/// A local less a non-negative constant, `B - c`, reads as `B` with the `c`
/// it takes away: the form `condition_implication` leaves where it merged
/// `i < B` and `i + c < B` into one check.
fn parse_check_bound(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
    op: Operand,
) -> Option<(BoundKey, i64)> {
    let op = peel_capture_block(engine, binds, op);
    if let Some(c) = parse_const_i64(engine, binds, op) {
        return Some((BoundKey::Const(c), 0));
    }
    if let Some(l) = operand_local(engine.body, op) {
        if subtree_redefines(engine, loop_body, &[l])
            && let Some(&read) = binds.get(&l)
            && is_seq_length(engine, binds, read)
            && let Some(field @ BoundKey::Field(..)) = parse_bound(engine, binds, read)
        {
            return Some((field, 0));
        }
        return Some((BoundKey::Local(l), 0));
    }
    let (minuend, NirBinaryOp::Sub, less) = binary_parts(engine.body, op)? else {
        return None;
    };
    let c = parse_const_i64(engine, binds, less).filter(|&c| c >= 0)?;
    Some((BoundKey::Local(operand_local(engine.body, minuend)?), c))
}

/// Read the conjunct `left < right` as a limit on `var`. A constant `right`
/// takes away from its bound moves onto the shift: `i < B - c` is
/// `i + c < B` once the residual puts `B` at or over the shift's `konst`
/// ([`limit_terms`]), so the subtraction does not wrap.
fn parse_limit(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
    left: Operand,
    right: Operand,
    var: u32,
) -> Option<Limit> {
    let shift = parse_shift(engine, binds, left, var)?;
    let (bound, less) = parse_check_bound(engine, binds, loop_body, right)?;
    Some(Limit {
        shift: shift.plus(less)?,
        bound,
    })
}

/// Analyze one loop for versioning. See the module docs for the conditions.
fn analyze_loop(
    engine: &Engine,
    binds: &Binds,
    type_table: &TypeTable,
    heap: &LazyHeapFrame,
    parent: BlockId,
    loop_stmt: StmtId,
    loop_body: BlockId,
) -> Option<Plan> {
    // Leaf loops only: versioning a loop that contains another loop would
    // clone the inner loop as well, compounding code size.
    if block_contains_loop(engine.body, loop_body) {
        return None;
    }
    let (guard_idx, var, h, guard_le, offset) = parse_loop_guard(engine, binds, loop_body)?;
    // Scan statements after the guard while `var` / `H` are unmodified —
    // within that window the guard fact `var <= H` still holds.
    let mut checks: Vec<Check> = Vec::new();
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    for &s in stmts.iter().skip(guard_idx + 1) {
        if stmt_modifies(engine, s, var, h) {
            break;
        }
        collect_checks_in_node(engine, binds, loop_body, NodeRef::Stmt(s), var, &mut checks);
    }
    // A floor is answered from the fast arm's own facts, so it must hold for
    // every collected check before any of them is deleted.
    let demanded = checks.iter().map(|c| c.floor).max()?;
    let mut limits: Vec<Limit> = Vec::new();
    for check in &checks {
        // An unshifted check on the bound of an unshifted guard is statically
        // decidable, not our case.
        let decidable = offset == 0
            && check.limit
                == Limit {
                    shift: Shift::NONE,
                    bound: h,
                };
        if !decidable && !limits.contains(&check.limit) {
            limits.push(check.limit);
        }
    }
    if limits.is_empty() || limits.len() > MAX_LIMITS {
        return None;
    }
    // One consistent signedness for guard, check, and residual comparisons.
    let locals = engine.locals();
    let ty = locals.get(var as usize)?.type_id;
    if !type_table.is_integer(ty) {
        return None;
    }
    let key = type_table.type_key(ty);
    // A residual evaluated in `i64`, where no `i32` product or sum wraps, is
    // what reads a strided check or an offset guard. An offset guard also
    // leans on `var >= 0`, so `var + g` stays above `var`.
    let is_i32 = key == type_table.type_key(TypeTable::I32);
    if offset != 0 && (!is_i32 || demanded < 0 || !spells_same(type_table, ty, offset)?) {
        return None;
    }
    let of_ty = |l: u32| {
        locals
            .get(l as usize)
            .is_some_and(|d| type_table.type_key(d.type_id) == key)
    };
    // Every local the residual reads, which must hold still across the loop.
    let mut invariant: Vec<u32> = Vec::new();
    // The one local a reloaded bound's length is read off.
    let mut reload: Option<u32> = None;
    let len_path = FieldPath::of(SeqField::Len.index());
    for limit in &limits {
        if let BoundKey::Field(root, path) = limit.bound {
            if path != len_path || reload.is_some_and(|r| r != root) {
                return None;
            }
            reload = Some(root);
        }
    }
    // The local a bound reads, `Some(None)` for a constant or a reloaded
    // length, `None` for neither.
    let bound_local = |b: BoundKey| match b {
        BoundKey::Local(l) if of_ty(l) => Some(Some(l)),
        BoundKey::Const(c) if spells_same(type_table, ty, c)? => Some(None),
        BoundKey::Field(..) if type_table.type_key(TypeTable::I32) == key => Some(None),
        BoundKey::Local(_) | BoundKey::Const(_) | BoundKey::Field(..) => None,
    };
    invariant.extend(bound_local(h)?);
    for limit in &limits {
        invariant.extend(bound_local(limit.bound)?);
        if !spells_same(type_table, ty, limit.shift.konst)? {
            return None;
        }
        if let Some(s) = limit.shift.local {
            if !of_ty(s) {
                return None;
            }
            invariant.push(s);
        }
        if let Some(stride) = limit.shift.stride {
            if !is_i32 {
                return None;
            }
            invariant.extend(bound_local(stride)?);
        }
    }
    let floor = match demanded {
        i64::MIN => None,
        _ => match induction_entry(engine, binds, parent, loop_stmt, loop_body, var) {
            // The first guard's `var + g` must not wrap, as later ones cannot.
            Some(entry) if entry >= demanded && i32::try_from(entry + offset).is_ok() => None,
            // A constant entry below the floor is a loop no fast arm can take.
            Some(_) => return None,
            None => Some(runtime_floor(
                engine, binds, type_table, loop_body, var, ty, demanded,
            )?),
        },
    };
    let step_local = floor.as_ref().and_then(|f| match f.step {
        InductionStep::Local(s) => Some(s),
        InductionStep::Const(..) => None,
    });
    if step_local.is_some_and(|s| !of_ty(s)) {
        return None;
    }
    // `H`, every `B` and shift, and the step must be loop-invariant: the
    // residual is evaluated once at the version point. `var` re-bindings would
    // break the guard-to-check window scan the same way.
    invariant.extend(step_local);
    if stmts
        .iter()
        .any(|&s| modifies_any_root(engine, NodeRef::Stmt(s), &invariant))
    {
        return None;
    }
    invariant.push(var);
    if subtree_redefines(engine, loop_body, &invariant) {
        return None;
    }
    let reload = match reload {
        Some(root) => {
            // `$lim` drops below every value to end the fast loop, which `<=`
            // would not compare past.
            if guard_le {
                return None;
            }
            let holders: Vec<NodeRef> = checks.iter().map(|c| c.holder).collect();
            let points = reload::points(
                engine, binds, heap, type_table, root, loop_body, guard_idx, &holders,
            )?;
            Some(Reload { points })
        }
        None => None,
    };
    Some(Plan {
        parent,
        loop_stmt,
        loop_body,
        guard_idx,
        var,
        bound: h,
        guard_le,
        guard_offset: offset,
        unsigned: type_table.is_unsigned_int(ty),
        limits,
        floor,
        reload,
    })
}

/// Whether the residual's raw constant bits of `ty` spell `c`: only a value in
/// `0..=MAX` reads the same in every integer width. `None` for a type with no
/// integer maximum.
fn spells_same(type_table: &TypeTable, ty: TypeId, c: i64) -> Option<bool> {
    let max = type_table.primitive_head(ty)?.int_max()?;
    Some(u128::try_from(c).is_ok_and(|c| c <= max))
}

/// The floor terms for a loop whose entry reads as no constant: a step that
/// never lowers `var`, and the type maximum the no-wrap term compares against.
fn runtime_floor(
    engine: &Engine,
    binds: &Binds,
    type_table: &TypeTable,
    loop_body: BlockId,
    var: u32,
    ty: TypeId,
    demanded: i64,
) -> Option<RuntimeFloor> {
    if !spells_same(type_table, ty, demanded)? {
        return None;
    }
    let step = induction_step(engine, binds, loop_body, var)?;
    if let InductionStep::Const(k) = step
        && k < 0
    {
        return None;
    }
    let max = type_table.primitive_head(ty)?.int_max()?;
    let type_max = u64::try_from(max).expect("a scalar integer maximum fits u64");
    Some(RuntimeFloor {
        demanded,
        step,
        type_max,
    })
}

/// Build a `Local` read expression for `index`.
fn local_read(engine: &mut Engine, index: u32, span: Span) -> Operand {
    let ty = engine.locals()[index as usize].type_id;
    let name = engine.local_name(index);
    let e = engine.alloc_expr(ExprKind::Local { index, name }, ty, span);
    Operand::Expr(e)
}

/// Read a local or constant bound as an operand of `var`'s type.
fn bound_read(engine: &mut Engine, plan: &Plan, bound: BoundKey, span: Span) -> Operand {
    match bound {
        BoundKey::Local(b) => local_read(engine, b, span),
        BoundKey::Const(c) => {
            let ty = engine.locals()[plan.var as usize].type_id;
            engine.const_operand(ValueKind::Int(c as u64, ty), ty)
        }
        BoundKey::Field(root, _) => {
            let base = local_read(engine, root, span);
            let e = engine.alloc_expr(
                ExprKind::FieldAccess {
                    expr: base,
                    field_index: SeqField::Len.index(),
                    field_name: SeqField::Len.field_name().to_string(),
                },
                TypeTable::I32,
                span,
            );
            Operand::Expr(e)
        }
    }
}

fn alloc_pred(
    engine: &mut Engine,
    left: Operand,
    op: NirBinaryOp,
    right: Operand,
    span: Span,
) -> Operand {
    alloc_binary(engine, left, op, right, TypeTable::BOOL, span)
}

fn alloc_binary(
    engine: &mut Engine,
    left: Operand,
    op: NirBinaryOp,
    right: Operand,
    ty: TypeId,
    span: Span,
) -> Operand {
    let e = engine.alloc_expr(ExprKind::Binary { left, op, right }, ty, span);
    Operand::Expr(e)
}

/// What the fast loop's own guard bound is minted under, where the bound is
/// reloaded.
const LIMIT: &str = "bce_limit";

/// Apply one versioning plan: clone the loop, delete the implied checks in
/// the clone, and replace the loop with
/// `if <residual> { fast } else { original }`. Where the bound is reloaded, it
/// is `let $lim = H; if <residual> { fast } original`: the fast loop runs while
/// `var < $lim`, a failed re-test drops `$lim` below every value, and the
/// original loop takes over where the fast one stopped, or exits at once.
fn apply_version(
    engine: &mut Engine,
    binds: &Binds,
    type_table: &TypeTable,
    plan: &Plan,
) -> FastArm {
    let span = engine.body.stmts[plan.loop_stmt].span;
    let var_ty = engine.locals()[plan.var as usize].type_id;
    // The re-tests go into the original first, so the clone carries them at
    // the statements the analysis found, and come out of it after.
    let reload = plan.reload.as_ref().map(|reload| {
        let lim = engine.alloc_minted_local(LIMIT, var_ty, true);
        let floor = if plan.unsigned {
            0
        } else {
            let bits = type_table
                .primitive_head(var_ty)
                .and_then(PrimitiveType::int_max)
                .expect("the guard variable is an integer");
            !u64::try_from(bits).expect("a scalar integer maximum fits u64")
        };
        let mut retests = Vec::with_capacity(reload.points.len());
        for &(block, after) in &reload.points {
            let retest = alloc_retest(engine, plan, lim, floor, span);
            let mut stmts = engine.body.blocks[block].stmts.clone();
            let pos = stmts
                .iter()
                .position(|&s| s == after)
                .expect("a re-test point is a statement of its block");
            stmts.insert(pos + 1, retest);
            engine.set_block_stmts(block, stmts);
            retests.push((block, retest));
        }
        (lim, retests)
    });
    let fast_body = engine.clone_block(plan.loop_body);
    if let Some((lim, retests)) = &reload {
        for &(block, retest) in retests {
            let mut stmts = engine.body.blocks[block].stmts.clone();
            stmts.retain(|&s| s != retest);
            engine.set_block_stmts(block, stmts);
        }
        // The original guard with `$lim` for its bound: `var + g >= $lim`.
        let mut guarded = local_read(engine, plan.var, span);
        if plan.guard_offset != 0 {
            let g = engine.const_operand(ValueKind::Int(plan.guard_offset as u64, var_ty), var_ty);
            guarded = alloc_binary(engine, guarded, NirBinaryOp::Add, g, var_ty, span);
        }
        let lim_read = local_read(engine, *lim, span);
        let done = alloc_pred(engine, guarded, NirBinaryOp::GtEq, lim_read, span);
        let exit = engine.alloc_stmt(
            StmtKind::Break {
                label: None,
                value: None,
            },
            span,
        );
        let guard = alloc_if_then(engine, done, exit, span);
        let mut stmts = engine.body.blocks[fast_body].stmts.clone();
        stmts[plan.guard_idx] = guard;
        engine.set_block_stmts(fast_body, stmts);
    }
    // `fast_body` is a structural clone of `plan.loop_body`, so the guard/check
    // layout `analyze_loop` recorded on `plan` holds verbatim — no re-parse.
    eliminate_checks_in_fast(engine, binds, plan, fast_body);

    let fast_stmt = engine.alloc_stmt(StmtKind::Loop { body: fast_body }, span);
    let slow_stmt = engine.alloc_stmt(
        StmtKind::Loop {
            body: plan.loop_body,
        },
        span,
    );
    let then_block = engine.alloc_block(vec![fast_stmt], span);

    let residual = residual(engine, plan, span);
    let mut stmts = engine.body.blocks[plan.parent].stmts.clone();
    let pos = stmts
        .iter()
        .position(|&s| s == plan.loop_stmt)
        .expect("versioned loop stmt must be in its parent block");
    let else_block = reload
        .is_none()
        .then(|| engine.alloc_block(vec![slow_stmt], span));
    let if_stmt = engine.alloc_stmt(
        StmtKind::If {
            condition: residual,
            then_block,
            else_block,
        },
        span,
    );
    if let Some((lim, _)) = &reload {
        let h = bound_read(engine, plan, plan.bound, span);
        let init = alloc_local_set(engine, *lim, h, span);
        stmts.splice(pos..=pos, [init, if_stmt, slow_stmt]);
    } else {
        stmts[pos] = if_stmt;
    }
    engine.set_block_stmts(plan.parent, stmts);

    FastArm {
        version_if: if_stmt,
        then_block,
        fast_body,
    }
}

/// The condition under which the fast loop's checks all hold.
fn residual(engine: &mut Engine, plan: &Plan, span: Span) -> Operand {
    let mut residual = limit_terms(engine, plan, plan.limits[0], span);
    for &limit in &plan.limits[1..] {
        let terms = limit_terms(engine, plan, limit, span);
        residual = alloc_pred(engine, residual, NirBinaryOp::And, terms, span);
    }
    if let Some(floor) = &plan.floor {
        residual = and_floor_terms(engine, plan, floor, residual, span);
    }
    residual
}

/// `if !<the reloaded limits' terms> { $lim = floor }`: the re-test after a
/// statement that may lower the reloaded bound, ending the fast loop at this
/// iteration's close when it did.
fn alloc_retest(engine: &mut Engine, plan: &Plan, lim: u32, floor: u64, span: Span) -> StmtId {
    let mut holds: Option<Operand> = None;
    for &limit in &plan.limits {
        if matches!(limit.bound, BoundKey::Field(..)) {
            let terms = limit_terms(engine, plan, limit, span);
            holds = Some(match holds {
                Some(rest) => alloc_pred(engine, rest, NirBinaryOp::And, terms, span),
                None => terms,
            });
        }
    }
    let holds = holds.expect("a reloaded plan has a reloaded limit");
    let failed = engine.alloc_expr(
        ExprKind::Unary {
            op: NirUnaryOp::Not,
            expr: holds,
        },
        TypeTable::BOOL,
        span,
    );
    let ty = engine.locals()[lim as usize].type_id;
    let floor = engine.const_operand(ValueKind::Int(floor, ty), ty);
    let end = alloc_local_set(engine, lim, floor, span);
    alloc_if_then(engine, Operand::Expr(failed), end, span)
}

/// `if condition { stmt }`.
fn alloc_if_then(engine: &mut Engine, condition: Operand, stmt: StmtId, span: Span) -> StmtId {
    let then_block = engine.alloc_block(vec![stmt], span);
    engine.alloc_stmt(
        StmtKind::If {
            condition,
            then_block,
            else_block: None,
        },
        span,
    )
}

/// The residual terms under which every `var` the guard admits keeps
/// `var + shift < B`: `H` at or under `B - konst - local` (strictly under for
/// `<=`), with `B >= konst` and a term on `local` keeping that subtraction and
/// the check's own addition clear of the wrap. The term is `local >= 0` for a
/// signed type and `local <= B - konst` for an unsigned one. An unshifted
/// check needs only the comparison. A strided check, or any under an offset
/// guard, takes [`wide_limit_terms`].
fn limit_terms(engine: &mut Engine, plan: &Plan, limit: Limit, span: Span) -> Operand {
    if limit.shift.stride.is_some() || plan.guard_offset != 0 {
        return wide_limit_terms(engine, plan, limit, span);
    }
    let ty = engine.locals()[plan.var as usize].type_id;
    let int = |e: &mut Engine, c: i64| e.const_operand(ValueKind::Int(c as u64, ty), ty);
    let bound = |e: &mut Engine| bound_read(e, plan, limit.bound, span);
    let less_konst = |e: &mut Engine| {
        let b = bound(e);
        if limit.shift.konst == 0 {
            return b;
        }
        let konst = int(e, limit.shift.konst);
        alloc_binary(e, b, NirBinaryOp::Sub, konst, ty, span)
    };
    let mut room = less_konst(engine);
    let mut clear = Vec::new();
    let bound_clear = match limit.bound {
        BoundKey::Const(c) => c >= limit.shift.konst,
        BoundKey::Local(_) => plan.unsigned && limit.shift.konst == 0,
        BoundKey::Field(..) => false,
    };
    if limit.shift != Shift::NONE && !bound_clear {
        let b = bound(engine);
        let konst = int(engine, limit.shift.konst);
        clear.push(alloc_pred(engine, b, NirBinaryOp::GtEq, konst, span));
    }
    if let Some(s) = limit.shift.local {
        let s_read = local_read(engine, s, span);
        clear.push(if plan.unsigned {
            let most = less_konst(engine);
            alloc_pred(engine, s_read, NirBinaryOp::LtEq, most, span)
        } else {
            let zero = int(engine, 0);
            alloc_pred(engine, s_read, NirBinaryOp::GtEq, zero, span)
        });
        let s_read = local_read(engine, s, span);
        room = alloc_binary(engine, room, NirBinaryOp::Sub, s_read, ty, span);
    }
    let op = if plan.guard_le {
        NirBinaryOp::Lt
    } else {
        NirBinaryOp::LtEq
    };
    let h_read = bound_read(engine, plan, plan.bound, span);
    let fits = alloc_pred(engine, h_read, op, room, span);
    conjoin(engine, clear, fits, span)
}

/// The residual terms for an `i32` check `var * stride + local + konst < B`
/// (the stride `1` where it has none), compared in `i64`: the largest `var`
/// the guard admits (`H - g`, or `H - g - 1` under `<`) lands under `B`, with
/// `local >= 0` and `stride >= 1`. With the check's demanded `var >= 0`, every
/// partial sum the loop computes then lies in `0..B`, so none wraps. Where that
/// largest `var` is `>= 0`, the only case the loop runs an iteration in,
/// `stride >= 1`, `local >= 0` and `konst >= 0` also keep it under `B`: the
/// headroom a `+ 1` step's wrap under a `<=` guard rests on (see
/// [`induction_entry`]).
fn wide_limit_terms(engine: &mut Engine, plan: &Plan, limit: Limit, span: Span) -> Operand {
    let ty = engine.locals()[plan.var as usize].type_id;
    let wide = |e: &mut Engine, expr: Operand| {
        Operand::Expr(e.alloc_expr(
            ExprKind::Cast {
                expr,
                target_type: TypeTable::I64,
            },
            TypeTable::I64,
            span,
        ))
    };
    let long = |e: &mut Engine, c: i64| {
        e.const_operand(ValueKind::Int(c as u64, TypeTable::I64), TypeTable::I64)
    };
    let add = |e: &mut Engine, l, r| alloc_binary(e, l, NirBinaryOp::Add, r, TypeTable::I64, span);
    let mut clear = Vec::new();
    let h_read = bound_read(engine, plan, plan.bound, span);
    let mut reach = wide(engine, h_read);
    let below = plan.guard_offset + i64::from(!plan.guard_le);
    if below != 0 {
        let less = long(engine, -below);
        reach = add(engine, reach, less);
    }
    if let Some(stride) = limit.shift.stride {
        let stride = match stride {
            BoundKey::Local(s) => {
                let s_read = local_read(engine, s, span);
                let one = engine.const_operand(ValueKind::Int(1, ty), ty);
                clear.push(alloc_pred(engine, s_read, NirBinaryOp::GtEq, one, span));
                let s_read = local_read(engine, s, span);
                wide(engine, s_read)
            }
            BoundKey::Const(c) => long(engine, c),
            BoundKey::Field(..) => unreachable!("`Terms::scaled` reads a local or a constant"),
        };
        reach = alloc_binary(
            engine,
            reach,
            NirBinaryOp::Mul,
            stride,
            TypeTable::I64,
            span,
        );
    }
    if let Some(l) = limit.shift.local {
        let l_read = local_read(engine, l, span);
        let zero = engine.const_operand(ValueKind::Int(0, ty), ty);
        clear.push(alloc_pred(engine, l_read, NirBinaryOp::GtEq, zero, span));
        let l_read = local_read(engine, l, span);
        let l_wide = wide(engine, l_read);
        reach = add(engine, reach, l_wide);
    }
    if limit.shift.konst != 0 {
        let konst = long(engine, limit.shift.konst);
        reach = add(engine, reach, konst);
    }
    let b_read = bound_read(engine, plan, limit.bound, span);
    let b_wide = wide(engine, b_read);
    let fits = alloc_pred(engine, reach, NirBinaryOp::Lt, b_wide, span);
    conjoin(engine, clear, fits, span)
}

/// `terms[0] && … && last`, each guarding the ones after it.
fn conjoin(engine: &mut Engine, terms: Vec<Operand>, last: Operand, span: Span) -> Operand {
    terms.into_iter().rev().fold(last, |rest, term| {
        alloc_pred(engine, term, NirBinaryOp::And, rest, span)
    })
}

/// Conjoin the floor terms onto `residual`: `var >= demanded`, `var <= MAX - g`
/// under an offset guard, and the `step >= 0` / `H <= MAX - step` pair that
/// says `var` rises without wrapping.
fn and_floor_terms(
    engine: &mut Engine,
    plan: &Plan,
    floor: &RuntimeFloor,
    residual: Operand,
    span: Span,
) -> Operand {
    let ty = engine.locals()[plan.var as usize].type_id;
    let pred = |e: &mut Engine, l, op, r| alloc_pred(e, l, op, r, span);

    let var_read = local_read(engine, plan.var, span);
    let demanded = engine.const_operand(ValueKind::Int(floor.demanded as u64, ty), ty);
    let at_least = pred(engine, var_read, NirBinaryOp::GtEq, demanded);
    let mut out = pred(engine, residual, NirBinaryOp::And, at_least);
    // The first guard's `var + g` must not wrap; each later `var` is one the
    // guard admitted, plus a step the terms below keep clear of it.
    if plan.guard_offset != 0 {
        let var_read = local_read(engine, plan.var, span);
        let offset = u64::try_from(plan.guard_offset).expect("`parse_loop_guard` admits g > 0");
        let bits = floor.type_max - offset;
        let most = engine.const_operand(ValueKind::Int(bits, ty), ty);
        let room = pred(engine, var_read, NirBinaryOp::LtEq, most);
        out = pred(engine, out, NirBinaryOp::And, room);
    }

    let headroom = match floor.step {
        // The residual already puts the last `i + 1` at or under `B`, so no wrap.
        InductionStep::Const(k) if k <= 1 => return out,
        InductionStep::Const(k) => {
            let bits = floor.type_max.wrapping_sub(k as u64);
            engine.const_operand(ValueKind::Int(bits, ty), ty)
        }
        InductionStep::Local(s) => {
            let step_read = local_read(engine, s, span);
            let zero = engine.const_operand(ValueKind::Int(0, ty), ty);
            let rising = pred(engine, step_read, NirBinaryOp::GtEq, zero);
            out = pred(engine, out, NirBinaryOp::And, rising);
            let max = engine.const_operand(ValueKind::Int(floor.type_max, ty), ty);
            let step_read = local_read(engine, s, span);
            alloc_binary(engine, max, NirBinaryOp::Sub, step_read, ty, span)
        }
    };
    let h_read = bound_read(engine, plan, plan.bound, span);
    let no_wrap = pred(engine, h_read, NirBinaryOp::LtEq, headroom);
    pred(engine, out, NirBinaryOp::And, no_wrap)
}

/// In the fast clone, drive every implied check to `false` (the paired
/// `BranchPruneRule` removes the dead panic arms), and constify the
/// single-purpose `let $cond = <cmp>` temp feeding each check — in the fast
/// arm the comparison is provably constant, so this is exact for any reader.
/// `fast_body` is a clone of `plan.loop_body`, so `plan`'s guard layout applies.
fn eliminate_checks_in_fast(engine: &mut Engine, binds: &Binds, plan: &Plan, fast_body: BlockId) {
    let (var, h) = (plan.var, plan.bound);
    let mut checks: Vec<Check> = Vec::new();
    let stmts = engine.body.blocks[fast_body].stmts.clone();
    for &s in stmts.iter().skip(plan.guard_idx + 1) {
        if stmt_modifies(engine, s, var, h) {
            break;
        }
        collect_checks_in_node(engine, binds, fast_body, NodeRef::Stmt(s), var, &mut checks);
    }
    for check in checks {
        // As in `condition_implication`: the elimination drops the condition
        // expression, so a condition that writes must stay.
        if !plan.limits.contains(&check.limit) || !is_pure_operand(engine.body, check.cond) {
            continue;
        }
        constify_check_temp(engine, binds, check.cond, plan.var, check.limit, fast_body);
        eliminate_condition(engine, check.holder, check.cond);
    }
}

/// If the check condition reads a `let`-bound temp, replace that temp's
/// initializer in the fast clone with the constant the comparison provably
/// evaluates to (`!c` panics ⇒ `c` is `true`), deleting the per-iteration
/// compare. Overwritten only when the initializer structurally *is* that
/// comparison, a function-scoped slot being re-bindable.
fn constify_check_temp(
    engine: &mut Engine,
    binds: &Binds,
    cond: Operand,
    var: u32,
    limit: Limit,
    fast_body: BlockId,
) {
    let negated = negated_operand(engine, binds, cond);
    let Some(temp) = operand_local(engine.body, negated.unwrap_or(cond)) else {
        return;
    };
    let konst = negated.is_some();
    let limit = konst.then_some(limit);
    // Find the temp's comparison `let` inside the fast clone and overwrite it.
    let mut stack = vec![NodeRef::Block(fast_body)];
    while let Some(n) = stack.pop() {
        if let NodeRef::Stmt(s) = n
            && let StmtKind::Let {
                local_index, value, ..
            } = &engine.body.stmts[s].kind
            && *local_index == temp
            && is_pure_operand(engine.body, *value)
            && is_check_comparison(engine, binds, fast_body, *value, var, limit)
        {
            let v = engine
                .body
                .values
                .alloc_unshared(ValueKind::Bool(konst), TypeTable::BOOL);
            if let StmtKind::Let { value, .. } = &mut engine.body.stmts[s].kind {
                *value = Operand::Value(v);
            }
            engine.enqueue(NodeRef::Stmt(s));
            return;
        }
        engine.body.for_each_child(n, |c| stack.push(c));
    }
}

/// Whether `value` is a bounds-check comparison over `var` — a relational
/// operator (`<`, `<=`, `>`, `>=`) with `var` as one operand, or the check's
/// own `var + shift < B` — in either the skeleton (`ExprKind::Binary`) or
/// promoted-operand (`ValueKind::Binary`) form. The eliminated check's
/// condition is exactly such a comparison, so this confirms the `let` being
/// constified is that comparison and not an unrelated binding reusing the
/// temp's local slot. `limit` is `None` where the temp holds the failing
/// predicate, which the check's `<` never is.
fn is_check_comparison(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
    value: Operand,
    var: u32,
    limit: Option<Limit>,
) -> bool {
    let Some((left, op, right)) = binary_parts(engine.body, value) else {
        return false;
    };
    let reads_var = |op| operand_local(engine.body, op) == Some(var);
    let relational = matches!(
        op,
        NirBinaryOp::Lt | NirBinaryOp::LtEq | NirBinaryOp::Gt | NirBinaryOp::GtEq
    );
    let is_limit = limit.is_some_and(|limit| {
        op == NirBinaryOp::Lt
            && parse_limit(engine, binds, loop_body, left, right, var) == Some(limit)
    });
    (relational && (reads_var(left) || reads_var(right))) || is_limit
}

/// Whether the subtree under `block` re-binds any of `locals` via `let`, or
/// contains any `LetDestructure` (whose pattern could bind one). A per-
/// iteration re-binding would invalidate the loop-invariance the residual
/// relies on; `node_modifies` only tracks `Assign` / `&mut`, so `let`
/// re-definitions need this separate scan. (Fresh NIR mints a new local per
/// shadowing `let`, so this is a defensive guard against synthesized IR.)
fn subtree_redefines(engine: &Engine, block: BlockId, locals: &[u32]) -> bool {
    engine
        .body
        .find_in_nodes_under(NodeRef::Block(block), |n| {
            let NodeRef::Stmt(s) = n else { return None };
            match &engine.body.stmts[s].kind {
                StmtKind::Let { local_index, .. } => locals.contains(local_index).then_some(()),
                StmtKind::LetDestructure { .. } => Some(()),
                StmtKind::Expr(_)
                | StmtKind::Return { .. }
                | StmtKind::If { .. }
                | StmtKind::Loop { .. }
                | StmtKind::Break { .. }
                | StmtKind::Continue
                | StmtKind::LabeledBlock { .. } => None,
            }
        })
        .is_some()
}

/// Collapse a cleaned fast arm — `loop { if !(i CMP H) break; [local writes];
/// array_set(A, i, CONST); i += 1 }` — into one `array_fill` followed by those
/// writes replayed once at the last iterated `i`. The count is `H + 1 - i` for
/// `<=` (the residual proving no overflow) and `H - i` for `<`; the wrapping
/// `if` preserves the zero-iteration case exactly.
fn try_fill_idiom(
    engine: &mut Engine,
    binds: &Binds,
    descriptors: &[FunctionRef],
    fill_id: FuncId,
    plan: &Plan,
    arm: &FastArm,
) -> bool {
    // The fill form needs an unshifted guard at index 0 (no leading lets), whose
    // count it reads; `plan` carries the guard layout from analysis, and
    // `arm.fast_body` is its structural clone.
    let (var, h, guard_le) = (plan.var, plan.bound, plan.guard_le);
    if plan.guard_idx != 0 || plan.guard_offset != 0 {
        return false;
    }
    // The then-block must still hold exactly the fast loop statement.
    let then_stmts = engine.body.blocks[arm.then_block].stmts.clone();
    let [loop_stmt] = then_stmts[..] else {
        return false;
    };
    if !matches!(
        engine.body.stmts[loop_stmt].kind,
        StmtKind::Loop { body } if body == arm.fast_body
    ) {
        return false;
    }
    let stmts = engine.body.blocks[arm.fast_body].stmts.clone();
    // Shape: guard first, then pure lets, one array_set, and `i += 1` last.
    let n = stmts.len();
    if n < 3 {
        return false;
    }
    // Increment: `i = i + 1` as the last statement.
    let StmtKind::Expr(incr_op) = &engine.body.stmts[stmts[n - 1]].kind else {
        return false;
    };
    let Some(incr_e) = incr_op.as_expr() else {
        return false;
    };
    let ExprKind::Assign { target, value } = &engine.body.exprs[incr_e].kind else {
        return false;
    };
    if !matches!(&engine.body.exprs[*target].kind, ExprKind::Local { index, .. } if *index == var) {
        return false;
    }
    if parse_var_offset(engine, binds, *value) != Some((var, 1)) {
        return false;
    }
    // The array_set call just before the increment.
    let StmtKind::Expr(call_op) = &engine.body.stmts[stmts[n - 2]].kind else {
        return false;
    };
    let Some(call_e) = call_op.as_expr() else {
        return false;
    };
    let ExprKind::Call { func_id, args, .. } = &engine.body.exprs[call_e].kind else {
        return false;
    };
    if !callee_descriptor(descriptors, *func_id).is_builtin_named("array_set") || args.len() != 3 {
        return false;
    }
    let (arr_op, arr_is_mut) = (args[0].expr, args[0].is_mut);
    let idx_op = args[1].expr;
    let val_op = args[2].expr;
    // Index must be exactly `i` (offset 0).
    if parse_var_offset(engine, binds, idx_op) != Some((var, 0)) {
        return false;
    }
    // Value must be a constant (loop-invariant by construction, and safe to
    // re-emit once).
    let Operand::Value(val_v) = val_op else {
        return false;
    };
    if !matches!(
        engine.body.values.kind(val_v),
        ValueKind::Int(..) | ValueKind::Float(..) | ValueKind::Bool(_) | ValueKind::Char(_)
    ) {
        return false;
    }
    // Array operand: a (`&mut`) read of a local defined outside the loop —
    // pure, so evaluating it once instead of per iteration is exact.
    let arr_local = match arr_op {
        Operand::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => match expr.as_expr().map(|ie| &engine.body.exprs[ie].kind) {
                Some(ExprKind::Local { index, .. }) => *index,
                _ => return false,
            },
            ExprKind::Local { index, .. } => *index,
            _ => return false,
        },
        Operand::Value(_) => return false,
    };
    // Middle statements are replayed verbatim at the last iterated `i`, so a
    // read of a local the sequence writes at or after it would see the wrong
    // iteration's value.
    let middle: Vec<StmtId> = stmts[1..n - 2].to_vec();
    let mut reserved = vec![var, arr_local];
    if let BoundKey::Local(h) = h {
        reserved.push(h);
    }
    let mut effects: Vec<Effects> = Vec::new();
    for &s in &middle {
        let Some(e) = replayable_effects(engine, s, &reserved) else {
            return false;
        };
        effects.push(e);
    }
    for (i, e) in effects.iter().enumerate() {
        let carried = |r: &u32| effects[i..].iter().any(|w| w.writes.contains(r));
        if e.reads.iter().any(carried) {
            return false;
        }
    }
    // A branch-guarded write is not the last iteration's to make, so it
    // survives only where nothing can tell which iteration made it last: the
    // slow arm alone names the local, and the version `if` runs once.
    let mut conditional = effects.iter().flat_map(|e| &e.conditional).peekable();
    if conditional.peek().is_some()
        && (block_repeats(engine.body, plan.parent)
            || conditional.any(|&l| !confined_to(engine, l, arm.version_if)))
    {
        return false;
    }
    let span = engine.body.stmts[loop_stmt].span;
    let ty = engine.locals()[var as usize].type_id;
    let one = engine.body.values.int_typed(1, ty);

    // `H + 1` (for `<=`) / `H` (for `<`) — the exclusive upper end.
    let upper = |engine: &mut Engine| -> Operand {
        let h_read = bound_read(engine, plan, h, span);
        if guard_le {
            alloc_binary(
                engine,
                h_read,
                NirBinaryOp::Add,
                Operand::Value(one),
                ty,
                span,
            )
        } else {
            h_read
        }
    };

    let upper_for_len = upper(engine);
    let i_read = local_read(engine, var, span);
    let count = alloc_binary(engine, upper_for_len, NirBinaryOp::Sub, i_read, ty, span);
    let offset = local_read(engine, var, span);
    let fill_call = engine.alloc_expr(
        ExprKind::Call {
            func_id: fill_id,
            type_args: Vec::new(),
            args: CallArgs::free(vec![
                ArenaCallArg {
                    expr: arr_op,
                    is_mut: arr_is_mut,
                },
                ArenaCallArg {
                    expr: offset,
                    is_mut: false,
                },
                ArenaCallArg {
                    expr: val_op,
                    is_mut: false,
                },
                ArenaCallArg {
                    expr: count,
                    is_mut: false,
                },
            ]),
        },
        TypeTable::UNIT,
        span,
    );
    let fill_stmt = engine.alloc_stmt(StmtKind::Expr(Operand::Expr(fill_call)), span);

    let mut body_stmts = vec![fill_stmt];
    // The fill consumed every iteration's store; what the middle statements
    // leave behind is the last iteration's, so set `i` to the value that
    // iteration saw (`H` for `<=`, `H - 1` for `<`) and run them once.
    if !middle.is_empty() {
        let last_iterated = if guard_le {
            bound_read(engine, plan, h, span)
        } else {
            let h_read = bound_read(engine, plan, h, span);
            alloc_binary(
                engine,
                h_read,
                NirBinaryOp::Sub,
                Operand::Value(one),
                ty,
                span,
            )
        };
        body_stmts.push(alloc_local_set(engine, var, last_iterated, span));
        body_stmts.extend(middle);
    }
    // `i`'s final value: `H + 1` for `<=`, `H` for `<`.
    let i_final = upper(engine);
    body_stmts.push(alloc_local_set(engine, var, i_final, span));

    // `if i CMP H { ... }` preserves the zero-iteration case.
    let i_read2 = local_read(engine, var, span);
    let h_read2 = bound_read(engine, plan, h, span);
    let op = if guard_le {
        NirBinaryOp::LtEq
    } else {
        NirBinaryOp::Lt
    };
    let enter = alloc_binary(engine, i_read2, op, h_read2, TypeTable::BOOL, span);
    let body_block = engine.alloc_block(body_stmts, span);
    let fill_if = engine.alloc_stmt(
        StmtKind::If {
            condition: enter,
            then_block: body_block,
            else_block: None,
        },
        span,
    );
    engine.set_block_stmts(arm.then_block, vec![fill_if]);
    true
}

/// The locals one middle statement reads and writes.
struct Effects {
    reads: Vec<u32>,
    writes: Vec<u32>,
    /// The writes a branch guards, which the last iteration need not have made.
    conditional: Vec<u32>,
}

/// What `stmt` reads and writes, or `None` when running it once cannot stand
/// for the loop's last iteration. A trap is the effect of the iteration that
/// raises it, so a statement that may trap is not replayable either.
fn replayable_effects(engine: &Engine, stmt: StmtId, reserved: &[u32]) -> Option<Effects> {
    let body = &engine.body;
    let types = engine.value_graph_type_table();
    let mut eff = Effects {
        reads: Vec::new(),
        writes: Vec::new(),
        conditional: Vec::new(),
    };
    // A block that binds a local and yields it reads that local only to yield
    // it, so the read is its own write and not a value carried from before.
    let mut captured: Vec<u32> = Vec::new();
    let mut roots = vec![NodeRef::Stmt(stmt)];
    while let Some(root) = roots.pop() {
        let rejected = body.walk_nodes_under::<()>(root, |n| {
            if !pooled_operands_are_literal(body, n) {
                return ControlFlow::Break(());
            }
            match n {
                NodeRef::Block(_) => {}
                // A pattern binds locals of its own, which no walk over
                // expressions accounts for.
                NodeRef::Pat(_) => return ControlFlow::Break(()),
                NodeRef::Stmt(s) => match &body.stmts[s].kind {
                    StmtKind::Let { local_index, .. } => eff.writes.push(*local_index),
                    StmtKind::If { .. } => collect_writes(body, n, &mut eff.conditional),
                    StmtKind::LabeledBlock { label, .. } if has_break_to(body, n, label) => {
                        collect_writes(body, n, &mut eff.conditional);
                    }
                    StmtKind::Expr(_) | StmtKind::LabeledBlock { .. } => {}
                    _ => return ControlFlow::Break(()),
                },
                NodeRef::Expr(e) => match &body.exprs[e].kind {
                    ExprKind::Local { index, .. } => eff.reads.push(*index),
                    ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr,
                    } => {
                        let Some(ExprKind::Local { index, .. }) =
                            expr.as_expr().map(|ie| &body.exprs[ie].kind)
                        else {
                            return ControlFlow::Break(());
                        };
                        eff.writes.push(*index);
                        return ControlFlow::Continue(false);
                    }
                    ExprKind::Assign { target, value } => {
                        let ExprKind::Local { index, .. } = &body.exprs[*target].kind else {
                            return ControlFlow::Break(());
                        };
                        eff.writes.push(*index);
                        if let Some(ve) = value.as_expr() {
                            roots.push(NodeRef::Expr(ve));
                        }
                        return ControlFlow::Continue(false);
                    }
                    ExprKind::LabeledBlock { label, .. } => {
                        if let Some((l, _)) = capture_block_binding(engine, Operand::Expr(e))
                            && local_read_count(body, NodeRef::Stmt(stmt), l) == 1
                        {
                            captured.push(l);
                        }
                        // A block nothing breaks out of runs to its end, so its
                        // writes are as unconditional as the block is.
                        if has_break_to(body, n, label) {
                            collect_writes(body, n, &mut eff.conditional);
                        }
                    }
                    ExprKind::If { .. } | ExprKind::Match { .. } | ExprKind::Switch { .. } => {
                        collect_writes(body, n, &mut eff.conditional);
                    }
                    // `&&` / `||` short-circuit, so the right side runs only on
                    // the iterations the left side let through.
                    ExprKind::Binary {
                        op: NirBinaryOp::And | NirBinaryOp::Or,
                        right,
                        ..
                    } => {
                        if let Some(re) = right.as_expr() {
                            collect_writes(body, NodeRef::Expr(re), &mut eff.conditional);
                        }
                    }
                    ExprKind::Dead
                    | ExprKind::GlobalVarSet { .. }
                    | ExprKind::Call { .. }
                    | ExprKind::CmRawCall { .. }
                    | ExprKind::IndirectCall { .. }
                    | ExprKind::ClosureToCanonical { .. } => return ControlFlow::Break(()),
                    _ if expr_node_may_trap_typed(body, e, types) => return ControlFlow::Break(()),
                    _ => {}
                },
            }
            ControlFlow::Continue(true)
        });
        if rejected.is_some() {
            return None;
        }
    }
    if eff.writes.iter().any(|w| reserved.contains(w)) {
        return None;
    }
    eff.reads.retain(|r| !captured.contains(r));
    Some(eff)
}

/// Whether every pooled operand of `node` is a literal. Anything else is a
/// promoted expression, which hides reads and writes from a skeleton walk.
fn pooled_operands_are_literal(body: &Body, node: NodeRef) -> bool {
    let mut literal = true;
    body.for_each_operand(node, |op| {
        if let Operand::Value(v) = op {
            literal &= matches!(
                body.values.kind(v),
                ValueKind::Int(..) | ValueKind::Float(..) | ValueKind::Bool(_) | ValueKind::Char(_)
            );
        }
    });
    literal
}

/// Every local written anywhere under `node`.
fn collect_writes(body: &Body, node: NodeRef, out: &mut Vec<u32>) {
    body.for_each_node_under(node, |n| {
        if let Some(l) = local_written_by(body, n) {
            out.push(l);
        }
        if let NodeRef::Stmt(s) = n
            && let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
        {
            out.push(*local_index);
        }
    });
}

/// Whether local `l` is named nowhere in the body but under `version_if`. The
/// slow arm is then its only other reader, and an arm the fast one replaced
/// does not run.
fn confined_to(engine: &Engine, l: u32, version_if: StmtId) -> bool {
    let body = &engine.body;
    let root = NodeRef::Block(body.root());
    !mentions_local_except(body, root, Some(NodeRef::Stmt(version_if)), l)
}

/// Whether `block` sits inside a loop, and so may run more than once.
fn block_repeats(body: &Body, block: BlockId) -> bool {
    body.find_in_nodes_under(NodeRef::Block(body.root()), |n| {
        let NodeRef::Stmt(s) = n else { return None };
        let StmtKind::Loop { body: inner } = &body.stmts[s].kind else {
            return None;
        };
        block_under(body, *inner, block).then_some(())
    })
    .is_some()
}

/// Whether `block` is `root` or nested under it.
fn block_under(body: &Body, root: BlockId, block: BlockId) -> bool {
    root == block
        || body
            .find_in_nodes_under(NodeRef::Block(root), |n| {
                (n == NodeRef::Block(block)).then_some(())
            })
            .is_some()
}

/// How many times `root` reads local `l`. An assignment's target names a place
/// rather than reading one, so it does not count.
fn local_read_count(body: &Body, root: NodeRef, l: u32) -> usize {
    let mut count = 0;
    body.walk_nodes_under::<()>(root, |n| {
        if let NodeRef::Expr(e) = n {
            match &body.exprs[e].kind {
                ExprKind::Local { index, .. } if *index == l => count += 1,
                ExprKind::Assign { value, .. } => {
                    if let Some(ve) = value.as_expr() {
                        count += local_read_count(body, NodeRef::Expr(ve), l);
                    }
                    return ControlFlow::Continue(false);
                }
                _ => {}
            }
        }
        ControlFlow::Continue(true)
    });
    count
}

/// A `Let` statement re-binding local `l` to `value` (locals are
/// function-scoped slots, so a second `let` is a plain re-definition).
fn alloc_local_set(engine: &mut Engine, l: u32, value: Operand, span: Span) -> StmtId {
    let name = engine.local_name(l);
    let ty = engine.locals()[l as usize].type_id;
    let is_mut = engine.locals()[l as usize].is_mut;
    engine.alloc_stmt(
        StmtKind::Let {
            name,
            local_index: l,
            is_mut,
            type_id: ty,
            value,
            skip_value_copy: true,
        },
        span,
    )
}
