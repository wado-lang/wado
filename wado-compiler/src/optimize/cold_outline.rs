//! Move what a `cold_path()` marker opens into a function of its own.
//!
//! The inline cost model stops counting at the marker, so a body whose bulk is
//! a rare arm prices at its hot path. Nothing made that true: the whole body
//! was still copied at every call site. This pass performs the split the price
//! already assumed — the caller keeps the branch, the cold arm becomes a call.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionKind, FunctionRef, InlineHint, NirFunction};
use crate::nir_arena::{
    ArenaCallArg, BlockId, BlockNode, Body, ExprKind, ExprNode, NodeRef, Operand, PatKind, StmtId,
    StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueId;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::dce::{DescriptorCache, callee_descriptor};
use super::inline::{InlineCtx, splice_stmt};

/// Split every cold region the preconditions admit, in every function —
/// including the ones this pass itself creates, so a region nested inside a
/// region is reached on a later round.
pub fn outline_cold_regions(
    project: &mut NirPackage,
    descriptor_cache: &mut DescriptorCache,
) -> bool {
    let unit = project.type_table.borrow_mut().intern(ResolvedType::Unit);
    let mut changed = false;
    let mut fi = 0;
    while fi < project.functions.len() {
        let mut ordinal = 0;
        while let Some(region) = find_region(project, fi, descriptor_cache) {
            outline(project, fi, region, ordinal, unit);
            ordinal += 1;
            changed = true;
        }
        fi += 1;
    }
    changed
}

/// A cold region: the statements after a `cold_path()` marker, to the end of
/// the block holding it.
struct Region {
    block: BlockId,
    /// Position of the marker in `block`; the region is everything after it.
    marker: usize,
    stmts: Vec<StmtId>,
}

/// Whether a function's own shape allows moving code out of it at all. The
/// exclusions are the signatures whose calling convention is not an ordinary
/// call: an ABI bridge, an effect dispatcher, and an async frame, whose `task
/// return` leaves through the frame the region would no longer be in.
fn is_splittable(func: &NirFunction) -> bool {
    !func.is_cm_binding && !func.is_dispatch_wrapper && !func.is_cm_export && !func.is_async
}

/// The first region in function `fi` that this pass may move.
fn find_region(
    project: &NirPackage,
    fi: usize,
    descriptor_cache: &mut DescriptorCache,
) -> Option<Region> {
    let descriptors = descriptor_cache.descriptors(project);
    let func = project.functions[fi].borrow();
    if func.is_dead || !is_splittable(&func) {
        return None;
    }
    let body = func.body.as_ref()?;
    let param_count = func.params.len() as u32;
    let type_table = project.type_table.borrow();
    for (block, under_loop) in valueless_blocks(body, &type_table) {
        let stmts = &body.blocks[block].stmts;
        let Some(marker) = stmts
            .iter()
            .position(|&s| is_cold_marker(body, s, descriptors))
        else {
            continue;
        };
        let region: Vec<StmtId> = stmts[marker + 1..].to_vec();
        if is_already_outlined(body, &region) {
            continue;
        }
        crate::compiler_trace!(
            "cold_outline",
            "{}: region of {} stmt(s)",
            func.name,
            region.len(),
        );
        if is_movable(body, &region, param_count, under_loop) {
            return Some(Region {
                block,
                marker,
                stmts: region,
            });
        }
    }
    None
}

/// The reachable blocks whose tail nothing reads, each with whether a loop
/// encloses it: a statement `if` or `loop`, and the arms of the expression
/// forms whose own type is `()`. Replacing the tail of one with a `()` call
/// cannot change what anything evaluates to, which is what lets a region become
/// a call.
fn valueless_blocks(body: &Body, type_table: &TypeTable) -> Vec<(BlockId, bool)> {
    let unit = |id| matches!(type_table.get(id), ResolvedType::Unit);
    let mut out = Vec::new();
    let mut stack = vec![(NodeRef::Block(body.root), false)];
    while let Some((node, under_loop)) = stack.pop() {
        let mut arms: Vec<BlockId> = Vec::new();
        let mut enters_loop = false;
        match node {
            NodeRef::Stmt(s) => match &body.stmts[s].kind {
                StmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => arms.extend([Some(*then_block), *else_block].into_iter().flatten()),
                StmtKind::Loop { body: b } => {
                    arms.push(*b);
                    enters_loop = true;
                }
                _ => {}
            },
            NodeRef::Expr(e) if unit(body.exprs[e].type_id) => match &body.exprs[e].kind {
                ExprKind::If {
                    then_branch,
                    else_branch,
                    ..
                } => arms.extend([Some(*then_branch), *else_branch].into_iter().flatten()),
                ExprKind::Switch {
                    arms: a, default, ..
                } => {
                    arms.extend(a.iter().copied());
                    arms.push(*default);
                }
                ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => arms.push(*b),
                _ => {}
            },
            _ => {}
        }
        let inner = under_loop || enters_loop;
        out.extend(arms.iter().map(|&b| (b, inner)));
        body.for_each_child(node, |c| stack.push((c, inner)));
    }
    out
}

/// Whether the region is already out of line: empty, or one call and nothing
/// else. Moving a lone call into a function that only makes that call adds a
/// frame and removes no code — and, since that is the shape this pass leaves
/// behind, declining it is also what makes a second scan a fixed point.
fn is_already_outlined(body: &Body, region: &[StmtId]) -> bool {
    let [only] = region else {
        return region.is_empty();
    };
    let StmtKind::Expr(op) = &body.stmts[*only].kind else {
        return false;
    };
    op.as_expr()
        .is_some_and(|e| matches!(&body.exprs[e].kind, ExprKind::Call { .. }))
}

/// Whether `stmt` is a bare `builtin::cold_path()` call.
fn is_cold_marker(body: &Body, stmt: StmtId, descriptors: &[FunctionRef]) -> bool {
    let StmtKind::Expr(op) = &body.stmts[stmt].kind else {
        return false;
    };
    let Some(expr) = op.as_expr() else {
        return false;
    };
    matches!(
        &body.exprs[expr].kind,
        ExprKind::Call { func_id, .. }
            if callee_descriptor(descriptors, *func_id).builtin_name().as_deref()
                == Some("builtin::cold_path")
    )
}

/// Whether the region can become a call without changing what the function
/// does.
///
/// Two conditions, both about what crosses the new boundary. Control must not
/// leave the region, since a `return` in a function of its own returns from the
/// wrong frame. And every local it touches must be one the call can hand over:
/// a parameter, passed straight through as the enclosing function received it,
/// or one the region declares itself.
fn is_movable(body: &Body, region: &[StmtId], param_count: u32, under_loop: bool) -> bool {
    let mut declared: IndexSet<u32> = IndexSet::default();
    let mut mentioned: IndexSet<u32> = IndexSet::default();
    let mut seen: IndexSet<ValueId> = IndexSet::default();
    let mut stack: Vec<NodeRef> = region.iter().map(|&s| NodeRef::Stmt(s)).collect();
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Stmt(s) => {
                if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind {
                    declared.insert(*local_index);
                }
            }
            NodeRef::Pat(p) => {
                if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                    declared.insert(*local_index);
                }
            }
            NodeRef::Expr(e) => {
                if let ExprKind::Local { index, .. } = &body.exprs[e].kind {
                    mentioned.insert(*index);
                }
            }
            NodeRef::Block(_) => {}
        }
        body.for_each_operand(node, |op| {
            if let Some(v) = op.as_value() {
                body.values
                    .collect_opaque_locals_seen(v, &mut seen, &mut mentioned);
            }
        });
        body.for_each_child(node, |c| stack.push(c));
    }
    let outside = mentions_outside(body, region);
    let foreign: Vec<u32> = mentioned
        .iter()
        .copied()
        .filter(|&idx| {
            idx >= param_count
                && !declared.contains(&idx)
                // A slot nothing outside the region touches moves with it: the
                // helper inherits the frame, so the index still denotes the same
                // local, and no one is left behind to read it. Only under a loop
                // is that different — a second entry would find the slot fresh
                // instead of holding what the first left.
                && (outside.contains(&idx) || under_loop)
        })
        .collect();
    let escapes = control_escapes(body, region);
    crate::compiler_trace!(
        "cold_outline",
        "  escapes={escapes} foreign={foreign:?} params={param_count} under_loop={under_loop}"
    );
    !escapes && foreign.is_empty()
}

/// Every local mentioned in the body *outside* `region`, a declaration counting
/// as a mention: an owner outside is what makes a local the enclosing
/// function's rather than the region's.
fn mentions_outside(body: &Body, region: &[StmtId]) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    let mut seen: IndexSet<ValueId> = IndexSet::default();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node {
            if region.contains(&s) {
                continue;
            }
            if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind {
                out.insert(*local_index);
            }
        }
        if let NodeRef::Pat(p) = node
            && let PatKind::Binding { local_index, .. } = &body.pats[p].kind
        {
            out.insert(*local_index);
        }
        if let NodeRef::Expr(e) = node
            && let ExprKind::Local { index, .. } = &body.exprs[e].kind
        {
            out.insert(*index);
        }
        body.for_each_operand(node, |op| {
            if let Some(v) = op.as_value() {
                body.values
                    .collect_opaque_locals_seen(v, &mut seen, &mut out);
            }
        });
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

/// Whether control can leave the region. A `return` always can. A `break` or
/// `continue` only does when its target — a label, or the nearest enclosing
/// loop — is outside, which the region's own labels and loop depth decide.
fn control_escapes(body: &Body, region: &[StmtId]) -> bool {
    let mut labels: Vec<&str> = Vec::new();
    region
        .iter()
        .any(|&s| escapes_from(body, NodeRef::Stmt(s), &mut labels, 0))
}

fn escapes_from<'a>(
    body: &'a Body,
    node: NodeRef,
    labels: &mut Vec<&'a str>,
    loops: usize,
) -> bool {
    let scoped = |labels: &mut Vec<&'a str>, label: &'a str, block, loops| {
        labels.push(label);
        let out = escapes_from(body, NodeRef::Block(block), labels, loops);
        labels.pop();
        out
    };
    match node {
        NodeRef::Stmt(s) => match &body.stmts[s].kind {
            StmtKind::Return { .. } => return true,
            StmtKind::Continue => return loops == 0,
            StmtKind::Break { label, .. } => match label {
                None => return loops == 0,
                Some(l) if !labels.contains(&l.as_str()) => return true,
                Some(_) => {}
            },
            StmtKind::Loop { body: b } => {
                return escapes_from(body, NodeRef::Block(*b), labels, loops + 1);
            }
            StmtKind::LabeledBlock { label, block } => return scoped(labels, label, *block, loops),
            _ => {}
        },
        NodeRef::Expr(e) => {
            if let ExprKind::LabeledBlock { label, block, .. } = &body.exprs[e].kind {
                return scoped(labels, label, *block, loops);
            }
        }
        _ => {}
    }
    let mut escaped = false;
    body.for_each_child(node, |c| {
        escaped = escaped || escapes_from(body, c, labels, loops);
    });
    escaped
}

/// Move `region` into a function of its own and leave a call in its place.
fn outline(project: &mut NirPackage, fi: usize, region: Region, ordinal: u32, unit: TypeId) {
    let id = project.next_func_id();
    let helper = {
        let parent = project.functions[fi].borrow();
        build_helper(&parent, &region, id, ordinal, unit)
    };
    let key = FunctionRef::from_resolved(&helper, helper.module_source.clone()).function_id();
    project.func_index.insert(key, id);
    project.functions.push(Rc::new(RefCell::new(helper)));

    let mut parent = project.functions[fi].borrow_mut();
    let span = parent.span;
    // The helper takes the enclosing parameter list unchanged, so the call is
    // each parameter passed straight back. `is_mut` is `is_mut_ref` — whether
    // the callee may write the caller's storage through the slot — the same
    // reading `lower` gives it.
    let params = parent.params.clone();
    let body = parent.body.as_mut().expect("a region implies a body");
    let args = params
        .iter()
        .map(|p| {
            let expr = body.exprs.push(ExprNode {
                kind: ExprKind::Local {
                    index: p.local_index,
                    name: p.name.clone(),
                },
                type_id: p.type_id,
                span,
            });
            ArenaCallArg {
                expr: Operand::Expr(expr),
                is_mut: p.is_mut_ref,
            }
        })
        .collect();
    let call = body.exprs.push(ExprNode {
        kind: ExprKind::Call {
            func_id: id,
            type_args: Vec::new(),
            args,
            has_receiver: false,
        },
        type_id: unit,
        span,
    });
    let stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(Operand::Expr(call)),
        span,
    });
    let block = &mut body.blocks[region.block];
    block.stmts.truncate(region.marker + 1);
    block.stmts.push(stmt);
}

/// The function a region becomes: the enclosing function's frame, and the
/// region as its whole body. Cloning the enclosing record rather than building
/// one field by field is what keeps the two in step — a field added to
/// [`NirFunction`] is inherited, and only what must differ is written here.
fn build_helper(
    parent: &NirFunction,
    region: &Region,
    id: FuncId,
    ordinal: u32,
    unit: TypeId,
) -> NirFunction {
    let parent_body = parent.body.as_ref().expect("a region implies a body");
    let mut body = Body::empty();
    // Identity remap: the helper keeps the enclosing frame, so every local index
    // in the moved statements still denotes the same local.
    let no_params = IndexMap::default();
    let no_labels = IndexMap::default();
    let ctx = InlineCtx::identity(parent.params.len() as u32, &no_params, &no_labels);
    let stmts: Vec<StmtId> = region
        .stmts
        .iter()
        .map(|&s| splice_stmt(&mut body, parent_body, s, &ctx))
        .collect();
    body.root = body.blocks.push(BlockNode {
        stmts,
        span: parent_body.blocks[region.block].span,
    });

    let mut helper = parent.clone();
    helper.id = Some(id);
    helper.name = crate::name::cold_region_helper_name(&parent.name, ordinal);
    helper.visibility = crate::ast::Visibility::Private;
    helper.is_export = false;
    helper.export_name = None;
    // A helper is a plain function of the enclosing frame: not the method, the
    // monomorphization, or the compiler item its enclosing function was, and
    // never re-entered by whatever recognised those.
    helper.method_info = None;
    helper.monomorph_info = None;
    // The moved statements are concrete — monomorphization ran long before the
    // optimizer — so the helper carries no type parameters to instantiate.
    helper.type_params = Vec::new();
    helper.impl_type_params = Vec::new();
    helper.compiler_item = None;
    helper.allocator_tag = None;
    helper.kind = FunctionKind::Regular;
    helper.scalarized_from = None;
    helper.inline_hint = InlineHint::default();
    helper.return_type = unit;
    helper.task_return_type = None;
    helper.body = Some(body);
    helper
}
