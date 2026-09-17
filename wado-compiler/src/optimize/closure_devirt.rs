//! Devirtualize an `IndirectCall` whose callee traces back to one
//! `ClosureToCanonical`, so the `$call` behind it becomes a direct call the
//! inliner can splice.
//!
//! `lower`'s fn-param specializer takes the closure that never leaves its
//! declaring local. Every iterator adaptor parks one in a struct field
//! instead, and the read of that field only exists after `inline`.

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::{CLOSURE_CALL_METHOD, FunctionId};
use crate::nir::{FuncId, FunctionRef};
use crate::nir_arena::{
    ArenaCallArg, BlockId, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::nir_visitor::reachable_exprs;

use super::arena_query::{is_addressed, is_pure_nontrapping_operand_typed, strip_refs};

use cranelift_entity::EntityRef;

/// How far a callee walk follows bindings, borrows, blocks and fields, so a
/// long chain costs a fixed amount. Well past what the adaptor shapes need.
const MAX_DEPTH: u32 = 12;

/// One functor's `$call`: the callee to name, and one flag per parameter
/// (`self` first) saying whether it writes the caller's storage. Its length is
/// the arity a call has to agree with.
struct CallTarget {
    func_id: FuncId,
    params_is_mut: Vec<bool>,
}

pub(super) struct ClosureDevirtRule {
    targets: IndexMap<FunctionId, CallTarget>,
}

/// Index every functor's `$call` under the key [`FunctionRef::closure_call`]
/// builds from what a `ClosureToCanonical` carries.
pub(super) fn build_closure_devirt(project: &NirPackage) -> ClosureDevirtRule {
    let suffix = format!("::{CLOSURE_CALL_METHOD}");
    let mut targets: IndexMap<FunctionId, CallTarget> = IndexMap::default();
    for (id, &func_id) in &project.func_index {
        let FunctionId::Free(free) = id else {
            continue;
        };
        if !free.name.ends_with(&suffix) {
            continue;
        }
        let func = project.functions[func_id.index()].borrow();
        if func.is_dead || func.body.is_none() || func.params.is_empty() {
            continue;
        }
        targets.insert(
            id.clone(),
            CallTarget {
                func_id,
                params_is_mut: func.params.iter().map(|p| p.is_mut_ref).collect(),
            },
        );
    }
    ClosureDevirtRule { targets }
}

/// The expression the operand denotes, past the references around it and the
/// blocks that only yield it, with the walk budget those hops left.
fn past_transparent(engine: &Engine, op: Operand, mut depth: u32) -> Option<(ExprId, u32)> {
    let mut expr = strip_refs(engine.body, op.as_expr().filter(|_| depth > 0)?);
    while let Some(yielded) = engine.body.block_yield(expr) {
        depth -= 1;
        expr = strip_refs(engine.body, yielded.as_expr().filter(|_| depth > 0)?);
    }
    Some((expr, depth))
}

/// The `ClosureToCanonical` the operand's value was built by, following the
/// bindings, borrows, blocks and struct fields between the two.
fn resolve_canonical(engine: &mut Engine, op: Operand, depth: u32) -> Option<ExprId> {
    let (expr, depth) = past_transparent(engine, op, depth)?;
    match &engine.body.exprs[expr].kind {
        ExprKind::ClosureToCanonical { .. } => Some(expr),
        ExprKind::Cast { expr: inner, .. } => {
            let inner = *inner;
            resolve_canonical(engine, inner, depth - 1)
        }
        ExprKind::Local { index, .. } => {
            let index = *index;
            let value = binding_value(engine, index)?;
            resolve_canonical(engine, value, depth - 1)
        }
        ExprKind::FieldAccess {
            expr: base,
            field_index,
            ..
        } => {
            let (base, field_index) = (*base, *field_index);
            let field = struct_literal_field(engine, base, field_index, depth - 1)?;
            resolve_canonical(engine, field, depth - 1)
        }
        _ => None,
    }
}

/// The struct literal the operand's value was built by. Every local holding
/// the object on the way must be read through a plain field access alone —
/// that is what keeps the field the literal wrote the field the read sees.
fn resolve_struct_literal(engine: &mut Engine, op: Operand, depth: u32) -> Option<ExprId> {
    let (expr, depth) = past_transparent(engine, op, depth)?;
    match &engine.body.exprs[expr].kind {
        ExprKind::StructLiteral { .. } => Some(expr),
        ExprKind::Local { index, .. } => {
            let index = *index;
            if !field_read_only_local(engine, index) {
                return None;
            }
            let value = binding_value(engine, index)?;
            resolve_struct_literal(engine, value, depth - 1)
        }
        _ => None,
    }
}

/// The value bound to `local`, when the body binds it exactly once.
fn binding_value(engine: &mut Engine, local: u32) -> Option<Operand> {
    if !engine.local_has_one_version(local) {
        return None;
    }
    let stmt = engine.local_def(local)?;
    let StmtKind::Let { value, .. } = &engine.body.stmts[stmt].kind else {
        return None;
    };
    Some(*value)
}

/// The operand `base.field_index` reads, when `base` names a fresh struct
/// literal nothing can write through.
fn struct_literal_field(
    engine: &mut Engine,
    base: Operand,
    field_index: u32,
    depth: u32,
) -> Option<Operand> {
    let literal = resolve_struct_literal(engine, base, depth)?;
    let ExprKind::StructLiteral { fields, .. } = &engine.body.exprs[literal].kind else {
        return None;
    };
    fields.get(field_index as usize).map(|f| f.value)
}

/// Whether every mention of `local` is a plain field read: nothing writes
/// through it, borrows a field off it, or hands it to a callee.
fn field_read_only_local(engine: &mut Engine, local: u32) -> bool {
    if engine.promoted_read_count(local) > 0 || engine.body_address_taken().contains(&local) {
        return false;
    }
    let reads = engine.local_reads(local).to_vec();
    reads.iter().all(|&read| {
        let Some(NodeRef::Expr(access)) = engine.parent_of(NodeRef::Expr(read)) else {
            return false;
        };
        matches!(engine.body.exprs[access].kind, ExprKind::FieldAccess { .. })
            && !engine.is_assign_target(access)
            && !is_addressed(engine, access)
    })
}

/// Every enclosing block of `node`, innermost first, each with the index of
/// the statement of that block the node is under.
fn ancestor_positions(engine: &Engine, node: NodeRef) -> Vec<(BlockId, usize)> {
    let mut out = Vec::new();
    let mut cur = node;
    while let Some(parent) = engine.parent_of(cur) {
        if let NodeRef::Block(block) = parent {
            let NodeRef::Stmt(stmt) = cur else {
                break;
            };
            let Some(at) = engine.body.blocks[block]
                .stmts
                .iter()
                .position(|&s| s == stmt)
            else {
                break;
            };
            out.push((block, at));
        }
        cur = parent;
    }
    out
}

/// Whether the wrapper's statement runs before the call's, in a block that
/// holds them both.
///
/// The devirtualized call is what makes the wrapper's own value dead, so a
/// later pass is free to drop the region the wrapper sits in. Nothing outside
/// that region may be left reading the binding it carries.
fn runs_before(engine: &Engine, canonical: ExprId, call: ExprId) -> bool {
    let Some(&(block, at)) = ancestor_positions(engine, NodeRef::Expr(canonical)).first() else {
        return false;
    };
    ancestor_positions(engine, NodeRef::Expr(call))
        .into_iter()
        .any(|(call_block, call_at)| call_block == block && call_at > at)
}

/// The local the wrapper's functor is parked in, when it holds the same object
/// at a later call as here: bound once, with no reference into it.
fn functor_local(engine: &mut Engine, canonical: ExprId) -> Option<(u32, String)> {
    let ExprKind::ClosureToCanonical { functor, .. } = &engine.body.exprs[canonical].kind else {
        return None;
    };
    let ExprKind::Local { index, name } = &engine.body.exprs[functor.as_expr()?].kind else {
        return None;
    };
    let (index, name) = (*index, name.clone());
    if !engine.local_has_one_version(index) || engine.body_address_taken().contains(&index) {
        return None;
    }
    Some((index, name))
}

/// Every `let x = ClosureToCanonical { … }` among `stmts` whose functor is not
/// a local yet, as its position, the wrapper, and the functor.
fn unparked_wrappers(engine: &Engine, stmts: &[StmtId]) -> Vec<(usize, ExprId, ExprId)> {
    stmts
        .iter()
        .enumerate()
        .filter_map(|(at, &stmt)| {
            let StmtKind::Let { value, .. } = &engine.body.stmts[stmt].kind else {
                return None;
            };
            let canonical = value.as_expr()?;
            let ExprKind::ClosureToCanonical { functor, .. } = &engine.body.exprs[canonical].kind
            else {
                return None;
            };
            let functor = functor.as_expr()?;
            (!matches!(engine.body.exprs[functor].kind, ExprKind::Local { .. }))
                .then_some((at, canonical, functor))
        })
        .collect()
}

impl ClosureDevirtRule {
    /// The `$call` this canonical wrapper dispatches to, for a call of
    /// `arg_count` arguments.
    fn target(&self, engine: &Engine, canonical: ExprId, arg_count: usize) -> Option<&CallTarget> {
        let ExprKind::ClosureToCanonical {
            functor_id,
            closure_module,
            ..
        } = &engine.body.exprs[canonical].kind
        else {
            return None;
        };
        let target = self
            .targets
            .get(&FunctionRef::closure_call(closure_module, *functor_id).function_id())?;
        (target.params_is_mut.len() == arg_count + 1).then_some(target)
    }

    /// The wrapper `call` dispatches through, when this rule can make the
    /// dispatch direct.
    fn devirt_site(&self, engine: &mut Engine, call: ExprId) -> Option<ExprId> {
        let ExprKind::IndirectCall { callee, args } = &engine.body.exprs[call].kind else {
            return None;
        };
        let (callee, arity) = (*callee, args.len());
        // Naming the functor's local drops the callee operand, and with it
        // whatever the trace saw through — an inlined helper's writes, say.
        if !is_pure_nontrapping_operand_typed(engine.body, callee, engine.value_graph_type_table())
        {
            return None;
        }
        let canonical = resolve_canonical(engine, callee, MAX_DEPTH)?;
        let devirtualizable =
            runs_before(engine, canonical, call) && self.target(engine, canonical, arity).is_some();
        devirtualizable.then_some(canonical)
    }

    /// Every wrapper in the body some `IndirectCall` dispatches through.
    /// Parking a functor nothing devirtualizes would only add a binding.
    fn devirt_sites(&self, engine: &mut Engine) -> IndexSet<ExprId> {
        reachable_exprs(engine.body)
            .into_iter()
            .filter_map(|call| self.devirt_site(engine, call))
            .collect()
    }
}

impl Rule for ClosureDevirtRule {
    /// `f(args)` where `f`'s value came from one functor → `$call(f, args)`.
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let Some(canonical) = self.devirt_site(engine, id) else {
            return false;
        };
        let Some((local, name)) = functor_local(engine, canonical) else {
            return false;
        };
        let ExprKind::IndirectCall { args, .. } = &engine.body.exprs[id].kind else {
            unreachable!("`devirt_site` matched an IndirectCall");
        };
        let args = args.clone();
        let target = self
            .target(engine, canonical, args.len())
            .expect("`devirt_site` found this target");
        let (func_id, params_is_mut) = (target.func_id, target.params_is_mut.clone());
        let type_id = engine.locals()[local as usize].type_id;
        let span = engine.body.exprs[id].span;
        let receiver = engine.alloc_expr(ExprKind::Local { index: local, name }, type_id, span);
        let args = args
            .into_iter()
            .zip(params_is_mut.iter().skip(1))
            .map(|(expr, &is_mut)| ArenaCallArg { expr, is_mut })
            .collect();
        let call = ExprKind::method_call(func_id, Operand::Expr(receiver), params_is_mut[0], args);
        engine.replace_expr_kind(id, call);
        true
    }

    /// Park a canonical wrapper's functor in a local, so the call sites reached
    /// by the wrapper have a handle on the one object it holds.
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        let candidates = unparked_wrappers(engine, &stmts);
        if candidates.is_empty() {
            return false;
        }
        let sites = self.devirt_sites(engine);
        let Some((at, canonical, functor)) = candidates
            .into_iter()
            .find(|&(_, canonical, _)| sites.contains(&canonical))
        else {
            return false;
        };
        let type_id = engine.body.exprs[functor].type_id;
        let span = engine.body.exprs[functor].span;
        let name = format!("$functor_{}", functor.index());
        let local = engine.alloc_local(name.clone(), type_id, false);
        let bind = engine.alloc_stmt(
            StmtKind::Let {
                name: name.clone(),
                local_index: local,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: Operand::Expr(functor),
                skip_value_copy: true,
            },
            span,
        );
        let read = engine.alloc_expr(ExprKind::Local { index: local, name }, type_id, span);
        let ExprKind::ClosureToCanonical {
            functor_id,
            target_fn_type,
            closure_module,
            ..
        } = engine.body.exprs[canonical].kind.clone()
        else {
            unreachable!("`unparked_wrappers` matched a ClosureToCanonical");
        };
        engine.replace_expr_kind(
            canonical,
            ExprKind::ClosureToCanonical {
                functor: Operand::Expr(read),
                functor_id,
                target_fn_type,
                closure_module,
            },
        );
        let mut kept = stmts;
        kept.insert(at, bind);
        engine.set_block_stmts(id, kept);
        true
    }
}
