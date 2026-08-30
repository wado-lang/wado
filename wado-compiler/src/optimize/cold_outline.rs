//! Move what a `cold_path()` marker opens into a function of its own, so that
//! `inline`'s cold discount describes the callee instead of promising a split.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionKind, FunctionRef, InlineHint, NirFunction, NirLocal, NirParam};
use crate::nir_arena::{
    ArenaCallArg, BlockId, BlockNode, Body, ExprKind, ExprNode, NodeRef, Operand, PatKind, StmtId,
    StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueId;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use cranelift_entity::EntityRef;

use super::dce::DescriptorCache;
use super::inline::{InlineCtx, splice_stmt};

/// Split every cold region the preconditions admit, in every function —
/// including the ones this pass itself creates, so a region nested inside a
/// region is reached on a later round.
pub fn outline_cold_regions(
    project: &mut NirPackage,
    descriptor_cache: &mut DescriptorCache,
) -> bool {
    // The marker's own id, so recognising one is an integer compare rather than
    // a descriptor lookup per call node. A program that never names it has no
    // region to find at all.
    let Some(cold) = cold_path_id(descriptor_cache.descriptors(project)) else {
        return false;
    };
    // Interned once: the table is borrowed immutably while a region is being
    // classified, and both are what a helper can return.
    let exits = {
        let mut table = project.type_table.borrow_mut();
        Exits {
            unit: table.intern(ResolvedType::Unit),
            never: table.intern(ResolvedType::Never),
        }
    };
    let mut changed = false;
    let mut fi = 0;
    while fi < project.functions.len() {
        let mut ordinal = 0;
        while let Some(region) = find_region(project, fi, cold, exits, descriptor_cache) {
            outline(project, fi, region, ordinal);
            ordinal += 1;
            changed = true;
        }
        fi += 1;
    }
    changed
}

/// The two types a helper can return: `()` when the region falls through, `!`
/// when it cannot.
#[derive(Clone, Copy)]
struct Exits {
    unit: TypeId,
    never: TypeId,
}

/// The `FuncId` of `builtin::cold_path`, if the package resolved one.
fn cold_path_id(descriptors: &[FunctionRef]) -> Option<FuncId> {
    descriptors
        .iter()
        .position(|d| d.builtin_name().as_deref() == Some("builtin::cold_path"))
        .map(FuncId::new)
}

/// A cold region: the statements after a `cold_path()` marker, to the end of
/// the block holding it.
struct Region {
    block: BlockId,
    /// Position of the marker in `block`; the region is everything after it.
    marker: usize,
    stmts: Vec<StmtId>,
    /// Locals of the enclosing frame the region reads, which the helper takes
    /// as parameters past the ones it inherits. Ascending.
    args: Vec<u32>,
    /// What the helper returns, and so what the call in its place is worth to
    /// every later pass: `!` when the region cannot fall through — the shape a
    /// panic guard has, and what proves the guarded branch never continues.
    /// Losing that turns a bounds check into an ordinary `if`.
    return_type: TypeId,
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
    cold: FuncId,
    exits: Exits,
    descriptor_cache: &mut DescriptorCache,
) -> Option<Region> {
    let descriptors = descriptor_cache.descriptors(project);
    let func = project.functions[fi].borrow();
    if func.is_dead || !is_splittable(&func) {
        return None;
    }
    let body = func.body.as_ref()?;
    // Nearly every function has no marker, so settle that with one linear scan
    // of the arena before classifying its blocks.
    if !body
        .exprs
        .values()
        .any(|n| matches!(&n.kind, ExprKind::Call { func_id, .. } if *func_id == cold))
    {
        return None;
    }
    let params = func.params.len();
    let type_table = project.type_table.borrow();
    for (block, under_loop) in valueless_blocks(body, &type_table) {
        let stmts = &body.blocks[block].stmts;
        let Some(marker) = stmts.iter().position(|&s| is_cold_marker(body, s, cold)) else {
            continue;
        };
        let region: Vec<StmtId> = stmts[marker + 1..].to_vec();
        if buys_nothing(body, &type_table, descriptors, &region, params) {
            continue;
        }
        crate::compiler_trace!(
            "cold_outline",
            "{}: region of {} stmt(s), {} to hold",
            func.name,
            region.len(),
            super::inline::region_size(body, &type_table, descriptors, &region),
        );
        if let Some(args) = free_vars(body, &region, params as u32, under_loop, &func, &type_table)
        {
            // A region ending in a `-> !` call cannot fall through, so the
            // helper inherits that: the call left behind still tells every
            // later pass the branch never continues.
            let diverges = region.last().is_some_and(|&s| match &body.stmts[s].kind {
                StmtKind::Expr(op) => op
                    .as_expr()
                    .is_some_and(|e| type_table.is_never(body.exprs[e].type_id)),
                _ => false,
            });
            return Some(Region {
                block,
                marker,
                args,
                return_type: if diverges { exits.never } else { exits.unit },
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

/// Whether moving the region would buy nothing: it holds no more than the call
/// that would replace it. Weighing the region rather than counting its
/// statements is what sees a lone `panic(…)` whose argument builds a message —
/// one statement, and the bulk of the function.
///
/// This is also what makes a second scan a fixed point: what the pass leaves
/// behind is exactly a call of that size.
fn buys_nothing(
    body: &Body,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
    region: &[StmtId],
    param_count: usize,
) -> bool {
    region.is_empty()
        || super::inline::region_size(body, type_table, descriptors, region)
            <= super::inline::call_site_size(param_count)
}

/// Whether `stmt` is a bare `cold_path()` call.
fn is_cold_marker(body: &Body, stmt: StmtId, cold: FuncId) -> bool {
    let StmtKind::Expr(op) = &body.stmts[stmt].kind else {
        return false;
    };
    let Some(expr) = op.as_expr() else {
        return false;
    };
    matches!(&body.exprs[expr].kind, ExprKind::Call { func_id, .. } if *func_id == cold)
}

/// How the region's free variables cross the new boundary, or `None` when one
/// of them cannot.
///
/// Control must not leave the region, since a `return` in a function of its own
/// returns from the wrong frame. Beyond that, each local the region touches has
/// to be one the call can hand over. A parameter, or a local the region
/// declares, needs nothing. A local of the enclosing frame is one of three
/// cases: nothing outside reads it, so it moves with the region; the region only
/// reads it, so it rides in as an argument; or the region writes one the
/// enclosing function still reads, which no call can carry back.
fn free_vars(
    body: &Body,
    region: &[StmtId],
    param_count: u32,
    under_loop: bool,
    func: &NirFunction,
    type_table: &TypeTable,
) -> Option<Vec<u32>> {
    if control_escapes(body, region) {
        return None;
    }
    let inside = LocalRefs::collect(
        body,
        region.iter().map(|&s| NodeRef::Stmt(s)).collect(),
        &[],
    );
    // A declaration counts as a mention here: an owner outside is what makes a
    // local the enclosing function's rather than the region's.
    let outer = LocalRefs::collect(body, vec![NodeRef::Block(body.root)], region);
    let mut args = Vec::new();
    for idx in inside.mentioned.iter().copied() {
        if idx < param_count || inside.declared.contains(&idx) {
            continue;
        }
        // A slot nothing outside touches moves with the region: the helper
        // inherits the frame, so the index still denotes the same local and no
        // one is left behind to read it. Under a loop that differs — a second
        // entry would find the slot fresh instead of holding what the first left
        // — so it has to cross as an argument like any other.
        if !outer.declared.contains(&idx) && !outer.mentioned.contains(&idx) && !under_loop {
            continue;
        }
        // Reading it is what an argument can carry; a write the enclosing
        // function still reads is not.
        if inside.written.contains(&idx) || func.address_taken_locals.contains(&idx) {
            crate::compiler_trace!("cold_outline", "  local {idx} is written across the split");
            return None;
        }
        // An argument is read at the call, where the region read it under
        // whatever guard stands in front of it. For a primitive that is the same
        // value either way, the slot always holding one. A reference slot need
        // not: an assertion's short-circuit operands are filled only on the path
        // that evaluates them, and passing one the guard would have skipped
        // hands a null across a non-null parameter.
        if !matches!(
            type_table.get(func.locals[idx as usize].type_id),
            ResolvedType::Primitive(_)
        ) {
            crate::compiler_trace!("cold_outline", "  local {idx} may be unset at the call");
            return None;
        }
        args.push(idx);
    }
    args.sort_unstable();
    Some(args)
}

/// The locals a walk touched.
#[derive(Default)]
struct LocalRefs {
    declared: IndexSet<u32>,
    mentioned: IndexSet<u32>,
    written: IndexSet<u32>,
}

impl LocalRefs {
    /// Walk from `roots`, skipping each statement in `skip` and its subtree.
    fn collect(body: &Body, roots: Vec<NodeRef>, skip: &[StmtId]) -> Self {
        let mut refs = Self::default();
        let mut seen: IndexSet<ValueId> = IndexSet::default();
        let mut stack = roots;
        while let Some(node) = stack.pop() {
            match node {
                NodeRef::Stmt(s) => {
                    if skip.contains(&s) {
                        continue;
                    }
                    if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind {
                        refs.declared.insert(*local_index);
                    }
                }
                NodeRef::Pat(p) => {
                    if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                        refs.declared.insert(*local_index);
                    }
                }
                NodeRef::Expr(e) => match &body.exprs[e].kind {
                    ExprKind::Local { index, .. } => {
                        refs.mentioned.insert(*index);
                    }
                    ExprKind::Assign { target, .. } => {
                        if let ExprKind::Local { index, .. } = &body.exprs[*target].kind {
                            refs.written.insert(*index);
                        }
                    }
                    _ => {}
                },
                NodeRef::Block(_) => {}
            }
            body.for_each_operand(node, |op| {
                if let Some(v) = op.as_value() {
                    body.values
                        .collect_opaque_locals_seen(v, &mut seen, &mut refs.mentioned);
                }
            });
            body.for_each_child(node, |c| stack.push(c));
        }
        refs
    }
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
fn outline(project: &mut NirPackage, fi: usize, region: Region, ordinal: u32) {
    let id = project.next_func_id();
    let helper = {
        let parent = project.functions[fi].borrow();
        build_helper(&parent, &region, id, ordinal)
    };
    let key = FunctionRef::from_resolved(&helper, helper.module_source.clone()).function_id();
    project.func_index.insert(key, id);
    project.functions.push(Rc::new(RefCell::new(helper)));

    let mut parent = project.functions[fi].borrow_mut();
    let span = parent.span;
    // The helper takes the enclosing parameter list, then the locals the region
    // reads, so the call passes each parameter straight back and each of those
    // locals by value. `is_mut` is `is_mut_ref` — whether the callee may write
    // the caller's storage through the slot — the same reading `lower` gives it.
    let passed: Vec<(u32, TypeId, String, bool)> = parent
        .params
        .iter()
        .map(|p| (p.local_index, p.type_id, p.name.clone(), p.is_mut_ref))
        .chain(region.args.iter().map(|&i| {
            let l = &parent.locals[i as usize];
            (i, l.type_id, l.name.clone(), false)
        }))
        .collect();
    let body = parent.body.as_mut().expect("a region implies a body");
    let args = passed
        .iter()
        .map(|(index, type_id, name, is_mut)| {
            let expr = body.exprs.push(ExprNode {
                kind: ExprKind::Local {
                    index: *index,
                    name: name.clone(),
                },
                type_id: *type_id,
                span,
            });
            ArenaCallArg {
                expr: Operand::Expr(expr),
                is_mut: *is_mut,
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
        type_id: region.return_type,
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
/// region as its whole body. Cloned from the enclosing record so a field added
/// to [`NirFunction`] is inherited, leaving only what must differ written here.
fn build_helper(parent: &NirFunction, region: &Region, id: FuncId, ordinal: u32) -> NirFunction {
    let parent_body = parent.body.as_ref().expect("a region implies a body");
    let inherited = parent.params.len() as u32;
    // The helper's frame is the enclosing one with the region's read-only
    // locals lifted to parameters: each takes the next slot past the inherited
    // ones, and every other local shifts by as many.
    let lifted: IndexMap<u32, u32> = region
        .args
        .iter()
        .enumerate()
        .map(|(k, &idx)| (idx, inherited + k as u32))
        .collect();
    let no_labels = IndexMap::default();
    let ctx = InlineCtx::lifting(inherited, &lifted, &no_labels);

    let mut body = Body::empty();
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
    helper
        .params
        .extend(region.args.iter().enumerate().map(|(k, &idx)| {
            let local = &parent.locals[idx as usize];
            NirParam {
                name: local.name.clone(),
                type_id: local.type_id,
                local_index: inherited + k as u32,
                is_mut: false,
                is_mut_ref: false,
                span: parent.span,
            }
        }));
    helper.locals = helper
        .params
        .iter()
        .map(|p| NirLocal {
            name: p.name.clone(),
            type_id: p.type_id,
            is_mut: p.is_mut,
        })
        .chain(parent.locals[inherited as usize..].iter().cloned())
        .collect();
    // The lifted locals now live in two slots; the shifted copy is dead in the
    // helper, which is what `elide_local` and the backend's local census expect.
    helper.address_taken_locals = parent
        .address_taken_locals
        .iter()
        .map(|&i| ctx.local(i))
        .collect();
    helper.stores_aliased_locals = parent
        .stores_aliased_locals
        .iter()
        .map(|&i| ctx.local(i))
        .collect();
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
    helper.return_type = region.return_type;
    helper.task_return_type = None;
    helper.body = Some(body);
    helper
}
