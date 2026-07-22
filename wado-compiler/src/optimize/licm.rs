//! Loop-Invariant Code Motion (LICM) for Wado NIR
//!
//! This module hoists loop-invariant computations out of loops to improve performance.
//! Two kinds of candidates move to the pre-header: field accesses on
//! variables the loop does not modify (legality via [`ModifiedVars`]), and
//! pure-arithmetic subtrees whose `Local` leaves are pre-header-stable
//! (never modified in the loop), deduped by structural identity
//! ([`ArithKey`]; see [`ArithHoist`]).
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and applies LICM to every loop in the function. All
//! mutations route through the engine edit API (`alloc_expr`, `alloc_stmt`,
//! `alloc_local`, `clone_expr`, `set_block_stmts`, `replace_expr_kind`) so
//! the parent map and use index stay coherent.
//!
//! The hoist-candidate and replacement walks recurse over
//! [`Body::for_each_child`], skipping pattern children; `collect_modified_vars`
//! keeps its own walk because it special-cases assignments, calls, and pattern
//! bindings.

use std::cell::Cell;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{
    ArenaCallArg, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueId;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

/// Tracks which variables and fields are modified within a loop.
///
/// Distinguishes between full-object modification (e.g., `buf = new_string`, `&mut buf`)
/// and field-level modification (e.g., `buf.len = buf.len + 1`), enabling LICM to
/// hoist field accesses like `buf.repr` even when `buf.len` is modified.
///
/// Also tracks GC reference aliases: when `let a = b` copies a GC struct reference,
/// `a` and `b` point to the same heap object. Modifications through one alias must
/// prevent hoisting field accesses on the other.
#[derive(Default)]
struct ModifiedVars {
    /// Locals that are fully modified (assigned as a whole, passed as &mut, etc.).
    /// Membership poisons the whole alias set: a full modification of one alias
    /// may write the shared heap object every alias points at.
    fully: IndexSet<u32>,
    /// Alias locals (re)bound by an in-loop `let x = y` / `let r = &y`: the
    /// local itself is not pre-header-stable (its binding runs inside the
    /// loop), but the binding writes only the local slot, never the pointee —
    /// so, unlike `fully`, membership does not poison the alias set.
    rebound: IndexSet<u32>,
    /// (`local_index`, `field_index`) pairs where only a specific field is modified.
    fields: IndexSet<(u32, u32)>,
    /// GC alias pairs: if `(a, b)` is present, `a` and `b` may point to the same object.
    aliases: Vec<(u32, u32)>,
    /// `(pointee_type, field_index)` for every field written in the loop. Wado
    /// references alias, so a write through one `&T` is seen through any other;
    /// the `(local, field)` tracking above misses writes via a different alias.
    /// Used by `is_reference_field_aliasing_written`.
    written_field_types: IndexSet<(TypeId, u32)>,
    /// Pointee struct types passed by `&mut` to a call/method in the loop: the
    /// callee may write *any* field, so no field of that type is invariant.
    clobbered_pointee_types: IndexSet<TypeId>,
}

impl ModifiedVars {
    fn insert_full(&mut self, local_idx: u32) {
        self.fully.insert(local_idx);
    }

    fn insert_rebound(&mut self, local_idx: u32) {
        self.rebound.insert(local_idx);
    }

    fn insert_field(&mut self, local_idx: u32, field_idx: u32) {
        self.fields.insert((local_idx, field_idx));
    }

    fn insert_written_field_type(&mut self, pointee: TypeId, field_idx: u32) {
        self.written_field_types.insert((pointee, field_idx));
    }

    fn insert_clobbered_pointee_type(&mut self, pointee: TypeId) {
        self.clobbered_pointee_types.insert(pointee);
    }

    /// True when hoisting `x.field_idx` is unsound: `x` is a reference whose
    /// pointee's `field_idx` is written in the loop — directly via an alias, or
    /// opaquely by a call that received the pointee by `&mut`. By-value roots
    /// are covered by the `fully`/`fields`/alias machinery.
    ///
    /// The opaque-`&mut`-call case is type-keyed and restricted to plain structs:
    /// any `&Struct`/`&mut Struct` read of a clobbered struct type is treated as
    /// aliasing. Generic instances (`List`/`String`) are deliberately excluded
    /// here — type-keying them would block reads of an unrelated read-only
    /// `&List` whenever any same-typed list is mutated in the loop (e.g. a lookup
    /// `table` while building `out`). Their cascade hazard is handled precisely
    /// in `licm_loop` via [`Self::is_clobbered_gc_value`] on hoist locals.
    fn is_reference_field_aliasing_written(
        &self,
        root_type: TypeId,
        field_idx: u32,
        type_table: &TypeTable,
    ) -> bool {
        match type_table.get(root_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                let pointee = strip_references(*inner, type_table);
                let struct_clobbered =
                    matches!(type_table.get(pointee), ResolvedType::Struct { .. })
                        && self.clobbered_pointee_types.contains(&pointee);
                struct_clobbered || self.written_field_types.contains(&(pointee, field_idx))
            }
            _ => false,
        }
    }

    /// True when `value_type`'s pointee is a GC heap object `&mut`-clobbered by an
    /// opaque call in the loop. Used only for hoist locals: a hoisted handle
    /// (`_licm = obj.list`, an aliasing copy) whose object is then mutated
    /// through another alias has opaquely-changing fields (e.g. a `List`'s
    /// length), so cascade-hoisting `_licm.used` would freeze a loop guard
    /// (#1472). The handle hoist itself stays — only its sub-field hoist is
    /// blocked, so the common "hoist a String/List handle, mutate through it"
    /// pattern is unaffected.
    fn is_clobbered_gc_value(&self, value_type: TypeId, type_table: &TypeTable) -> bool {
        let pointee = strip_references(value_type, type_table);
        is_gc_heap_type(pointee, type_table) && self.clobbered_pointee_types.contains(&pointee)
    }

    fn add_alias(&mut self, a: u32, b: u32) {
        self.aliases.push((a, b));
    }

    /// Collect all locals that alias with `local_idx` (transitively).
    fn alias_set(&self, local_idx: u32) -> IndexSet<u32> {
        let mut set = IndexSet::default();
        set.insert(local_idx);
        let mut changed = true;
        while changed {
            changed = false;
            for &(a, b) in &self.aliases {
                if set.contains(&a) && set.insert(b) {
                    changed = true;
                }
                if set.contains(&b) && set.insert(a) {
                    changed = true;
                }
            }
        }
        set
    }

    /// Returns true if hoisting `local_idx.field_idx` to the pre-header is
    /// legal: the local itself is not (re)bound inside the loop (a rebound
    /// local is out of scope at the pre-header), it is not fully modified,
    /// and the specific field is not field-modified — considering all aliases
    /// of the local. A `rebound` alias does not block: its binding rewrites
    /// only the alias slot, never the shared pointee.
    fn is_field_hoistable(&self, local_idx: u32, field_idx: u32) -> bool {
        if self.rebound.contains(&local_idx) {
            return false;
        }
        let aliases = self.alias_set(local_idx);
        for &idx in &aliases {
            if self.fully.contains(&idx) || self.fields.contains(&(idx, field_idx)) {
                return false;
            }
        }
        true
    }

    /// Whether `local_idx`'s value can change inside the loop — i.e. it is
    /// **not** loop-invariant: it is (re)bound by an in-loop `let`, or it (or
    /// any alias) is fully modified. This is exactly the invariance the
    /// `ValueGraph`'s `use-site value == loop-entry value` check computed, so an
    /// arith-hoist leaf check can read it instead of querying `value_of`.
    fn local_modified(&self, local_idx: u32) -> bool {
        self.rebound.contains(&local_idx)
            || self
                .alias_set(local_idx)
                .iter()
                .any(|idx| self.fully.contains(idx))
    }
}

/// Apply Loop-Invariant Code Motion to all functions in the project.
pub fn apply_licm(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let panic_ids = super::condition_implication::resolve_panic_ids(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::Licm, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = LicmRule {
            type_table: &type_table,
            applied: Cell::new(false),
        };
        let NirFunction {
            body,
            locals,
            params,
            address_taken_locals,
            stores_aliased_locals,
            ..
        } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let (aliased, untrackable, mut_escaped) = super::alias::builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let param_locals: Vec<u32> = params.iter().map(|p| p.local_index).collect();
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        engine.set_value_graph_type_table(&type_table);
        engine.set_param_locals(param_locals);
        engine.set_panic_callee_ids(&panic_ids);
        engine.set_pure_builtin_callees(&pure_builtin_callees);
        let licm_changed = engine.run(&[&rule]);
        // Condition implication shares licm's session: licm hoists only
        // loop-invariant, move-safe code, so values are preserved and the
        // ValueGraph stays valid. cond-impl runs after licm here — the same
        // document order as the standalone passes — so it still sees the hoisted
        // body.
        let cond_changed = super::condition_implication::eliminate_at_root(&mut engine);
        licm_changed || cond_changed
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function LICM walk at the body root.
pub(super) struct LicmRule<'a> {
    type_table: &'a TypeTable,
    applied: Cell<bool>,
}

impl Rule for LicmRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        let root = engine.body.root;
        let mut ctx = LicmCtx::new(self.type_table, engine.locals());
        let mut outer_aliases: Vec<(u32, u32)> = Vec::new();
        licm_block(engine, root, &mut ctx, &mut outer_aliases)
    }
}

/// Per-function LICM session state, threaded through the whole walk.
struct LicmCtx<'a> {
    type_table: &'a TypeTable,
    /// Locals created by a LICM hoist. Every hoist in this session inserts
    /// its fresh local; at session start the set is seeded from the
    /// [`LICM_HOIST_PREFIX`] naming convention — the only marker that
    /// persists on hoist locals surviving from a prior pass invocation.
    hoist_locals: IndexSet<u32>,
}

impl<'a> LicmCtx<'a> {
    fn new(type_table: &'a TypeTable, locals: &[crate::nir::NirLocal]) -> Self {
        let hoist_locals = locals
            .iter()
            .enumerate()
            .filter(|(_, l)| l.name.starts_with(LICM_HOIST_PREFIX))
            .map(|(i, _)| i as u32)
            .collect();
        Self {
            type_table,
            hoist_locals,
        }
    }
}

/// Collect a node's children into a buffer, so a mutating walk can recurse
/// without holding the [`Body::for_each_child`] borrow across the mutation.
fn child_nodes(body: &Body, node: NodeRef) -> Vec<NodeRef> {
    let mut children = Vec::new();
    body.for_each_child(node, |c| children.push(c));
    children
}

/// Apply LICM to all loops in a block.
///
/// `outer_aliases` accumulates `let x = y` (and `&y` / `&mut y` / labeled-
/// or plain-block tail equivalents) pairs from let-statements that
/// precede each loop. The fixpoint loop in `licm_loop` consumes these so
/// that a write to one alias inside the loop body invalidates hoist
/// candidates targeting the other alias.
fn licm_block(
    engine: &mut Engine,
    block: BlockId,
    ctx: &mut LicmCtx,
    outer_aliases: &mut Vec<(u32, u32)>,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    // Iterate a clone, not `mem::take`: `hoist_invariant_arith` rebuilds
    // the value graph from the body root mid-walk, so ancestor blocks must
    // stay populated.
    for s in engine.body.blocks[block].stmts.clone() {
        // Classify without holding the borrow across the mutable recursion.
        let loop_body = match &engine.body.stmts[s].kind {
            StmtKind::Loop { body: lb } => Some(*lb),
            StmtKind::Let {
                local_index, value, ..
            } => {
                // Track outer-scope aliases so a subsequent loop's LICM can
                // see them.
                if let Some(ve) = value.as_expr()
                    && let Some(src_idx) = extract_alias_source(engine.body, ve)
                    && is_gc_heap_type(engine.body.exprs[ve].type_id, ctx.type_table)
                {
                    outer_aliases.push((*local_index, src_idx));
                }
                None
            }
            StmtKind::Expr(_)
            | StmtKind::Return { .. }
            | StmtKind::If { .. }
            | StmtKind::LabeledBlock { .. }
            | StmtKind::Break { .. }
            | StmtKind::Continue
            | StmtKind::LetDestructure { .. } => None,
        };

        if let Some(lb) = loop_body {
            let hoist_stmts = licm_loop(engine, lb, ctx, outer_aliases);
            if !hoist_stmts.is_empty() {
                changed = true;
            }
            new_stmts.extend(hoist_stmts);
        } else {
            // Recurse into every nested block — `if`/`match`/`switch` arms,
            // labeled blocks, expression blocks in a `let` value — so loops
            // anywhere under this statement are visited. Sharing the alias
            // accumulator across sibling branches is safe: aliasing is
            // monotone-correct (extra aliases only cause conservative misses,
            // never wrong hoists).
            changed |= licm_children(engine, NodeRef::Stmt(s), ctx, outer_aliases);
        }
        new_stmts.push(s);
    }

    engine.set_block_stmts(block, new_stmts);
    changed
}

/// Recurse `licm_block` into every block child under `node` (a statement or
/// expression), descending expression children to find them.
fn licm_children(
    engine: &mut Engine,
    node: NodeRef,
    ctx: &mut LicmCtx,
    outer_aliases: &mut Vec<(u32, u32)>,
) -> bool {
    let mut changed = false;
    for c in child_nodes(engine.body, node) {
        match c {
            NodeRef::Block(b) => changed |= licm_block(engine, b, ctx, outer_aliases),
            NodeRef::Expr(e) => {
                changed |= licm_children(engine, NodeRef::Expr(e), ctx, outer_aliases);
            }
            NodeRef::Pat(_) => {}
            NodeRef::Stmt(_) => panic!("statement child outside a block"),
        }
    }
    changed
}

/// Name prefix for every LICM-created hoist local. [`LicmCtx::new`] seeds its
/// `hoist_locals` set from it, recognizing hoisted handles persisting from a
/// prior LICM invocation so their clobbered-object sub-fields stay un-hoisted;
/// within a session the set itself is authoritative.
const LICM_HOIST_PREFIX: &str = "_licm_";

/// Apply LICM to a single loop, returning hoisting statement ids to prepend.
fn licm_loop(
    engine: &mut Engine,
    loop_body: BlockId,
    ctx: &mut LicmCtx,
    outer_aliases: &[(u32, u32)],
) -> Vec<StmtId> {
    let mut all_hoist_stmts = Vec::new();

    // Run LICM iteratively until no more candidates are found (second-level
    // hoisting), bounded to avoid pathological cases.
    const MAX_LICM_ITERATIONS: usize = 10;
    for _iteration in 0..MAX_LICM_ITERATIONS {
        // Step 1: Collect all variables modified in the loop.
        let mut modified_vars = ModifiedVars::default();
        for &(a, b) in outer_aliases {
            modified_vars.add_alias(a, b);
        }
        collect_modified_vars_in_block(engine.body, loop_body, &mut modified_vars, ctx.type_table);

        // Step 2: Collect immutable reference bindings for look-through, then
        // keep only bindings whose local provably holds `&source` at every
        // in-loop use: bound by exactly one `let` (a second binding could
        // retarget it) and never reassigned or `&mut`-escaped (`fully`).
        let mut ref_bindings =
            collect_immutable_ref_bindings(engine.body, loop_body, ctx.type_table);
        ref_bindings.retain(|local, binding| {
            binding.let_count == 1 && !modified_vars.fully.contains(local)
        });

        // Step 3: Find field accesses that can be hoisted, deduped by
        // (local, field); locals are allocated in step 4.
        let mut candidates = Vec::new();
        let mut seen = IndexSet::default();
        find_hoist_candidates(
            engine.body,
            NodeRef::Block(loop_body),
            &modified_vars,
            &ref_bindings,
            &mut candidates,
            &mut seen,
        );

        // Step 3.5: Drop `x.f` candidates that would be unsound to hoist:
        // (a) `x` is a struct reference whose pointee field is aliasing-written;
        // (b) `x` is a LICM hoist local (an aliasing handle from a prior
        //     iteration) whose GC-heap object is `&mut`-clobbered in the loop, so
        //     its sub-fields change opaquely (#1472 cascade). Pre-existing roots
        //     are not subject to (b): a read-only `&List` lookup must stay
        //     hoistable even when a same-typed list is mutated nearby.
        candidates.retain(|c| {
            let locals = engine.locals();
            let root_ty = if (c.local_index as usize) < locals.len() {
                locals[c.local_index as usize].type_id
            } else {
                c.type_id
            };
            if modified_vars.is_reference_field_aliasing_written(
                root_ty,
                c.field_index,
                ctx.type_table,
            ) {
                return false;
            }
            let is_hoist_local = ctx.hoist_locals.contains(&c.local_index);
            !(is_hoist_local && modified_vars.is_clobbered_gc_value(root_ty, ctx.type_table))
        });

        if candidates.is_empty() {
            // Field-hoisting has converged for this loop. Try hoisting maximal
            // pre-header-stable pure-arithmetic subexpressions (e.g. the
            // `_licm_end - _licm_start` a scan loop recomputes in its guard
            // every iteration). Runs here, after field-hoisting, so the
            // `_licm_*` locals it created are visible as stable operands.
            if hoist_invariant_arith(engine, loop_body, &modified_vars, &mut all_hoist_stmts, ctx) {
                continue;
            }
            break;
        }

        // Step 4: Create hoisting statements. Each candidate gets its local
        // from `engine.alloc_local` (which also pushes the `NirLocal` entry),
        // so the surviving hoist locals are contiguous from the function's
        // current local count. The allocated name travels with the
        // replacement so every rewritten read reuses it verbatim.
        let mut replacements = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let local_type_id = {
                let locals = engine.locals();
                if (candidate.local_index as usize) < locals.len() {
                    locals[candidate.local_index as usize].type_id
                } else {
                    candidate.type_id
                }
            };

            let hoist_name = format!(
                "{LICM_HOIST_PREFIX}{}_{}",
                candidate.field_name,
                engine.locals().len()
            );
            let new_local_index = engine.alloc_local(
                hoist_name.clone(),
                candidate.type_id,
                /* is_mut */ false,
            );
            ctx.hoist_locals.insert(new_local_index);

            // Build `local.field` as fresh arena nodes via the engine.
            let local_expr = engine.alloc_expr(
                ExprKind::Local {
                    index: candidate.local_index,
                    name: candidate.local_name.clone(),
                },
                local_type_id,
                Span::new(0, 0, 0, 0),
            );
            let field_access_expr = engine.alloc_expr(
                ExprKind::FieldAccess {
                    expr: local_expr.into(),
                    field_index: candidate.field_index,
                    field_name: candidate.field_name.clone(),
                },
                candidate.type_id,
                Span::new(0, 0, 0, 0),
            );
            let hoist_stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name: hoist_name.clone(),
                    local_index: new_local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: candidate.type_id,
                    value: field_access_expr.into(),
                    skip_value_copy: true,
                },
                Span::new(0, 0, 0, 0),
            );
            all_hoist_stmts.push(hoist_stmt);
            replacements.push(HoistedField {
                local_index: candidate.local_index,
                field_index: candidate.field_index,
                new_local_index,
                name: hoist_name,
            });
        }

        // Step 5: Replace field accesses in the loop body with the hoisted locals.
        replace_hoisted(
            engine,
            NodeRef::Block(loop_body),
            &replacements,
            &ref_bindings,
        );
    }

    // Step 6: reloadable field-load hoisting. A field read `s.f` whose only
    // obstacle is an opaque `&mut`-call clobber of `s`'s pointee type (a *may*
    // alias the compiler cannot rule out, e.g. `write_escaped_string(&mut buf,
    // &s)` where a caller could pass `buf === s`) is loop-invariant on every
    // clobber-free path. Hoist it to the pre-header and re-load it right after
    // each clobbering statement, so the common (clobber-free) path reads the
    // pre-header local while a genuine alias still observes the fresh field.
    hoist_reloadable_field_loads(engine, loop_body, ctx, outer_aliases, &mut all_hoist_stmts);

    // Nested loops: recurse. The nested `licm_block` accumulates aliases from
    // the outer loop's `let` statements on its own walk.
    let mut nested_aliases: Vec<(u32, u32)> = outer_aliases.to_vec();
    licm_block(engine, loop_body, ctx, &mut nested_aliases);

    all_hoist_stmts
}

/// Type of the source local a candidate reads, falling back to the field type
/// for a synthetic (not-yet-allocated) local index.
fn candidate_root_ty(engine: &Engine, local_index: u32, fallback: TypeId) -> TypeId {
    let locals = engine.locals();
    if (local_index as usize) < locals.len() {
        locals[local_index as usize].type_id
    } else {
        fallback
    }
}

/// Hoist field loads blocked *only* by an opaque `&mut`-call clobber of their
/// pointee type, reloading them after each clobbering statement. See the call
/// site in [`licm_loop`] for the rationale.
fn hoist_reloadable_field_loads(
    engine: &mut Engine,
    loop_body: BlockId,
    ctx: &mut LicmCtx,
    outer_aliases: &[(u32, u32)],
    all_hoist_stmts: &mut Vec<StmtId>,
) {
    let mut modified_vars = ModifiedVars::default();
    for &(a, b) in outer_aliases {
        modified_vars.add_alias(a, b);
    }
    collect_modified_vars_in_block(engine.body, loop_body, &mut modified_vars, ctx.type_table);

    // No opaque `&mut`-call clobber of any struct pointee means no reloadable
    // candidate can exist — skip the ref-binding and candidate walks entirely
    // (the common case for loops without a same-typed mutating call).
    if modified_vars.clobbered_pointee_types.is_empty() {
        return;
    }

    let mut ref_bindings = collect_immutable_ref_bindings(engine.body, loop_body, ctx.type_table);
    ref_bindings
        .retain(|local, binding| binding.let_count == 1 && !modified_vars.fully.contains(local));

    let mut candidates = Vec::new();
    let mut seen = IndexSet::default();
    find_hoist_candidates(
        engine.body,
        NodeRef::Block(loop_body),
        &modified_vars,
        &ref_bindings,
        &mut candidates,
        &mut seen,
    );

    // Keep only candidates whose sole obstacle is an opaque `&mut`-call clobber
    // of a struct pointee: not directly field-written, and with a genuine
    // (non-reload) read still present in the loop.
    candidates.retain(|c| {
        let root_ty = candidate_root_ty(engine, c.local_index, c.type_id);
        let Some(pointee) = reloadable_pointee(root_ty, ctx.type_table) else {
            return false;
        };
        if !modified_vars.clobbered_pointee_types.contains(&pointee) {
            return false;
        }
        if modified_vars
            .written_field_types
            .contains(&(pointee, c.field_index))
        {
            return false;
        }
        count_genuine_field_reads(
            engine.body,
            NodeRef::Block(loop_body),
            c.local_index,
            c.field_index,
            &ctx.hoist_locals,
        ) > 0
    });

    if candidates.is_empty() {
        return;
    }

    // The pointee types whose clobbers force a reload.
    let mut clobber_types: IndexSet<TypeId> = IndexSet::default();
    for c in &candidates {
        let root_ty = candidate_root_ty(engine, c.local_index, c.type_id);
        if let Some(p) = reloadable_pointee(root_ty, ctx.type_table) {
            clobber_types.insert(p);
        }
    }

    // Soundness gate: no statement both clobbers and reads a hoisted field.
    // `replace_hoisted` rewrites reads through immutable ref-bindings too
    // (`r.field` where `r` aliases the source), so the gate must consider those
    // aliases — a clobbering statement reading `r.field` after the clobber would
    // otherwise observe a stale hoisted local.
    let mut read_specs: Vec<(u32, u32)> = candidates
        .iter()
        .map(|c| (c.local_index, c.field_index))
        .collect();
    for (&ref_local, binding) in &ref_bindings {
        for c in &candidates {
            if binding.source_index == c.local_index {
                read_specs.push((ref_local, c.field_index));
            }
        }
    }
    if !reload_gate_ok(
        engine.body,
        loop_body,
        &read_specs,
        &clobber_types,
        ctx.type_table,
    ) {
        return;
    }
    // A clobbering call that is the value-producing tail of a value-block would
    // have its (non-unit) value dropped by an appended reload — bail on those.
    if has_nonunit_clobber_value_tail(
        engine.body,
        NodeRef::Block(loop_body),
        &clobber_types,
        ctx.type_table,
    ) {
        return;
    }

    // Materialise the hoists (mutable, so reloads can rebind them).
    let mut specs = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        let local_type_id = candidate_root_ty(engine, candidate.local_index, candidate.type_id);
        let pointee = reloadable_pointee(local_type_id, ctx.type_table)
            .expect("retained candidate has a reloadable struct pointee");
        let hoist_name = format!(
            "{LICM_HOIST_PREFIX}{}_{}",
            candidate.field_name,
            engine.locals().len()
        );
        let new_local_index = engine.alloc_local(
            hoist_name.clone(),
            candidate.type_id,
            /* is_mut */ true,
        );
        ctx.hoist_locals.insert(new_local_index);

        let hoist_value = build_field_access(
            engine,
            candidate.local_index,
            &candidate.local_name,
            local_type_id,
            candidate.field_index,
            &candidate.field_name,
            candidate.type_id,
        );
        let hoist_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name: hoist_name.clone(),
                local_index: new_local_index,
                is_mut: true,
                is_reactive: false,
                type_id: candidate.type_id,
                value: hoist_value.into(),
                skip_value_copy: true,
            },
            Span::new(0, 0, 0, 0),
        );
        all_hoist_stmts.push(hoist_stmt);
        specs.push(ReloadSpec {
            source_local: candidate.local_index,
            source_name: candidate.local_name.clone(),
            source_type: local_type_id,
            field_index: candidate.field_index,
            field_name: candidate.field_name.clone(),
            field_type: candidate.type_id,
            pointee,
            hoist_local: new_local_index,
            hoist_name,
        });
    }

    // Replace genuine reads with the hoisted locals, *then* insert reloads (so
    // the reload's own `source.field` read is not itself rewritten to the local).
    let hoisted: Vec<HoistedField> = specs
        .iter()
        .map(|s| HoistedField {
            local_index: s.source_local,
            field_index: s.field_index,
            new_local_index: s.hoist_local,
            name: s.hoist_name.clone(),
        })
        .collect();
    replace_hoisted(engine, NodeRef::Block(loop_body), &hoisted, &ref_bindings);
    insert_reloads(engine, loop_body, &specs, &clobber_types, ctx.type_table);
}

/// A field load hoisted with reload-after-clobber: the pre-header local
/// `hoist_local` serves `source.field`, and a `hoist_local = source.field`
/// reload is emitted after each clobbering statement.
struct ReloadSpec {
    source_local: u32,
    source_name: String,
    source_type: TypeId,
    field_index: u32,
    field_name: String,
    field_type: TypeId,
    /// The struct pointee type whose clobbers require this field to be reloaded.
    pointee: TypeId,
    hoist_local: u32,
    hoist_name: String,
}

/// Build a fresh `source.field` field-access expression.
fn build_field_access(
    engine: &mut Engine,
    local_index: u32,
    local_name: &str,
    local_type_id: TypeId,
    field_index: u32,
    field_name: &str,
    field_type_id: TypeId,
) -> ExprId {
    let local_expr = engine.alloc_expr(
        ExprKind::Local {
            index: local_index,
            name: local_name.to_string(),
        },
        local_type_id,
        Span::new(0, 0, 0, 0),
    );
    engine.alloc_expr(
        ExprKind::FieldAccess {
            expr: local_expr.into(),
            field_index,
            field_name: field_name.to_string(),
        },
        field_type_id,
        Span::new(0, 0, 0, 0),
    )
}

/// Append `hoist_local = source.field` reload statements after every
/// bare-statement clobbering call under `block` (recursing into nested blocks).
fn insert_reloads(
    engine: &mut Engine,
    block: BlockId,
    specs: &[ReloadSpec],
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) {
    // Recurse into nested blocks first.
    let stmts = engine.body.blocks[block].stmts.clone();
    for &s in &stmts {
        let mut child_blocks = Vec::new();
        collect_child_blocks(engine.body, NodeRef::Stmt(s), &mut child_blocks);
        for b in child_blocks {
            insert_reloads(engine, b, specs, clobber_types, type_table);
        }
    }

    let mut new_stmts: Vec<StmtId> = Vec::with_capacity(stmts.len());
    let mut changed = false;
    for s in stmts {
        new_stmts.push(s);
        // The pointee types this statement actually clobbers; only fields of
        // those types can have gone stale, so reload just their specs.
        let hit = node_clobbered_types(engine.body, NodeRef::Stmt(s), clobber_types, type_table);
        if !hit.is_empty() {
            for spec in specs.iter().filter(|sp| hit.contains(&sp.pointee)) {
                let value = build_field_access(
                    engine,
                    spec.source_local,
                    &spec.source_name,
                    spec.source_type,
                    spec.field_index,
                    &spec.field_name,
                    spec.field_type,
                );
                let target = engine.alloc_expr(
                    ExprKind::Local {
                        index: spec.hoist_local,
                        name: spec.hoist_name.clone(),
                    },
                    spec.field_type,
                    Span::new(0, 0, 0, 0),
                );
                let assign = engine.alloc_expr(
                    ExprKind::Assign {
                        target,
                        value: value.into(),
                    },
                    TypeTable::UNIT,
                    Span::new(0, 0, 0, 0),
                );
                let reload =
                    engine.alloc_stmt(StmtKind::Expr(assign.into()), Span::new(0, 0, 0, 0));
                new_stmts.push(reload);
                changed = true;
            }
        }
    }
    if changed {
        engine.set_block_stmts(block, new_stmts);
    }
}

/// Collect the block ids nested directly inside a statement (through its
/// expression children), without descending into those blocks.
fn collect_child_blocks(body: &Body, node: NodeRef, out: &mut Vec<BlockId>) {
    body.for_each_child(node, |c| match c {
        NodeRef::Block(b) => out.push(b),
        NodeRef::Expr(_) | NodeRef::Stmt(_) => collect_child_blocks(body, c, out),
        NodeRef::Pat(_) => {}
    });
}

/// Pointee type of a reference-typed local, when it is a plain `struct` that a
/// `&mut` call could opaquely clobber. Returns `None` for value-typed locals or
/// non-struct pointees.
fn reloadable_pointee(root_type: TypeId, type_table: &TypeTable) -> Option<TypeId> {
    match type_table.get(root_type) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            let pointee = strip_references(*inner, type_table);
            matches!(type_table.get(pointee), ResolvedType::Struct { .. }).then_some(pointee)
        }
        _ => None,
    }
}

/// True when `e` is a `Call`/`MethodCall` that passes a `&mut T` argument whose
/// pointee `T` is in `clobber_types` — i.e. the call may write that pointee's
/// fields. Mirrors [`record_mut_ref_clobber`]'s `&mut` detection.
fn expr_clobbers_types(
    body: &Body,
    e: ExprId,
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> bool {
    let args: &[ArenaCallArg] = match &body.exprs[e].kind {
        ExprKind::Call { args, .. } => args,
        ExprKind::MethodCall { receiver, args, .. } => {
            if let Some(re) = receiver.as_expr()
                && expr_type_clobbers(body, re, clobber_types, type_table)
            {
                return true;
            }
            args
        }
        _ => return false,
    };
    args.iter().any(|a| {
        a.expr
            .as_expr()
            .is_some_and(|ae| expr_type_clobbers(body, ae, clobber_types, type_table))
    })
}

/// True when some value-producing block under `node` has a last statement that
/// is a non-unit clobbering call. `insert_reloads` appends a reload (which
/// yields unit) after such a tail, replacing the block's observed value — so a
/// non-unit value-tail clobber makes the loop ineligible. A `Block` child of an
/// *expression* is value-producing (its tail is the value); a `Block` child of
/// a *statement* (loop body, `if`-statement arm) discards its tail, so appending
/// there is harmless and stays eligible.
fn has_nonunit_clobber_value_tail(
    body: &Body,
    node: NodeRef,
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> bool {
    if let NodeRef::Expr(_) = node {
        let mut hit = false;
        body.for_each_child(node, |c| {
            if hit {
                return;
            }
            if let NodeRef::Block(b) = c
                && let Some(&last) = body.blocks[b].stmts.last()
                && let StmtKind::Expr(Operand::Expr(e)) = &body.stmts[last].kind
                && expr_clobbers_types(body, *e, clobber_types, type_table)
                && body.exprs[*e].type_id != TypeTable::UNIT
            {
                hit = true;
            }
        });
        if hit {
            return true;
        }
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found && !matches!(c, NodeRef::Pat(_)) {
            found = has_nonunit_clobber_value_tail(body, c, clobber_types, type_table);
        }
    });
    found
}

/// The subset of `clobber_types` a statement clobbers, viewing only its own
/// expression tree (block-stopping, mirroring [`node_contains_clobber`]) so a
/// reload placed after this statement targets exactly the fields that may have
/// gone stale.
fn node_clobbered_types(
    body: &Body,
    node: NodeRef,
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> IndexSet<TypeId> {
    let mut hit = IndexSet::default();
    collect_clobbered_types(body, node, clobber_types, type_table, &mut hit);
    hit
}

fn collect_clobbered_types(
    body: &Body,
    node: NodeRef,
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
    hit: &mut IndexSet<TypeId>,
) {
    if let NodeRef::Expr(e) = node {
        let operands: &[ArenaCallArg] = match &body.exprs[e].kind {
            ExprKind::Call { args, .. } => args,
            ExprKind::MethodCall { receiver, args, .. } => {
                if let Some(re) = receiver.as_expr()
                    && let Some(t) = mut_ref_pointee(body, re, clobber_types, type_table)
                {
                    hit.insert(t);
                }
                args
            }
            _ => &[],
        };
        for a in operands {
            if let Some(ae) = a.expr.as_expr()
                && let Some(t) = mut_ref_pointee(body, ae, clobber_types, type_table)
            {
                hit.insert(t);
            }
        }
    }
    body.for_each_child(node, |c| {
        if !matches!(c, NodeRef::Pat(_) | NodeRef::Block(_)) {
            collect_clobbered_types(body, c, clobber_types, type_table, hit);
        }
    });
}

/// The pointee `T` of `e` when `e` has type `&mut T` and `T ∈ clobber_types`.
fn mut_ref_pointee(
    body: &Body,
    e: ExprId,
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> Option<TypeId> {
    let mut ty = body.exprs[e].type_id;
    let mut saw_mut = false;
    loop {
        match type_table.get(ty) {
            ResolvedType::MutRef(inner) => {
                saw_mut = true;
                ty = *inner;
            }
            ResolvedType::Ref(inner) => ty = *inner,
            _ => break,
        }
    }
    (saw_mut && clobber_types.contains(&ty)).then_some(ty)
}

fn expr_type_clobbers(
    body: &Body,
    e: ExprId,
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> bool {
    mut_ref_pointee(body, e, clobber_types, type_table).is_some()
}

/// True when the expression tree under `node` — *without crossing into nested
/// blocks* — contains a clobbering call for a `clobber_type`. Nested blocks are
/// separate reload units handled by [`insert_reloads`]/[`reload_gate_ok`]
/// recursion, so stopping at block boundaries keeps the per-statement view
/// precise (a labeled block grouping a read and a clobber is not treated as one
/// clobber-and-read statement).
fn node_contains_clobber(
    body: &Body,
    node: NodeRef,
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> bool {
    if let NodeRef::Expr(e) = node
        && expr_clobbers_types(body, e, clobber_types, type_table)
    {
        return true;
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found && !matches!(c, NodeRef::Pat(_) | NodeRef::Block(_)) {
            found = node_contains_clobber(body, c, clobber_types, type_table);
        }
    });
    found
}

/// True when the tree under `node` reads `source_local.field_index` as a field
/// access. `cross_blocks` controls whether the walk descends into nested blocks;
/// True when `e` is directly `source.field` for one of the `(source, field)`
/// specs (a single node, not a subtree walk).
fn expr_is_spec_read(body: &Body, e: ExprId, specs: &[(u32, u32)]) -> bool {
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[e].kind
        && let Some(ie) = inner.as_expr()
        && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
    {
        specs.iter().any(|&(src, fld)| src == *index && fld == *field_index)
    } else {
        false
    }
}

/// Soundness gate. A hoisted local goes stale the moment a clobber runs and
/// stays stale until the next reload, which `insert_reloads` emits *after* each
/// statement carrying a direct clobber. So the only unsound read is one
/// evaluated after a clobber but before that statement ends. This walks the
/// loop in evaluation order threading a `poison` flag (set at a clobbering call,
/// which happens after its own arguments; cleared after a direct-clobber
/// statement's reload) and rejects if a field read is reached while poisoned. A
/// read inside a clobbering call's arguments stays safe (evaluated first), so
/// e.g. `traverse(node.children[i], …)` remains hoistable.
fn reload_gate_ok(
    body: &Body,
    block: BlockId,
    specs: &[(u32, u32)],
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> bool {
    !gate_eval_block(body, block, false, specs, clobber_types, type_table).0
}

/// Sequence `block`'s statements, threading poison; a direct-clobber statement's
/// trailing reload clears poison for the rest. Returns
/// `(found_stale_read, poison_after_block)`.
fn gate_eval_block(
    body: &Body,
    block: BlockId,
    mut poison: bool,
    specs: &[(u32, u32)],
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> (bool, bool) {
    for &s in &body.blocks[block].stmts {
        let (bad, p) = gate_eval_node(
            body,
            NodeRef::Stmt(s),
            poison,
            specs,
            clobber_types,
            type_table,
        );
        if bad {
            return (true, poison);
        }
        poison = if node_contains_clobber(body, NodeRef::Stmt(s), clobber_types, type_table) {
            false
        } else {
            p
        };
    }
    (false, poison)
}

/// Evaluate a node's subtree in evaluation order (operand children before the
/// node's own operation; nested blocks via [`gate_eval_block`] so their trailing
/// reloads are modelled). Returns `(found_stale_read, poison_after)`.
fn gate_eval_node(
    body: &Body,
    node: NodeRef,
    mut poison: bool,
    specs: &[(u32, u32)],
    clobber_types: &IndexSet<TypeId>,
    type_table: &TypeTable,
) -> (bool, bool) {
    if let NodeRef::Block(b) = node {
        return gate_eval_block(body, b, poison, specs, clobber_types, type_table);
    }
    let mut children = Vec::new();
    body.for_each_child(node, |c| {
        if !matches!(c, NodeRef::Pat(_)) {
            children.push(c);
        }
    });
    for c in children {
        let (bad, p) = gate_eval_node(body, c, poison, specs, clobber_types, type_table);
        if bad {
            return (true, poison);
        }
        poison = p;
    }
    if let NodeRef::Expr(e) = node {
        if poison && expr_is_spec_read(body, e, specs) {
            return (true, poison);
        }
        if expr_clobbers_types(body, e, clobber_types, type_table) {
            poison = true;
        }
    }
    (false, poison)
}

/// Count field reads `source.field` under `node` that are *genuine* uses, i.e.
/// not the value of a reload assignment `_licm_x = source.field` a prior run
/// (or an earlier candidate) already inserted. Zero genuine reads means the
/// hoist already happened — skipping keeps the rule idempotent across the
/// optimizer's fixed-point iterations.
fn count_genuine_field_reads(
    body: &Body,
    node: NodeRef,
    source_local: u32,
    field_index: u32,
    hoist_locals: &IndexSet<u32>,
) -> usize {
    // Skip the value of a reload `Assign { target: Local(hoist), value: … }` —
    // its field read is bookkeeping, not a use to optimise.
    if let NodeRef::Expr(e) = node
        && let ExprKind::Assign { target, value } = &body.exprs[e].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
        && hoist_locals.contains(index)
    {
        let _ = value;
        return 0;
    }
    let mut n = 0;
    if let NodeRef::Expr(e) = node
        && let ExprKind::FieldAccess {
            expr: inner,
            field_index: fi,
            ..
        } = &body.exprs[e].kind
        && *fi == field_index
        && let Some(ie) = inner.as_expr()
        && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
        && *index == source_local
    {
        n += 1;
    }
    body.for_each_child(node, |c| {
        if !matches!(c, NodeRef::Pat(_)) {
            n += count_genuine_field_reads(body, c, source_local, field_index, hoist_locals);
        }
    });
    n
}

// ---------------------------------------------------------------------------
// Modified-variable collection (special-cased walk)
// ---------------------------------------------------------------------------

fn collect_modified_vars_in_block(
    body: &Body,
    block: BlockId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    for s in &body.blocks[block].stmts {
        collect_modified_vars_in_stmt(body, *s, modified, type_table);
    }
}

fn mark_gc_local_as_fully_modified_operand(
    body: &Body,
    op: Operand,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let Some(e) = op.as_expr() {
        mark_gc_local_as_fully_modified(body, e, modified, type_table);
    }
}

/// Mark a local as fully modified if it has a GC struct type and is passed to a
/// function call (callees can mutate any field). Immutable `&T` locals are
/// skipped — no callee can mutate the pointee through them.
fn mark_gc_local_as_fully_modified(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let ExprKind::Local { index, .. } = &body.exprs[e].kind
        && is_gc_heap_type(body.exprs[e].type_id, type_table)
    {
        if let ResolvedType::Ref(inner) = type_table.get(body.exprs[e].type_id)
            && !matches!(type_table.get(*inner), ResolvedType::MutRef(_))
        {
            return;
        }
        modified.insert_full(*index);
    }
}

/// Walk through reference wrappers and tail-return blocks to find the source
/// local a let-binding aliases. Alias-precision-only: missing an alias is a
/// soundness bug, extra aliases are at worst a missed optimisation.
fn extract_alias_source(body: &Body, e: ExprId) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => inner
            .as_expr()
            .and_then(|ie| extract_alias_source(body, ie)),
        ExprKind::Block(block) => {
            let tail = *body.blocks[*block].stmts.last()?;
            let StmtKind::Expr(Operand::Expr(tail_expr)) = &body.stmts[tail].kind else {
                return None;
            };
            extract_alias_source(body, *tail_expr)
        }
        ExprKind::LabeledBlock { label, block, .. } => {
            let last = *body.blocks[*block].stmts.last()?;
            let StmtKind::Break {
                label: Some(brk_label),
                value: Some(brk_value),
            } = &body.stmts[last].kind
            else {
                return None;
            };
            if brk_label != label {
                return None;
            }
            brk_value
                .as_expr()
                .and_then(|e| extract_alias_source(body, e))
        }
        _ => None,
    }
}

/// Check if a type is a GC heap type whose fields can be mutated by a callee.
fn is_gc_heap_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            is_gc_heap_type(*inner, type_table)
        }
        _ => false,
    }
}

fn mark_local_as_fully_modified_operand(body: &Body, op: Operand, modified: &mut ModifiedVars) {
    if let Some(e) = op.as_expr() {
        mark_local_as_fully_modified(body, e, modified);
    }
}

/// Mark a local as fully modified, traversing through unary ops and nested
/// field accesses to the root.
fn mark_local_as_fully_modified(body: &Body, e: ExprId, modified: &mut ModifiedVars) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            modified.insert_full(*index);
        }
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Unary { expr: inner, .. } => {
            mark_local_as_fully_modified_operand(body, *inner, modified);
        }
        _ => {}
    }
}

/// A chain of field accesses bottoming out at a `Local` (`a`, `a.b`, `a.b.c`),
/// with no `Index`, deref, or call.
fn is_pure_field_chain(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        // A promoted `Operand::Value` receiver is not a pure local-read chain.
        ExprKind::FieldAccess { expr: inner, .. } => inner
            .as_expr()
            .is_some_and(|e| is_pure_field_chain(body, e)),
        _ => false,
    }
}

/// Strip all `Ref`/`MutRef` wrappers, returning the pointee type.
fn strip_references(type_id: TypeId, type_table: &TypeTable) -> TypeId {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            strip_references(*inner, type_table)
        }
        _ => type_id,
    }
}

fn record_mut_ref_clobber_operand(
    body: &Body,
    op: Operand,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let Some(e) = op.as_expr() {
        record_mut_ref_clobber(body, e, modified, type_table);
    }
}

/// If `expr` is a `&mut`-reference to a heap object passed to a call, record its
/// pointee as clobbered. Covers both plain structs and generic instances
/// (`List<T>`, `String`, …) — a `&mut List<i32>` method like `push` mutates the
/// pointee just as a `&mut Node` method does (issue #1472).
fn record_mut_ref_clobber(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    let mut ty = body.exprs[e].type_id;
    let mut saw_mut = false;
    loop {
        match type_table.get(ty) {
            ResolvedType::MutRef(inner) => {
                saw_mut = true;
                ty = *inner;
            }
            ResolvedType::Ref(inner) => ty = *inner,
            _ => break,
        }
    }
    if saw_mut
        && matches!(
            type_table.get(ty),
            ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
        )
    {
        modified.insert_clobbered_pointee_type(ty);
    }
}

/// Record a field-access write into `written_field_types`, keyed by the pointee
/// type of the assigned object.
fn record_written_field_type(
    body: &Body,
    target: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[target].kind
        // A write place's receiver is never a promoted `Operand::Value`.
        && let Some(inner_e) = inner.as_expr()
    {
        let pointee = strip_references(body.exprs[inner_e].type_id, type_table);
        modified.insert_written_field_type(pointee, *field_index);
    }
}

/// Mark what is modified by an assignment target.
fn mark_assignment_target_as_modified(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            modified.insert_full(*index);
        }
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let inner = *inner;
            let field_index = *field_index;
            record_written_field_type(body, e, modified, type_table);
            if let Some(inner_e) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[inner_e].kind
            {
                modified.insert_field(*index, field_index);
            } else if inner
                .as_expr()
                .is_some_and(|ie| is_pure_field_chain(body, ie))
            {
                // `a.b.c = x` mutates `*a.b`, not a field of the root `a`.
            } else {
                // A promoted-value receiver (or other shape) falls back to the
                // conservative whole-local invalidation.
                mark_local_as_fully_modified_operand(body, inner, modified);
            }
        }
        ExprKind::Unary { expr: inner, .. } => {
            mark_local_as_fully_modified_operand(body, *inner, modified);
        }
        _ => {}
    }
}

fn collect_modified_vars_in_stmt(
    body: &Body,
    s: StmtId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &body.stmts[s].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let local_index = *local_index;
            let value = *value;
            // An alias-shaped `let` (a GC reference copy) rebinds only the
            // local slot: record it as `rebound` plus an alias edge, so the
            // source's fields stay hoistable while anything rooted at the
            // rebound local (or written through it) is still blocked. Any
            // other `let` fully modifies the bound local.
            if let Some(ve) = value.as_expr()
                && let Some(src_idx) = extract_alias_source(body, ve)
                && is_gc_heap_type(body.exprs[ve].type_id, type_table)
            {
                modified.insert_rebound(local_index);
                modified.add_alias(local_index, src_idx);
            } else {
                modified.insert_full(local_index);
            }
            collect_modified_vars_in_operand(body, value, modified, type_table);
        }
        StmtKind::Expr(expr) => {
            collect_modified_vars_in_operand(body, *expr, modified, type_table);
        }
        StmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_vars_in_operand(body, *v, modified, type_table);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let condition = *condition;
            let then_block = *then_block;
            let else_block = *else_block;
            collect_modified_vars_in_operand(body, condition, modified, type_table);
            collect_modified_vars_in_block(body, then_block, modified, type_table);
            if let Some(eb) = else_block {
                collect_modified_vars_in_block(body, eb, modified, type_table);
            }
        }
        StmtKind::Loop { body: lb } => {
            collect_modified_vars_in_block(body, *lb, modified, type_table);
        }
        StmtKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(body, *block, modified, type_table);
        }
        StmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_modified_vars_in_operand(body, *v, modified, type_table);
            }
        }
        StmtKind::Continue => {}
        StmtKind::LetDestructure { pattern, value, .. } => {
            let pattern = *pattern;
            let value = *value;
            collect_pattern_bindings(body, pattern, modified);
            collect_modified_vars_in_operand(body, value, modified, type_table);
        }
    }
}

/// Collect all local variable indices bound by a pattern.
fn collect_pattern_bindings(
    body: &Body,
    pat: crate::nir_arena::PatId,
    modified: &mut ModifiedVars,
) {
    match &body.pats[pat].kind {
        PatKind::Binding { local_index, .. } => {
            modified.insert_full(*local_index);
        }
        PatKind::Variant { bindings, .. } => {
            let bindings = bindings.clone();
            for b in bindings {
                collect_pattern_bindings(body, b, modified);
            }
        }
        PatKind::Tuple(patterns, _) => {
            let patterns = patterns.clone();
            for p in patterns {
                collect_pattern_bindings(body, p, modified);
            }
        }
        PatKind::Struct { fields, .. } => {
            let fields: Vec<_> = fields.iter().map(|f| f.pattern).collect();
            for p in fields {
                collect_pattern_bindings(body, p, modified);
            }
        }
        PatKind::Or(alternatives) => {
            let alternatives = alternatives.clone();
            for p in alternatives {
                collect_pattern_bindings(body, p, modified);
            }
        }
        PatKind::Wildcard
        | PatKind::Literal(_)
        | PatKind::Enum { .. }
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => {}
    }
}

fn collect_modified_vars_in_operand(
    body: &Body,
    op: Operand,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let Some(e) = op.as_expr() {
        collect_modified_vars_in_expr(body, e, modified, type_table);
    }
}

fn collect_modified_vars_in_expr(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &body.exprs[e].kind {
        ExprKind::Assign { target, value } => {
            let target = *target;
            let value = *value;
            mark_assignment_target_as_modified(body, target, modified, type_table);
            collect_modified_vars_in_expr(body, target, modified, type_table);
            collect_modified_vars_in_operand(body, value, modified, type_table);
        }
        ExprKind::Binary { left, right, .. } => {
            let left = *left;
            let right = *right;
            collect_modified_vars_in_operand(body, left, modified, type_table);
            collect_modified_vars_in_operand(body, right, modified, type_table);
        }
        ExprKind::Unary { op, expr: inner } => {
            let inner = *inner;
            if let Some(ie) = inner.as_expr()
                && matches!(op, NirUnaryOp::MutRef)
                && matches!(body.exprs[ie].kind, ExprKind::Local { .. })
            {
                mark_local_as_fully_modified(body, ie, modified);
            }
            collect_modified_vars_in_operand(body, inner, modified, type_table);
        }
        ExprKind::Cast { expr: inner, .. } => {
            collect_modified_vars_in_operand(body, *inner, modified, type_table);
        }
        ExprKind::Call { args, .. } => {
            let arg_ids: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
            for a in arg_ids {
                mark_gc_local_as_fully_modified(body, a, modified, type_table);
                record_mut_ref_clobber(body, a, modified, type_table);
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
            mark_gc_local_as_fully_modified_operand(body, receiver, modified, type_table);
            record_mut_ref_clobber_operand(body, receiver, modified, type_table);
            collect_modified_vars_in_operand(body, receiver, modified, type_table);
            for a in arg_ids {
                mark_gc_local_as_fully_modified(body, a, modified, type_table);
                record_mut_ref_clobber(body, a, modified, type_table);
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let arg_ids = args.clone();
            for a in arg_ids {
                collect_modified_vars_in_operand(body, a, modified, type_table);
            }
        }
        ExprKind::FieldAccess { expr: inner, .. } => {
            collect_modified_vars_in_operand(body, *inner, modified, type_table);
        }
        ExprKind::Index { expr: inner, index } => {
            let inner = *inner;
            let index = *index;
            collect_modified_vars_in_operand(body, inner, modified, type_table);
            collect_modified_vars_in_operand(body, index, modified, type_table);
        }
        ExprKind::Block(block) => {
            collect_modified_vars_in_block(body, *block, modified, type_table);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let condition = *condition;
            let then_branch = *then_branch;
            let else_branch = *else_branch;
            collect_modified_vars_in_operand(body, condition, modified, type_table);
            collect_modified_vars_in_block(body, then_branch, modified, type_table);
            if let Some(eb) = else_branch {
                collect_modified_vars_in_block(body, eb, modified, type_table);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            let vals: Vec<ExprId> = fields.iter().filter_map(|f| f.value.as_expr()).collect();
            for v in vals {
                collect_modified_vars_in_expr(body, v, modified, type_table);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            for el in elements {
                collect_modified_vars_in_operand(body, el, modified, type_table);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let arg_ids = args.clone();
            collect_modified_vars_in_operand(body, callee, modified, type_table);
            for a in arg_ids {
                mark_gc_local_as_fully_modified_operand(body, a, modified, type_table);
                collect_modified_vars_in_operand(body, a, modified, type_table);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            collect_modified_vars_in_operand(body, *functor, modified, type_table);
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_modified_vars_in_operand(body, *p, modified, type_table);
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(body, *block, modified, type_table);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            collect_modified_vars_in_operand(body, *value, modified, type_table);
        }
        ExprKind::VariantTag { expr } | ExprKind::VariantTest { expr, .. } => {
            collect_modified_vars_in_operand(body, *expr, modified, type_table);
        }
        ExprKind::VariantPayload { expr, .. } => {
            collect_modified_vars_in_operand(body, *expr, modified, type_table);
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            collect_modified_vars_in_operand(body, scrutinee, modified, type_table);
            for arm in arms {
                collect_modified_vars_in_block(body, arm, modified, type_table);
            }
            collect_modified_vars_in_block(body, default, modified, type_table);
        }
        ExprKind::PackedArray(_)
        | ExprKind::Dead
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => {}
        ExprKind::Match { expr, arms } => {
            let expr = *expr;
            let arm_data: Vec<(crate::nir_arena::PatId, Option<ExprId>, Option<ExprId>)> = arms
                .iter()
                .map(|a| {
                    (
                        a.pattern,
                        a.guard.and_then(Operand::as_expr),
                        a.body.as_expr(),
                    )
                })
                .collect();
            collect_modified_vars_in_operand(body, expr, modified, type_table);
            for (pattern, guard, body_expr) in arm_data {
                collect_pattern_bindings(body, pattern, modified);
                if let Some(g) = guard {
                    collect_modified_vars_in_expr(body, g, modified, type_table);
                }
                if let Some(be) = body_expr {
                    collect_modified_vars_in_expr(body, be, modified, type_table);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Immutable-reference binding collection
// ---------------------------------------------------------------------------

/// An in-loop immutable reference binding `let ref_var: &T = &source_var`.
/// Valid for look-through only when `ref_var` provably holds `&source_var` at
/// every in-loop use: bound by exactly one in-loop `let` (`let_count == 1`;
/// any second binding could retarget it) and never reassigned or
/// `&mut`-escaped (checked against `ModifiedVars::fully` by the caller).
#[derive(Debug, Clone)]
struct LicmRefBinding {
    source_index: u32,
    source_name: String,
    /// Total in-loop `let` statements binding this local (ref-shaped or not).
    let_count: u32,
}

fn collect_immutable_ref_bindings(
    body: &Body,
    block: BlockId,
    type_table: &TypeTable,
) -> IndexMap<u32, LicmRefBinding> {
    let mut bindings = IndexMap::default();
    let mut let_counts: IndexMap<u32, u32> = IndexMap::default();
    collect_licm_ref_bindings(
        body,
        NodeRef::Block(block),
        type_table,
        &mut bindings,
        &mut let_counts,
    );
    for (local, binding) in &mut bindings {
        binding.let_count = let_counts[local];
    }
    bindings
}

fn collect_licm_ref_bindings(
    body: &Body,
    node: NodeRef,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
    let_counts: &mut IndexMap<u32, u32>,
) {
    // `let x: &T = &y` (immutable ref to a local) records `x -> y`; every
    // `let` counts toward its local's binding total.
    if let NodeRef::Stmt(s) = node
        && let StmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } = &body.stmts[s].kind
    {
        *let_counts.entry(*local_index).or_insert(0) += 1;
        if matches!(type_table.get(*type_id), ResolvedType::Ref(_))
            && let Some(ve) = value.as_expr()
            && let ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: source,
            } = &body.exprs[ve].kind
            && let Some(se) = source.as_expr()
            && let ExprKind::Local {
                index: source_idx,
                name: source_name,
            } = &body.exprs[se].kind
        {
            bindings.insert(
                *local_index,
                LicmRefBinding {
                    source_index: *source_idx,
                    source_name: source_name.clone(),
                    let_count: 0,
                },
            );
        }
    }
    body.for_each_child(node, |c| {
        if !matches!(c, NodeRef::Pat(_)) {
            collect_licm_ref_bindings(body, c, type_table, bindings, let_counts);
        }
    });
}

// ---------------------------------------------------------------------------
// Hoist-candidate detection
// ---------------------------------------------------------------------------

/// A hoistable `local.field` access found in the loop body.
#[derive(Debug)]
struct HoistCandidate {
    local_index: u32,
    local_name: String,
    field_index: u32,
    field_name: String,
    type_id: TypeId,
}

/// A hoist performed in step 4: the (source local, field) pair now served by
/// the pre-header local `new_local_index`, whose allocated `name` every
/// rewritten read reuses.
struct HoistedField {
    local_index: u32,
    field_index: u32,
    new_local_index: u32,
    name: String,
}

fn find_hoist_candidates(
    body: &Body,
    node: NodeRef,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
) {
    // The key pattern: field access on a loop-invariant local.
    if let NodeRef::Expr(e) = node
        && let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } = &body.exprs[e].kind
        && let Some(inner_e) = inner.as_expr()
        && let ExprKind::Local { index, name } = &body.exprs[inner_e].kind
    {
        let field_index = *field_index;
        // Case 1: direct access on a loop-invariant local.
        if modified_vars.is_field_hoistable(*index, field_index) {
            if seen.insert((*index, field_index)) {
                candidates.push(HoistCandidate {
                    local_index: *index,
                    local_name: name.clone(),
                    field_index,
                    field_name: field_name.clone(),
                    type_id: body.exprs[e].type_id,
                });
            }
        }
        // Case 2: access through an immutable reference to a loop-invariant local.
        else if let Some(ref_binding) = ref_bindings.get(index)
            && modified_vars.is_field_hoistable(ref_binding.source_index, field_index)
            && seen.insert((ref_binding.source_index, field_index))
        {
            candidates.push(HoistCandidate {
                local_index: ref_binding.source_index,
                local_name: ref_binding.source_name.clone(),
                field_index,
                field_name: field_name.clone(),
                type_id: body.exprs[e].type_id,
            });
        }
    }
    body.for_each_child(node, |c| {
        if !matches!(c, NodeRef::Pat(_)) {
            find_hoist_candidates(body, c, modified_vars, ref_bindings, candidates, seen);
        }
    });
}

// ---------------------------------------------------------------------------
// Loop-invariant pure-arithmetic hoisting
// ---------------------------------------------------------------------------

/// Binary ops that are pure and total (cannot trap, no side effects), so a
/// loop-invariant instance can be speculatively computed once in the
/// pre-header. `Div` / `Mod` are excluded (trap on a zero divisor — hoisting
/// out of a possibly-zero-iteration loop could trap where the original would
/// not). `RefEq` / `RefNotEq` are excluded (reference operands, not arithmetic).
fn is_hoistable_binop(op: crate::nir::NirBinaryOp) -> bool {
    use crate::nir::NirBinaryOp::{
        Add, And, BitAnd, BitOr, BitXor, Eq, Gt, GtEq, Lt, LtEq, Mul, NotEq, Or, Shl, Shr, Sub,
    };
    matches!(
        op,
        Add | Sub
            | Mul
            | Eq
            | NotEq
            | Lt
            | LtEq
            | Gt
            | GtEq
            | And
            | Or
            | BitAnd
            | BitOr
            | BitXor
            | Shl
            | Shr
    )
}

/// Unary ops that are pure and total, the unary counterpart of
/// [`is_hoistable_binop`]. `Deref` and the reference constructors are not
/// arithmetic; nothing else can be speculated.
fn is_hoistable_unop(op: NirUnaryOp) -> bool {
    matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
}

/// Whether `e`'s shape fits the hoistable-arithmetic grammar: a tree of pure,
/// total ops over `Local` leaves. A promoted (`Operand::Value`) leaf has no
/// skeleton expr and is treated as hoistable.
///
/// `Cast` is deliberately excluded: a float→int cast lowers to the trapping
/// `i32.trunc_f64_s` family (not `trunc_sat`), so hoisting one to the
/// pre-header could trap on a NaN/out-of-range value where a zero-iteration
/// loop never would — the same trap-soundness reason `Div`/`Mod` are excluded.
fn is_hoistable_arith_shape(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, op, right } => {
            is_hoistable_binop(*op)
                && left
                    .as_expr()
                    .is_none_or(|e| is_hoistable_arith_shape(body, e))
                && right
                    .as_expr()
                    .is_none_or(|e| is_hoistable_arith_shape(body, e))
        }
        ExprKind::Unary { op, expr } => {
            is_hoistable_unop(*op)
                && expr
                    .as_expr()
                    .is_none_or(|e| is_hoistable_arith_shape(body, e))
        }
        _ => false,
    }
}

/// Collect every `Local` leaf of a hoistable-arithmetic tree.
fn collect_arith_local_leaves(body: &Body, e: ExprId, out: &mut Vec<(ExprId, u32)>) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => out.push((e, *index)),
        ExprKind::Binary { left, right, .. } => {
            if let Some(le) = left.as_expr() {
                collect_arith_local_leaves(body, le, out);
            }
            if let Some(re) = right.as_expr() {
                collect_arith_local_leaves(body, re, out);
            }
        }
        ExprKind::Unary { expr, .. } => {
            if let Some(ie) = expr.as_expr() {
                collect_arith_local_leaves(body, ie, out);
            }
        }
        _ => {}
    }
}

/// A structural identity key for a hoistable-arith tree: kind / op / leaf-local
/// / promoted-operand-value, with commutative operands sorted so `a + b` and
/// `b + a` agree (matching the `ValueGraph` hash-cons). Once a tree's `Local`
/// leaves are all loop-invariant, two trees with the same key denote the same
/// value — so this replaces the `value_of` `ValueId` for the hoist dedup.
///
/// Ops are stored as their discriminants (the enums are fieldless, so `as u8`
/// is exact identity), which lets the key derive `Ord` for the commutative
/// operand sort without an `Ord` on the op enums.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ArithKey {
    Local(u32),
    /// A promoted operand is a frozen value: equal ids denote equal values.
    Value(u32),
    /// Any other shape is not part of a hoistable-arith tree; key it by expr
    /// id so it never spuriously dedups with another node.
    Opaque(u32),
    Unary(u8, Box<ArithKey>),
    Binary(u8, Box<ArithKey>, Box<ArithKey>),
}

fn arith_structural_key(body: &Body, e: ExprId) -> ArithKey {
    match &body.exprs[e].kind {
        ExprKind::Binary { left, op, right } => {
            let mut l = arith_operand_key(body, *left);
            let mut r = arith_operand_key(body, *right);
            // Commutative ops: order-independent so `a+b` ≡ `b+a`.
            if matches!(
                op,
                NirBinaryOp::Add
                    | NirBinaryOp::Mul
                    | NirBinaryOp::BitAnd
                    | NirBinaryOp::BitOr
                    | NirBinaryOp::BitXor
            ) && r < l
            {
                std::mem::swap(&mut l, &mut r);
            }
            ArithKey::Binary(*op as u8, Box::new(l), Box::new(r))
        }
        ExprKind::Unary { op, expr } => {
            ArithKey::Unary(*op as u8, Box::new(arith_operand_key(body, *expr)))
        }
        ExprKind::Local { index, .. } => ArithKey::Local(*index),
        ExprKind::Assign { .. }
        | ExprKind::Cast { .. }
        | ExprKind::Call { .. }
        | ExprKind::MethodCall { .. }
        | ExprKind::CmRawCall { .. }
        | ExprKind::IndirectCall { .. }
        | ExprKind::ClosureToCanonical { .. }
        | ExprKind::FieldAccess { .. }
        | ExprKind::Index { .. }
        | ExprKind::Block(_)
        | ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::Switch { .. }
        | ExprKind::LabeledBlock { .. }
        | ExprKind::StructLiteral { .. }
        | ExprKind::TupleLiteral { .. }
        | ExprKind::ArrayLiteral { .. }
        | ExprKind::VariantConstruct { .. }
        | ExprKind::VariantTag { .. }
        | ExprKind::VariantTest { .. }
        | ExprKind::VariantPayload { .. }
        | ExprKind::EnumConstruct { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::GlobalVarSet { .. }
        | ExprKind::PackedArray(_)
        | ExprKind::Dead => ArithKey::Opaque(e.index() as u32),
    }
}

fn arith_operand_key(body: &Body, op: Operand) -> ArithKey {
    match op {
        Operand::Expr(e) => arith_structural_key(body, e),
        Operand::Value(v) => ArithKey::Value(v.index()),
    }
}

/// Inputs shared by the arithmetic-hoist candidate walk.
struct ArithHoist<'a> {
    /// Locals bound by hoist `let`s whose statements are not in the tree
    /// yet (the caller prepends them after `licm_loop` returns): no entry
    /// value, but read-only pre-header temps are stable by construction.
    pending_hoist_locals: &'a IndexSet<u32>,
    /// Address-taken locals — writes through references are not modelled, so
    /// their use-site values cannot be trusted as loop-invariant.
    address_taken: &'a IndexSet<u32>,
    /// Loop-modified locals — a leaf is invariant iff none of its aliases are
    /// here (replaces the `value_of` `use == entry` invariance check).
    modified: &'a ModifiedVars,
}

impl ArithHoist<'_> {
    /// Whether `e` is a compound arithmetic expression that may move to
    /// the pre-header, returning its structural key for dedup. Each `Local`
    /// leaf's use-site value must equal the pre-header entry value, so the
    /// hoisted clone computes what every occurrence reads — cross-iteration
    /// invariance alone would wrongly admit `loop { x = 5; … x + n … }`.
    fn candidate(&self, body: &Body, e: ExprId) -> Option<ArithKey> {
        let compound = matches!(
            &body.exprs[e].kind,
            ExprKind::Binary { .. } | ExprKind::Unary { .. }
        );
        if !compound || !is_hoistable_arith_shape(body, e) {
            return None;
        }
        let mut leaves: Vec<(ExprId, u32)> = Vec::new();
        collect_arith_local_leaves(body, e, &mut leaves);
        // A constant-only tree is left for constant folding.
        if leaves.is_empty() {
            return None;
        }
        for (_leaf, idx) in leaves {
            if self.pending_hoist_locals.contains(&idx) {
                continue;
            }
            if self.address_taken.contains(&idx) {
                return None;
            }
            // The leaf must be loop-invariant. Read it from `modified_vars`
            // (value-graph-free) instead of `value(leaf) == loop_entry_value`.
            if self.modified.local_modified(idx) {
                return None;
            }
        }
        // With every `Local` leaf invariant, the structural key is exact
        // value-identity for the dedup (replaces `engine.value(e)`).
        Some(arith_structural_key(body, e))
    }

    /// Collect the maximal hoistable arithmetic subexpressions under `node`,
    /// paired with their structural keys. "Maximal" means a hoistable expression
    /// whose parent is not itself hoistable, so each whole tree is hoisted
    /// once. Nested loops are skipped — the recursive `licm_loop` call
    /// hoists each nested loop's own invariants into that loop's pre-header.
    fn collect(&self, body: &Body, node: NodeRef, out: &mut Vec<(ExprId, ArithKey)>) {
        if let NodeRef::Stmt(s) = node
            && matches!(body.stmts[s].kind, StmtKind::Loop { .. })
        {
            return;
        }
        if let NodeRef::Expr(e) = node
            && let Some(key) = self.candidate(body, e)
        {
            out.push((e, key));
            return; // maximal: do not recurse into a hoisted tree's children.
        }
        body.for_each_child(node, |c| {
            if !matches!(c, NodeRef::Pat(_)) {
                self.collect(body, c, out);
            }
        });
    }
}

/// Hoist maximal pre-header-stable pure-arithmetic subexpressions out of
/// `loop_body`, one temp per distinct `ValueId` (so copies share: `let t =
/// x; … t + y … x + y …`). The `let`s are appended to `all_hoist_stmts`,
/// which the caller prepends before the loop.
fn hoist_invariant_arith(
    engine: &mut Engine,
    loop_body: BlockId,
    modified: &ModifiedVars,
    all_hoist_stmts: &mut Vec<StmtId>,
    ctx: &mut LicmCtx,
) -> bool {
    // Earlier hoist rounds may have changed which locals are address-taken;
    // refresh that scan. The value graph is not rebuilt (build-once invariant):
    // an arith hoist appends a pre-header `let t = <invariant>` and never
    // reassigns an existing local, so every existing local's loop-entry value
    // stays valid; the new `t` simply has no entry and is not a candidate.
    engine.invalidate_address_taken();

    let mut pending_hoist_locals: IndexSet<u32> = IndexSet::default();
    for &s in all_hoist_stmts.iter() {
        if let StmtKind::Let { local_index, .. } = &engine.body.stmts[s].kind {
            pending_hoist_locals.insert(*local_index);
        }
    }
    let address_taken: IndexSet<u32> = engine.body_address_taken().clone();

    let walk = ArithHoist {
        pending_hoist_locals: &pending_hoist_locals,
        address_taken: &address_taken,
        modified,
    };
    let mut found: Vec<(ExprId, ArithKey)> = Vec::new();
    walk.collect(engine.body, NodeRef::Block(loop_body), &mut found);
    if found.is_empty() {
        // No skeleton arith trees, but operand promotion may have left the
        // invariant as a bare `Operand::Value` slot (no skeleton expr) — hoist
        // those.
        let mut c = hoist_invariant_value_operands(engine, loop_body, all_hoist_stmts, ctx);
        c |= cse_loop_body(engine, loop_body, modified);
        return c;
    }

    // Group occurrences by (structural key, type): structurally-equal invariant
    // trees of equal type share one temp. The type key is belt-and-braces —
    // same-key trees over a shared `Local` leaf already agree on types.
    let mut groups: Vec<(ArithKey, TypeId, Vec<ExprId>)> = Vec::new();
    'next: for (e, key) in found {
        let ty = engine.body.exprs[e].type_id;
        for g in &mut groups {
            if g.0 == key && g.1 == ty {
                g.2.push(e);
                continue 'next;
            }
        }
        groups.push((key, ty, vec![e]));
    }

    for (_, type_id, occ) in groups {
        let rep = occ[0];
        let name = format!("{LICM_HOIST_PREFIX}arith_{}", engine.locals().len());
        let new_idx = engine.alloc_local(name.clone(), type_id, /* is_mut */ false);
        ctx.hoist_locals.insert(new_idx);

        // Clone the representative into the pre-header `let` *before* rewriting
        // the in-loop occurrences (which include `rep` itself) to a `Local`.
        let value = engine.clone_expr(rep);
        let let_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name: name.clone(),
                local_index: new_idx,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: value.into(),
                skip_value_copy: true,
            },
            Span::new(0, 0, 0, 0),
        );
        all_hoist_stmts.push(let_stmt);

        for o in occ {
            engine.replace_expr_kind(
                o,
                ExprKind::Local {
                    index: new_idx,
                    name: name.clone(),
                },
            );
        }
    }

    hoist_invariant_value_operands(engine, loop_body, all_hoist_stmts, ctx);
    cse_loop_body(engine, loop_body, modified);
    true
}

/// Whether `idx` is in scope at the CSE insertion point (before `min_i`): it is
/// a loop-entry local, or bound by a top-level `let` of the loop body earlier.
fn cse_local_available(
    engine: &mut Engine,
    idx: u32,
    min_i: usize,
    toplevel_lets: &[(usize, u32)],
    loop_body: BlockId,
) -> bool {
    toplevel_lets.iter().any(|(i, l)| *i < min_i && *l == idx)
        || engine.loop_entry_value(loop_body, idx).is_some()
}

/// Whether cloning skeleton `e` at the insertion point is sound: every `Local`
/// leaf it reads is in scope there (see [`cse_local_available`]).
fn cse_clone_in_scope(
    engine: &mut Engine,
    e: ExprId,
    min_i: usize,
    toplevel_lets: &[(usize, u32)],
    loop_body: BlockId,
) -> bool {
    enum K {
        Local(u32),
        Bin(Operand, Operand),
        Un(Operand),
        Lit,
        No,
    }
    let k = match &engine.body.exprs[e].kind {
        ExprKind::Local { index, .. } => K::Local(*index),
        ExprKind::Binary { left, right, .. } => K::Bin(*left, *right),
        ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => K::Un(*expr),
        ExprKind::PackedArray(_) => K::Lit,
        _ => K::No,
    };
    match k {
        K::Local(idx) => cse_local_available(engine, idx, min_i, toplevel_lets, loop_body),
        K::Bin(l, r) => {
            cse_operand_in_scope(engine, l, min_i, toplevel_lets, loop_body)
                && cse_operand_in_scope(engine, r, min_i, toplevel_lets, loop_body)
        }
        K::Un(o) => cse_operand_in_scope(engine, o, min_i, toplevel_lets, loop_body),
        K::Lit => true,
        K::No => false,
    }
}

fn cse_operand_in_scope(
    engine: &mut Engine,
    op: Operand,
    min_i: usize,
    toplevel_lets: &[(usize, u32)],
    loop_body: BlockId,
) -> bool {
    match op {
        Operand::Expr(e) => cse_clone_in_scope(engine, e, min_i, toplevel_lets, loop_body),
        Operand::Value(v) => {
            let mut leaves: IndexSet<u32> = IndexSet::default();
            engine.body.values.collect_opaque_locals(v, &mut leaves);
            leaves
                .iter()
                .all(|&idx| cse_local_available(engine, idx, min_i, toplevel_lets, loop_body))
        }
    }
}

/// Common-subexpression elimination inside a loop body under operand promotion.
///
/// The value graph hash-conses a pure subexpression (`p * p` over a loop-carried
/// `p`) to one `ValueId`, so the two occurrences in a guard and the body share
/// an identity — but each is a distinct *skeleton* `Binary` expr the extractor
/// can not promote to a bare `Operand::Value` (a loop-carried local's value is
/// not reemittable at an arbitrary slot, so it stays a sourceless `Opaque`).
/// Each is therefore re-emitted. This restores the one-computation `__cse_N`
/// shape the standalone `cse` pass produced before hash-consing subsumed the
/// *deduplication* (but not the materialisation): bind a clone of the
/// subexpression to a temp placed before the earliest top-level statement that
/// contains an occurrence, and redirect every occurrence to read the temp.
///
/// Soundness — placement and availability:
/// - The temp lands before the earliest top-level statement of the loop body
///   that holds an occurrence, so it dominates every (later or equal) occurrence
///   in the body's linear statement list.
/// - A value with ≥2 occurrences sharing one `ValueId` reads the *same* leaf
///   values at each, so those leaves are in scope at all of them — hence bound
///   before the earliest occurrence's statement (or loop-carried / a param),
///   available where the temp is inserted. The clone re-emits the original
///   skeleton (a `local.get` of each leaf), so it computes exactly the shared
///   value. Trap-prone ops are excluded, so computing it once up front (possibly
///   on an iteration a conditional occurrence would have skipped) cannot trap.
fn cse_loop_body(engine: &mut Engine, loop_body: BlockId, modified: &ModifiedVars) -> bool {
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    // Occurrences of each materialisable arith value, keyed by a value-graph-free
    // **structural key**, as (top-level stmt index, expr) in first-seen order.
    // Two structurally-equal trees denote the same value exactly when their
    // leaves hold the same values at both points; the per-run split below (no
    // leaf assigned across the span) establishes that without `value_of`,
    // replacing the value graph's per-point flow-sensitivity. Nested loops are
    // not descended.
    let mut occ: IndexMap<ArithKey, Vec<(usize, ExprId)>> = IndexMap::default();
    for (i, &s) in stmts.iter().enumerate() {
        let mut exprs = Vec::new();
        collect_cse_exprs(engine.body, NodeRef::Stmt(s), &mut exprs);
        for e in exprs {
            if !is_cse_candidate_expr(engine.body, e) {
                continue;
            }
            let mut leaves: Vec<(ExprId, u32)> = Vec::new();
            collect_arith_local_leaves(engine.body, e, &mut leaves);
            // A constant-only tree is left to const folding.
            if leaves.is_empty() {
                continue;
            }
            occ.entry(arith_structural_key(engine.body, e))
                .or_default()
                .push((i, e));
        }
    }

    // Locals bound by a top-level `let` of the loop body, with their statement
    // index — the in-scope set at the insertion point grows as these precede it.
    let toplevel_lets: Vec<(usize, u32)> = stmts
        .iter()
        .enumerate()
        .filter_map(|(i, &s)| match engine.body.stmts[s].kind {
            StmtKind::Let { local_index, .. } => Some((i, local_index)),
            _ => None,
        })
        .collect();
    let address_taken: IndexSet<u32> = engine.body_address_taken().clone();

    // (stmt index, let) inserts and per-expr redirects, computed before any
    // mutation so indices stay stable.
    let mut inserts: Vec<(usize, StmtId)> = Vec::new();
    for (_key, occs) in occ {
        if occs.len() < 2 {
            continue;
        }
        // Leaves of this group (same key ⇒ same structure ⇒ same leaf locals).
        let mut leaves: Vec<(ExprId, u32)> = Vec::new();
        collect_arith_local_leaves(engine.body, occs[0].1, &mut leaves);
        let leaf_ids: Vec<u32> = leaves.iter().map(|(_, idx)| *idx).collect();
        // Address-taken leaves can be mutated through an alias at an unknown
        // point, which per-statement assignment tracking does not see — so an
        // address-taken leaf must be loop-**invariant** (never modified) for the
        // value to be stable. If any is modified, skip the whole group.
        if leaf_ids
            .iter()
            .any(|idx| address_taken.contains(idx) && modified.local_modified(*idx))
        {
            continue;
        }
        // Split the occurrences (in statement order) into maximal **runs** within
        // which no non-address-taken leaf is directly assigned across the span —
        // each run computes one value, soundly CSE'd into one temp. This replaces
        // the value graph's per-point flow-sensitivity (`p*p` before `p += 1` is
        // one value; after, a new one).
        let mut occs = occs;
        occs.sort_by_key(|(i, _)| *i);
        let mut runs: Vec<Vec<(usize, ExprId)>> = Vec::new();
        for (i, e) in occs {
            let start_new = match runs.last() {
                Some(run) => {
                    let lo = run[0].0;
                    (lo..=i).any(|si| {
                        let s = stmts[si];
                        leaf_ids.iter().any(|idx| {
                            !address_taken.contains(idx)
                                && local_assigned_in(engine.body, NodeRef::Stmt(s), *idx)
                        })
                    })
                }
                None => true,
            };
            if start_new {
                runs.push(vec![(i, e)]);
            } else {
                runs.last_mut().unwrap().push((i, e));
            }
        }
        for occs in runs {
            if occs.len() < 2 {
                continue;
            }
            let ty = engine.body.exprs[occs[0].1].type_id;
            let min_i = occs.iter().map(|(i, _)| *i).min().unwrap();
            // Clone an occurrence whose skeleton is in scope at the insertion point
            // (before `min_i`): every `Local` leaf must be a loop-entry local or
            // bound by a top-level `let` before `min_i`. The value graph already
            // proved every occurrence equal, so any in-scope occurrence computes the
            // right value; cloning a bare alias read of an inner-scope local (e.g.
            // `let __cse = a` where `a` is bound inside a nested block) would read a
            // stale loop-carried value. Skip the value if none qualifies.
            let Some(&(_, src_expr)) = occs
                .iter()
                .find(|(_, e)| cse_clone_in_scope(engine, *e, min_i, &toplevel_lets, loop_body))
            else {
                continue;
            };
            let span = engine.body.exprs[src_expr].span;
            let name = format!("__cse_{}", engine.locals().len());
            let temp = engine.alloc_local(name.clone(), ty, /* is_mut */ false);
            // Clone the chosen occurrence's skeleton subtree for the temp's value
            // (the value itself is a sourceless-Opaque tree the extractor can not
            // re-emit; the skeleton can).
            let cloned = engine.clone_expr(src_expr);
            let let_stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name: name.clone(),
                    local_index: temp,
                    is_mut: false,
                    is_reactive: false,
                    type_id: ty,
                    value: Operand::Expr(cloned),
                    skip_value_copy: true,
                },
                span,
            );
            // Redirect each occurrence to a *skeleton* `local.get __cse` (not a
            // promoted value): the temp is reassigned every iteration, so a value
            // operand `Opaque(Local(__cse))` would read as loop-invariant to the
            // arith hoist. `__cse` has no loop-entry value, so a skeleton read is
            // correctly treated as loop-carried.
            let mut any = false;
            for (_, e) in &occs {
                let lread = engine.alloc_expr(
                    ExprKind::Local {
                        index: temp,
                        name: name.clone(),
                    },
                    ty,
                    span,
                );
                any |= engine.redirect_expr(*e, Operand::Expr(lread));
            }
            if any {
                inserts.push((min_i, let_stmt));
            }
        }
    }
    if inserts.is_empty() {
        return false;
    }
    // Splice the temps in, each before its target statement.
    let mut new_stmts: Vec<StmtId> = Vec::new();
    for (i, &s) in stmts.iter().enumerate() {
        for (_, let_stmt) in inserts.iter().filter(|(mi, _)| *mi == i) {
            new_stmts.push(*let_stmt);
        }
        new_stmts.push(s);
    }
    engine.set_block_stmts(loop_body, new_stmts);
    true
}

/// Collect every expression under `node`, descending through blocks but
/// **not** into nested `Loop` bodies (keeping occurrences within one loop's
/// dominance scope).
fn collect_cse_exprs(body: &Body, node: NodeRef, out: &mut Vec<ExprId>) {
    if let NodeRef::Stmt(s) = node
        && matches!(body.stmts[s].kind, StmtKind::Loop { .. })
    {
        return;
    }
    if let NodeRef::Expr(e) = node {
        out.push(e);
    }
    body.for_each_child(node, |c| {
        if !matches!(c, NodeRef::Pat(_)) {
            collect_cse_exprs(body, c, out);
        }
    });
}

/// Whether the subtree under `node` assigns local `idx`: `idx = …` (an
/// `Assign` rooting at `idx`), `let idx = …`, or a pattern binding of `idx`
/// (`let [idx, …] = …`, a `match` arm). For a **non-address-taken** local
/// these are the only paths that change its value (no reference can alias it,
/// and a call cannot reach a non-escaping local) — so a `cse_loop_body`
/// occurrence span free of such assignments reads the same leaf value at every
/// occurrence. The whole subtree is searched, **including** nested `Loop`
/// bodies: occurrences are collected only outside them, but an assignment
/// inside one still changes what a later occurrence reads.
fn local_assigned_in(body: &Body, node: NodeRef, idx: u32) -> bool {
    match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
                && *local_index == idx
            {
                return true;
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::Assign { target, .. } = &body.exprs[e].kind
                && super::arena_query::storage_root(body, *target) == Some(idx)
            {
                return true;
            }
        }
        NodeRef::Pat(p) => {
            if let PatKind::Binding { local_index, .. } = &body.pats[p].kind
                && *local_index == idx
            {
                return true;
            }
        }
        NodeRef::Block(_) => {}
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found {
            found = local_assigned_in(body, c, idx);
        }
    });
    found
}

/// Whether `e` is a pure arithmetic compound worth CSE-materialising: a
/// `Binary` / `Unary` with a non-trap-prone op, checked structurally
/// (value-graph-free). The leaves need no availability check here — see
/// [`cse_loop_body`]'s soundness note (shared scope of ≥2 occurrences) — only
/// the root must be a compound, not a bare leaf.
fn is_cse_candidate_expr(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Binary { op, .. } => is_hoistable_binop(*op),
        ExprKind::Unary { op, .. } => is_hoistable_unop(*op),
        _ => false,
    }
}

/// Hoist loop-invariant promoted *value* operands into a pre-header
/// `let _licm_arith_N`. Operand promotion can leave a loop-invariant compound
/// (e.g. `hi - lo`, born as a value before the loop) as a bare `Operand::Value`
/// slot with no skeleton expr, so [`ArithHoist`] (which scans skeleton trees)
/// never sees it. Materialise each distinct invariant value once in the
/// pre-header and redirect its in-loop slots to a read of the temp.
fn hoist_invariant_value_operands(
    engine: &mut Engine,
    loop_body: BlockId,
    all_hoist_stmts: &mut Vec<StmtId>,
    ctx: &mut LicmCtx,
) -> bool {
    use crate::nir_value_graph::OpaqueSource;

    let (expr_ids, stmt_ids) = collect_loop_subtree(engine.body, loop_body);

    // Phase 1: snapshot every operand slot in the subtree, in a fixed order.
    let mut ops: Vec<Operand> = Vec::new();
    for &e in &expr_ids {
        engine.body.map_expr_operands(e, &mut |op| {
            ops.push(op);
            op
        });
    }
    for &s in &stmt_ids {
        engine.body.map_stmt_operands(s, &mut |op| {
            ops.push(op);
            op
        });
    }

    // Locals available at the loop pre-header (where the hoisted `let` lands):
    // exactly those with a loop-entry value. A value whose `Opaque(Local)` leaf
    // names a local *bound inside* the loop (a pattern / while-let / nested
    // binding — the value graph gives it a fresh per-iteration Opaque, never a
    // `LoopPhi`) has no entry value, so hoisting it would compute the wrong
    // thing. Collect the candidate leaves first, then keep only the entry-live
    // ones — soundness gate for the hoist.
    let mut leaf_locals: IndexSet<u32> = IndexSet::default();
    for op in &ops {
        if let Operand::Value(v) = *op {
            engine
                .body
                .values
                .collect_opaque_locals(v, &mut leaf_locals);
        }
    }
    let mut entry_locals: IndexSet<u32> = IndexSet::default();
    for idx in leaf_locals {
        if engine.loop_entry_value(loop_body, idx).is_some() {
            entry_locals.insert(idx);
        }
    }

    // Phase 2: pick the distinct invariant compound value reps, in first-seen
    // order, and materialise a pre-header temp + read value for each.
    let mut rep_read: IndexMap<ValueId, ValueId> = IndexMap::default();
    for op in &ops {
        let Operand::Value(rep) = *op else { continue };
        if rep_read.contains_key(&rep)
            || !is_hoistable_value(&engine.body.values, rep, &entry_locals)
        {
            continue;
        }
        let Some(ty) = engine.body.values.type_of(rep) else {
            continue;
        };
        let name = format!("{LICM_HOIST_PREFIX}arith_{}", engine.locals().len());
        let temp = engine.alloc_local(name.clone(), ty, /* is_mut */ false);
        ctx.hoist_locals.insert(temp);
        let read = engine
            .body
            .values
            .fresh_opaque_with_source(OpaqueSource::Local(temp));
        engine.body.values.set_type(read, ty);
        let let_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name,
                local_index: temp,
                is_mut: false,
                is_reactive: false,
                type_id: ty,
                value: Operand::Value(rep),
                skip_value_copy: true,
            },
            Span::new(0, 0, 0, 0),
        );
        all_hoist_stmts.push(let_stmt);
        rep_read.insert(rep, read);
    }
    if rep_read.is_empty() {
        return false;
    }

    // Phase 3: precompute the new operand for each snapshot slot, then re-apply
    // in the same order (the closure touches no `Body` field, so no borrow
    // conflicts with the map).
    let new_ops: Vec<Operand> = ops
        .iter()
        .map(|op| match *op {
            Operand::Value(v) => match rep_read.get(&v) {
                Some(&read) => Operand::Value(read),
                None => *op,
            },
            _ => *op,
        })
        .collect();
    let mut i = 0;
    for &e in &expr_ids {
        engine.body.map_expr_operands(e, &mut |_| {
            let r = new_ops[i];
            i += 1;
            r
        });
    }
    for &s in &stmt_ids {
        engine.body.map_stmt_operands(s, &mut |_| {
            let r = new_ops[i];
            i += 1;
            r
        });
    }
    true
}

/// Collect every expression and statement id reachable from `loop_body` (the
/// whole loop subtree, including nested loops — a pre-header temp dominates
/// them, so rewriting their slots stays sound). Patterns are excluded: they
/// carry no operand slots the caller rewrites.
fn collect_loop_subtree(body: &Body, loop_body: BlockId) -> (Vec<ExprId>, Vec<StmtId>) {
    let mut expr_ids = Vec::new();
    let mut stmt_ids = Vec::new();
    let mut work = vec![NodeRef::Block(loop_body)];
    while let Some(node) = work.pop() {
        match node {
            NodeRef::Expr(e) => expr_ids.push(e),
            NodeRef::Stmt(s) => stmt_ids.push(s),
            NodeRef::Block(_) => {}
            NodeRef::Pat(_) => continue,
        }
        body.for_each_child(node, |c| work.push(c));
    }
    (expr_ids, stmt_ids)
}

/// Whether `v` is a loop-invariant arithmetic compound worth hoisting: a
/// `Binary` / `Unary` root over leaves that are constants or `Opaque(Local)`
/// reads of pre-header-available locals (`entry_locals`), with at least one such
/// local leaf (a constant-only tree is left to const folding). Trap-prone ops
/// (`Div` / `Mod`) and flow-merge / heap kinds (`LoopPhi` / `Select` /
/// `FieldAccess` / `Opaque(Expr)` / `Cast`) are excluded — the same shape
/// `ArithHoist` admits.
fn is_hoistable_value(
    pool: &crate::nir_value_graph::ValuePool,
    v: ValueId,
    entry_locals: &IndexSet<u32>,
) -> bool {
    use crate::nir_value_graph::ValueKind;
    let compound = matches!(
        pool.kind(v),
        ValueKind::Binary { .. } | ValueKind::Unary { .. }
    );
    if !compound || !value_is_invariant(pool, v, entry_locals) {
        return false;
    }
    let mut leaves = IndexSet::default();
    pool.collect_opaque_locals(v, &mut leaves);
    !leaves.is_empty()
}

fn value_is_invariant(
    pool: &crate::nir_value_graph::ValuePool,
    v: ValueId,
    entry_locals: &IndexSet<u32>,
) -> bool {
    use crate::nir_value_graph::{OpaqueSource, ValueKind};
    match pool.kind(v) {
        ValueKind::Int(..)
        | ValueKind::Float(..)
        | ValueKind::Bool(_)
        | ValueKind::Char(_)
        | ValueKind::Null
        | ValueKind::Unit => true,
        ValueKind::Opaque(oid) => match pool.opaque_source(*oid) {
            Some(OpaqueSource::Local(idx)) => entry_locals.contains(&idx),
            _ => false,
        },
        ValueKind::Binary { op, lhs, rhs, .. } => {
            is_hoistable_binop(*op)
                && value_is_invariant(pool, *lhs, entry_locals)
                && value_is_invariant(pool, *rhs, entry_locals)
        }
        ValueKind::Unary { op, operand, .. } => {
            is_hoistable_unop(*op) && value_is_invariant(pool, *operand, entry_locals)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Replace hoisted field accesses with the hoisted locals
// ---------------------------------------------------------------------------

fn replace_hoisted(
    engine: &mut Engine,
    node: NodeRef,
    hoisted: &[HoistedField],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    // First, check if this expression matches a hoisted access.
    let matched = if let NodeRef::Expr(e) = node
        && let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &engine.body.exprs[e].kind
        && let Some(inner_e) = inner.as_expr()
        && let ExprKind::Local { index, .. } = &engine.body.exprs[inner_e].kind
    {
        let index = *index;
        let field_index = *field_index;
        // Case 1: direct match — local.field where local is the hoisted source.
        let direct = hoisted
            .iter()
            .find(|h| h.local_index == index && h.field_index == field_index);
        if let Some(h) = direct {
            Some((e, h))
        } else if let Some(ref_binding) = ref_bindings.get(&index) {
            // Case 2: look through immutable reference — ref_var.field.
            hoisted
                .iter()
                .find(|h| h.local_index == ref_binding.source_index && h.field_index == field_index)
                .map(|h| (e, h))
        } else {
            None
        }
    } else {
        None
    };
    if let Some((e, h)) = matched {
        engine.replace_expr_kind(
            e,
            ExprKind::Local {
                index: h.new_local_index,
                name: h.name.clone(),
            },
        );
        return;
    }

    // Recurse into sub-expressions / sub-blocks.
    for c in child_nodes(engine.body, node) {
        if !matches!(c, NodeRef::Pat(_)) {
            replace_hoisted(engine, c, hoisted, ref_bindings);
        }
    }
}
