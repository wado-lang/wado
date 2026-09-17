//! Annotation pass for closure expressions. The body walk allocates the
//! synthetic `$ref_<var>` locals, address-takes their outer bindings, walks the
//! body so its `ModuleSemantics` lands, projects the closure's `fn(…)` type for
//! the caller's typecheck, and records the
//! [`super::sem::types::ClosureCaptureInfo`] reify rebuilds from.

use crate::hashmap::IndexSet;

use crate::ast::{self};
use crate::compiler_host::CompilerHost;
use crate::name::mut_capture_ref_name;
use crate::tir::{CaptureSource, ResolvedType, TirCapture, TypeId, TypeTable};

use super::Elaborator;
use super::types::{FunctionContext, OuterReach, TypeError, VarRef};
use crate::elaborator::sem::types::{CaptureEntry, ClosureCaptureInfo, MutCapture};
use crate::hashmap::IndexMap;

/// The captures reify emits: the seeded environment, each source resolved
/// against `ctx`, and the recorded types, which hole inference substitutes into
/// after annotate is done.
pub(super) fn relink_recorded_captures(
    recorded: &[CaptureEntry],
    closure_ctx: &FunctionContext,
    ctx: &mut FunctionContext,
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
        .map(|(linked, recorded)| TirCapture {
            type_id: recorded.type_id,
            ..linked
        })
        .collect()
}

/// Expected function-type info extracted from an `expected_type` hint.
struct ExpectedFn {
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
            }
        })
        .collect()
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Whether `ty` is a bare rigid type parameter.
    ///
    /// A closure is not constrained by an expected return type of that shape:
    /// the parameter belongs to the signature the call is instantiating, and
    /// the closure's own body is what determines it. Seeding the body with it
    /// would demand that the body produce an opaque type it cannot construct
    /// — `fold(0, |acc, x| acc + x)` asked the closure to return `Acc`.
    pub(super) fn is_rigid_type_param(&self, ty: TypeId) -> bool {
        matches!(
            self.tysys.type_table.borrow().get(ty),
            ResolvedType::TypeParam { .. }
        )
    }

    fn extract_expected_fn(&self, expected_type: Option<TypeId>) -> Option<ExpectedFn> {
        let tid = expected_type?;
        let tt = self.tysys.type_table.borrow();
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
                params: params.clone(),
                return_type: *return_type,
            }),
            _ => None,
        }
    }

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
        if let Some(ef) = expected_fn
            && let Some(t) = ef.params.get(index)
        {
            return *t;
        }
        TypeTable::UNKNOWN
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
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
        let expected_fn = self.extract_expected_fn(expected_type);

        // Collect outer bindings the body assigns to.
        let mut assigned_names: IndexSet<String> = IndexSet::default();
        Self::collect_mutated_vars(closure, &mut assigned_names);

        // For each assigned name that resolves to an outer `mut` local,
        // record a `MutCapture` (so reify replays the `$ref_<var>`
        // materialisation in the same order) and mark the outer local
        // address-taken. The `$ref_<var>` local slot is also reserved on
        // the outer `ctx` so any subsequent local-index accounting in the
        // parent function stays consistent with what reify will produce.
        let mut deref_overrides: IndexMap<String, (String, TypeId)> = IndexMap::default();
        let mut mut_captures: Vec<MutCapture> = Vec::new();
        let mut any_mutating_capture = false;

        for var_name in &assigned_names {
            let Some(local) = ctx.lookup(var_name) else {
                // A binding `ctx` only reaches by capture is boxed where it is
                // owned; writing through that box is still a mutating capture.
                any_mutating_capture |= ctx.binding(var_name).is_some_and(|b| b.is_mut);
                continue;
            };
            if local.is_mut {
                any_mutating_capture = true;
                let inner_type = local.type_id;
                let outer_index = local.index;
                let ref_type = self.tysys.type_table.borrow_mut().make_mut_ref(inner_type);
                let ref_name = mut_capture_ref_name(var_name);
                let ref_index = ctx.add_local(ref_name.clone(), ref_type, false, None);
                ctx.address_taken_locals.insert(outer_index);

                mut_captures.push(MutCapture {
                    var_name: var_name.clone(),
                    ref_name: ref_name.clone(),
                    inner_type,
                    ref_type,
                    ref_index,
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
                closure_ctx.add_local_at(
                    p.name.clone(),
                    type_id,
                    p.is_mut,
                    Some(p.id),
                    p.name_span,
                );
                self.record_local_symbol(p.id, &p.name, p.name_span, p.is_mut, type_id);
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
        let body_expected = declared_return.or_else(|| {
            expected_fn
                .as_ref()
                .map(|ef| ef.return_type)
                .filter(|&rt| !self.is_rigid_type_param(rt))
        });
        // Seed the closure's return type before walking the body, so a `?`
        // operator in the body (which checks `ctx.return_type` for
        // Result/Option) resolves against the real return type instead of the
        // UNKNOWN placeholder. Symmetric to the same seeding in
        // `reify_closure`.
        if let Some(rt) = body_expected {
            closure_ctx.return_type = rt;
        }
        let body_type = self.resolve_expr(&closure.body, &mut closure_ctx, body_expected);

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
            },
        );

        // A closure body is its own label stack: a loop around the closure
        // binds no `break` / `continue` written inside it.
        if let ast::Expr::Block(ref block) = closure.body {
            self.validate_loop_jumps_ast(Some(block));
        }

        // Diagnose missing returns / type mismatches on block-bodied
        // closures via the AST-walker, gating on the AST block shape so an
        // explicit `return` inside a partial branch reports the error with no
        // `TirExpr` to inspect. Single-expression closure bodies (e.g.
        // `|c: char| c.to_ascii_uppercase()`) take the body's type as
        // the return type directly — no missing-return check applies.
        let return_type = if let Some(dt) = declared_return {
            dt
        } else if let ast::Expr::Block(ref block) = closure.body {
            match self.ast_find_return_type_in_block(block) {
                Some(t) => {
                    if !self.ast_block_always_exits(block)
                        && body_type != t
                        && body_type != TypeTable::NEVER
                    {
                        let _ = self.emit(TypeError::MissingReturn {
                            return_type: self.tysys.type_table.borrow().type_name(t),
                            span: closure.span,
                        });
                    }
                    t
                }
                None if body_type == TypeTable::UNIT || body_type == TypeTable::NEVER => body_type,
                None => {
                    let _ = self.emit(TypeError::MissingReturn {
                        return_type: self.tysys.type_table.borrow().type_name(body_type),
                        span: closure.span,
                    });
                    TypeTable::UNIT
                }
            }
        } else {
            body_type
        };

        // Project the closure's `fn(...)` / `fn mut(...)` type so the
        // caller's typecheck context (`expression_types[closure.id]`, etc.)
        // sees it. Reify rebuilds an equivalent type from its own facts.
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.tysys.type_table.borrow_mut().make_function_with_mut(
            any_mutating_capture,
            param_types,
            return_type,
            Vec::new(),
        );

        // Placeholder — reify is the sole producer of the closure's TIR shape.
        drop(closure_ctx);
        func_type
    }
}
