//! Devirtualize an `IndirectCall` whose callee traces back to one
//! `ClosureToCanonical`, so the `$call` behind it becomes a direct call the
//! inliner can splice.
//!
//! `lower` already rewrites a call through a closure-bearing local; what it
//! cannot see is a closure parked in a struct field and read back after
//! `inline` has copied the reader in. `xs.map(f).collect()` is that shape: the
//! functor goes into `IterMap.f`, and the loop `from_iter` leaves dispatches
//! through a `call_ref` and a wrapper per element.

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::{
    CLOSURE_CALL_METHOD, CLOSURE_STRUCT_PREFIX, FqTypeName, FunctionId, LocalMethodName, MethodName,
};
use crate::nir::{FuncId, FunctionRef, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, ExprId, ExprKind, NodeRef, Operand, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::tir::TypeId;

use super::arena_query::has_break_to;

use cranelift_entity::EntityRef;

/// How far the callee walk follows bindings, borrows, blocks and fields before
/// giving up. Every shape the iterator combinators leave is within four hops.
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

/// Index every functor's `$call` under the name
/// [`call_method_id`] spells from what a `ClosureToCanonical` carries.
pub(super) fn build_closure_devirt(project: &NirPackage) -> ClosureDevirtRule {
    let mut targets: IndexMap<FunctionId, CallTarget> = IndexMap::default();
    for (id, &func_id) in &project.func_index {
        let FunctionId::Free(free) = id else {
            continue;
        };
        if !free.name.ends_with(&format!("::{CLOSURE_CALL_METHOD}")) {
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

/// The name of functor `functor_id`'s `$call`, as `lower` minted it.
fn call_method_id(closure_module: &ModuleSource, functor_id: u32) -> FunctionId {
    let functor_fq = FqTypeName::shape(
        closure_module,
        &format!("{CLOSURE_STRUCT_PREFIX}{functor_id}"),
    );
    FunctionRef {
        module_source: closure_module.clone(),
        name: MethodName::format_local(&functor_fq, None, CLOSURE_CALL_METHOD),
        monomorph_info: None,
        method_info: Some(LocalMethodName::new(
            functor_fq,
            None,
            CLOSURE_CALL_METHOD.to_string(),
        )),
    }
    .function_id()
}

/// The `ClosureToCanonical` the operand's value was built by, following the
/// bindings, borrows, blocks and struct fields between the two.
fn resolve_canonical(engine: &mut Engine, op: Operand, depth: u32) -> Option<ExprId> {
    if depth == 0 {
        return None;
    }
    let expr = op.as_expr()?;
    match &engine.body.exprs[expr].kind {
        ExprKind::ClosureToCanonical { .. } => Some(expr),
        ExprKind::Cast { expr: inner, .. } => {
            let inner = *inner;
            resolve_canonical(engine, inner, depth - 1)
        }
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => {
            let inner = *inner;
            resolve_canonical(engine, inner, depth - 1)
        }
        ExprKind::Local { index, .. } => {
            let index = *index;
            let value = binding_value(engine, index)?;
            resolve_canonical(engine, value, depth - 1)
        }
        ExprKind::LabeledBlock { .. } => {
            let tail = block_tail(engine, expr)?;
            resolve_canonical(engine, tail, depth - 1)
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

/// The value a labeled block yields, when its last statement is that value and
/// no `break` names it.
fn block_tail(engine: &Engine, expr: ExprId) -> Option<Operand> {
    let ExprKind::LabeledBlock { label, block, .. } = &engine.body.exprs[expr].kind else {
        return None;
    };
    if has_break_to(engine.body, NodeRef::Block(*block), label) {
        return None;
    }
    let last = *engine.body.blocks[*block].stmts.last()?;
    let StmtKind::Expr(value) = &engine.body.stmts[last].kind else {
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

/// The struct literal the operand's value was built by. Every binding on the
/// way must be single-version, and every local holding the object must be read
/// through a plain field access alone — that is what keeps the field the
/// literal wrote the field the read sees.
fn resolve_struct_literal(engine: &mut Engine, op: Operand, depth: u32) -> Option<ExprId> {
    if depth == 0 {
        return None;
    }
    let expr = op.as_expr()?;
    match &engine.body.exprs[expr].kind {
        ExprKind::StructLiteral { .. } => Some(expr),
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => {
            let inner = *inner;
            resolve_struct_literal(engine, inner, depth - 1)
        }
        ExprKind::LabeledBlock { .. } => {
            let tail = block_tail(engine, expr)?;
            resolve_struct_literal(engine, tail, depth - 1)
        }
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

/// Whether every mention of `local` is a field read: no write through it, no
/// borrow of a field, no argument position, and no promoted read.
fn field_read_only_local(engine: &mut Engine, local: u32) -> bool {
    if engine.promoted_read_count(local) > 0 || engine.body_address_taken().contains(&local) {
        return false;
    }
    let reads = engine.local_reads(local).to_vec();
    reads.iter().all(|&read| {
        let Some(NodeRef::Expr(access)) = engine.parent_of(NodeRef::Expr(read)) else {
            return false;
        };
        if !matches!(engine.body.exprs[access].kind, ExprKind::FieldAccess { .. })
            || engine.is_assign_target(access)
        {
            return false;
        }
        !matches!(
            engine.parent_of(NodeRef::Expr(access)),
            Some(NodeRef::Expr(outer))
                if matches!(
                    engine.body.exprs[outer].kind,
                    ExprKind::Unary { op: NirUnaryOp::Ref | NirUnaryOp::MutRef, .. },
                )
        )
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

/// The functor the canonical wrapper holds, once it is parked in a local of its
/// own — the handle a direct call can name wherever the wrapper reaches.
fn functor_local(engine: &Engine, canonical: ExprId) -> Option<(u32, String, TypeId)> {
    let ExprKind::ClosureToCanonical { functor, .. } = &engine.body.exprs[canonical].kind else {
        return None;
    };
    let ExprKind::Local { index, name } = &engine.body.exprs[functor.as_expr()?].kind else {
        return None;
    };
    Some((
        *index,
        name.clone(),
        engine.locals()[*index as usize].type_id,
    ))
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
            .get(&call_method_id(closure_module, *functor_id))?;
        (target.params_is_mut.len() == arg_count + 1).then_some(target)
    }
}

impl Rule for ClosureDevirtRule {
    /// `f(args)` where `f`'s value came from one functor → `$call(f, args)`.
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let ExprKind::IndirectCall { callee, args } = &engine.body.exprs[id].kind else {
            return false;
        };
        let (callee, args) = (*callee, args.clone());
        let Some(canonical) = resolve_canonical(engine, callee, MAX_DEPTH) else {
            return false;
        };
        if !runs_before(engine, canonical, id) {
            return false;
        }
        let Some((local, name, type_id)) = functor_local(engine, canonical) else {
            return false;
        };
        let Some(target) = self.target(engine, canonical, args.len()) else {
            return false;
        };
        let (func_id, params_is_mut) = (target.func_id, target.params_is_mut.clone());
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
        let found = stmts.iter().enumerate().find_map(|(at, &stmt)| {
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
        });
        let Some((at, canonical, functor)) = found else {
            return false;
        };
        if !self.reaches_a_devirt_site(engine, canonical) {
            return false;
        }
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
            unreachable!("`found` matched a ClosureToCanonical");
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

impl ClosureDevirtRule {
    /// Whether some `IndirectCall` in the body dispatches through this wrapper.
    /// Splitting one nothing devirtualizes would only add a binding.
    fn reaches_a_devirt_site(&self, engine: &mut Engine, canonical: ExprId) -> bool {
        let calls: Vec<(ExprId, Operand, usize)> = engine
            .body
            .exprs
            .iter()
            .filter_map(|(id, node)| match &node.kind {
                ExprKind::IndirectCall { callee, args } => Some((id, *callee, args.len())),
                _ => None,
            })
            .collect();
        calls.into_iter().any(|(call, callee, arity)| {
            resolve_canonical(engine, callee, MAX_DEPTH) == Some(canonical)
                && runs_before(engine, canonical, call)
                && self.target(engine, canonical, arity).is_some()
        })
    }
}
