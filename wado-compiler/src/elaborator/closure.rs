//! Annotation pass for closure expressions. The body walk allocates the
//! synthetic `$ref_<var>` locals, address-takes their outer bindings, walks the
//! body so its `ModuleSemantics` lands, projects the closure's `fn(…)` type for
//! the caller's typecheck, and records the
//! [`super::sem::types::ClosureCaptureInfo`] reify rebuilds from.

use crate::ModuleSource;
use crate::ast::{self, Expr};
use crate::compiler_host::CompilerHost;
use crate::effect_check::{OwnedEffectData, WalkInProgress, call_rows, closure_row};
use crate::name::capture_ref_name;
use crate::tir::{
    CaptureSource, EffectRef, EffectRows, ResolvedType, RowClosing, TirCapture, TypeId,
};
use crate::type_table::TypeTable;

use super::Elaborator;
use super::types::{FunctionContext, OuterReach, TypeError, VarRef};
use super::tysys::TypeSystem;
use crate::elaborator::sem::types::{CaptureEntry, ClosureCaptureInfo, MutCapture};
use crate::hashmap::IndexMap;

/// The captures reify emits: the seeded environment, each source resolved
/// against `ctx`, and the recorded types, which hole inference substitutes into
/// after annotate is done.
pub(super) fn relink_recorded_captures(
    recorded: &[CaptureEntry],
    closure_ctx: &FunctionContext,
    ctx: &mut FunctionContext,
    type_table: &TypeTable,
) -> Vec<TirCapture> {
    let linked = link_parent_captures(closure_ctx, ctx);
    assert_eq!(
        linked.len(),
        recorded.len(),
        "in {}: the seeded environment pairs up with the record it was seeded from",
        closure_ctx.function_name
    );
    linked
        .into_iter()
        .zip(recorded)
        .map(|(linked, recorded)| {
            assert!(
                !closure_ctx.outer_box_types.contains_key(&linked.name)
                    || type_table.is_mut_ref(recorded.type_id),
                "in {}: reify reads `{}` through its box, so annotate must have recorded the box",
                closure_ctx.function_name,
                linked.name
            );
            TirCapture {
                type_id: recorded.type_id,
                ..linked
            }
        })
        .collect()
}

/// Expected function-type info extracted from an `expected_type` hint.
struct ExpectedFn {
    ty: TypeId,
    params: Vec<TypeId>,
    return_type: TypeId,
}

/// The environment slot `ctx` holds `name` in, registering the capture if this
/// is the first inner closure to ask for it.
fn parent_capture_slot(ctx: &mut FunctionContext, name: &str) -> u32 {
    match ctx.lookup_or_capture(name) {
        Some(VarRef::Capture { index, .. } | VarRef::DerefCapture { index, .. }) => index,
        Some(VarRef::Local { .. }) => {
            unreachable!(
                "in {}: `{name}` is a local of the frame that was recorded as reaching it by capture",
                ctx.function_name
            )
        }
        None => {
            unreachable!(
                "in {}: `{name}` was reached through the enclosing environment but is not in it",
                ctx.function_name
            )
        }
    }
}

/// One [`TirCapture`] per capture, resolved against `ctx`, the frame that builds
/// the closure. A binding `ctx` only reaches through its own environment makes
/// `ctx` capture it too, which is what makes capture transitive.
pub(super) fn link_parent_captures(
    closure_ctx: &FunctionContext,
    ctx: &mut FunctionContext,
) -> Vec<TirCapture> {
    closure_ctx
        .get_captures()
        .into_iter()
        .map(|(name, local, reach)| {
            let source = match reach {
                OuterReach::ParentLocal(index) => CaptureSource::Local(index),
                OuterReach::ParentEnv => CaptureSource::Capture(parent_capture_slot(ctx, &name)),
            };
            TirCapture {
                name,
                source,
                type_id: local.type_id,
                through_ref: false,
            }
        })
        .collect()
}

impl TypeSystem {
    fn extract_expected_fn(&self, expected_type: Option<TypeId>) -> Option<ExpectedFn> {
        let tid = expected_type?;
        let tt = self.type_table.borrow();
        // See through newtype layers so a closure assigned to a `type Handler =
        // fn(...)` newtype still gets its parameter types inferred from the
        // underlying fn signature.
        let base_id = tt.representation_head(tid);
        match tt.get(base_id) {
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => Some(ExpectedFn {
                ty: tid,
                params: params.clone(),
                return_type: *return_type,
            }),
            _ => None,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve a closure parameter's type, defaulting unannotated params to
    /// the expected-type's positional param when one is available.
    fn closure_param_type(
        &mut self,
        param: &ast::ClosureParam,
        index: usize,
        expected_fn: Option<&ExpectedFn>,
    ) -> TypeId {
        if let Some(ty) = &param.ty {
            self.reject_unresolved_annotation(ty);
            return self.resolve_type(ty);
        }
        if let Some(ef) = expected_fn {
            if let Some(&t) = ef.params.get(index) {
                return t;
            }
            // The fn-type check passes an unknown parameter, so the arity is
            // reported here.
            let (expected, _) = self
                .tysys
                .type_table
                .borrow()
                .names_for_mismatch(ef.ty, ef.ty);
            let _ = self.emit(TypeError::TypeMismatch {
                expected,
                found: format!("a closure of {} or more parameters", index + 1),
                span: param.name_span,
            });
            return TypeTable::UNKNOWN;
        }
        let _ = self.emit(TypeError::CannotInferType {
            message: format!(
                "cannot infer the type of closure parameter `{}`; annotate it",
                param.name
            ),
            span: param.name_span,
        });
        TypeTable::UNKNOWN
    }

    /// Reject default parameter values on closures. Parser accepts the syntax
    /// for uniform recovery, but defaults cannot survive the fn-type erasure
    /// closures undergo, so they're rejected here.
    fn reject_closure_defaults(&mut self, closure: &ast::ClosureExpr) {
        for param in &closure.params {
            if let Some(default) = &param.default {
                let _ = self.emit(TypeError::DefaultInClosure {
                    param: param.name.clone(),
                    span: default.span(),
                });
            }
        }
    }

    pub(super) fn resolve_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        self.reject_closure_defaults(closure);
        let expected_fn = self.tysys.extract_expected_fn(expected_type);

        // Reify replays the `MutCapture`s in this order.
        let writes = Self::collect_capture_writes(closure);
        let mut deref_overrides: IndexMap<String, (String, TypeId)> = IndexMap::default();
        let mut mut_captures: Vec<MutCapture> = Vec::new();
        let mut any_mutating_capture = false;

        for var_name in writes.assigned.union(&writes.mut_referenced) {
            let written = writes.assigned.contains(var_name);
            let Some(local) = ctx.lookup(var_name) else {
                // A binding `ctx` only reaches by capture is boxed where it is
                // owned; writing through that box is still a mutating capture.
                any_mutating_capture |= written && ctx.binding(var_name).is_some_and(|b| b.is_mut);
                continue;
            };
            if local.is_mut {
                any_mutating_capture |= written;
                let inner_type = local.type_id;
                let outer_index = local.index;
                let ref_type = self.tysys.type_table.borrow_mut().make_mut_ref(inner_type);
                let ref_name = capture_ref_name(var_name);
                ctx.add_local(ref_name.clone(), ref_type, false, None);
                ctx.mut_captured_locals.insert(outer_index);

                mut_captures.push(MutCapture {
                    var_name: var_name.clone(),
                    ref_name: ref_name.clone(),
                    inner_type,
                    ref_type,
                });
                deref_overrides.insert(var_name.clone(), (ref_name, inner_type));
            }
        }

        // Open the closure scope with the deref overrides and walk params
        // + body so their facts land on `ModuleSemantics`.
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.tysys.type_table);
        closure_ctx.deref_overrides = deref_overrides;

        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let type_id = self.closure_param_type(p, i, expected_fn.as_ref());
                self.bind_local(
                    &mut closure_ctx,
                    p.id,
                    &p.name,
                    p.name_span,
                    p.is_mut,
                    type_id,
                );
                (p.name.clone(), type_id)
            })
            .collect();

        // An explicit `|params| -> Type ...` annotation is the authoritative
        // return type; otherwise fall back to the expected fn type from the
        // surrounding context (e.g. a `let f: fn(..) -> R = ..` binding).
        if let Some(ty) = closure.return_type.as_ref() {
            self.reject_unresolved_annotation(ty);
        }
        let declared_return = closure.return_type.as_ref().map(|ty| self.resolve_type(ty));
        // A bare type parameter belongs to the signature the call instantiates,
        // and the closure's body determines it: `fold(0, |acc, x| acc + x)`.
        let body_expected = declared_return.or_else(|| {
            expected_fn.as_ref().map(|ef| ef.return_type).filter(|&rt| {
                !matches!(
                    self.tysys.type_table.borrow().get(rt),
                    ResolvedType::TypeParam { .. }
                )
            })
        });
        // Seed the closure's return type before walking the body, so a `?`
        // operator in the body (which checks `ctx.return_type` for
        // Result/Option) resolves against the real return type instead of the
        // UNKNOWN placeholder. Symmetric to the same seeding in
        // `reify_closure`.
        if let Some(rt) = body_expected {
            closure_ctx.return_type = rt;
        }
        // A block body is a function body: its last statement is a
        // statement, its type comes from its `return`s or else from the type
        // its context expects, and it owes one on every path that does not
        // diverge. A single-expression body (`|c|
        // c.to_ascii_uppercase()`) is its value.
        let return_type = match &closure.body {
            ast::Expr::Block(block) => {
                self.resolve_block(block, &mut closure_ctx, None);
                // A closure body is its own label stack: a loop around the
                // closure binds no `break` / `continue` written inside it.
                self.validate_loop_jumps_ast(Some(block));
                let body_type = if self.ast_block_always_exits(block) {
                    TypeTable::NEVER
                } else {
                    TypeTable::UNIT
                };
                self.record_expression_type(closure.body.id(), body_type);
                let mut return_types = self.ast_return_types_in_block(block);
                self.settle_branch_holes(&mut return_types, None);
                let return_type = declared_return
                    .or(return_types.first().copied())
                    .or(self.settled_result_expectation(body_expected))
                    .unwrap_or(body_type);
                self.check_missing_return(return_type, block, closure.span);
                return_type
            }
            body => {
                let body_type = self.resolve_expr(body, &mut closure_ctx, body_expected);
                // A diverging body returns what its context expects, as a
                // block body's does: its type is not the closure's to settle.
                let diverged_into = body_expected.filter(|_| body_type == TypeTable::NEVER);
                declared_return.or(diverged_into).unwrap_or(body_type)
            }
        };

        // Only the walk knows which `&mut`s reach a capture's own storage.
        for var_name in std::mem::take(&mut closure_ctx.mut_ref_captures) {
            if closure_ctx.deref_overrides.contains_key(&var_name) {
                any_mutating_capture = true;
            } else if let Some(local) = ctx.lookup(&var_name) {
                assert!(
                    !local.is_mut,
                    "a `mut` receiver `{var_name}` is boxed before the walk"
                );
            } else {
                any_mutating_capture |= ctx.binding(&var_name).is_some_and(|b| b.is_mut);
                ctx.mut_ref_captures.insert(var_name);
            }
        }

        // The source each capture reads from belongs to this walk alone: reify
        // resolves it again against its own frame, so recording it would be a
        // second answer to one question.
        let recorded_captures = link_parent_captures(&closure_ctx, ctx)
            .into_iter()
            .map(|capture| CaptureEntry {
                name: capture.name,
                type_id: capture.type_id,
            })
            .collect();

        // WEP 2026-05-26: the only signal reify needs
        // for the closure's capture analysis.
        self.record_closure_captures(
            closure.id,
            ClosureCaptureInfo {
                mut_captures,
                captures: recorded_captures,
                is_mutating: any_mutating_capture,
                declared_return,
                return_type,
            },
        );

        // Project the closure's `fn(...)` / `fn mut(...)` type so the
        // caller's typecheck context (`expression_types[closure.id]`, etc.)
        // sees it. Reify rebuilds an equivalent type from its own facts.
        // Its row is a variable, solved here to what the body performs where
        // every row the body reads is solved already, and by the effect check
        // otherwise.
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let performed = self.closure_performs(closure);
        let site = RowClosing {
            span: closure.span,
            module: self.home_module(closure.id).source_path(),
        };
        let mut table = self.tysys.type_table.borrow_mut();
        let var = table.mint_effect_var();
        if let Some(performed) = performed {
            table.solve_effect_var(var.name(), performed, site);
        }
        let row = vec![var];
        let func_type =
            table.make_function_with_mut(any_mutating_capture, param_types, return_type, row);
        drop(table);

        ctx.source_bindings.extend(closure_ctx.source_bindings);
        func_type
    }

    /// What the body of `closure`, just walked, performs, where every row it
    /// reads is solved.
    fn closure_performs(&self, closure: &ast::ClosureExpr) -> Option<Vec<EffectRef>> {
        self.walk_effects_so_far(|facts, decls, module, solved| {
            closure_row(facts, decls, module, closure, solved)
        })?
    }

    /// Solve each variable the call `expr`, just resolved, carries out in its
    /// type, where every row it reads is solved: what reads it next, a bound
    /// among them, reads the row the effect check ends with.
    pub(super) fn solve_call_rows(&self, expr: &Expr) {
        let Some(solved) = self.walk_effects_so_far(|facts, decls, module, solved| {
            call_rows(facts, decls, module, expr, solved)
        }) else {
            return;
        };
        let site = RowClosing {
            span: expr.span(),
            module: self.home_module(expr.id()).source_path(),
        };
        let mut table = self.tysys.type_table.borrow_mut();
        for (var, row) in solved {
            table.solve_effect_var(&var, row, site.clone());
        }
    }

    /// What `walk` answers over the facts elaborated so far and the rows solved
    /// so far. `None` before the effect declarations are indexed.
    fn walk_effects_so_far<R>(
        &self,
        walk: impl FnOnce(&WalkInProgress<'_>, &OwnedEffectData, &ModuleSource, &EffectRows) -> R,
    ) -> Option<R> {
        let decls = self.effect_decls.as_ref()?;
        let types = self.tysys.type_table.borrow();
        let facts = WalkInProgress {
            types: &types,
            sem: &self.sem,
        };
        Some(walk(
            &facts,
            decls,
            &self.current_module_source,
            types.early_effect_rows(),
        ))
    }
}
