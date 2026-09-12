//! Method call and static method call resolution.

use super::trait_env::ImplTargetKey;
use crate::ast::{self, AstId};
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, Receiver, RefKind};
use crate::tir::{
    FunctionRef, MonomorphInfo, ResolvedType, SubstitutionContext, TypeId, TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::call::SigChoice;
use super::callee::StaticMethodRef;
use super::coercion::is_numeric_literal_arg;
use super::expr::IndexAccess;
use super::infer::InferCtx;
use super::instantiate::Instantiation;
use super::method_lookup::MethodInferenceInput;
use super::reflect::ReflectDispatch;
use super::sem::types::{CalleeParams, StaticMethodDispatch};
use super::sig::{MethodSig, Param};
use super::static_call::{CandidateKind, Selector, StaticLookup, StaticQuery};
use super::synth::ArgClass;
use super::types::{FunctionContext, MethodInfo, MethodOwner, TypeError};
use crate::compiler_item::CompilerItem;
use crate::elaborator::ast::Expr;
use crate::elaborator::call::{merge_turbofish_type_args, turbofish_has_hole, turbofish_holes};
use crate::elaborator::expr::MemberOwner;
use crate::elaborator::method_lookup::adjusted_receiver_type;
use crate::elaborator::sig;
use crate::elaborator::synth::ArgProbe;
use crate::elaborator::trait_env::{
    BlanketBound, BlanketReceiver, ImplHeader, get_type_name_static,
};
use crate::elaborator::types::{ImplMemberKind, RequiredTrait};
use crate::name::{DeclName, FqTraitName};
use crate::resolve::Resolution;
use crate::unparse::unparse_type_into;
use crate::{hashmap, tir};

/// A static call named the way [symbol notation] writes it — the receiver's
/// type arguments included (`List<i32>::with_capacity`). Rendering only the
/// head collapsed `Take<A>::take` and `Take<B>::take` onto one string, so a
/// diagnostic naming both could not tell them apart.
///
/// [symbol notation]: ../../../docs/wep-2026-06-14-symbol-notation.md
fn static_call_symbol_name(static_call: &ast::StaticMethodCallExpr) -> String {
    let mut name = String::new();
    unparse_type_into(&static_call.target_type, &mut name);
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
    pub required_trait: Option<RequiredTrait>,
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
    pub method_def: Option<DefId>,
}

/// The value blanket a static call dispatches through.
pub(super) struct BlanketStatic {
    pub trait_name: FqTraitName,
    /// The receiver parameter as written (`T`) — what the static-method
    /// indices key on.
    pub param: String,
    pub binder: FqTypeName,
    pub module: ModuleSource,
    pub def: DefId,
}

pub(super) struct MethodSignatureFacts {
    pub param_is_mut: Vec<bool>,
    pub param_names: Vec<String>,
    pub param_defaults: Vec<Option<ast::Expr>>,
    pub param_types: Vec<TypeId>,
    pub self_kind: ast::SelfKind,
    /// The scope `param_defaults` resolve in, where the selected method is not
    /// the declaration that wrote them.
    pub defaults_module: Option<ModuleSource>,
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
        let type_arg_holes = if turbofish_has_hole(&method_call.type_args) {
            turbofish_holes(&method_call.type_args)
        } else {
            Vec::new()
        };

        let outcome = self.resolve_method_call_with(
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
        );

        // A `&mut self` method mutates the element `xs[i]` names, so the
        // receiver has to be that element. The desugar above owns every element
        // a `&mut` writes through; for the rest a by-value copy would take the
        // mutation and be thrown away. Record the aliasing subscript instead,
        // which is the borrow `&mut xs[i]` takes, and leave the write-back pass
        // to write it back or refuse it.
        if let ast::Expr::Index(index_expr) = &method_call.receiver
            && outcome.dispatch.as_ref().is_some_and(|dispatch| {
                dispatch.self_kind == ast::SelfKind::MutRef && !dispatch.is_ref_impl
            })
            && self.index_element_denies_ref_mut(index_expr, ctx)
        {
            self.resolve_index_access(index_expr, ctx, IndexAccess::Mutable);
        }

        outcome.type_id
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
        let mut probe = ArgProbe::new(args_ast, ctx);

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
        let mut trait_name: Option<FqTraitName> = None;
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
            let value_key = this.impl_target_of(base_type_id, &DeclName::new(&struct_name));
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
            && self.tysys.type_table.borrow().is_unrestricted_resource(def)
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
                &self.impl_target_of(base_type_id, &DeclName::new(&struct_name)),
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
                    let named: Vec<FqTraitName> = bounds.into_iter().collect();
                    // Each rebuilt bound is paired with the identity it stands
                    // for by its own id, not by its name: two same-named traits
                    // from different modules are two bounds, and a by-name map
                    // would collapse them back into one.
                    let mut resolved: hashmap::IndexMap<AstId, FqTraitName> =
                        hashmap::IndexMap::default();
                    let bounds: Vec<ast::TraitBound> = named
                        .iter()
                        .map(|b| {
                            let id = AstId::fresh();
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
            param_is_mut,
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
            defaults_module,
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
                defaults_module: None,
            }
        };

        self.check_inherent_member_visibility(
            inherent_visibility,
            inherent_impl_module.as_ref(),
            MemberOwner::Type(base_type_id),
            method_name,
            ImplMemberKind::Method,
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

        // Instantiate the method's own slots before an argument is resolved
        // against one of its parameter types, as `resolve_call` does for a free
        // function. A rigid slot is the declaration's own: a closure passed to
        // `fn(Acc, Item) -> Acc` would meet an `Acc` no expression can
        // construct, where against a variable its body defers and a sibling
        // argument answers it. The lookup already instantiated the declaring
        // level, so only these slots remain.
        let arg_inst = (!method_type_param_ids.is_empty()).then(|| {
            self.instantiate(
                &method_type_param_ids,
                &Instantiation {
                    kind: "method",
                    name: method_name,
                    span,
                },
            )
        });
        // Carry each slot's declared bounds onto the variable now. An argument
        // may pin a variable here and nowhere else, and the pinned answer is
        // the only thing a bound was ever written about.
        if let Some(inst) = &arg_inst {
            self.record_slot_bounds(inst, &method_own_params, span);
        }
        let instantiated_param_types = arg_inst
            .as_ref()
            .map(|inst| self.instantiate_types(&expected_param_types, inst));
        let arg_param_types = instantiated_param_types
            .as_ref()
            .unwrap_or(&expected_param_types);

        // Resolve arguments with coercion using method parameter types
        let mut args: Vec<TypeId> =
            self.resolve_args_against_params(args_ast, ctx, arg_param_types);

        if let Some(inst) = &arg_inst {
            self.settle_onto_slots(inst, &method_type_param_ids, &mut args);
        }

        // The module that declares this method: the scope its own defaults —
        // parameter values and type-parameter defaults alike — resolve in,
        // since a default may name a type the call site cannot (WEP
        // 2026-04-11). The chain `method_module_source` takes below, without
        // its inherited-owner steps. A default the method did not write itself,
        // a trait's on an impl of it, names its own declaring module.
        let callee_module = defaults_module
            .clone()
            .or_else(|| trait_impl_module_source.clone())
            .or_else(|| inherent_impl_module.clone())
            .unwrap_or_else(|| struct_module.clone());

        // Pad missing trailing args with declared parameter defaults.
        // Earlier-parameter references inside a default (e.g. `fn f(w, h = w)`)
        // are handled by substituting the caller's arg ASTs for those parameter
        // names before resolving, mirroring the free-function path in
        // `pad_args_with_defaults`.
        if args.len() < expected_param_types.len() && !param_defaults.is_empty() {
            let mut subs: hashmap::IndexMap<String, ast::Expr> = hashmap::IndexMap::default();
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
            self.pin_arg_hole_against(arg, expected_type);
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

        receiver = adjusted_receiver_type(receiver, self_kind, is_ref_impl, &self.tysys.type_table);

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
                trait_decl: trait_name.as_ref().and_then(FqTraitName::canonical),
                declaring_module: Some(callee_module),
                span,
            });
            if type_args.is_empty() {
                inferred
            } else {
                let mut merged = type_args;
                merge_turbofish_type_args(&mut merged, &type_arg_holes, &inferred);
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
                self.typecheck(*arg, expected, args_ast.get(i).map_or(span, Expr::span));
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
        let is_type_param_receiver = self
            .tysys
            .type_table
            .borrow()
            .receiver_head_awaits_substitution(base_type_id);
        let base_receiver = match matched_ref_kind {
            Some(kind) => Receiver::Ref(kind),
            None => Receiver::Type(base_struct_name),
        };
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
            defaults_module: defaults_module.clone(),
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
        self.trait_declares_method(trait_name, method_name, |kind| kind != ast::SelfKind::None)
    }

    /// [`Self::is_trait_instance_method`] for the receiver-less kind — what
    /// `Trait::<T>::method(…)` binds `Self` for, since it has no receiver
    /// argument to pin it.
    pub(super) fn is_trait_static_method(&self, trait_name: &str, method_name: &str) -> bool {
        self.trait_declares_method(trait_name, method_name, |kind| kind == ast::SelfKind::None)
    }

    fn trait_declares_method(
        &self,
        trait_name: &str,
        method_name: &str,
        of_kind: impl Fn(ast::SelfKind) -> bool,
    ) -> bool {
        self.decl_key_or_local(trait_name).is_some_and(|key| {
            self.tysys.trait_env.declares_trait(&key)
                && self
                    .trait_sig_of(&key)
                    .and_then(|sig| sig.method(method_name))
                    .is_some_and(|m| of_kind(m.sig.self_kind))
        })
    }

    /// `Trait::method(recv, args…)` — the receiver is the first argument, so
    /// dispatch is the ordinary method-call path with the named trait as a
    /// constraint on which impl may answer.
    pub(super) fn resolve_trait_qualified_call(
        &mut self,
        head_site: Option<AstId>,
        trait_name: &str,
        method_name: &str,
        call: &ast::CallExpr,
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let required = RequiredTrait {
            // The site's own answer. A prefix naming a type-parameter binder
            // is a `Binder`, which matches no trait declaration — the same
            // outcome the fabricated key produced, said rather than simulated.
            decl: head_site.map_or(Resolution::Unresolved, |site| {
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
        required_trait: RequiredTrait,
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
                StaticMethodDispatch {
                    method_def: dispatched.method_def,
                    defaults_module: sig
                        .defaults_module
                        .unwrap_or_else(|| function_ref.module_source.clone()),
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
        self.resolve_static_method_call_of_trait(static_call, None, ctx)
    }

    /// [`Self::resolve_static_method_call`] restricted to one trait's impls,
    /// which is what a `Trait::<T>::method(…)` spelling names.
    fn resolve_static_method_call_of_trait(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        required_trait: Option<DefId>,
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
                ReflectDispatch::Template => {
                    self.resolve_reflect_template_static_call(self_ty, static_call, ctx)
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
                let required = RequiredTrait {
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
            // `Tagged::<V>::tag(…)` — the static counterpart. A receiver-less
            // declaration has no receiver argument to pin `Self`, so the
            // turbofish supplies it and the call reads as `V::tag(…)`. Only
            // where the trait declares no parameters of its own: the branch
            // above claims the turbofish for a trait that does, and there is
            // then nowhere left to write `Self`.
            if let [self_ty_ast] = g.args.as_slice()
                && self.is_trait_static_method(&g.name, &static_call.method)
                && self
                    .decl_key_at(g.id, &g.name)
                    .and_then(|key| self.trait_decl_type_params_of(&key))
                    .is_none_or(|params| params.is_empty())
                && self.resolve_type(self_ty_ast) != TypeTable::UNKNOWN
            {
                let mut on_self = static_call.clone();
                on_self.target_type = self_ty_ast.clone();
                // Restricted to the named trait: the rewritten spelling reads
                // as `V::tag(…)`, and without it a case or an inherent static
                // `V` declares of that name answers in the trait's place.
                let required = self.tysys.resolutions.declared(g.id);
                return self.resolve_static_method_call_of_trait(&on_self, required, ctx);
            }
            // The same spelling on a trait that does declare parameters: the
            // turbofish is already spoken for, so say that rather than let the
            // call read as an unknown function.
            if self.is_trait_static_method(&g.name, &static_call.method)
                && self
                    .decl_key_at(g.id, &g.name)
                    .and_then(|key| self.trait_decl_type_params_of(&key))
                    .is_some_and(|params| !params.is_empty())
            {
                let _ = self.emit(TypeError::StaticNeedsWrittenReceiver {
                    trait_name: self.declared_trait_name(&g.name),
                    method: static_call.method.clone(),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            }
            let _ = self.emit(TypeError::UnknownFunction {
                name: static_call_symbol_name(static_call),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        // Extract struct name AND canonical decl key for parameter type
        // lookup (follow newtypes to base). The canonical key disambiguates
        // two modules' same-named structs whose methods both live in the
        // global `ImplMethodIndex`.
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

        // Literal preselect for a static call (WEP 2026-07-31 phase 4): choose
        // the impl before the arguments are elaborated, so their expected types
        // come from the selected impl instead of whichever the name-keyed index
        // returns first — the circular ordering this WEP diagnoses. It runs
        // *before* the resolution below and keys it, so the parameter list and
        // the mangled name come from one answer.
        let preselected = match &struct_name_for_lookup {
            Some(recv_name) => {
                let recv_name = recv_name.clone();
                self.preselect_static_args(
                    StaticReceiver {
                        key: struct_key_for_lookup.as_ref(),
                        ty: Some(target_type_id),
                        required_trait,
                        ..StaticReceiver::of(&recv_name)
                    },
                    &static_call.method,
                    &static_call.args,
                    static_call.span,
                    ctx,
                )
            }
            None => PreselectedArg::Undecided,
        };
        if matches!(preselected, PreselectedArg::Reported) {
            return TypeTable::ERROR;
        }
        let preselected = preselected.picked();
        // The selected impl's parameters stand in for the arguments the
        // resolution has not elaborated yet. Empty where nothing was picked,
        // which admits every candidate.
        let arg_types: Vec<TypeId> = preselected.clone().unwrap_or_default();

        let callee_sig = static_receiver
            .as_ref()
            .and_then(|key| self.unique_qualified_method_sig_keyed(key, &static_call.method));
        let resolved = match (&static_receiver, &struct_name_for_lookup) {
            (Some(receiver), Some(name)) => {
                let name = name.clone();
                self.static_callee_params(
                    receiver,
                    target_type_id,
                    &static_call.method,
                    &name,
                    &arg_types,
                    required_trait,
                )
            }
            _ => StaticLookup::NotStatic,
        };
        if self.report_ambiguous_static(&resolved, &static_call.method, static_call.span) {
            return TypeTable::ERROR;
        }
        let (callee_params, declares_params) = resolved.params();
        let CalleeParams {
            param_is_mut,
            param_defaults: static_method_defaults,
            mut param_types,
            self_in_args,
            defaults_module,
        } = callee_params;

        if let Some(picked) = &preselected {
            PreselectedArg::shape(&mut param_types, picked);
        }

        // The module those defaults were written in, so their bodies answer to
        // it rather than to this call site. The signature answers for a default
        // its own declaration did not write — a trait's, on an impl's method.
        let static_method_module = defaults_module.or_else(|| {
            static_receiver.as_ref().and_then(|receiver| {
                self.static_method_entry(receiver, &static_call.method)
                    .map(|e| e.module.clone())
            })
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
        let mut method_type_args: Vec<TypeId> = static_call
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
                && let Some(sig) = callee_sig.as_ref()
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
                // `param_types` leads with the receiver exactly where the
                // spelling wrote one, so the instantiated list must start at
                // the same parameter.
                let skip = if self_in_args {
                    0
                } else {
                    sig.first_value_param().min(instantiated.param_types.len())
                };
                for (param_type, &instantiated_type) in param_types
                    .iter_mut()
                    .zip(&instantiated.param_types[skip..])
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
        let mut arg_spans: Vec<Span> = static_call.args.iter().map(Expr::span).collect();

        // A static's own slots, where the spelling wrote none. With no slots of
        // its own the block leaves the method's numbered from zero and the
        // receiver's substitution reaches them anyway; with slots of its own it
        // does not, so they are solved here from the arguments — as instance
        // dispatch solves them — and an unsolved one is reported rather than
        // left to reach codegen unsubstituted.
        if let Some(sig) = callee_sig
            && static_call.type_args.is_empty()
            && sig.declaring_slot_count > 0
            && let Some(own) = sig.own_params.first()
            && let Some(receiver) = struct_name_for_lookup.clone()
        {
            let own_ids = sig.own_type_param_ids();
            let mut infer = InferCtx::new(&self.tysys.type_table, own_ids.clone());
            for (i, (&param_type, &arg)) in param_types.iter().zip(args.iter()).enumerate() {
                if is_numeric_literal_arg(static_call.args.get(i)) {
                    infer.add_deferred(param_type, arg);
                } else {
                    infer.add(param_type, arg);
                }
            }
            let (mut inferred, bindings) = infer.solve_with_bindings();
            // A slot the arguments do not pin takes the default its declaration
            // wrote (WEP 2026-04-11), as every other spelling does. Without it
            // a block declaring slots of its own rejected the call the same
            // declaration accepts on a receiver that declares none.
            let defaulted = self.fill_static_default_type_args(&sig, target_type_id, &mut inferred);
            if defaulted || own_ids.iter().all(|id| bindings.contains_key(id)) {
                method_type_args = inferred;
                let declaring_args = self
                    .receiver_declaring_args(Some(target_type_id), &[])
                    .unwrap_or_default();
                let declaring = sig
                    .declaring_impl
                    .and_then(|id| self.tysys.signatures.impl_sig(id))
                    .cloned();
                let instantiated = sig.instantiate_call_with(
                    &self.tysys.type_table,
                    declaring.as_ref(),
                    &declaring_args,
                    &method_type_args,
                );
                param_types = instantiated.param_types;
                self.recoerce_literal_args(&static_call.args, &mut args, &param_types);
            } else {
                let _ = self.emit(TypeError::UninferredStaticTypeArg {
                    receiver,
                    method: static_call.method.clone(),
                    param: own.name.clone(),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            }
        }

        // Pad omitted trailing arguments with declared parameter defaults.
        // Variant / flags constructors carry no defaults, so the arg-count
        // checks below are unaffected.
        if args.len() < param_types.len() && !static_method_defaults.is_empty() {
            let defaults = &static_method_defaults;
            let mut subs: hashmap::IndexMap<String, ast::Expr> = hashmap::IndexMap::default();
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

        // A case, a flags member and a variant constructor are the receiver's
        // own declarations. A spelling that names a trait asks for none of
        // them — the same rule the resolution applies to its candidates.
        let builds_own_case = required_trait.is_none();

        // Handle flags type static methods: none() and all()
        if builds_own_case {
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
        if builds_own_case
            && let ResolvedType::Variant { .. } =
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
        if builds_own_case && is_generic_instance {
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
                        ast::Type::Generic(g) if turbofish_has_hole(&g.args)
                    );
                    let result_type = if has_target_hole {
                        let target_holes = match &static_call.target_type {
                            ast::Type::Generic(g) => turbofish_holes(&g.args),
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
                                .map_or(static_call.span, Expr::span);
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
                    if self.declares_method_directly(&newtype_name, &static_call.method) {
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
                    if self.declares_method_directly(&flags_name, &static_call.method) {
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

        // A trait impl's static is mangled with its trait, so WIR resolves it.
        // The receiver comes off the resolved type: re-deriving it from
        // `struct_name` searches the caller's frame, which an aliased import
        // leaves without that name at all.
        let receiver_key = self.impl_target_of(target_type_id, &DeclName::new(&struct_name));
        let Ok(resolution) = self.static_trait_ref(
            StaticQuery {
                receiver_key: Some(&receiver_key),
                arg_types: &args,
                receiver_type: Some(target_type_id),
                required_trait,
                ..StaticQuery::of(&struct_name, &static_call.method)
            },
            static_call.span,
        ) else {
            return TypeTable::ERROR;
        };
        let selected = resolution.selected;
        let trait_name_opt = selected.as_ref().and_then(|r| r.trait_name.clone());

        let mangled_func_name = MethodName::format_local(
            &mangled_struct_name,
            trait_name_opt.as_ref(),
            &static_call.method,
        );

        let mut return_type = resolution.return_type;

        // A value blanket indexes statics under its receiver *param* name, so
        // the concrete receiver's own bucket misses.
        if return_type == TypeTable::UNKNOWN
            && let Some(resolved) = self.resolve_blanket_static_method(
                target_type_id,
                &static_call.method,
                static_call.id,
                &method_type_args,
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

        // Substitute the method's own parameters, taken from the signature
        // rather than counted off the receiver. The declaring block's are
        // already filled: the resolution read the signature at the receiver,
        // and binding them a second time here is what let the two answers
        // differ.
        {
            let method_params = self.qualified_method_own_slots(&struct_name, &static_call.method);
            let subst_ctx = SubstitutionContext::new().bind(&method_params, &method_type_args);
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

        // Build method_info with base struct name and trait name (if applicable)
        let mut method_info = LocalMethodName::new(
            self.qualified_receiver_name(&struct_name),
            trait_name_opt,
            static_call.method.clone(),
        )
        .with_type_args(&impl_only_type_arg_names, &method_type_arg_names);

        // The `#[cm("...")]` import the callee binds, off the signature this
        // call resolved to, at the receiver it resolved at.
        method_info.cm_name = static_receiver
            .as_ref()
            .and_then(|key| self.qualified_method_sig_keyed(key, &static_call.method))
            .and_then(|sig| sig.cm_name);

        // The selection covers trait impls only; an inherent static has none
        // and reaches the index instead.
        if let Some(method_def) = selected.as_ref().and_then(|r| r.method_id).or_else(|| {
            let receiver = self.impl_target_of(target_type_id, &DeclName::new(&struct_name));
            self.qualified_method_decl_id(&receiver, &static_call.method)
                .or_else(|| self.qualified_method_decl_at(None, &struct_name, &static_call.method))
        }) {
            self.record_reference_to_decl(static_call.method_id, method_def);
        }

        // A concrete block hosts its own function; a generic one's instance is
        // materialised in the receiver's module (`FuncInstState::impl_module`).
        // Naming the receiver's for both minted an extern stub beside a
        // definition of that name, where the two modules differ.
        let func_ref = FunctionRef {
            module_source: self
                .concrete_impl_module_of(selected.as_ref())
                .unwrap_or(struct_module),
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
            StaticMethodDispatch {
                method_def: selected.as_ref().and_then(|r| r.method_id),
                // The scope annotate resolved these defaults in, so reify
                // resolves them in the same one.
                defaults_module: static_method_module
                    .unwrap_or_else(|| func_ref.module_source.clone()),
                function_ref: func_ref,
                param_is_mut,
                type_args: method_type_args,
                param_defaults: static_method_defaults,
                param_types: param_types.clone(),
                self_in_args,
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
        // The blanket's own declaration of the method: its bucket is keyed by
        // the receiver *param*, which no name written at a call site reaches.
        let method_def = self.tysys.declared_method(blanket_def, method);
        // Its defaults come from the template's own signature: no receiver-keyed
        // lookup reaches a blanket, so the call site brings none.
        let (static_method_defaults, template_defaults_module) = method_def
            .and_then(|def| self.tysys.signatures.method_sig(def))
            .map(|sig| {
                (
                    Param::named_defaults(&sig.params),
                    sig.defaults_module.clone(),
                )
            })
            .unwrap_or_default();
        let method_ref = StaticMethodRef::new(
            blanket_module.clone(),
            blanket_param.clone(),
            method.to_string(),
            Some(trait_name.clone()),
            method_def,
        );
        let template_return = self.lookup_static_method_return_type(&method_ref, &binder);
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
        if !self.check_static_call_args(
            &param_types,
            args,
            arg_spans,
            &static_method_defaults,
            span,
        ) {
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
            StaticMethodDispatch {
                method_def,
                defaults_module: template_defaults_module
                    .unwrap_or_else(|| func_ref.module_source.clone()),
                function_ref: func_ref,
                param_is_mut: Vec::new(),
                type_args: method_type_args.to_vec(),
                param_defaults: static_method_defaults,
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
        let key = ImplTargetKey::TypeParam(blanket_module.clone(), blanket_param.to_string());
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

    /// A qualified method's own type parameters — the slots past the declaring
    /// block's, split where its signature says they split. The block's are the
    /// resolution's to fill, so only these are left for a call site.
    fn qualified_method_own_slots(&self, struct_name: &str, method_name: &str) -> Vec<TypeId> {
        self.qualified_method_sig(struct_name, method_name)
            .map(|sig| sig.own_type_params().iter().map(|(_, id)| *id).collect())
            .unwrap_or_default()
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
    /// bounds `receiver_type_id` satisfies. Where several do, the resolution
    /// has already ranked them and reported what no rule separates.
    pub(super) fn find_blanket_static_method(
        &mut self,
        receiver_type_id: TypeId,
        method_name: &str,
    ) -> Option<BlanketStatic> {
        self.applicable_blanket_statics(receiver_type_id, method_name)
            .into_iter()
            .next()
    }

    /// Every value blanket supplying a static `method_name` that the receiver's
    /// bounds admit, in the order the blocks were written. Facts only: which of
    /// several answers is [`Elaborator::resolve_static_callee`]'s to decide.
    pub(super) fn applicable_blanket_statics(
        &mut self,
        receiver_type_id: TypeId,
        method_name: &str,
    ) -> Vec<BlanketStatic> {
        let candidates: Vec<(BlanketStatic, Vec<BlanketBound>)> = self
            .tysys
            .trait_env
            .blanket_impls
            .iter()
            .flat_map(|(trait_name, impls)| impls.iter().map(move |b| (trait_name, b)))
            .filter(|(_, b)| b.receiver == BlanketReceiver::Value)
            // The index is keyed by the receiver *param*, which every blanket in
            // a module shares (`Serialize` beside `Deserialize`, both over `T`),
            // so an entry speaks for this blanket only if this impl block
            // declares the method. The header alone would match an instance
            // method of the same name; both indices together will not.
            .filter(|(_, b)| {
                let Some(header) = self.tysys.trait_env.impl_headers.get(&b.def) else {
                    return false;
                };
                self.static_method_entries(
                    &ImplTargetKey::TypeParam(b.module.clone(), b.param.clone()),
                    method_name,
                )
                .any(|e| header.methods.iter().any(|m| m.def == e.method_id))
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
            .filter(|(_, bounds)| {
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
            .collect()
    }

    /// Whether an impl block on `struct_name` itself declares `method_name`, of
    /// either kind, which decides whether a newtype answers a qualified call or
    /// its base does. No newtype fallback here, for that reason.
    fn declares_method_directly(&self, struct_name: &str, method_name: &str) -> bool {
        self.impl_method_entries(&self.impl_target(struct_name), method_name)
            .next()
            .is_some()
    }

    /// What a qualified call returns, off the declaration it selected where it
    /// has one. `receiver` is the declaration the call site resolved, never a
    /// spelling to re-resolve: beside a same-named local declaration, the
    /// caller's own frame answers with the wrong one.
    pub(super) fn lookup_static_method_return_type(
        &mut self,
        method_ref: &StaticMethodRef,
        receiver: &FqTypeName,
    ) -> TypeId {
        let struct_name = method_ref.type_name.as_str();
        let method_name = method_ref.method_name.as_str();
        // The declaration the selection picked answers first: an overload's
        // members may return differently, and the pick is what the call's
        // mangled name was built from.
        if let Some(sig) = method_ref
            .method_id
            .and_then(|def| self.tysys.signatures.method_sig(def))
        {
            return sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        }

        // The receiver's own declaration is the key the resolution answers at.
        // A head naming none leaves it to derive one from the name, which is
        // the same vantage every other site now uses.
        let static_key = receiver
            .head()
            .def()
            .map(|def| ImplTargetKey::of_decl(self.tysys.resolutions.defs(), def));
        let resolved = self.resolve_static_callee(StaticQuery {
            receiver_key: static_key.as_ref(),
            ..StaticQuery::of(struct_name, method_name)
        });
        // The outcome's own answer, not the picked declaration's: an overload
        // returns where its candidates agree, and reading only `found()` threw
        // that away for an `Unknown` the caller cannot act on.
        resolved.return_type()
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
        target_hint: Option<&ImplTargetKey>,
    ) -> Option<Vec<TypeId>> {
        let static_key = self.static_receiver_key(struct_name, target_hint);
        if let Some(sig) = self.unique_static_method_sig(&static_key, method_name) {
            return Some(sig.value_param_types());
        }
        // A resource declares its statics in Wado like any other declaration,
        // so they answer from the same signature table at the same point in the
        // pass — `Response::new` is checked where `P::make` is. Statics are not
        // inherited, so the receiver's own declaration answers, never its chain.
        if let ImplTargetKey::Decl(def) = &static_key
            && let Some(sig) = self.tysys.signatures.resource_method_sig(*def, method_name)
            && sig.self_kind == ast::SelfKind::None
        {
            return Some(sig.value_param_types());
        }
        // The index holds only the declaring resource's own methods, so an
        // inherited one is reached by walking the chain. Instance methods only:
        // a static the receiver declares itself shadows an inherited instance
        // method of the same name, and the arm above has already answered it.
        if let ImplTargetKey::Decl(def) = &static_key
            && !self.declares_resource_static(*def, method_name)
            && let Some((_, sig)) = self.resource_instance_method(*def, method_name)
        {
            return Some(sig.value_param_types());
        }
        None
    }

    /// Whether the resource `def` declares `method_name` as a static of its own.
    fn declares_resource_static(&self, def: DefId, method_name: &str) -> bool {
        self.tysys
            .trait_env
            .resource_static(&ImplTargetKey::Decl(def), method_name)
            .is_some()
    }

    /// Resolve a static-method receiver `TypeId` to its `(struct_name,
    /// decl_key)` for impl / parameter lookups: follow newtypes to the base,
    /// map flags to `u32` and builtin arrays to `core:array`.
    pub(super) fn static_receiver_struct_key(
        &self,
        target_type_id: TypeId,
    ) -> (Option<String>, Option<ImplTargetKey>) {
        use crate::elaborator::trait_env::ImplTargetKey;
        let key: Option<ImplTargetKey> = {
            let mut current_type = target_type_id;
            loop {
                match self.tysys.type_table.borrow().get(current_type).clone() {
                    // Keyed on what they wrap, not on themselves: a newtype's
                    // impls are looked up on its base, and `flags`' on `u32`.
                    ResolvedType::Newtype { base_type, .. } => current_type = base_type,
                    ResolvedType::Flags { .. } => {
                        current_type = TypeTable::U32;
                    }
                    ResolvedType::BuiltinArray(_) => {
                        break Some(ImplTargetKey::Builtin(
                            TypeTable::ARRAY_TYPE_NAME.to_string(),
                        ));
                    }
                    // Every other nominal type keys on its own declaration.
                    _ => {
                        break self
                            .tysys
                            .type_table
                            .borrow()
                            .nominal_def(current_type)
                            .map(|def| ImplTargetKey::of_decl(self.tysys.resolutions.defs(), def));
                    }
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

    /// The receiver a static lookup keys on: the key its caller already
    /// resolved, else the written name over the walking module's frame.
    pub(super) fn static_receiver_key(
        &self,
        struct_name: &str,
        target_hint: Option<&ImplTargetKey>,
    ) -> ImplTargetKey {
        target_hint
            .cloned()
            .unwrap_or_else(|| self.impl_target(struct_name))
    }

    /// The *trait* impl blocks a receiver written `struct_name` reaches,
    /// current-module-first. Every block whose head names the receiver's
    /// declaration is one, whether or not it declares the method asked about:
    /// a block that overrides nothing still answers with the trait's default.
    ///
    /// A receiver reaches two namespaces: its declaration, and an impl binding
    /// the name as its own type parameter (`impl<V: Bound> Trait for V`), which
    /// keys under that binder. Both are searched in the current module, only
    /// the declaration namespace outside it.
    pub(super) fn trait_impls_for_receiver(
        &self,
        struct_name: &str,
        target_hint: Option<&ImplTargetKey>,
    ) -> Vec<DefId> {
        let defs = self.tysys.resolutions.defs();
        let target = self.static_receiver_key(struct_name, target_hint);
        let declared_name = target.type_name(defs).unwrap_or(struct_name).to_string();

        let env = &self.tysys.trait_env;
        let declared = env.entries_by_receiver_vec(&target.receiver(defs));
        let binder = env.entries_by_receiver_vec(&Receiver::Type(FqTypeName::param_bucket(
            &self.current_module_source,
            &declared_name,
        )));
        let is_current = |k: &&DefId| *defs.module(**k) == self.current_module_source;
        let mut keys: Vec<DefId> = declared
            .iter()
            .chain(binder.iter())
            .filter(is_current)
            .copied()
            .collect();
        keys.extend(declared.iter().filter(|k| !is_current(k)).copied());
        keys.retain(|key| {
            let header = &env.impl_headers[key];
            header.trait_type.is_some()
                && self.impl_head_decl_name(header, defs.module(*key)) == declared_name
        });
        keys
    }

    /// The static method declared under this name, `None` when several impls
    /// declare it and the index has nothing to choose between them. A call that
    /// must choose goes through [`Self::preselect_static_arg`].
    fn unique_static_method_sig(
        &self,
        static_key: &ImplTargetKey,
        method_name: &str,
    ) -> Option<&sig::MethodSig> {
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
        static_key: &ImplTargetKey,
    ) -> Vec<TypeId> {
        self.unique_static_method_sig(static_key, method_name)
            .map(|sig| sig.decl.type_params.iter().map(|(_, id)| *id).collect())
            .unwrap_or_default()
    }

    /// Whether a `From<arg_type>` impl for `target_type` is pending synthesis,
    /// so a call may name a conversion no impl block declares yet.
    pub(super) fn has_from_synthesis_request(
        &self,
        target_type: &ast::Type,
        arg_type_id: &tir::TypeId,
    ) -> bool {
        let target_name = get_type_name_static(target_type);
        let arg_type_name = self.tysys.type_table.borrow().type_name(*arg_type_id);
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(CompilerItem::From)
            .to_string();
        self.tysys.trait_env.impl_headers.values().any(|header| {
            if !header.is_synthesize_request {
                return false;
            }
            let Some(trait_type) = &header.trait_type else {
                return false;
            };
            if header.trait_name.as_deref() != Some(from_trait_name.as_str())
                || get_type_name_static(&header.ty) != target_name
            {
                return false;
            }
            matches!(trait_type, ast::Type::Generic(generic)
                if generic.args.len() == 1
                    && self.get_type_name_full(&generic.args[0]) == arg_type_name)
        })
    }

    /// Report why a static call's arguments matched no impl, when the
    /// receiver's impls explain it: a blanket impl this path cannot
    /// instantiate, or concrete impls none of which accept the arguments'
    /// types. Returns whether an error was emitted — the caller then stops
    /// instead of building an unresolvable mangled name (an ICE at WIR build).
    pub(super) fn report_unmatched_static_arg(
        &mut self,
        recv: StaticReceiver<'_>,
        method_name: &str,
        arg_types: &[TypeId],
        span: Span,
    ) -> bool {
        let struct_name = recv.name;
        let survey = self.static_arg_survey(recv, method_name);
        let spelled = render_type_list(&self.tysys.type_table.borrow(), arg_types);
        if let Some(trait_name) = survey.blanket_trait {
            let _ = self.emit(TypeError::UnsupportedBlanketInstantiation {
                trait_name,
                receiver: struct_name.to_string(),
                method: method_name.to_string(),
                arg_type: spelled,
                span,
            });
            return true;
        }
        // The trait the first candidate names. Where several traits supply the
        // name the call is an overload, reported before it reaches here.
        let Some(trait_name) = survey.candidates.first().map(|c| c.trait_name.clone()) else {
            return false;
        };
        // An impl the first argument matches is one the selection kept, so the
        // call did not fail on that argument: it failed on the list. Reporting
        // the unmatched case here named the argument as both unaccepted and
        // available.
        if survey
            .candidates
            .iter()
            .any(|c| c.params.first() == arg_types.first())
        {
            let _ = self.emit(TypeError::NoMatchingArgumentList {
                trait_name,
                receiver: struct_name.to_string(),
                method: method_name.to_string(),
                span,
            });
            return true;
        }
        let _ = self.emit(TypeError::NoMatchingTraitArgument {
            trait_name,
            receiver: struct_name.to_string(),
            method: method_name.to_string(),
            arg_type: spelled,
            candidates: survey.candidates.into_iter().map(|c| c.spelling).collect(),
            span,
        });
        true
    }

    /// The shared preselect entry for a static call (`Wrapper::from(42)`, in
    /// either its static-call or plain-call spelling). It runs before the
    /// callee is resolved, and its answer is what the resolution keys on:
    /// resolving without the arguments and mangling with them is two answers
    /// for one call.
    pub(super) fn preselect_static_args(
        &mut self,
        recv: StaticReceiver<'_>,
        method_name: &str,
        args: &[ast::Expr],
        span: Span,
        ctx: &mut FunctionContext,
    ) -> PreselectedArg {
        let recv_name = recv.name;
        if args.is_empty() || self.has_inherent_static_method(recv_name, method_name, recv.key) {
            return PreselectedArg::Undecided;
        }
        let classes: Vec<ArgClass> = args
            .iter()
            .map(|arg| self.synthesize_arg_class(arg, ctx))
            .collect();
        // A trait's default body reads `Self` off the receiver's type. Resolved
        // here rather than asked of every caller: it costs a scope, and the
        // returns above are the calls that never survey (WEP: "Resolving is not
        // free").
        let mut recv = recv;
        if recv.ty.is_none() {
            recv.ty = Some(self.resolve_unsited_type_name(recv_name, span));
        }
        match self.static_arg_preselect(recv, method_name, &classes) {
            ArgPreselect::Selected(params) => PreselectedArg::Types(params),
            ArgPreselect::Ambiguous(candidates) => {
                let _ = self.emit(TypeError::AmbiguousStaticArgument {
                    receiver: recv_name.to_string(),
                    method: method_name.to_string(),
                    candidates,
                    span,
                });
                PreselectedArg::Reported
            }
            ArgPreselect::Pass => PreselectedArg::Undecided,
        }
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
        self.inherent_shadows(&target, method_name, false)
    }

    /// The argument preselect over a receiver's impls: `Selected` and
    /// `Ambiguous` short-circuit resolution, so it decides calls. It must run
    /// *before* the argument is elaborated — the expected type shaping a literal
    /// comes from the selected impl. Admissibility is [`Elaborator::class_admits`]
    /// over each impl's *resolved* parameter type, since spelling under-admits.
    pub(super) fn static_arg_preselect(
        &mut self,
        recv: StaticReceiver<'_>,
        method_name: &str,
        classes: &[ArgClass],
    ) -> ArgPreselect {
        // Every argument opaque leaves nothing to select on. One opaque among
        // others admits every parameter, so the rest still decide.
        if classes.iter().all(|c| matches!(c, ArgClass::Opaque(_))) {
            return ArgPreselect::Pass;
        }
        let admitted: Vec<ArgCandidate> = self
            .static_arg_survey(recv, method_name)
            .candidates
            .into_iter()
            .filter(|c| self.params_admit(&c.params, classes))
            .collect();
        match admitted.as_slice() {
            [] => ArgPreselect::Pass,
            [only] => ArgPreselect::Selected(only.params.clone()),
            // A `Head` names a family, not a type — `Pair { a: 5 }` is a
            // `Pair` of something — so several same-head impls are the expected
            // answer, not a tie. Only classes each denoting one type may call
            // two candidates ambiguous; elaborating the arguments decides the
            // rest.
            _ if classes.iter().any(|c| matches!(c, ArgClass::Head(_))) => ArgPreselect::Pass,
            _ => ArgPreselect::Ambiguous(admitted.into_iter().map(|c| c.spelling).collect()),
        }
    }

    /// Whether an impl's parameters admit the call's argument classes, each
    /// against the one written for it. A call supplying fewer than the
    /// declaration takes is checked as far as it goes: the rest are defaults.
    fn params_admit(&self, params: &[TypeId], classes: &[ArgClass]) -> bool {
        classes.len() <= params.len()
            && params.iter().zip(classes).all(|(&param, class)| {
                param != TypeTable::UNKNOWN
                    && param != TypeTable::ERROR
                    && self.class_admits(param, class)
            })
    }

    /// The first-parameter types the receiver's trait impls declare for
    /// `method_name` (`From<String>`'s `String` beside `From<i64>`'s `i64`,
    /// `Enc<A>`'s `A` beside `Enc<B>`'s `B`), in candidate order, plus the
    /// trait of any blanket among them. The parameter is read from the
    /// declaration rather than off the trait reference: a conversion trait's
    /// source type is also its trait argument, but no other trait's is.
    ///
    /// It walks the impls directly rather than reading the resolution, because
    /// its consumers need every candidate and the resolution keeps one.
    pub(super) fn static_arg_survey(
        &self,
        recv: StaticReceiver<'_>,
        method_name: &str,
    ) -> StaticArgSurvey {
        let mut survey = StaticArgSurvey::default();
        for impl_def in self.trait_impls_for_receiver(recv.name, recv.key) {
            // A qualified spelling names a trait, so another trait's impl is
            // not a candidate to weigh against — the same rule the resolution
            // applies, asked where the arguments are surveyed.
            if let Some(required) = recv.required_trait
                && self
                    .tysys
                    .signatures
                    .impl_sig(impl_def)
                    .and_then(|sig| sig.trait_decl)
                    != Some(required)
            {
                continue;
            }
            let header = &self.tysys.trait_env.impl_headers[&impl_def];
            let Some(trait_decl) = self
                .tysys
                .signatures
                .impl_sig(impl_def)
                .and_then(|sig| sig.trait_decl)
            else {
                continue;
            };
            // The same walk the rules read, so a body the block inherits is a
            // candidate here too.
            let Some(offer) =
                self.impl_static_offer(header, impl_def, trait_decl, method_name, recv.ty)
            else {
                continue;
            };
            if offer.kind != CandidateKind::Static {
                continue;
            }
            let trait_name = || {
                header
                    .trait_name
                    .clone()
                    .expect("trait_impls_for_receiver yields trait impls alone")
            };
            // The selection's own question, asked through the selection's own
            // answer: a blanket accepts a family rather than a type, so the
            // blanket resolver answers such a call and it is never an unmatched
            // alternative worth listing. Asking it a second way here is what
            // let the two disagree.
            let params = match offer.selector {
                Selector::Absent => continue,
                Selector::Blanket => {
                    survey.blanket_trait.get_or_insert_with(trait_name);
                    continue;
                }
                Selector::Params(params) => params,
            };
            // By the types, as `Selector` is: two distinct types printing one
            // name are two candidates, not one.
            if survey.candidates.iter().any(|c| c.params == params) {
                continue;
            }
            let table = self.tysys.type_table.borrow();
            // The parameters as the impl's own frame resolved them, so a
            // private or aliased name means what the impl wrote.
            let spelling = render_type_list(&table, &params);
            survey.candidates.push(ArgCandidate {
                spelling,
                params,
                trait_name: trait_name(),
            });
        }
        survey
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
    fn impl_head_decl_name(&self, header: &ImplHeader, impl_module: &ModuleSource) -> String {
        let head = get_type_name_static(&header.ty);
        if header.type_params.iter().any(|p| p.name == head) {
            return head;
        }
        self.import_original_name(&head, impl_module)
    }

    /// The block a selection came from, where it is written for a single
    /// instantiation: it hosts its own function, under its own head. `None` for
    /// a generic block, whose instance monomorphization materialises in the
    /// receiver's module and under the receiver's own name, and for a spelling
    /// no trait impl answered.
    ///
    /// Read from the target's resolved arguments, not from the block's declared
    /// parameters: `impl Default for List<T>` declares none and is still
    /// generic in `T`, which the receiver fills. An argument that *contains* a
    /// parameter leaves the block open too — `Holder<fn(T) -> i32>` is no one
    /// instantiation.
    fn concrete_impl_of(&self, selected: Option<&StaticMethodRef>) -> Option<DefId> {
        let impl_def = self
            .tysys
            .signatures
            .method_sig(selected?.method_id?)?
            .declaring_impl?;
        let sig = self.tysys.signatures.impl_sig(impl_def)?;
        let table = self.tysys.type_table.borrow();
        let open = sig
            .target_type_args
            .iter()
            .any(|&arg| table.contains_type_param(arg));
        (!open).then_some(impl_def)
    }

    /// The module a concrete block hosts its function in — its own.
    fn concrete_impl_module_of(&self, selected: Option<&StaticMethodRef>) -> Option<ModuleSource> {
        let impl_def = self.concrete_impl_of(selected)?;
        Some(self.tysys.resolutions.defs().module(impl_def).clone())
    }

    /// The head a concrete block wrote, arguments included: `impl … for
    /// Cell<i32>` hosts its function under `Cell<i32>`, and a call spelling the
    /// receiver `Cell` has to name that, not the bare declaration. `None` where
    /// the block's target is not generic, which leaves the head as written.
    pub(super) fn concrete_impl_head_of(
        &self,
        selected: Option<&StaticMethodRef>,
    ) -> Option<FqTypeName> {
        let sig = self
            .tysys
            .signatures
            .impl_sig(self.concrete_impl_of(selected)?)?;
        if sig.target_type_args.is_empty() {
            return None;
        }
        let table = self.tysys.type_table.borrow();
        let args: Vec<FqTypeName> = sig
            .target_type_args
            .iter()
            .map(|&arg| table.fq_type_name(arg))
            .collect();
        Some(sig.target_fq.clone().with_args(args))
    }

    /// Whether only the argument can fill this parameter — a blanket, whose
    /// unsubstituted spelling must not be mangled. Three things fill a slot and
    /// the receiver and the method take the other two.
    pub(super) fn param_filled_by_block(
        &self,
        header: &ImplHeader,
        sig: &MethodSig,
        param: TypeId,
    ) -> bool {
        if header.type_params.is_empty() {
            return false;
        }
        let table = self.tysys.type_table.borrow();
        // A reference to a slot is the slot. A slot the receiver mentions is the
        // receiver's to fill, not the argument's, and one at or past
        // `method_slot_base` is the method's. By the numbering, not the count:
        // a concrete head argument leaves a gap, and the block's last slot then
        // sits past how many names it contributed.
        match table.get(table.peel_refs(param)) {
            ResolvedType::TypeParam { index, name }
            | ResolvedType::TypePack { index, name, .. } => {
                *index < sig.method_slot_base && !header.ty.mentions(name)
            }
            _ => false,
        }
    }

    /// The `Default::default` no declaration backs, which bound-driven
    /// synthesis emits on demand. It is not a candidate: nothing declares it,
    /// so no rule has anything to read, and it answers only where the rules
    /// found nothing.
    pub(super) fn auto_derived_default_ref(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<StaticMethodRef> {
        if method_name == "default"
            && let Some(struct_type) = self
                .tysys
                .auto_derive_default_struct_type(&self.type_lookup(), struct_name)
        {
            let default_trait_name = self
                .tysys
                .type_table
                .borrow()
                .compiler_trait_fq(CompilerItem::Default);
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

    /// Whether `struct_name::method_name` names a declaration at all — the
    /// resolution [`Elaborator::resolve_static_callee`] performs, asked for its
    /// outcome alone. The site decides which declaration `struct_name` names;
    /// see [`Elaborator::impl_target_at`].
    pub(super) fn is_static_method_at(
        &mut self,
        site: Option<AstId>,
        struct_name: &str,
        method_name: &str,
    ) -> bool {
        self.resolve_static_callee(StaticQuery {
            site,
            ..StaticQuery::of(struct_name, method_name)
        })
        .resolves()
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
            if self.declares_method_directly(struct_name, method_name) {
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

        // The argument separates a user-defined `impl From<MyType> for i32`
        // from the primitive's, and `impl Conv<A>` from `impl Conv<B>`. A
        // newtype's static call dispatches to its base, whose name is not the
        // caller's to resolve — that frame can hold a same-named declaration of
        // its own.
        let receiver_key = newtype_dispatch.as_ref().map(|(_, base_type_id, _)| {
            self.impl_target_of(*base_type_id, &DeclName::new(&actual_struct_name))
        });
        // The receiver a newtype dispatches to, and the arguments this site
        // resolves it with — inferred at the call, since the receiver is
        // spelled as a bare name. `impl_type_args` is what the site substitutes
        // with afterwards, so the resolution reading them says the same thing.
        let receiver_type = newtype_dispatch.as_ref().map(|(_, base, _)| *base);
        let Ok(resolution) = self.static_trait_ref(
            StaticQuery {
                receiver_key: receiver_key.as_ref(),
                arg_types: args,
                receiver_type,
                receiver_args: impl_type_args,
                ..StaticQuery::of(&actual_struct_name, method_name)
            },
            span,
        ) else {
            return TypeTable::ERROR;
        };

        // An inherent impl may live in any module of the package that owns the
        // type, and its methods are registered under that module. So the
        // fallback takes the impl's module where one is indexed, and the type's
        // home only where none is (`cross_module_inherent_static.wado`).
        let method_ref = resolution.selected.unwrap_or_else(|| {
            let target = self.static_receiver_key(&actual_struct_name, receiver_key.as_ref());
            let module = self
                .static_method_entries(&target, method_name)
                .find(|e| e.is_inherent())
                .map(|e| e.module.clone())
                .unwrap_or_else(|| self.declaring_module_of(&actual_struct_name));
            StaticMethodRef::new(module, &actual_struct_name, method_name, None, None)
        });

        // A concrete block hosts its function under the head it wrote:
        // `impl … for Cell<i32>` emits `Cell<i32>::wrap`, so a call spelling
        // the receiver `Cell` names that. A generic block's instance is
        // monomorphized under the receiver's own name, and keeps it.
        let receiver_fq = self
            .concrete_impl_head_of(Some(&method_ref))
            .unwrap_or(actual_struct_fq);
        // Use trait-qualified mangled name if this is a trait method
        let final_mangled_name = if let Some(ref trait_name) = method_ref.trait_name {
            MethodName::format_local(&receiver_fq, Some(trait_name), method_name)
        } else {
            actual_mangled_name
        };

        let mut return_type = resolution.return_type;

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

        // The signature the spelling names, whichever kind of method it is: the
        // `#[cm("...")]` import the callee binds is read off it. A static-only
        // index answers for one kind of method, and an instance one reached
        // qualified then lost its binding and left reify emitting a call to a
        // name nothing declares.
        let cm_name = self
            .static_call_sig(
                &actual_struct_name,
                method_name,
                receiver_key.as_ref(),
                SigChoice::Any,
            )
            .and_then(|sig| sig.cm_name);

        // From the resolution that named the callee. Asking again by the base's
        // bare name asks the *caller's* frame, which an alias leaves without
        // that name at all.
        let callee_params = resolution.params;

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
                // The same head the name was built from: mono looks the concrete
                // block's module up by this very spelling.
                let mut m =
                    LocalMethodName::new(receiver_fq, trait_name_opt, method_name.to_string());
                m.cm_name = cm_name;
                m
            }),
        };

        // Record the static-method dispatch decision so reify can reproduce
        // the same `Call` shape without re-running impl lookup, mangled-name
        // construction, or monomorph-info shaping. Recorded for every call this
        // path resolves: reify rebuilds the `Call` from this fact alone, so a
        // callee whose signature no lookup answers still needs the shape.
        self.sem.types.static_method_dispatch.insert(
            call_id,
            StaticMethodDispatch::of_params(method_ref.method_id, func_ref, vec![], callee_params),
        );

        return_type
    }
}

/// What [`Elaborator::preselect_static_args`] settled about a static call,
/// before the callee is resolved.
pub(super) enum PreselectedArg {
    /// The parameter types of the impl the arguments' classes picked. They
    /// shape the arguments *and* key the resolution, so both name one
    /// declaration.
    Types(Vec<TypeId>),
    /// Nothing to pick between, or classes that pick none: the resolution
    /// decides on its own.
    Undecided,
    /// Reported as ambiguous. The caller stops.
    Reported,
}

impl PreselectedArg {
    pub(super) fn picked(self) -> Option<Vec<TypeId>> {
        match self {
            Self::Types(params) => Some(params),
            Self::Undecided | Self::Reported => None,
        }
    }

    /// Install the picked parameters as the arguments' expected types. Only as
    /// far as the picked list goes: a callee may declare further parameters the
    /// defaults fill, and truncating the list left their arity unchecked and
    /// defaults unpadded.
    pub(super) fn shape(param_types: &mut Vec<TypeId>, picked: &[TypeId]) {
        param_types.resize(param_types.len().max(picked.len()), TypeTable::UNKNOWN);
        param_types[..picked.len()].copy_from_slice(picked);
    }
}

/// See [`Elaborator::static_arg_preselect`].
pub(super) enum ArgPreselect {
    /// Exactly one impl admits the arguments: elaborate them against these
    /// parameter types, and the name hint then finds the same impl.
    Selected(Vec<TypeId>),
    /// Several impls admit them — a literal never selects, so the call is
    /// reported with the admitted alternatives.
    Ambiguous(Vec<String>),
    /// The preselect does not apply (opaque arguments, no admitted candidate,
    /// or an unresolvable parameter type): the existing path decides.
    Pass,
}

/// One non-blanket impl's declared parameter list: the spelling for
/// diagnostics, the resolved types for admissibility, and the trait the impl
/// names so a report says which one it failed to match. See
/// [`Elaborator::static_arg_survey`].
pub(super) struct ArgCandidate {
    pub(super) spelling: String,
    pub(super) params: Vec<TypeId>,
    pub(super) trait_name: String,
}

/// A parameter or argument list as one spelling: the type alone where there is
/// one, a parenthesised list where there are several.
pub(super) fn render_type_list(table: &TypeTable, types: &[TypeId]) -> String {
    let names: Vec<String> = types.iter().map(|&t| table.type_name(t)).collect();
    match names.as_slice() {
        [one] => one.clone(),
        _ => format!("({})", names.join(", ")),
    }
}

/// The receiver a static call names, as the walks that ask about its impls
/// before the arguments are elaborated read it. The same facts
/// [`StaticQuery`] carries, so the survey and the rules see one receiver.
#[derive(Clone, Copy)]
pub(super) struct StaticReceiver<'a> {
    pub(super) name: &'a str,
    pub(super) key: Option<&'a ImplTargetKey>,
    /// Its type, which a trait's default body reads `Self` from. Without it
    /// every `Self`-typed parameter instantiates to `unknown`, which no
    /// argument admits.
    pub(super) ty: Option<TypeId>,
    /// The trait a qualified spelling names; only its impls answer.
    pub(super) required_trait: Option<DefId>,
}

impl<'a> StaticReceiver<'a> {
    /// The name alone. Every other fact is what a call site adds.
    pub(super) fn of(name: &'a str) -> Self {
        Self {
            name,
            key: None,
            ty: None,
            required_trait: None,
        }
    }
}

/// What a receiver's trait impls accept as a static's arguments.
#[derive(Default)]
pub(super) struct StaticArgSurvey {
    pub(super) candidates: Vec<ArgCandidate>,
    /// The trait of a blanket impl among them, which admits every argument and
    /// is therefore no alternative to list.
    pub(super) blanket_trait: Option<String>,
}
