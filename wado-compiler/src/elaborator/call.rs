//! Function call resolution.

use crate::hashmap::IndexMap;

use crate::ast::{self, Expr, Type};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName};
use crate::tir::{FunctionRef, MonomorphInfo, ResolvedType, TypeId, TypeTable};

use super::Elaborator;
use super::callee::{CalleeRef, StaticMethodRef};
use super::expr::BareCase;
use super::infer::InferCtx;
use super::instantiate::Instantiation;
use super::scope::{BinderInScope, Scope};
use super::sig::MethodSig;
use super::trait_env;
use super::trait_env::ImplTargetKey;
use super::types::{FunctionContext, TypeError};
use super::tysys::TypeSystem;

/// The parameter an associated-type equality binds: a bare parameter
/// (`Builder<Output = T>`) or a pack spelt as the whole tuple
/// (`ReflectFlags<Members = [..M]>`). A pack binds to the projected tuple,
/// which is what the pack stands for everywhere else.
fn assoc_bound_target_param(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(named) => Some(&named.name),
        Type::Tuple(elems) => match elems.as_slice() {
            [Type::TypePackSpread(name, _)] => Some(name),
            _ => None,
        },
        _ => None,
    }
}

/// Per-position `_` mask for a turbofish: `holes[i]` is true when argument `i`
/// was written `_`. Slots past the end count as holes too (omitted trailing
/// args), so the mask need only cover the supplied args.
pub(super) fn turbofish_holes(ast_args: &[Type]) -> Vec<bool> {
    ast_args
        .iter()
        .map(|t| matches!(t, Type::Infer(_)))
        .collect()
}

/// Whether slot `i` is an inference hole given the `_` mask: an explicit `_`
/// (`holes[i]`) or a slot omitted past the end of the turbofish.
fn is_turbofish_hole(holes: &[bool], i: usize) -> bool {
    holes.get(i).copied().unwrap_or(true)
}

/// Whether a turbofish carries an explicit `_` placeholder. Non-allocating, so
/// callers gate the (allocating) [`turbofish_holes`] mask on it.
pub(super) fn turbofish_has_hole(ast_args: &[Type]) -> bool {
    ast_args.iter().any(|t| matches!(t, Type::Infer(_)))
}

/// True when a turbofish needs inference to fill some type-argument slot: it
/// supplies fewer args than the generic has parameters (omitted trailing args)
/// or it contains an explicit `_` placeholder.
pub(super) fn turbofish_needs_inference(ast_args: &[Type], param_count: usize) -> bool {
    ast_args.len() < param_count || turbofish_has_hole(ast_args)
}

/// Merge inferred type args into the explicitly-resolved ones in place. A slot
/// takes the inferred value when it was written `_` or omitted past the end of
/// the turbofish; every other slot keeps its explicit type, so the explicit
/// (non-`_`) args always win. `inferred` is a full param-length vec (unbound
/// params stay as `TypeParam`); an empty `inferred` (inference found nothing)
/// leaves the explicit args untouched.
pub(super) fn merge_turbofish_type_args(
    explicit: &mut Vec<TypeId>,
    holes: &[bool],
    inferred: &[TypeId],
) {
    for (i, &filled) in inferred.iter().enumerate() {
        if !is_turbofish_hole(holes, i) {
            continue;
        }
        if i < explicit.len() {
            explicit[i] = filled;
        } else {
            explicit.push(filled);
        }
    }
}

/// View of a `ResolvedType::Function` after peeling references and
/// fn-type newtypes. Returned by [`Elaborator::as_fn_signature`].
pub(super) struct FnSignature {
    is_mut: bool,
    params: Vec<TypeId>,
    pub(super) return_type: TypeId,
}

/// Classification of an `Ident`-form call callee, with any `Self::` / `T::`
/// prefix already substituted, so `resolve_call` can look up parameter types and
/// resolve arguments once with the right expected-type hints. Re-entering
/// `resolve_call` with a synthetic `CallExpr` instead fired the assert-capture
/// hook twice per sub-expression, emitting each `let __vK = …` binding twice.
enum CalleeIdentKind<'a> {
    /// No prefix substitution needed. Covers plain ident calls
    /// (`foo(x)`), already-concrete qualified calls (`Type::method(x)`,
    /// `Effect::op(x)`, `builtin::f(x)`), and so on. Borrows
    /// `ident.name` for `effective_name`.
    AsIs(&'a ast::IdentExpr),
    /// `Self::suffix(...)` or `T::suffix(...)` where `T` is a type
    /// parameter currently bound to a concrete type. Holds the fully
    /// resolved `Concrete::suffix` name.
    Rewritten(String),
    /// A bare case call (`Some(x)`), the expected type having supplied the
    /// case. `owner` is the variant declaring it and `spelled` its
    /// `Variant::Case` form, so the qualified constructor path serves it.
    Case {
        owner: crate::defs::DefId,
        spelled: String,
    },
    /// `T::suffix(...)` where `T` is still an abstract type parameter
    /// constrained only by trait bounds. Dispatched independently via
    /// `resolve_type_param_static_call`.
    AbstractTypeParam {
        prefix: String,
        suffix: String,
        type_param_type_id: TypeId,
    },
}

impl CalleeIdentKind<'_> {
    /// The effective callee name used by `lookup_function_signature`
    /// and the dispatch match. Not callable on `AbstractTypeParam`
    /// because that variant takes its own dispatch path before any name
    /// lookup happens.
    fn effective_name(&self) -> &str {
        match self {
            Self::AsIs(ident) => &ident.name,
            Self::Rewritten(name) | Self::Case { spelled: name, .. } => name,
            Self::AbstractTypeParam { .. } => {
                unreachable!("AbstractTypeParam takes the type-param dispatch path")
            }
        }
    }

    /// The variant a bare case call constructs; `None` for every other shape,
    /// whose receiver is read from its own segment.
    fn case_owner(&self) -> Option<crate::defs::DefId> {
        match self {
            Self::Case { owner, .. } => Some(*owner),
            Self::AsIs(_) | Self::Rewritten(_) | Self::AbstractTypeParam { .. } => None,
        }
    }

    /// The reference site of the callee itself, which says which declaration a
    /// bare `name(…)` means. `Rewritten` is synthesised from an already-resolved
    /// `Self::` / `T::` prefix, so no walk saw it.
    fn callee_site(&self) -> Option<ast::AstId> {
        match self {
            Self::AsIs(ident) => Some(ident.id),
            Self::Rewritten(_) | Self::Case { .. } | Self::AbstractTypeParam { .. } => None,
        }
    }

    /// The reference site of a qualified callee's receiver segment — the `Type` of
    /// `Type::method`, which the walk answered for in the module that wrote it.
    ///
    /// Two segments exactly: consumers pair this with the prefix they split off
    /// `effective_name`, and only here are the two the same segment. A namespace
    /// prefix, an unqualified call and `Rewritten` all answer `None`.
    fn receiver_site(&self) -> Option<ast::AstId> {
        match self {
            Self::AsIs(ident) => match ident.segments.as_slice() {
                [receiver, _method] => Some(receiver.id),
                _ => None,
            },
            _ => None,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// If `type_id` is a function type — possibly behind references or
    /// fn-type newtypes such as `type Handler = fn(...);` — return its
    /// signature. Otherwise return `None`. Borrows the type table once.
    pub(super) fn as_fn_signature(&self, type_id: TypeId) -> Option<FnSignature> {
        let table = self.tysys.type_table.borrow();
        let peeled_ref = table.peel_refs(type_id);
        let base = table.representation_head(peeled_ref);
        match table.get(base) {
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                ..
            } => Some(FnSignature {
                is_mut: *is_mut,
                params: params.clone(),
                return_type: *return_type,
            }),
            _ => None,
        }
    }

    /// The declared type of a *global* (current-module or imported) named
    /// `name`, if one exists. Lets a bare call resolve a global callee (a
    /// function-typed global becomes an indirect call; any other type gets a
    /// clear not-callable diagnostic instead of "unknown function").
    fn global_var_type(&self, name: &str) -> Option<TypeId> {
        self.sem
            .decls
            .lookup_global(name, &self.current_module_source)
            .map(|(_, _, ty, _)| ty)
    }

    /// Walk a callee expression down to its *root place* identifier so
    /// `(h.f)()`, `(a.b.c.f)()`, `arr[i]()`, and `(arr[i].f)()` all
    /// resolve to the underlying local binding (`h`, `a`, `arr`).
    /// Returns `None` for callees whose root is not a binding the
    /// function context can see — typically a temporary such as a call
    /// result or a literal. Mirrors `MutatedVarsCollector::root_ident_of_lvalue`.
    fn place_root_ident(callee: &Expr) -> Option<&ast::IdentExpr> {
        match callee {
            Expr::Ident(id) => Some(id),
            Expr::FieldAccess(fa) => Self::place_root_ident(&fa.expr),
            Expr::Index(idx) => Self::place_root_ident(&idx.expr),
            _ => None,
        }
    }

    /// Enforce Rust's `FnMut` rule: when the callee type is `fn mut`,
    /// the *root* of the place expression must be a mutable place.
    /// A no-op when `fn_is_mut` is false or the root is a temporary
    /// (no binding to check). A binding is a mutable place when it is
    /// declared `mut` *or* its type is `&mut T` — in the latter case
    /// the place is mutable through the reference even though the
    /// binding itself cannot be reassigned (matches Rust's rule for
    /// `&mut self`).
    fn check_fn_mut_root_mutability(
        &mut self,
        callee: &Expr,
        ctx: &FunctionContext,
        fn_is_mut: bool,
    ) {
        if !fn_is_mut {
            return;
        }
        let Some(root) = Self::place_root_ident(callee) else {
            return; // Temporary root — no binding to require `mut` on.
        };
        if root.name.contains("::") {
            return; // Qualified name (e.g. `Type::method`) — not a local.
        }
        let Some(local) = ctx.lookup(&root.name) else {
            return; // Not a local — must be a top-level fn or capture seen later.
        };
        if local.is_mut {
            return;
        }
        let is_mut_ref = matches!(
            self.tysys.type_table.borrow().get(local.type_id),
            ResolvedType::MutRef(_)
        );
        if is_mut_ref {
            return;
        }
        let _ = self.emit(TypeError::ClosureMutBindingRequired {
            name: root.name.clone(),
            span: root.span,
        });
    }
}

impl TypeSystem {
    /// Classify a call callee's prefix so `resolve_call` can rewrite
    /// `Self::` / `T::` (T bound to concrete) before any name lookup
    /// happens. `T::` (T abstract) is routed through its own static
    /// dispatch path; everything else is left as written.
    fn classify_call_callee<'a>(
        &self,
        ctx: &Scope,
        ident: &'a ast::IdentExpr,
    ) -> CalleeIdentKind<'a> {
        let Some(pos) = ident.name.find("::") else {
            return CalleeIdentKind::AsIs(ident);
        };
        let prefix = &ident.name[..pos];
        let suffix = &ident.name[pos + 2..];

        if prefix == "Self"
            && let Some(self_type_id) = ctx.trait_ctx.self_type
        {
            let self_name = self.type_table.borrow().type_name(self_type_id);
            // `Self` names the receiver, not one of its bounds: an impl
            // *provides* the trait whose default body this is.
            return CalleeIdentKind::Rewritten(format!("{self_name}::{suffix}"));
        }

        if let Some(&BinderInScope { type_id, .. }) = ctx.trait_ctx.type_params.get(prefix) {
            return self.callee_ident_for_type_param(prefix, suffix, type_id);
        }

        CalleeIdentKind::AsIs(ident)
    }

    /// A `Param::method()` call, where `Param` is the type parameter bound to
    /// `type_id`. Reached for `Self` too: a blanket's `Self` *is* its receiver.
    fn callee_ident_for_type_param<'a>(
        &self,
        prefix: &str,
        suffix: &str,
        type_id: TypeId,
    ) -> CalleeIdentKind<'a> {
        // A parameter bound to a concrete type dispatches statically on it. An
        // abstract one keeps its form and routes through trait-bound dispatch,
        // for the monomorphizer to substitute later.
        let is_abstract = matches!(
            self.type_table.borrow().get(type_id),
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
        );
        if is_abstract {
            return CalleeIdentKind::AbstractTypeParam {
                prefix: prefix.to_string(),
                suffix: suffix.to_string(),
                type_param_type_id: type_id,
            };
        }
        // Consumed as a declaration name: `is_static_method` and
        // `locate_static_method_impl` key on what an `impl` header
        // writes, which carries no module.
        let concrete_name = self.type_table.borrow().fq_type_name(type_id).to_display();
        CalleeIdentKind::Rewritten(format!("{concrete_name}::{suffix}"))
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Whether `name` is a declared effect (`interface`) or resource —
    /// the set of identifiers `resolve_call`'s qualified-call fallback may
    /// treat as a deferred effect operation (`Stdout::write()`, etc.).
    fn is_effect_or_resource_decl(&self, def: crate::defs::DefId) -> bool {
        self.tysys.trait_env.effect_decl_index.contains(&def)
            || self.tysys.trait_env.resource_decl_index.contains(&def)
    }

    /// The effect / resource declaration a qualified callee's receiver segment
    /// names — answered by the site the walk resolved, so an import alias needs
    /// no translation back into a spelling.
    fn effect_or_resource_decl_at(&self, site: Option<ast::AstId>) -> Option<crate::defs::DefId> {
        let def = self.tysys.resolutions.declared(site?)?;
        self.is_effect_or_resource_decl(def).then_some(def)
    }

    /// The variant a `Variant::Case(...)` callee constructs: the one the walk
    /// answered for a bare case, else the one `prefix` names at its site.
    fn variant_of_callee(
        &self,
        callee_kind: &CalleeIdentKind<'_>,
        receiver_site: Option<ast::AstId>,
        prefix: &str,
    ) -> Option<&super::types::VariantInfo> {
        match callee_kind.case_owner() {
            Some(owner) => self.type_lookup().variant_cases_of(owner),
            None => self.lookup_variant_cases_at(receiver_site, prefix),
        }
    }

    pub(super) fn resolve_call(
        &mut self,
        call: &ast::CallExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Closure call: a bare identifier that names a *value* binding (a
        // local/param — checked first, so shadowing wins — or a module/imported
        // global) is invoked on its value, not looked up as a named function.
        if let Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
        {
            let local = ctx
                .lookup(&ident.name)
                .map(|local| (local.type_id, local.defining_ast_id));
            let value_ty = local
                .map(|(ty, _)| ty)
                .or_else(|| self.global_var_type(&ident.name));
            if let Some(value_ty) = value_ty {
                // Record the use→def edge the same way `resolve_ident` would,
                // so navigation on a value-binding callee (local or global)
                // still resolves — the fast path bypasses `resolve_ident`.
                match local {
                    Some((_, defining_ast_id)) => {
                        self.record_reference_opt(ident.id, defining_ast_id);
                    }
                    None => self.record_item_reference_by_name(ident.id, &ident.name),
                }

                if let Some(sig) = self.as_fn_signature(value_ty) {
                    // `fn mut` closures need a `mut` root binding — mirrors
                    // Rust's FnMut rule. The check goes through the same helper
                    // used by the indirect-call path below so identifier and
                    // non-identifier callees share one code path.
                    self.check_fn_mut_root_mutability(&call.callee, ctx, sig.is_mut);

                    // Closure `let`-site defaults can pad missing trailing args
                    // only for local callees.
                    return self.build_indirect_call(
                        call,
                        ctx,
                        &sig.params,
                        sig.return_type,
                        /* pad_with_defaults */ local.is_some(),
                    );
                }

                // Names a binding that is not a function — a clear
                // not-callable diagnostic, not the misleading "unknown
                // function 'x'" from the named-function lookup below.
                let type_name = self.tysys.type_table.borrow().type_name(value_ty);
                let _ = self.emit(TypeError::CalleeNotCallable {
                    type_name,
                    span: call.callee.span(),
                });
                // Still resolve the arguments so errors inside them are
                // reported rather than masked by the callee error.
                for arg in &call.args {
                    self.resolve_expr(arg, ctx, None);
                }
                return TypeTable::ERROR;
            }
        }

        // Indirect call on a non-identifier callee. Any expression whose
        // value type is a function type can be invoked here — e.g.
        // `arr[i](x)`, `(foo.bar)(x)`, `(get_fn())(x)`, `(|x| x)(1)`.
        // Identifier callees are handled separately above (locals with
        // fn type) and below (named functions, static methods, variant
        // constructors, ...). Method-call syntax `foo.bar()` is parsed as
        // `ast::Expr::MethodCall`, not `Call { callee: FieldAccess }`, so this
        // branch never alters method dispatch (Rust policy).
        if !matches!(&call.callee, Expr::Ident(_)) {
            let callee_type = self.resolve_expr(&call.callee, ctx, None);

            if let Some(sig) = self.as_fn_signature(callee_type) {
                // Enforce `fn mut` root-mutability for non-identifier
                // callees too: `(h.f)()`, `arr[i]()`, `(arr[i].f)()`, …
                // require the root binding to be `mut`. A temporary root
                // (call result, literal, …) has no binding and is OK.
                self.check_fn_mut_root_mutability(&call.callee, ctx, sig.is_mut);
                return self.build_indirect_call(
                    call,
                    ctx,
                    &sig.params,
                    sig.return_type,
                    /* pad_with_defaults */ false,
                );
            }

            // The callee resolved successfully but its type is not a
            // function. Emit a clear diagnostic instead of falling through
            // to the named-function lookup, which would surface a
            // confusing "unknown function" error. Suppress the message
            // when the callee already resolved to the error type so we
            // don't pile a second diagnostic on top of the first.
            if callee_type != TypeTable::ERROR {
                let type_name = self.tysys.type_table.borrow().type_name(callee_type);
                let _ = self.emit(TypeError::CalleeNotCallable {
                    type_name,
                    span: call.callee.span(),
                });
            }
            return TypeTable::ERROR;
        }

        // After the closure-call and non-Ident-callee fast paths above
        // the callee is necessarily an `Ident`. Resolve any `Self::` /
        // `T::` prefix BEFORE looking up parameter types so the single
        // argument-resolution pass below sees the correct expected-type
        // hints.
        let Expr::Ident(ident) = &call.callee else {
            unreachable!("non-Ident callees are handled by the indirect-call fast path above")
        };
        let mut callee_kind = self.tysys.classify_call_callee(&self.annotate_ctx, ident);
        // A bare case constructs (`Some(x)`) only where the expected type
        // supplies it, and then ahead of any function of that name.
        if let CalleeIdentKind::AsIs(bare) = callee_kind
            && !bare.name.contains("::")
        {
            match self.bare_case(bare, expected_type) {
                BareCase::Of { owner, spelled } => {
                    callee_kind = CalleeIdentKind::Case { owner, spelled };
                }
                BareCase::NeedsContext => return TypeTable::ERROR,
                BareCase::None => {}
            }
        }

        // `Trait::method(recv, args…)` — the trait-qualified (UFCS) call form
        // (WEP 2026-07-31). Routed before the argument walk below because the
        // dispatcher elaborates the non-receiver arguments itself, against the
        // signature it selects.
        if let Some(pos) = ident.name.find("::")
            && self.is_trait_instance_method(&ident.name[..pos], &ident.name[pos + 2..])
        {
            let (trait_name, method_name) = (
                ident.name[..pos].to_string(),
                ident.name[pos + 2..].to_string(),
            );
            // The path's leading segment is the trait's reference site.
            let head_site = ident.segments.first().map(|seg| seg.id);
            return self.resolve_trait_qualified_call(
                head_site,
                &trait_name,
                &method_name,
                call,
                expected_type,
                ctx,
            );
        }

        // Abstract `T::method(...)` takes its own dispatch path. Args
        // are resolved without coercion hints (the trait-bound dispatch
        // walks them on its own; threading hints through here is a
        // separate concern from the synthetic-AST removal).
        if let CalleeIdentKind::AbstractTypeParam {
            prefix,
            suffix,
            type_param_type_id,
            ..
        } = &callee_kind
        {
            let args: Vec<TypeId> = call
                .args
                .iter()
                .map(|a| self.resolve_expr(a, ctx, None))
                .collect();
            return self.resolve_type_param_static_call(
                prefix,
                suffix,
                *type_param_type_id,
                &args,
                call,
                ctx,
            );
        }

        let effective_name = callee_kind.effective_name();
        // The receiver of `Type::method` is named at its own segment, and the
        // walk answered for it. Every receiver lookup below goes through that
        // site, so the spelling is never split back into an identity.
        let receiver_site = callee_kind.receiver_site();
        // A case is reachable wherever its type is; only a static method has
        // a visibility of its own to check.
        if callee_kind.case_owner().is_none()
            && let Some((struct_name, _)) = effective_name.rsplit_once("::")
        {
            let receiver = self.impl_target_at(receiver_site, struct_name);
            self.check_static_call_visibility(&receiver, effective_name, Some(call.id), call.span);
        }

        // First, determine expected parameter types to handle coercion.
        let signature = self.lookup_function_signature(
            effective_name,
            receiver_site,
            callee_kind.callee_site(),
        );
        let signature_known = signature.is_some();
        let (mut param_types, callee_slots) = signature.unwrap_or_default();
        // The declaration's own frame, before instantiation replaces its slots
        // with inference variables. Inferred type arguments substitute into
        // these, not into the variables.
        let declared_param_types = param_types.clone();

        // Instantiate the callee's slots before an argument is resolved
        // against one of its parameter types. A rigid slot is the callee's
        // own and opaque here, so a literal checked against `List<T>` reports
        // every element as heterogeneous; against `List<?0>` the check defers
        // and the elements decide what `?0` is.
        let arg_inst = (!callee_slots.is_empty()).then(|| {
            self.instantiate(
                &callee_slots,
                &Instantiation {
                    kind: "function",
                    name: effective_name,
                    span: call.span,
                },
            )
        });
        if let Some(inst) = &arg_inst {
            param_types = self.instantiate_types(&param_types, inst);
        }

        // Literal preselect for a conversion call (WEP 2026-07-31 phase 4):
        // see `resolve_static_method_call` — same rule, for the
        // `Wrapper::from(42)` spelling that arrives as a plain call.
        if let Some(pos) = effective_name.find("::")
            && call.args.len() == 1
        {
            let (recv_name, method_name) = (
                effective_name[..pos].to_string(),
                effective_name[pos + 2..].to_string(),
            );
            if self.try_conversion_preselect(
                &recv_name,
                &method_name,
                &call.args[0],
                call.span,
                ctx,
                &mut param_types,
                None,
            ) {
                return TypeTable::ERROR;
            }
        }

        // Whether `param_types` holds a variant payload rather than declared
        // function params (see the hole-pin loop below).
        let mut is_variant_payload = false;

        // For variant constructors with type args (e.g., Option::<List<u8>>::Some([])),
        // compute substituted payload type so literal coercion works on first resolve.
        if param_types.is_empty()
            && let Some(pos) = effective_name.find("::")
        {
            let prefix = &effective_name[..pos];
            let suffix = &effective_name[pos + 2..];
            if let Some(variant_info) = self
                .variant_of_callee(&callee_kind, receiver_site, prefix)
                .cloned()
                && let Some((_, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == suffix)
            {
                let payload_is_unit = matches!(
                    self.tysys.type_table.borrow().get(case_data.payload),
                    ResolvedType::Unit
                );
                if !payload_is_unit {
                    let variant_type_args: Vec<TypeId> = call
                        .type_args
                        .iter()
                        .map(|ty| self.resolve_type(ty))
                        .collect();
                    let mut payload_type = case_data.payload;
                    if !variant_type_args.is_empty() {
                        payload_type = self
                            .tysys
                            .substitute_type_params(payload_type, &variant_type_args);
                    } else if let Some(expected) = expected_type {
                        // Infer type args from expected type (e.g. Option::Some(null) expecting Option<Option<i32>>)
                        let expected_resolved =
                            self.tysys.type_table.borrow().get(expected).clone();
                        if let ResolvedType::GenericInstance {
                            def: expected_def,
                            type_args: expected_args,
                        } = expected_resolved
                            && Some(expected_def)
                                == self
                                    .tysys
                                    .resolutions
                                    .defs()
                                    .of_ast_id(variant_info.defined_at)
                            && expected_args.len() == variant_info.type_param_type_ids.len()
                        {
                            payload_type = self
                                .tysys
                                .substitute_type_params(payload_type, &expected_args);
                        }
                    }
                    param_types.push(payload_type);
                    is_variant_payload = true;
                }
            }
        }

        // Resolve arguments with coercion awareness
        let mut args: Vec<TypeId> = call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = param_types.get(i).copied();
                self.resolve_expr(arg, ctx, expected_type)
            })
            .collect();

        // Settle this resolution's variables. `solve_infer_var` keeps the
        // first answer, so a slot the arguments pinned stays pinned and one
        // they left open goes back to the declaration's parameter — leaving
        // inference exactly what it saw before this step existed.
        if let Some(inst) = &arg_inst {
            let pairs: Vec<(TypeId, TypeId)> = inst
                .vars
                .iter()
                .copied()
                .zip(callee_slots.iter().copied())
                .collect();
            for (var, slot) in pairs {
                self.solve_infer_var(var, slot);
            }
            for arg in &mut args {
                *arg = self.apply_infer_holes(*arg);
            }
        }

        // Pin a deferred hole carried into a variant payload (`Result::Ok(v)`,
        // `v = gen()?`) against the payload type. Regular call arguments are
        // pinned post-inference below; this loop runs pre-inference, so it is
        // scoped to variant payloads to avoid touching them.
        if is_variant_payload {
            for (i, arg) in args.iter_mut().enumerate() {
                if let Some(&expected) = param_types.get(i)
                    && self.type_has_infer_hole(*arg)
                    && self.hole_pinnable_against(expected)
                {
                    self.solve_infer_holes_against(*arg, expected);
                    *arg = self.apply_infer_holes(*arg);
                }
            }
        }

        // Resolve the callee's identity. `Some(CalleeRef)` means we know
        // both the defining module and the name-as-defined; `None` means the
        // call target could not be resolved and we'll emit `UnknownFunction`.
        // `display_name` is always populated for diagnostics. The match
        // dispatches on `effective_name` (after any `Self::` / `T::`
        // prefix rewriting) while `ident` is kept around for LSP
        // segment-edge recording and other AST-id needs.
        let (callee_opt, display_name): (Option<CalleeRef>, String) = if let Some(pos) =
            effective_name.find("::")
        {
            let prefix = &effective_name[..pos];
            let suffix = &effective_name[pos + 2..];

            // Builtin functions: resolve through core:builtin module. The
            // prefix names the module directly rather than an import, but it
            // buys no extra reach — a declaration there is visible exactly as
            // any other module's is.
            if prefix == "builtin" {
                let builtin_source = ModuleSource::builtin();
                self.check_namespaced_visibility(&builtin_source, suffix, ident.span);
                (
                    self.callee_in_module(&builtin_source, suffix),
                    effective_name.to_string(),
                )
            }
            // Static method call (Type::method). Static methods are
            // registered with mangled names "Type::method".
            else if self.is_static_method_at(receiver_site, prefix, suffix) {
                // Record the receiver-type segment (prefix) as a reference to
                // the type's decl. After `Self::` / `T::` rewriting, `prefix`
                // is the concrete type name and the segment's AstId is the
                // `Self` / `T` token — the edge correctly resolves clicks on
                // `Self` to the concrete type's decl.
                if let Some(prefix_seg) = ident.segments.first() {
                    self.record_item_reference_by_name(prefix_seg.id, prefix);
                }
                // Record the method segment (suffix) as a reference to the
                // declaration this call resolves to. The impl selection knows
                // which one answered — two conversion impls on a type declare
                // the same `from`, and only the argument's type separates
                // them. It covers trait impls only; an inherent static has no
                // selection and reaches the index instead.
                if let Some(suffix_seg) = ident.segments.get(1) {
                    let arg_hint = if (suffix == "from" || suffix == "try_from") && args.len() == 1
                    {
                        Some(self.tysys.type_table.borrow().type_name(args[0]))
                    } else {
                        None
                    };
                    let method_def = self
                        .locate_static_method_impl(prefix, suffix, arg_hint.as_deref(), None)
                        .and_then(|r| r.method_id)
                        .or_else(|| self.qualified_method_decl_at(receiver_site, prefix, suffix));
                    if let Some(method_def) = method_def {
                        self.record_reference_to_decl(suffix_seg.id, method_def);
                    }
                }
                // Resolve method-level type args (e.g., i32::deserialize::<MockDeserializer>)
                let mut method_type_args: Vec<TypeId> = call
                    .type_args
                    .iter()
                    .map(|ty| self.resolve_type(ty))
                    .collect();
                // Impl-level type args inferred from the LHS / receiver type.
                // Only populated by `infer_static_method_type_args`; the
                // explicit `call.type_args` only carries method-level args.
                let mut impl_type_args_inferred: Vec<TypeId> = Vec::new();
                // Omitted turbofish infers both levels; an explicit `_` fills
                // only the hole slots (see `infer_static_call_type_args`).
                if method_type_args.is_empty() {
                    let (impl_args, method_args) = self.infer_static_call_type_args(
                        prefix,
                        suffix,
                        &call.args,
                        &args,
                        expected_type,
                        call.span,
                    );
                    impl_type_args_inferred = impl_args;
                    method_type_args = method_args;
                } else if turbofish_has_hole(&call.type_args) {
                    // Partial method-level turbofish (`Type::m::<_, U>(..)`):
                    // fill the `_` slots from inference, explicit args stay put.
                    let (impl_args, method_args) = self.infer_static_call_type_args(
                        prefix,
                        suffix,
                        &call.args,
                        &args,
                        expected_type,
                        call.span,
                    );
                    if impl_type_args_inferred.is_empty() {
                        impl_type_args_inferred = impl_args;
                    }
                    let holes = turbofish_holes(&call.type_args);
                    merge_turbofish_type_args(&mut method_type_args, &holes, &method_args);
                }
                // Record `[impl_args, method_args]` — the same order
                // `lookup_static_method_param_types` substitutes in — since reify
                // needs both halves to rebuild the mangled `__<Type>__<method>`.
                // The instance type is UNKNOWN: a static call anchors no
                // `GenericInstance`, so reify reads `expression_types` instead.
                {
                    let mut combined = impl_type_args_inferred.clone();
                    combined.extend_from_slice(&method_type_args);
                    self.record_generic_instantiation(call.id, combined, TypeTable::UNKNOWN);
                }
                self.report_uninferred_static_method_type_args(
                    prefix,
                    suffix,
                    &impl_type_args_inferred,
                    &method_type_args,
                    call.span,
                );
                // Enforce the static method's type-arg bounds (shared rule).
                if !method_type_args.is_empty() {
                    let mtype_params = self.lookup_static_method_type_params(prefix, suffix);
                    self.enforce_type_arg_bounds(&mtype_params, &method_type_args, call.span);
                }
                // Handle From conversions with no explicit impl: reflexive and newtype.
                if suffix == "from" && args.len() == 1 {
                    let arg_type = args[0];
                    let arg_type_name = self.tysys.type_table.borrow().type_name(arg_type);

                    // Reflexive `T::from(T_val)`: the outer Call evaporates, so
                    // tag it `NewtypeFromCollapse` or reify emits a `Call` the
                    // elaborator never built. Matched by canonical decl identity,
                    // since two modules' `Instant` share a bare name. Generic
                    // instances compare by name: a decl key drops type args.
                    let arg_is_generic = {
                        let tt = self.tysys.type_table.borrow();
                        matches!(
                            tt.get(tt.peel_refs(arg_type)),
                            crate::tir::ResolvedType::GenericInstance { .. }
                                | crate::tir::ResolvedType::GenericResource { .. }
                        )
                    };
                    let is_reflexive = if arg_is_generic {
                        arg_type_name == prefix
                    } else if let Some(arg_key) = self.type_decl_key(arg_type) {
                        Some(arg_key) == self.decl_key_or_local(prefix)
                    } else {
                        arg_type_name == prefix
                    };
                    if is_reflexive {
                        self.record_desugar(
                            call.id,
                            super::sem::types::DesugarKind::NewtypeFromCollapse,
                        );
                        return args[0];
                    }

                    // Newtype→Base: u64::from(UserId_val) where type UserId = u64
                    let base_of_arg = self.tysys.type_table.borrow().get_newtype_base(arg_type);
                    if let Some(base_id) = base_of_arg
                        && self.tysys.type_table.borrow().type_name(base_id) == prefix
                    {
                        self.record_desugar(
                            call.id,
                            super::sem::types::DesugarKind::NewtypeFromUnwrap,
                        );
                        // Reify rebuilds the newtype `Cast` from the
                        // recorded `DesugarKind`; project only the result type.
                        return base_id;
                    }

                    // Base→Newtype: UserId::from(u64_val) where type UserId = u64
                    if let Some(newtype_type_id) = self.lookup_newtype(prefix) {
                        let base_opt = self
                            .tysys
                            .type_table
                            .borrow()
                            .get_newtype_base(newtype_type_id);
                        if let Some(base_id) = base_opt
                            && self.tysys.type_table.borrow().type_name(base_id) == arg_type_name
                        {
                            self.record_desugar(
                                call.id,
                                super::sem::types::DesugarKind::NewtypeFromWrap,
                            );
                            // Reify rebuilds the newtype `Cast` from
                            // the recorded `DesugarKind`; project only the type.
                            return newtype_type_id;
                        }
                    }
                }

                // Literal args resolved against `TypeParam`/`Unknown` fell back to
                // i32/f64, so re-coerce once the substitution is known. A
                // non-generic call is checked too, or a mismatch only shows at
                // codegen, as an invalid module rather than at its own span.
                let raw_param_types = self
                    .qualified_call_param_types(prefix, suffix)
                    .unwrap_or_default();
                let substituted: Vec<TypeId> =
                    if method_type_args.is_empty() && impl_type_args_inferred.is_empty() {
                        raw_param_types
                    } else {
                        let mut combined_type_args = impl_type_args_inferred.clone();
                        combined_type_args.extend_from_slice(&method_type_args);
                        raw_param_types
                            .iter()
                            .map(|&t| self.tysys.substitute_type_params(t, &combined_type_args))
                            .collect()
                    };
                self.recoerce_literal_args(&call.args, &mut args, &substituted);
                // Per-argument checking alone passes a call of the wrong length:
                // the loop below reaches neither a missing argument nor a
                // surplus one, and the call reaches codegen as an invalid
                // module. `Self::arg_count_fits` is the same rule the other
                // static spellings check.
                //
                // From the same lookup `raw_param_types` came from, so the two
                // cannot disagree: an overloaded name yields no signature here,
                // and so no count to check — the overload path picks the impl
                // by argument, and reports its own mismatch.
                let optional = self
                    .unique_qualified_method_sig(prefix, suffix)
                    .map(|sig| sig.params.iter().filter(|p| p.default.is_some()).count());
                if let Some(optional) = optional
                    && !Self::arg_count_fits(args.len(), substituted.len(), optional)
                {
                    let _ = self.emit(TypeError::ArgumentCountMismatch {
                        expected: substituted.len(),
                        found: args.len(),
                        span: call.span,
                    });
                    return TypeTable::ERROR;
                }
                for (i, arg) in args.iter().enumerate() {
                    if let Some(&expected) = substituted.get(i) {
                        self.typecheck(
                            *arg,
                            expected,
                            call.args.get(i).map_or(call.span, ast::Expr::span),
                        );
                    }
                }

                // WEP 2026-05-26: `resolve_static_method_call_from_qualified`
                // records the resolved `FunctionRef` under `call.id` itself
                // (`static_method_dispatch`) so reify can reproduce the same
                // `Call` shape without re-running impl lookup, mangled-name
                // construction, or monomorph-info shaping — facts reify
                // cannot reconstruct from the AST alone. It returns only a
                // typed placeholder.
                return self.resolve_static_method_call_from_qualified(
                    prefix,
                    suffix,
                    &args,
                    &impl_type_args_inferred,
                    &method_type_args,
                    call.id,
                    call.span,
                    ctx,
                );
            }
            // Check if this is a flags type method call: Perms::none(), Perms::all()
            // A newtype reaches its base's constants and keeps its own type:
            // `M::none()` on `type M = Perm` is `Perm::none() as M`.
            else if let Some((flags_info, named)) =
                self.flags_members_through_newtype(receiver_site, prefix)
                && matches!(suffix, "none" | "all")
            {
                if let Some(prefix_seg) = ident.segments.first() {
                    self.record_item_reference_by_name(prefix_seg.id, prefix);
                }
                // Reify rebuilds the flags `none()` / `all()`
                // constant from the AST + flags info; the body walk
                // projects only the result type.
                return named.unwrap_or(flags_info.type_id);
            }
            // Check if this is a variant case construction (Color::Red)
            else if let Some(variant_info) =
                self.variant_of_callee(&callee_kind, receiver_site, prefix)
            {
                // Clone needed data to release the borrow on self
                let variant_info = variant_info.clone();
                let case_match = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == suffix)
                    .map(|(i, c)| (i, c.clone()));
                let prefix_owned = prefix.to_string();

                // Find the case by name
                if let Some((_case_index, case_data)) = case_match {
                    self.record_qualified_case(ident, &prefix_owned, case_data.ast_id);
                    // Each variant case has exactly one payload.
                    // Unit variants expect 0 args, non-unit variants expect 1 arg.
                    let payload_is_unit = matches!(
                        self.tysys.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        let _ = self.emit(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: call.span,
                        });
                        return TypeTable::ERROR;
                    }

                    let payload = args.into_iter().next();

                    // Infer variant type: use GenericInstance for generic variants
                    let variant_type = if variant_info.type_params.is_empty() {
                        self.tysys
                            .type_table
                            .borrow()
                            .type_id_of_decl(variant_info.defined_at)
                    } else {
                        {
                            let inferred = self.tysys.infer_variant_type_args(
                                &self.annotate_ctx,
                                &variant_info,
                                &case_data,
                                payload,
                                expected_type,
                                &[],
                                &[],
                            );
                            self.defer_uninferable_variant(
                                inferred,
                                &prefix_owned,
                                &variant_info,
                                call.span,
                            )
                        }
                    };

                    // WEP 2026-05-26: record generic
                    // type args for variant constructors. Non-generic
                    // variants emit a `Variant` (no type_args) and the
                    // recording is skipped via the empty-`type_args`
                    // guard inside `record_generic_instantiation`.
                    let type_args = match self.tysys.type_table.borrow().get(variant_type) {
                        ResolvedType::GenericInstance { type_args, .. } => type_args.clone(),
                        _ => Vec::new(),
                    };
                    self.record_generic_instantiation(call.id, type_args, variant_type);

                    return variant_type;
                }
                // If no matching case, check for From<T> synthesis requests
                else if suffix == "from" && args.len() == 1 {
                    let target_type_id = self
                        .tysys
                        .type_table
                        .borrow()
                        .type_id_of_decl(variant_info.defined_at);
                    let from_type = args[0];
                    let from_type_name = self.tysys.type_table.borrow().type_name(from_type);
                    let from_trait_name = self
                        .tysys
                        .type_table
                        .borrow()
                        .compiler_trait_name(crate::compiler_item::CompilerItem::From)
                        .to_string();
                    // `impl From<X> for Prefix;` — a body-less derivation
                    // request. Both the flag and the trait reference are
                    // header facts, so the impls are reached by the target's
                    // canonical key rather than by scanning one module's AST
                    // for a matching written name.
                    let matching_impl = self
                        .tysys
                        .trait_env
                        .all_impl_keys(&self.impl_target(prefix))
                        .iter()
                        .filter_map(|key| self.tysys.trait_env.impl_headers.get(key))
                        .any(|header| {
                            header.is_synthesize_request
                                && header.trait_name.as_deref() == Some(from_trait_name.as_str())
                                && matches!(&header.trait_type, Some(ast::Type::Generic(generic))
                                    if generic.args.len() == 1
                                        && self.get_type_name_full(&generic.args[0])
                                            == from_type_name)
                        });
                    if matching_impl {
                        return self.resolve_from_call(target_type_id, from_type, call.id);
                    }
                    if let Some(return_type) = self.resolve_named_type_blanket_static(
                        prefix, suffix, call.id, &args, &call.args, call.span,
                    ) {
                        return return_type;
                    }
                    let _ = self.emit(TypeError::UnknownFunction {
                        name: format!("{prefix}::{suffix}"),
                        span: call.span,
                    });
                    return TypeTable::ERROR;
                } else {
                    if let Some(return_type) = self.resolve_named_type_blanket_static(
                        prefix, suffix, call.id, &args, &call.args, call.span,
                    ) {
                        return return_type;
                    }
                    let _ = self.emit(TypeError::UnknownFunction {
                        name: format!("{prefix}::{suffix}"),
                        span: call.span,
                    });
                    return TypeTable::ERROR;
                }
            }
            // If prefix is a known type (struct/enum/newtype/flags) with no matching
            // static method, emit a compile error.
            else if self.tysys.is_known_type_name(prefix) {
                if let Some(return_type) = self.resolve_named_type_blanket_static(
                    prefix, suffix, call.id, &args, &call.args, call.span,
                ) {
                    return return_type;
                }
                let _ = self.emit(TypeError::UnknownFunction {
                    name: format!("{prefix}::{suffix}"),
                    span: call.span,
                });
                return TypeTable::ERROR;
            }
            // Namespace import: `use ns from "..."` then `ns::Type::method()`
            // or `ns::VariantType::Case(...)`.
            else if let Some(ns_source) = self.sem.imports.namespace_imports.get(prefix).cloned()
            {
                // suffix may be "Type::method" or plain "func"
                if let Some(inner_pos) = suffix.find("::") {
                    let type_name = &suffix[..inner_pos];
                    let method_name = &suffix[inner_pos + 2..];

                    // Check if this is a variant construction in the namespace.
                    // `ns::Type::Case` names `Type` with its middle segment,
                    // which the resolve walk answered for under the `ns$Type`
                    // alias — so the declaration comes from the site rather
                    // than from asking the namespace module about a spelling.
                    let ns_variant = self
                        .qualified_owner_decl(ident)
                        .and_then(|def| self.tysys.all_variant_cases.get(&def))
                        .cloned();
                    if let Some(variant_info) = ns_variant {
                        let case_match = variant_info
                            .cases
                            .iter()
                            .enumerate()
                            .find(|(_, c)| c.name == method_name)
                            .map(|(i, c)| (i, c.clone()));
                        if let Some((_case_index, case_data)) = case_match {
                            self.record_namespaced_case(ident, case_data.ast_id);
                            let payload_is_unit = matches!(
                                self.tysys.type_table.borrow().get(case_data.payload),
                                ResolvedType::Unit
                            );
                            let expected_args = usize::from(!payload_is_unit);
                            if args.len() != expected_args {
                                let _ = self.emit(TypeError::ArgumentCountMismatch {
                                    expected: expected_args,
                                    found: args.len(),
                                    span: call.span,
                                });
                                return TypeTable::ERROR;
                            }
                            let payload = args.into_iter().next();
                            let variant_type = if variant_info.type_params.is_empty() {
                                self.tysys
                                    .type_table
                                    .borrow()
                                    .type_id_of_decl(variant_info.defined_at)
                            } else {
                                {
                                    let inferred = self.tysys.infer_variant_type_args(
                                        &self.annotate_ctx,
                                        &variant_info,
                                        &case_data,
                                        payload,
                                        expected_type,
                                        &[],
                                        &[],
                                    );
                                    self.defer_uninferable_variant(
                                        inferred,
                                        type_name,
                                        &variant_info,
                                        call.span,
                                    )
                                }
                            };

                            // Record generic type args for
                            // namespace-qualified variant ctors.
                            let type_args = match self.tysys.type_table.borrow().get(variant_type) {
                                ResolvedType::GenericInstance { type_args, .. } => {
                                    type_args.clone()
                                }
                                _ => Vec::new(),
                            };
                            self.record_generic_instantiation(call.id, type_args, variant_type);

                            return variant_type;
                        }
                    }

                    // Static method call on a type from the namespace module.
                    let method_type_args: Vec<TypeId> = call
                        .type_args
                        .iter()
                        .map(|ty| self.resolve_type(ty))
                        .collect();

                    // `ns::Type::method` never reaches the bare-spelling check,
                    // so the ladder is enforced here. The receiver is named at
                    // its own segment, which the walk answered for.
                    {
                        let receiver_site = ident
                            .segments
                            .len()
                            .checked_sub(2)
                            .map(|i| ident.segments[i].id);
                        let receiver = self.impl_target_at(receiver_site, type_name);
                        let qualified = format!("{type_name}::{method_name}");
                        self.check_static_call_visibility(
                            &receiver,
                            &qualified,
                            Some(call.id),
                            call.span,
                        );
                    }

                    // Find the impl module via the trait env (global index)
                    let arg_type_hint = if (method_name == "from" || method_name == "try_from")
                        && args.len() == 1
                    {
                        Some(self.tysys.type_table.borrow().type_name(args[0]))
                    } else {
                        None
                    };
                    let resolved = self.locate_static_method_impl(
                        type_name,
                        method_name,
                        arg_type_hint.as_deref(),
                        None,
                    );
                    let method_ref = resolved.unwrap_or_else(|| {
                        StaticMethodRef::new(ns_source.clone(), type_name, method_name, None, None)
                    });
                    let trait_name = method_ref.trait_name.clone();
                    let struct_module = method_ref.module.clone();

                    // The bare `Type::method` branch records this edge, but
                    // `is_static_method("ns", "Type::method")` declines, so a
                    // namespaced call lands here instead. `ident` is
                    // `ns::Type::method`, so the method is its third segment —
                    // the position `record_namespaced_case` also reads.
                    if let Some(method_seg) = ident.segments.get(2)
                        && let Some(method_def) = method_ref.method_id.or_else(|| {
                            // The receiver is `ns::Type`, whose middle segment
                            // the resolve walk answered for under the `ns$Type`
                            // alias. No spelling is re-resolved from the call
                            // site's frame, which declares its own `Type`.
                            let defs = self.tysys.resolutions.defs();
                            let receiver = trait_env::ImplTargetKey::of_decl(
                                defs,
                                self.qualified_owner_decl(ident)?,
                            );
                            self.qualified_method_decl_id(&receiver, method_name)
                        })
                    {
                        self.record_reference_to_decl(method_seg.id, method_def);
                    }

                    // Qualify by the module the impl was located in:
                    // `helper::Pair` and a local `Pair` are different
                    // declarations.
                    let receiver = self.namespace_member(prefix, type_name).map_or_else(
                        || crate::name::FqTypeName::shape(&struct_module, type_name),
                        |def| crate::name::FqTypeName::of_head(self.tysys.resolutions.defs(), def),
                    );
                    let final_mangled = MethodName::format_local(
                        &receiver,
                        method_ref.trait_name.as_ref(),
                        method_name,
                    );

                    let mut return_type = self.lookup_static_method_return_type(
                        &method_ref,
                        &receiver,
                        &final_mangled,
                    );
                    if !method_type_args.is_empty() {
                        return_type = self
                            .tysys
                            .substitute_type_params(return_type, &method_type_args);
                    }

                    let monomorph_info = if method_type_args.is_empty() {
                        None
                    } else {
                        Some(MonomorphInfo {
                            generic_name: final_mangled.clone(),
                            impl_type_args: vec![],
                            method_type_args: method_type_args.clone(),
                            is_blanket: false,
                        })
                    };

                    // The importing module never names `Type` on its own, so a
                    // bare-name key reaches nothing and every `mut` parameter
                    // would read as non-mut for the mutation and alias passes.
                    let ns_key = self.namespace_member(prefix, type_name).map(|def| {
                        trait_env::ImplTargetKey::of_decl(self.tysys.resolutions.defs(), def)
                    });
                    let param_is_mut = self.lookup_static_method_param_is_mut(
                        type_name,
                        method_name,
                        ns_key.as_ref(),
                    );

                    let func_ref = FunctionRef {
                        module_source: struct_module,
                        name: final_mangled,
                        monomorph_info,
                        // The receiver `final_mangled` was built from: DCE and
                        // monomorphization key on this, so a different one here
                        // names a different type than the call reaches.
                        method_info: Some(LocalMethodName::new(
                            receiver,
                            trait_name,
                            method_name.to_string(),
                        )),
                    };

                    // Recorded so reify replays this Call shape without re-running
                    // dispatch. Empty defaults would leave codegen a call short an
                    // argument; empty types would leave reify padding untyped.
                    let param_defaults = self.lookup_static_method_param_defaults(
                        type_name,
                        method_name,
                        ns_key.as_ref(),
                    );
                    let declared_params = self.lookup_static_method_param_types_keyed(
                        type_name,
                        method_name,
                        ns_key.as_ref(),
                    );
                    let declares_params = declared_params.is_some();
                    let param_types = declared_params.unwrap_or_default();
                    let checked: Vec<TypeId> = if method_type_args.is_empty() {
                        param_types.clone()
                    } else {
                        param_types
                            .iter()
                            .map(|&t| self.tysys.substitute_type_params(t, &method_type_args))
                            .collect()
                    };
                    self.recoerce_literal_args(&call.args, &mut args, &checked);
                    // The same check the bare `Type::method` spelling gets: a
                    // count is only skipped where no signature answered.
                    let arg_spans: Vec<crate::Span> =
                        call.args.iter().map(ast::Expr::span).collect();
                    if declares_params
                        && !self.check_static_call_args(
                            &checked,
                            &args,
                            &arg_spans,
                            &param_defaults,
                            call.span,
                        )
                    {
                        return TypeTable::ERROR;
                    }

                    let key = call.id;
                    self.sem.types.static_method_dispatch.insert(
                        key,
                        super::sem::types::StaticMethodDispatch {
                            method_def: method_ref.method_id,
                            function_ref: func_ref,
                            param_is_mut,
                            type_args: vec![],
                            param_defaults,
                            param_types,
                            self_in_args: false,
                        },
                    );

                    // Reify rebuilds the `Call` from the recorded
                    // `static_method_dispatch`; the body walk projects
                    // only the result type.
                    return return_type;
                }
                // `use`'s namespace form registers only the reachable members
                // as `ns$member`; this arm looks the module up directly, so it
                // owes the same visibility check — otherwise a path names what
                // an import of the identical symbol is refused.
                self.check_namespaced_visibility(&ns_source, suffix, ident.span);
                // `ns::func` — a plain free-function call through a namespace
                // import (the `suffix.find("::")` arm above always returns for
                // the `Type::method` / `Variant::Case` shapes). Record a use→def
                // edge to the target function in the namespace module so
                // liveness sees it reached and the Design-B effect checker sees
                // its declared effects. The whole-path `ident.id` is the key the
                // effect walker resolves free calls on (`check_effects_semantic`),
                // and the suffix segment id is the key LSP jump-to-def uses.
                let def_key = self
                    .symbols
                    .lookup_in_module(&ns_source, suffix)
                    .map(|sym| sym.defined_at);
                if let Some(def_key) = def_key {
                    self.record_reference_to_def(ident.id, def_key);
                    if let Some(seg) = ident.segments.get(1) {
                        self.record_reference_to_def(seg.id, def_key);
                    }
                }
                (
                    self.callee_in_module(&ns_source, suffix),
                    effective_name.to_string(),
                )
            }
            // Effect operations - pass through to codegen. This covers
            // `Stdout::write()`, etc. Effects/resources have no static-method
            // registration to check against above, so validate both halves of
            // the path against the declaration directly: `prefix` must name an
            // effect/resource and `suffix` an operation it declares. Either
            // half unanswered leaves `callee_opt` `None` so the caller falls
            // through to the standard "unknown function" error instead of
            // deferring an unvalidated call to codegen, where it would panic
            // instead of failing cleanly.
            else if let Some(decl) =
                self.effect_or_resource_decl_at(ident.segments.first().map(|seg| seg.id))
                && self
                    .tysys
                    .signatures
                    .resource_method_sig(decl, suffix)
                    .is_some()
            {
                // Signature resolution, the effect check, dispatch and WIR all
                // key on the declaration's name; an alias must not split them.
                let declared = self.tysys.resolutions.defs().name(decl).to_string();
                (
                    Some(CalleeRef::local_namespace(
                        &mut self.interner.borrow_mut(),
                        &declared,
                        suffix,
                    )),
                    effective_name.to_string(),
                )
            } else {
                (None, effective_name.to_string())
            }
        }
        // The call's own reference site, answered by the module that wrote it
        // (WEP 2026-08-12) — not by the module the walk is standing in, which
        // for a parameter default is the caller's.
        else if let Some(callee) =
            self.tysys
                .resolutions
                .declared_if_walked(ident.id)
                .filter(|def| {
                    self.tysys.resolutions.defs().kind(*def) == crate::defs::DefKind::Function
                })
        {
            self.record_reference_to_decl(ident.id, callee);
            (Some(self.callee_of(callee)), effective_name.to_string())
        }
        // `panic` / `unreachable` where no site answered — a synthesised call.
        // A module declaring either name of its own is answered above.
        else if matches!(effective_name, "panic" | "unreachable") {
            (
                self.callee_in_module(&ModuleSource::rt(), effective_name),
                effective_name.to_string(),
            )
        } else {
            // Unknown function - will report error
            (None, effective_name.to_string())
        };

        // Resolve the callee down to a single `CalleeRef`. For unknown
        // callees we emit `UnknownFunction` and fall back to a sentinel in
        // the current module so downstream lookups return empty safely.
        let callee = if let Some(c) = callee_opt {
            c
        } else {
            let _ = self.emit(TypeError::UnknownFunction {
                name: display_name.clone(),
                span: call.span,
            });
            CalleeRef::rendered(self.current_module_source.clone(), display_name)
        };

        // Resolve explicit type arguments (`_` resolves to UNKNOWN).
        let mut type_args: Vec<TypeId> = call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        // Fill inference slots from the argument / expected types. One path
        // serves three forms — a fully omitted turbofish, omitted trailing args
        // (`from_bytes::<Blob>(bytes)`), and explicit `_` (`pick::<_, bool>(..)`)
        // — and the explicit (non-`_`) args always win. `infer_fn_type_args`
        // returns a full param-length vec, so no slot reaches codegen
        // unsubstituted.
        if type_args.is_empty() {
            type_args =
                self.infer_fn_type_args(&callee, &call.args, &args, expected_type, call.span);
        } else {
            let type_param_count = self
                .lookup_function_type_params(&callee)
                .iter()
                .filter(|p| !p.is_effect)
                .count();
            if turbofish_needs_inference(&call.type_args, type_param_count) {
                let holes = turbofish_holes(&call.type_args);
                let inferred =
                    self.infer_fn_type_args(&callee, &call.args, &args, expected_type, call.span);
                merge_turbofish_type_args(&mut type_args, &holes, &inferred);
            }
        }

        // Before the bound check and defer/report below, so the bound check
        // sees the concrete default (and records any bound-driven synthesis).
        self.fill_defaulted_fn_type_args(&callee, &mut type_args);

        // Group flat turbofish args into the variadic pack so a pack slot holds
        // one tuple — the per-param shape inference already produces.
        self.group_variadic_type_args(&callee, &mut type_args);

        // Check trait bounds on function type arguments
        if !type_args.is_empty() {
            self.check_function_type_arg_bounds(&callee, &type_args, call.span);
            // Resolve any type parameter that appears only inside another
            // parameter's associated-type-equality bound (e.g.
            // `fn f<T, I: Iterator<Item = T>>`). The bounds check above
            // registered the owner's associated types, so this can now
            // project them.
            self.infer_type_args_from_assoc_bounds(&callee, &mut type_args);
        }

        // Defer (mint holes) or report uninferred type params.
        self.defer_or_report_uninferred_fn_type_args(
            &callee,
            &mut type_args,
            &args,
            expected_type,
            call.span,
        );

        // Look up function return type
        let mut return_type =
            self.lookup_function_return_type(&callee, callee_kind.receiver_site());

        // If we have explicit type args, substitute type parameters in the return type
        if !type_args.is_empty() {
            return_type = self.tysys.substitute_type_params(return_type, &type_args);
        }

        // WEP 2026-05-26: record the inferred /
        // explicit `type_args` so reify can emit
        // `TirExprKind::Call { type_args, … }` without re-running
        // inference. Free-function calls have no `GenericInstance`-style
        // anchor type — the substituted return type plays the same role
        // (it pins the per-call monomorphic shape reify needs to seed
        // mangled-name construction).
        self.record_generic_instantiation(call.id, type_args.clone(), return_type);

        // Check each argument: reject &T/&mut T passed where non-ref is expected.
        // For generic functions with explicit type args, rebuild param types with
        // type params substituted so UNKNOWN params become concrete types.
        let check_param_types = if type_args.is_empty() {
            param_types
        } else {
            declared_param_types
                .iter()
                .map(|&param| self.tysys.substitute_type_params(param, &type_args))
                .collect()
        };
        if !check_param_types.is_empty() && args.len() < check_param_types.len() {
            self.pad_args_with_defaults(
                &call.callee,
                &call.args,
                &mut args,
                &check_param_types,
                ctx,
            );
        }
        if signature_known && args.len() != check_param_types.len() {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: check_param_types.len(),
                found: args.len(),
                span: call.span,
            });
            return TypeTable::ERROR;
        }
        // Re-coerce literal-number args to inferred parameter types. This catches
        // calls like `two<T>(1 as u8, 2)` where the literal `2` was first resolved
        // as the default i32 because the original expected type was a TypeParam.
        if !type_args.is_empty() {
            self.recoerce_literal_args(&call.args, &mut args, &check_param_types);
        }
        for (i, arg) in args.iter_mut().enumerate() {
            if let Some(&expected) = check_param_types.get(i) {
                // Pin a deferred hole carried into this argument
                // (`let v = gen()?; foo(v)`) against the parameter type.
                if self.type_has_infer_hole(*arg) && self.hole_pinnable_against(expected) {
                    self.solve_infer_holes_against(*arg, expected);
                    *arg = self.apply_infer_holes(*arg);
                }
                self.typecheck(
                    *arg,
                    expected,
                    call.args.get(i).map_or(call.span, ast::Expr::span),
                );
            }
        }
        // Record the resolved param types so reify can replay per-argument
        // expected types (closure-literal coercion to a fn-typed param).
        if !check_param_types.is_empty() {
            self.record_call_param_types(call.id, check_param_types.clone());
        }

        let param_is_mut = self.lookup_function_param_is_mut(&call.callee);
        let func_ref = FunctionRef {
            module_source: callee.module().clone(),
            name: callee.name().to_string(),
            monomorph_info: None,
            method_info: None, // Free function call,
        };
        // WEP 2026-05-26: record the resolved callee for the
        // free / builtin / namespaced call paths so reify reproduces
        // the same FunctionRef shape (module_source, mangled name,
        // method_info) without re-running the dispatch logic. The
        // static-method path already records via the early-return at
        // the `is_static_method` arm; this covers the remaining
        // shapes (`println(x)`, `builtin::array_new(n)`,
        // `ns::foo(x)` for use-namespaced imports).
        let key = call.id;
        self.sem.types.static_method_dispatch.insert(
            key,
            super::sem::types::StaticMethodDispatch {
                // A free function, whose spelled callee already records the
                // edge; this fact exists for reify's `FunctionRef` shape.
                method_def: None,
                function_ref: func_ref,
                param_is_mut,
                type_args: type_args.clone(),
                param_defaults: vec![],
                param_types: check_param_types,
                self_in_args: false,
            },
        );
        // Reify rebuilds the `Call` TIR from the recorded
        // `static_method_dispatch` + `generic_instantiations` +
        // `call_param_types` and the resolved args; the body walk
        // projects only the result type.
        return_type
    }

    /// Lower `call` into a `TirExprKind::IndirectCall` using `callee_expr`
    /// as the resolved callee. Shared by the local-fn-typed-variable path
    /// and the general non-identifier path.
    ///
    /// `pad_with_defaults` is true only for the local-variable path, where
    /// closure defaults declared at the `let` site can fill missing
    /// trailing arguments. Function-typed values stored elsewhere (fields,
    /// array elements, call results) carry no default information.
    fn build_indirect_call(
        &mut self,
        call: &ast::CallExpr,
        ctx: &mut FunctionContext,
        fn_params: &[TypeId],
        return_type: TypeId,
        pad_with_defaults: bool,
    ) -> TypeId {
        let mut args: Vec<TypeId> = call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = fn_params.get(i).copied();
                self.resolve_expr(arg, ctx, expected_type)
            })
            .collect();

        if pad_with_defaults && args.len() < fn_params.len() {
            self.pad_args_with_defaults(&call.callee, &call.args, &mut args, fn_params, ctx);
        }

        if args.len() != fn_params.len() {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: fn_params.len(),
                found: args.len(),
                span: call.span,
            });
            return TypeTable::ERROR;
        }

        for (i, arg) in args.iter().enumerate() {
            if let Some(&expected) = fn_params.get(i) {
                self.typecheck(
                    *arg,
                    expected,
                    call.args.get(i).map_or(call.span, ast::Expr::span),
                );
            }
        }

        // Reify (`reify_call`'s indirect-call branch) rebuilds
        // the `IndirectCall` from the AST — resolving the callee, applying
        // `deref_to_value`, and reifying the args — so the body walk
        // projects only the result type. The `args` Vec was built above for the
        // `resolve_expr` / `typecheck` fact-recording side effects.
        return_type
    }

    /// Look up the return type of a function
    pub(super) fn lookup_function_return_type(
        &mut self,
        callee: &CalleeRef,
        receiver_site: Option<ast::AstId>,
    ) -> TypeId {
        let callee_module = callee.module();
        let func_name = callee.name();
        // Handle builtin functions
        if callee_module.is_core_builtin() {
            return self.get_builtin_return_type(func_name);
        }
        // Legacy: builtin::name pattern
        if let Some(builtin_name) = func_name.strip_prefix("builtin::") {
            return self.get_builtin_return_type(builtin_name);
        }

        // Effect operations are routed here as `CalleeRef::local_namespace`, so
        // `ModuleSource::Local { path }` matches `is_effect_like()`.
        if callee_module.is_effect_like()
            && let Some(decl) = self.effect_or_resource_decl_at(receiver_site)
            && let Some((_, Some(return_type))) = self.resolve_effect_op_signature(decl, func_name)
        {
            return return_type;
        }

        // First, try local functions (entry point module)
        if callee_module.is_entry_point()
            && let Some(&return_type) = self.sem.decls.function_return_types.get(func_name)
        {
            return return_type;
        }

        if let Some(def) = callee.def()
            && let Some(sig) = self.tysys.signatures.function_sig(def)
            && let Some(return_type) = sig.decl.return_type
        {
            return return_type;
        }

        // Default to UNIT for unknown functions (they might be external/builtin)
        TypeTable::UNIT
    }

    /// Resolve an effect operation's `(param types, return type)` — the single
    /// source of truth shared by `lookup_function_signature` and
    /// `lookup_function_return_type` (issue #1371). An operation is a method on
    /// an `interface` (WASI/user effect) or a `resource` (WASI handle); both
    /// store methods as `InterfaceMethod`s, and the decl pass recorded both
    /// kinds as a [`MethodSig`] in the declaration's own frame.
    fn resolve_effect_op_signature(
        &self,
        effect: crate::defs::DefId,
        operation: &str,
    ) -> Option<(Vec<TypeId>, Option<TypeId>)> {
        let sig = self
            .tysys
            .signatures
            .resource_method_sig(effect, operation)?;
        Some((sig.decl.param_types.clone(), sig.decl.return_type))
    }

    /// Get the String struct type (from core:prelude/string.wado)
    pub(super) fn get_string_struct_type(&mut self) -> TypeId {
        self.tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String)
    }

    /// Get the return type of a builtin function
    ///
    /// Returns the pre-resolved `TypeId` from the `BuiltinRegistry`.
    /// For generic builtins like `array_new<T>`, returns a type containing
    /// `TypeParam` placeholders that get substituted during monomorphization.
    pub(super) fn get_builtin_return_type(&self, name: &str) -> TypeId {
        self.tysys
            .builtin_registry
            .get_return_type(name)
            .unwrap_or(TypeTable::UNIT)
    }

    /// A callee's declared parameter types and the slots they mention, by the
    /// effective callee name (after [`Self::classify_call_callee`]'s `Self::` /
    /// `T::` rewriting). One lookup answers both: a parameter type is usable as
    /// an argument's expected type only once its slots are instantiated, and a
    /// rigid slot is opaque — a literal checked against one can only be
    /// rejected. Written qualified, an instance method's receiver is one of the
    /// parameters, since the call writes it as the first argument.
    ///
    /// `None` is a callee this lookup could not find, which an empty parameter
    /// list does not say: a callee declaring no parameters has a count to check
    /// like any other.
    pub(super) fn lookup_function_signature(
        &mut self,
        name: &str,
        receiver_site: Option<ast::AstId>,
        callee_site: Option<ast::AstId>,
    ) -> Option<(Vec<TypeId>, Vec<TypeId>)> {
        // Check for qualified name (Type::method or Effect::operation)
        if let Some(pos) = name.find("::") {
            let prefix = &name[..pos];
            let suffix = &name[pos + 2..];
            // Check if it's a static method
            if self.is_static_method(prefix, suffix) {
                // One signature answers both halves: the slots this use site
                // instantiates and the parameters it checks against. The
                // parameter types come back in the declaration's own frame, so
                // the call site has the same reason to instantiate them as it
                // does for a free function.
                let sig = self.unique_qualified_method_sig(prefix, suffix)?;
                let slots = sig.decl.type_params.iter().map(|(_, id)| *id).collect();
                return Some((sig.decl.param_types, slots));
            }

            // Builtin functions resolve through the `core:builtin` module,
            // slots and all: a generic builtin takes its type parameter from
            // the expected type, and a literal argument is re-coerced to it.
            if prefix == "builtin"
                && let Some(def) = self.decl_in_module(&ModuleSource::builtin(), suffix)
                && let Some(sig) = self.tysys.signatures.function_sig(def)
            {
                return Some((
                    sig.decl.param_types.clone(),
                    sig.decl.type_params.iter().map(|(_, id)| *id).collect(),
                ));
            }

            if let Some(decl) = self.effect_or_resource_decl_at(receiver_site)
                && let Some((params, _)) = self.resolve_effect_op_signature(decl, suffix)
            {
                return Some((params, Vec::new()));
            }

            // A namespace member's signature lives in that module, which no
            // bare-name lookup reaches. Without it the arguments resolve with no
            // expected type, so a sequence literal never coerces to its `List`
            // parameter and reaches codegen mismatched.
            if self.sem.imports.namespace_imports.contains_key(prefix) {
                let ns_source = self.sem.imports.namespace_imports[prefix].clone();
                if let Some(def) = self.decl_in_module(&ns_source, suffix)
                    && let Some(sig) = self.tysys.signatures.function_sig(def)
                {
                    return Some((
                        sig.decl.param_types.clone(),
                        sig.decl.type_params.iter().map(|(_, id)| *id).collect(),
                    ));
                }
                // `is_static_method` above declines the `ns::Type::method`
                // shape, so the receiver resolves through the namespace instead.
                if let Some((type_name, method_name)) = suffix.split_once("::")
                    && let Some(def) = self.namespace_member(prefix, type_name)
                {
                    let ns_key =
                        trait_env::ImplTargetKey::of_decl(self.tysys.resolutions.defs(), def);
                    if let Some(params) = self.lookup_static_method_param_types_keyed(
                        type_name,
                        method_name,
                        Some(&ns_key),
                    ) {
                        let slots = self.lookup_static_method_slots(method_name, &ns_key);
                        return Some((params, slots));
                    }
                }
            }
            return None;
        }

        // One read for this module's functions, its imports under either
        // spelling, and a default expression's callee scope.
        let sig = callee_site.and_then(|site| self.free_function_sig_at(site))?;
        Some((
            sig.decl.param_types.clone(),
            sig.decl.type_params.iter().map(|(_, id)| *id).collect(),
        ))
    }

    /// Fill missing trailing arguments from the callee's declared defaults,
    /// resolving each in the caller's context. A default may name an earlier
    /// parameter (`fn rect(w, h = w)`), so param-name idents in its cloned AST
    /// are substituted with the caller's argument AST before resolution. A
    /// position with no declared default is left for the arity check.
    pub(super) fn pad_args_with_defaults(
        &mut self,
        callee: &Expr,
        call_args_ast: &[Expr],
        args: &mut Vec<TypeId>,
        param_types: &[TypeId],
        ctx: &mut FunctionContext,
    ) {
        let (defaults, callee_module) = self.lookup_function_param_defaults(callee, ctx);
        if defaults.is_empty() {
            return;
        }
        let mut subs: IndexMap<String, Expr> = IndexMap::default();
        for (i, arg_ast) in call_args_ast.iter().enumerate() {
            if let Some((name, _)) = defaults.get(i) {
                subs.insert(name.clone(), arg_ast.clone());
            }
        }
        self.with_default_scope_module(callee_module, |s| {
            for i in args.len()..param_types.len() {
                let (name, default_ast) = match defaults.get(i) {
                    Some((n, Some(d))) => (n.clone(), d.clone()),
                    _ => break,
                };
                let mut default_expr = default_ast;
                let vantage = s
                    .annotate_ctx
                    .default_scope_module
                    .clone()
                    .map(|m| (m, default_expr.id().space()));
                default_expr.substitute_idents(&subs);
                let expected_type = param_types[i];
                let resolved = s.with_foreign_vantage(vantage, |s| {
                    s.resolve_expr(&default_expr, ctx, Some(expected_type))
                });
                if resolved == TypeTable::UNIT
                    && expected_type != TypeTable::UNIT
                    && expected_type != TypeTable::ERROR
                    && expected_type != TypeTable::UNKNOWN
                {
                    let expected_name = s.tysys.type_table.borrow().type_name(expected_type);
                    panic!(
                        "compiler bug: default expression for parameter '{name}' \
                         re-resolved to () at call site but parameter expects '{expected_name}'. \
                         Likely cause: the default references callee-only scope \
                         (e.g. a callee type parameter like `T::default()`) that is \
                         invisible during call-site re-resolution. \
                         Resolving defaults per-monomorphization is deferred work; \
                         see WEP 2026-04-11 `docs/wep-2026-04-11-default-arguments.md`. \
                         Default span: {:?}",
                        default_expr.span()
                    );
                }
                args.push(resolved);
                subs.insert(name, default_expr);
            }
        });
    }

    /// Look up the default-value AST and parameter name for each parameter of a
    /// free function callee, in declaration order, along with the callee's
    /// defining [`ModuleSource`] (for resolving defaults in the callee's
    /// lexical scope). Returns `(Vec::new(), None)` for unknown/builtin
    /// functions. Used to synthesize missing trailing arguments at the call
    /// site.
    pub(super) fn lookup_function_param_defaults(
        &mut self,
        callee: &Expr,
        ctx: &FunctionContext,
    ) -> (Vec<(String, Option<Expr>)>, Option<ModuleSource>) {
        let Expr::Ident(ident) = callee else {
            return (Vec::new(), None);
        };
        if let Some(defaults) = ctx.closure_defaults.get(&ident.name) {
            return (defaults.clone(), None);
        }
        let Some(def) = self.free_function_at(ident.id) else {
            return (Vec::new(), None);
        };
        let Some(sig) = self.tysys.signatures.function_sig(def) else {
            return (Vec::new(), None);
        };
        // A default resolves in the declaring module's scope, which is the
        // callee's own — never the caller's, even under the same spelling.
        (
            crate::elaborator::sig::Param::named_defaults(&sig.params),
            Some(self.tysys.resolutions.defs().module(def).clone()),
        )
    }

    /// Look up whether each parameter of a free function is `mut`.
    /// Returns empty vec for unknown/builtin functions (conservative: always copy).
    pub(super) fn lookup_function_param_is_mut(&mut self, callee: &Expr) -> Vec<bool> {
        let Expr::Ident(ident) = callee else {
            return Vec::new();
        };

        // Qualified names (Type::method, static calls) handled elsewhere
        if ident.name.contains("::") {
            return Vec::new();
        }

        self.free_function_sig_at(ident.id)
            .map(|sig| crate::elaborator::sig::Param::is_mut_flags(&sig.params))
            .unwrap_or_default()
    }

    /// Infer a generic call's type arguments from its actual argument types, in
    /// three [`InferCtx`] tiers: typed arguments first, numeric literals second
    /// so a literal's default cannot clobber a typed neighbour's binding, then
    /// the declared return type against `expected_type`. Returns declaration
    /// order, or empty if any parameter is left unbound — all-or-nothing.
    pub(super) fn infer_fn_type_args(
        &mut self,
        callee: &CalleeRef,
        raw_args: &[Expr],
        args: &[TypeId],
        expected_type: Option<TypeId>,
        span: crate::token::Span,
    ) -> Vec<TypeId> {
        let func_name = callee.name();
        // Builtin functions: pull type-param / param / return info from the
        // BuiltinRegistry so that calls like `builtin::select(a, b, c)` and
        // `builtin::array_new(n)` infer their generic type parameters from
        // argument types or LHS annotations the same way ordinary generic
        // functions do.
        if let Some(info) = self.tysys.builtin_registry.get(func_name) {
            if info.type_params.is_empty() {
                return vec![];
            }
            // Copy what the signature needs before instantiating: `info`
            // borrows the registry, and minting variables takes `self`.
            let real_type_params: Vec<String> = info.type_params.clone();
            let decl_param_types: Vec<TypeId> = info.params.iter().map(|(_, t)| *t).collect();
            let decl_return = info.return_type;
            let param_ids: Vec<TypeId> = real_type_params
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::TypeParam {
                            index: i as u32,
                            name: name.clone(),
                        })
                })
                .collect();

            let inst = self.instantiate(
                &param_ids,
                &Instantiation {
                    kind: "builtin",
                    name: func_name,
                    span,
                },
            );
            let resolved_param_types = self.instantiate_types(&decl_param_types, &inst);
            let decl_return_type = self.instantiate_type(decl_return, &inst);

            let mut infer = InferCtx::new(&self.tysys.type_table, inst.vars.clone());
            for (i, (param_type, arg)) in resolved_param_types.iter().zip(args.iter()).enumerate() {
                if Self::is_literal_number_arg(raw_args.get(i)) {
                    infer.add_deferred(*param_type, *arg);
                } else {
                    infer.add(*param_type, *arg);
                }
            }
            if let Some(expected) = expected_type {
                infer.add_expected_return(decl_return_type, expected);
            }
            let (inferred, bindings) = infer.solve_with_bindings();
            // A use site's variables are fresh, so "was anything inferred" is
            // simply whether any of them got bound.
            if !inst.vars.iter().any(|v| bindings.contains_key(v)) {
                return vec![];
            }
            self.record_instantiation(&inst, &inferred);
            return inferred;
        }

        // Fast path: current-module cache (populated during item resolution).
        let cached = if let (Some(tp), Some(rp)) = (
            self.sem
                .decls
                .generic_function_params
                .get(func_name)
                .cloned(),
            self.sem
                .decls
                .generic_function_resolved_param_types
                .get(func_name)
                .cloned(),
        ) {
            let decl_return = self
                .sem
                .decls
                .generic_function_resolved_return_types
                .get(func_name)
                .copied();
            Some((tp, rp, decl_return))
        } else {
            None
        };

        let (type_param_list, resolved_param_types, decl_return_type) = if let Some(v) = cached {
            v
        } else {
            // Fallback: cross-module lookup via imported functions / loaded modules.
            let Some(info) = self.lookup_generic_func_for_inference(callee) else {
                return vec![];
            };
            info
        };

        if type_param_list.is_empty() {
            return vec![];
        }

        let param_ids: Vec<TypeId> = type_param_list.iter().map(|(_, id)| *id).collect();

        // Instantiate the callee's slots first. Inside a generic scope a caller
        // may forward its own type parameter (`outer<T>` calling `inner(g)`
        // where `g: &mut T`), and the solver answers with the caller's id —
        // which is what monomorphization needs. What it must not do is confuse
        // the callee's own slot with that caller parameter; fresh variables
        // make the two distinguishable rather than merely distinguishable-ish.
        let inst = self.instantiate(
            &param_ids,
            &Instantiation {
                kind: "function",
                name: func_name,
                span,
            },
        );
        let resolved_param_types = self.instantiate_types(&resolved_param_types, &inst);
        let decl_return_type = decl_return_type.map(|r| self.instantiate_type(r, &inst));

        let mut infer = InferCtx::new(&self.tysys.type_table, inst.vars.clone());
        for (i, (param_type, arg)) in resolved_param_types.iter().zip(args.iter()).enumerate() {
            if Self::is_literal_number_arg(raw_args.get(i)) {
                infer.add_deferred(*param_type, *arg);
            } else {
                infer.add(*param_type, *arg);
            }
        }
        if let (Some(decl_ret), Some(expected)) = (decl_return_type, expected_type) {
            infer.add_expected_return(decl_ret, expected);
        }

        let (inferred, bindings) = infer.solve_with_bindings();
        if !inst.vars.iter().any(|v| bindings.contains_key(v)) {
            return vec![];
        }
        self.record_instantiation(&inst, &inferred);
        inferred
    }

    /// Resolve a type parameter appearing only inside another's
    /// associated-type-equality bound (`fn f<T, I: Iterator<Item = T>>`), which
    /// [`Self::infer_fn_type_args`] leaves unbound since it is in no parameter
    /// type. For each `Owner: Trait<Assoc = Target>` with `Owner` already
    /// concrete, project its `Assoc` to bind `Target`, iterating to a fixpoint.
    fn infer_type_args_from_assoc_bounds(
        &mut self,
        callee: &CalleeRef,
        type_args: &mut Vec<TypeId>,
    ) {
        // The dense type-argument space: non-effect, non-`fn`-bound params in
        // declaration order, the same space `type_args` is indexed by. An
        // effect param sits in the declared list but consumes no slot, so
        // walking the declared list would misalign every index past it.
        let params: Vec<ast::GenericParam> = self
            .lookup_function_type_params(callee)
            .into_iter()
            .filter(super::super::ast::GenericParam::is_real_type_param)
            .collect();
        // A turbofish naming only the non-pack params (`parse::<Perms>()`,
        // where the subject appears solely in the return type) leaves the
        // trailing pack slot absent. Seed it with its declared, still-unbound
        // form so the projection below can pin it from the owner's bound.
        for (i, param) in params.iter().enumerate().skip(type_args.len()) {
            let declared = {
                let mut tt = self.tysys.type_table.borrow_mut();
                if param.is_pack {
                    tt.make_type_pack(param.name.clone(), i as u32)
                } else {
                    tt.make_type_param(param.name.clone(), i as u32)
                }
            };
            type_args.push(declared);
        }
        self.resolve_assoc_bound_args(&params, type_args);
    }

    /// Report "cannot infer type parameter" at the call site rather than letting
    /// an unsubstituted `TypeParam` reach codegen and trap. Effect parameters,
    /// `fn`-bound ones (constrained structurally), defaulted ones (already
    /// filled), and ones bound to an outer-scope `TypeParam` (the caller
    /// forwarding its own generics) are all excluded.
    fn report_uninferred_fn_type_args(
        &mut self,
        callee: &CalleeRef,
        type_args: &[TypeId],
        span: crate::token::Span,
    ) {
        let params = self.lookup_function_type_params(callee);
        let inferable: Vec<&ast::GenericParam> = params
            .iter()
            .filter(|p| !p.is_effect && p.default.is_none() && !p.has_fn_bound())
            .collect();
        if inferable.is_empty() {
            return;
        }
        let scope_params = self.scope_type_param_ids();

        // When inference produced no type args at all, every inferable
        // parameter is unresolved. Otherwise check each against its inferred
        // slot (parallel to the full declared parameter list).
        let unresolved: Vec<&str> = if type_args.is_empty() {
            inferable.iter().map(|p| p.name.as_str()).collect()
        } else if type_args.len() == params.len() {
            params
                .iter()
                .zip(type_args.iter())
                .filter(|&(p, &tid)| {
                    !p.is_effect
                        && p.default.is_none()
                        && !p.has_fn_bound()
                        && self.is_unbound_type_param(tid)
                        && !scope_params.contains(&tid)
                })
                .map(|(p, _)| p.name.as_str())
                .collect()
        } else {
            // Length mismatch (packs/effects interleaved): be conservative
            // and do not risk a false diagnostic.
            return;
        };

        if unresolved.is_empty() {
            return;
        }
        let names = unresolved
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let func_name = callee.name();
        let _ = self.emit(TypeError::CannotInferType {
            message: format!(
                "cannot infer type parameter {names} of function `{func_name}`; \
                 add a turbofish (`{func_name}::<...>()`) or a type annotation"
            ),
            span,
        });
    }

    fn report_uninferred_static_method_type_args(
        &mut self,
        prefix: &str,
        suffix: &str,
        impl_type_args: &[TypeId],
        method_type_args: &[TypeId],
        span: crate::token::Span,
    ) {
        let Some(sig) = self.qualified_method_sig(prefix, suffix) else {
            return;
        };
        let (declaring_slots, method_slots) = (sig.declaring_type_params(), sig.own_type_params());
        if declaring_slots.is_empty() && method_slots.is_empty() {
            return;
        }

        let scope_params = self.scope_type_param_ids();
        let unresolved = |this: &Self, slot: Option<&TypeId>| -> bool {
            match slot {
                None => true,
                Some(&t) => this.is_unbound_type_param(t) && !scope_params.contains(&t),
            }
        };

        let mut names: Vec<String> = declaring_slots
            .iter()
            .enumerate()
            .filter(|&(i, _)| unresolved(self, impl_type_args.get(i)))
            .map(|(_, (name, _))| name.clone())
            .collect();
        let type_level_unresolved = !names.is_empty();
        names.extend(
            method_slots
                .iter()
                .enumerate()
                .filter(|&(i, _)| unresolved(self, method_type_args.get(i)))
                .map(|(_, (name, _))| name.clone()),
        );
        if names.is_empty() {
            return;
        }

        let joined = names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let turbofish = if type_level_unresolved {
            format!("`{prefix}::<...>::{suffix}()`")
        } else {
            format!("`{prefix}::{suffix}::<...>()`")
        };
        let _ = self.emit(TypeError::CannotInferType {
            message: format!(
                "cannot infer type parameter {joined} of `{prefix}::{suffix}`; \
                 add a turbofish ({turbofish}) or a type annotation"
            ),
            span,
        });
    }

    /// Substitute a declared default (`fn f<T = Fallback>`) into any dense
    /// type-arg slot call-site inference left unbound, seeding an empty
    /// `type_args` first so an omitted turbofish is covered. Each default
    /// resolves with the callee's params in scope (`<T, U = T>` picks up `T`)
    /// and at `default_scope_module`, so it may name a type private to it.
    fn fill_defaulted_fn_type_args(&mut self, callee: &CalleeRef, type_args: &mut Vec<TypeId>) {
        let params = self.lookup_function_type_params(callee);
        let space: Vec<ast::GenericParam> = params
            .iter()
            .filter(|p| p.is_real_type_param())
            .cloned()
            .collect();
        if !space.iter().any(|p| p.default.is_some()) {
            return;
        }
        let n = space.len();

        let defaults: Vec<Option<TypeId>> =
            self.with_default_scope_module(Some(callee.module().clone()), |s| {
                let mut scope = s.enter_inherited_type_param_scope();
                scope.annotate_ctx.trait_ctx.type_params.clear();
                scope.register_generic_params(&params, 0);
                space
                    .iter()
                    .map(|p| p.default.as_ref().map(|ty| scope.resolve_type(ty)))
                    .collect()
            });

        if type_args.is_empty() {
            *type_args = space
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_param(p.name.clone(), i as u32)
                })
                .collect();
        }
        if type_args.len() != n {
            // Pack / effect interleaving produced a non-dense list; don't
            // risk a misaligned substitution.
            return;
        }
        // A slot bound to an enclosing scope's own type param is a caller
        // forwarding its generics (`fn outer<T>() { defaulted(x) }` where
        // `x: T`); monomorphization resolves it, so leave it — replacing it
        // with the default would wrongly pin it to the default type. Mirrors
        // the `scope_params` guard in `defer_or_report_uninferred_fn_type_args`.
        let scope_params = self.scope_type_param_ids();
        // Fill in declaration order so a default that references an earlier
        // param (`U = T`) sees `T`'s slot already resolved.
        for i in 0..n {
            let slot = type_args[i];
            if self.is_unbound_type_param(slot)
                && !scope_params.contains(&slot)
                && let Some(default_ty) = defaults[i]
            {
                let snapshot = type_args.clone();
                type_args[i] = self.tysys.substitute_type_params(default_ty, &snapshot);
            }
        }
    }

    /// Defer (mint inference holes) or report unresolved free-function type
    /// parameters, mirroring the instance-method deferral. Gated on a hole-free
    /// argument list and no expected type. Runs after
    /// [`Self::fill_defaulted_fn_type_args`], so any declared default is already
    /// substituted and only genuinely unresolvable params remain here.
    pub(super) fn defer_or_report_uninferred_fn_type_args(
        &mut self,
        callee: &CalleeRef,
        type_args: &mut Vec<TypeId>,
        args: &[TypeId],
        expected_type: Option<TypeId>,
        span: crate::token::Span,
    ) {
        let params = self.lookup_function_type_params(callee);
        // Dense type-argument index space (matches `populate_generic_function_cache`):
        // non-effect, non-`fn`-bound params in declaration order.
        let space: Vec<&ast::GenericParam> =
            params.iter().filter(|p| p.is_real_type_param()).collect();
        let n = space.len();
        if n == 0 {
            return;
        }
        // Defaults are already substituted (`fill_defaulted_fn_type_args`), so
        // the dense-aware logic below handles the rest. Not `report_uninferred`
        // (which keys on the full param count, dropping the diagnostic when an
        // effect / `fn`-bound param shifts the dense length).
        // Full-length (some slots unbound) or empty (nothing inferred); any other
        // length is a pack/effect interleaving we do not touch.
        let from_empty = type_args.is_empty();
        if !from_empty && type_args.len() != n {
            self.report_uninferred_fn_type_args(callee, type_args, span);
            return;
        }
        let scope_params = self.scope_type_param_ids();

        let unresolved = |this: &Self, i: usize| -> bool {
            if from_empty {
                return true;
            }
            let t = type_args[i];
            this.is_unbound_type_param(t) && !scope_params.contains(&t)
        };

        let unresolved_names: Vec<String> = space
            .iter()
            .enumerate()
            .filter(|&(i, _)| unresolved(self, i))
            .map(|(_, p)| p.name.clone())
            .collect();
        if unresolved_names.is_empty() {
            return;
        }

        let can_defer =
            expected_type.is_none() && args.iter().all(|a| !self.type_has_infer_hole(*a));
        if !can_defer {
            self.report_uninferred_fn_type_args(callee, type_args, span);
            return;
        }

        let func_name = callee.name();
        let message = format!(
            "cannot infer type parameter {} of function `{func_name}`; \
             add a turbofish (`{func_name}::<...>()`) or a type annotation",
            unresolved_names
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut new_args: Vec<TypeId> = Vec::with_capacity(n);
        for (i, p) in space.iter().enumerate() {
            if !unresolved(self, i) {
                new_args.push(type_args[i]);
                continue;
            }
            let bounds = self.declared_bounds(p);
            // `infer_fn_type_args` already instantiated this slot, so the
            // variable standing in for it is the one to blame — minting a
            // second would orphan the first, which the sweep would then pin to
            // `error` behind a diagnostic nobody asked for.
            let existing = type_args
                .get(i)
                .copied()
                .filter(|&t| self.tysys.type_table.borrow().contains_infer_var(t));
            new_args.push(match existing {
                Some(var) => {
                    self.attach_infer_var_diag(var, span, message.clone());
                    self.attach_infer_var_bounds(var, p.name.clone(), bounds, span);
                    var
                }
                None => self.mint_infer_hole(span, message.clone(), p.name.clone(), bounds),
            });
        }
        *type_args = new_args;
    }

    /// Shared core of bound-driven inference, used by both the free-function and
    /// method paths: for each already-concrete parameter, read its bounds'
    /// `Trait<Assoc = Target>` bindings and, where `Target` names a still-unbound
    /// parameter, project the owner's `Assoc` to bind it. `params` and `args`
    /// share an index space; iterates to a fixpoint for chained bounds.
    pub(super) fn resolve_assoc_bound_args(
        &mut self,
        params: &[ast::GenericParam],
        args: &mut [TypeId],
    ) {
        if params.len() != args.len() {
            return;
        }
        loop {
            let mut progressed = false;
            for (owner_idx, param) in params.iter().enumerate() {
                let owner_ty = args[owner_idx];
                if self.is_unbound_type_param(owner_ty) {
                    continue;
                }
                for bound in &param.bounds {
                    for assoc in &bound.assoc_types {
                        let Some(target_name) = assoc_bound_target_param(&assoc.ty) else {
                            continue;
                        };
                        let Some(target_idx) = params.iter().position(|p| p.name == target_name)
                        else {
                            continue;
                        };
                        if !self.is_unbound_type_param(args[target_idx]) {
                            continue;
                        }
                        let resolved = {
                            let mut tt = self.tysys.type_table.borrow_mut();
                            match tt.resolve_assoc_type(owner_ty, &assoc.name) {
                                Some(r) => Some(r),
                                None => tt.resolve_generic_assoc_type_mono(owner_ty, &assoc.name),
                            }
                        };
                        // Reflection's associated types are registered by a
                        // synthesis phase that runs after elaboration, so the
                        // registry is empty here; compute the subject's.
                        let resolved = resolved.or_else(|| {
                            self.concrete_reflect_assoc_type(owner_ty, &bound.name, &assoc.name)
                        });
                        if let Some(resolved) = resolved {
                            args[target_idx] = resolved;
                            progressed = true;
                        }
                    }
                }
            }
            if !progressed {
                break;
            }
        }
    }

    /// Whether `ty` is a type argument nothing has determined yet — either a
    /// slot the solver left as its own parameter, or an inference variable it
    /// never solved. Both mean "no answer", so a default may still fill it.
    pub(super) fn is_unbound_type_param(&self, ty: TypeId) -> bool {
        matches!(
            self.tysys.type_table.borrow().get(ty),
            ResolvedType::TypeParam { .. }
                | ResolvedType::TypePack { .. }
                | ResolvedType::InferVar(_)
        )
    }

    /// Group flat turbofish type args into a variadic pack. `ids::<i32, bool>()`
    /// resolves two args for a single `..T`, but the pack slot must hold one
    /// tuple `[i32, bool]` — the per-param shape inference produces. A no-op when
    /// the call has no pack or the args are already in per-param form (arg count
    /// ≤ param count), so inference results and single-arg packs pass through.
    fn group_variadic_type_args(
        &mut self,
        callee: &super::callee::CalleeRef,
        type_args: &mut Vec<TypeId>,
    ) {
        let real: Vec<ast::GenericParam> = self
            .lookup_function_type_params(callee)
            .into_iter()
            .filter(|p| !p.is_effect)
            .collect();
        let Some(pack_pos) = real.iter().position(|p| p.is_pack) else {
            return;
        };
        if type_args.len() <= real.len() {
            return;
        }
        // Single pack (guaranteed by the parser): it absorbs every arg past the
        // non-pack params.
        let non_pack = real.len() - 1;
        let pack_count = type_args.len() - non_pack;
        let pack_args: Vec<TypeId> = type_args.drain(pack_pos..pack_pos + pack_count).collect();
        let tuple = self.tysys.type_table.borrow_mut().make_tuple(pack_args);
        type_args.insert(pack_pos, tuple);
    }

    /// Look up a generic function (current or imported) and produce a temporary
    /// `(type_param_list, resolved_param_types, decl_return_type)` triple suitable
    /// for type-arg inference. Sets up the function's type params in scope while
    /// resolving param and return types.
    fn lookup_generic_func_for_inference(
        &self,
        callee: &CalleeRef,
    ) -> Option<(Vec<(String, TypeId)>, Vec<TypeId>, Option<TypeId>)> {
        let sig = self.tysys.signatures.function_sig(callee.def()?)?;
        if sig.type_param_ids.is_empty() {
            return Some((vec![], vec![], None));
        }
        Some((
            sig.type_param_ids.clone(),
            sig.decl.param_types.clone(),
            sig.decl.return_type,
        ))
    }

    /// Returns true if `arg` is a numeric literal expression (`123`, `3.14`, `-5`)
    /// with no explicit type annotation. These args participate in literal coercion
    /// and should be deferred during type-arg inference's first phase.
    pub(super) fn is_literal_number_arg(arg: Option<&Expr>) -> bool {
        match arg {
            Some(Expr::Literal(lit)) => matches!(lit.value, ast::Literal::Number(_)),
            Some(Expr::Unary(unary)) if unary.op == ast::UnaryOp::Neg => matches!(
                &unary.expr,
                Expr::Literal(inner) if matches!(inner.value, ast::Literal::Number(_))
            ),
            _ => false,
        }
    }

    /// Infer the type args of a `Type::method(...)` static call whose
    /// method-level turbofish is omitted or carries `_`. Probes `suffix` as a
    /// local free function first — covering builtin-registry entries keyed by
    /// bare name — then falls back to full static-method inference, which also
    /// yields the impl-level args. Returns `(impl_args, method_args)`.
    fn infer_static_call_type_args(
        &mut self,
        prefix: &str,
        suffix: &str,
        raw_args: &[Expr],
        args: &[TypeId],
        expected_type: Option<TypeId>,
        span: crate::token::Span,
    ) -> (Vec<TypeId>, Vec<TypeId>) {
        let probe = CalleeRef::rendered(self.current_module_source.clone(), suffix);
        let method_args = self.infer_fn_type_args(&probe, raw_args, args, expected_type, span);
        if !method_args.is_empty() {
            return (Vec::new(), method_args);
        }
        self.infer_static_method_type_args(prefix, suffix, raw_args, args, expected_type)
    }

    /// Infer a generic static method's type arguments, sharing the three-tier
    /// [`InferCtx`] model with [`Self::infer_fn_type_args`]. Returns
    /// `(declaring_args, method_args)` — the first from `impl Container<T>`, the
    /// second from `fn make<U>()` — either possibly empty. Reads the signature's
    /// canonical types directly, solving *for* an instantiation's arguments.
    fn infer_static_method_type_args(
        &mut self,
        struct_name: &str,
        method_name: &str,
        raw_args: &[Expr],
        args: &[TypeId],
        expected_type: Option<TypeId>,
    ) -> (Vec<TypeId>, Vec<TypeId>) {
        let Some(sig) = self.qualified_method_sig(struct_name, method_name) else {
            return (vec![], vec![]);
        };
        if sig.decl.type_params.is_empty() {
            return (vec![], vec![]);
        }

        let all_param_ids: Vec<TypeId> = sig.decl.type_params.iter().map(|(_, id)| *id).collect();
        let decl_return_type = sig.decl.return_type;

        // `args` are the arguments as written, so they begin with the receiver
        // when the method declares one. Dropping it here would slide every pair
        // one position left and solve each slot from the wrong argument.
        let mut infer = InferCtx::new(&self.tysys.type_table, all_param_ids.clone());
        for (i, (param_type, arg)) in sig.decl.param_types.iter().zip(args.iter()).enumerate() {
            if Self::is_literal_number_arg(raw_args.get(i)) {
                infer.add_deferred(*param_type, *arg);
            } else {
                infer.add(*param_type, *arg);
            }
        }
        if let (Some(decl_ret), Some(expected)) = (decl_return_type, expected_type) {
            infer.add_expected_return(decl_ret, expected);
        }

        // Permissive solve — see `infer_fn_type_args` for the
        // TypeParam-forwarding rationale.
        let (inferred, bindings) = infer.solve_with_bindings();
        if !all_param_ids.iter().any(|p| bindings.contains_key(p)) {
            return (vec![], vec![]);
        }

        let split = sig.declaring_split();
        (inferred[..split].to_vec(), inferred[split..].to_vec())
    }

    /// Enforce the visibility ladder on a qualified `Type::method(...)` call.
    /// `receiver` is the key the neighbouring lookups resolved, not one
    /// re-derived from the spelling, which a splice can make mean another type.
    pub(super) fn check_static_call_visibility(
        &mut self,
        receiver: &super::trait_env::ImplTargetKey,
        effective_name: &str,
        node: Option<crate::ast::AstId>,
        span: crate::token::Span,
    ) {
        let Some((struct_name, method_name)) = effective_name.rsplit_once("::") else {
            return;
        };
        let Some(entry) = self.static_method_entry(receiver, method_name) else {
            return;
        };
        let (module, visibility) = (entry.module.clone(), entry.inherent_visibility);
        let owner = struct_name.to_string();
        self.check_inherent_member_visibility(
            visibility,
            Some(&module),
            super::expr::MemberOwner::Named(&owner),
            method_name,
            super::types::ImplMemberKind::Method,
            node,
            span,
        );
    }

    /// The static-method index entry `receiver::name` selects, for the
    /// questions the signature alone cannot answer — which module declared it,
    /// and at what visibility.
    pub(super) fn static_method_entry(
        &self,
        receiver: &super::trait_env::ImplTargetKey,
        method_name: &str,
    ) -> Option<&super::trait_env::StaticMethodEntry> {
        self.tysys
            .trait_env
            .static_method_index
            .get(receiver)?
            .iter()
            .find(|e| e.name == method_name)
    }

    /// [`Self::qualified_method_sig`] where the name resolves to exactly one
    /// declaration. An overloaded name (several `From` impls on one target)
    /// answers `None`: the call site chooses among the candidates by argument,
    /// so committing to the first indexed one would decide the overload here.
    /// Both kinds of declaration count, or a name carried by one static and one
    /// instance method reads as unique and the two never meet to be compared.
    pub(super) fn unique_qualified_method_sig(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<MethodSig> {
        let key = self.impl_target(struct_name);
        let mut declared = self.qualified_method_decl_ids(&key, method_name);
        if declared.next().is_some() && declared.next().is_some() {
            return None;
        }
        self.qualified_method_sig(struct_name, method_name)
    }

    /// The canonical signature `struct_name::method_name` names, receiver-less
    /// or instance: the spelling admits both, and a call site instantiates its
    /// slots from whichever it names. Callers hold a name split out of a mangled
    /// spelling, so there is no reference site to take.
    pub(super) fn qualified_method_sig(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<MethodSig> {
        self.qualified_method_sig_keyed(&self.impl_target(struct_name), method_name)
    }

    /// [`Self::qualified_method_sig`] for a caller that already resolved its
    /// receiver. Deriving a second key from the bare name answers from another
    /// vantage than the one the call resolved at.
    pub(super) fn qualified_method_sig_keyed(
        &self,
        key: &ImplTargetKey,
        method_name: &str,
    ) -> Option<MethodSig> {
        let trait_env = &self.tysys.trait_env;
        if let Some(entry) = trait_env
            .static_method_index
            .get(key)
            .and_then(|methods| methods.iter().find(|e| e.name == method_name))
        {
            return self.tysys.signatures.method_sig(entry.method_id).cloned();
        }
        if let Some((_, _, decl_id, _)) = trait_env.resource_static(key, method_name) {
            return self
                .tysys
                .signatures
                .resource_method_sig(*decl_id, method_name)
                .cloned();
        }
        // One name over several impls is an overload only an argument
        // separates, so a single signature is not this lookup's to pick.
        let mut declared = self
            .impl_method_decl_ids(key, method_name)
            .filter_map(|def| self.tysys.signatures.method_sig(def).cloned());
        if let Some(sig) = declared.next()
            && declared.next().is_none()
        {
            return Some(sig);
        }
        // The qualified form disambiguates a colliding name, so it reaches an
        // inherited method too.
        let &ImplTargetKey::Decl(def) = key else {
            return None;
        };
        // A resource declares its own statics whether or not the static index
        // classified them, so the signature table answers directly for the
        // ones it did not.
        if let Some(sig) = self.tysys.signatures.resource_method_sig(def, method_name)
            && sig.self_kind == ast::SelfKind::None
        {
            return Some(sig.clone());
        }
        self.resource_instance_method(def, method_name)
            .map(|(_, sig)| sig)
    }

    /// The parameter types a `Type::method(...)` call's arguments check against:
    /// the declaration's whole list, since the spelling writes the receiver as
    /// the first argument when the method declares one. An overloaded name
    /// answers `None` — its call site picks the impl by argument, and coercing
    /// toward the first indexed one would decide that here.
    pub(super) fn qualified_call_param_types(
        &mut self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<Vec<TypeId>> {
        if let Some(sig) = self.unique_qualified_method_sig(struct_name, method_name) {
            return Some(sig.decl.param_types);
        }
        self.lookup_static_method_param_types_keyed(struct_name, method_name, None)
    }

    /// `Type::method()` reaching a value blanket's static, which is indexed
    /// under the blanket's receiver param and so misses `type_name`'s own
    /// bucket. The variant-case branch owns the `Variant::Name` shape, so it
    /// shares this entry rather than falling through to the known-type one.
    pub(super) fn resolve_named_type_blanket_static(
        &mut self,
        type_name: &str,
        method: &str,
        call_id: crate::AstId,
        args: &[TypeId],
        raw_args: &[Expr],
        span: crate::Span,
    ) -> Option<TypeId> {
        // `type_name` is the receiver spelling after `Self::` / `T::`
        // rewriting, which no source segment names.
        let receiver_ty = self.resolve_unsited_type_name(type_name, span);
        // The blanket would key on the argument-less head, which carries no
        // layout (a generic variant never becomes its own declaration, WEP
        // 2026-02-09) and reaches WIR build unregistered.
        if let Some(expected) = self
            .type_decl_at(None, type_name)
            .and_then(|def| self.bare_generic_type_arity(def))
            && self
                .find_blanket_static_method(receiver_ty, method)
                .is_some()
        {
            let _ = self.emit(TypeError::MissingTypeArguments {
                name: type_name.to_string(),
                expected,
                span,
            });
            return Some(TypeTable::ERROR);
        }
        // Built here rather than on the caller's hot path: only a call that
        // actually reaches a blanket static needs the per-argument spans.
        let arg_spans: Vec<crate::Span> = raw_args.iter().map(Expr::span).collect();
        self.resolve_blanket_static_method(
            receiver_ty,
            method,
            call_id,
            &[],
            &[],
            args,
            &arg_spans,
            span,
        )
    }
}

impl TypeSystem {
    /// Infer type arguments for a variant constructor `Variant::Case(payload)`.
    ///
    /// Uses [`InferCtx`] with:
    /// * a strong constraint from the payload expression (forward inference), and
    /// * expected-return constraints from the declaration-site's type parameters,
    ///   unified against the caller's expected generic-instance type args.
    ///
    /// Falls back to a bare `Variant` type if any type parameter remains unbound
    /// and we are in a non-generic context — preserving the legacy behaviour.
    pub(super) fn infer_variant_type_args(
        &mut self,
        ctx: &Scope,
        variant_info: &super::types::VariantInfo,
        case_data: &super::types::VariantCaseData,
        payload: Option<TypeId>,
        expected_type: Option<TypeId>,
        explicit_args: &[TypeId],
        holes: &[bool],
    ) -> TypeId {
        // An expected type pins the declaration the instance is interned
        // against: a `Result` annotation and the variant reached through the
        // prelude are one declaration, and the annotation is the one the
        // caller's frame resolved.
        let mut canonical_def = None;
        let variant_def = self
            .type_table
            .borrow()
            .defs()
            .of_ast_id(variant_info.defined_at);

        let mut infer = InferCtx::new(&self.type_table, variant_info.type_param_type_ids.clone());

        // Explicit turbofish args pin their slots as strong constraints; a `_`
        // slot is skipped so the payload/expected passes infer it. This is how
        // `Result::<_, MyErr>::Ok(x)` keeps `MyErr` while inferring `T`.
        for (i, &param_id) in variant_info.type_param_type_ids.iter().enumerate() {
            if !holes.get(i).copied().unwrap_or(true)
                && let Some(&explicit) = explicit_args.get(i)
            {
                infer.add(param_id, explicit);
            }
        }

        // Forward inference: unify the case payload's declared type against
        // the actual payload expression's type.
        if let Some(payload_expr) = payload {
            infer.add(case_data.payload, payload_expr);
        }

        // Backward inference: extract type args from the caller's expected type.
        // Queued through `add_expected_return` so it runs after the payload pass,
        // preserving the "stronger constraint wins" policy via `or_insert`.
        if let Some(expected) = expected_type {
            let expected_resolved = self.type_table.borrow().get(expected).clone();
            if let ResolvedType::GenericInstance {
                def,
                type_args: expected_args,
            } = expected_resolved
                && Some(def) == variant_def
                && expected_args.len() == variant_info.type_param_type_ids.len()
            {
                canonical_def = Some(def);
                for (&param_id, &expected_arg) in variant_info
                    .type_param_type_ids
                    .iter()
                    .zip(expected_args.iter())
                {
                    infer.add_expected_return(param_id, expected_arg);
                }
            }
        }

        let type_args = infer.solve();

        // If unresolved type params remain in concrete code, fall back to bare Variant
        let has_unresolved = type_args
            .iter()
            .any(|&t| self.type_table.borrow().contains_type_param(t));

        if has_unresolved && ctx.trait_ctx.type_params.is_empty() {
            return self
                .type_table
                .borrow()
                .type_id_of_decl(variant_info.defined_at);
        }

        let def = canonical_def.unwrap_or_else(|| {
            self.type_table
                .borrow()
                .defs()
                .of_ast_id(variant_info.defined_at)
                .expect("the variant being instantiated is declared")
        });
        self.type_table
            .borrow_mut()
            .make_generic_instance(def, type_args)
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve a static call through a type parameter: `T::method(args)`
    /// where T is bound by a trait (e.g., `T: Constructable`).
    fn resolve_type_param_static_call(
        &mut self,
        type_param_name: &str,
        method_name: &str,
        type_param_type_id: TypeId,
        args: &[TypeId],
        call: &ast::CallExpr,
        _ctx: &mut FunctionContext,
    ) -> TypeId {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(type_param_name)
            .cloned();

        if let Some(bounds) = bounds
            && let Some((found_trait, method_info_result)) = {
                self.find_method_in_trait_bounds(
                    &bounds,
                    method_name,
                    type_param_type_id,
                    call.span,
                    None,
                )
            }
        {
            let return_type = method_info_result.return_type;

            // Resolve method-level type args (e.g., T::deserialize::<JsonDeserializer>)
            let mut method_type_args: Vec<TypeId> = call
                .type_args
                .iter()
                .map(|ty| self.resolve_type(ty))
                .collect();

            // If no explicit type args, infer method-level type params from argument
            // types via the shared `InferCtx` solver. `find_method_in_trait_bounds`
            // already registered the method's own type params inside its resolution
            // scope and handed them back via `method_type_param_ids`; we feed those
            // to the solver so it knows exactly which TypeIds to bind. Outer-scope
            // type params (`Self`, forwarded caller params) will not appear in
            // `method_type_param_ids` and therefore cannot contaminate the result
            // even when interning assigns them the same index as a method-level
            // one.
            if method_type_args.is_empty()
                && !method_info_result.param_types.is_empty()
                && !method_info_result.method_type_param_ids.is_empty()
            {
                let method_param_ids = method_info_result.method_type_param_ids.clone();
                let mut infer = InferCtx::new(&self.tysys.type_table, method_param_ids.clone());
                for (i, (param_type, arg)) in method_info_result
                    .param_types
                    .iter()
                    .zip(args.iter())
                    .enumerate()
                {
                    if Self::is_literal_number_arg(call.args.get(i)) {
                        infer.add_deferred(*param_type, *arg);
                    } else {
                        infer.add(*param_type, *arg);
                    }
                }
                let (inferred, bindings) = infer.solve_with_bindings();
                // Only adopt the inferred vector if the solver actually bound at
                // least one method-level parameter; otherwise leave
                // `method_type_args` empty so downstream code treats the call as
                // having no inferred method args (matching the legacy behaviour).
                if method_param_ids.iter().any(|p| bindings.contains_key(p)) {
                    method_type_args = inferred;
                }
            }

            // Don't substitute method-level type params in the return type here.
            // The return type contains function-level TypeParams (e.g., Self -> TypeParam{T})
            // which will be substituted during monomorphization. Method-level type params
            // (e.g., D in deserialize<D>) are handled by the monomorphizer.
            let final_return_type = return_type;

            // Build method_info with is_type_param_receiver = true
            let method_type_arg_names: Vec<FqTypeName> = method_type_args
                .iter()
                .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                .collect();
            let mut method_info = LocalMethodName::new(
                self.binder_in_scope(type_param_name),
                Some(found_trait),
                method_name.to_string(),
            );
            method_info.is_type_param_receiver = true;
            if !method_type_arg_names.is_empty() {
                method_info.method_type_args = method_type_arg_names;
            }

            let mangled_name = method_info.to_mangled_name();

            // Build monomorph_info for method-level type args so the
            // monomorphizer can substitute TypeParam type args and
            // queue instantiations.
            let monomorph_info = if method_type_args.is_empty() {
                None
            } else {
                Some(MonomorphInfo {
                    generic_name: mangled_name.clone(),
                    impl_type_args: vec![],
                    method_type_args,
                    is_blanket: false,
                })
            };

            let func_ref = FunctionRef {
                module_source: self.current_module_source.clone(),
                name: mangled_name,
                monomorph_info,
                method_info: Some(method_info),
            };

            // Record the resolved abstract `T::method(...)` dispatch so
            // reify can replay the same `Call` without re-running
            // trait-bound method lookup (reify has no `trait_ctx`). Keyed
            // by the `CallExpr`'s AstId — distinct from any
            // `StaticMethodCallExpr` id, so reusing `static_method_dispatch`
            // is collision-free. Args here carry no `is_mut` (the
            // production builder below uses all-false `CallArg`s). Carry the
            // trait-declared parameter defaults so reify pads omitted trailing
            // arguments.
            let param_defaults: Vec<(String, Option<ast::Expr>)> = method_info_result
                .param_names
                .iter()
                .cloned()
                .zip(method_info_result.param_defaults.iter().cloned())
                .collect();
            let key = call.id;
            self.sem.types.static_method_dispatch.insert(
                key,
                super::sem::types::StaticMethodDispatch {
                    method_def: method_info_result.method_def,
                    function_ref: func_ref,
                    param_is_mut: vec![false; args.len()],
                    type_args: vec![],
                    param_defaults,
                    param_types: method_info_result.param_types,
                    self_in_args: false,
                },
            );

            // Reify rebuilds the `T::method(...)` `Call` from the
            // recorded `static_method_dispatch`; project only the result type.
            return final_return_type;
        }

        let _ = self.emit(TypeError::UnknownFunction {
            name: format!("{type_param_name}::{method_name}"),
            span: call.span,
        });
        TypeTable::ERROR
    }
}
