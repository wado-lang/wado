//! Method call and static method call resolution.

use super::trait_env::ImplTargetKey;
use crate::ast::{self, AstId};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, Receiver, RefKind};
use crate::tir::{
    FunctionRef, MonomorphInfo, ResolvedType, SubstitutionContext, TypeId, TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::callee::StaticMethodRef;
use super::method_lookup::MethodInferenceInput;
use super::reflect::ReflectDispatch;
use super::types::{FunctionContext, MethodInfo, MethodOwner, TypeError};

/// A static call named the way [symbol notation] writes it — the receiver's
/// type arguments included (`List<i32>::with_capacity`). Rendering only the
/// head collapsed `Take<A>::take` and `Take<B>::take` onto one string, so a
/// diagnostic naming both could not tell them apart.
///
/// [symbol notation]: ../../../docs/wep-2026-06-14-symbol-notation.md
fn static_call_symbol_name(static_call: &ast::StaticMethodCallExpr) -> String {
    let mut name = String::new();
    crate::unparse::unparse_type_into(&static_call.target_type, &mut name);
    name.push_str("::");
    name.push_str(&static_call.method);
    name
}

/// Inputs to [`Elaborator::resolve_method_call_with`], the TIR-level method-call
/// dispatcher. [`Elaborator::resolve_method_call`] wraps it for AST-driven calls;
/// a synthesised one (for-of's `into_iter()` / `next()`) calls it directly with
/// an already-resolved receiver. Both ids are then `None`, which suppresses the
/// use→def edge — keeping internal helpers out of jump-to-definition — and the
/// `method_dispatch` entry, since reify walks source-level nodes only.
pub(super) struct MethodCallInput<'a> {
    pub receiver: TypeId,
    /// The receiver's source AST when the call comes from user syntax. The
    /// body walk holds only the receiver's type, so the `&mut self`
    /// receiver-mutability check walks this instead. `None` for synthetic
    /// dispatches (for-of desugaring), whose receivers are compiler-owned
    /// locals.
    pub receiver_ast: Option<&'a ast::Expr>,
    pub method_name: &'a str,
    pub method_id: Option<AstId>,
    pub call_id: Option<AstId>,
    pub type_args: Vec<TypeId>,
    /// Per-position `_` mask for `type_args` (see `call::turbofish_holes`).
    /// Empty when the caller supplied no `_` placeholders (synthetic callers
    /// and fully-explicit turbofish), which leaves inference untriggered.
    pub type_arg_holes: Vec<bool>,
    pub args: &'a [ast::Expr],
    pub expected_type: Option<TypeId>,
    pub span: Span,
    /// The trait a qualified call named (`Alpha::describe(&x)`), constraining
    /// which impl may be picked — the escape hatch for a method name two
    /// traits share (WEP 2026-07-31). `None` for an ordinary `x.m()`, whose
    /// candidates span every trait implemented for the receiver.
    pub required_trait: Option<super::types::RequiredTrait>,
}

/// Result of [`Elaborator::resolve_method_call_with`]: the call's result
/// type plus, on successful dispatch, the receiver-adjustment
/// inputs and resolved target a synthetic caller (for-of's `into_iter()`
/// / `next()`, whose `call_id == None` skips `record_method_dispatch`)
/// needs to record the decision its own way. `None` when a short-circuit
/// path returned early or method lookup failed.
pub(super) struct MethodCallOutcome {
    pub type_id: TypeId,
    pub dispatch: Option<DispatchedMethod>,
    /// The resolved signature, for a caller that suppressed
    /// `record_method_dispatch` with `call_id: None` and files its own record.
    /// The qualified-call path files a *static* dispatch, which needs the same
    /// facts: without them its arguments lose their defaults, their `is_mut`
    /// shape, and the expected types an unannotated closure argument infers
    /// from.
    pub signature: Option<MethodSignatureFacts>,
}

/// What dispatch selected, for a caller that suppressed
/// [`Elaborator::record_method_dispatch`] with `call_id: None` and files its
/// own record — the for-of iterator path and the trait-qualified static path.
pub(super) struct DispatchedMethod {
    pub self_kind: ast::SelfKind,
    pub is_ref_impl: bool,
    pub func: FunctionRef,
    /// The declaration dispatch chose. `None` for a builtin or an
    /// auto-derived method, which no declaration backs.
    pub method_def: Option<crate::defs::DefId>,
}

/// The value blanket a static call dispatches through.
pub(super) struct BlanketStatic {
    pub trait_name: crate::name::FqTraitName,
    /// The receiver parameter as written (`T`) — what the static-method
    /// indices key on.
    pub param: String,
    pub binder: FqTypeName,
    pub module: ModuleSource,
    pub def: crate::defs::DefId,
}

pub(super) struct MethodSignatureFacts {
    pub param_is_mut: Vec<bool>,
    pub param_names: Vec<String>,
    pub param_defaults: Vec<Option<ast::Expr>>,
    pub param_types: Vec<TypeId>,
    pub self_kind: ast::SelfKind,
}

impl MethodCallOutcome {
    fn no_dispatch(type_id: TypeId) -> Self {
        Self {
            type_id,
            dispatch: None,
            signature: None,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Check for IndexMut desugaring: container[i].method() where method needs &mut self
        // We need to detect this BEFORE resolving the receiver, because resolve_index
        // would otherwise generate Index::index instead of IndexMut::index_mut
        if let ast::Expr::Index(index_expr) = &method_call.receiver
            && let Some(result) =
                self.try_resolve_index_mut_method_call(index_expr, method_call, ctx)
        {
            return result;
        }

        let receiver = self.resolve_expr(&method_call.receiver, ctx, None);

        // A `_` resolves to UNKNOWN here; its position is recorded in the hole
        // mask below so the dispatch fills it from inference.
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        // Build the mask only for the `_` case; an empty vec (no allocation)
        // marks "no holes" for the fully-explicit common path.
        let type_arg_holes = if super::call::turbofish_has_hole(&method_call.type_args) {
            super::call::turbofish_holes(&method_call.type_args)
        } else {
            Vec::new()
        };

        self.resolve_method_call_with(
            MethodCallInput {
                receiver,
                receiver_ast: Some(&method_call.receiver),
                method_name: &method_call.method,
                method_id: Some(method_call.method_id),
                call_id: Some(method_call.id),
                type_args,
                type_arg_holes,
                args: &method_call.args,
                expected_type,
                span: method_call.span,
                required_trait: None,
            },
            ctx,
        )
        .type_id
    }

    /// Dispatch a method call from an already-resolved receiver TIR. See
    /// [`MethodCallInput`] for the contract.
    pub(super) fn resolve_method_call_with(
        &mut self,
        input: MethodCallInput<'_>,
        ctx: &mut FunctionContext,
    ) -> MethodCallOutcome {
        let MethodCallInput {
            mut receiver,
            receiver_ast,
            method_name,
            method_id,
            call_id,
            type_args,
            type_arg_holes,
            args: args_ast,
            expected_type,
            span,
            required_trait,
        } = input;
        // A qualified call names one trait, so the inherent-method step — a
        // different namespace — is skipped; only that trait's impls may
        // answer. The ref-impl priority step still runs (with the filter):
        // it is trait-impl lookup too, and skipping it would send
        // `IntoIterator::into_iter(&list)` to the base type's impl where
        // `(&list).into_iter()` selects `impl IntoIterator for &List<T>`.
        let required_trait = required_trait.as_ref();
        // NOTE: args are resolved later (after method lookup) to enable literal coercion
        // using the method's parameter types as expected types.

        // The handle argument-directed selection classifies through (WEP
        // 2026-07-31). Constructing it costs nothing: a class is synthesized
        // only if the candidate set turns out to be an overload set.
        let mut probe = super::synth::ArgProbe::new(args_ast, ctx);

        // Base (non-ref) type for method lookup. `mut`: deferred-inference may
        // concretise the receiver below.
        let mut base_type_id = self.tysys.get_base_type(receiver);

        // Get struct name and module source from base type
        // The struct_module is where the struct is defined (and inherent methods live)
        let (struct_name, struct_module) = match self.tysys.type_table.borrow().get(base_type_id) {
            ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => self
                .tysys
                .type_table
                .borrow()
                .nominal_head(base_type_id)
                .expect("a nominal type names a declaration"),
            // Primitive types have impl blocks in core:prelude/primitive
            ResolvedType::Primitive(_) => (
                self.tysys
                    .type_table
                    .borrow()
                    .mangle_type_name(base_type_id),
                ModuleSource::primitive(),
            ),
            // Unit type () has impl blocks in core:prelude/primitive
            ResolvedType::Unit => (
                TypeTable::UNIT_TYPE_NAME.to_string(),
                ModuleSource::primitive(),
            ),
            // Enum types - use enum name and its defining module
            // Enum, generic resource, newtype and flags are all named by the
            // declaration they carry.
            ResolvedType::Enum { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. } => self
                .tysys
                .type_table
                .borrow()
                .nominal_head(base_type_id)
                .expect("a nominal type names a declaration"),
            // Raw GC array `Array<T>`: inherent methods live in
            // `impl Array<T>` (core:prelude/array.wado), keyed by "Array".
            ResolvedType::BuiltinArray(_) => (
                TypeTable::ARRAY_TYPE_NAME.to_string(),
                ModuleSource::array(),
            ),
            _ => (
                self.tysys
                    .type_table
                    .borrow()
                    .mangle_type_name(base_type_id),
                self.current_module_source.clone(),
            ),
        };

        // Extract receiver type args for generic types (used for resolving associated types)
        let type_args_source_id = {
            let tt = self.tysys.type_table.borrow();
            if matches!(tt.get(base_type_id), ResolvedType::Newtype { .. }) {
                tt.representation_head(base_type_id)
            } else {
                base_type_id
            }
        };
        let receiver_type_args_for_trait: Option<Vec<TypeId>> = match self
            .tysys
            .type_table
            .borrow()
            .get(type_args_source_id)
            .clone()
        {
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. }
                if !type_args.is_empty() =>
            {
                Some(type_args)
            }
            // The raw GC array `Array<T>` carries its element as a single
            // type arg, so a trait method's associated types (e.g.
            // `IntoIterator::Iter` / `Item` for `impl IntoIterator for
            // Array<T>`) resolve against `[elem]` just like a generic
            // container's.
            ResolvedType::BuiltinArray(elem) => Some(vec![elem]),
            _ => None,
        };

        let mut method_info: Option<MethodInfo> = None;
        let mut trait_name: Option<crate::name::FqTraitName> = None;
        let mut trait_impl_module_source: Option<ModuleSource> = None;
        let mut blanket_type_param: Option<String> = None;
        let mut blanket_binder: Option<FqTypeName> = None;
        let mut trait_impl_struct_name: Option<FqTypeName> = None;
        let mut matched_impl_struct_name: Option<String> = None;
        // `Some` when the ref-priority path below adopts a `&T` / `&mut T` impl,
        // so `base_struct_name` (then `"&"` / `"&mut"`) keys back to its typed
        // `Receiver::Ref` without re-inspecting the string.
        let mut matched_ref_kind: Option<RefKind> = None;

        // If receiver is a reference type, try ref-type trait impls first.
        // e.g., impl IntoIterator for &List<T> takes priority over impl IntoIterator for List<T>.
        // Only specific ref impls are preferred (not blanket impls like impl Inspect for &T).
        {
            let is_ref = matches!(
                self.tysys.type_table.borrow().get(receiver),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            );
            if is_ref {
                let ref_kind =
                    RefKind::from_resolved(&self.tysys.type_table.borrow().get(receiver).clone())
                        .expect("ref classify");
                let result = self.find_trait_method_for_type(
                    &ImplTargetKey::Ref(ref_kind),
                    method_name,
                    receiver_type_args_for_trait.as_deref(),
                    Some(base_type_id),
                    span,
                    required_trait,
                    Some(&mut probe),
                );
                // Only use ref-type impls that target a concrete container type
                // (e.g., impl IntoIterator for &List<T>), NOT blanket ref impls
                // (e.g., impl Inspect for &T where the inner type is just a type param).
                if let Some(trait_match) = result
                    && !trait_match.is_blanket_ref_impl
                {
                    matched_impl_struct_name = Some(trait_match.impl_struct_name.clone());
                    trait_impl_struct_name = Some(trait_match.impl_struct_fq);
                    matched_ref_kind = Some(ref_kind);
                    trait_name = Some(trait_match.trait_name);
                    let mut info = trait_match.method_info;
                    info.is_ref_impl = true;
                    method_info = Some(info);
                    trait_impl_module_source = Some(trait_match.impl_module_source);
                    blanket_type_param = trait_match.blanket_type_param;
                    blanket_binder = trait_match.blanket_binder;
                }
            }
        }

        // Reachable through both the `extends` chain and a trait impl: picking
        // either side rebinds call sites when the other grows the name. Asked
        // of the receiver alone, so no resolution order can hide one of them —
        // a `&T` impl resolves first, and is keyed by its reference kind
        // rather than by the receiver's declaration, so both keys are asked.
        // The qualified forms have returned above.
        let colliding_trait = |this: &Self| {
            let value_key =
                this.impl_target_of(base_type_id, &crate::name::DeclName::new(&struct_name));
            this.trait_impl_declaring(&value_key, method_name)
                .or_else(|| {
                    let kind = RefKind::from_resolved(
                        &this.tysys.type_table.borrow().get(receiver).clone(),
                    )?;
                    this.trait_impl_declaring(&ImplTargetKey::Ref(kind), method_name)
                })
        };
        if required_trait.is_none()
            && let Some(def) = self.tysys.type_table.borrow().nominal_def(base_type_id)
            && self
                .tysys
                .type_table
                .borrow()
                .is_extern_handle_resource(def)
            && let Some((declaring, _)) = self.resource_instance_method(def, method_name)
            && let Some(trait_name) = colliding_trait(self)
        {
            let _ = self.emit(TypeError::AmbiguousResourceMethod {
                method: method_name.to_string(),
                resource: self.tysys.resolutions.defs().name(declaring).to_string(),
                trait_name,
                span,
            });
        }

        // Look up method info based on receiver type (inherent + base type trait methods)
        if method_info.is_none() && required_trait.is_none() {
            method_info = self.lookup_method_info(receiver, method_name);
        }

        // Fall back to base type trait methods
        if method_info.is_none()
            && let Some(trait_match) = self.find_trait_method_for_type(
                &self.impl_target_of(base_type_id, &crate::name::DeclName::new(&struct_name)),
                method_name,
                receiver_type_args_for_trait.as_deref(),
                Some(base_type_id),
                span,
                required_trait,
                Some(&mut probe),
            )
        {
            matched_impl_struct_name = Some(trait_match.impl_struct_name.clone());
            if trait_match.impl_struct_name != struct_name {
                trait_impl_struct_name = Some(trait_match.impl_struct_fq);
            }
            trait_name = Some(trait_match.trait_name);
            method_info = Some(trait_match.method_info);
            trait_impl_module_source = Some(trait_match.impl_module_source);
            blanket_type_param = trait_match.blanket_type_param;
            blanket_binder = trait_match.blanket_binder;
        }

        // Selection is over; the classes come out of the probe so the arguments
        // can be elaborated (which needs `ctx` mutably) and then checked.
        let synthesized = probe.take_classes();

        // If still not found and receiver is a TypeParam, try trait bounds
        // e.g., T: Ord -> look up cmp() in Ord trait declaration
        if method_info.is_none() {
            let type_param_name = {
                let resolved = self.tysys.type_table.borrow().get(base_type_id).clone();
                if let ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } =
                    resolved
                {
                    Some(name)
                } else {
                    None
                }
            };
            if let Some(name) = type_param_name
                && let Some(bounds) = self
                    .annotate_ctx
                    .trait_ctx
                    .type_param_bounds
                    .get(&name)
                    .cloned()
                && let Some((found_trait, info)) = self.find_method_in_trait_bounds(
                    &bounds,
                    method_name,
                    base_type_id,
                    span,
                    required_trait,
                )
            {
                trait_name = Some(found_trait);
                method_info = Some(info);
            }
        }

        // If still not found and receiver is an AssocTypeProjection, try its bounds
        // e.g., S::SeqSerializer: SerializeSeq -> look up element() in SerializeSeq
        if method_info.is_none() {
            let assoc_bounds = {
                let resolved = self.tysys.type_table.borrow().get(base_type_id).clone();
                if let ResolvedType::AssocTypeProjection { bounds, .. } = resolved {
                    if bounds.is_empty() {
                        None
                    } else {
                        Some(bounds)
                    }
                } else {
                    None
                }
            };
            if let Some(bounds) = assoc_bounds
                && let Some((found_trait, info)) = {
                    // A projection carries its bounds as identities, answered
                    // where the trait declaration wrote them. The `ast` bounds
                    // rebuilt here are spellings for the by-name lookups only;
                    // which trait each means comes from `resolved`.
                    let named: Vec<crate::name::FqTraitName> = bounds.into_iter().collect();
                    // Each rebuilt bound is paired with the identity it stands
                    // for by its own id, not by its name: two same-named traits
                    // from different modules are two bounds, and a by-name map
                    // would collapse them back into one.
                    let mut resolved: crate::hashmap::IndexMap<
                        crate::ast::AstId,
                        crate::name::FqTraitName,
                    > = crate::hashmap::IndexMap::default();
                    let bounds: Vec<ast::TraitBound> = named
                        .iter()
                        .map(|b| {
                            let id = crate::ast::AstId::fresh();
                            resolved.insert(id, b.clone());
                            ast::TraitBound {
                                id,
                                name: b.base_name().to_string(),
                                assoc_types: Vec::new(),
                                span,
                                fn_signature: None,
                                // The referent this bound was rebuilt from.
                                // Recorded on the bound, so nothing has to
                                // resolve `name` at an id the walk never saw.
                                resolved: b.canonical(),
                            }
                        })
                        .collect();
                    self.find_method_in_trait_bounds_with(
                        &bounds,
                        &resolved,
                        method_name,
                        base_type_id,
                        span,
                        required_trait,
                    )
                }
            {
                trait_name = Some(found_trait);
                method_info = Some(info);
            }
        }

        // Get method info (error if method not found)
        // Track whether the lookup actually found a real method. The
        // error-recovery branch below fabricates a placeholder MethodInfo
        // so resolution can continue past a diagnostic, but the
        // FunctionRef we then build mangles a non-existent method
        // against the receiver's struct module — we MUST NOT record that
        // as a successful dispatch in `sem.types.method_dispatch`, or
        // reify would try to lower a call to a function that does not exist.
        let method_found = method_info.is_some();
        let MethodInfo {
            method_def: dispatched_method_def,
            mut return_type,
            self_kind,
            param_types,
            param_is_mut: _,
            owner,
            cm_name,
            is_ref_impl,
            method_type_param_ids,
            method_own_params,
            impl_module: inherent_impl_module,
            from_concrete_impl,
            param_defaults,
            param_names,
            consumes_self,
            inherent_visibility,
        } = if let Some(info) = method_info {
            info
        } else {
            let type_name = self.tysys.type_table.borrow().type_name(base_type_id);
            let _ = self.emit(TypeError::MethodNotFound {
                type_name,
                method_name: method_name.to_string(),
                hint: String::new(),
                span,
            });
            // Default to Unknown type for error recovery
            MethodInfo {
                method_def: None,
                return_type: TypeTable::UNKNOWN,
                self_kind: ast::SelfKind::Ref,
                param_types: vec![],
                param_is_mut: vec![],
                owner: MethodOwner::Receiver,
                cm_name: None,
                is_ref_impl: false,
                method_type_param_ids: vec![],
                method_own_params: vec![],
                impl_module: None,
                from_concrete_impl: false,
                param_defaults: vec![],
                param_names: vec![],
                consumes_self: false,
                inherent_visibility: None,
            }
        };

        self.check_inherent_member_visibility(
            inherent_visibility,
            inherent_impl_module.as_ref(),
            super::expr::MemberOwner::Type(base_type_id),
            method_name,
            super::types::ImplMemberKind::Method,
            call_id,
            span,
        );

        // `Tuple.len()` needs no call: reify folds it to a literal, or leaves
        // the fold to monomorphization when a `..T` pack makes the arity
        // unknown. Either way the type is the same.
        if method_name == "len" && self.tysys.type_table.borrow().is_tuple(base_type_id) {
            return MethodCallOutcome::no_dispatch(TypeTable::I32);
        }

        // `Tuple.zip()` transposes a tuple-of-tuples,
        // `[[A0, A1], [B0, B1]]` → `[[A0, B0], [A1, B1]]`. Reify expands it,
        // or leaves the expansion to monomorphization when a `..T` pack is
        // present; `return_type` already says what it yields.
        if method_name == "zip" && self.tysys.type_table.borrow().is_tuple(base_type_id) {
            return MethodCallOutcome::no_dispatch(return_type);
        }

        // Static methods (no self parameter) cannot be called with instance method syntax.
        // e.g., `obj.static_method()` should be `Type::static_method()` instead.
        if self_kind == ast::SelfKind::None {
            let type_name = self.tysys.type_table.borrow().type_name(base_type_id);
            let _ = self.emit(TypeError::MethodNotFound {
                type_name: type_name.clone(),
                method_name: method_name.to_string(),
                hint: format!(
                    "'{method_name}' is a static method; use {type_name}::{method_name}() instead"
                ),
                span,
            });
            return MethodCallOutcome::no_dispatch(TypeTable::ERROR);
        }

        // Type check method arguments against expected parameter types (newtype-aware)
        // If method was inherited from a newtype's base type, substitute base->newtype in params
        let expected_param_types: Vec<TypeId> = if let Some(base_type_id) = owner.newtype_base() {
            // Get the newtype that the method is being called on
            let newtype_id = self.tysys.get_base_type(receiver);
            // Substitute base type with newtype in all parameter types
            param_types
                .iter()
                .map(|&ty| {
                    self.tysys
                        .substitute_newtype_in_type(ty, base_type_id, newtype_id)
                })
                .collect()
        } else {
            param_types
        };

        // Resolve arguments with coercion using method parameter types
        let mut args: Vec<TypeId> = args_ast
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = expected_param_types.get(i).copied();
                self.resolve_expr(arg, ctx, expected_type)
            })
            .collect();

        // The module that declares this method: the scope its own defaults —
        // parameter values and type-parameter defaults alike — resolve in,
        // since a default may name a type the call site cannot (WEP
        // 2026-04-11). The chain `method_module_source` takes below, without
        // its inherited-owner steps.
        let callee_module = trait_impl_module_source
            .clone()
            .or_else(|| inherent_impl_module.clone())
            .unwrap_or_else(|| struct_module.clone());

        // Pad missing trailing args with declared parameter defaults.
        // Earlier-parameter references inside a default (e.g. `fn f(w, h = w)`)
        // are handled by substituting the caller's arg ASTs for those parameter
        // names before resolving, mirroring the free-function path in
        // `pad_args_with_defaults`.
        if args.len() < expected_param_types.len() && !param_defaults.is_empty() {
            let mut subs: crate::hashmap::IndexMap<String, ast::Expr> =
                crate::hashmap::IndexMap::default();
            for (i, arg_ast) in args_ast.iter().enumerate() {
                if let Some(name) = param_names.get(i) {
                    subs.insert(name.clone(), arg_ast.clone());
                }
            }
            self.with_default_scope_module(Some(callee_module.clone()), |s| {
                for i in args.len()..expected_param_types.len() {
                    let Some(Some(default_ast)) = param_defaults.get(i) else {
                        break;
                    };
                    let expected_type = expected_param_types[i];
                    let mut default_expr = default_ast.clone();
                    let vantage = Some((callee_module.clone(), default_expr.id().space()));
                    default_expr.substitute_idents(&subs);
                    let resolved = s.with_foreign_vantage(vantage, |s| {
                        s.resolve_expr(&default_expr, ctx, Some(expected_type))
                    });
                    args.push(resolved);
                    if let Some(name) = param_names.get(i) {
                        subs.insert(name.clone(), default_expr);
                    }
                }
            });
        }

        // Arity, once the declared defaults have filled what they can. A
        // defaulted parameter is optional and the rest are required; the
        // receiver is neither, so `args` and `expected_param_types` count the
        // same list.
        //
        // Here rather than beside the per-argument check below, which waits for
        // inference: a call of the wrong length has no operand list to infer
        // from, and reaches codegen as an invalid module. `method_found`
        // guards it because the recovery `MethodInfo` above declares no
        // parameters, and "expected 0 arguments" is not "no method of that
        // name".
        let optional = param_defaults.iter().filter(|d| d.is_some()).count();
        let required = expected_param_types.len().saturating_sub(optional);
        if method_found && (args.len() < required || args.len() > expected_param_types.len()) {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: expected_param_types.len(),
                found: args.len(),
                span,
            });
            return MethodCallOutcome::no_dispatch(TypeTable::ERROR);
        }

        // Pin a deferred hole that rode a prior binding into an argument
        // (`let v = gen()?; out.push(v)`) against the parameter type.
        //
        // Argument *types* are not checked here: the parameter types still name
        // the method's own slots, which are opaque until inference — which needs
        // these argument types — has run. That check happens once below,
        // against the substituted parameter types.
        for (arg, &expected_type) in args.iter_mut().zip(expected_param_types.iter()) {
            if self.type_has_infer_hole(*arg) && self.hole_pinnable_against(expected_type) {
                self.solve_infer_holes_against(*arg, expected_type);
                *arg = self.apply_infer_holes(*arg);
            }
        }

        self.verify_arg_synthesis(&synthesized, args_ast, ctx, &args, span);

        // Substitute return type for inherited newtype methods
        // e.g., Point::clone_point() -> Point becomes Location::clone_point() -> Location
        if let Some(base_type_id) = owner.newtype_base() {
            let newtype_id = self.tysys.get_base_type(receiver);
            return_type =
                self.tysys
                    .substitute_newtype_in_type(return_type, base_type_id, newtype_id);
        }

        // Address-taken tracking for an implicit `&mut self` borrow on a
        // primitive local receiver is owned by reify (`reify.rs` method-call
        // arm marks `address_taken_locals` on the TIR it emits); the body walk
        // has no node to mark, since `resolve_ident` answers with a type.

        if self_kind == ast::SelfKind::MutRef && !is_ref_impl {
            self.check_mut_receiver(receiver, receiver_ast, method_name, span, ctx);
        }

        receiver = super::method_lookup::adjusted_receiver_type(
            receiver,
            self_kind,
            is_ref_impl,
            &self.tysys.type_table,
        );

        let mut subst_ctx = SubstitutionContext::new();

        // Inference runs when the turbofish is omitted entirely or carries an
        // explicit `_` placeholder; in the latter case the inferred holes are
        // merged into the explicit args, which always win.
        let has_hole = type_arg_holes.iter().any(|&h| h);
        let method_type_args = if type_args.is_empty() || has_hole {
            let inferred = self.infer_method_type_args(MethodInferenceInput {
                receiver_type: receiver,
                method_name,
                slots: &method_type_param_ids,
                own_params: &method_own_params,
                param_types: &expected_param_types,
                args: &args,
                raw_args: args_ast,
                decl_return_type: return_type,
                expected_return_type: expected_type,
                trait_decl: trait_name
                    .as_ref()
                    .and_then(crate::name::FqTraitName::canonical),
                declaring_module: Some(callee_module),
                span,
            });
            if type_args.is_empty() {
                inferred
            } else {
                let mut merged = type_args;
                super::call::merge_turbofish_type_args(&mut merged, &type_arg_holes, &inferred);
                merged
            }
        } else {
            type_args
        };

        if !method_type_args.is_empty() {
            // The lookup already instantiated the declaring level, so only the
            // method's own parameters remain — and it reports them.
            subst_ctx = subst_ctx.bind(&method_type_param_ids, &method_type_args);
            // Enforce the method's type-arg bounds (shared rule); a violating
            // concrete arg would otherwise trap WIR build. Hole args are
            // skipped and re-checked in `finalize_infer_holes`. The parameters
            // come from the signature dispatch chose, so the explicit-turbofish
            // path checks against the same declaration inference would have.
            self.enforce_type_arg_bounds(&method_own_params, &method_type_args, span);
        }

        // Apply unified substitution
        if !subst_ctx.is_empty() {
            return_type =
                subst_ctx.substitute(return_type, &mut self.tysys.type_table.borrow_mut());
        }

        // Deferred-inference solve point: a hole that flowed in from an
        // uninferred generic receiver (`p.get()` in `p.get().unwrap()`) is
        // solved against this call's expected type and concretised *before* the
        // mangling/recording below embeds the receiver type in a name a later
        // TypeId sweep could not fix.
        if let Some(expected) = expected_type
            && (self.type_has_infer_hole(return_type) || self.type_has_infer_hole(receiver))
        {
            self.solve_infer_holes_against(return_type, expected);
            receiver = self.apply_infer_holes(receiver);
            return_type = self.apply_infer_holes(return_type);
            base_type_id = self.tysys.get_base_type(receiver);
        }
        // A hole may still ride the receiver (a deep chain's intermediate call,
        // `gen().keep().unwrap()`): the recorded name embeds `Type<?hole>`, but
        // the monomorphizer rebuilds names from the receiver type, which the
        // module-end sweep concretises once the hole is solved further out.

        // The one place arguments are checked: against the parameter types with
        // this call's type arguments substituted in. Doing it here rather than
        // before inference is what lets `h.two_method<T>(1 as i64, 2 as i32)`
        // report that `T` cannot be both — and what keeps a generic method's
        // own slots, opaque until solved, out of the comparison.
        let substituted_param_types: Vec<TypeId> = if method_type_args.is_empty() {
            expected_param_types.clone()
        } else {
            expected_param_types
                .iter()
                .map(|&t| subst_ctx.substitute(t, &mut self.tysys.type_table.borrow_mut()))
                .collect()
        };
        if !method_type_args.is_empty() {
            self.recoerce_literal_args(args_ast, &mut args, &substituted_param_types);
        }
        for (i, arg) in args.iter().enumerate() {
            if let Some(&expected) = substituted_param_types.get(i) {
                self.typecheck(
                    *arg,
                    expected,
                    args_ast.get(i).map_or(span, super::ast::Expr::span),
                );
            }
        }

        // Get struct name and monomorph info from base type for mangled method name.
        // For inherited methods (Newtype/Flags), use the actual implementation type's name,
        // since the function is defined on the base type (e.g., Point::sum, not Location::sum).
        let method_impl_type_id = owner.declaring(base_type_id);
        let (
            mut receiver_struct_name,
            mut base_struct_name,
            impl_type_arg_names,
            receiver_type_args,
        ) = match self
            .tysys
            .type_table
            .borrow()
            .get(method_impl_type_id)
            .clone()
        {
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
                let (name, _module_source) = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(method_impl_type_id)
                    .expect("a nominal type names a declaration");
                // Qualify the base and the arguments alike, so a concrete-generic
                // impl's method name matches its definition (issue #1348). A
                // tuple carries the tuple head, not a declared one, so it keeps
                // the `[a,b]` spelling every other namespace gives it.
                let type_arg_names: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                    .collect();
                let base = if TypeTable::is_tuple_type(&name) {
                    FqTypeName::tuple(Vec::new())
                } else {
                    self.tysys
                        .type_table
                        .borrow()
                        .fq_base_type_name(method_impl_type_id)
                };
                let mangled = base.clone().with_args(type_arg_names.clone());
                (mangled, base, type_arg_names, Some(type_args))
            }
            // The raw GC array splits like a generic instance: the receiver
            // name is the full `Array<T>` spelling, but the method-owner base
            // name is "Array" (matching `impl Array<T>`'s registration).
            ResolvedType::BuiltinArray(elem) => {
                let arg_name = self.tysys.type_table.borrow().fq_type_name(elem);
                let base = self.qualified_receiver_name(TypeTable::ARRAY_TYPE_NAME);
                let mangled = base.clone().with_args(vec![arg_name.clone()]);
                (mangled, base, vec![arg_name], Some(vec![elem]))
            }
            // Named by its declaring module: a bare head names no definition,
            // and re-resolution would peel past the impl to the base.
            ResolvedType::Newtype { def, .. }
                if matched_impl_struct_name.as_deref()
                    == Some(self.tysys.type_table.borrow().def_name(def)) =>
            {
                let base = self
                    .tysys
                    .type_table
                    .borrow()
                    .fq_base_type_name(method_impl_type_id);
                (base.clone(), base, vec![], None)
            }
            // A generic newtype's instantiation carries its arguments beside
            // the head, so the impl index gets the head an `impl` header writes.
            ResolvedType::Newtype {
                type_args: newtype_args,
                ..
            } if !newtype_args.is_empty() => {
                let (_name, _module_source) = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(method_impl_type_id)
                    .expect("a newtype names a declaration");
                // Not the base's: a base may re-shape them, and the `impl`
                // header names the newtype.
                let type_args = newtype_args;
                let head = self
                    .tysys
                    .type_table
                    .borrow()
                    .fq_base_type_name(method_impl_type_id);
                let type_arg_names: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                    .collect();
                let mangled = head.clone().with_args(type_arg_names.clone());
                (mangled, head, type_arg_names, Some(type_args))
            }
            _ => {
                let name = self
                    .tysys
                    .type_table
                    .borrow()
                    .fq_type_name(method_impl_type_id);
                let head = name.head_only();
                (name, head, vec![], None)
            }
        };

        // For trait methods found through the newtype chain, override with the actual impl struct.
        // E.g., `loc.describe()` where `loc: Location`, `impl Describable for Point` →
        // use "Point" so the call resolves to "Point^Describable::describe".
        if let Some(impl_name) = trait_impl_struct_name {
            receiver_struct_name.clone_from(&impl_name);
            base_struct_name = impl_name;
        }

        let mangled_method_name =
            MethodName::format_local(&receiver_struct_name, trait_name.as_ref(), method_name);

        // Build monomorph_info for method calls on generic types or with method type args
        let monomorph_info = if from_concrete_impl {
            // A method from a concrete instantiation impl (`impl List<u8>`) is a
            // per-instantiation concrete function (`List<u8>::method`), not an
            // impl-level template. If the method has NO type params of its own,
            // it is fully concrete — call it directly (no monomorph_info) so
            // cross-module inclusion / DCE / WIR resolution handle it and
            // distinct instantiations stay distinct. If it DOES have method-level
            // type params (`impl List<u8> { fn f<T>() }`), it still needs
            // monomorphization over those — keyed by the per-instantiation
            // template name with NO impl type args (the receiver is concrete).
            if method_type_args.is_empty() {
                None
            } else {
                let generic_name = MethodName::format_local(
                    &receiver_struct_name,
                    trait_name.as_ref(),
                    method_name,
                );
                Some(MonomorphInfo {
                    generic_name,
                    impl_type_args: vec![],
                    method_type_args: method_type_args.clone(),
                    is_blanket: false,
                })
            }
        } else if let Some(ref blanket_param) = blanket_type_param {
            // For blanket impls, the template function uses the type param name (e.g., "I").
            // The call site uses the concrete receiver (e.g., "ListIter<i32>").
            // monomorph_info maps from the concrete name back to the template.
            let binder = blanket_binder.unwrap_or_else(|| FqTypeName::binder(blanket_param));
            let generic_name = MethodName::format_local(&binder, trait_name.as_ref(), method_name);
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: vec![base_type_id],
                method_type_args: method_type_args.clone(),
                is_blanket: true,
            })
        } else if receiver_type_args.is_some() || !method_type_args.is_empty() {
            let generic_name = MethodName::format_local(&base_struct_name, None, method_name);
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: receiver_type_args.unwrap_or_default(),
                method_type_args: method_type_args.clone(),
                is_blanket: false,
            })
        } else {
            None
        };

        // Convert method type args to string names for method_info
        // Use inferred type args if available, otherwise use explicit type args
        let method_type_arg_names: Vec<FqTypeName> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
            .collect();

        // Build method_info with base struct name, then apply impl and method type args
        let is_type_param_receiver = matches!(
            self.tysys.type_table.borrow().get(base_type_id),
            ResolvedType::TypeParam { .. }
                | ResolvedType::TypePack { .. }
                | ResolvedType::AssocTypeProjection { .. }
        );
        let base_receiver = match matched_ref_kind {
            Some(kind) => Receiver::Ref(kind),
            None => Receiver::Type(base_struct_name.clone()),
        };
        let base_target = match matched_ref_kind {
            Some(kind) => ImplTargetKey::Ref(kind),
            // `impl_target_of` falls back on a written name, so hand it the
            // declaration name rather than the module-qualified head.
            None => self.impl_target_of(base_type_id, &base_struct_name.decl_name()),
        };
        let param_is_mut = self.lookup_method_param_is_mut(&base_target, method_name);
        let mut method_info =
            LocalMethodName::of(base_receiver, trait_name, method_name.to_string())
                .with_type_args(&impl_type_arg_names, &method_type_arg_names);
        method_info.is_type_param_receiver = is_type_param_receiver;
        method_info.is_ref_impl = is_ref_impl;
        method_info.cm_name = cm_name;

        // `module_source` is the body's home module. The body lives:
        //   1. In the trait-impl block's module for cross-module trait impls
        //      (e.g. `impl Display for String` in `core:prelude/format`).
        //   2. In the declaring type's module when the method was inherited —
        //      through a newtype (`type MyArray<T> = List<T>`; `arr.len()`
        //      reaches `List::len` in `core:prelude/array`, not the newtype's
        //      module) or through an `extends` chain.
        //   3. In the receiver type's module otherwise — inherent methods
        //      live alongside the type they're declared on.
        let method_module_source = trait_impl_module_source
            // Inherent methods: the body lives in the module that declares the
            // `impl` block, which may differ from the receiver type's module
            // (a user-written `impl List<u8>` on the prelude `List`). Prefer
            // that so cross-module inherent impls resolve.
            .or_else(|| inherent_impl_module.clone())
            .or_else(|| {
                owner.inherited().and_then(|base_id| {
                    match self.tysys.type_table.borrow().get(base_id) {
                        ResolvedType::Struct { .. }
                        | ResolvedType::GenericInstance { .. }
                        | ResolvedType::Enum { .. }
                        | ResolvedType::Variant { .. }
                        | ResolvedType::Newtype { .. }
                        | ResolvedType::Flags { .. }
                        | ResolvedType::Resource { .. }
                        | ResolvedType::GenericResource { .. } => self
                            .tysys
                            .type_table
                            .borrow()
                            .nominal_head(base_id)
                            .map(|(_, m)| m),
                        ResolvedType::Primitive(_) | ResolvedType::Unit => {
                            Some(ModuleSource::primitive())
                        }
                        ResolvedType::BuiltinArray(_) => Some(ModuleSource::array()),
                        _ => None,
                    }
                })
            })
            .unwrap_or_else(|| struct_module.clone());

        // Record use->def for jump-to-definition on the method name token.
        // Synthetic call sites (e.g. for-of's `.into_iter()` / `.next()`) pass
        // `method_id == None` so no edge is recorded — the call has no
        // source-level method name to navigate from.
        // The target is the declaration dispatch selected, carried on its
        // signature. A name scan cannot stand in: two impls on one type can
        // declare the same method, and only dispatch knows which answered.
        if let (Some(method_id), Some(def)) = (method_id, dispatched_method_def) {
            self.record_reference_to_decl(method_id, def);
        }

        let func = FunctionRef {
            module_source: method_module_source,
            name: mangled_method_name,
            monomorph_info,
            method_info: Some(method_info),
        };

        // Record the dispatch decision so reify can emit the same TIR without
        // re-running trait lookup or mangling. Skipped for a synthetic call, for
        // the short-circuits that returned above, and on the error-recovery path.
        // Only a trait-qualified caller reads the signature facts back, so an
        // ordinary call skips the four vector clones and their default ASTs.
        let signature = (method_found && required_trait.is_some()).then(|| MethodSignatureFacts {
            param_is_mut: param_is_mut.clone(),
            param_names: param_names.clone(),
            param_defaults: param_defaults.clone(),
            param_types: expected_param_types.clone(),
            self_kind,
        });
        let dispatch = if method_found {
            self.record_method_dispatch(
                call_id,
                dispatched_method_def,
                &func,
                self_kind,
                is_ref_impl,
                param_is_mut,
                param_names,
                param_defaults,
                return_type,
                method_type_args,
                consumes_self,
            );
            // The `method_found` gate keeps the error-recovery placeholder
            // from leaking into the returned dispatch.
            Some(DispatchedMethod {
                self_kind,
                is_ref_impl,
                func,
                method_def: dispatched_method_def,
            })
        } else {
            None
        };

        MethodCallOutcome {
            type_id: return_type,
            dispatch,
            signature,
        }
    }

    /// Whether `Trait::method` names a trait's instance method, making a call
    /// on it the trait-qualified (UFCS) form `Trait::method(recv, args…)`
    /// (WEP 2026-07-31). A trait's *static* method is not included: it has no
    /// receiver argument to bind `Self` from.
    pub(super) fn is_trait_instance_method(&self, trait_name: &str, method_name: &str) -> bool {
        self.decl_key_or_local(trait_name).is_some_and(|key| {
            self.tysys.trait_env.declares_trait(&key)
                && self
                    .trait_sig_of(&key)
                    .and_then(|sig| sig.method(method_name))
                    .is_some_and(|m| m.sig.self_kind != ast::SelfKind::None)
        })
    }

    /// `Trait::method(recv, args…)` — the receiver is the first argument, so
    /// dispatch is the ordinary method-call path with the named trait as a
    /// constraint on which impl may answer.
    pub(super) fn resolve_trait_qualified_call(
        &mut self,
        head_site: Option<crate::ast::AstId>,
        trait_name: &str,
        method_name: &str,
        call: &ast::CallExpr,
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let required = super::types::RequiredTrait {
            // The site's own answer. A prefix naming a type-parameter binder
            // is a `Binder`, which matches no trait declaration — the same
            // outcome the fabricated key produced, said rather than simulated.
            decl: head_site.map_or(crate::resolve::Resolution::Unresolved, |site| {
                self.tysys.resolutions.get(site)
            }),
            args: None,
            display: self.declared_trait_name(trait_name),
        };
        let type_args: Vec<TypeId> = call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        // The edge for jump-to-definition is recorded against the method name.
        let method_id = match &call.callee {
            ast::Expr::Ident(ident) => ident.segments.get(1).map(|seg| seg.id),
            _ => None,
        };
        self.resolve_trait_qualified_call_parts(
            required,
            method_name,
            &call.args,
            type_args,
            call.id,
            method_id,
            call.span,
            expected_type,
            ctx,
        )
    }

    /// The shared engine behind both qualified spellings: the bare
    /// `Trait::method(recv, …)` ident form, and the trait-turbofish
    /// `Take::<A>::take(recv, …)` static form whose `required_trait` carries
    /// the resolved trait arguments and thereby pins one argument list.
    #[allow(clippy::too_many_arguments)]
    fn resolve_trait_qualified_call_parts(
        &mut self,
        required_trait: super::types::RequiredTrait,
        method_name: &str,
        args: &[ast::Expr],
        type_args: Vec<TypeId>,
        call_id: AstId,
        // The method-name token, for the use→def edge.
        method_id: Option<AstId>,
        span: Span,
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let Some((receiver_ast, rest)) = args.split_first() else {
            let _ = self.emit(TypeError::TraitQualifiedCallNeedsReceiver {
                trait_name: required_trait.display,
                method: method_name.to_string(),
                span,
            });
            return TypeTable::ERROR;
        };
        let trait_display = required_trait.display.clone();
        let receiver_type = self.resolve_expr(receiver_ast, ctx, None);
        // `call_id: None` — the dispatcher would file the decision under
        // `method_dispatch`, which reify only reads for a `MethodCallExpr`
        // node. A qualified call spells its receiver's mode itself (`&x` for
        // `&self`), so no receiver adjustment is owed and the call is an
        // ordinary one whose first argument happens to be the receiver: the
        // decision is recorded as a static dispatch, which reify's `Call` arm
        // already replays.
        let outcome = self.resolve_method_call_with(
            MethodCallInput {
                receiver: receiver_type,
                receiver_ast: Some(receiver_ast),
                method_name,
                method_id,
                call_id: None,
                type_args: type_args.clone(),
                type_arg_holes: vec![],
                args: rest,
                expected_type,
                span,
                required_trait: Some(required_trait),
            },
            ctx,
        );
        if let Some(sig) = &outcome.signature {
            self.check_trait_qualified_receiver_mode(
                &trait_display,
                method_name,
                sig.self_kind,
                receiver_type,
                receiver_ast.span(),
            );
        }
        if let (Some(dispatched), Some(sig)) = (outcome.dispatch, outcome.signature) {
            let function_ref = dispatched.func;
            // The receiver occupies slot 0 of the static shape, so every
            // per-parameter list gains a leading entry for it. It is spelled at
            // the call site and never omitted, hence no default; it is `mut`
            // exactly when the method takes `&mut self`.
            let mut param_is_mut = vec![sig.self_kind == ast::SelfKind::MutRef];
            param_is_mut.extend(sig.param_is_mut);
            let mut param_defaults: Vec<(String, Option<ast::Expr>)> =
                vec![("self".to_string(), None)];
            param_defaults.extend(sig.param_names.into_iter().zip(sig.param_defaults));
            let mut param_types = vec![receiver_type];
            param_types.extend(sig.param_types);
            // An unannotated closure argument infers its parameter types from
            // this; without it the closure's functor is generated with
            // `unknown` params and dropped before codegen.
            self.record_call_param_types(call_id, param_types.clone());
            self.sem.types.static_method_dispatch.insert(
                call_id,
                super::sem::types::StaticMethodDispatch {
                    method_def: dispatched.method_def,
                    function_ref,
                    param_is_mut,
                    type_args,
                    param_defaults,
                    param_types,
                    self_in_args: true,
                },
            );
        }
        outcome.type_id
    }

    /// The receiver of a qualified call spells its own mode (WEP 2026-07-31);
    /// enforce that the spelling agrees with the method's `self` parameter.
    /// Without this, a by-value receiver against `&mut self` mutates a copy
    /// and silently drops the change. A `&mut` receiver still answers a
    /// `&self` method — the one reference coercion the language has.
    fn check_trait_qualified_receiver_mode(
        &mut self,
        trait_name: &str,
        method: &str,
        self_kind: ast::SelfKind,
        receiver_type: TypeId,
        span: Span,
    ) {
        if receiver_type == TypeTable::ERROR || receiver_type == TypeTable::UNKNOWN {
            return;
        }
        let resolved = self.tysys.type_table.borrow().get(receiver_type).clone();
        let is_ref = matches!(resolved, ResolvedType::Ref(_));
        let is_mut_ref = matches!(resolved, ResolvedType::MutRef(_));
        let (expected, spelled) = match self_kind {
            ast::SelfKind::Value if is_ref || is_mut_ref => ("self", "value"),
            ast::SelfKind::Ref if !(is_ref || is_mut_ref) => ("&self", "&value"),
            ast::SelfKind::MutRef if !is_mut_ref => ("&mut self", "&mut value"),
            _ => return,
        };
        let _ = self.emit(TypeError::TraitQualifiedReceiverMode {
            trait_name: trait_name.to_string(),
            method: method.to_string(),
            expected: expected.to_string(),
            spelled: spelled.to_string(),
            span,
        });
    }

    /// Resolve a static method call: `List::<i32>::with_capacity(100)` or `Point::origin()`
    pub(super) fn resolve_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        // A reflection trait is a trait, not a type, so `target_type` would not
        // resolve: intercept and route to `T`'s synthesized `T^Trait::method`.
        // It is the only spelling — a bare `T::members()` never resolves, so
        // type namespaces stay the author's.
        if let ast::Type::Generic(g) = &static_call.target_type
            && let Some(dispatch) = self.reflect_dispatch_of(&g.name, &static_call.method)
        {
            let [self_ty_ast] = g.args.as_slice() else {
                let _ = self.emit(TypeError::UnknownFunction {
                    name: format!(
                        "{}::<…>::{} (one subject type argument, found {})",
                        g.name,
                        static_call.method,
                        g.args.len()
                    ),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            };
            let self_ty = self.resolve_type(self_ty_ast);
            return match dispatch {
                ReflectDispatch::Root => {
                    self.resolve_reflect_root_static_call(self_ty, static_call, ctx)
                }
                ReflectDispatch::Struct => {
                    self.resolve_reflect_static_call(self_ty, static_call, ctx)
                }
                ReflectDispatch::Variant => {
                    self.resolve_reflect_variant_static_call(self_ty, static_call, ctx)
                }
                ReflectDispatch::Scalar(spec) => {
                    self.resolve_reflect_scalar_static_call(spec, self_ty, static_call, ctx)
                }
            };
        }

        // Resolve the target type first to get struct name for parameter type lookup
        let target_type_id = self.resolve_type(&static_call.target_type);

        // `Tag::<Point>::tag()` where `Tag` is a trait resolves to no type;
        // unreported it types `unknown` and lowering builds an invalid module.
        if target_type_id == TypeTable::UNKNOWN
            && let ast::Type::Generic(g) = &static_call.target_type
            && self
                .decl_key_at(g.id, &g.name)
                .is_some_and(|key| self.tysys.trait_env.declares_trait(&key))
        {
            // `Take::<A>::take(recv, …)` — the trait-turbofish qualified call
            // (WEP 2026-07-31): the turbofish pins one argument list by the
            // *types* its arguments resolve to, so an aliased spelling still
            // names the impl written under the original name; the head
            // resolves past any `use … as` alias to its declaration. Gated on
            // the turbofish matching the trait's declared arity: on a
            // zero-parameter trait the turbofish cannot be trait arguments
            // (`Shape::<Sq>::area` writes the receiver — a pre-existing
            // misuse), so that shape keeps its unknown-function error.
            if self.is_trait_instance_method(&g.name, &static_call.method)
                && self
                    .decl_key_at(g.id, &g.name)
                    .and_then(|key| self.trait_decl_type_params_of(&key))
                    .is_some_and(|params| !params.is_empty() && params.len() == g.args.len())
            {
                let declared_head = self.declared_trait_name(&g.name);
                let trait_args: Vec<TypeId> = g.args.iter().map(|a| self.resolve_type(a)).collect();
                let args_spelled: Vec<String> =
                    g.args.iter().map(|a| self.get_type_name_full(a)).collect();
                let required = super::types::RequiredTrait {
                    decl: self.tysys.resolutions.get(g.id),
                    args: Some(trait_args),
                    display: format!("{declared_head}<{}>", args_spelled.join(", ")),
                };
                let method_type_args: Vec<TypeId> = static_call
                    .type_args
                    .iter()
                    .map(|ty| self.resolve_type(ty))
                    .collect();
                return self.resolve_trait_qualified_call_parts(
                    required,
                    &static_call.method.clone(),
                    &static_call.args,
                    method_type_args,
                    static_call.id,
                    Some(static_call.method_id),
                    static_call.span,
                    None,
                    ctx,
                );
            }
            let _ = self.emit(TypeError::UnknownFunction {
                name: static_call_symbol_name(static_call),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        // Extract struct name AND canonical decl key for parameter type
        // lookup (follow newtypes to base). The canonical key disambiguates
        // two modules' same-named structs whose static methods both live in
        // the global `StaticMethodIndex`.
        let (struct_name_for_lookup, struct_key_for_lookup) =
            self.static_receiver_struct_key(target_type_id);

        // `Type::<T>::method()` parses as a static-method call and never
        // reaches `resolve_call`, which checks the bare spelling. The receiver
        // key comes from the resolved target type, as every lookup below does.
        let static_receiver = struct_name_for_lookup
            .as_ref()
            .map(|name| self.static_receiver_key(name, struct_key_for_lookup.as_ref()));
        if let (Some(name), Some(receiver)) = (&struct_name_for_lookup, &static_receiver) {
            self.check_static_call_visibility(
                receiver,
                &format!("{name}::{}", static_call.method),
                Some(static_call.id),
                static_call.span,
            );
        }

        // Look up parameter types for coercion. Thread the canonical
        // receiver key (from the resolved target type) so that two
        // modules' same-named structs each route to their own impl.
        let declared_params = struct_name_for_lookup.as_ref().and_then(|name| {
            self.lookup_static_method_param_types_keyed(
                name,
                &static_call.method,
                struct_key_for_lookup.as_ref(),
            )
        });
        // Whether a signature answered at all. A variant case or a flags member
        // reaches this path with no signature behind it, and its own arm below
        // owns its argument count; only a declared static has one to check here.
        let declares_params = declared_params.is_some();
        let mut param_types = declared_params.unwrap_or_default();

        // Literal preselect for a conversion call (WEP 2026-07-31 phase 4):
        // choose the impl before the argument is elaborated, so the expected
        // type comes from the selected impl instead of whichever the
        // name-keyed index returns first — the circular ordering this WEP
        // diagnoses. The name hint below then finds the same impl.
        if static_call.args.len() == 1
            && let Some(recv_name) = struct_name_for_lookup.clone()
            && self.try_conversion_preselect(
                &recv_name,
                &static_call.method,
                &static_call.args[0],
                static_call.span,
                ctx,
                &mut param_types,
                struct_key_for_lookup.as_ref(),
            )
        {
            return TypeTable::ERROR;
        }

        // Looked up once, reused for arg padding and the recorded dispatch fact.
        let static_method_defaults: Vec<(String, Option<ast::Expr>)> = struct_name_for_lookup
            .as_ref()
            .map(|name| {
                self.lookup_static_method_param_defaults(
                    name,
                    &static_call.method,
                    struct_key_for_lookup.as_ref(),
                )
            })
            .unwrap_or_default();
        // The module those defaults were written in, so their bodies answer to
        // it rather than to this call site.
        let static_method_module = static_receiver.as_ref().and_then(|receiver| {
            self.static_method_entry(receiver, &static_call.method)
                .map(|e| e.module.clone())
        });

        // For generic variant constructors (e.g., Option::<List<u8>>::Some([])),
        // compute substituted payload type so literal coercion works on first resolve.
        if param_types.is_empty() {
            let generic_data = {
                let resolved = self.tysys.type_table.borrow().get(target_type_id).clone();
                if let ResolvedType::GenericInstance {
                    type_args: instance_type_args,
                    ..
                } = resolved
                {
                    Some(instance_type_args)
                } else {
                    None
                }
            };
            if let Some(instance_type_args) = generic_data
                && let Some(variant_info) = self.variant_of_type(target_type_id).cloned()
                && let Some((_, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
            {
                let payload_is_unit = matches!(
                    self.tysys.type_table.borrow().get(case_data.payload),
                    ResolvedType::Unit
                );
                if !payload_is_unit {
                    let mut payload_type = case_data.payload;
                    if !instance_type_args.is_empty() {
                        payload_type = self
                            .tysys
                            .substitute_type_params(payload_type, &instance_type_args);
                    }
                    param_types.push(payload_type);
                }
            }
        }

        // Resolve method-level type arguments
        let method_type_args: Vec<TypeId> = static_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Not folded into `lookup_static_method_param_types`: variant
        // constructors need its answer to stay empty.
        {
            let has_type_args = matches!(&static_call.target_type, ast::Type::Generic(_))
                || !method_type_args.is_empty();
            if has_type_args
                && !param_types.is_empty()
                && let Some(name) = struct_name_for_lookup.as_deref()
                && let Some(sig) = self.qualified_method_sig(name, &static_call.method)
            {
                let declaring_args: Vec<TypeId> = match &static_call.target_type {
                    ast::Type::Generic(g) => g.args.iter().map(|t| self.resolve_type(t)).collect(),
                    _ => vec![],
                };
                // `TreeMap::<String, i32>` spells the *target's* arguments;
                // `impl … for TreeMap<String, V>` numbers only `V`. The
                // declaring block is what aligns the two.
                let declaring = sig
                    .declaring_impl
                    .and_then(|id| self.tysys.signatures.impl_sig(id));
                let instantiated = sig.instantiate_call_with(
                    &self.tysys.type_table,
                    declaring,
                    &declaring_args,
                    &method_type_args,
                );
                let first_value = sig.first_value_param().min(instantiated.param_types.len());
                for (param_type, &instantiated_type) in param_types
                    .iter_mut()
                    .zip(&instantiated.param_types[first_value..])
                {
                    *param_type = instantiated_type;
                }
            }
        }

        // Resolve arguments with expected types for coercion. `arg_spans` runs
        // parallel to `args` so a diagnostic still lands on the argument that
        // caused it rather than on the whole call.
        let mut args: Vec<TypeId> = static_call
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let expected_type = param_types.get(i).copied();
                self.resolve_expr(a, ctx, expected_type)
            })
            .collect();
        let mut arg_spans: Vec<Span> = static_call
            .args
            .iter()
            .map(super::ast::Expr::span)
            .collect();

        // Pad omitted trailing arguments with declared parameter defaults.
        // Variant / flags constructors carry no defaults, so the arg-count
        // checks below are unaffected.
        if args.len() < param_types.len() && !static_method_defaults.is_empty() {
            let defaults = &static_method_defaults;
            let mut subs: crate::hashmap::IndexMap<String, ast::Expr> =
                crate::hashmap::IndexMap::default();
            for (i, arg_ast) in static_call.args.iter().enumerate() {
                if let Some((pname, _)) = defaults.get(i) {
                    subs.insert(pname.clone(), arg_ast.clone());
                }
            }
            for i in args.len()..param_types.len() {
                let Some((pname, Some(default_ast))) = defaults.get(i) else {
                    break;
                };
                let expected_type = param_types[i];
                let mut default_expr = default_ast.clone();
                let vantage = static_method_module
                    .clone()
                    .map(|m| (m, default_expr.id().space()));
                default_expr.substitute_idents(&subs);
                let resolved = self.with_default_scope_module(static_method_module.clone(), |s| {
                    s.with_foreign_vantage(vantage, |s| {
                        s.resolve_expr(&default_expr, ctx, Some(expected_type))
                    })
                });
                args.push(resolved);
                arg_spans.push(default_expr.span());
                subs.insert(pname.clone(), default_expr);
            }
        }

        // A declared static is checked against its signature here, where the
        // spelled `Type::<T>::method(…)` call would otherwise reach codegen
        // with its arguments dropped.
        if declares_params
            && !self.check_static_call_args(
                &param_types,
                &args,
                &arg_spans,
                &static_method_defaults,
                static_call.span,
            )
        {
            return TypeTable::ERROR;
        }

        // Option::Some and Option::None are handled by the generic variant
        // construction path below (line ~686). No special case needed.

        // Handle flags type static methods: none() and all()
        {
            // The receiver's own declaration, not its head resolved again.
            // Only a `flags` declaration has members, so this guards the kind.
            if let Some(flags_info) = self
                .tysys
                .type_table
                .borrow()
                .nominal_def(target_type_id)
                .and_then(|def| self.type_lookup().flags_members_of(def))
                .cloned()
            {
                match static_call.method.as_str() {
                    "none" => {
                        if !args.is_empty() {
                            let _ = self.emit(TypeError::ArgumentCountMismatch {
                                expected: 0,
                                found: args.len(),
                                span: static_call.span,
                            });
                            return TypeTable::ERROR;
                        }
                        return flags_info.type_id;
                    }
                    "all" => {
                        if !args.is_empty() {
                            let _ = self.emit(TypeError::ArgumentCountMismatch {
                                expected: 0,
                                found: args.len(),
                                span: static_call.span,
                            });
                            return TypeTable::ERROR;
                        }
                        return flags_info.type_id;
                    }
                    _ => {}
                }
            }
        }

        // Handle custom variant construction: Shape::Circle(5.0) or MyVariant::Unit
        if let ResolvedType::Variant { .. } =
            self.tysys.type_table.borrow().get(target_type_id).clone()
        {
            // Look up the variant case info
            if let Some(variant_info) = self.variant_of_type(target_type_id) {
                // Find the case by name
                if let Some((_case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                {
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.tysys.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        let _ = self.emit(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TypeTable::ERROR;
                    }

                    return target_type_id;
                }
                // If no matching case, fall through to general method lookup
                // (e.g., trait methods like `AppError::from(e)`)
            }
        }

        // Handle generic variant construction: Result::<i32, String>::Ok(42)
        let is_generic_instance = matches!(
            self.tysys.type_table.borrow().get(target_type_id),
            ResolvedType::GenericInstance { .. }
        );
        if is_generic_instance {
            // Check if the base type is a variant
            if let Some(variant_info) = self.variant_of_type(target_type_id).cloned() {
                let name = variant_info.name.clone();
                // This is a generic variant like Result<T, E>
                // Find the case by name
                if let Some((_case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                    .map(|(i, c)| (i, c.clone()))
                {
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.tysys.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        let _ = self.emit(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TypeTable::ERROR;
                    }

                    // Refine `_` placeholders in the turbofish (`Result::<_,
                    // MyErr>::Ok(7)`): infer the hole slots from the payload
                    // while the explicit args stay pinned. Without holes the
                    // explicitly-resolved `target_type_id` is already complete.
                    let has_target_hole = matches!(
                        &static_call.target_type,
                        ast::Type::Generic(g) if super::call::turbofish_has_hole(&g.args)
                    );
                    let result_type = if has_target_hole {
                        let target_holes = match &static_call.target_type {
                            ast::Type::Generic(g) => super::call::turbofish_holes(&g.args),
                            _ => Vec::new(),
                        };
                        let explicit_args = match self.tysys.type_table.borrow().get(target_type_id)
                        {
                            ResolvedType::GenericInstance { type_args, .. } => type_args.clone(),
                            _ => Vec::new(),
                        };
                        {
                            let inferred = self.tysys.infer_variant_type_args(
                                &self.annotate_ctx,
                                &variant_info,
                                &case_data,
                                args.first().copied(),
                                None,
                                &explicit_args,
                                &target_holes,
                            );
                            self.defer_uninferable_variant(
                                inferred,
                                &name,
                                &variant_info,
                                static_call.span,
                            )
                        }
                    } else {
                        target_type_id
                    };

                    // Check payload type against the variant case's payload
                    // type, substituted with the (possibly refined) type args.
                    if !args.is_empty() {
                        let result_args = match self.tysys.type_table.borrow().get(result_type) {
                            ResolvedType::GenericInstance { type_args, .. } => {
                                Some(type_args.clone())
                            }
                            _ => None,
                        };
                        let expected_payload = match result_args {
                            Some(args_vec) => Some(
                                self.tysys
                                    .substitute_type_params(case_data.payload, &args_vec),
                            ),
                            None => param_types.first().copied(),
                        };
                        if let Some(expected_type) = expected_payload {
                            let span = static_call
                                .args
                                .first()
                                .map_or(static_call.span, super::ast::Expr::span);
                            self.typecheck(args[0], expected_type, span);
                        }
                    }

                    return result_type;
                }
                // If no matching case, fall through to general method lookup
                // (e.g., trait methods like `Result::<T, E>::from(e)`)
            }
        }

        // Handle From<T>::from calls resolved via bodyless `impl From<T> for Type;`
        // The synthesized function doesn't exist during resolution, so we generate the call inline.
        if static_call.method == "from"
            && args.len() == 1
            && self.has_from_synthesis_request(&static_call.target_type, &args[0])
        {
            return self.resolve_from_call(target_type_id, args[0], static_call.id);
        }

        // Reflexive identity: From<T> for T — return the value unchanged.
        if static_call.method == "from" && args.len() == 1 && args[0] == target_type_id {
            return args.into_iter().next().unwrap();
        }

        // Newtype From conversions: From<Base> for Newtype and From<Newtype> for Base.
        // Newtypes share the same representation as their base type, so this is a Cast.
        if static_call.method == "from" && args.len() == 1 {
            let arg_type = args[0];
            let base_of_target = self
                .tysys
                .type_table
                .borrow()
                .get_newtype_base(target_type_id);
            let base_of_arg = self.tysys.type_table.borrow().get_newtype_base(arg_type);
            if base_of_target == Some(arg_type) || base_of_arg == Some(target_type_id) {
                // Reify rebuilds the newtype `Cast`; the body walk projects
                // only the result type.
                return target_type_id;
            }
        }

        let (struct_name, struct_module, mangled_struct_name, struct_type_args) =
            match self.tysys.type_table.borrow().get(target_type_id) {
                ResolvedType::Struct { .. } | ResolvedType::Resource { .. } => {
                    let (name, module_source) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(target_type_id)
                        .expect("a nominal type names a declaration");
                    let fq = self
                        .tysys
                        .type_table
                        .borrow()
                        .fq_base_type_name(target_type_id);
                    (name, module_source, fq, vec![])
                }
                // Generic resource types (Future<T>, Stream<T>, etc.) - handle like generic structs
                // for static method resolution: use the base name and type args for substitution.
                ResolvedType::GenericResource { type_args, .. } => {
                    let (name, module_source) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(target_type_id)
                        .expect("a generic resource names a declaration");
                    let type_arg_names: Vec<FqTypeName> = type_args
                        .iter()
                        .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                        .collect();
                    let mangled = self
                        .tysys
                        .type_table
                        .borrow()
                        .fq_base_type_name(target_type_id)
                        .with_args(type_arg_names);
                    (name, module_source, mangled, type_args.clone())
                }
                ResolvedType::Primitive(prim) => (
                    prim.as_str().to_string(),
                    ModuleSource::primitive(),
                    FqTypeName::builtin(prim.as_str()),
                    vec![],
                ),
                ResolvedType::BuiltinArray(elem) => {
                    let elem = *elem;
                    let arg = self.tysys.type_table.borrow().fq_type_name(elem);
                    (
                        TypeTable::ARRAY_TYPE_NAME.to_string(),
                        ModuleSource::array(),
                        FqTypeName::builtin(TypeTable::ARRAY_TYPE_NAME).with_args(vec![arg]),
                        vec![elem],
                    )
                }
                ResolvedType::Enum { .. } | ResolvedType::Variant { .. } => {
                    let (name, module_source) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(target_type_id)
                        .expect("a nominal type names a declaration");
                    let fq = self
                        .tysys
                        .type_table
                        .borrow()
                        .fq_base_type_name(target_type_id);
                    (name, module_source, fq, vec![])
                }
                ResolvedType::GenericInstance { type_args, .. } => {
                    let (name, module_source) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(target_type_id)
                        .expect("a generic instance names a declaration");
                    let args: Vec<FqTypeName> = type_args
                        .iter()
                        .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                        .collect();
                    let mangled = self
                        .tysys
                        .type_table
                        .borrow()
                        .fq_base_type_name(target_type_id)
                        .with_args(args);
                    (name, module_source, mangled, type_args.clone())
                }
                ResolvedType::Newtype { base_type, .. } => {
                    // First try the newtype's own name (for methods defined via `impl NewtypeName`)
                    let (newtype_name, newtype_module) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(target_type_id)
                        .expect("a newtype names a declaration");

                    // Check if the newtype itself has the static method
                    if self.has_static_method_direct(&newtype_name, &static_call.method) {
                        let fq = self
                            .tysys
                            .type_table
                            .borrow()
                            .fq_base_type_name(target_type_id);
                        (newtype_name, newtype_module, fq, vec![])
                    } else {
                        // Fall back to the base type for inherited methods
                        match self.tysys.type_table.borrow().get(*base_type).clone() {
                            ResolvedType::Struct { .. } => {
                                let (name, module_source) = self
                                    .tysys
                                    .type_table
                                    .borrow()
                                    .nominal_head(*base_type)
                                    .expect("a struct names a declaration");
                                let fq =
                                    self.tysys.type_table.borrow().fq_base_type_name(*base_type);
                                (name, module_source, fq, vec![])
                            }
                            ResolvedType::GenericInstance { type_args, .. } => {
                                let (name, module_source) = self
                                    .tysys
                                    .type_table
                                    .borrow()
                                    .nominal_head(*base_type)
                                    .expect("a generic instance names a declaration");
                                let args: Vec<FqTypeName> = type_args
                                    .iter()
                                    .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                                    .collect();
                                let fq = self
                                    .tysys
                                    .type_table
                                    .borrow()
                                    .fq_base_type_name(*base_type)
                                    .with_args(args);
                                (name, module_source, fq, type_args)
                            }
                            ResolvedType::Newtype {
                                base_type: inner_base,
                                ..
                            } => {
                                let mut current = inner_base;
                                loop {
                                    match self.tysys.type_table.borrow().get(current).clone() {
                                        ResolvedType::Struct { .. } => {
                                            let (name, module_source) = self
                                                .tysys
                                                .type_table
                                                .borrow()
                                                .nominal_head(current)
                                                .expect("a struct names a declaration");
                                            let fq = self
                                                .tysys
                                                .type_table
                                                .borrow()
                                                .fq_base_type_name(current);
                                            break (name, module_source, fq, vec![]);
                                        }
                                        ResolvedType::Newtype {
                                            base_type: next, ..
                                        } => current = next,
                                        _ => {
                                            let fq = self
                                                .tysys
                                                .type_table
                                                .borrow()
                                                .fq_base_type_name(target_type_id);
                                            break (newtype_name, newtype_module, fq, vec![]);
                                        }
                                    }
                                }
                            }
                            ResolvedType::Primitive(prim) => (
                                prim.as_str().to_string(),
                                ModuleSource::primitive(),
                                FqTypeName::builtin(prim.as_str()),
                                vec![],
                            ),
                            _ => {
                                let fq = self
                                    .tysys
                                    .type_table
                                    .borrow()
                                    .fq_base_type_name(target_type_id);
                                (newtype_name, newtype_module, fq, vec![])
                            }
                        }
                    }
                }
                ResolvedType::Flags { .. } => {
                    // First try the flags' own name, then fall back to u32
                    let (flags_name, flags_module) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(target_type_id)
                        .expect("a flags type names a declaration");
                    if self.has_static_method_direct(&flags_name, &static_call.method) {
                        let fq = self
                            .tysys
                            .type_table
                            .borrow()
                            .fq_base_type_name(target_type_id);
                        (flags_name, flags_module, fq, vec![])
                    } else {
                        (
                            "u32".to_string(),
                            ModuleSource::primitive(),
                            FqTypeName::builtin("u32"),
                            vec![],
                        )
                    }
                }
                // The target names no struct-like type: a trait, an undeclared
                // name, a turbofish on a non-generic.
                _ => {
                    let _ = self.emit(TypeError::UnknownFunction {
                        name: static_call_symbol_name(static_call),
                        span: static_call.span,
                    });
                    return TypeTable::ERROR;
                }
            };

        // Find trait name: if the static method belongs to a trait impl, include the
        // trait name in the mangled function name so WIR can resolve it correctly.
        // For From/TryFrom, disambiguate by matching the first argument's type.
        let arg_type_hint = if (static_call.method == "from" || static_call.method == "try_from")
            && args.len() == 1
        {
            Some(self.tysys.type_table.borrow().type_name(args[0]))
        } else {
            None
        };
        // Keep the whole selection: its trait names the mangled function, and
        // its `method_id` is what the use→def edge below is recorded against.
        // A name lookup cannot stand in — two conversion impls on one type
        // declare the same `from`, and only the argument's type separates
        // them. The receiver comes off the resolved type: re-deriving it from
        // `struct_name` searches the caller's frame, which an aliased import
        // leaves without that name at all.
        let receiver_key =
            self.impl_target_of(target_type_id, &crate::name::DeclName::new(&struct_name));
        let selected = self.locate_static_method_impl(
            &struct_name,
            &static_call.method,
            arg_type_hint.as_deref(),
            Some(&receiver_key),
        );
        let trait_name_opt = selected.as_ref().and_then(|r| r.trait_name.clone());

        // The expected type that shaped the argument came from
        // `lookup_static_method_param_types_keyed`, which keys on (receiver,
        // method) alone — with two conversion impls it can be a different
        // impl's than the one the argument's type then selects. Left alone the
        // mangled name loses its trait and reaches WIR build unresolved, so the
        // disagreement is reported here instead of ICE-ing there.
        if trait_name_opt.is_none()
            && let Some(arg_type) = arg_type_hint.as_deref()
            && !self.has_inherent_static_method(
                &struct_name,
                &static_call.method,
                Some(&receiver_key),
            )
            && self.report_unmatched_conversion(
                &struct_name,
                &static_call.method,
                arg_type,
                static_call.span,
                Some(&receiver_key),
            )
        {
            return TypeTable::ERROR;
        }

        let mangled_func_name = MethodName::format_local(
            &mangled_struct_name,
            trait_name_opt.as_ref(),
            &static_call.method,
        );

        let method_ref = StaticMethodRef::new(
            struct_module.clone(),
            struct_name.clone(),
            static_call.method.clone(),
            trait_name_opt.clone(),
            selected.as_ref().and_then(|r| r.method_id),
        );

        // Look up return type
        let mut return_type = self.lookup_static_method_return_type(
            &method_ref,
            &mangled_struct_name,
            &mangled_func_name,
        );

        // A value blanket indexes statics under its receiver *param* name, so
        // the concrete receiver's own bucket misses.
        if return_type == TypeTable::UNKNOWN
            && let Some(resolved) = self.resolve_blanket_static_method(
                target_type_id,
                &static_call.method,
                static_call.id,
                &method_type_args,
                &static_method_defaults,
                &args,
                &arg_spans,
                static_call.span,
            )
        {
            return resolved;
        }

        // Emit a compile error if the static method was not found anywhere
        if return_type == TypeTable::UNKNOWN {
            let _ = self.emit(TypeError::UnknownFunction {
                name: static_call_symbol_name(static_call),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        // Substitute the declaring block's and the method's own parameters,
        // taken from the signature rather than counted off the receiver.
        {
            let (decl_params, method_params) =
                self.qualified_method_slot_params(&struct_name, &static_call.method);
            let decl_args =
                self.aligned_declaring_args(&struct_name, &static_call.method, &struct_type_args);
            let subst_ctx = SubstitutionContext::new()
                .bind(&decl_params, &decl_args)
                .bind(&method_params, &method_type_args);
            if !subst_ctx.is_empty() {
                return_type =
                    subst_ctx.substitute(return_type, &mut self.tysys.type_table.borrow_mut());
            }
        }

        // Build monomorph_info for generic instantiations
        let monomorph_info = if struct_type_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            let generic_name = MethodName::format_local(
                &self.qualified_receiver_name(&struct_name),
                trait_name_opt.as_ref(),
                &static_call.method,
            );
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: struct_type_args.clone(),
                method_type_args: method_type_args.clone(),
                is_blanket: false,
            })
        };

        let method_type_arg_names: Vec<FqTypeName> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
            .collect();
        let impl_only_type_arg_names: Vec<FqTypeName> = struct_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
            .collect();

        let param_is_mut = struct_name_for_lookup
            .as_deref()
            .map(|name| {
                self.lookup_static_method_param_is_mut(
                    name,
                    &static_call.method,
                    struct_key_for_lookup.as_ref(),
                )
            })
            .unwrap_or_default();

        // Build method_info with base struct name and trait name (if applicable)
        let mut method_info = LocalMethodName::new(
            self.qualified_receiver_name(&struct_name),
            trait_name_opt,
            static_call.method.clone(),
        )
        .with_type_args(&impl_only_type_arg_names, &method_type_arg_names);

        // Propagate #[cm("...")] from resource static methods for CM binding synthesis.
        let cm_owner = self.tysys.type_table.borrow().nominal_def(target_type_id);
        method_info.cm_name = self.lookup_resource_static_cm(cm_owner, &static_call.method);

        // The selection covers trait impls only; an inherent static has none
        // and reaches the index instead.
        if let Some(method_def) = selected.as_ref().and_then(|r| r.method_id).or_else(|| {
            let receiver =
                self.impl_target_of(target_type_id, &crate::name::DeclName::new(&struct_name));
            self.qualified_method_decl_id(&receiver, &static_call.method)
                .or_else(|| self.qualified_method_decl_at(None, &struct_name, &static_call.method))
        }) {
            self.record_reference_to_decl(static_call.method_id, method_def);
        }

        let func_ref = FunctionRef {
            module_source: struct_module,
            name: mangled_func_name,
            monomorph_info,
            method_info: Some(method_info),
        };

        // WEP 2026-05-26: record the resolved static-method
        // call so reify reproduces the same `FunctionRef` (mangled name,
        // monomorph info, and `cm_name` for CM binding synthesis) without
        // re-resolving the target type — reify's from-scratch resolution
        // fails on imported / CM generic targets (`Future::<T>::new`,
        // `Result::<…>::Ok`), yielding an empty struct name. Keyed on the
        // `StaticMethodCallExpr`'s own `AstId`; variant-ctor turbofish
        // shapes are handled by reify before this fact is consulted.
        let key = static_call.id;
        self.sem.types.static_method_dispatch.insert(
            key,
            super::sem::types::StaticMethodDispatch {
                method_def: selected.as_ref().and_then(|r| r.method_id),
                function_ref: func_ref,
                param_is_mut,
                type_args: method_type_args,
                param_defaults: static_method_defaults,
                param_types: param_types.clone(),
                self_in_args: false,
            },
        );

        return_type
    }

    /// Resolve a `static` trait method reached through a value blanket impl
    /// (`impl<T: Bound> Trait for T`). The blanket has no per-type home, so its
    /// statics are indexed under the receiver param name (`T`) and the concrete
    /// receiver's bucket never sees them. Select the blanket whose bounds the
    /// receiver satisfies and dispatch to its template, the way an instance
    /// method reached through the same impl already does.
    pub(super) fn resolve_blanket_static_method(
        &mut self,
        receiver_type_id: TypeId,
        method: &str,
        call_id: AstId,
        method_type_args: &[TypeId],
        static_method_defaults: &[(String, Option<ast::Expr>)],
        args: &[TypeId],
        arg_spans: &[Span],
        span: Span,
    ) -> Option<TypeId> {
        let BlanketStatic {
            trait_name,
            param: blanket_param,
            binder,
            module: blanket_module,
            def: blanket_def,
        } = self.find_blanket_static_method(receiver_type_id, method)?;

        let template_name = MethodName::format_local(&binder, Some(&trait_name), method);
        let method_ref = StaticMethodRef::new(
            blanket_module.clone(),
            blanket_param.clone(),
            method.to_string(),
            Some(trait_name.clone()),
            None,
        );
        let template_return =
            self.lookup_static_method_return_type(&method_ref, &binder, &template_name);
        if template_return == TypeTable::UNKNOWN {
            return None;
        }
        // The template is written against the blanket param, so `-> Self` /
        // `-> T` lands on the receiver at the call site.
        let blanket_slot = self.blanket_param_slot(&blanket_param);
        let return_type = SubstitutionContext::new()
            .bind(&[blanket_slot], &[receiver_type_id])
            .substitute(template_return, &mut self.tysys.type_table.borrow_mut());

        // Unchecked, a mis-arity or mis-typed call reaches codegen and surfaces
        // as a Wasm validation failure.
        let param_types = self.blanket_static_param_types(
            &blanket_module,
            &blanket_param,
            method,
            receiver_type_id,
        );
        if !self.check_static_call_args(&param_types, args, arg_spans, static_method_defaults, span)
        {
            return Some(TypeTable::ERROR);
        }

        let receiver_arg_name = self
            .tysys
            .type_table
            .borrow()
            .fq_type_name(receiver_type_id);
        let method_type_arg_names: Vec<FqTypeName> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
            .collect();
        let method_info = LocalMethodName::new(
            self.tysys.fq_receiver_head(receiver_type_id),
            Some(trait_name),
            method.to_string(),
        )
        .with_type_args(&[receiver_arg_name], &method_type_arg_names);

        let func_ref = FunctionRef {
            module_source: blanket_module,
            name: method_info.to_mangled_name(),
            monomorph_info: Some(MonomorphInfo {
                generic_name: template_name,
                impl_type_args: vec![receiver_type_id],
                method_type_args: method_type_args.to_vec(),
                is_blanket: true,
            }),
            method_info: Some(method_info),
        };
        self.sem.types.static_method_dispatch.insert(
            call_id,
            super::sem::types::StaticMethodDispatch {
                method_def: self.tysys.declared_method(blanket_def, method),
                function_ref: func_ref,
                param_is_mut: Vec::new(),
                type_args: method_type_args.to_vec(),
                param_defaults: static_method_defaults.to_vec(),
                param_types,
                self_in_args: false,
            },
        );

        Some(return_type)
    }

    /// The blanket template's value-parameter types with its receiver param
    /// bound to the concrete receiver, so the call site compares against the
    /// types the instantiation will actually take.
    fn blanket_static_param_types(
        &mut self,
        blanket_module: &ModuleSource,
        blanket_param: &str,
        method: &str,
        receiver_type_id: TypeId,
    ) -> Vec<TypeId> {
        let key = crate::elaborator::trait_env::ImplTargetKey::TypeParam(
            blanket_module.clone(),
            blanket_param.to_string(),
        );
        let template = self
            .lookup_static_method_param_types_keyed(blanket_param, method, Some(&key))
            .unwrap_or_default();
        let blanket_slot = self.blanket_param_slot(blanket_param);
        let mut tt = self.tysys.type_table.borrow_mut();
        template
            .iter()
            .map(|&pt| {
                SubstitutionContext::new()
                    .bind(&[blanket_slot], &[receiver_type_id])
                    .substitute(pt, &mut tt)
            })
            .collect()
    }

    /// The blanket impl's own parameter. `impl<T> Trait for T` declares
    /// exactly one, and the `DefId` this path is built on *is* its name, so
    /// the binder is the declaration rather than a reconstruction of it.
    fn blanket_param_slot(&self, blanket_param: &str) -> TypeId {
        self.tysys
            .type_table
            .borrow_mut()
            .make_type_param(blanket_param.to_string(), 0)
    }

    /// The declaring block's and the method's own type parameters for a
    /// qualified method, split where its signature says they split.
    fn qualified_method_slot_params(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> (Vec<TypeId>, Vec<TypeId>) {
        let Some(sig) = self.qualified_method_sig(struct_name, method_name) else {
            return (vec![], vec![]);
        };
        let ids = |ps: &[(String, TypeId)]| ps.iter().map(|(_, id)| *id).collect();
        (ids(sig.declaring_type_params()), ids(sig.own_type_params()))
    }

    /// A receiver's spelled type arguments (`TreeMap::<String, i32>`) reordered
    /// into the declaring block's slot order. `impl … for TreeMap<String, V>`
    /// numbers only `V`, so a positional zip binds it to `String`; the block's
    /// own alignment says position 1 fills it.
    fn aligned_declaring_args(
        &self,
        struct_name: &str,
        method_name: &str,
        receiver_args: &[TypeId],
    ) -> Vec<TypeId> {
        let Some(slots) = self
            .qualified_method_sig(struct_name, method_name)
            .and_then(|sig| sig.declaring_impl)
            .and_then(|id| self.tysys.signatures.impl_sig(id))
            .and_then(|impl_sig| impl_sig.spelled_slots(&self.tysys.type_table, receiver_args))
        else {
            return receiver_args.to_vec();
        };
        let (decl_params, _) = self.qualified_method_slot_params(struct_name, method_name);
        let table = self.tysys.type_table.borrow();
        decl_params
            .iter()
            .filter_map(|slot| match table.get(*slot) {
                ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                    slots.get(index).copied()
                }
                _ => None,
            })
            .collect()
    }

    /// Whether `args` arguments fill a callee declaring `params` parameters,
    /// `optional` of them defaulted. A defaulted parameter may be omitted and is
    /// filled at reify; nothing may be passed beyond the declared list, or the
    /// extra argument is dropped along with whatever its expression did.
    pub(super) fn arg_count_fits(args: usize, params: usize, optional: usize) -> bool {
        args >= params.saturating_sub(optional) && args <= params
    }

    /// Report an argument list a static method's signature cannot accept,
    /// returning `false` once a diagnostic was emitted. Shared by the three
    /// spellings that reach one: `Type::method(…)`, `Type::<T>::method(…)` and
    /// `ns::Type::method(…)`. A parameter still carrying a type param belongs
    /// to a blanket's pack, which the call site cannot pin: it counts toward
    /// arity but its type is not compared.
    pub(super) fn check_static_call_args(
        &mut self,
        param_types: &[TypeId],
        args: &[TypeId],
        arg_spans: &[Span],
        static_method_defaults: &[(String, Option<ast::Expr>)],
        span: Span,
    ) -> bool {
        assert_eq!(
            args.len(),
            arg_spans.len(),
            "every resolved argument carries the span it was written at"
        );
        let optional = static_method_defaults
            .iter()
            .filter(|(_, d)| d.is_some())
            .count();
        if !Self::arg_count_fits(args.len(), param_types.len(), optional) {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: param_types.len(),
                found: args.len(),
                span,
            });
            return false;
        }
        for (i, (arg, &expected)) in args.iter().zip(param_types).enumerate() {
            if self.tysys.type_table.borrow().contains_type_param(expected) {
                continue;
            }
            self.typecheck(*arg, expected, arg_spans[i]);
        }
        true
    }

    /// The value blanket impl carrying a static `method_name` whose receiver
    /// bounds `receiver_type_id` satisfies.
    pub(super) fn find_blanket_static_method(
        &mut self,
        receiver_type_id: TypeId,
        method_name: &str,
    ) -> Option<BlanketStatic> {
        let candidates: Vec<(BlanketStatic, Vec<super::trait_env::BlanketBound>)> = self
            .tysys
            .trait_env
            .blanket_impls
            .iter()
            .flat_map(|(trait_name, impls)| impls.iter().map(move |b| (trait_name, b)))
            .filter(|(_, b)| b.receiver == super::trait_env::BlanketReceiver::Value)
            // The index is keyed by the receiver *param*, which every blanket in
            // a module shares (`Serialize` beside `Deserialize`, both over `T`),
            // so an entry speaks for this blanket only if this impl block
            // declares the method. The header alone would match an instance
            // method of the same name; both indices together will not.
            .filter(|(_, b)| {
                let Some(header) = self.tysys.trait_env.impl_headers.get(&b.def) else {
                    return false;
                };
                self.tysys
                    .trait_env
                    .static_method_index
                    .get(&crate::elaborator::trait_env::ImplTargetKey::TypeParam(
                        b.module.clone(),
                        b.param.clone(),
                    ))
                    .is_some_and(|entries| {
                        entries.iter().any(|e| {
                            e.name == method_name
                                && header.methods.iter().any(|m| m.def == e.method_id)
                        })
                    })
            })
            // The trait comes off the impl's own header, so the blanket
            // index's bare-name key never reaches a mangled name.
            .filter_map(|(_, b)| {
                let header = self.tysys.trait_env.impl_headers.get(&b.def)?;
                Some((
                    BlanketStatic {
                        trait_name: self
                            .tysys
                            .trait_env
                            .fq_trait_of_impl(header, &self.tysys.resolutions)?,
                        param: b.param.clone(),
                        binder: b.receiver_binder(self.tysys.resolutions.defs()),
                        module: b.module.clone(),
                        def: b.def,
                    },
                    b.bounds.clone(),
                ))
            })
            .collect();

        candidates
            .into_iter()
            .find(|(_, bounds)| {
                bounds.iter().all(|bound| {
                    bound.decl_ref.is_some_and(|bound_def| {
                        self.tysys.type_implements_trait(
                            &self.annotate_ctx,
                            &self.type_lookup(),
                            receiver_type_id,
                            bound_def,
                        )
                    })
                })
            })
            .map(|(blanket, _)| blanket)
    }

    /// Look up `#[cm("...")]` for a static (no-self) method on the resource
    /// `def` declares.
    fn lookup_resource_static_cm(
        &self,
        def: Option<crate::defs::DefId>,
        method_name: &str,
    ) -> Option<String> {
        let key = crate::elaborator::trait_env::ImplTargetKey::of_decl(
            self.tysys.resolutions.defs(),
            def?,
        );
        let (_, _, decl_id, _) = self.tysys.trait_env.resource_static(&key, method_name)?;
        self.tysys
            .signatures
            .resource_method_sig(*decl_id, method_name)?
            .cm_name
            .clone()
    }

    /// Check if a static method exists directly for a given type name (no newtype fallback).
    fn has_static_method_direct(&self, struct_name: &str, method_name: &str) -> bool {
        let qualified = self.qualified_receiver_name(struct_name);
        let mangled = MethodName::format_local(&qualified, None, method_name);
        if self.sem.decls.function_return_types.contains_key(&mangled) {
            return true;
        }
        // Also check with trait-qualified name
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name) {
            let trait_mangled =
                MethodName::format_local(&qualified, Some(&trait_name), method_name);
            if self
                .sem
                .decls
                .function_return_types
                .contains_key(&trait_mangled)
            {
                return true;
            }
        }
        // Every impl block on the type, inherent and trait alike.
        let keys = self
            .tysys
            .trait_env
            .all_impl_keys(&self.impl_target(struct_name));
        self.keys_declare_static_method(&keys, method_name)
    }

    /// Look up static method return type based on struct name and method name
    /// `receiver` is the declaration the call site resolved, not a spelling to
    /// re-resolve: beside a same-named local declaration, the caller's own frame
    /// answers with the wrong one.
    pub(super) fn lookup_static_method_return_type(
        &mut self,
        method_ref: &StaticMethodRef,
        receiver: &FqTypeName,
        mangled_func_name: &str,
    ) -> TypeId {
        let struct_name = method_ref.type_name.as_str();
        let method_name = method_ref.method_name.as_str();
        // First check locally registered function_return_types
        if let Some(&return_type) = self.sem.decls.function_return_types.get(mangled_func_name) {
            return return_type;
        }

        // Also try with just StructName::method (for non-generic types)
        let simple_name = MethodName::format_local(receiver, None, method_name);
        if let Some(&return_type) = self.sem.decls.function_return_types.get(&simple_name) {
            return return_type;
        }

        // Try with trait-qualified name (StructName^TraitName::method)
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name) {
            let trait_mangled = MethodName::format_local(receiver, Some(&trait_name), method_name);
            if let Some(&return_type) = self.sem.decls.function_return_types.get(&trait_mangled) {
                return return_type;
            }
        }

        // The receiver's own declaration is the key. A head naming none falls
        // to the frame derivation, which is one vantage; a second key tried
        // when the first misses makes the order a silent tiebreak.
        let static_key = receiver.head().def().map_or_else(
            || self.impl_target(struct_name),
            |def| super::trait_env::ImplTargetKey::of_decl(self.tysys.resolutions.defs(), def),
        );
        // The decl pass already resolved this signature in the impl's own
        // frame — impl and method type params interned, `Self` bound to the
        // impl target, the impl module's imports in scope. Re-deriving all of
        // that here is what the digest exists to avoid.
        if let Some(return_type) = self.agreed_qualified_method_return(&static_key, method_name) {
            return return_type;
        }

        // Search resource declarations via pre-built index. Same canonical
        // key disambiguation as the inherent-impl path above. The decl pass
        // resolved these in the resource's own frame, so a generic resource's
        // `Option<T>` is already a `TypeParam` here.
        let indexed_resource_return = self
            .tysys
            .trait_env
            .resource_static(&static_key, method_name)
            .and_then(|(name, _, item_id, _)| {
                let sig = self.tysys.signatures.resource_method_sig(*item_id, name)?;
                Some(sig.decl.return_type.unwrap_or(TypeTable::UNIT))
            });
        if let Some(return_type) = indexed_resource_return {
            return return_type;
        }

        // The index is keyed by the declaring resource, so an inherited
        // method is reachable only by walking the chain.
        if let super::trait_env::ImplTargetKey::Decl(def) = &static_key
            && let Some((_, sig)) = self.resource_instance_method(*def, method_name)
        {
            return sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        }

        // Auto-derived `Default::default()` returns the struct type itself.
        if method_name == "default"
            && let Some(struct_type) = self
                .tysys
                .auto_derive_default_struct_type(&self.type_lookup(), struct_name)
        {
            return struct_type;
        }

        // Fall back to a trait default method body. When
        // `impl Trait for Type` does not override a static method that the
        // trait provides a default for, concrete `Type::method()` calls
        // must still resolve — this mirrors how generic dispatch
        // (`T::method()`) already reaches the trait default.
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name)
            && let Some(default_method) = self
                .trait_sig_by_name(trait_name.base_name())
                .and_then(|sig| sig.method(method_name))
                .filter(|m| m.default_body.is_some() && m.sig.self_kind == ast::SelfKind::None)
                .cloned()
        {
            let mut scope = self.enter_inherited_type_param_scope();
            let self_type_id = scope.resolve_unsited_type_name(struct_name, Span::default());
            let result = default_method
                .sig
                .instantiate_call(&scope.tysys.type_table, &[self_type_id], &[])
                .return_type;
            drop(scope);
            return result;
        }

        TypeTable::UNKNOWN
    }

    /// A static method's value parameters, in the declaration's own frame — its
    /// slots still `TypeParam`, for the caller to substitute after inference.
    ///
    /// `target_hint` is the receiver's canonical key, from a call site that
    /// already resolved the target to a `TypeId`. Without it the bare
    /// `struct_name` canonicalises against a global "first matching name"
    /// bucket, which picks another module's same-named struct.
    ///
    /// `None` is a receiver / method pair nothing declares, which an empty list
    /// would otherwise report as "declares no parameters" — and a count checked
    /// against that drops the arguments a caller wrote.
    pub(super) fn lookup_static_method_param_types_keyed(
        &mut self,
        struct_name: &str,
        method_name: &str,
        target_hint: Option<&crate::elaborator::trait_env::ImplTargetKey>,
    ) -> Option<Vec<TypeId>> {
        // O(1) lookup via pre-built static method index. The index is
        // keyed by the receiver's canonical decl key so two same-named
        // structs in different modules each resolve to their own
        // bucket. Prefer the caller's pre-resolved key when available
        // (it threads through the `TypeId`'s module source and so
        // distinguishes `CounterA::make` from `CounterB::make` even
        // though both alias the same bare name `"Counter"`).
        let static_key = self.static_receiver_key(struct_name, target_hint);
        // Carry the impl's defining module out of the index alongside
        // the AST so the per-param elaborator can swap into its perspective —
        // a static method's signature references types the impl module
        // imports, not the caller's.
        // Static methods take no receiver, so the digest's canonical form —
        // impl type params left abstract — is already the answer.
        // Value parameters only: every caller keeps a receiver of its own,
        // separate from this list.
        if let Some(sig) = self.unique_static_method_sig(&static_key, method_name) {
            return Some(sig.decl.param_types[sig.first_value_param()..].to_vec());
        }
        // A resource declares its statics in Wado like any other declaration,
        // so they answer from the same signature table at the same point in the
        // pass — `Response::new` is checked where `P::make` is. Statics are not
        // inherited, so the receiver's own declaration answers, never its chain.
        if let super::trait_env::ImplTargetKey::Decl(def) = &static_key
            && let Some(sig) = self.tysys.signatures.resource_method_sig(*def, method_name)
            && sig.self_kind == ast::SelfKind::None
        {
            return Some(sig.decl.param_types[sig.first_value_param()..].to_vec());
        }
        // The index holds only the declaring resource's own methods, so an
        // inherited one is reached by walking the chain. Instance methods only:
        // a static the receiver declares itself shadows an inherited instance
        // method of the same name, and the arm above has already answered it.
        if let super::trait_env::ImplTargetKey::Decl(def) = &static_key
            && !self.declares_resource_static(*def, method_name)
            && let Some((_, sig)) = self.resource_instance_method(*def, method_name)
        {
            return Some(sig.decl.param_types[sig.first_value_param()..].to_vec());
        }
        None
    }

    /// Whether the resource `def` declares `method_name` as a static of its own.
    fn declares_resource_static(&self, def: crate::defs::DefId, method_name: &str) -> bool {
        self.tysys
            .trait_env
            .resource_static(
                &crate::elaborator::trait_env::ImplTargetKey::Decl(def),
                method_name,
            )
            .is_some()
    }

    /// Resolve a static-method receiver `TypeId` to its `(struct_name,
    /// decl_key)` for impl / parameter lookups: follow newtypes to the base,
    /// map flags to `u32` and builtin arrays to `core:array`.
    pub(super) fn static_receiver_struct_key(
        &self,
        target_type_id: TypeId,
    ) -> (
        Option<String>,
        Option<crate::elaborator::trait_env::ImplTargetKey>,
    ) {
        use crate::elaborator::trait_env::ImplTargetKey;
        let key: Option<ImplTargetKey> = {
            let mut current_type = target_type_id;
            loop {
                match self.tysys.type_table.borrow().get(current_type).clone() {
                    ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => {
                        break self
                            .tysys
                            .type_table
                            .borrow()
                            .nominal_def(current_type)
                            .map(|def| ImplTargetKey::of_decl(self.tysys.resolutions.defs(), def));
                    }
                    ResolvedType::Newtype { base_type, .. } => current_type = base_type,
                    ResolvedType::Flags { .. } => {
                        current_type = TypeTable::U32;
                    }
                    ResolvedType::BuiltinArray(_) => {
                        break Some(ImplTargetKey::Builtin(
                            TypeTable::ARRAY_TYPE_NAME.to_string(),
                        ));
                    }
                    _ => break None,
                }
            }
        };
        let defs = self.tysys.resolutions.defs();
        let name = key
            .as_ref()
            .and_then(|key| key.type_name(defs))
            .map(str::to_string);
        (name, key)
    }

    /// Default-value expressions for a static method's non-self parameters, in
    /// the same order as [`Self::lookup_static_method_param_types_keyed`].
    /// Returns `(param_name, default_expr)` pairs; `default_expr` is `None` for
    /// parameters without a declared default.
    pub(super) fn lookup_static_method_param_defaults(
        &mut self,
        struct_name: &str,
        method_name: &str,
        target_hint: Option<&crate::elaborator::trait_env::ImplTargetKey>,
    ) -> Vec<(String, Option<ast::Expr>)> {
        let static_key = self.static_receiver_key(struct_name, target_hint);
        // Names and defaults come out of the same record, so their order
        // matches the parameter types by construction.
        self.unique_static_method_sig(&static_key, method_name)
            .map(|sig| crate::elaborator::sig::Param::named_defaults(&sig.params))
            .unwrap_or_default()
    }

    /// The receiver a static lookup keys on: the key its caller already
    /// resolved, else the written name over the walking module's frame.
    fn static_receiver_key(
        &self,
        struct_name: &str,
        target_hint: Option<&ImplTargetKey>,
    ) -> ImplTargetKey {
        target_hint
            .cloned()
            .unwrap_or_else(|| self.impl_target(struct_name))
    }

    /// The *trait* impl blocks a receiver written `struct_name` reaches,
    /// current-module-first, with the declared name their heads must spell.
    ///
    /// A receiver reaches two namespaces: its declaration, and an impl binding
    /// the name as its own type parameter (`impl<V: Bound> Trait for V`), which
    /// keys under that binder. Both are searched in the current module, only
    /// the declaration namespace outside it.
    fn trait_impls_for_receiver(
        &self,
        struct_name: &str,
        target_hint: Option<&ImplTargetKey>,
    ) -> (Vec<crate::defs::DefId>, String) {
        let defs = self.tysys.resolutions.defs();
        let target = self.static_receiver_key(struct_name, target_hint);
        let declared_name = target.type_name(defs).unwrap_or(struct_name).to_string();

        let env = &self.tysys.trait_env;
        let declared = env.entries_by_receiver_vec(&target.receiver(defs));
        let binder = env.entries_by_receiver_vec(&Receiver::Type(FqTypeName::param_bucket(
            &self.current_module_source,
            &declared_name,
        )));
        let is_current = |k: &&crate::defs::DefId| *defs.module(**k) == self.current_module_source;
        let mut keys: Vec<crate::defs::DefId> = declared
            .iter()
            .chain(binder.iter())
            .filter(is_current)
            .copied()
            .collect();
        keys.extend(declared.iter().filter(|k| !is_current(k)).copied());
        (keys, declared_name)
    }

    /// Canonical signatures of the methods named `method_name` declared on
    /// `type_key`, current-module-first. `all_impl_index` is already in global
    /// order, so the partition needs no per-call sort.
    fn impl_method_sigs<'b>(
        &'b self,
        type_key: &ImplTargetKey,
        method_name: &str,
    ) -> Vec<&'b super::sig::MethodSig> {
        let env = &self.tysys.trait_env;
        let Some(keys) = env.all_impl_index.get(type_key) else {
            return Vec::new();
        };
        let mut current: Vec<&super::sig::MethodSig> = Vec::new();
        let mut others: Vec<&super::sig::MethodSig> = Vec::new();
        for key in keys {
            let header = &env.impl_headers[key];
            for method in header.methods.iter().filter(|m| m.name == method_name) {
                let sig = self
                    .tysys
                    .signatures
                    .method_sig(method.def)
                    .expect("the decl pass records every impl-declared method's signature");
                if *self.tysys.resolutions.defs().module(*key) == self.current_module_source {
                    current.push(sig);
                } else {
                    others.push(sig);
                }
            }
        }
        current.extend(others);
        current
    }

    /// Look up whether each non-self parameter of an instance method is `mut`.
    /// Returns empty vec (conservative) for unknown methods.
    fn lookup_method_param_is_mut(&self, type_key: &ImplTargetKey, method_name: &str) -> Vec<bool> {
        self.impl_method_sigs(type_key, method_name)
            .first()
            .map(|sig| super::sig::Param::is_mut_flags(&sig.params))
            .unwrap_or_default()
    }

    /// The return type every method the qualified spelling can name agrees on,
    /// `None` when they disagree. An overload set still answers: every `From`
    /// impl returns the receiver, so which one this call reaches cannot change
    /// the result.
    fn agreed_qualified_method_return(
        &self,
        static_key: &crate::elaborator::trait_env::ImplTargetKey,
        method_name: &str,
    ) -> Option<TypeId> {
        let mut returns = self
            .qualified_method_decl_ids(static_key, method_name)
            .filter_map(|def| self.tysys.signatures.method_sig(def))
            .map(|sig| sig.decl.return_type.unwrap_or(TypeTable::UNIT));
        let first = returns.next()?;
        returns.all(|r| r == first).then_some(first)
    }

    /// The static method declared under this name, `None` when several impls
    /// declare it and the index has nothing to choose between them. A conversion
    /// that must choose goes through [`Self::conversion_preselect`].
    fn unique_static_method_sig(
        &self,
        static_key: &crate::elaborator::trait_env::ImplTargetKey,
        method_name: &str,
    ) -> Option<&super::sig::MethodSig> {
        let mut declared = self.static_method_entries(static_key, method_name);
        let only = declared.next()?;
        if declared.next().is_some() {
            return None;
        }
        self.tysys.signatures.method_sig(only.method_id)
    }

    /// The declared type-param slots of a static method, keyed like
    /// [`Self::lookup_static_method_param_types_keyed`].
    pub(super) fn lookup_static_method_slots(
        &self,
        method_name: &str,
        static_key: &crate::elaborator::trait_env::ImplTargetKey,
    ) -> Vec<TypeId> {
        self.unique_static_method_sig(static_key, method_name)
            .map(|sig| sig.decl.type_params.iter().map(|(_, id)| *id).collect())
            .unwrap_or_default()
    }

    /// Whether each parameter of a static method is `mut`, empty for an unknown
    /// method. The receiver key is pre-resolved where the caller holds one — a
    /// namespace member's bare spelling cannot reach it.
    pub(super) fn lookup_static_method_param_is_mut(
        &self,
        struct_name: &str,
        method_name: &str,
        target_hint: Option<&crate::elaborator::trait_env::ImplTargetKey>,
    ) -> Vec<bool> {
        let type_target = self.static_receiver_key(struct_name, target_hint);
        self.impl_method_sigs(&type_target, method_name)
            .into_iter()
            .find(|sig| sig.self_kind == ast::SelfKind::None)
            .map(|sig| super::sig::Param::is_mut_flags(&sig.params))
            .unwrap_or_default()
    }

    /// Find the trait name for a static method on a struct, if the method belongs to a trait impl.
    /// Returns `None` for inherent static methods, `Some(trait_name)` for trait static methods.
    pub(super) fn has_from_synthesis_request(
        &self,
        target_type: &ast::Type,
        arg_type_id: &crate::tir::TypeId,
    ) -> bool {
        let target_name = super::trait_env::get_type_name_static(target_type);
        let arg_type_name = self.tysys.type_table.borrow().type_name(*arg_type_id);
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        self.tysys.trait_env.impl_headers.values().any(|header| {
            if !header.is_synthesize_request {
                return false;
            }
            let Some(trait_type) = &header.trait_type else {
                return false;
            };
            if header.trait_name.as_deref() != Some(from_trait_name.as_str())
                || super::trait_env::get_type_name_static(&header.ty) != target_name
            {
                return false;
            }
            matches!(trait_type, ast::Type::Generic(generic)
                if generic.args.len() == 1
                    && self.get_type_name_full(&generic.args[0]) == arg_type_name)
        })
    }

    pub(super) fn find_static_method_trait(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<crate::name::FqTraitName> {
        self.locate_static_method_impl(struct_name, method_name, None, None)
            .and_then(|r| r.trait_name)
    }

    /// Which conversion trait a static `from` / `try_from` call names.
    pub(super) fn conversion_trait_name(&self, method_name: &str) -> String {
        if method_name == "try_from" {
            "TryFrom".to_string()
        } else {
            self.tysys
                .type_table
                .borrow()
                .compiler_trait_name(crate::compiler_item::CompilerItem::From)
                .to_string()
        }
    }

    /// Report why a conversion call's argument matched no impl, when the
    /// receiver's conversion impls explain it: a blanket impl this path
    /// cannot instantiate, or concrete impls none of which accept the
    /// argument's type. Returns whether an error was emitted — the caller
    /// then stops instead of building an unresolvable mangled name (an ICE
    /// at WIR build).
    pub(super) fn report_unmatched_conversion(
        &mut self,
        struct_name: &str,
        method_name: &str,
        arg_type: &str,
        span: Span,
        target_hint: Option<&ImplTargetKey>,
    ) -> bool {
        let (candidates, has_blanket) =
            self.conversion_impl_survey(struct_name, method_name, target_hint);
        if has_blanket {
            let _ = self.emit(TypeError::UnsupportedBlanketConversion {
                trait_name: self.conversion_trait_name(method_name),
                receiver: struct_name.to_string(),
                method: method_name.to_string(),
                arg_type: arg_type.to_string(),
                span,
            });
            return true;
        }
        if !candidates.is_empty() {
            let _ = self.emit(TypeError::NoMatchingTraitArgument {
                trait_name: self.conversion_trait_name(method_name),
                receiver: struct_name.to_string(),
                method: method_name.to_string(),
                arg_type: arg_type.to_string(),
                candidates: candidates.into_iter().map(|c| c.spelling).collect(),
                span,
            });
            return true;
        }
        false
    }

    /// The shared preselect entry for a one-argument conversion call
    /// (`Wrapper::from(42)`, in either its static-call or plain-call
    /// spelling): `Selected` installs the chosen impl's source type as the
    /// argument's expected type; `Ambiguous` reports and returns `true` so
    /// the caller stops.
    pub(super) fn try_conversion_preselect(
        &mut self,
        recv_name: &str,
        method_name: &str,
        arg: &ast::Expr,
        span: Span,
        ctx: &mut FunctionContext,
        param_types: &mut Vec<TypeId>,
        target_hint: Option<&ImplTargetKey>,
    ) -> bool {
        if (method_name != "from" && method_name != "try_from")
            || self.has_inherent_static_method(recv_name, method_name, target_hint)
        {
            return false;
        }
        let class = self.synthesize_arg_class(arg, ctx);
        match self.conversion_preselect(recv_name, method_name, &class, target_hint) {
            ConversionPreselect::Selected(source) => {
                *param_types = vec![source];
                false
            }
            ConversionPreselect::Ambiguous(candidates) => {
                let _ = self.emit(TypeError::AmbiguousConversionArgument {
                    receiver: recv_name.to_string(),
                    method: method_name.to_string(),
                    candidates,
                    span,
                });
                true
            }
            ConversionPreselect::Pass => false,
        }
    }

    /// Whether any impl block among `keys` declares `method_name` taking no
    /// receiver. Headers name the methods and the signature digest says
    /// whether each takes `self`, so the question is answered without an
    /// impl-block AST — and keyed canonically, so two modules' same-named
    /// types cannot answer for each other.
    fn keys_declare_static_method(&self, keys: &[crate::defs::DefId], method_name: &str) -> bool {
        keys.iter().any(|key| {
            self.tysys
                .trait_env
                .impl_headers
                .get(key)
                .into_iter()
                .flat_map(|header| header.methods.iter())
                .any(|m| {
                    m.name == method_name
                        && self
                            .tysys
                            .signatures
                            .method_sig(m.def)
                            .is_some_and(|sig| sig.self_kind == ast::SelfKind::None)
                })
        })
    }

    /// Whether an inherent impl (`impl Type { … }`) declares a no-self method
    /// of this name. A conversion-call guard needs the distinction: a trait
    /// lookup returning `None` is a failure only when no inherent static can
    /// answer instead.
    pub(super) fn has_inherent_static_method(
        &self,
        struct_name: &str,
        method_name: &str,
        target_hint: Option<&ImplTargetKey>,
    ) -> bool {
        let target = self.static_receiver_key(struct_name, target_hint);
        let keys = self.tysys.trait_env.inherent_impl_keys(&target);
        self.keys_declare_static_method(&keys, method_name)
    }

    /// The argument preselect over a receiver's conversion impls: `Selected` and
    /// `Ambiguous` short-circuit resolution, so it decides calls. It must run
    /// *before* the argument is elaborated — the expected type shaping a literal
    /// comes from the selected impl. Admissibility is [`Elaborator::class_admits`]
    /// over each impl's *resolved* source type, since spelling under-admits.
    pub(super) fn conversion_preselect(
        &mut self,
        struct_name: &str,
        method_name: &str,
        class: &super::synth::ArgClass,
        target_hint: Option<&ImplTargetKey>,
    ) -> ConversionPreselect {
        use super::synth::ArgClass;
        if matches!(class, ArgClass::Opaque(_)) {
            return ConversionPreselect::Pass;
        }
        let (candidates, _has_blanket) =
            self.conversion_impl_survey(struct_name, method_name, target_hint);
        let admitted: Vec<ConversionCandidate> = candidates
            .into_iter()
            .filter(|c| {
                c.source != TypeTable::UNKNOWN
                    && c.source != TypeTable::ERROR
                    && self.class_admits(c.source, class)
            })
            .collect();
        match admitted.as_slice() {
            [] => ConversionPreselect::Pass,
            [only] => ConversionPreselect::Selected(only.source),
            // A `Head` names a family, not a type — `Pair { a: 5 }` is a
            // `Pair` of something — so several same-head impls are the expected
            // answer, not a tie. Only a class denoting one type may call two
            // candidates ambiguous; elaborating the argument decides the rest.
            _ if matches!(class, ArgClass::Head(_)) => ConversionPreselect::Pass,
            _ => ConversionPreselect::Ambiguous(admitted.into_iter().map(|c| c.spelling).collect()),
        }
    }

    /// The source types the receiver's conversion impls accept
    /// (`From<String>` beside `From<i64>`), each with its spelling (for
    /// diagnostics) and its type resolved in the impl's own frame (for
    /// admissibility), in candidate order, plus whether a blanket conversion
    /// impl exists. It walks the impls directly rather than sharing
    /// [`Self::locate_static_method_impl`]'s early-return traversal, because
    /// its consumers need the full candidate list.
    pub(super) fn conversion_impl_survey(
        &self,
        struct_name: &str,
        method_name: &str,
        target_hint: Option<&ImplTargetKey>,
    ) -> (Vec<ConversionCandidate>, bool) {
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let mut candidates: Vec<ConversionCandidate> = Vec::new();
        let mut has_blanket = false;
        let (impl_defs, declared_name) = self.trait_impls_for_receiver(struct_name, target_hint);
        for impl_def in impl_defs {
            let header = &self.tysys.trait_env.impl_headers[&impl_def];
            let module = self.tysys.resolutions.defs().module(impl_def).clone();
            let Some(trait_type) = header.trait_type.as_ref() else {
                continue;
            };
            let base = super::trait_env::get_type_name_static(trait_type);
            if self.impl_head_decl_name(header, &module) != declared_name
                || (base != from_trait_name && base != "TryFrom")
                || !header.methods.iter().any(|m| m.name == method_name)
            {
                continue;
            }
            let ast::Type::Generic(g) = trait_type else {
                continue;
            };
            let Some(arg) = g.args.first() else {
                continue;
            };
            // A source mentioning one of the impl's type parameters is a
            // blanket: it accepts (a family of) everything, its presence means
            // the trait-less path can resolve the call through the blanket
            // resolver, and it is never an unmatched alternative worth listing.
            if ast_type_mentions_param(arg, &header.type_params) {
                has_blanket = true;
                continue;
            }
            // Full spelling with the head un-aliased, so the alternatives read
            // `List<i32>`, not a bare `List`.
            let head = super::trait_env::get_type_name_static(arg);
            let head = self.import_original_name(&head, &module);
            let mut rendered = String::new();
            crate::unparse::unparse_type_into(arg, &mut rendered);
            let spelling = match rendered.split_once('<') {
                Some((_, args)) => format!("{head}<{args}"),
                None => head,
            };
            if candidates.iter().any(|c| c.spelling == spelling) {
                continue;
            }
            // The source type as the impl's own frame resolved it, so a
            // private or aliased name means what the impl wrote.
            let source = *self
                .tysys
                .signatures
                .impl_sig(impl_def)
                .expect("the decl pass records every impl block's declaration facts")
                .trait_type_args
                .first()
                .expect("the trait reference is generic, so its arguments were resolved");
            candidates.push(ConversionCandidate { spelling, source });
        }
        (candidates, has_blanket)
    }

    /// The original (un-aliased) name `name` resolves to *within `module`* — its
    /// `use { Original as name }` original, or `name` itself when not aliased.
    /// Resolving in the impl's own module (not the call site) makes `From`-impl
    /// matching independent of whatever alias the caller uses for the source
    /// type.
    fn import_original_name(&self, name: &str, module: &ModuleSource) -> String {
        // One question — what did `module` import under this name — asked of
        // the module whatever it is, rather than of two maps chosen by whether
        // it happens to be the frame's own.
        self.tysys
            .resolutions
            .imported_as(module, name)
            .map_or_else(
                || name.to_string(),
                |def| self.tysys.resolutions.defs().name(def).to_string(),
            )
    }

    /// An impl header's target head as a declaration name, resolved through the
    /// impl's own imports — unless its type parameters bind the spelling, which
    /// shadows them.
    fn impl_head_decl_name(
        &self,
        header: &super::trait_env::ImplHeader,
        impl_module: &ModuleSource,
    ) -> String {
        let head = super::trait_env::get_type_name_static(&header.ty);
        if header.type_params.iter().any(|p| p.name == head) {
            return head;
        }
        self.import_original_name(&head, impl_module)
    }

    /// Whether `rendered` names `param` as a whole segment — the spelling-level
    /// stand-in for "this type mentions the impl's type parameter".
    fn mentions_type_param(rendered: &str, param: &str) -> bool {
        rendered
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|seg| seg == param)
    }

    pub(super) fn locate_static_method_impl(
        &self,
        struct_name: &str,
        method_name: &str,
        arg_type_name: Option<&str>,
        target_hint: Option<&ImplTargetKey>,
    ) -> Option<StaticMethodRef> {
        let (impl_defs, declared_name) = self.trait_impls_for_receiver(struct_name, target_hint);
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let is_from_or_try_from =
            |base: &str| -> bool { base == from_trait_name || base == "TryFrom" };
        // A `From` / `TryFrom` impl discriminates its methods by the source
        // type, so its trait segment keeps the argument; every other trait
        // names the declaration alone.
        let resolve_trait_name =
            |header: &super::trait_env::ImplHeader| -> Option<crate::name::FqTraitName> {
                let fq = self
                    .tysys
                    .trait_env
                    .fq_trait_of_impl(header, &self.tysys.resolutions)?;
                Some(if is_from_or_try_from(fq.base_name()) {
                    fq
                } else {
                    fq.head_only()
                })
            };

        let matches_arg_type = |trait_type: &ast::Type,
                                impl_ty: &ast::Type,
                                impl_module: &ModuleSource,
                                type_params: &[ast::GenericParam]|
         -> bool {
            let Some(expected) = arg_type_name else {
                return true;
            };
            let base = super::trait_env::get_type_name_static(trait_type);
            if is_from_or_try_from(&base)
                && let ast::Type::Generic(g) = trait_type
                && let Some(arg) = g.args.first()
            {
                // A blanket source (`impl<T: Display> From<T> for Wrapper`)
                // is deliberately NOT matched here: baking its unsubstituted
                // `From<T>` spelling into the mangled name defeats the
                // instantiation `resolve_blanket_static_method` performs.
                // Rejecting it sends the call down the trait-less path, where
                // the blanket resolver picks it up.
                if let ast::Type::Named(n) = arg
                    && type_params.iter().any(|p| p.name == n.name)
                {
                    return false;
                }
                // Un-alias the impl's source-type head *in the impl's module*
                // before comparing: `impl From<ClockInstant>` (where
                // `ClockInstant` is `use { Instant as ClockInstant }`) must match
                // a call whose argument's real name is `Instant`, regardless of
                // the alias the caller used. The verbatim name would miss the
                // impl and fall back to a (non-existent) inherent `Type::from`.
                let head = super::trait_env::get_type_name_static(arg);
                let head = self.import_original_name(&head, impl_module);
                let expected_head = expected.split('<').next().unwrap_or(expected);
                if head != expected_head {
                    return false;
                }
                // A bare-head argument spelling is fully compared already. A
                // generic one must match its arguments too (whitespace
                // ignored), or two impls sharing a head (`From<List<i32>>`
                // beside `From<List<String>>`) both answer and the first one
                // wins wrongly. Nested aliasing can make the spellings
                // disagree and miss an impl — the name-based hint mechanism's
                // ceiling; TypeId matching is the replacement
                // (WEP 2026-07-31 phase 4).
                if !expected.contains('<') {
                    return true;
                }
                let mut rendered = String::new();
                crate::unparse::unparse_type_into(arg, &mut rendered);
                // A source type generic in the impl's own parameters
                // (`impl From<Array<T>> for List<T>`) is spelled with those
                // parameters, so no instantiation ever equals it verbatim. The
                // head is what separates it from a sibling impl, and the
                // mangled name carries the impl's spelling either way.
                let declared = self.tysys.build_declared_type_params(impl_ty, type_params);
                if declared
                    .iter()
                    .any(|name| Self::mentions_type_param(&rendered, name))
                {
                    return true;
                }
                let full: String = match rendered.split_once('<') {
                    Some((_, args)) => format!("{head}<{args}"),
                    None => head,
                };
                let strip = |t: &str| t.replace(' ', "");
                return strip(&full) == strip(expected);
            }
            !is_from_or_try_from(&base)
        };

        // Returns the trait the impl names and the method it declares there —
        // the identity of what this selection picked, so a caller recording a
        // use→def edge names the impl the argument chose rather than the
        // receiver's first same-named method.
        let check_impl = |header: &super::trait_env::ImplHeader,
                          impl_module: &ModuleSource|
         -> Option<(crate::name::FqTraitName, crate::defs::DefId)> {
            let trait_type = header.trait_type.as_ref()?;
            if self.impl_head_decl_name(header, impl_module) != declared_name
                || !matches_arg_type(trait_type, &header.ty, impl_module, &header.type_params)
            {
                return None;
            }
            for method in header.methods.iter().filter(|m| m.name == method_name) {
                let sig = self
                    .tysys
                    .signatures
                    .method_sig(method.def)
                    .expect("the decl pass records every impl-declared method's signature");
                if sig.self_kind == ast::SelfKind::None {
                    return Some((resolve_trait_name(header)?, method.def));
                }
            }
            // Fall back to the trait declaration's default methods: when
            // `impl Trait for Type` does not override a defaulted static
            // method, the trait still provides the body, so `Type::method`
            // (called concretely, not via a generic bound) must resolve to
            // the trait's default. This mirrors how generic dispatch
            // (`T::method()`) already finds default methods.
            let trait_name_base = super::trait_env::get_type_name_static(trait_type);
            if let Some(method) = self
                .trait_sig_by_name(&trait_name_base)
                .and_then(|sig| sig.method(method_name))
                && method.default_body.is_some()
                && method.sig.self_kind == ast::SelfKind::None
            {
                return Some((resolve_trait_name(header)?, method.sig.def));
            }
            None
        };

        for impl_def in impl_defs {
            let header = &self.tysys.trait_env.impl_headers[&impl_def];
            let module_source = self.tysys.resolutions.defs().module(impl_def).clone();
            if let Some((trait_name, method_id)) = check_impl(header, &module_source) {
                return Some(StaticMethodRef::new(
                    module_source,
                    struct_name,
                    method_name,
                    Some(trait_name),
                    Some(method_id),
                ));
            }
        }

        if method_name == "default"
            && let Some(struct_type) = self
                .tysys
                .auto_derive_default_struct_type(&self.type_lookup(), struct_name)
        {
            let default_trait_name = self
                .tysys
                .type_table
                .borrow()
                .compiler_trait_fq(crate::compiler_item::CompilerItem::Default);
            let module_source = self.declaring_module_of(struct_name);
            self.tysys
                .type_table
                .borrow_mut()
                .record_bound_driven_synth_request_for(
                    struct_type,
                    &module_source,
                    &default_trait_name
                        .canonical()
                        .expect("a compiler trait item names a declaration"),
                );
            return Some(StaticMethodRef::new(
                module_source,
                struct_name,
                method_name,
                Some(default_trait_name),
                None,
            ));
        }

        None
    }

    /// Get the operator trait and method name for a binary operator.
    pub(super) fn is_static_method(&self, struct_name: &str, method_name: &str) -> bool {
        self.is_static_method_at(None, struct_name, method_name)
    }

    /// [`Self::is_static_method`] for a receiver written at a reference site.
    /// The site decides which declaration `struct_name` names; see
    /// [`Elaborator::impl_target_at`].
    pub(super) fn is_static_method_at(
        &self,
        site: Option<crate::ast::AstId>,
        struct_name: &str,
        method_name: &str,
    ) -> bool {
        let mangled_name = MethodName::format_local(
            &self.qualified_receiver_name(struct_name),
            None,
            method_name,
        );

        // Check if it's registered in function_return_types (static methods are registered there)
        if self
            .sem
            .decls
            .function_return_types
            .contains_key(&mangled_name)
        {
            return true;
        }

        // O(1) lookup via pre-built static method index (impl blocks).
        // Canonicalise so a same-named struct in another module doesn't
        // accidentally claim this name.
        let static_key = self.impl_target_at(site, struct_name);
        if let Some(methods) = self.tysys.trait_env.static_method_index.get(&static_key)
            && methods.iter().any(|e| e.name == method_name)
        {
            return true;
        }

        // The index holds only what `TraitEnv::build` classified as a static
        // method; ask the headers directly for the rest.
        if self.keys_declare_static_method(
            &self
                .tysys
                .trait_env
                .all_impl_keys(&self.impl_target_at(site, struct_name)),
            method_name,
        ) {
            return true;
        }

        if self
            .tysys
            .trait_env
            .resource_static(&static_key, method_name)
            .is_some()
        {
            return true;
        }

        // Same walk as `lookup_static_method_return_type`: the index holds
        // only the declaring resource's own methods.
        if let super::trait_env::ImplTargetKey::Decl(def) = &static_key
            && self.resource_instance_method(*def, method_name).is_some()
        {
            return true;
        }

        // For newtypes/flags, check if the base type has the static method
        if let Some((_, base_name)) = self.newtype_base(struct_name)
            && self.is_static_method(&base_name, method_name)
        {
            return true;
        }

        // Auto-derived `Default::default()` for structs whose fields all have
        // default expressions. No user impl exists (previous checks would have
        // caught it), but `synthesis::traits` will emit the body.
        if method_name == "default"
            && self
                .tysys
                .auto_derive_default_struct_type(&self.type_lookup(), struct_name)
                .is_some()
        {
            return true;
        }

        // `ReflectStruct` metadata is reachable only through the trait-qualified form
        // `ReflectStruct::<T>::method()` (see `resolve_call`), never as a bare
        // `T::method()` static method — that keeps struct namespaces clean.

        // Defaulted trait method: when `impl Trait for Type` does not
        // override a static method that the trait provides a default for,
        // `Type::method` must still resolve. `locate_static_method_impl`
        // applies the same fallback to find the trait name and module.
        if self
            .locate_static_method_impl(struct_name, method_name, None, None)
            .is_some()
        {
            return true;
        }

        false
    }

    /// Resolve a static method call from a qualified name like `Point::origin()`
    pub(super) fn resolve_static_method_call_from_qualified(
        &mut self,
        struct_name: &str,
        method_name: &str,
        args: &[TypeId],
        impl_type_args: &[TypeId],
        method_type_args: &[TypeId],
        call_id: AstId,
        span: Span,
        _ctx: &mut FunctionContext,
    ) -> TypeId {
        // The call site may refer to the receiver type through a
        // `use { Counter as CounterA }` alias. Resolve the alias to its
        // canonical declaration name so the mangled TIR function
        // (`Counter::make`) can be found at WIR-build time — that name
        // is keyed by the *original* `Counter`, not the local alias.
        // The other lookups below still consume `struct_name` as-is and
        // canonicalise internally via `Elaborator::decl_key_or_local`.
        // Rebuilt from the canonical key, not the local alias.
        let qualified_struct_name = self.qualified_receiver_name(struct_name);
        let mangled_func_name_owned =
            MethodName::format_local(&qualified_struct_name, None, method_name);
        let mangled_func_name = mangled_func_name_owned.as_str();
        // For newtypes, check if the newtype itself has the method first,
        // then fall back to the base type's static method
        let mut newtype_dispatch: Option<(TypeId, TypeId, Vec<TypeId>)> = None;
        // The written name keys the impl indices; the fq form names the method.
        let (actual_struct_name, actual_struct_fq, actual_mangled_name) = if let Some(newtype_id) =
            self.lookup_newtype(struct_name)
        {
            // First check if the newtype itself has this static method
            if self.has_static_method_direct(struct_name, method_name) {
                (
                    struct_name.to_string(),
                    qualified_struct_name,
                    mangled_func_name.to_string(),
                )
            } else {
                let base_type_id = match self.tysys.type_table.borrow().get(newtype_id).clone() {
                    ResolvedType::Newtype { .. } => Some(
                        self.tysys
                            .type_table
                            .borrow()
                            .representation_head(newtype_id),
                    ),
                    _ => None,
                };
                let base_name = base_type_id
                    .map(|b| self.tysys.get_ultimate_base_struct_name(b))
                    .or_else(|| match self.tysys.type_table.borrow().get(newtype_id) {
                        ResolvedType::Flags { .. } => Some("u32".to_string()),
                        _ => None,
                    });
                if let (Some(base_name), Some(base_type_id)) = (base_name.clone(), base_type_id) {
                    let base_args = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_type_args(base_type_id)
                        .unwrap_or_default();
                    newtype_dispatch = Some((newtype_id, base_type_id, base_args));
                    let base_fq = self.tysys.fq_receiver_head(base_type_id);
                    let mangled = MethodName::format_local(&base_fq, None, method_name);
                    (base_name, base_fq, mangled)
                } else if let Some(base_name) = base_name {
                    let base_fq = self.qualified_receiver_name(&base_name);
                    let mangled = MethodName::format_local(&base_fq, None, method_name);
                    (base_name, base_fq, mangled)
                } else {
                    (
                        struct_name.to_string(),
                        qualified_struct_name,
                        mangled_func_name.to_string(),
                    )
                }
            }
        } else {
            (
                struct_name.to_string(),
                qualified_struct_name,
                mangled_func_name.to_string(),
            )
        };

        let impl_type_args_owned: Vec<TypeId> = match &newtype_dispatch {
            Some((_, _, base_args)) if impl_type_args.is_empty() && !base_args.is_empty() => {
                base_args.clone()
            }
            _ => impl_type_args.to_vec(),
        };
        let impl_type_args = impl_type_args_owned.as_slice();

        // Find trait name and the module where the impl block lives.
        // For From/TryFrom, disambiguate by matching the first argument's type so that
        // user-defined `impl From<MyType> for i32` is resolved to its actual defining module
        // rather than the default `ModuleSource::primitive()` for `i32`.
        let arg_type_hint =
            if (method_name == "from" || method_name == "try_from") && args.len() == 1 {
                Some(self.tysys.type_table.borrow().type_name(args[0]))
            } else {
                None
            };
        // A newtype's static call dispatches to its base, whose name is not
        // the caller's to resolve — that frame can hold a same-named
        // declaration of its own.
        let receiver_key = newtype_dispatch.as_ref().map(|(_, base_type_id, _)| {
            self.impl_target_of(
                *base_type_id,
                &crate::name::DeclName::new(&actual_struct_name),
            )
        });
        let resolved = self.locate_static_method_impl(
            &actual_struct_name,
            method_name,
            arg_type_hint.as_deref(),
            receiver_key.as_ref(),
        );
        // The expected type that shaped the argument came from
        // `lookup_static_method_param_types_keyed`, which keys on (receiver,
        // method) alone — with two conversion impls it can be a different
        // impl's than the one the argument's type then selects. Left alone the
        // mangled name loses its trait and reaches WIR build unresolved, so the
        // disagreement is reported here instead of ICE-ing there.
        if resolved.is_none()
            && let Some(arg_type) = arg_type_hint.as_deref()
            && !self.has_inherent_static_method(
                &actual_struct_name,
                method_name,
                receiver_key.as_ref(),
            )
            && self.report_unmatched_conversion(
                &actual_struct_name,
                method_name,
                arg_type,
                span,
                receiver_key.as_ref(),
            )
        {
            return TypeTable::ERROR;
        }

        let method_ref = resolved.unwrap_or_else(|| {
            StaticMethodRef::new(
                self.declaring_module_of(&actual_struct_name),
                &actual_struct_name,
                method_name,
                None,
                None,
            )
        });

        // Use trait-qualified mangled name if this is a trait method
        let final_mangled_name = if let Some(ref trait_name) = method_ref.trait_name {
            MethodName::format_local(&actual_struct_fq, Some(trait_name), method_name)
        } else {
            actual_mangled_name
        };

        // Look up return type using the actual struct name
        let mut return_type = self.lookup_static_method_return_type(
            &method_ref,
            &actual_struct_fq,
            &final_mangled_name,
        );

        // Substitute impl-level + method-level type parameters in return type.
        // `lookup_static_method_return_type` registers impl params at indices
        // 0..impl_count and method params at indices impl_count..total, so a
        // single flat substitution list `[impl_args.., method_args..]` lines
        // up correctly with `substitute_type_params` (which substitutes by index).
        if !impl_type_args.is_empty() || !method_type_args.is_empty() {
            let mut combined = impl_type_args.to_vec();
            combined.extend_from_slice(method_type_args);
            return_type = self.tysys.substitute_type_params(return_type, &combined);
        }

        if let Some((newtype_id, base_type_id, _)) = newtype_dispatch
            && return_type == base_type_id
        {
            return_type = newtype_id;
        }

        // Build monomorph_info for impl-level and/or method-level generic instantiation
        let monomorph_info = if impl_type_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            Some(MonomorphInfo {
                generic_name: final_mangled_name.clone(),
                impl_type_args: impl_type_args.to_vec(),
                method_type_args: method_type_args.to_vec(),
                is_blanket: false,
            })
        };

        // The signature the spelling names, whichever kind of method it is. The
        // dispatch derives its per-parameter lists from it, so they agree with
        // each other and with where the receiver sits.
        let callee_sig = self.qualified_method_sig(&actual_struct_name, method_name);

        // Propagate #[cm("...")] from resource static methods. A method the
        // *resource* declares names it as its own owner; one an `impl` block
        // declares owns to the block, which names no resource, so the spelling
        // answers there as it did before impl methods were identified.
        let defs = self.tysys.resolutions.defs();
        let cm_owner = method_ref
            .method_id
            .and_then(|method| defs.parent(method))
            .filter(|owner| defs.kind(*owner) == crate::defs::DefKind::Resource)
            .or_else(|| self.decl_key_or_local(&actual_struct_name));
        let cm_name = self.lookup_resource_static_cm(cm_owner, method_name);

        let StaticMethodRef {
            module: struct_module,
            trait_name: trait_name_opt,
            ..
        } = method_ref;

        let func_ref = FunctionRef {
            module_source: struct_module,
            name: final_mangled_name,
            monomorph_info,
            method_info: Some({
                let mut m =
                    LocalMethodName::new(actual_struct_fq, trait_name_opt, method_name.to_string());
                m.cm_name = cm_name;
                m
            }),
        };

        // Record the static-method dispatch decision so reify can reproduce
        // the same `Call` shape without re-running impl lookup, mangled-name
        // construction, or monomorph-info shaping.
        if let Some(sig) = &callee_sig {
            self.sem.types.static_method_dispatch.insert(
                call_id,
                super::sem::types::StaticMethodDispatch::of_signature(
                    method_ref.method_id,
                    func_ref,
                    vec![],
                    sig,
                    true,
                ),
            );
        }

        return_type
    }
}

/// See [`Elaborator::conversion_preselect`].
pub(super) enum ConversionPreselect {
    /// Exactly one conversion impl admits the literal: elaborate the argument
    /// against this source type, and the name hint then finds the same impl.
    Selected(TypeId),
    /// Several impls admit the literal — a literal never selects, so the call
    /// is reported with the admitted alternatives.
    Ambiguous(Vec<String>),
    /// The preselect does not apply (non-literal argument, no admitted
    /// candidate, or an unresolvable source type): the existing path decides.
    Pass,
}

/// One non-blanket conversion impl's source type: the spelling for
/// diagnostics, the resolved type for admissibility. See
/// [`Elaborator::conversion_impl_survey`].
pub(super) struct ConversionCandidate {
    pub(super) spelling: String,
    pub(super) source: TypeId,
}

/// Whether an AST type syntactically mentions one of `params`. Shapes the
/// walk does not descend into count as mentioning, so a caller skipping
/// resolution for open types never resolves one by mistake.
fn ast_type_mentions_param(ty: &ast::Type, params: &[ast::GenericParam]) -> bool {
    match ty {
        ast::Type::Named(n) => params.iter().any(|p| p.name == n.name),
        ast::Type::Generic(g) => {
            params.iter().any(|p| p.name == g.name)
                || g.args.iter().any(|a| ast_type_mentions_param(a, params))
        }
        ast::Type::Tuple(elems) => elems.iter().any(|e| ast_type_mentions_param(e, params)),
        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
            ast_type_mentions_param(inner, params)
        }
        _ => !params.is_empty(),
    }
}
