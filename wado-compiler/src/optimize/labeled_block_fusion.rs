//! LabeledBlock-variant fusion: eliminate the intermediate `Option` / `Result`
//! an inlined helper leaves at a variant-discriminating consumer, as one
//! [`Rule`] with two entry points. `apply_block` fuses the value-discarding
//! `let temp = LB; if VariantTest(temp) …`; `apply_expr` threads the
//! value-producing `match LB` of `x = f()?`. Post-inline only.
//!
//! `sroa_variant_return` runs just before `inline`, so the intermediate may
//! instead be a `[tag, slots…]` tuple. Value-discarding fusion recognises that
//! too — [`FusedValue`] is the only axis the two differ on.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirLiteralPattern, NirLocal};
use crate::nir_arena::{
    ArmData, BlockId, BlockRole, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId,
    StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::ValueKind;
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::arena_query::{
    block_yields_value, has_break_to, is_local, is_local_operand,
    promoted_read_count_at, single_payload_binding,
};
use super::sroa_variant_return::{Pad, zero_pad};

/// The slot `sroa_variant_return` reserves for the tag in every scalarized
/// variant return.
const TAG_SLOT: u32 = 0;

/// Block-level fusion rule for the unified post-inline peephole session.
/// The rule keeps no per-function state: every precondition is re-derived from
/// the current body on each `apply_block`, since fusion candidates appear and
/// disappear as neighbouring rewrites land.
pub(super) struct LabeledBlockFusionRule;

/// Build a [`LabeledBlockFusionRule`] for one function. Mirrors the
/// `build_ref_elim` / `build_elide_box_local` constructors so the peephole
/// wiring is uniform; no per-function analysis is needed yet.
pub(super) fn build_labeled_block_fusion() -> LabeledBlockFusionRule {
    LabeledBlockFusionRule
}

impl Rule for LabeledBlockFusionRule {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let Some(plan) = plan_threading(engine.body, id, engine.locals()) else {
            return false;
        };
        perform_threading(engine, id, plan);
        true
    }

    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        if stmts.len() < 2 {
            return false;
        }
        // The fused block uses value-less `break $fused_L;` to terminate each
        // arm, so the original consumer site cannot be in a position where its
        // value is observed. Refusing fusion here matches the standalone pass's
        // `yields_value && i + 2 == stmts.len()` guard.
        let yields = block_yields_value(engine, block);
        for i in 0..stmts.len() - 1 {
            let Some(info) =
                check_fusion_preconditions(engine.body, stmts[i], stmts[i + 1], engine.locals())
            else {
                continue;
            };
            // `perform_fusion` deletes the `let temp = LB` binding, so the temp
            // is dead afterwards. The precondition check only inspects the
            // consumer's arms; a use of the temp anywhere else in the function
            // (a later statement, another branch) would then read a deleted
            // local. Fuse only when every mention of the temp lives inside the
            // consumer statement.
            let consumer_uses =
                count_local_uses_in_stmt(engine.body, stmts[i + 1], info.temp_local);
            if body_uses(engine, info.temp_local) != consumer_uses {
                continue;
            }
            if yields && i + 2 == stmts.len() {
                continue;
            }
            let temp = info.temp_local;
            perform_fusion(engine, block, &stmts, i, info);
            // `perform_fusion` deleted the binding that defined `temp`; the
            // session end audits that no read of it survived. See
            // `Engine::note_elided_local`.
            engine.note_elided_local(temp);
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Fusion driver
// ---------------------------------------------------------------------------

/// Information extracted from the two statements during the precondition check.
struct FusionInfo {
    temp_local: u32,
    label: String,
    value: FusedValue,
    exits: Vec<Exit>,
}

/// How the labeled block discriminates its break values, and what the consumer
/// reads out of them.
enum FusedValue {
    /// `break L: VariantConstruct` read back as `VariantPayload(temp, case)`.
    Variant {
        case_index: u32,
        payload_type: TypeId,
        pattern_payload_binding: Option<u32>,
    },
    /// `break L: [tag, …]` — the shape `sroa_variant_return` leaves behind —
    /// read back as `temp.k`, discriminated by the constant in slot 0.
    Slots {
        tag_value: i128,
        slots: Vec<SlotRead>,
    },
}

/// One tuple slot the consumer reads, as `temp.field_index`.
struct SlotRead {
    field_index: u32,
    type_id: TypeId,
}

fn check_fusion_preconditions(
    body: &Body,
    let_s: StmtId,
    if_s: StmtId,
    locals: &[NirLocal],
) -> Option<FusionInfo> {
    check_fusion_preconditions_if_variant_test(body, let_s, if_s)
        .or_else(|| check_fusion_preconditions_match(body, let_s, if_s, locals))
        .or_else(|| check_fusion_preconditions_slot_match(body, let_s, if_s))
}

fn check_fusion_preconditions_if_variant_test(
    body: &Body,
    let_s: StmtId,
    if_s: StmtId,
) -> Option<FusionInfo> {
    // --- Stmt 1: Let { value: LabeledBlock { label, block } } ---
    let StmtKind::Let {
        local_index: temp_local,
        value: let_value,
        ..
    } = &body.stmts[let_s].kind
    else {
        return None;
    };
    let temp_local = *temp_local;
    let lv = let_value.as_expr()?;
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[lv].kind
    else {
        return None;
    };
    let label = label.clone();
    let lb_block = *lb_block;
    if body.falls_through(lb_block) {
        return None;
    }

    // --- Stmt 2: If { condition: VariantTest(Local(X), case=C), then, else } ---
    let StmtKind::If {
        condition,
        then_block,
        else_block,
    } = &body.stmts[if_s].kind
    else {
        return None;
    };
    let condition = *condition;
    let then_block = *then_block;
    let else_block = *else_block;
    let ExprKind::VariantTest {
        expr: vt_expr,
        case_index,
        ..
    } = &body.exprs[condition.as_expr()?].kind
    else {
        return None;
    };
    let case_index = *case_index;
    let ExprKind::Local {
        index: tested_idx, ..
    } = &body.exprs[vt_expr.as_expr()?].kind
    else {
        return None;
    };
    if *tested_idx != temp_local {
        return None;
    }

    // --- LabeledBlock only breaks to L with null or VariantConstruct ---
    let (payload_type, exits) =
        check_lb_breaks_and_get_payload(body, lb_block, &label, case_index)?;

    // --- temp is only used as VariantPayload(Local(X), C) in then_block,
    //     and not at all in else_block ---
    let then_uses = count_local_uses_in_block(body, then_block, temp_local);
    let payload_uses =
        count_variant_payload_uses_in_block(body, then_block, temp_local, case_index);
    if then_uses != payload_uses {
        return None;
    }
    if let Some(eb) = else_block
        && count_local_uses_in_block(body, eb, temp_local) > 0
    {
        return None;
    }

    let else_escapes = else_block.map_or_else(Escapes::default, |eb| {
        Escapes::of(body, NodeRef::Block(eb))
    });
    if !arms_stay_free(
        body,
        &exits,
        FirstArm::Case(case_index),
        &Escapes::of(body, NodeRef::Block(then_block)),
        &else_escapes,
    ) {
        return None;
    }

    Some(FusionInfo {
        temp_local,
        label,
        value: FusedValue::Variant {
            case_index,
            payload_type,
            pattern_payload_binding: None,
        },
        exits,
    })
}

fn check_fusion_preconditions_match(
    body: &Body,
    let_s: StmtId,
    if_s: StmtId,
    locals: &[NirLocal],
) -> Option<FusionInfo> {
    // --- Stmt 1: Let { value: LabeledBlock { label, block } } ---
    let StmtKind::Let {
        local_index: temp_local,
        value: let_value,
        ..
    } = &body.stmts[let_s].kind
    else {
        return None;
    };
    let temp_local = *temp_local;
    let lv = let_value.as_expr()?;
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[lv].kind
    else {
        return None;
    };
    let label = label.clone();
    let lb_block = *lb_block;
    if body.falls_through(lb_block) {
        return None;
    }

    // --- Stmt 2: Expr(Match { scrut: Local(temp), arms: [Variant, Wildcard] }) ---
    let StmtKind::Expr(Operand::Expr(match_expr)) = &body.stmts[if_s].kind else {
        return None;
    };
    let ExprKind::Match { expr: scrut, arms } = &body.exprs[*match_expr].kind else {
        return None;
    };
    if arms.len() != 2 {
        return None;
    }
    let scrut_e = scrut.as_expr()?;
    let ExprKind::Local {
        index: tested_idx, ..
    } = &body.exprs[scrut_e].kind
    else {
        return None;
    };
    if *tested_idx != temp_local {
        return None;
    }

    let arm0 = &arms[0];
    let arm1 = &arms[1];
    // Both arms must be guard-free; arm0 Variant, arm1 Wildcard.
    let arm0_is_variant = matches!(&body.pats[arm0.pattern].kind, PatKind::Variant { .. });
    let arm1_is_wildcard = matches!(&body.pats[arm1.pattern].kind, PatKind::Wildcard);
    if !(arm0_is_variant && arm1_is_wildcard && arm0.guard.is_none() && arm1.guard.is_none()) {
        return None;
    }
    let variant_arm_body = arm0.body;
    let else_arm_body = arm1.body;

    let PatKind::Variant {
        case_index,
        bindings,
        ..
    } = &body.pats[arm0.pattern].kind
    else {
        return None;
    };
    let case_index = *case_index;

    // At most one payload binding slot.
    let pattern_payload_binding = single_payload_binding(body, bindings)?;

    // --- LabeledBlock only breaks to L with null or VariantConstruct ---
    let (payload_type, exits) =
        check_lb_breaks_and_get_payload(body, lb_block, &label, case_index)?;

    // --- The reused binding slot must already be declared with payload type. ---
    if let Some(binding) = pattern_payload_binding
        && locals
            .get(binding as usize)
            .is_none_or(|local| local.type_id != payload_type)
    {
        return None;
    }

    // --- temp must not be read outside the Match scrutinee position. ---
    if count_local_uses_in_operand(body, variant_arm_body, temp_local) > 0 {
        return None;
    }
    if count_local_uses_in_operand(body, else_arm_body, temp_local) > 0 {
        return None;
    }

    if !arms_stay_free(
        body,
        &exits,
        FirstArm::Case(case_index),
        &Escapes::of_operand(body, variant_arm_body),
        &Escapes::of_operand(body, else_arm_body),
    ) {
        return None;
    }

    Some(FusionInfo {
        temp_local,
        label,
        value: FusedValue::Variant {
            case_index,
            payload_type,
            pattern_payload_binding,
        },
        exits,
    })
}

/// `let t = L: { … break L: [tag, …] … }; match t.0 { K => A, _ => B }` — the
/// consumer `sroa_variant_return` leaves once the helper returns its variant as
/// a tuple. Structurally the variant recogniser above with the tag in slot 0
/// and the payloads in slots 1…N.
fn check_fusion_preconditions_slot_match(
    body: &Body,
    let_s: StmtId,
    if_s: StmtId,
) -> Option<FusionInfo> {
    let StmtKind::Let {
        local_index: temp_local,
        value: let_value,
        ..
    } = &body.stmts[let_s].kind
    else {
        return None;
    };
    let temp_local = *temp_local;
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[let_value.as_expr()?].kind
    else {
        return None;
    };
    let (label, lb_block) = (label.clone(), *lb_block);
    if body.falls_through(lb_block) {
        return None;
    }

    let StmtKind::Expr(Operand::Expr(match_expr)) = &body.stmts[if_s].kind else {
        return None;
    };
    let ExprKind::Match { expr: scrut, arms } = &body.exprs[*match_expr].kind else {
        return None;
    };
    if arms.len() != 2 {
        return None;
    }
    if tag_slot_of(body, scrut.as_expr()?) != Some((temp_local, TAG_SLOT)) {
        return None;
    }

    let (tag_arm, else_arm) = (&arms[0], &arms[1]);
    if tag_arm.guard.is_some() || else_arm.guard.is_some() {
        return None;
    }
    if !matches!(&body.pats[else_arm.pattern].kind, PatKind::Wildcard) {
        return None;
    }
    let PatKind::Literal(NirLiteralPattern::I128(tag_value)) = &body.pats[tag_arm.pattern].kind
    else {
        return None;
    };
    let tag_value = *tag_value;

    // The tag arm reads the temp only as `temp.k`; the wildcard arm not at all.
    let slots = slot_reads_in_operand(body, tag_arm.body, temp_local)?;
    if count_local_uses_in_operand(body, else_arm.body, temp_local) > 0 {
        return None;
    }

    // Every exit must carry a tuple literal whose tag slot is a constant, so
    // each one selects an arm at fusion time, and whose unread elements are
    // pure — fusion drops those.
    let read: IndexSet<u32> = slots.iter().map(|s| s.field_index).collect();
    let exits = check_lb_breaks_are_tagged_tuples(body, lb_block, &label, tag_value, &read)?;

    if !arms_stay_free(
        body,
        &exits,
        FirstArm::Tag(tag_value),
        &Escapes::of_operand(body, tag_arm.body),
        &Escapes::of_operand(body, else_arm.body),
    ) {
        return None;
    }

    Some(FusionInfo {
        temp_local,
        label,
        value: FusedValue::Slots { tag_value, slots },
        exits,
    })
}

/// `Local(temp).k` → `(temp, k)`.
fn tag_slot_of(body: &Body, e: ExprId) -> Option<(u32, u32)> {
    let ExprKind::FieldAccess {
        expr, field_index, ..
    } = &body.exprs[e].kind
    else {
        return None;
    };
    let ExprKind::Local { index, .. } = &body.exprs[expr.as_expr()?].kind else {
        return None;
    };
    Some((*index, *field_index))
}

/// What sends an exit to the consumer's first arm rather than its other one.
#[derive(Clone, Copy)]
enum FirstArm {
    /// The case the exit's variant constructs.
    Case(u32),
    /// The constant in the tag slot of the exit's tuple.
    Tag(i128),
}

impl FirstArm {
    fn takes(self, body: &Body, value: Option<Operand>) -> bool {
        let Some(e) = value.and_then(Operand::as_expr) else {
            return false;
        };
        match self {
            Self::Case(case) => matches!(&body.exprs[e].kind,
                ExprKind::VariantConstruct { case_index, .. } if *case_index == case),
            Self::Tag(tag) => matches!(&body.exprs[e].kind,
                ExprKind::TupleLiteral { elements } if break_tag_value(body, elements) == Some(tag)),
        }
    }
}

/// The value an exit carries.
fn exit_value(body: &Body, exit: StmtId) -> Option<Operand> {
    let StmtKind::Break { value, .. } = body.stmts[exit].kind else {
        unreachable!("an exit is a break")
    };
    value
}

/// Whether the arm each exit selects, cloned in place of the exit, keeps every
/// jump out of it.
fn arms_stay_free(
    body: &Body,
    exits: &[Exit],
    first_arm: FirstArm,
    first: &Escapes,
    other: &Escapes,
) -> bool {
    exits.iter().all(|exit| {
        let arm = if first_arm.takes(body, exit_value(body, exit.stmt)) {
            first
        } else {
            other
        };
        !arm.captured_at(&exit.scope)
    })
}

/// A Match arm body as a block the fusion can re-parent, wrapping the body in a
/// one-statement block. Engine-routed so the new stmt/block ids are registered
/// in the parent map.
///
/// Handing back the arm's own block instead would drop the expression node that
/// held it, and the label on that node is what the arm's own breaks name.
/// `const_branch_prune` flattens the wrapper again once nothing names it.
fn arm_body_into_block(engine: &mut Engine, arm_body: ExprId, fallback_span: Span) -> BlockId {
    let stmt = engine.alloc_stmt(StmtKind::Expr(arm_body.into()), fallback_span);
    engine.alloc_block(vec![stmt], fallback_span)
}

/// Like [`arm_body_into_block`] but accepts an `Operand`: a promoted pure value
/// (e.g. a unit arm body) has no skeleton node, so wrap it as a single-statement
/// block carrying the value operand.
fn arm_body_operand_into_block(
    engine: &mut Engine,
    arm_body: Operand,
    fallback_span: Span,
) -> BlockId {
    if let Some(e) = arm_body.as_expr() {
        arm_body_into_block(engine, e, fallback_span)
    } else {
        let stmt = engine.alloc_stmt(StmtKind::Expr(arm_body), fallback_span);
        engine.alloc_block(vec![stmt], fallback_span)
    }
}

// Shared label-exit walk: one traversal answers every check, and the rewrites
// take the exits it records rather than walking again, so no rewrite reaches an
// exit its check did not see. It honours label shadowing, rejects an exit
// hidden in an expression it does not descend, and hands each exit to an
// [`ExitSink`] encoding the per-check policy. A promoted `Operand::Value` is
// accepted vacuously, carrying no skeleton subtree and hence no break.

/// Per-exit policy for the shared [`walk_exits`] traversal.
trait ExitSink {
    /// Handle one `break <label>: value` exit. Returning `false` aborts the
    /// walk with an overall `false`.
    fn visit(&mut self, body: &Body, value: Option<Operand>) -> bool;
    /// Descend structurally into `Match` / `Switch` arms (the coverage the
    /// threading rewrite handles) rather than treating the node as opaque.
    fn descend_branches(&self) -> bool;
}

/// One `break label` exit, with what encloses it inside the labeled block.
struct Exit {
    stmt: StmtId,
    scope: ExitScope,
}

/// The blocks and loops between an exit and its labeled block: what a consumer
/// arm cloned in place of the exit would sit inside.
#[derive(Clone, Default)]
struct ExitScope {
    labels: Vec<String>,
    loops: u32,
}

/// Every `break label` exit of `block`, once `sink` has accepted each.
fn walk_exits<S: ExitSink>(
    body: &Body,
    block: BlockId,
    label: &str,
    sink: &mut S,
) -> Option<Vec<Exit>> {
    let mut walk = ExitWalk {
        label,
        sink,
        scope: ExitScope::default(),
        exits: Vec::new(),
    };
    walk.block(body, block).then_some(walk.exits)
}

struct ExitWalk<'s, S> {
    label: &'s str,
    sink: &'s mut S,
    scope: ExitScope,
    exits: Vec<Exit>,
}

impl<S: ExitSink> ExitWalk<'_, S> {
    fn block(&mut self, body: &Body, block: BlockId) -> bool {
        body.blocks[block].stmts.iter().all(|s| self.stmt(body, *s))
    }

    fn labeled(&mut self, body: &Body, label: &str, block: BlockId) -> bool {
        self.scope.labels.push(label.to_owned());
        let ok = self.block(body, block);
        self.scope.labels.pop();
        ok
    }

    fn stmt(&mut self, body: &Body, s: StmtId) -> bool {
        match &body.stmts[s].kind {
            StmtKind::Break {
                label: Some(l),
                value,
            } if l == self.label => {
                self.exits.push(Exit {
                    stmt: s,
                    scope: self.scope.clone(),
                });
                self.sink.visit(body, *value)
            }
            StmtKind::LabeledBlock { label: l, .. } if l == self.label => true,
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.operand(body, *condition)
                    && self.block(body, *then_block)
                    && else_block.is_none_or(|eb| self.block(body, eb))
            }
            StmtKind::Loop { body: b } => {
                self.scope.loops += 1;
                let ok = self.block(body, *b);
                self.scope.loops -= 1;
                ok
            }
            StmtKind::LabeledBlock { label, block, .. } => self.labeled(body, label, *block),
            StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
                self.operand(body, *value)
            }
            StmtKind::Expr(op) => self.operand(body, *op),
            StmtKind::Return { value } | StmtKind::Break { value, .. } => {
                value.is_none_or(|v| self.operand(body, v))
            }
            StmtKind::Continue => true,
        }
    }

    fn operand(&mut self, body: &Body, op: Operand) -> bool {
        op.as_expr().is_none_or(|e| self.expr(body, e))
    }

    fn expr(&mut self, body: &Body, e: ExprId) -> bool {
        match &body.exprs[e].kind {
            ExprKind::LabeledBlock {
                label: l, block, ..
            } => l == self.label || self.labeled(body, l, *block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.operand(body, *condition)
                    && self.block(body, *then_branch)
                    && else_branch.is_none_or(|eb| self.block(body, eb))
            }
            ExprKind::Match { expr, arms } if self.sink.descend_branches() => {
                self.operand(body, *expr)
                    && arms.iter().all(|arm| {
                        self.operand(body, arm.body)
                            && arm.guard.is_none_or(|g| self.operand(body, g))
                    })
            }
            ExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } if self.sink.descend_branches() => {
                self.operand(body, *scrutinee)
                    && arms.iter().all(|b| self.block(body, *b))
                    && self.block(body, *default)
            }
            // Opaque expression: an exit hidden inside is one no rewrite reaches.
            _ => !has_break_to(body, NodeRef::Expr(e), self.label),
        }
    }
}

/// The jumps a consumer arm makes out of itself: the labels its breaks name
/// past its own blocks, and whether an unlabeled `break` or `continue` leaves it.
#[derive(Default)]
struct Escapes {
    labels: IndexSet<String>,
    loop_exit: bool,
}

impl Escapes {
    fn of_operand(body: &Body, op: Operand) -> Self {
        op.as_expr()
            .map_or_else(Self::default, |e| Self::of(body, NodeRef::Expr(e)))
    }

    fn of(body: &Body, node: NodeRef) -> Self {
        let mut escapes = Self::default();
        escapes.collect(body, node, &mut Vec::new(), 0);
        escapes
    }

    fn collect(&mut self, body: &Body, node: NodeRef, bound: &mut Vec<String>, loops: u32) {
        let mut binds: Option<&str> = None;
        let mut inner_loops = loops;
        match node {
            NodeRef::Stmt(s) => match &body.stmts[s].kind {
                StmtKind::Break { label: Some(l), .. } => {
                    if !bound.contains(l) {
                        self.labels.insert(l.clone());
                    }
                }
                StmtKind::Break { label: None, .. } | StmtKind::Continue => {
                    self.loop_exit |= loops == 0;
                }
                StmtKind::LabeledBlock { label, .. } => binds = Some(label),
                StmtKind::Loop { .. } => inner_loops += 1,
                StmtKind::Let { .. }
                | StmtKind::LetDestructure { .. }
                | StmtKind::Expr(_)
                | StmtKind::Return { .. }
                | StmtKind::If { .. } => {}
            },
            NodeRef::Expr(e) => {
                if let ExprKind::LabeledBlock { label, .. } = &body.exprs[e].kind {
                    binds = Some(label);
                }
            }
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        }
        if let Some(label) = binds {
            bound.push(label.to_owned());
        }
        body.for_each_child(node, |c| self.collect(body, c, bound, inner_loops));
        if binds.is_some() {
            bound.pop();
        }
    }

    /// Whether a clone placed at an exit would have one of these jumps taken
    /// by a block or loop enclosing the exit.
    fn captured_at(&self, scope: &ExitScope) -> bool {
        (self.loop_exit && scope.loops > 0) || scope.labels.iter().any(|l| self.labels.contains(l))
    }
}

/// `stem`, or `stem` with the first suffix no break under `regions` names, so
/// a block taking the label captures none of their jumps.
fn fresh_label(body: &Body, stem: String, regions: &[NodeRef]) -> String {
    let free = |label: &str| regions.iter().all(|&r| !has_break_to(body, r, label));
    if free(&stem) {
        return stem;
    }
    (1..)
        .map(|n| format!("{stem}_{n}"))
        .find(|label| free(label))
        .expect("an unbounded range of labels")
}

/// Replace exit `s` with `with` in the block holding it.
fn replace_exit(engine: &mut Engine, s: StmtId, with: Vec<StmtId>) {
    let Some(NodeRef::Block(parent)) = engine.parent_of(NodeRef::Stmt(s)) else {
        panic!("labeled-block exit {s:?} is not held by a block");
    };
    let mut stmts = engine.body.blocks[parent].stmts.clone();
    let at = stmts
        .iter()
        .position(|held| *held == s)
        .expect("a block holds the statement it parents");
    stmts.splice(at..=at, with);
    engine.set_block_stmts(parent, stmts);
}

/// [`ExitSink`] for `check_lb_breaks_and_get_payload`: every `break L:` must
/// carry `null` or a `VariantConstruct`, and the matching case's payload type
/// is recorded. Value-discarding fusion does not descend `Match` / `Switch`
/// (kept opaque, as before) but rejects any hidden break.
struct BreakChecker<'a> {
    label: &'a str,
    case_index: u32,
    payload_type: &'a mut Option<TypeId>,
}

impl ExitSink for BreakChecker<'_> {
    fn visit(&mut self, body: &Body, value: Option<Operand>) -> bool {
        // A break carrying no value, or a promoted `Null` placeholder, is the
        // empty/None break the fusion accepts.
        let Some(e) = value.and_then(Operand::as_expr) else {
            return value.is_none_or(|v| {
                v.as_value()
                    .is_some_and(|vid| matches!(body.values.kind(vid), ValueKind::Null))
            });
        };
        let ExprKind::VariantConstruct {
            case_index: ci,
            payload,
            ..
        } = &body.exprs[e].kind
        else {
            return false;
        };
        let (ci, payload) = (*ci, *payload);
        if let Some(p) = payload
            && p.as_expr()
                .is_some_and(|pe| has_break_to(body, NodeRef::Expr(pe), self.label))
        {
            return false;
        }
        if ci == self.case_index
            && let Some(p) = payload
        {
            *self.payload_type = Some(body.operand_type(p));
        }
        true
    }
    fn descend_branches(&self) -> bool {
        false
    }
}

/// Verify that all `break L: v` in `block` have `v` as either `null` or
/// `VariantConstruct`. Returns the payload type of the matching case, and the exits.
fn check_lb_breaks_and_get_payload(
    body: &Body,
    block: BlockId,
    label: &str,
    case_index: u32,
) -> Option<(TypeId, Vec<Exit>)> {
    let mut payload_type: Option<TypeId> = None;
    let mut sink = BreakChecker {
        label,
        case_index,
        payload_type: &mut payload_type,
    };
    let exits = walk_exits(body, block, label, &mut sink)?;
    Some((payload_type?, exits))
}

/// [`ExitSink`] for [`check_lb_breaks_are_tagged_tuples`]: every `break L:`
/// must carry a tuple literal whose tag slot is a constant integer, so fusion
/// can pick the arm for it, and whose dropped elements are pure. An exit that
/// selects the wildcard arm drops all of them; one that selects the tag arm
/// keeps exactly the slots the arm reads.
struct TaggedTupleChecker<'a> {
    label: &'a str,
    tag_value: i128,
    read: &'a IndexSet<u32>,
}

impl ExitSink for TaggedTupleChecker<'_> {
    fn visit(&mut self, body: &Body, value: Option<Operand>) -> bool {
        let Some(e) = value.and_then(Operand::as_expr) else {
            return false;
        };
        let ExprKind::TupleLiteral { elements } = &body.exprs[e].kind else {
            return false;
        };
        let Some(tag) = break_tag_value(body, elements) else {
            return false;
        };
        let keeps_reads = tag == self.tag_value;
        if keeps_reads && self.read.iter().any(|k| *k as usize >= elements.len()) {
            return false;
        }
        elements.iter().enumerate().all(|(i, op)| {
            let Some(oe) = op.as_expr() else {
                // A promoted value is pure and carries no exit.
                return true;
            };
            // A skeleton element survives only where the arm reads it: fusion
            // relocates it into a `let`, and dropping it would drop its
            // effects. Either way it must not carry its own exit, which would
            // move with it.
            keeps_reads
                && self.read.contains(&u32::try_from(i).expect("tuple arity"))
                && !has_break_to(body, NodeRef::Expr(oe), self.label)
        })
    }
    fn descend_branches(&self) -> bool {
        false
    }
}

fn check_lb_breaks_are_tagged_tuples(
    body: &Body,
    block: BlockId,
    label: &str,
    tag_value: i128,
    read: &IndexSet<u32>,
) -> Option<Vec<Exit>> {
    let mut sink = TaggedTupleChecker {
        label,
        tag_value,
        read,
    };
    walk_exits(body, block, label, &mut sink)
}

/// The constant in a break tuple's tag slot.
fn break_tag_value(body: &Body, elements: &[Operand]) -> Option<i128> {
    let tag = elements.get(TAG_SLOT as usize)?;
    // Case indices are small and non-negative, so the raw constant compares
    // directly against the arm's literal pattern.
    match body.values.kind(tag.as_value()?) {
        ValueKind::Int(value, _) => Some(i128::from(*value)),
        _ => None,
    }
}

/// The slots `op` reads off `local_idx`, or `None` if it reads the local any
/// other way — the fused block has no aggregate left to hand such a read.
fn slot_reads_in_operand(body: &Body, op: Operand, local_idx: u32) -> Option<Vec<SlotRead>> {
    let mut v = SlotReadCollector::in_operand(local_idx);
    match op {
        Operand::Expr(e) => v.visit_node(body, NodeRef::Expr(e)),
        // The operand itself is the read: no `FieldAccess` slot to hand it.
        Operand::Value(value) if body.values.value_reads_local(value, local_idx) => return None,
        Operand::Value(_) => {}
    }
    if v.direct_uses != v.slot_uses {
        return None;
    }
    // Field order, so the relocated `let`s evaluate the elements in the order
    // the tuple literal did.
    let mut slots: Vec<SlotRead> = v
        .slots
        .into_iter()
        .map(|(field_index, type_id)| SlotRead {
            field_index,
            type_id,
        })
        .collect();
    slots.sort_by_key(|s| s.field_index);
    Some(slots)
}

/// Tallies the temp's reads two ways: `direct_uses` counts every one, skeleton
/// and pool alike, and `slot_uses` counts those that are a `FieldAccess`
/// receiver. Equal tallies mean every read is a slot read, which is the only
/// shape the fused block can serve.
///
/// `lets` counts the `let`s declaring the temp, riding along because the
/// whole-body census already runs the traversal that answers it.
#[derive(Default)]
struct SlotReadCollector {
    local_idx: u32,
    slots: IndexMap<u32, TypeId>,
    /// The name each slot is read under, for the projection a materializing
    /// exit synthesises. A tuple index reads as its own decimal name.
    slot_names: IndexMap<u32, String>,
    direct_uses: usize,
    slot_uses: usize,
    lets: usize,
    /// Stop once a non-slot read is proven to exist. A `FieldAccess` is visited
    /// before the `Local` it reads, so `direct_uses - slot_uses` is the non-slot
    /// reads seen minus the receivers still to be descended into: above zero,
    /// one exists and the final tallies cannot match.
    stop_on_excess: bool,
    stopped: bool,
}

impl SlotReadCollector {
    /// Census of the whole body, which may stop early: its verdict is a
    /// yes/no, so partial tallies past a proven mismatch decide nothing.
    fn whole_body(local_idx: u32) -> Self {
        SlotReadCollector {
            local_idx,
            stop_on_excess: true,
            ..Self::default()
        }
    }

    /// Tally within one operand, which runs to completion — the caller needs
    /// the exact slot list, not just the verdict.
    fn in_operand(local_idx: u32) -> Self {
        SlotReadCollector {
            local_idx,
            ..Self::default()
        }
    }
}

impl NirRefVisitor for SlotReadCollector {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if self.stopped {
            return;
        }
        match node {
            NodeRef::Expr(e) => {
                if is_local(body, e, self.local_idx) {
                    self.direct_uses += 1;
                }
                if let ExprKind::FieldAccess {
                    expr: inner,
                    field_index,
                    field_name,
                } = &body.exprs[e].kind
                    && is_local_operand(body, *inner, self.local_idx)
                {
                    self.slot_uses += 1;
                    self.slots.insert(*field_index, body.exprs[e].type_id);
                    self.slot_names.insert(*field_index, field_name.clone());
                }
            }
            NodeRef::Stmt(s) => {
                if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
                    && *local_index == self.local_idx
                {
                    self.lets += 1;
                }
            }
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        }
        self.direct_uses += promoted_read_count_at(body, node, self.local_idx);
        if self.stop_on_excess && self.direct_uses > self.slot_uses {
            self.stopped = true;
            return;
        }
        self.walk_node(body, node);
    }
}

/// Every use of `local` in the body, skeleton and value pool alike — the total
/// [`count_local_uses_in_stmt`] counts within one statement, so the two compare.
/// The engine's use index sees the skeleton only, hence the second term.
fn body_uses(engine: &Engine, local: u32) -> usize {
    engine.local_reads(local).len() + engine.promoted_read_count(local)
}

/// Counts every read of `local_idx`, skeleton and value pool alike.
struct LocalUseCounter {
    local_idx: u32,
    count: usize,
}

impl NirRefVisitor for LocalUseCounter {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(e) = node
            && is_local(body, e, self.local_idx)
        {
            self.count += 1;
        }
        self.count += promoted_read_count_at(body, node, self.local_idx);
        self.walk_node(body, node);
    }
}

/// Count all occurrences of `Local { index: local_idx }` in a block.
fn count_local_uses_in_block(body: &Body, block: BlockId, local_idx: u32) -> usize {
    let mut v = LocalUseCounter {
        local_idx,
        count: 0,
    };
    v.visit_node(body, NodeRef::Block(block));
    v.count
}

fn count_local_uses_in_stmt(body: &Body, stmt: StmtId, local_idx: u32) -> usize {
    let mut v = LocalUseCounter {
        local_idx,
        count: 0,
    };
    v.visit_node(body, NodeRef::Stmt(stmt));
    v.count
}

fn count_local_uses_in_operand(body: &Body, op: Operand, local_idx: u32) -> usize {
    let mut v = LocalUseCounter {
        local_idx,
        count: 0,
    };
    match op {
        Operand::Expr(e) => v.visit_node(body, NodeRef::Expr(e)),
        Operand::Value(value) => {
            if body.values.value_reads_local(value, local_idx) {
                v.count += 1;
            }
        }
    }
    v.count
}

struct VariantPayloadUseCounter {
    local_idx: u32,
    case_index: u32,
    count: usize,
}

impl NirRefVisitor for VariantPayloadUseCounter {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(e) = node
            && let ExprKind::VariantPayload {
                expr: inner,
                case_index: ci,
                ..
            } = &body.exprs[e].kind
            && *ci == self.case_index
            && is_local_operand(body, *inner, self.local_idx)
        {
            self.count += 1;
        }
        self.walk_node(body, node);
    }
}

/// Count `VariantPayload { expr: Local(local_idx), case_index }` in a block.
fn count_variant_payload_uses_in_block(
    body: &Body,
    block: BlockId,
    local_idx: u32,
    case_index: u32,
) -> usize {
    let mut v = VariantPayloadUseCounter {
        local_idx,
        case_index,
        count: 0,
    };
    v.visit_node(body, NodeRef::Block(block));
    v.count
}

// ---------------------------------------------------------------------------
// Fusion (engine-routed)
// ---------------------------------------------------------------------------

/// Everything the transform needs to rewrite one labeled block's exits into the
/// consumer's arms.
struct Fusion<'a> {
    fused_label: &'a str,
    temp_local: u32,
    then_block: BlockId,
    else_block: Option<BlockId>,
    span: Span,
    value: BoundValue,
}

/// [`FusedValue`] with the locals the relocated arm bodies read allocated.
enum BoundValue {
    Variant {
        case_index: u32,
        payload_local: u32,
        payload_type: TypeId,
    },
    Slots {
        tag_value: i128,
        slots: Vec<BoundSlot>,
    },
}

struct BoundSlot {
    field_index: u32,
    local_index: u32,
    type_id: TypeId,
}

fn perform_fusion(
    engine: &mut Engine,
    outer_block: BlockId,
    stmts: &[StmtId],
    i: usize,
    info: FusionInfo,
) {
    let let_s = stmts[i];
    let if_s = stmts[i + 1];
    let span = engine.body.stmts[let_s].span;

    // Extract the LabeledBlock body from the Let statement.
    let StmtKind::Let {
        value: let_value, ..
    } = &engine.body.stmts[let_s].kind
    else {
        unreachable!("guarded by check_fusion_preconditions")
    };
    let lv = let_value
        .as_expr()
        .expect("guarded by check_fusion_preconditions");
    let ExprKind::LabeledBlock {
        block: lb_block,
        role,
        ..
    } = &engine.body.exprs[lv].kind
    else {
        unreachable!("guarded by check_fusion_preconditions")
    };
    let (lb_block, role) = (*lb_block, *role);

    // Extract the then/else blocks from the consumer statement.
    let (then_block, else_block) = match &engine.body.stmts[if_s].kind {
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => (*then_block, *else_block),
        StmtKind::Expr(match_expr) => {
            let match_expr = match_expr
                .as_expr()
                .expect("match scrutinee is a skeleton expr");
            let ExprKind::Match { arms, .. } = &engine.body.exprs[match_expr].kind else {
                unreachable!()
            };
            let variant_body = arms[0].body;
            let else_body = arms[1].body;
            let then_block = arm_body_operand_into_block(engine, variant_body, span);
            // A unit-valued else arm (the `None` case) contributes no block.
            let else_block = if else_body
                .as_value()
                .is_some_and(|v| matches!(engine.body.values.kind(v), ValueKind::Unit))
            {
                None
            } else {
                Some(arm_body_operand_into_block(engine, else_body, span))
            };
            (then_block, else_block)
        }
        _ => unreachable!(),
    };

    let value = bind_value(engine, info.value);
    // The fused block encloses the labeled block's body and the arms cloned into it.
    let mut enclosed = vec![NodeRef::Block(lb_block), NodeRef::Block(then_block)];
    enclosed.extend(else_block.map(NodeRef::Block));
    let fused_label = fresh_label(engine.body, format!("$fused_{}", info.label), &enclosed);
    let fusion = Fusion {
        fused_label: &fused_label,
        temp_local: info.temp_local,
        then_block,
        else_block,
        span,
        value,
    };
    for exit in info.exits {
        let with = fuse_exit(engine, exit.stmt, &fusion);
        replace_exit(engine, exit.stmt, with);
    }

    // The LB block becomes unreachable once the outer block drops the `let`;
    // taking its list moves the stmts rather than sharing them.
    let fused_stmts = std::mem::take(&mut engine.body.blocks[lb_block].stmts);
    let fused_body = engine.alloc_block(fused_stmts, span);
    let fused_stmt = engine.alloc_stmt(
        StmtKind::LabeledBlock {
            label: fused_label,
            block: fused_body,
            role,
        },
        span,
    );

    // Replace the (let, if/match) pair with the single fused LabeledBlock stmt.
    let mut kept = Vec::with_capacity(stmts.len() - 1);
    kept.extend_from_slice(&stmts[..i]);
    kept.push(fused_stmt);
    kept.extend_from_slice(&stmts[i + 2..]);
    engine.set_block_stmts(outer_block, kept);
}

/// Allocate the locals the relocated arm bodies read. A `Match` arm's own
/// pattern binding slot is reused when it has one, so the arm body needs no
/// substitution for it.
fn bind_value(engine: &mut Engine, value: FusedValue) -> BoundValue {
    match value {
        FusedValue::Variant {
            case_index,
            payload_type,
            pattern_payload_binding,
        } => {
            let payload_local = pattern_payload_binding.unwrap_or_else(|| {
                let next = engine.locals().len() as u32;
                engine.alloc_local(
                    format!("$fused_payload_{next}"),
                    payload_type,
                    /* is_mut */ false,
                )
            });
            BoundValue::Variant {
                case_index,
                payload_local,
                payload_type,
            }
        }
        FusedValue::Slots { tag_value, slots } => BoundValue::Slots {
            tag_value,
            slots: slots
                .into_iter()
                .map(|slot| {
                    let local_index =
                        alloc_indexed_local(engine, "$fused_slot_", slot.type_id, false);
                    BoundSlot {
                        field_index: slot.field_index,
                        local_index,
                        type_id: slot.type_id,
                    }
                })
                .collect(),
        },
    }
}

fn emit_variant_payload_let(engine: &mut Engine, vc: ExprId, f: &Fusion, out: &mut Vec<StmtId>) {
    let BoundValue::Variant {
        payload_local,
        payload_type,
        ..
    } = f.value
    else {
        unreachable!("variant break under a slot fusion")
    };
    let ExprKind::VariantConstruct { payload, .. } = &engine.body.exprs[vc].kind else {
        unreachable!("guarded by the caller's case-index filter")
    };
    let value = payload.unwrap_or_else(|| engine.const_operand(ValueKind::Unit, payload_type));
    let stmt = engine.alloc_stmt(
        StmtKind::Let {
            name: format!("$fused_payload_{payload_local}"),
            local_index: payload_local,
            is_mut: false,
            is_reactive: false,
            type_id: payload_type,
            value,
            skip_value_copy: false,
        },
        f.span,
    );
    out.push(stmt);
}

/// The payload of a break whose case the fused arm does not take, kept as a
/// statement: it still evaluates at the break.
fn emit_untaken_payload(
    engine: &mut Engine,
    value: Option<Operand>,
    span: Span,
    out: &mut Vec<StmtId>,
) {
    let Some(vc) = value.and_then(Operand::as_expr) else {
        return;
    };
    let ExprKind::VariantConstruct {
        payload: Some(payload @ Operand::Expr(_)),
        ..
    } = engine.body.exprs[vc].kind
    else {
        return;
    };
    out.push(engine.alloc_stmt(StmtKind::Expr(payload), span));
}

/// `let $fused_slot_k = <element k>;` for each slot the arm reads. The
/// elements it does not read are dropped, which the precondition allows only
/// for pure ones.
fn emit_slot_lets(engine: &mut Engine, elements: &[Operand], f: &Fusion, out: &mut Vec<StmtId>) {
    let BoundValue::Slots { slots, .. } = &f.value else {
        unreachable!("slot break under a variant fusion")
    };
    for slot in slots {
        let value = elements[slot.field_index as usize];
        let stmt = engine.alloc_stmt(
            StmtKind::Let {
                name: format!("$fused_slot_{}", slot.local_index),
                local_index: slot.local_index,
                is_mut: false,
                is_reactive: false,
                type_id: slot.type_id,
                value,
                skip_value_copy: false,
            },
            f.span,
        );
        out.push(stmt);
    }
}

/// What exit `s` becomes: its payload bound, the consumer arm it selects, and a
/// `break` out of the fused block.
fn fuse_exit(engine: &mut Engine, s: StmtId, f: &Fusion) -> Vec<StmtId> {
    let value = exit_value(engine.body, s);
    let first_arm = match f.value {
        BoundValue::Variant { case_index, .. } => FirstArm::Case(case_index),
        BoundValue::Slots { tag_value, .. } => FirstArm::Tag(tag_value),
    };
    let selected = first_arm.takes(engine.body, value);
    let mut out = Vec::new();
    match (&f.value, value.and_then(Operand::as_expr)) {
        (BoundValue::Variant { .. }, Some(vc)) if selected => {
            emit_variant_payload_let(engine, vc, f, &mut out);
        }
        (BoundValue::Variant { .. }, _) => emit_untaken_payload(engine, value, f.span, &mut out),
        (BoundValue::Slots { .. }, Some(tuple)) if selected => {
            let ExprKind::TupleLiteral { elements } = &engine.body.exprs[tuple].kind else {
                unreachable!("guarded by check_lb_breaks_are_tagged_tuples")
            };
            let elements = elements.clone();
            emit_slot_lets(engine, &elements, f, &mut out);
        }
        (BoundValue::Slots { .. }, _) => {}
    }

    if selected {
        let subst_then = engine.clone_block(f.then_block);
        subst_temp_reads_in_block(engine, subst_then, f);
        // Move the cloned stmts into `out` and empty the source block.
        // Leaving them parented to `subst_then` AND a new block at the
        // same time double-claims the stmt ids: the engine still
        // enqueues `subst_then` for `apply_block`, and downstream
        // rules (e.g. `const_branch_prune::eliminate_dead_stmts`'s
        // void-block flatten) see the now-orphaned stmts a second
        // time, which can erase live work.
        let cloned_stmts = std::mem::take(&mut engine.body.blocks[subst_then].stmts);
        out.extend(cloned_stmts);
    } else if let Some(eb) = f.else_block {
        // None / non-matching case → emit a clone of the else block.
        let cloned = engine.clone_block(eb);
        let cloned_stmts = std::mem::take(&mut engine.body.blocks[cloned].stmts);
        out.extend(cloned_stmts);
    }

    // Emit `break fused_label;` unless the last emitted statement already
    // terminates control flow.
    if !out.last().is_some_and(|s| engine.body.terminates(*s)) {
        let brk = engine.alloc_stmt(
            StmtKind::Break {
                label: Some(f.fused_label.to_owned()),
                value: None,
            },
            f.span,
        );
        out.push(brk);
    }
    out
}

/// Redirect the consumer's reads of the fused temp — `VariantPayload(temp,
/// case)` or `temp.k` — to the locals [`bind_value`] allocated. Engine-routed
/// so each rewrite updates the use index (the new `Local` mention is
/// registered, the replaced node's children are orphaned but never queried
/// again).
fn subst_temp_reads_in_block(engine: &mut Engine, block: BlockId, f: &Fusion) {
    for s in engine.body.blocks[block].stmts.clone() {
        subst_temp_reads_in_stmt(engine, s, f);
    }
}

fn subst_temp_reads_in_stmt(engine: &mut Engine, s: StmtId, f: &Fusion) {
    enum Shape {
        Expr(ExprId),
        ExprAndBlocks(Option<ExprId>, Vec<BlockId>),
        None,
    }
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            // A promoted-constant value mentions no local — nothing to rewrite.
            value.as_expr().map_or(Shape::None, Shape::Expr)
        }
        StmtKind::Expr(expr) => expr.as_expr().map_or(Shape::None, Shape::Expr),
        StmtKind::Return { value } => match value {
            Some(v) => v.as_expr().map_or(Shape::None, Shape::Expr),
            None => Shape::None,
        },
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut blocks = vec![*then_block];
            if let Some(eb) = else_block {
                blocks.push(*eb);
            }
            Shape::ExprAndBlocks(condition.as_expr(), blocks)
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            Shape::ExprAndBlocks(None, vec![*b])
        }
        StmtKind::Break { value, .. } => match value {
            Some(v) => v.as_expr().map_or(Shape::None, Shape::Expr),
            None => Shape::None,
        },
        StmtKind::Continue => Shape::None,
    };
    match shape {
        Shape::Expr(e) => {
            subst_temp_reads_in_expr(engine, e, f);
        }
        Shape::ExprAndBlocks(cond, blocks) => {
            if let Some(c) = cond {
                subst_temp_reads_in_expr(engine, c, f);
            }
            for b in blocks {
                subst_temp_reads_in_block(engine, b, f);
            }
        }
        Shape::None => {}
    }
}

/// The `Local` that replaces `e`, when `e` is one of the consumer's reads of
/// the fused temp.
fn replacement_for(body: &Body, e: ExprId, f: &Fusion) -> Option<ExprKind> {
    match &f.value {
        BoundValue::Variant {
            case_index,
            payload_local,
            ..
        } => {
            let ExprKind::VariantPayload {
                expr: inner,
                case_index: ci,
                ..
            } = &body.exprs[e].kind
            else {
                return None;
            };
            (ci == case_index && is_local_operand(body, *inner, f.temp_local)).then(|| {
                ExprKind::Local {
                    index: *payload_local,
                    name: format!("$fused_payload_{payload_local}"),
                }
            })
        }
        BoundValue::Slots { slots, .. } => {
            let ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } = &body.exprs[e].kind
            else {
                return None;
            };
            if !is_local_operand(body, *inner, f.temp_local) {
                return None;
            }
            let slot = slots.iter().find(|s| s.field_index == *field_index)?;
            Some(ExprKind::Local {
                index: slot.local_index,
                name: format!("$fused_slot_{}", slot.local_index),
            })
        }
    }
}

fn subst_temp_reads_in_expr(engine: &mut Engine, e: ExprId, f: &Fusion) {
    if let Some(kind) = replacement_for(engine.body, e, f) {
        engine.replace_expr_kind(e, kind);
        return;
    }

    // Recurse into sub-expressions / sub-blocks (patterns excluded).
    enum Walk {
        Exprs(Vec<ExprId>),
        ExprsAndBlocks(Vec<ExprId>, Vec<BlockId>),
        Block(BlockId),
        None,
    }
    let walk = match &engine.body.exprs[e].kind {
        ExprKind::Binary { left, right, .. }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => Walk::Exprs(
            [*left, *right]
                .into_iter()
                .filter_map(Operand::as_expr)
                .collect(),
        ),
        ExprKind::Assign { target, value } => {
            Walk::Exprs(std::iter::once(*target).chain(value.as_expr()).collect())
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => {
            Walk::Exprs(inner.as_expr().into_iter().collect())
        }
        ExprKind::Call { args, .. } => {
            Walk::Exprs(args.iter().filter_map(|a| a.expr.as_expr()).collect())
        }
        ExprKind::CmRawCall { args, .. } => {
            Walk::Exprs(args.iter().filter_map(|o| o.as_expr()).collect())
        }
        ExprKind::IndirectCall { callee, args } => {
            let mut v: Vec<ExprId> = callee.as_expr().into_iter().collect();
            v.extend(args.iter().filter_map(|o| o.as_expr()));
            Walk::Exprs(v)
        }
        ExprKind::StructLiteral { fields, .. } => {
            Walk::Exprs(fields.iter().filter_map(|f| f.value.as_expr()).collect())
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            Walk::Exprs(elements.iter().filter_map(|o| o.as_expr()).collect())
        }
        ExprKind::VariantConstruct { payload, .. } => {
            Walk::Exprs(payload.iter().filter_map(|o| o.as_expr()).collect())
        }
        ExprKind::LabeledBlock { block, .. } => Walk::Block(*block),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut blocks = vec![*then_branch];
            if let Some(eb) = else_branch {
                blocks.push(*eb);
            }
            Walk::ExprsAndBlocks(condition.as_expr().into_iter().collect(), blocks)
        }
        ExprKind::Match { expr, arms } => {
            let mut exprs: Vec<ExprId> = expr.as_expr().into_iter().collect();
            for arm in arms {
                if let Some(b) = arm.body.as_expr() {
                    exprs.push(b);
                }
                if let Some(g) = arm.guard.and_then(Operand::as_expr) {
                    exprs.push(g);
                }
            }
            Walk::Exprs(exprs)
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut blocks = arms.clone();
            blocks.push(*default);
            Walk::ExprsAndBlocks(scrutinee.as_expr().into_iter().collect(), blocks)
        }
        _ => Walk::None,
    };
    match walk {
        Walk::Exprs(v) => {
            for id in v {
                subst_temp_reads_in_expr(engine, id, f);
            }
        }
        Walk::ExprsAndBlocks(exprs, blocks) => {
            for id in exprs {
                subst_temp_reads_in_expr(engine, id, f);
            }
            for b in blocks {
                subst_temp_reads_in_block(engine, b, f);
            }
        }
        Walk::Block(b) => {
            subst_temp_reads_in_block(engine, b, f);
        }
        Walk::None => {}
    }
}

// Value-producing threading (`apply_expr`): `match LB { … }` → `LB` in place.

struct ArmInfo {
    /// `None` for a wildcard arm; `Some(index)` for a `Variant` pattern.
    case_index: Option<u32>,
    binding: Option<u32>,
    body: Operand,
}

struct ThreadPlan {
    scrut: ExprId,
    label: String,
    lb_block: BlockId,
    arms: Vec<ArmInfo>,
    result_type: TypeId,
    unit_result: bool,
    exits: Vec<Exit>,
}

fn plan_threading(body: &Body, id: ExprId, locals: &[NirLocal]) -> Option<ThreadPlan> {
    let ExprKind::Match { expr: scrut, arms } = &body.exprs[id].kind else {
        return None;
    };
    let scrut = scrut.as_expr()?;
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[scrut].kind
    else {
        return None;
    };
    let label = label.clone();
    let lb_block = *lb_block;

    let arm_infos: Vec<ArmInfo> = arms
        .iter()
        .map(|arm| arm_info(body, arm))
        .collect::<Option<_>>()?;

    if body.falls_through(lb_block) {
        return None;
    }

    let result_type = body.exprs[id].type_id;
    let unit_result = result_type == TypeTable::UNIT;

    // Validate every `break L:` exit and record which arms it selects.
    let mut selected = vec![false; arm_infos.len()];
    let exits = validate_exits_in_block(body, lb_block, &label, &arm_infos, locals, &mut selected)?;

    let escapes: Vec<Escapes> = arm_infos
        .iter()
        .map(|arm| Escapes::of_operand(body, arm.body))
        .collect();
    if exits.iter().any(|exit| {
        escapes[selected_arm(body, exit.stmt, &arm_infos)].captured_at(&exit.scope)
    }) {
        return None;
    }
    // A non-unit match needs a tail value from every threaded arm.
    if !unit_result
        && arm_infos.iter().zip(&selected).any(|(arm, used)| {
            *used && arm.body.as_expr().is_some_and(|e| !arm_body_decomposable(body, e))
        })
    {
        return None;
    }

    Some(ThreadPlan {
        scrut,
        label,
        lb_block,
        arms: arm_infos,
        result_type,
        unit_result,
        exits,
    })
}

fn arm_info(body: &Body, arm: &ArmData) -> Option<ArmInfo> {
    if arm.guard.is_some() {
        return None;
    }
    match &body.pats[arm.pattern].kind {
        PatKind::Wildcard => Some(ArmInfo {
            case_index: None,
            binding: None,
            body: arm.body,
        }),
        PatKind::Variant {
            case_index,
            bindings,
            ..
        } => {
            let binding = single_payload_binding(body, bindings)?;
            Some(ArmInfo {
                case_index: Some(*case_index),
                binding,
                body: arm.body,
            })
        }
        _ => None,
    }
}

/// First arm a `VariantConstruct` of `case_index` selects: the same-case
/// `Variant` arm or a wildcard. Case A never matches a `Variant` pattern of
/// case B, so skipping non-matching variant arms is exact.
fn select_arm(arms: &[ArmInfo], case_index: u32) -> Option<usize> {
    arms.iter()
        .position(|a| a.case_index.is_none_or(|index| index == case_index))
}

/// The arm a validated exit selects, with the variant it constructs.
fn selected_arm(body: &Body, exit: StmtId, arms: &[ArmInfo]) -> usize {
    let vc = exit_value(body, exit)
        .and_then(Operand::as_expr)
        .expect("guarded by plan_threading");
    let ExprKind::VariantConstruct { case_index, .. } = &body.exprs[vc].kind else {
        unreachable!("guarded by plan_threading");
    };
    select_arm(arms, *case_index).expect("guarded by plan_threading")
}

/// Whether a non-unit arm body splits into `stmts + tail value`: a plain
/// operand is its own tail; a block must end in an `Expr` or a terminator.
fn arm_body_decomposable(body: &Body, e: ExprId) -> bool {
    let Some(b) = body.unbroken_block(e) else {
        return true;
    };
    let Some(last) = body.blocks[b].stmts.last() else {
        return false;
    };
    matches!(body.stmts[*last].kind, StmtKind::Expr(_)) || body.terminates(*last)
}

/// [`ExitSink`] for threading: resolves each `break L:` exit to the arm it
/// selects (marking `selected`) and checks the arm's payload binding type.
/// Descends `Match` / `Switch` arms (the threading transform rewrites them) and
/// rejects any hidden break.
struct ExitValidator<'a> {
    label: &'a str,
    arms: &'a [ArmInfo],
    locals: &'a [NirLocal],
    selected: &'a mut [bool],
}

impl ExitSink for ExitValidator<'_> {
    fn visit(&mut self, body: &Body, value: Option<Operand>) -> bool {
        let Some(vc) = value.and_then(Operand::as_expr) else {
            // A value-less or promoted (`null`) break has no static case.
            return false;
        };
        let ExprKind::VariantConstruct {
            case_index,
            payload,
            ..
        } = &body.exprs[vc].kind
        else {
            return false;
        };
        let (case_index, payload) = (*case_index, *payload);
        if payload.is_some_and(|p| {
            p.as_expr()
                .is_some_and(|e| has_break_to(body, NodeRef::Expr(e), self.label))
        }) {
            return false;
        }
        let Some(idx) = select_arm(self.arms, case_index) else {
            return false;
        };
        if let Some(binding) = self.arms[idx].binding {
            let Some(p) = payload else {
                return false;
            };
            if self
                .locals
                .get(binding as usize)
                .is_none_or(|local| local.type_id != body.operand_type(p))
            {
                return false;
            }
        }
        self.selected[idx] = true;
        true
    }
    fn descend_branches(&self) -> bool {
        true
    }
}

/// Validate every `break L:` exit and mark which arm each selects, mirroring
/// the value-discarding [`check_lb_breaks_and_get_payload`] but resolving to
/// arms rather than a single payload type.
fn validate_exits_in_block(
    body: &Body,
    block: BlockId,
    label: &str,
    arms: &[ArmInfo],
    locals: &[NirLocal],
    selected: &mut [bool],
) -> Option<Vec<Exit>> {
    let mut sink = ExitValidator {
        label,
        arms,
        locals,
        selected,
    };
    walk_exits(body, block, label, &mut sink)
}

fn perform_threading(engine: &mut Engine, match_id: ExprId, plan: ThreadPlan) {
    let mut enclosed = vec![NodeRef::Block(plan.lb_block)];
    enclosed.extend(
        plan.arms
            .iter()
            .filter_map(|arm| arm.body.as_expr().map(NodeRef::Expr)),
    );
    let fused_label = fresh_label(engine.body, format!("$thread_{}", plan.label), &enclosed);
    for exit in &plan.exits {
        let with = thread_exit(engine, exit.stmt, &plan, &fused_label);
        replace_exit(engine, exit.stmt, with);
    }
    // Move the scrutinee's LabeledBlock kind onto the match node, killing the
    // vacated node first so the block is never double-claimed.
    let role = match &engine.body.exprs[plan.scrut].kind {
        ExprKind::LabeledBlock { role, .. } => *role,
        _ => BlockRole::Plain,
    };
    engine.replace_expr_kind(plan.scrut, ExprKind::Dead);
    engine.replace_expr_kind(
        match_id,
        ExprKind::LabeledBlock {
            label: fused_label,
            block: plan.lb_block,
            result_type: plan.result_type,
            role,
        },
    );
}

/// What exit `s` becomes: its payload bound, a clone of the arm it selects, and
/// a `break` out of the threaded block carrying the arm's value.
fn thread_exit(
    engine: &mut Engine,
    s: StmtId,
    plan: &ThreadPlan,
    fused_label: &str,
) -> Vec<StmtId> {
    let StmtKind::Break { value, .. } = engine.body.stmts[s].kind else {
        unreachable!("an exit is a break")
    };
    let span = engine.body.stmts[s].span;
    let mut out = Vec::new();
    let vc = value
        .and_then(Operand::as_expr)
        .expect("guarded by plan_threading");
    let ExprKind::VariantConstruct {
        case_index,
        payload,
        ..
    } = &engine.body.exprs[vc].kind
    else {
        unreachable!("guarded by plan_threading");
    };
    let (case_index, payload) = (*case_index, *payload);
    let arm_idx = select_arm(&plan.arms, case_index).expect("guarded by plan_threading");
    let arm = &plan.arms[arm_idx];
    let arm_body = arm.body;
    let binding = arm.binding;

    // Bind or preserve the payload. An unbound effectful payload still runs
    // (source order: the payload evaluates at the break, before dispatch).
    if let Some(b_local) = binding {
        let payload_op = payload.expect("guarded by plan_threading");
        let local = &engine.locals()[b_local as usize];
        let (name, type_id) = (local.name.clone(), local.type_id);
        let let_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name,
                local_index: b_local,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: payload_op,
                skip_value_copy: false,
            },
            span,
        );
        out.push(let_stmt);
    } else if let Some(p) = payload
        && p.as_expr().is_some()
    {
        out.push(engine.alloc_stmt(StmtKind::Expr(p), span));
    }

    // Clone the arm body and split it into leading statements plus the tail
    // value the fused break carries.
    let (body_stmts, tail): (Vec<StmtId>, Option<Operand>) = match arm_body {
        Operand::Value(v) => (vec![], Some(Operand::Value(v))),
        Operand::Expr(e) => {
            if let Some(b) = engine.body.unbroken_block(e) {
                let cloned = engine.clone_block(b);
                // Move the cloned stmts out so the source block never
                // double-claims them (same discipline as the fusion half).
                let mut stmts = std::mem::take(&mut engine.body.blocks[cloned].stmts);
                let tail = match stmts.last().map(|s| &engine.body.stmts[*s].kind) {
                    Some(StmtKind::Expr(op)) => {
                        let op = *op;
                        stmts.pop();
                        Some(op)
                    }
                    Some(StmtKind::Break { .. } | StmtKind::Return { .. } | StmtKind::Continue) => {
                        None
                    }
                    _ => Some(engine.const_operand(ValueKind::Unit, TypeTable::UNIT)),
                };
                (stmts, tail)
            } else {
                (vec![], Some(Operand::Expr(engine.clone_expr(e))))
            }
        }
    };
    out.extend(body_stmts);

    match tail {
        Some(t) if !plan.unit_result => {
            out.push(engine.alloc_stmt(
                StmtKind::Break {
                    label: Some(fused_label.to_owned()),
                    value: Some(t),
                },
                span,
            ));
        }
        Some(t) => {
            // Unit match: evaluate an effectful tail as a statement and break
            // value-less, keeping the fused block unit-typed.
            if t.as_expr().is_some() {
                out.push(engine.alloc_stmt(StmtKind::Expr(t), span));
            }
            out.push(engine.alloc_stmt(
                StmtKind::Break {
                    label: Some(fused_label.to_owned()),
                    value: None,
                },
                span,
            ));
        }
        // A divergent arm already terminates; no fused break needed.
        None => {}
    }
    out
}

/// Tagged-tuple temp scalarization: the `let temp = L: { …; break L: [tag,
/// slots…]; }` an inlined `sroa_variant_return` callee leaves behind, where
/// every read of `temp` is a `temp.k` projection.
///
/// [`LabeledBlockFusionRule`] folds the shapes whose consumer it can relocate
/// into the block. The one it cannot is the value-producing `let v = match
/// temp.0 { … }` of an inlined `let x = f()?`, and there the result tuple is
/// allocated on the heap once per call and read straight back — `json-canada`
/// pays one per coordinate. Scalarizing the temp needs no consumer analysis at
/// all: give each projected slot a local, fill them at every exit, and the
/// tuple never exists.
/// Build the rule for one function. Mirrors the sibling constructors; the rule
/// keeps no per-function state beyond the type table it classifies slot types
/// against.
pub(super) fn build_slot_temp_sroa(type_table: &TypeTable) -> SlotTempSroaRuleWithTypes<'_> {
    SlotTempSroaRuleWithTypes { type_table }
}

/// One scalarizable temp: its labeled block, and the slots the body projects.
struct SlotTempSroa {
    temp_local: u32,
    label: String,
    lb_block: BlockId,
    role: BlockRole,
    /// Statements a `Block` wrapper ran before the labeled block, hoisted out.
    lead: Vec<StmtId>,
    /// Projected slots in field order, each with the local that replaces it.
    slots: Vec<BoundSlot>,
    /// The name each slot is read under, for a materializing exit's projection.
    names: IndexMap<u32, String>,
    /// The declaring `let mut slot = <zero>` each one needs ahead of the block.
    zeros: Vec<(u32, TypeId, Operand)>,
    span: Span,
    exits: Vec<Exit>,
}

impl SlotTempSroa {
    /// The local standing in for `temp.field_index`, if that slot is read.
    fn slot_of(&self, field_index: u32) -> Option<&BoundSlot> {
        self.slots.iter().find(|s| s.field_index == field_index)
    }
}

pub(super) struct SlotTempSroaRuleWithTypes<'t> {
    type_table: &'t TypeTable,
}

impl Rule for SlotTempSroaRuleWithTypes<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        for (i, &s) in stmts.iter().enumerate() {
            let Some(plan) = plan_slot_temp_sroa(engine, s, self.type_table) else {
                continue;
            };
            perform_slot_temp_sroa(engine, block, &stmts, i, plan);
            return true;
        }
        false
    }
}

fn plan_slot_temp_sroa(
    engine: &mut Engine,
    let_s: StmtId,
    type_table: &TypeTable,
) -> Option<SlotTempSroa> {
    let mut zeros: Vec<(u32, TypeId, Operand)> = Vec::new();
    let body = &*engine.body;
    let StmtKind::Let {
        local_index: temp_local,
        value: let_value,
        is_mut: false,
        ..
    } = &body.stmts[let_s].kind
    else {
        return None;
    };
    let temp_local = *temp_local;
    // `let_block_flatten` normalises `let x = { stmts…; tail }`, but this tail
    // is a labeled block rather than a plain value, so the wrapper can still be
    // in place: look through it and carry its leading statements out ahead of
    // the block.
    let lv = let_value.as_expr()?;
    let (lead, lb_expr) = match body.unbroken_block(lv) {
        Some(b) => {
            let (&last, lead) = body.blocks[b].stmts.split_last()?;
            let StmtKind::Expr(Operand::Expr(tail)) = &body.stmts[last].kind else {
                return None;
            };
            (lead.to_vec(), *tail)
        }
        None => (Vec::new(), lv),
    };
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        role,
        ..
    } = &body.exprs[lb_expr].kind
    else {
        return None;
    };
    let (label, lb_block, role) = (label.clone(), *lb_block, *role);

    // Ordered ahead of the read census below, which walks the whole body.
    if body.falls_through(lb_block) {
        return None;
    }

    // Every read of the temp must be a `temp.k` projection — the block has no
    // aggregate left to hand any other — and exactly one `let` may declare it:
    // `clone_block` copies `local_index` verbatim, and a second copy's reads
    // would be rewritten to slot locals only the first copy's exits assign.
    let mut reads = SlotReadCollector::whole_body(temp_local);
    reads.visit_node(body, NodeRef::Block(body.root));
    assert!(
        reads.stopped || reads.slot_uses <= reads.direct_uses,
        "every slot read is also a direct read, so a completed census cannot \
         count more slot reads than direct ones"
    );
    if reads.stopped
        || reads.slot_uses == 0
        || reads.direct_uses != reads.slot_uses
        || reads.lets != 1
    {
        return None;
    }
    let mut fields: Vec<(u32, TypeId)> = reads.slots.into_iter().collect();
    fields.sort_by_key(|(field_index, _)| *field_index);
    let widest = fields
        .last()
        .map_or(0, |(field_index, _)| *field_index as usize);

    // Each slot local is declared once, before the block, so its definition
    // dominates every projection the consumer left behind; the exits assign it.
    // A type with no zero value to declare it with is refused here rather than
    // producing a `let` per exit, which would leave reads no single definition
    // dominates — and a later fold would pick one arbitrarily.
    let span = body.stmts[let_s].span;
    let pads: Vec<Pad> = fields
        .iter()
        .map(|(_, type_id)| zero_pad(*type_id, type_table))
        .collect::<Option<_>>()?;

    // Only a materializing exit reads a field this pass wrote itself, and
    // `$value_copy$T` insertion is long past. Offer it where every slot is a
    // scalar, which value semantics copy for free.
    let materializable = pads.iter().all(|p| !matches!(p, Pad::NoneOf(_)));
    let mut sink = SlotExitChecker {
        label: &label,
        widest,
        wanted: fields.iter().map(|(field_index, _)| *field_index).collect(),
        materializable,
        struct_arity: None,
        saw_materialize: false,
    };
    let exits = walk_exits(body, lb_block, &label, &mut sink)?;
    if !sink.admissible() {
        return None;
    }

    let mut slots = Vec::with_capacity(fields.len());
    for ((field_index, type_id), pad) in fields.into_iter().zip(pads) {
        let zero = materialize_pad(engine, pad, span);
        let local_index =
            alloc_indexed_local(engine, "$sroa_slot_", type_id, /* is_mut */ true);
        slots.push(BoundSlot {
            field_index,
            local_index,
            type_id,
        });
        zeros.push((local_index, type_id, zero));
    }
    Some(SlotTempSroa {
        temp_local,
        label,
        lb_block,
        role,
        lead,
        slots,
        names: reads.slot_names,
        zeros,
        span,
        exits,
    })
}

/// [`ExitSink`] for the slot rule. A literal exit hands over its own operands,
/// anything else the aggregate, which the rewrite binds and projects.
struct SlotExitChecker<'a> {
    label: &'a str,
    widest: usize,
    /// The field indices the consumer projects. A struct literal's fields are
    /// named rather than positional, so width says nothing about which are
    /// there, and [`SlotExitChecker::admissible`] wants the count.
    wanted: Vec<u32>,
    materializable: bool,
    /// Arity of the struct the exits build, from the first literal that says.
    struct_arity: Option<usize>,
    saw_materialize: bool,
}

impl SlotExitChecker<'_> {
    /// A named struct is decomposed only when the reads cover every field.
    ///
    /// A `[tag, slots…]` tuple is a carrier `sroa_variant_return` made and
    /// nothing else knows, so an unread element of one is free to drop. Other
    /// passes recognise a named struct by its shape: `tmpl_hoist` keys on
    /// `String`, whose `repr` a `s.len()` consumer never reads, and taking half
    /// of one leaves the object alive with that shape gone.
    ///
    /// A materializing exit needs a struct-literal sibling. Alone it is
    /// pointless, since a block whose every exit calls out allocates on each of
    /// them anyway. Beside a tuple it would be sound, `widest` having already
    /// proved every projected slot is carried; that case waits on something to
    /// measure it against.
    fn admissible(&self) -> bool {
        match self.struct_arity {
            Some(arity) => arity == self.wanted.len(),
            None => !self.saw_materialize,
        }
    }
}

impl ExitSink for SlotExitChecker<'_> {
    fn visit(&mut self, body: &Body, value: Option<Operand>) -> bool {
        let Some(e) = value.and_then(Operand::as_expr) else {
            return false;
        };
        if has_break_to(body, NodeRef::Expr(e), self.label) {
            return false;
        }
        match &body.exprs[e].kind {
            ExprKind::TupleLiteral { elements } => elements.len() > self.widest,
            ExprKind::StructLiteral { fields, .. } => {
                if self
                    .struct_arity
                    .replace(fields.len())
                    .is_some_and(|a| a != fields.len())
                {
                    return false;
                }
                self.wanted
                    .iter()
                    .all(|w| fields.iter().any(|f| f.field_index == *w))
            }
            _ => {
                self.saw_materialize = true;
                self.materializable
            }
        }
    }
    fn descend_branches(&self) -> bool {
        true
    }
}

fn perform_slot_temp_sroa(
    engine: &mut Engine,
    outer_block: BlockId,
    stmts: &[StmtId],
    i: usize,
    plan: SlotTempSroa,
) {
    let span = plan.span;
    for exit in &plan.exits {
        let with = scalarize_exit(engine, exit.stmt, &plan);
        replace_exit(engine, exit.stmt, with);
    }

    // Every `temp.k` now reads the slot local instead. Collect first: the
    // rewrite only replaces expression kinds, so the ids stay valid.
    let mut hits = SlotAccessCollector {
        local_idx: plan.temp_local,
        hits: Vec::new(),
    };
    hits.visit_node(engine.body, NodeRef::Block(engine.body.root));
    for (e, field_index) in hits.hits {
        let Some(slot) = plan.slot_of(field_index) else {
            continue;
        };
        engine.replace_expr_kind(
            e,
            ExprKind::Local {
                index: slot.local_index,
                name: format!("$sroa_slot_{}", slot.local_index),
            },
        );
    }

    // The labeled block yields nothing now, so it becomes a statement and the
    // temp's binding goes with it.
    let lb_stmt = engine.alloc_stmt(
        StmtKind::LabeledBlock {
            label: plan.label,
            block: plan.lb_block,
            role: plan.role,
        },
        span,
    );
    let mut kept = Vec::with_capacity(stmts.len() + plan.zeros.len() + plan.lead.len());
    kept.extend_from_slice(&stmts[..i]);
    kept.extend_from_slice(&plan.lead);
    for (local_index, type_id, zero) in plan.zeros {
        let decl = engine.alloc_stmt(
            StmtKind::Let {
                name: format!("$sroa_slot_{local_index}"),
                local_index,
                is_mut: true,
                is_reactive: false,
                type_id,
                value: zero,
                skip_value_copy: false,
            },
            span,
        );
        kept.push(decl);
    }
    kept.push(lb_stmt);
    kept.extend_from_slice(&stmts[i + 1..]);
    engine.set_block_stmts(outer_block, kept);
    engine.note_elided_local(plan.temp_local);
}

/// What `break L: [e0, …]` becomes: the slot assignments the projections read
/// and a value-less `break L`.
fn scalarize_exit(engine: &mut Engine, s: StmtId, plan: &SlotTempSroa) -> Vec<StmtId> {
    let StmtKind::Break {
        value: Some(Operand::Expr(exit)),
        ..
    } = engine.body.stmts[s].kind
    else {
        unreachable!("guarded by SlotExitChecker")
    };
    let span = engine.body.stmts[s].span;
    let mut out = Vec::new();
    // Field-carrying exits hand each operand straight to its slot; the rest bind
    // the aggregate first and project. `SlotExitChecker` admitted exactly these.
    let carried: Option<Vec<(u32, Operand)>> = match &engine.body.exprs[exit].kind {
        ExprKind::TupleLiteral { elements } => Some(
            elements
                .iter()
                .enumerate()
                .map(|(i, e)| (u32::try_from(i).expect("tuple arity"), *e))
                .collect(),
        ),
        ExprKind::StructLiteral { fields, .. } => {
            Some(fields.iter().map(|f| (f.field_index, f.value)).collect())
        }
        _ => None,
    };
    match carried {
        Some(carried) => {
            for (field_index, value) in carried {
                let kind = match plan.slot_of(field_index) {
                    Some(slot) => slot_assign(engine, slot, value, span),
                    // Unread: a promoted element is pure and drops with the
                    // aggregate, a skeleton one stays for its effects and its
                    // evaluation order.
                    None if value.as_expr().is_none() => continue,
                    None => StmtKind::Expr(value),
                };
                let stmt = engine.alloc_stmt(kind, span);
                out.push(stmt);
            }
        }
        None => scalarize_materialized_exit(engine, exit, plan, span, &mut out),
    }
    let brk = engine.alloc_stmt(
        StmtKind::Break {
            label: Some(plan.label.clone()),
            value: None,
        },
        span,
    );
    out.push(brk);
    out
}

/// Allocate a local named after the index it lands at, so its declared name and
/// every later mention rebuilt from `local_index` agree by construction.
fn alloc_indexed_local(engine: &mut Engine, prefix: &str, type_id: TypeId, is_mut: bool) -> u32 {
    let index = engine.locals().len() as u32;
    let allocated = engine.alloc_local(format!("{prefix}{index}"), type_id, is_mut);
    assert_eq!(
        allocated, index,
        "a local lands at the index it was named for"
    );
    index
}

/// `$sroa_slot_N = value`, the statement every exit form ends up emitting.
fn slot_assign(engine: &mut Engine, slot: &BoundSlot, value: Operand, span: Span) -> StmtKind {
    let target = engine.alloc_expr(
        ExprKind::Local {
            index: slot.local_index,
            name: format!("$sroa_slot_{}", slot.local_index),
        },
        slot.type_id,
        span,
    );
    StmtKind::Expr(Operand::Expr(engine.alloc_expr(
        ExprKind::Assign { target, value },
        TypeTable::UNIT,
        span,
    )))
}

/// An exit handing over the aggregate rather than its fields, usually a call.
/// Binding it and projecting keeps this path's allocation and removes the merge.
fn scalarize_materialized_exit(
    engine: &mut Engine,
    exit: ExprId,
    plan: &SlotTempSroa,
    span: Span,
    out: &mut Vec<StmtId>,
) {
    let agg_type = engine.body.exprs[exit].type_id;
    let index = alloc_indexed_local(engine, "$sroa_agg_", agg_type, /* is_mut */ false);
    let name = format!("$sroa_agg_{index}");
    out.push(engine.alloc_stmt(
        StmtKind::Let {
            name: name.clone(),
            local_index: index,
            is_mut: false,
            is_reactive: false,
            type_id: agg_type,
            value: Operand::Expr(exit),
            skip_value_copy: true,
        },
        span,
    ));
    for slot in &plan.slots {
        let recv = engine.alloc_expr(
            ExprKind::Local {
                index,
                name: name.clone(),
            },
            agg_type,
            span,
        );
        let read = engine.alloc_expr(
            ExprKind::FieldAccess {
                expr: Operand::Expr(recv),
                field_index: slot.field_index,
                field_name: plan
                    .names
                    .get(&slot.field_index)
                    .expect("every slot was found by a read that named it")
                    .clone(),
            },
            slot.type_id,
            span,
        );
        let kind = slot_assign(engine, slot, Operand::Expr(read), span);
        out.push(engine.alloc_stmt(kind, span));
    }
}

/// The zero operand for a slot's declaring `let`, built through the engine so
/// its per-node buffers grow with the arena.
fn materialize_pad(engine: &mut Engine, pad: Pad, span: Span) -> Operand {
    match pad {
        Pad::Int(ty) => engine.const_operand(ValueKind::Int(0, ty), ty),
        Pad::Float(ty) => engine.const_operand(ValueKind::Float(0.0f64.to_bits(), ty), ty),
        Pad::Bool => engine.const_operand(ValueKind::Bool(false), TypeTable::BOOL),
        Pad::Char => engine.const_operand(ValueKind::Char('\0'), TypeTable::CHAR),
        Pad::NoneOf(option_type) => Operand::Expr(engine.alloc_expr(
            ExprKind::VariantConstruct {
                variant_type: option_type,
                case_index: OPTION_NONE_CASE,
                case_name: "None".to_string(),
                payload: None,
            },
            option_type,
            span,
        )),
    }
}

/// `Option`'s `None` case index, as declared in `lib/core/prelude/types.wado`.
const OPTION_NONE_CASE: u32 = 1;

/// Collects every `Local(temp).k` projection as `(node, field_index)`.
struct SlotAccessCollector {
    local_idx: u32,
    hits: Vec<(ExprId, u32)>,
}

impl NirRefVisitor for SlotAccessCollector {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(e) = node
            && let ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } = &body.exprs[e].kind
            && is_local_operand(body, *inner, self.local_idx)
        {
            self.hits.push((e, *field_index));
        }
        self.walk_node(body, node);
    }
}
