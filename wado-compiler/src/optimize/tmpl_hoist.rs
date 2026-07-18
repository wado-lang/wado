//! Template String Buffer Hoisting for Loops
//!
//! When a template string (`__tmpl` labeled block) appears inside a loop,
//! this pass hoists the entire `String` allocation before the loop and reuses
//! it across iterations, resetting `used = 0` instead of creating a new struct.
//!
//! **Before:**
//! ```text
//! loop {
//!     let s = __tmpl: {
//!         let mut __r = String { repr: array_new(N), used: 0 };
//!         __r.push_str(...);
//!         break __tmpl: __r;
//!     };
//!     s.len();   // s only used as method receiver
//! }
//! ```
//!
//! **After:**
//! ```text
//! let mut __tmpl_buf_0 = String { repr: array_new(N), used: 0 };
//! loop {
//!     let s /* skip_value_copy */ = __tmpl: {
//!         __tmpl_buf_0.used = 0;        // reset (no struct.new)
//!         __tmpl_buf_0.push_str(...);      // reuse same String
//!         break __tmpl: __tmpl_buf_0;
//!     };
//!     s.len();   // s aliases __tmpl_buf_0
//! }
//! ```
//!
//! Safety: The optimization reuses the same String GC struct across iterations.
//! It is only applied when the template result does not escape the iteration:
//! the result must be bound to a Let variable whose value never reaches an
//! escaping position (call argument, aggregate field/element, assignment /
//! global / return / break value) — directly, through `if`/`match`/block
//! result tails, or through uncopied alias `let`s (see [`EscapeScan`]). The
//! inner `__r` buffer is checked too ([`template_buf_escapes`]).
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and walks every loop in the function, applying the template-
//! string buffer hoist per loop. All mutations route through the engine edit
//! API (`alloc_expr`, `alloc_stmt`, `alloc_local`, `set_block_stmts`,
//! `replace_expr_kind`) so the parent map and use index stay coherent.

use std::cell::{Cell, RefCell};

use crate::compiler_item::SeqField;
use crate::hashmap::IndexSet;
use crate::nir::{FuncId, FunctionRef, NirFunction, NirUnaryOp};
use crate::nir_arena::{
    ArenaStructField, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::arena_query::{is_local, stmt_mentions_local};
use super::gate::{FunctionGate, GatedPass};
use cranelift_entity::EntityRef;

/// The builtin callee ids the template-hoist recognizers match, resolved once
/// per pass run so each match is an integer `func_id` compare (not a name read
/// off the call node's `FunctionRef`). Each callee is resolved by exact
/// identity — the builtin descriptor (`builtin_name` /
/// `monomorphized_builtin_name`, the mechanism `loop_version_bce` uses for
/// `builtin::array_set`) or the resolved method identity — never by substring,
/// so a same-named user function is never captured. Builtins are *sets* over
/// every monomorphized instance.
pub(super) struct TmplCalleeIds {
    /// The stdlib `String::with_capacity` (the pre-lowered template init).
    with_capacity: IndexSet<FuncId>,
    /// `builtin::array_new` and its monomorphizations.
    array_new: IndexSet<FuncId>,
    /// `builtin::ref.as_non_null`.
    ref_as_non_null: IndexSet<FuncId>,
}

/// Exact identity of the stdlib `String::with_capacity`: an inherent
/// `with_capacity` method on `String` declared in a `core:` module. Matching
/// the resolved method identity (not the mangled name string) keeps a
/// user-defined `String::with_capacity` in a local module from being captured.
fn is_string_with_capacity(f: &NirFunction) -> bool {
    f.module_source.is_core()
        && f.method_info.as_ref().is_some_and(|mi| {
            mi.base_struct_name == "String"
                && mi.method_name == "with_capacity"
                && mi.trait_name.is_none()
        })
}

impl TmplCalleeIds {
    fn resolve(project: &NirPackage) -> Self {
        let mut with_capacity = IndexSet::default();
        let mut array_new = IndexSet::default();
        let mut ref_as_non_null = IndexSet::default();
        for f in &project.functions {
            let f = f.borrow();
            let Some(id) = f.id else { continue };
            if is_string_with_capacity(&f) {
                with_capacity.insert(id);
            }
            let descriptor = FunctionRef::from_resolved(&f, f.module_source.clone());
            let builtin = descriptor
                .builtin_name()
                .or_else(|| descriptor.monomorphized_builtin_name());
            if builtin.as_deref() == Some("builtin::array_new") {
                array_new.insert(id);
            } else if builtin.as_deref() == Some("builtin::ref.as_non_null") {
                ref_as_non_null.insert(id);
            }
        }
        Self {
            with_capacity,
            array_new,
            ref_as_non_null,
        }
    }

    fn is(set: &IndexSet<FuncId>, func_id: FuncId) -> bool {
        set.contains(&func_id)
    }
}

/// Apply template string buffer hoisting to all functions in the project.
pub fn hoist_template_buffers(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.clone();
    let callee_ids = TmplCalleeIds::resolve(project);
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::TmplHoist, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = TmplHoistRule {
            type_table: &type_table,
            callee_ids: &callee_ids,
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function template-buffer hoist at the body root.
pub(super) struct TmplHoistRule<'a> {
    type_table: &'a RefCell<TypeTable>,
    callee_ids: &'a TmplCalleeIds,
    applied: Cell<bool>,
}

impl Rule for TmplHoistRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        let root = engine.body.root;
        hoist_in_block(engine, root, self.type_table, self.callee_ids)
    }
}

/// Walk `block` for `Loop` statements at any nesting depth — including loops
/// under `match`/`switch` arms and expression blocks, via the generic
/// `for_each_child` walk — and hoist template buffers out of each loop found.
/// The hoisted `let`s land immediately before the loop statement in its
/// containing block.
fn hoist_in_block(
    engine: &mut Engine,
    block: BlockId,
    type_table: &RefCell<TypeTable>,
    callee_ids: &TmplCalleeIds,
) -> bool {
    let mut changed = false;
    let mut new_stmts: Vec<StmtId> = Vec::new();

    for s in engine.body.blocks[block].stmts.clone() {
        if let StmtKind::Loop { body } = &engine.body.stmts[s].kind {
            let lb = *body;
            // Recurse into the loop body first (for nested loops).
            changed |= hoist_in_block(engine, lb, type_table, callee_ids);
            // Try to hoist template buffers out of this loop.
            let hoist_stmts = hoist_tmpl_from_loop(engine, lb, type_table, callee_ids);
            if !hoist_stmts.is_empty() {
                changed = true;
                new_stmts.extend(hoist_stmts);
            }
        } else {
            // Classify without holding the borrow across the mutable recursion.
            let mut blocks = Vec::new();
            nearest_child_blocks(engine.body, NodeRef::Stmt(s), &mut blocks);
            for b in blocks {
                changed |= hoist_in_block(engine, b, type_table, callee_ids);
            }
        }
        new_stmts.push(s);
    }

    engine.set_block_stmts(block, new_stmts);
    changed
}

/// The nearest enclosed blocks under `node`: descends expressions but stops at
/// each `Block`, which the caller recurses into itself — `hoist_in_block` must
/// own every statement list it may splice hoisted `let`s into.
fn nearest_child_blocks(body: &Body, node: NodeRef, out: &mut Vec<BlockId>) {
    body.for_each_child(node, |child| {
        if let NodeRef::Block(b) = child {
            out.push(b);
        } else {
            nearest_child_blocks(body, child, out);
        }
    });
}

/// Information about a `__tmpl` block that can be hoisted.
struct TmplCandidate {
    /// Index of the `__r` local in the `__tmpl` block
    buf_local_index: u32,
    /// The init-value expression node id (e.g. `String { repr: array_new(N),
    /// used: 0 }`). Reused as the hoisted `Let`'s value — the original first
    /// statement is replaced, so this subtree becomes the hoist's sole owner.
    init_value: ExprId,
    /// The String type ID
    string_type: TypeId,
    /// Field index of the container `used` field: the matched init literal's
    /// own index, or the canonical [`SeqField::Len`] index for the pre-lowered
    /// `with_capacity` form.
    used_field_index: u32,
    /// Type of the `used` field (the reset writes a typed zero).
    used_field_type: TypeId,
    /// The span of the original expression
    span: Span,
}

/// Information about a Formatter struct literal that can be hoisted out of a `__tmpl` block.
struct FmtCandidate {
    /// Index of the statement inside the `__tmpl` block that creates the Formatter
    stmt_index: usize,
    /// The local index being assigned to (e.g., `__local_13`)
    fmt_local_index: u32,
    /// The normalized Formatter struct-literal node id (`buf` pointing at the
    /// hoisted buffer). Reused as the hoisted `Let`'s value.
    init_value: ExprId,
    /// The Formatter type ID
    formatter_type: TypeId,
    /// Index of the `indent` field in the Formatter struct
    indent_field_index: u32,
    /// The span
    span: Span,
}

/// Scan a loop body for `__tmpl` labeled blocks and hoist their buffer allocations.
/// Returns hoisting statements to prepend before the loop.
fn hoist_tmpl_from_loop(
    engine: &mut Engine,
    loop_body: BlockId,
    type_table: &RefCell<TypeTable>,
    callee_ids: &TmplCalleeIds,
) -> Vec<StmtId> {
    // Phase 1: Collect all Let bindings whose value is a __tmpl LabeledBlock,
    // and check if the bound variable escapes (used as a non-self argument).
    let escaping_locals = collect_escaping_locals(engine.body, loop_body);

    // Phase 2: Transform safe __tmpl blocks
    let mut hoist_stmts = Vec::new();
    transform_stmts_in_block(
        engine,
        loop_body,
        &escaping_locals,
        &mut hoist_stmts,
        type_table,
        callee_ids,
    );
    hoist_stmts
}

/// Collect local indices that "escape" `block` — locals whose value may flow
/// out of the loop iteration. See [`EscapeScan`].
fn collect_escaping_locals(body: &Body, block: BlockId) -> IndexSet<u32> {
    let mut scan = EscapeScan::new(body);
    scan.scan_block(block);
    scan.finish()
}

/// Whether the template's own `__r` buffer local escapes its `__tmpl` block
/// through any position other than the two shapes the transform itself
/// rewrites: the verified trailing `break __tmpl: __r` (whose value becomes
/// the outer `let`, escape-checked separately) and Formatter `buf:` field
/// linkage (normalized to the hoisted buffer by `extract_fmt_candidates`; a
/// non-hoisted Formatter still only holds the buffer within the iteration).
fn template_buf_escapes(body: &Body, tmpl_block: BlockId, buf_local_index: u32) -> bool {
    let mut scan = EscapeScan::new(body);
    scan.exempt_break = body.blocks[tmpl_block].stmts.last().copied();
    scan.exempt_buf_fields = true;
    scan.scan_block(tmpl_block);
    scan.finish().contains(&buf_local_index)
}

/// Escape analysis for the template-buffer hoist.
///
/// A local escapes when its *value* reaches a position that may store it
/// beyond the iteration: a non-receiver call argument, an aggregate-literal
/// element or struct field, an assignment / global-set / return / break value.
/// At each such position the whole value-result chain of the consumed
/// expression is marked ([`for_each_chain_local`]): the bare local plus every
/// local reachable as the expression's result — through `&`/`&mut`/casts,
/// block tails, `if` branch tails, `match` arm bodies, `switch` arm tails, and
/// labeled-block break values. A call result or a fresh aggregate literal is a
/// new value, so the chain stops there: a value that leaves only through a
/// `$value_copy$…` helper stays hoistable.
///
/// `let t = <chain containing s>` records an alias edge `t → s` instead of
/// marking; [`Self::finish`] propagates escapes across edges to a fixpoint.
/// This keeps precision: a tail value consumed only by non-escaping positions
/// still hoists.
///
/// A `FieldAccess` base is deliberately *not* on the chain (`s.repr` as a
/// consumed value does not mark `s`): extracted fields feed iterators and
/// formatters consumed within the iteration, and marking them would disable
/// the pass for every `.bytes()`-style loop.
struct EscapeScan<'a> {
    body: &'a Body,
    escaping: IndexSet<u32>,
    /// `(target, source)` per `let target = …source-chain…` binding.
    alias_edges: Vec<(u32, u32)>,
    /// The template block's verified trailing `break __tmpl: __r`, exempt from
    /// break-value marking in the inner-buffer scan.
    exempt_break: Option<StmtId>,
    /// Exempt struct-literal fields named `buf` (Formatter linkage the
    /// transform itself rewrites) in the inner-buffer scan.
    exempt_buf_fields: bool,
}

impl<'a> EscapeScan<'a> {
    fn new(body: &'a Body) -> Self {
        Self {
            body,
            escaping: IndexSet::default(),
            alias_edges: Vec::new(),
            exempt_break: None,
            exempt_buf_fields: false,
        }
    }

    /// Propagate escapes backwards across alias edges to a fixpoint and
    /// return the final escaping set.
    fn finish(mut self) -> IndexSet<u32> {
        loop {
            let mut changed = false;
            for (target, source) in &self.alias_edges {
                if self.escaping.contains(target) && self.escaping.insert(*source) {
                    changed = true;
                }
            }
            if !changed {
                return self.escaping;
            }
        }
    }

    fn mark_chain(&mut self, op: Operand) {
        let body = self.body;
        for_each_chain_local(body, op, &mut |local| {
            self.escaping.insert(local);
        });
    }

    fn scan_block(&mut self, block: BlockId) {
        for s in &self.body.blocks[block].stmts {
            self.scan_stmt(*s);
        }
    }

    fn scan_stmt(&mut self, s: StmtId) {
        let body = self.body;
        match &body.stmts[s].kind {
            StmtKind::Let {
                local_index, value, ..
            } => {
                let target = *local_index;
                let value = *value;
                for_each_chain_local(body, value, &mut |source| {
                    self.alias_edges.push((target, source));
                });
                self.scan_operand(value);
            }
            StmtKind::Expr(expr) => self.scan_operand(*expr),
            StmtKind::Return { value: Some(expr) } => {
                // A returned value leaves the function.
                self.mark_chain(*expr);
                self.scan_operand(*expr);
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.scan_operand(*condition);
                self.scan_block(*then_block);
                if let Some(eb) = else_block {
                    self.scan_block(*eb);
                }
            }
            StmtKind::LabeledBlock { block, .. } => self.scan_block(*block),
            StmtKind::Loop { body: lb } => self.scan_block(*lb),
            StmtKind::Return { value: None } | StmtKind::Continue => {}
            StmtKind::Break { value, .. } => {
                // A break value flows to its target block's consumer, which
                // this statement-level walk does not track — conservatively
                // escaping, except the template's own verified trailing break.
                if self.exempt_break == Some(s) {
                    return;
                }
                if let Some(v) = value {
                    self.mark_chain(*v);
                    self.scan_operand(*v);
                }
            }
            StmtKind::LetDestructure { value, .. } => {
                // Pattern bindings alias components of the value in place.
                self.mark_chain(*value);
                self.scan_operand(*value);
            }
        }
    }

    fn scan_operand(&mut self, op: Operand) {
        if let Some(e) = op.as_expr() {
            self.scan_expr(e);
        }
    }

    fn scan_expr(&mut self, e: ExprId) {
        let body = self.body;
        match &body.exprs[e].kind {
            // Function call: args (not receiver) escape.
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.mark_chain(arg.expr);
                    self.scan_operand(arg.expr);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                // Receiver (self) doesn't escape — only non-self args escape.
                self.scan_operand(*receiver);
                for arg in args {
                    self.mark_chain(arg.expr);
                    self.scan_operand(arg.expr);
                }
            }
            ExprKind::IndirectCall { callee, args } => {
                self.scan_operand(*callee);
                for arg in args {
                    self.mark_chain(*arg);
                    self.scan_operand(*arg);
                }
            }
            ExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.mark_chain(*arg);
                    self.scan_operand(*arg);
                }
            }
            // Assignment: the value escapes (stored in the target location,
            // which may be a local declared outside the loop).
            ExprKind::Assign { target, value } => {
                self.mark_chain(*value);
                self.scan_expr(*target);
                self.scan_operand(*value);
            }
            // Struct literal fields: all field values escape, except the
            // Formatter `buf` linkage in the inner-buffer scan.
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    if !(self.exempt_buf_fields && field.name == "buf") {
                        self.mark_chain(field.value);
                    }
                    self.scan_operand(field.value);
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for elem in elements {
                    self.mark_chain(*elem);
                    self.scan_operand(*elem);
                }
            }
            ExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.mark_chain(*p);
                    self.scan_operand(*p);
                }
            }
            ExprKind::GlobalVarSet { value, .. } => {
                self.mark_chain(*value);
                self.scan_operand(*value);
            }
            ExprKind::Index { expr: inner, index } => {
                self.scan_operand(*inner);
                self.scan_operand(*index);
            }
            ExprKind::Binary { left, right, .. } => {
                self.scan_operand(*left);
                self.scan_operand(*right);
            }
            ExprKind::Unary { expr: inner, .. }
            | ExprKind::Cast { expr: inner, .. }
            | ExprKind::FieldAccess { expr: inner, .. }
            | ExprKind::VariantTag { expr: inner }
            | ExprKind::VariantTest { expr: inner, .. }
            | ExprKind::VariantPayload { expr: inner, .. }
            | ExprKind::ClosureToCanonical { functor: inner, .. } => {
                self.scan_operand(*inner);
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.scan_operand(*condition);
                self.scan_block(*then_branch);
                if let Some(eb) = else_branch {
                    self.scan_block(*eb);
                }
            }
            ExprKind::LabeledBlock { block, .. } | ExprKind::Block(block) => {
                self.scan_block(*block);
            }
            ExprKind::Match { expr: inner, arms } => {
                self.scan_operand(*inner);
                for arm in arms {
                    if let Some(g) = arm.guard {
                        self.scan_operand(g);
                    }
                    self.scan_operand(arm.body);
                }
            }
            ExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.scan_operand(*scrutinee);
                for arm in arms {
                    self.scan_block(*arm);
                }
                self.scan_block(*default);
            }
            // Leaf nodes
            ExprKind::Local { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::PackedArray(_)
            | ExprKind::Dead
            | ExprKind::EnumConstruct { .. } => {}
        }
    }
}

/// Visit every local whose value may *be* the result of `op`: the bare local,
/// and locals reachable through the value-result chain — `&`/`&mut`/casts,
/// block tails, `if` branch tails, `match` arm bodies, `switch` arm tails,
/// labeled-block break values. Calls and fresh aggregate literals produce new
/// values and end the chain; a `FieldAccess` base is deliberately excluded
/// (see [`EscapeScan`]).
fn for_each_chain_local(body: &Body, op: Operand, f: &mut impl FnMut(u32)) {
    if let Some(e) = op.as_expr() {
        for_each_chain_local_expr(body, e, f);
    }
}

fn for_each_chain_local_expr(body: &Body, e: ExprId, f: &mut impl FnMut(u32)) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => f(*index),
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            for_each_chain_local(body, *inner, f);
        }
        ExprKind::Block(block) => for_each_block_tail_chain(body, *block, f),
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            for_each_block_tail_chain(body, *then_branch, f);
            if let Some(eb) = else_branch {
                for_each_block_tail_chain(body, *eb, f);
            }
        }
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                for_each_chain_local(body, arm.body, f);
            }
        }
        ExprKind::Switch { arms, default, .. } => {
            for arm in arms {
                for_each_block_tail_chain(body, *arm, f);
            }
            for_each_block_tail_chain(body, *default, f);
        }
        ExprKind::LabeledBlock { label, block, .. } => {
            // The block's value is any `break label: v` under it, plus its
            // tail expression.
            for_each_label_break_value(body, NodeRef::Block(*block), label, &mut |v| {
                for_each_chain_local(body, v, f);
            });
            for_each_block_tail_chain(body, *block, f);
        }
        // Every other kind (calls, literals, field access, …) produces a new
        // value — or is a deliberate chain stop — so the chain ends here.
        _ => {}
    }
}

/// Chain-visit the tail expression statement of `block`, if any.
fn for_each_block_tail_chain(body: &Body, block: BlockId, f: &mut impl FnMut(u32)) {
    if let Some(s) = body.blocks[block].stmts.last()
        && let StmtKind::Expr(op) = &body.stmts[*s].kind
    {
        for_each_chain_local(body, *op, f);
    }
}

/// Visit the value of every `break label: v` under `node` targeting `label`,
/// stopping at nested same-label blocks (which shadow the target).
fn for_each_label_break_value(
    body: &Body,
    node: NodeRef,
    label: &str,
    f: &mut impl FnMut(Operand),
) {
    match node {
        NodeRef::Stmt(s) => match &body.stmts[s].kind {
            StmtKind::Break {
                label: Some(l),
                value: Some(v),
            } if l == label => {
                f(*v);
                // Keep walking the value: it may nest further same-label breaks.
            }
            StmtKind::LabeledBlock { label: l, .. } if l == label => return,
            _ => {}
        },
        NodeRef::Expr(e) => {
            if let ExprKind::LabeledBlock { label: l, .. } = &body.exprs[e].kind
                && l == label
            {
                return;
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    body.for_each_child(node, |child| {
        for_each_label_break_value(body, child, label, f);
    });
}

/// Recursively transform statements, looking for Let bindings with __tmpl blocks.
fn transform_stmts_in_block(
    engine: &mut Engine,
    block: BlockId,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
    callee_ids: &TmplCalleeIds,
) {
    for s in engine.body.blocks[block].stmts.clone() {
        transform_stmt(
            engine,
            s,
            escaping_locals,
            hoist_stmts,
            type_table,
            callee_ids,
        );
    }
}

fn transform_stmt(
    engine: &mut Engine,
    s: StmtId,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
    callee_ids: &TmplCalleeIds,
) {
    // `let x = __tmpl: { ... }` — the only statement shape that can hoist.
    let let_info = if let StmtKind::Let {
        local_index, value, ..
    } = &engine.body.stmts[s].kind
    {
        Some((*local_index, *value))
    } else {
        None
    };
    if let Some((local_index, value)) = let_info
        && let Some(ve) = value.as_expr()
    {
        let tmpl_block = match &engine.body.exprs[ve].kind {
            ExprKind::LabeledBlock { label, block, .. }
                if label == crate::name::TEMPLATE_BLOCK_LABEL =>
            {
                Some(*block)
            }
            _ => None,
        };
        if let Some(tb) = tmpl_block
            && !escaping_locals.contains(&local_index)
            && let Some(candidate) = extract_tmpl_candidate(engine.body, tb, callee_ids)
            && !template_buf_escapes(engine.body, tb, candidate.buf_local_index)
        {
            transform_tmpl_block(engine, tb, &candidate, hoist_stmts, type_table, callee_ids);
            // The hoisted String is reused; skip deep copy so `s` aliases `__tmpl_buf`.
            // This is a non-id field on `Let` and does not affect the engine's
            // parent map / use index, so the in-place write is safe.
            if let StmtKind::Let {
                skip_value_copy, ..
            } = &mut engine.body.stmts[s].kind
            {
                *skip_value_copy = true;
            }
            return;
        }
        // Recurse into the value expression
        transform_expr(
            engine,
            ve,
            escaping_locals,
            hoist_stmts,
            type_table,
            callee_ids,
        );
        return;
    }

    // Other statement shapes that carry transformable expressions / blocks.
    enum Shape {
        Expr(ExprId),
        If(Option<ExprId>, BlockId, Option<BlockId>),
        Labeled(BlockId),
        Break(ExprId),
        None,
    }
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::Expr(e) => e.as_expr().map_or(Shape::None, Shape::Expr),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => Shape::If(condition.as_expr(), *then_block, *else_block),
        StmtKind::LabeledBlock { block, .. } => Shape::Labeled(*block),
        StmtKind::Break { value: Some(e), .. } => e.as_expr().map_or(Shape::None, Shape::Break),
        // Don't recurse into nested loops.
        _ => Shape::None,
    };
    match shape {
        Shape::Expr(e) | Shape::Break(e) => {
            transform_expr(
                engine,
                e,
                escaping_locals,
                hoist_stmts,
                type_table,
                callee_ids,
            );
        }
        Shape::If(cond, tb, eb) => {
            if let Some(cond) = cond {
                transform_expr(
                    engine,
                    cond,
                    escaping_locals,
                    hoist_stmts,
                    type_table,
                    callee_ids,
                );
            }
            transform_stmts_in_block(
                engine,
                tb,
                escaping_locals,
                hoist_stmts,
                type_table,
                callee_ids,
            );
            if let Some(eb) = eb {
                transform_stmts_in_block(
                    engine,
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    type_table,
                    callee_ids,
                );
            }
        }
        Shape::Labeled(b) => {
            transform_stmts_in_block(
                engine,
                b,
                escaping_locals,
                hoist_stmts,
                type_table,
                callee_ids,
            );
        }
        Shape::None => {}
    }
}

fn transform_expr(
    engine: &mut Engine,
    e: ExprId,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
    callee_ids: &TmplCalleeIds,
) {
    // Mirror the original's restricted arm set: __tmpl in non-Let contexts is
    // not hoisted, so only these shapes recurse.
    enum Walk {
        Exprs(Vec<ExprId>),
        CondBlocks(Option<ExprId>, BlockId, Option<BlockId>),
        Block(BlockId),
        None,
    }
    let walk = match &engine.body.exprs[e].kind {
        ExprKind::Call { args, .. } => {
            Walk::Exprs(args.iter().filter_map(|a| a.expr.as_expr()).collect())
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let mut v: Vec<ExprId> = receiver.as_expr().into_iter().collect();
            v.extend(args.iter().filter_map(|a| a.expr.as_expr()));
            Walk::Exprs(v)
        }
        ExprKind::Binary { left, right, .. } => Walk::Exprs(
            [*left, *right]
                .into_iter()
                .filter_map(Operand::as_expr)
                .collect(),
        ),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. } => {
            Walk::Exprs(inner.as_expr().into_iter().collect())
        }
        ExprKind::Assign { target, value } => {
            Walk::Exprs(std::iter::once(*target).chain(value.as_expr()).collect())
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => Walk::CondBlocks(condition.as_expr(), *then_branch, *else_branch),
        ExprKind::LabeledBlock { block, .. } | ExprKind::Block(block) => Walk::Block(*block),
        _ => Walk::None,
    };
    match walk {
        Walk::Exprs(v) => {
            for id in v {
                transform_expr(
                    engine,
                    id,
                    escaping_locals,
                    hoist_stmts,
                    type_table,
                    callee_ids,
                );
            }
        }
        Walk::CondBlocks(cond, tb, eb) => {
            if let Some(cond) = cond {
                transform_expr(
                    engine,
                    cond,
                    escaping_locals,
                    hoist_stmts,
                    type_table,
                    callee_ids,
                );
            }
            transform_stmts_in_block(
                engine,
                tb,
                escaping_locals,
                hoist_stmts,
                type_table,
                callee_ids,
            );
            if let Some(eb) = eb {
                transform_stmts_in_block(
                    engine,
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    type_table,
                    callee_ids,
                );
            }
        }
        Walk::Block(b) => {
            transform_stmts_in_block(
                engine,
                b,
                escaping_locals,
                hoist_stmts,
                type_table,
                callee_ids,
            );
        }
        Walk::None => {}
    }
}

/// Check if a `__tmpl` block has the expected pattern.
///
/// Before lowering:
///   `let mut __r = String::with_capacity(N);`
///
/// After lowering (inlined):
///   `let mut __r = String { repr: array_new<u8>(N), used: 0 };`
///
/// Both end with:
///   `break __tmpl: __r;`
fn extract_tmpl_candidate(
    body: &Body,
    block: BlockId,
    callee_ids: &TmplCalleeIds,
) -> Option<TmplCandidate> {
    // First statement must be: let mut __r = ...
    let first_stmt = *body.blocks[block].stmts.first()?;
    let (buf_local_index, string_type, init_value, used_field_index, used_field_type, span) =
        match &body.stmts[first_stmt].kind {
            StmtKind::Let {
                name,
                local_index,
                value,
                type_id,
                ..
            } if name == crate::name::TEMPLATE_RESULT_LOCAL => {
                let local_index = *local_index;
                let value = *value;
                let type_id = *type_id;
                let init_value = value.as_expr()?;
                let value_span = body.exprs[init_value].span;
                // Operand promotion wraps the inlined builder init in a block with a
                // dead capacity binding (`{ let capacity = N; String { … } }`); the
                // hoist reuses the whole block as the init value (self-contained), so
                // verify against the block's tail struct but keep `init_value` whole.
                let struct_view = unwrap_block_tail(body, init_value);
                // Try pre-lowered form: String::with_capacity(N)
                if let ExprKind::Call { func_id, .. } = &body.exprs[struct_view].kind
                    && TmplCalleeIds::is(&callee_ids.with_capacity, *func_id)
                {
                    return Some(TmplCandidate {
                        buf_local_index: local_index,
                        init_value,
                        string_type: type_id,
                        used_field_index: SeqField::Len.index(),
                        used_field_type: TypeTable::I32,
                        span: value_span,
                    });
                }
                // Try post-lowered form: String { repr: array_new<u8>(N), used: 0 }
                if let ExprKind::StructLiteral {
                    struct_name,
                    fields,
                    ..
                } = &body.exprs[struct_view].kind
                {
                    if struct_name == "String" {
                        // Verify the repr field contains an array_new call
                        let repr_field = fields
                            .iter()
                            .find(|f| f.name == SeqField::Backing.field_name())?;
                        if !repr_field
                            .value
                            .as_expr()
                            .is_some_and(|rv| array_new_has_capacity(body, rv, callee_ids))
                        {
                            return None;
                        }
                        // Verify used field is 0
                        let used_field = fields
                            .iter()
                            .find(|f| f.name == SeqField::Len.field_name())?;
                        if body.operand_const_int(used_field.value) != Some(0) {
                            return None;
                        }
                        (
                            local_index,
                            type_id,
                            init_value,
                            used_field.field_index,
                            body.operand_type(used_field.value),
                            value_span,
                        )
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
            _ => return None,
        };

    // Last statement must be: break __tmpl: __r
    let last_stmt = *body.blocks[block].stmts.last()?;
    match &body.stmts[last_stmt].kind {
        StmtKind::Break {
            label: Some(label),
            value: Some(val),
        } if label == crate::name::TEMPLATE_BLOCK_LABEL => {
            match val.as_expr().map(|ve| &body.exprs[ve].kind) {
                Some(ExprKind::Local { index, .. }) if *index == buf_local_index => {}
                _ => return None,
            }
        }
        _ => return None,
    }

    Some(TmplCandidate {
        buf_local_index,
        init_value,
        string_type,
        used_field_index,
        used_field_type,
        span,
    })
}

/// Owned descriptor of a Formatter struct literal's fields, extracted from the
/// arena so the candidacy decision needs no borrow held across construction.
struct FmtFields {
    struct_name: String,
    fields: Vec<(String, u32, Operand)>,
    struct_type: TypeId,
    value_type_id: TypeId,
    value_span: Span,
}

/// Collect all Formatter struct literals in a `__tmpl` block that can be hoisted.
///
/// Detects three patterns:
///   1. Assign: `__local_N = Formatter { ... }`
///   2. Let:    `let mut __f = Formatter { ... }`
///   3. `LabeledBlock` (inlined `Formatter::new)`:
///      `let __f = label: { let buf = &mut __tmpl_buf; break: Formatter { ..., buf } }`
///      or `__local_N = label: { ... break: Formatter { ... } }`
fn extract_fmt_candidates(
    engine: &mut Engine,
    block: BlockId,
    hoisted_buf_index: u32,
    type_table: &RefCell<TypeTable>,
    callee_ids: &TmplCalleeIds,
) -> Vec<FmtCandidate> {
    // Phase A (read): decide candidacy without mutating the arena.
    struct Raw {
        stmt_index: usize,
        fmt_local_index: u32,
        ff: FmtFields,
        indent_field_index: u32,
    }
    let mut raws: Vec<Raw> = Vec::new();
    {
        let type_table = type_table.borrow();
        let stmts = engine.body.blocks[block].stmts.clone();
        let len = stmts.len();
        for (i, s) in stmts.iter().enumerate() {
            if i == 0 || i == len - 1 {
                continue;
            }

            // Try to extract (local_index, value_expr_id) from the statement.
            let (fmt_local_index, value_expr): (u32, ExprId) = match &engine.body.stmts[*s].kind {
                StmtKind::Expr(Operand::Expr(expr)) => {
                    let ExprKind::Assign { target, value } = &engine.body.exprs[*expr].kind else {
                        continue;
                    };
                    let ExprKind::Local { index, .. } = &engine.body.exprs[*target].kind else {
                        continue;
                    };
                    let Some(value_expr) = value.as_expr() else {
                        continue;
                    };
                    (*index, value_expr)
                }
                StmtKind::Let {
                    local_index, value, ..
                } => {
                    let Some(value_expr) = value.as_expr() else {
                        continue;
                    };
                    (*local_index, value_expr)
                }
                _ => continue,
            };

            let Some(ff) =
                extract_formatter_fields(engine.body, value_expr, hoisted_buf_index, callee_ids)
            else {
                continue;
            };

            if ff.struct_name != "Formatter" {
                continue;
            }

            // Find the `indent` field index
            let Some((_, indent_field_index, _)) =
                ff.fields.iter().find(|(name, _, _)| name == "indent")
            else {
                continue;
            };
            let indent_field_index = *indent_field_index;

            // Verify all non-buf fields are constant (literals)
            let all_const = ff.fields.iter().all(|(name, _, value)| {
                if name == "buf" {
                    return true;
                }
                is_constant_operand(engine.body, *value)
            });
            if !all_const {
                continue;
            }

            // Verify the Formatter type is a struct
            let resolved = type_table.get(ff.value_type_id);
            if !matches!(resolved, crate::tir::ResolvedType::Struct { .. }) {
                continue;
            }

            raws.push(Raw {
                stmt_index: i,
                fmt_local_index,
                ff,
                indent_field_index,
            });
        }
    }

    // Phase B (build): synthesize the normalized Formatter literal per candidate.
    let mut candidates = Vec::new();
    for raw in raws {
        let value_type_id = raw.ff.value_type_id;
        let value_span = raw.ff.value_span;
        let struct_type = raw.ff.struct_type;
        let mut init_fields: Vec<ArenaStructField> = Vec::new();
        for (name, field_index, value) in &raw.ff.fields {
            let new_value: Operand = if name == "buf" {
                // Normalize buf to &mut __tmpl_buf
                let buf_ty = engine.body.operand_type(*value);
                let local = engine.alloc_expr(
                    ExprKind::Local {
                        index: hoisted_buf_index,
                        name: format!("__tmpl_buf_{hoisted_buf_index}"),
                    },
                    buf_ty,
                    value_span,
                );
                engine
                    .alloc_expr(
                        ExprKind::Unary {
                            op: NirUnaryOp::MutRef,
                            expr: local.into(),
                        },
                        buf_ty,
                        value_span,
                    )
                    .into()
            } else {
                // A verified constant leaf. A promoted `Operand::Value` points
                // into the shared pool and is reused directly; a skeleton literal
                // is shallow-copied (a constant leaf has no children).
                match value {
                    Operand::Value(_) => *value,
                    Operand::Expr(e) => {
                        let node = engine.body.exprs[*e].clone();
                        engine.alloc_expr(node.kind, node.type_id, node.span).into()
                    }
                }
            };
            init_fields.push(ArenaStructField {
                name: name.clone(),
                value: new_value,
                field_index: *field_index,
            });
        }
        let init_value = engine.alloc_expr(
            ExprKind::StructLiteral {
                struct_type,
                struct_name: raw.ff.struct_name.clone(),
                fields: init_fields,
            },
            value_type_id,
            value_span,
        );
        candidates.push(FmtCandidate {
            stmt_index: raw.stmt_index,
            fmt_local_index: raw.fmt_local_index,
            init_value,
            formatter_type: struct_type,
            indent_field_index: raw.indent_field_index,
            span: value_span,
        });
    }
    candidates
}

/// Extract Formatter struct literal fields from a value expression.
///
/// Handles:
///   - Direct `StructLiteral { ... }` where buf references hoisted buffer
///   - `LabeledBlock { let buf = ...; break: StructLiteral { ..., buf } }`
///     where the intermediate `buf` local traces back to the hoisted buffer
fn extract_formatter_fields(
    body: &Body,
    value: ExprId,
    hoisted_buf_index: u32,
    callee_ids: &TmplCalleeIds,
) -> Option<FmtFields> {
    let value_type_id = body.exprs[value].type_id;
    let value_span = body.exprs[value].span;
    match &body.exprs[value].kind {
        ExprKind::StructLiteral {
            struct_name,
            fields,
            struct_type,
        } => {
            let buf_field = fields.iter().find(|f| f.name == "buf")?;
            if !buf_field_references_local_operand(
                body,
                buf_field.value,
                hoisted_buf_index,
                callee_ids,
            ) {
                return None;
            }
            Some(FmtFields {
                struct_name: struct_name.clone(),
                fields: fields
                    .iter()
                    .map(|f| (f.name.clone(), f.field_index, f.value))
                    .collect(),
                struct_type: *struct_type,
                value_type_id,
                value_span,
            })
        }
        ExprKind::LabeledBlock { block, .. } => {
            // Pattern: { let buf = &mut __tmpl_buf; break label: Formatter { ..., buf } }
            // or: { __local = __tmpl_buf; break: Formatter { ..., buf: ref.as_non_null(__local) } }
            let block = *block;
            let break_stmt = *body.blocks[block].stmts.last()?;
            let break_value = match &body.stmts[break_stmt].kind {
                StmtKind::Break { value: Some(v), .. } => *v,
                _ => return None,
            };
            extract_formatter_fields_from_block(
                body,
                block,
                break_value.as_expr()?,
                hoisted_buf_index,
                value_type_id,
                value_span,
                callee_ids,
            )
        }
        ExprKind::Block(block) => {
            // After `branch_prune`'s C3 rewrite flattens
            // `__inline_Formatter__new_*: { …; break: Formatter { … } }`
            // into a plain `Block { …; Expr(Formatter { … }) }`, the
            // surface shape `extract_formatter_fields` used to match on
            // (`LabeledBlock`) is gone. Defensively support the flattened
            // shape here so a future change to the inliner (e.g. one that
            // leaves a multi-use `buf` binding `copy_prop` can't fold) does
            // not silently disable Formatter hoisting.
            let block = *block;
            let tail_stmt = *body.blocks[block].stmts.last()?;
            let StmtKind::Expr(Operand::Expr(tail)) = &body.stmts[tail_stmt].kind else {
                return None;
            };
            extract_formatter_fields_from_block(
                body,
                block,
                *tail,
                hoisted_buf_index,
                value_type_id,
                value_span,
                callee_ids,
            )
        }
        _ => None,
    }
}

/// Shared "block carrying a `Formatter { … }` value" matcher used by both
/// the `LabeledBlock` (inlined `Formatter::new`) and the post-C3 `Block`
/// (flattened version of the same shape) arms of
/// `extract_formatter_fields`. `value_expr` is the producing expression —
/// either the broken value or the trailing `Expr` stmt.
fn extract_formatter_fields_from_block(
    body: &Body,
    block: BlockId,
    value_expr: ExprId,
    hoisted_buf_index: u32,
    value_type_id: TypeId,
    value_span: Span,
    callee_ids: &TmplCalleeIds,
) -> Option<FmtFields> {
    let ExprKind::StructLiteral {
        struct_name,
        fields,
        struct_type,
    } = &body.exprs[value_expr].kind
    else {
        return None;
    };

    let make = || FmtFields {
        struct_name: struct_name.clone(),
        fields: fields
            .iter()
            .map(|f| (f.name.clone(), f.field_index, f.value))
            .collect(),
        struct_type: *struct_type,
        value_type_id,
        value_span,
    };

    // Check if buf traces to hoisted buffer (directly or via intermediate local)
    let buf_field = fields.iter().find(|f| f.name == "buf")?;
    if buf_field_references_local_operand(body, buf_field.value, hoisted_buf_index, callee_ids) {
        return Some(make());
    }

    // Trace through intermediate local in the block
    let buf_inner_local = extract_local_from_ref(body, buf_field.value.as_expr()?, callee_ids)?;
    for s in &body.blocks[block].stmts {
        match &body.stmts[*s].kind {
            StmtKind::Let {
                local_index,
                value: let_value,
                ..
            } if *local_index == buf_inner_local => {
                if references_local_operand(body, *let_value, hoisted_buf_index) {
                    return Some(make());
                }
            }
            StmtKind::Expr(Operand::Expr(expr)) => {
                if let ExprKind::Assign { target, value: av } = &body.exprs[*expr].kind
                    && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                    && *index == buf_inner_local
                    && references_local_operand(body, *av, hoisted_buf_index)
                {
                    return Some(make());
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract the local index from a reference expression.
/// Handles `&mut Local(N)`, `Local(N)`, and `ref.as_non_null(Local(N))`.
fn extract_local_from_ref(body: &Body, e: ExprId, callee_ids: &TmplCalleeIds) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
            expr: inner,
        } => match inner.as_expr().map(|ie| &body.exprs[ie].kind) {
            Some(ExprKind::Local { index, .. }) => Some(*index),
            _ => None,
        },
        ExprKind::Call { func_id, args, .. }
            if TmplCalleeIds::is(&callee_ids.ref_as_non_null, *func_id) =>
        {
            args.first()
                .and_then(|a| match a.expr.as_expr().map(|ae| &body.exprs[ae].kind) {
                    Some(ExprKind::Local { index, .. }) => Some(*index),
                    _ => None,
                })
        }
        _ => None,
    }
}

/// Check if an expression *aliases* a specific local (the local itself or a
/// `&mut` chain to it).
///
/// Intentionally narrow: matching a non-alias *mention* (e.g. `foo(buf)`) would
/// make the caller force-rewrite a Formatter's `buf` to the wrong buffer — a
/// miscompile — so this must not be widened to a "mentions anywhere" walk such
/// as `expr_mentions_local`. Pinned by the `references_local_matches_only_aliases_not_mentions`
/// unit test below.
fn references_local_operand(body: &Body, op: Operand, local_index: u32) -> bool {
    op.as_expr()
        .is_some_and(|e| references_local(body, e, local_index))
}

fn references_local(body: &Body, e: ExprId, local_index: u32) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => *index == local_index,
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => references_local_operand(body, *inner, local_index),
        _ => false,
    }
}

/// Check if a `buf` field expression references the given local (the hoisted String buffer).
/// Handles both `&mut local` (NIR form) and `ref.as_non_null(local)` patterns.
fn buf_field_references_local_operand(
    body: &Body,
    op: Operand,
    local_index: u32,
    callee_ids: &TmplCalleeIds,
) -> bool {
    op.as_expr()
        .is_some_and(|e| buf_field_references_local(body, e, local_index, callee_ids))
}

fn buf_field_references_local(
    body: &Body,
    e: ExprId,
    local_index: u32,
    callee_ids: &TmplCalleeIds,
) -> bool {
    match &body.exprs[e].kind {
        // &mut __tmpl_buf (NIR level)
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => inner
            .as_expr()
            .is_some_and(|ie| is_local(body, ie, local_index)),
        // ref.as_non_null(__tmpl_buf) (WIR level / after lowering)
        ExprKind::Call { func_id, args, .. } => {
            TmplCalleeIds::is(&callee_ids.ref_as_non_null, *func_id)
                && args.len() == 1
                && args[0]
                    .expr
                    .as_expr()
                    .is_some_and(|ae| is_local(body, ae, local_index))
        }
        _ => false,
    }
}

/// Check if an expression is a compile-time constant (literal).
fn is_constant_expr(body: &Body, e: ExprId) -> bool {
    matches!(
        &body.exprs[e].kind,
            | ExprKind::EnumConstruct { .. }
    )
}

/// Operand form of [`is_constant_expr`]: a promoted scalar (`Operand::Value`)
/// is always a compile-time constant.
fn is_constant_operand(body: &Body, op: Operand) -> bool {
    op.as_expr().is_none_or(|e| is_constant_expr(body, e))
}

/// Whether `expr` is an `array_new<u8>(N)` call carrying a capacity argument.
fn array_new_has_capacity(body: &Body, e: ExprId, callee_ids: &TmplCalleeIds) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, args, .. } => {
            TmplCalleeIds::is(&callee_ids.array_new, *func_id) && !args.is_empty()
        }
        _ => false,
    }
}

/// Unwrap a `{ let* …; <tail> }` block to its tail expression. Operand
/// promotion wraps an inlined builder init in such a block (a dead `let
/// capacity = N` binding plus the struct tail); the tail is the real
/// constructor. Returns `e` unchanged when it is not a tail-yielding block.
fn unwrap_block_tail(body: &Body, e: ExprId) -> ExprId {
    let (ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. }) = &body.exprs[e].kind else {
        return e;
    };
    match body.blocks[*b].stmts.last().map(|s| &body.stmts[*s].kind) {
        Some(StmtKind::Expr(op)) => op.as_expr().map_or(e, |t| unwrap_block_tail(body, t)),
        _ => e,
    }
}

/// Transform a `__tmpl` block to reuse a hoisted String.
///
/// The entire String (not just the backing array) is hoisted before the loop.
/// Inside the block, `let mut __r = String { ... }` is replaced with
/// `__tmpl_buf.used = 0` (field reset), and all references to `__r` are
/// renamed to `__tmpl_buf`. The outer Let binding gets `skip_value_copy = true`
/// so the bound variable aliases the hoisted String directly.
fn transform_tmpl_block(
    engine: &mut Engine,
    block: BlockId,
    candidate: &TmplCandidate,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
    callee_ids: &TmplCalleeIds,
) {
    let span = candidate.span;
    let string_type = candidate.string_type;

    // Allocate a new local for the hoisted String via the engine.
    let buf_local_name = format!("__tmpl_buf_{}", engine.locals().len());
    let buf_local_index =
        engine.alloc_local(buf_local_name.clone(), string_type, /* is_mut */ true);

    // Hoist statement: let mut __tmpl_buf_N = String { repr: array_new(N), used: 0 };
    // Reuse the original init-value subtree (its old `Let` is replaced below).
    let hoist_let = engine.alloc_stmt(
        StmtKind::Let {
            name: buf_local_name.clone(),
            local_index: buf_local_index,
            is_mut: true,
            is_reactive: false,
            type_id: string_type,
            value: candidate.init_value.into(),
            skip_value_copy: false,
        },
        span,
    );
    hoist_stmts.push(hoist_let);

    // Replace the first statement (let mut __r = String { ... }) with a field reset:
    // __tmpl_buf_N.used = 0;
    let reset_stmt = build_field_reset(
        engine,
        buf_local_index,
        &buf_local_name,
        string_type,
        candidate.used_field_index,
        candidate.used_field_type,
        SeqField::Len.field_name(),
        span,
    );
    let mut new_stmts = engine.body.blocks[block].stmts.clone();
    new_stmts[0] = reset_stmt;
    engine.set_block_stmts(block, new_stmts);

    // Rename all references from __r (old local index) to __tmpl_buf_N (new local index)
    let old_index = candidate.buf_local_index;
    for s in engine.body.blocks[block].stmts.clone() {
        rename_local_in_stmt(engine, s, old_index, buf_local_index, &buf_local_name);
    }
    // The init `let` is gone, so any surviving mention of `__r` would read an
    // uninitialized local — an incomplete rename is a compiler bug.
    for s in &engine.body.blocks[block].stmts {
        assert!(
            !stmt_mentions_local(engine.body, *s, old_index),
            "tmpl_hoist: rename left a mention of replaced template buffer local {old_index}"
        );
    }

    // Phase 2: Hoist Formatter struct literals as well.
    // After the String rename above, the block may contain one or more Formatter
    // creations (direct struct literals or inlined Formatter::new LabeledBlocks).
    // Each distinct Formatter is hoisted to its own local before the loop.
    let fmt_candidates =
        extract_fmt_candidates(engine, block, buf_local_index, type_table, callee_ids);
    if !fmt_candidates.is_empty() {
        transform_fmts_in_tmpl_block(engine, block, &fmt_candidates, hoist_stmts);
    }
}

/// Build a `target_local.<field> = 0` statement (an `Expr(Assign)` over a
/// `FieldAccess`), returning its arena id.
#[allow(clippy::too_many_arguments)]
fn build_field_reset(
    engine: &mut Engine,
    local_index: u32,
    local_name: &str,
    local_type: TypeId,
    field_index: u32,
    field_type: TypeId,
    field_name: &str,
    span: Span,
) -> StmtId {
    let local = engine.alloc_expr(
        ExprKind::Local {
            index: local_index,
            name: local_name.to_string(),
        },
        local_type,
        span,
    );
    let field = engine.alloc_expr(
        ExprKind::FieldAccess {
            expr: local.into(),
            field_index,
            field_name: field_name.to_string(),
        },
        field_type,
        span,
    );
    let zero = engine.const_operand(
        crate::nir_value_graph::ValueKind::Int(0, field_type),
        field_type,
    );
    let assign = engine.alloc_expr(
        ExprKind::Assign {
            target: field,
            value: zero,
        },
        TypeTable::UNIT,
        span,
    );
    engine.alloc_stmt(StmtKind::Expr(assign.into()), span)
}

/// Hoist Formatter struct literals out of a `__tmpl` block.
///
/// Each candidate gets its own hoisted local. Replaces the struct literal with an
/// `indent` field reset, and renames all references to the hoisted local.
///
/// Processes candidates in reverse order so that `stmt_index` values remain valid
/// as we replace statements.
fn transform_fmts_in_tmpl_block(
    engine: &mut Engine,
    block: BlockId,
    candidates: &[FmtCandidate],
    hoist_stmts: &mut Vec<StmtId>,
) {
    // Sort by stmt_index ascending to compute rename ranges
    let mut sorted_candidates: Vec<_> = candidates.iter().collect();
    sorted_candidates.sort_by_key(|c| c.stmt_index);

    // Build a mapping from each candidate to its hoisted local and rename range.
    // When multiple candidates share the same fmt_local_index, each one's rename
    // range extends from its stmt_index to the next candidate's stmt_index (exclusive)
    // that shares the same local. For the last candidate with a given local, the
    // range extends to the end of the block.
    struct HoistInfo {
        hoisted_index: u32,
        hoisted_name: String,
        stmt_index: usize,
        rename_start: usize,
        rename_end: usize, // exclusive
        old_fmt_index: u32,
        formatter_type: TypeId,
        indent_field_index: u32,
        init_value: ExprId,
        span: Span,
    }
    let block_len = engine.body.blocks[block].stmts.len();
    let mut hoist_infos = Vec::new();

    for (pos, candidate) in sorted_candidates.iter().enumerate() {
        // Keep this name aligned with the corresponding `Let` statement
        // built below. WIR naming is primarily derived from discovered
        // `Let`s by local index, with `tir_func.locals[idx].name` used as a
        // fallback when no `Let` is found, so matching names mainly
        // improves fallback / debug output consistency.
        let hoisted_name = format!("__fmt_buf_{}", engine.locals().len());
        let fmt_local_index = engine.alloc_local(
            hoisted_name.clone(),
            candidate.formatter_type,
            /* is_mut */ true,
        );

        // Find the next candidate that shares the same fmt_local_index
        let rename_end = sorted_candidates[pos + 1..]
            .iter()
            .find(|c| c.fmt_local_index == candidate.fmt_local_index)
            .map(|c| c.stmt_index)
            .unwrap_or(block_len);

        hoist_infos.push(HoistInfo {
            hoisted_index: fmt_local_index,
            hoisted_name,
            stmt_index: candidate.stmt_index,
            rename_start: candidate.stmt_index,
            rename_end,
            old_fmt_index: candidate.fmt_local_index,
            formatter_type: candidate.formatter_type,
            indent_field_index: candidate.indent_field_index,
            init_value: candidate.init_value,
            span: candidate.span,
        });
    }

    for info in &hoist_infos {
        // Hoist statement: let mut __fmt_buf_N = Formatter { fill: ..., buf: ... };
        // Reuse the normalized Formatter literal built during extraction.
        let hoist_let = engine.alloc_stmt(
            StmtKind::Let {
                name: info.hoisted_name.clone(),
                local_index: info.hoisted_index,
                is_mut: true,
                is_reactive: false,
                type_id: info.formatter_type,
                value: info.init_value.into(),
                skip_value_copy: false,
            },
            info.span,
        );
        hoist_stmts.push(hoist_let);

        // Replace the Formatter struct literal with an indent field reset:
        //   __fmt_buf_N.indent = 0;
        //
        // IMPORTANT: Do NOT remove this indent = 0 reset! Format functions
        // (especially pretty-print with `:#?`) may modify the `indent` field
        // during formatting. Without this reset, the indent value would
        // accumulate across loop iterations, causing incorrect indentation.
        let indent_reset = build_field_reset(
            engine,
            info.hoisted_index,
            &info.hoisted_name,
            info.formatter_type,
            info.indent_field_index,
            TypeTable::I32,
            "indent",
            info.span,
        );
        let mut new_stmts = engine.body.blocks[block].stmts.clone();
        new_stmts[info.stmt_index] = indent_reset;
        engine.set_block_stmts(block, new_stmts);

        // Rename references from the old Formatter local to the hoisted one,
        // only within [rename_start, rename_end) to avoid clobbering other
        // candidates that share the same original local.
        let range_stmts: Vec<StmtId> =
            engine.body.blocks[block].stmts[info.rename_start..info.rename_end].to_vec();
        for s in range_stmts {
            rename_local_in_stmt(
                engine,
                s,
                info.old_fmt_index,
                info.hoisted_index,
                &info.hoisted_name,
            );
        }
    }
}

/// Rename every `Local(old_index)` mention under `s` to `Local(new_index)`
/// named `new_name` — a pure substitution over the complete generic
/// `for_each_child` walk, so no node kind can be missed. Mentions are
/// collected first, then rewritten through `replace_expr_kind`: the `Local`
/// kind's `index` field is what the engine's use index is keyed on, so the
/// edit drops the `old_index` mention and registers a `new_index` one.
fn rename_local_in_stmt(
    engine: &mut Engine,
    s: StmtId,
    old_index: u32,
    new_index: u32,
    new_name: &str,
) {
    let mut mentions = Vec::new();
    collect_local_mentions(engine.body, NodeRef::Stmt(s), old_index, &mut mentions);
    for e in mentions {
        engine.replace_expr_kind(
            e,
            ExprKind::Local {
                index: new_index,
                name: new_name.to_string(),
            },
        );
    }
}

fn collect_local_mentions(body: &Body, node: NodeRef, index: u32, out: &mut Vec<ExprId>) {
    if let NodeRef::Expr(e) = node
        && is_local(body, e, index)
    {
        out.push(e);
        return;
    }
    body.for_each_child(node, |child| {
        collect_local_mentions(body, child, index, out);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::tir::TypeId;

    /// Build a `Body` whose root holds a single expression (built by `build`)
    /// and return its arena id so the arena-side `references_local` can be
    /// exercised directly.
    fn body_of(build: impl FnOnce(&mut Body) -> ExprId) -> (Body, ExprId) {
        let mut body = Body::empty();
        let e = build(&mut body);
        let s = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(e.into()),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s],
            span: Span::default(),
        });
        (body, e)
    }

    fn local(body: &mut Body, idx: u32) -> ExprId {
        body.exprs.push(ExprNode {
            kind: ExprKind::Local {
                index: idx,
                name: format!("__l{idx}"),
            },
            type_id: TypeId(0),
            span: Span::default(),
        })
    }

    fn unary(body: &mut Body, op: NirUnaryOp, inner: ExprId) -> ExprId {
        body.exprs.push(ExprNode {
            kind: ExprKind::Unary {
                op,
                expr: inner.into(),
            },
            type_id: TypeId(0),
            span: Span::default(),
        })
    }

    /// `foo(arg)` — a free-function call carrying `arg`. The result is an
    /// independent value, so the call only *mentions* `arg`, it is not an
    /// alias of it.
    fn call_with(body: &mut Body, arg: ExprId) -> ExprId {
        use crate::nir_arena::ArenaCallArg;
        use cranelift_entity::EntityRef;
        body.exprs.push(ExprNode {
            kind: ExprKind::Call {
                func_id: crate::nir::FuncId::new(0),
                type_args: vec![],
                args: vec![ArenaCallArg {
                    expr: arg.into(),
                    is_mut: false,
                }],
            },
            type_id: TypeId(0),
            span: Span::default(),
        })
    }

    /// `references_local` decides whether an intermediate `let buf_inner = <e>`
    /// binding makes `buf_inner` an *alias* of the hoisted template buffer.
    /// When it answers yes, `extract_fmt_candidates` force-rewrites the matched
    /// Formatter's `buf` field to `&mut <hoisted buffer>` — so a false positive
    /// redirects the Formatter to the wrong buffer (a miscompile).
    ///
    /// It must therefore match only genuine alias shapes — a bare `Local` or a
    /// `&mut` chain down to it — and reject any expression that merely *mentions*
    /// the local (a call argument, an operand, …). Broadening it to a full
    /// "mentions anywhere" walk (e.g. `expr_mentions_local`) reintroduces that
    /// miscompile; these assertions exist to fail the moment someone does.
    #[test]
    fn references_local_matches_only_aliases_not_mentions() {
        const IDX: u32 = 7;

        // Genuine aliases — must match.
        let (b, e) = body_of(|b| local(b, IDX));
        assert!(references_local(&b, e, IDX));
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            unary(b, NirUnaryOp::MutRef, l)
        });
        assert!(references_local(&b, e, IDX));
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            let inner = unary(b, NirUnaryOp::MutRef, l);
            unary(b, NirUnaryOp::MutRef, inner)
        });
        assert!(references_local(&b, e, IDX));

        // A different local is unrelated.
        let (b, e) = body_of(|b| local(b, IDX + 1));
        assert!(!references_local(&b, e, IDX));

        // Non-alias *mentions* — must NOT match (this is the guard against
        // `expr_mentions_local`, which would return true for all of these and
        // miscompile the Formatter buffer rewrite).
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            call_with(b, l)
        });
        assert!(!references_local(&b, e, IDX));
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            let c = call_with(b, l);
            unary(b, NirUnaryOp::MutRef, c)
        });
        assert!(!references_local(&b, e, IDX));
        // `&buf` (shared ref) is not the `&mut` alias shape the hoist normalizes.
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            unary(b, NirUnaryOp::Ref, l)
        });
        assert!(!references_local(&b, e, IDX));
    }

    fn block_of(body: &mut Body, stmts: Vec<StmtId>) -> BlockId {
        body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        })
    }

    fn expr_stmt(body: &mut Body, e: ExprId) -> StmtId {
        body.stmts.push(StmtNode {
            kind: StmtKind::Expr(e.into()),
            span: Span::default(),
        })
    }

    fn let_stmt(body: &mut Body, local_index: u32, value: ExprId) -> StmtId {
        body.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: format!("__l{local_index}"),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id: TypeId(0),
                value: value.into(),
                skip_value_copy: false,
            },
            span: Span::default(),
        })
    }

    /// The value chain follows `if` branch tails (the shape
    /// `out.push(if c { s } else { t })` escapes through) but stops at a call
    /// result — a `$value_copy$…` wrapper severs the alias, keeping copied
    /// escapes hoistable.
    #[test]
    fn chain_follows_if_tails_and_stops_at_calls() {
        let mut b = Body::empty();
        let l7 = local(&mut b, 7);
        let l8 = local(&mut b, 8);
        let cond = local(&mut b, 9);
        let ts = expr_stmt(&mut b, l7);
        let tb = block_of(&mut b, vec![ts]);
        let es = expr_stmt(&mut b, l8);
        let eb = block_of(&mut b, vec![es]);
        let if_expr = b.exprs.push(ExprNode {
            kind: ExprKind::If {
                condition: cond.into(),
                then_branch: tb,
                else_branch: Some(eb),
            },
            type_id: TypeId(0),
            span: Span::default(),
        });
        let mut seen = Vec::new();
        for_each_chain_local_expr(&b, if_expr, &mut |l| seen.push(l));
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![7, 8],
            "branch tails are the if's value; the condition is not"
        );

        let arg = local(&mut b, 7);
        let call = call_with(&mut b, arg);
        let mut seen = Vec::new();
        for_each_chain_local_expr(&b, call, &mut |l| seen.push(l));
        assert!(seen.is_empty(), "a call result is a new value");
    }

    /// `let t = s; foo(t)` escapes `s` through the alias edge; an alias whose
    /// target never escapes leaves the source hoistable.
    #[test]
    fn escape_propagates_through_alias_lets() {
        let mut b = Body::empty();
        let l7 = local(&mut b, 7);
        let alias = let_stmt(&mut b, 1, l7);
        let t_use = local(&mut b, 1);
        let call = call_with(&mut b, t_use);
        let call_stmt = expr_stmt(&mut b, call);
        let blk = block_of(&mut b, vec![alias, call_stmt]);
        let escaping = collect_escaping_locals(&b, blk);
        assert!(escaping.contains(&1));
        assert!(
            escaping.contains(&7),
            "escape must propagate back through the alias let"
        );

        let mut b = Body::empty();
        let l7 = local(&mut b, 7);
        let alias = let_stmt(&mut b, 1, l7);
        let blk = block_of(&mut b, vec![alias]);
        let escaping = collect_escaping_locals(&b, blk);
        assert!(
            !escaping.contains(&7),
            "an alias that never escapes must not mark its source"
        );
    }
}
