//! AST Type to `TypeId` resolution.

use crate::ast::{AstId, GenericType, Type};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::types::TypeError;
use crate::symbol::SymbolKind;

/// A bound reachable from a frame, paired with the trait that wrote it —
/// `None` for one the frame wrote itself.
type FrameBound = (crate::ast::TraitBound, Option<crate::defs::DefId>);

/// Substitute named type parameters in an AST type.
/// `params[i]` is replaced by `args[i]` throughout the type.
pub(super) fn substitute_type_params(ty: &Type, params: &[String], args: &[Type]) -> Type {
    match ty {
        Type::Named(named) => {
            if let Some(i) = params.iter().position(|p| p == &named.name) {
                args[i].clone()
            } else {
                ty.clone()
            }
        }
        Type::Generic(generic) => {
            let new_args = generic
                .args
                .iter()
                .map(|a| substitute_type_params(a, params, args))
                .collect();
            Type::Generic(GenericType {
                id: generic.id,
                name: generic.name.clone(),
                args: new_args,
                span: generic.span,
            })
        }
        Type::Reference(inner) => {
            Type::Reference(Box::new(substitute_type_params(inner, params, args)))
        }
        Type::MutReference(inner) => {
            Type::MutReference(Box::new(substitute_type_params(inner, params, args)))
        }
        Type::Tuple(elems) => Type::Tuple(
            elems
                .iter()
                .map(|e| substitute_type_params(e, params, args))
                .collect(),
        ),
        _ => ty.clone(),
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_type(&mut self, ty: &Type) -> TypeId {
        match ty {
            Type::Named(named) => {
                self.record_type_name_reference(named.id, &named.name);
                self.resolve_named_type(named.id, &named.name, named.span, true)
            }
            Type::Generic(generic) => {
                self.record_type_name_reference(generic.id, &generic.name);
                self.resolve_generic_type(generic.id, &generic.name, &generic.args, generic.span)
            }
            Type::Function(func_ty) => {
                let params: Vec<TypeId> = func_ty
                    .params
                    .iter()
                    .map(|p| self.resolve_type(p))
                    .collect();
                let return_type = self.resolve_type(&func_ty.return_type);
                let stores: Vec<u32> = func_ty
                    .stores
                    .iter()
                    .filter_map(|e| match e {
                        crate::ast::StoresEntry::Index(n) => Some(*n),
                        crate::ast::StoresEntry::Name(_) => None, // Names only valid in fn decls
                    })
                    .collect();
                // Resolve effect names in function type position
                let effects = self.resolve_effects(&func_ty.effects, &func_ty.effect_ids);
                self.tysys.type_table.borrow_mut().make_function_with_mut(
                    func_ty.is_mut,
                    params,
                    return_type,
                    effects,
                    stores,
                )
            }
            Type::Tuple(elements) => {
                let elem_types: Vec<TypeId> =
                    elements.iter().map(|e| self.resolve_type(e)).collect();
                self.tysys.type_table.borrow_mut().make_tuple(elem_types)
            }
            Type::Reference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.tysys.type_table.borrow_mut().make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.tysys.type_table.borrow_mut().make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => self.resolve_namespaced_generic_type(namespaced),
            Type::TypePackSpread(name, span) => {
                // Look up the type pack parameter
                if let Some((index, _type_id)) = self.annotate_ctx.trait_ctx.type_params.get(name) {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_pack(name.clone(), *index)
                } else {
                    let _ = self.emit(TypeError::UnknownType {
                        name: format!("..{name}"),
                        span: *span,
                    });
                    TypeTable::ERROR
                }
            }
            // Inference placeholder `_` → UNKNOWN, the same value an omitted
            // turbofish slot carries. Turbofish resolution detects `_` slots
            // structurally (see `resolve_call`) and fills them; other positions
            // are rejected during validation.
            Type::Infer(_) => TypeTable::UNKNOWN,
            // Parser error-recovery placeholder: the syntax error was already
            // reported, so resolve to the error type to suppress cascades.
            Type::Error(_) => TypeTable::ERROR,
        }
    }

    /// Which of `param_name`'s trait bounds declares `assoc_name`, making
    /// `param_name::assoc_name` mean `<param_name as ThatTrait>::assoc_name`.
    /// Resolution needs the qualifier: one type may implement two traits that
    /// declare the same associated-type name.
    fn bound_declaring_assoc_type(
        &self,
        param_name: &str,
        assoc_name: &str,
    ) -> Option<crate::defs::DefId> {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(param_name)?;
        bounds
            .iter()
            .filter(|bound| {
                self.trait_assoc_type_decl(&bound.name, assoc_name)
                    .is_some()
            })
            // The bound's own reference site says which trait it names, so an
            // aliased bound and another module's same-named trait stay apart.
            .find_map(|bound| self.trait_decl_at(bound.id, &bound.name))
            // A bound inherits its supertraits' associated types, so
            // `T: Ord` answers for `Eq`'s. Searched after the direct bounds so
            // a trait redeclaring the name still wins for itself.
            .or_else(|| {
                bounds
                    .iter()
                    .filter_map(|bound| self.trait_decl_at(bound.id, &bound.name))
                    .flat_map(|decl| self.tysys.trait_env.supertrait_closure(&decl))
                    .find(|inherited| {
                        self.trait_assoc_type_decl(&inherited.bound.name, assoc_name)
                            .is_some()
                    })
                    .map(|inherited| inherited.decl)
            })
    }

    /// The identity an impl header names: the trait, plus the arguments it
    /// writes beyond the declared defaults — `impl Add<Cm> for Cm` is the bare
    /// instantiation a bound reaches, `impl Add<Inch> for Cm` keys its own.
    pub(super) fn impl_trait_ref(
        &mut self,
        trait_type: &crate::ast::Type,
        target: &crate::ast::Type,
        trait_decl: crate::defs::DefId,
    ) -> crate::tir::TraitRef {
        let Some(params) = self
            .tysys
            .trait_env
            .trait_decl_headers
            .get(&trait_decl)
            .map(|header| header.type_params.clone())
        else {
            return crate::tir::TraitRef::bare(trait_decl);
        };
        let kept = super::trait_env::non_default_arg_count(
            trait_type,
            target,
            &params,
            &self.tysys.resolutions,
        );
        let args = super::trait_env::written_arg_nodes(trait_type)
            .iter()
            .take(kept)
            .map(|arg| self.resolve_type(arg))
            .collect();
        crate::tir::TraitRef::new(trait_decl, args)
    }

    /// Report `T::Output` where two of `T`'s bounds declare `Output`, and say
    /// whether it did — answering with the first is a coin toss the writer
    /// never made. Supertraits do not count: a direct bound wins there.
    fn report_ambiguous_assoc_type(&self, param_name: &str, assoc_name: &str, span: Span) -> bool {
        let Some(bounds) = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(param_name)
        else {
            return false;
        };
        // One bound cannot be a coin toss, and the walk below asks the scope
        // for every bound's header.
        if bounds.len() < 2 {
            return false;
        }
        let declaring: Vec<&crate::ast::TraitBound> = bounds
            .iter()
            .filter(|bound| {
                self.trait_assoc_type_decl(&bound.name, assoc_name)
                    .is_some()
            })
            .collect();
        if declaring.len() < 2 {
            return false;
        }
        // Bounds that all pin the name to one type name one answer between
        // them: `T: Add<Output = T> + Mul<Output = T>` is not a coin toss,
        // where `Add<Output = Cm> + Mul<Output = Area>` is.
        let pins: Vec<Option<crate::name::FqTypeName>> = declaring
            .iter()
            .map(|bound| {
                bound
                    .assoc_types
                    .iter()
                    .find(|constraint| constraint.name == assoc_name)
                    .map(|constraint| {
                        super::trait_env::written_type_arg(&constraint.ty, &self.tysys.resolutions)
                    })
            })
            .collect();
        if pins.iter().all(|pin| pin.is_some() && *pin == pins[0]) {
            return false;
        }
        let _ = self.emit(TypeError::AmbiguousAssocType {
            assoc: assoc_name.to_string(),
            param: param_name.to_string(),
            traits: declaring.iter().map(|bound| bound.name.clone()).collect(),
            span,
        });
        true
    }

    /// Which trait declares `assoc_name` for the `impl` block being elaborated:
    /// the trait it names, or the supertrait the name is inherited from.
    fn self_trait_declaring_assoc_type(&self, assoc_name: &str) -> Option<crate::defs::DefId> {
        let self_trait = self.annotate_ctx.trait_ctx.self_trait?;
        if self
            .trait_assoc_type_decl(self.tysys.trait_env.defs.name(self_trait), assoc_name)
            .is_some()
        {
            return Some(self_trait);
        }
        self.tysys
            .trait_env
            .supertrait_closure(&self_trait)
            .iter()
            .find(|inherited| {
                self.trait_assoc_type_decl(&inherited.bound.name, assoc_name)
                    .is_some()
            })
            .map(|inherited| inherited.decl)
    }

    /// Resolve a namespaced generic type like `ns::Type<T>` or `Self::Output`
    pub(super) fn resolve_namespaced_generic_type(
        &mut self,
        namespaced: &crate::ast::NamespacedGenericType,
    ) -> TypeId {
        // Handle Self::AssociatedType
        if namespaced.namespace.as_str() == "Self" {
            // Look up the associated type binding
            if let Some(&type_id) = self
                .annotate_ctx
                .trait_ctx
                .assoc_type_bindings
                .get(&namespaced.name)
            {
                return type_id;
            }
            if let Some(self_type) = self.annotate_ctx.trait_ctx.self_type
                && !self
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_param(self_type)
            {
                if let Some(resolved) = self
                    .tysys
                    .type_table
                    .borrow()
                    .resolve_assoc_type(self_type, &namespaced.name)
                {
                    return resolved;
                }
                if let Some(resolved) = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .resolve_generic_assoc_type_mono(self_type, &namespaced.name)
                {
                    return resolved;
                }
            }
            // `Self` is a generic instance (`Cell<T>` elaborating a default body
            // it does not override), so the concrete-keyed lookup above is
            // skipped. The generic definition still answers: it substitutes the
            // instance's own arguments, which may themselves be type parameters.
            //
            // Qualified by the trait being implemented — or the supertrait that
            // declares the name — because the unqualified form gives up when two
            // traits declare it differently (WEP-2026-08-12).
            if let Some(self_type) = self.annotate_ctx.trait_ctx.self_type {
                if let Some(trait_key) = self.self_trait_declaring_assoc_type(&namespaced.name)
                    && let Some(resolved) = self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .resolve_trait_assoc_type_of_instance(
                            self_type,
                            &trait_key,
                            &namespaced.name,
                        )
                {
                    return resolved;
                }
                if let Some(resolved) = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .resolve_generic_assoc_type_mono(self_type, &namespaced.name)
                {
                    return resolved;
                }
            }
            if let Some(self_type) = self.annotate_ctx.trait_ctx.self_type
                && matches!(
                    self.tysys.type_table.borrow().get(self_type),
                    ResolvedType::TypeParam { .. }
                )
            {
                return self.make_frame_projection(self_type, "Self", &namespaced.name);
            }
            // If not found, it's an unknown associated type
            let _ = self.emit(TypeError::UnknownType {
                name: format!("Self::{}", namespaced.name),
                span: namespaced.span,
            });
            return TypeTable::ERROR;
        }

        // Handle T::AssociatedType where T is a type parameter in scope
        if let Some(&(_, param_type_id)) = self
            .annotate_ctx
            .trait_ctx
            .type_params
            .get(&namespaced.namespace)
        {
            // If the param is bound to a concrete type (not a TypeParam), look up the assoc
            // type from the TypeTable directly. This handles cases like blanket impl resolution
            // where we temporarily bind e.g. I = StrUtf8ByteIter (concrete struct), and
            // I::Item should resolve to u8 via (StrUtf8ByteIter, "Item") → u8.
            let param_is_concrete = !self
                .tysys
                .type_table
                .borrow()
                .contains_type_param(param_type_id);
            if param_is_concrete {
                // First try pre-registered concrete associated type resolution.
                if let Some(resolved) = self
                    .tysys
                    .type_table
                    .borrow()
                    .resolve_assoc_type(param_type_id, &namespaced.name)
                {
                    return resolved;
                }
                // Fallback: resolve via generic associated type definitions.
                // This handles GenericInstance types like ListIter<i32> whose Iterator impl
                // is generic — resolve_assoc_type won't find a pre-registered entry, but
                // resolve_generic_assoc_type_mono can derive i32 from ("ListIter", "Item") →
                // TypeParam(0), and substitutes the instance's args into a reference / nested
                // associated type (`&T`, `I::Item`) so it becomes concrete here at type-check.
                if let Some(resolved) = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .resolve_generic_assoc_type_mono(param_type_id, &namespaced.name)
                {
                    return resolved;
                }
            }

            let base_name = namespaced.namespace.clone();
            if self.report_ambiguous_assoc_type(&base_name, &namespaced.name, namespaced.span) {
                return TypeTable::ERROR;
            }
            // What the frame's bounds bind it to, where they say: `I:
            // IntoIterator<Item = u8>` answers `I::Item` directly.
            if let Some(direct_type) =
                self.frame_projection(param_type_id, &base_name, &namespaced.name)
            {
                return direct_type;
            }
            return self.make_frame_projection(param_type_id, &base_name, &namespaced.name);
        }

        if self
            .sem
            .imports
            .namespace_imports
            .contains_key(namespaced.namespace.as_str())
        {
            // `ns::Type` / `ns::Type<args>` (`ns` is a namespace-import alias):
            // resolve the `ns$Type` alias, which the import tier scopes to the
            // namespace's own module. Mirrors `canonical_ns_ref` for idents.
            let alias =
                crate::name::namespace_member_alias(&namespaced.namespace, &namespaced.name);
            if namespaced.args.is_empty() {
                self.resolve_named_type(namespaced.id, &alias, namespaced.span, true)
            } else {
                self.resolve_generic_type(namespaced.id, &alias, &namespaced.args, namespaced.span)
            }
        } else {
            let _ = self.emit(TypeError::UnknownType {
                name: format!("{}::{}", namespaced.namespace, namespaced.name),
                span: namespaced.span,
            });
            TypeTable::ERROR
        }
    }

    /// The type a named reference site resolves to.
    ///
    /// `site` is what decides which declaration is meant; `name` is carried
    /// for the binder tiers the walk does not answer for — `Self` and the type
    /// parameters in the frame — and for the diagnostic.
    pub(super) fn resolve_named_type(
        &mut self,
        site: AstId,
        name: &str,
        span: Span,
        enforce_arity: bool,
    ) -> TypeId {
        self.resolve_named_type_at(Some(site), name, span, enforce_arity)
    }

    /// [`Self::resolve_named_type`] for a receiver spelling the elaborator
    /// itself produced, where no segment of the source names the type: a
    /// `Self::` / `T::` prefix rewritten to a concrete name. The module scope
    /// answers, since there is no site to ask.
    pub(super) fn resolve_unsited_type_name(&mut self, name: &str, span: Span) -> TypeId {
        self.resolve_named_type_at(None, name, span, false)
    }

    fn resolve_named_type_at(
        &mut self,
        site: Option<AstId>,
        name: &str,
        span: Span,
        enforce_arity: bool,
    ) -> TypeId {
        // Handle `Self` type reference in impl blocks
        if name == "Self" {
            if let Some(self_type) = self.annotate_ctx.trait_ctx.self_type {
                return self_type;
            }
            // Self used outside of impl block - return Unknown
            return TypeTable::UNKNOWN;
        }

        // First check if it's a type parameter in scope
        if let Some(&(_, type_id)) = self.annotate_ctx.trait_ctx.type_params.get(name) {
            return type_id;
        }

        if let Some(primitive) = TypeTable::primitive_by_name(name) {
            return primitive;
        }

        if let Some(def) = self.type_decl_at(site, name) {
            if enforce_arity && let Some(expected) = self.bare_generic_type_arity(def) {
                let _ = self.emit(TypeError::MissingTypeArguments {
                    name: name.to_string(),
                    expected,
                    span,
                });
                return TypeTable::ERROR;
            }
            if let Some(type_id) = self.lookup_newtype_of_decl(def) {
                return type_id;
            }
            if let Some(defined_at) = self
                .lookup_struct_fields_of_decl(def)
                .map(|info| info.defined_at)
                .or_else(|| {
                    self.lookup_variant_case_of_decl(def)
                        .map(|info| info.defined_at)
                })
                .or_else(|| {
                    self.lookup_enum_case_of_decl(def)
                        .map(|info| info.defined_at)
                })
                .or_else(|| {
                    self.lookup_resource_type_of_decl(def)
                        .map(|info| info.defined_at)
                })
            {
                return self.tysys.type_table.borrow().type_id_of_decl(defined_at);
            }
        }
        if let Some(scope_mod) = self.annotate_ctx.default_scope_module.clone()
            && scope_mod != self.current_module_source
        {
            // A default re-resolved at the caller may name a type
            // private to the callee's module (`fn f<T = Priv>()` called
            // cross-module); the caller can't name it, so retry in the
            // callee's perspective. Mirrors the ident / call fallback.
            return self.with_module_perspective_for(&scope_mod, |s| {
                s.resolve_named_type_at(site, name, span, enforce_arity)
            });
        }
        TypeTable::UNKNOWN
    }

    /// How many type arguments the declaration `def` requires, when it requires
    /// any. The three kinds are asked of one declaration, so "is this generic"
    /// and "whose parameters are these" can never be about two of them.
    pub(super) fn bare_generic_type_arity(&self, def: crate::defs::DefId) -> Option<usize> {
        if let Some(info) = self.lookup_struct_fields_of_decl(def)
            && !info.type_param_bounds.is_empty()
        {
            return Some(info.type_param_bounds.len());
        }
        if let Some(info) = self.lookup_variant_case_of_decl(def)
            && !info.type_params.is_empty()
        {
            return Some(info.type_params.len());
        }
        if let Some(info) = self.lookup_generic_newtype_of_decl(def)
            && !info.type_params.is_empty()
        {
            return Some(info.type_params.len());
        }
        None
    }

    /// The type a generic application resolves to. `site` names the head's
    /// declaration; `name` is the compiler-item spelling and the diagnostic.
    pub(super) fn resolve_generic_type(
        &mut self,
        site: AstId,
        name: &str,
        args: &[Type],
        span: Span,
    ) -> TypeId {
        self.resolve_generic_type_at(Some(site), name, args, span)
    }

    fn resolve_generic_type_at(
        &mut self,
        site: Option<AstId>,
        name: &str,
        args: &[Type],
        span: Span,
    ) -> TypeId {
        // Prelude module path for looking up Option/Result
        let prelude_source = ModuleSource::prelude();

        match name {
            "Option" => {
                // Verify Option variant exists in symbol table (declared in prelude)
                // First check local imports, then fall back to prelude module
                let found_as_variant = self
                    .symbol_named(&self.current_module_source, "Option")
                    .or_else(|| self.symbols.lookup_in_module(&prelude_source, "Option"))
                    .is_some_and(|s| matches!(s.kind, SymbolKind::Variant(_)));

                if !found_as_variant {
                    // Option not found as a variant - likely #![no_prelude] without explicit import
                    let _ = self.emit(TypeError::UnknownType {
                        name: "Option".to_string(),
                        span,
                    });
                }
                let inner = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.tysys.type_table.borrow_mut().make_option(inner)
            }
            "Stream" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.tysys.type_table.borrow_mut().make_stream(elem)
            }
            "StreamWritable" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_stream_writable(elem)
            }
            "Future" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.tysys.type_table.borrow_mut().make_future(elem)
            }
            "FutureWritable" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_future_writable(elem)
            }
            // `Array<T>` is the user-facing spelling of the raw GC array
            // builtin (`ResolvedType::BuiltinArray`), declared
            // definition-less via `#[compiler_item("array")]` in
            // `core:prelude`. Resolved by its canonical name, like the
            // other prelude builtins (`Option` / `Stream` / `Future`);
            // this also resolves the builtin module's own signatures,
            // which are elaborated before the compiler-item registry is
            // populated.
            _ if name == TypeTable::ARRAY_TYPE_NAME => {
                if args.len() != 1 {
                    let _ = self.emit(TypeError::ArgumentCountMismatch {
                        expected: 1,
                        found: args.len(),
                        span,
                    });
                    return TypeTable::ERROR;
                }
                let element_type = self.resolve_type(&args[0]);
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_builtin_array(element_type)
            }
            _ => {
                // Which declaration the head names is the site's answer; the
                // kind it turns out to be decides which shape is built.
                let Some(def) = self.type_decl_at(site, name) else {
                    return self.resolve_generic_type_out_of_scope(site, name, args, span);
                };
                let struct_info = self.lookup_struct_fields_of_decl(def).cloned();
                if struct_info
                    .as_ref()
                    .is_some_and(|info| !info.type_param_bounds.is_empty())
                {
                    // Resolve type arguments
                    let type_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();

                    // Check trait bounds for each type argument
                    if let Some(info) = &struct_info {
                        for (i, (param_name, bounds)) in info.type_param_bounds.iter().enumerate() {
                            if let Some(&type_arg) = type_args.get(i) {
                                for bound in bounds {
                                    let Some(bound_def) = self.bound_trait_def(bound.site) else {
                                        continue;
                                    };
                                    if !self.tysys.type_implements_trait(
                                        &self.annotate_ctx,
                                        &self.type_lookup(),
                                        type_arg,
                                        bound_def,
                                    ) {
                                        // Get the type name for the error message
                                        let type_name = self.tysys.type_id_to_string(type_arg);
                                        let reason = self.tysys.trait_unimpl_reason_chain(
                                            &self.annotate_ctx,
                                            &self.type_lookup(),
                                            type_arg,
                                            &bound.name,
                                        );
                                        let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                                            type_name,
                                            trait_name: bound.name.clone(),
                                            param_name: param_name.clone(),
                                            reason,
                                            span,
                                        });
                                    }
                                }
                            }
                        }
                    }

                    // The instantiation is named by the declaration its head
                    // resolved to, and keeps its arguments beside it rather
                    // than fused into a rendered `Box<i32>` head no `impl`
                    // header writes.
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_generic_instance(def, type_args)
                } else if let Some(variant_info) = self.lookup_variant_case_of_decl(def).cloned() {
                    // Check if it's a generic variant (like Result<T, E>)
                    if variant_info.type_params.is_empty() {
                        TypeTable::UNKNOWN
                    } else {
                        let type_args: Vec<TypeId> =
                            args.iter().map(|t| self.resolve_type(t)).collect();
                        self.tysys
                            .type_table
                            .borrow_mut()
                            .make_generic_instance(def, type_args)
                    }
                } else if let Some(gn_info) = self.lookup_generic_newtype_of_decl(def).cloned() {
                    // Generic newtype instantiation: type MyArray<T> = List<T>
                    // Substitute type params in the base type AST, then resolve
                    let concrete_base_ast =
                        substitute_type_params(&gn_info.base_type_ast, &gn_info.type_params, args);
                    let base_type_id = self.resolve_type(&concrete_base_ast);
                    let resolved_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();
                    self.tysys.type_table.borrow_mut().make_newtype_instance(
                        def,
                        resolved_args,
                        base_type_id,
                    )
                } else {
                    self.resolve_generic_type_out_of_scope(site, name, args, span)
                }
            }
        }
    }

    /// The retry a generic application gets when its head names nothing here:
    /// a default re-resolved at the caller may spell a type only the callee's
    /// module imports (`entries: TreeMap<K, V> = TreeMap::new()` called
    /// cross-module). Mirrors the [`Self::resolve_named_type`] fallback.
    fn resolve_generic_type_out_of_scope(
        &mut self,
        site: Option<AstId>,
        name: &str,
        args: &[Type],
        span: Span,
    ) -> TypeId {
        let Some(scope_mod) = self.annotate_ctx.default_scope_module.clone() else {
            return TypeTable::UNKNOWN;
        };
        if scope_mod == self.current_module_source {
            return TypeTable::UNKNOWN;
        }
        self.with_module_perspective_for(&scope_mod, |s| {
            s.resolve_generic_type_at(site, name, args, span)
        })
    }

    /// Look up the trait bounds on an associated type declaration.
    /// Given a type parameter `param_id` (e.g., `S: Serializer`), find the trait that
    /// declares the associated type `assoc_name` and return its full bounds (with assoc types).
    fn find_assoc_type_bounds(
        &self,
        param_id: TypeId,
        assoc_name: &str,
    ) -> Vec<crate::ast::TraitBound> {
        let param_type = self.tysys.type_table.borrow().get(param_id).clone();
        if !matches!(param_type, ResolvedType::TypeParam { .. }) {
            return Vec::new();
        }

        self.tysys
            .trait_env
            .assoc_type_bound_index
            .get(assoc_name)
            .cloned()
            .unwrap_or_default()
    }

    /// What this frame knows the projection `base::assoc` to be, where
    /// `base_name` is the name the frame files `base`'s bounds under.
    ///
    /// Two sources answer, in order: the bindings a projection carries
    /// (`S::SeqSerializer` knowing its `Ok`), then the enclosing `where` clause
    /// (`I: IntoIterator<Item = u8>` answers `I::Item`).
    pub(super) fn frame_projection(
        &mut self,
        base: TypeId,
        base_name: &str,
        assoc: &str,
    ) -> Option<TypeId> {
        let carried = {
            let table = self.tysys.type_table.borrow();
            match table.get(base) {
                ResolvedType::AssocTypeProjection {
                    assoc_type_bindings,
                    ..
                } => assoc_type_bindings
                    .iter()
                    .find(|(name, _)| name == assoc)
                    .map(|(_, type_id)| *type_id),
                _ => None,
            }
        };
        if carried.is_some() {
            return carried;
        }
        let resolved: Vec<TypeId> = self
            .frame_assoc_bindings_of(base_name, assoc)
            .into_iter()
            .map(|ty| self.resolve_bound_binding(base_name, &ty))
            .collect();
        // Two bounds binding it differently is the coin toss the caller's
        // ambiguity check reports; answering with the first would hide it.
        let first = *resolved.first()?;
        resolved.iter().all(|t| *t == first).then_some(first)
    }

    /// A bound's right-hand side, resolved in this frame. `Self` inside one is
    /// the bounded type, which the frame files under `base_name`.
    fn resolve_bound_binding(&mut self, base_name: &str, ty: &crate::ast::Type) -> TypeId {
        match self
            .annotate_ctx
            .trait_ctx
            .type_params
            .get(base_name)
            .map(|&(_, id)| id)
        {
            Some(id) => self.with_self_type(id, |s| s.resolve_type(ty)),
            None => self.resolve_type(ty),
        }
    }

    /// Every bound on `base_name` a projection may be answered from, each
    /// paired with the trait that wrote it (`None` for the frame's own). A
    /// supertrait binds an assoc type too, so one walk serves every lookup.
    fn bound_closure_of(&mut self, base_name: &str) -> Option<Vec<FrameBound>> {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(base_name)?
            .clone();
        let inherited: Vec<FrameBound> = bounds
            .iter()
            .filter_map(|bound| self.trait_decl_at(bound.id, &bound.name))
            .flat_map(|decl| self.tysys.trait_env.supertrait_closure(&decl).to_vec())
            .map(|i| (i.bound, Some(i.writer)))
            .collect();
        Some(
            bounds
                .into_iter()
                .map(|bound| (bound, None))
                .chain(inherited)
                .collect(),
        )
    }

    /// Whether the asking frame can answer `ty` at all: an inherited bound may
    /// name its writer's own type parameters, which a bound cannot supply. Only
    /// `Self` crosses, being the bounded type here.
    fn frame_can_answer(&self, writer: Option<crate::defs::DefId>, ty: &crate::ast::Type) -> bool {
        let Some(header) = writer.and_then(|w| self.tysys.trait_env.trait_decl_headers.get(&w))
        else {
            return true;
        };
        let mut mentioned = Vec::new();
        ty.mentioned_names(&mut mentioned);
        !header
            .type_params
            .iter()
            .any(|param| mentioned.contains(&param.name))
    }

    /// What every bound in the closure binds `assoc` to. A binding the asking
    /// frame cannot answer is dropped.
    fn frame_assoc_bindings_of(&mut self, base_name: &str, assoc: &str) -> Vec<crate::ast::Type> {
        self.bound_closure_of(base_name)
            .unwrap_or_default()
            .iter()
            .flat_map(|(bound, writer)| {
                bound
                    .assoc_types
                    .iter()
                    .filter(|b| b.name == assoc && self.frame_can_answer(*writer, &b.ty))
                    .map(|b| b.ty.clone())
            })
            .collect()
    }

    /// The projection `base::assoc` as this frame builds it. The single
    /// builder, so one written in a signature and one synthesized for an
    /// expression intern to the same type.
    pub(super) fn make_frame_projection(
        &mut self,
        base: TypeId,
        base_name: &str,
        assoc: &str,
    ) -> TypeId {
        let owning_trait = self.bound_declaring_assoc_type(base_name, assoc);
        self.make_frame_projection_of_trait(base, base_name, owning_trait, assoc)
    }

    /// [`Self::make_frame_projection`] for a caller that already knows which
    /// trait declares `assoc`. `T: Add + Mul` declares `Output` twice, and only
    /// the site that dispatched can say which one `a * b` yields.
    pub(super) fn make_frame_projection_of_trait(
        &mut self,
        base: TypeId,
        base_name: &str,
        owning_trait: Option<crate::defs::DefId>,
        assoc: &str,
    ) -> TypeId {
        let assoc_bounds = self.find_assoc_type_bounds(base, assoc);
        let bound_names: Vec<crate::name::FqTraitName> = assoc_bounds
            .iter()
            .map(|b| self.fq_trait_name_at(b.id, &b.name))
            .collect();
        let assoc_type_bindings = self.frame_assoc_bindings(base, base_name, &assoc_bounds);
        self.tysys
            .type_table
            .borrow_mut()
            .make_assoc_type_projection_of_trait(
                base,
                owning_trait,
                assoc.to_string(),
                bound_names,
                assoc_type_bindings,
            )
    }

    /// [`Self::frame_projection`] scoped to one trait, so a second bound
    /// declaring the same name never answers for it: `T: Mul<Output = T>`
    /// answers `T::Output` where a bare `T: Mul` leaves it abstract.
    pub(super) fn frame_projection_of_trait(
        &mut self,
        base_name: &str,
        trait_: crate::defs::DefId,
        assoc: &str,
    ) -> Option<TypeId> {
        let bounds = self.bound_closure_of(base_name)?;
        let written = bounds
            .into_iter()
            .find_map(|(bound, writer)| {
                let fq = self.fq_trait_name_at(bound.id, &bound.name);
                (self.tysys.trait_env.trait_def_of_fq(&fq) == Some(trait_))
                    .then(|| bound.assoc_types.iter().find(|b| b.name == assoc).cloned())
                    .flatten()
                    .filter(|b| self.frame_can_answer(writer, &b.ty))
            })?
            .ty;
        Some(self.resolve_bound_binding(base_name, &written))
    }

    /// What `bounds` say the bounded type's own associated types are, as this
    /// frame knows them: `I: IntoIterator<Item = u8>` answers what `I::Item` is.
    /// `Self` inside a bound names the bounded type, so only a right-hand side
    /// the frame can answer binds and the rest stay abstract — rebinding `Self`
    /// and resolving instead lets the frame's own bindings shadow it, and
    /// recursion through a bound's right-hand side has no fixpoint.
    pub(super) fn frame_assoc_bindings(
        &mut self,
        base: TypeId,
        base_name: &str,
        bounds: &[crate::ast::TraitBound],
    ) -> Vec<(String, TypeId)> {
        let projections: Vec<(String, String)> = bounds
            .iter()
            .flat_map(|bound| &bound.assoc_types)
            .filter_map(|binding| match &binding.ty {
                crate::ast::Type::NamespacedGeneric(ns) if ns.namespace == "Self" => {
                    Some((binding.name.clone(), ns.name.clone()))
                }
                _ => None,
            })
            .collect();
        projections
            .into_iter()
            .filter_map(|(name, assoc)| {
                // A frame that binds `Self::X` answers with the binding, one
                // that does not with the projection. Building that reads
                // `assoc`'s own bounds, which may name this pair again — two
                // assoc types bounded through each other have no fixpoint, so
                // the one already on the walk stays abstract.
                let answer = self.frame_projection(base, base_name, &assoc).or_else(|| {
                    let key = (base, assoc.clone());
                    if !self.assoc_binding_stack.insert(key.clone()) {
                        return None;
                    }
                    let built = self.make_frame_projection(base, base_name, &assoc);
                    self.assoc_binding_stack.shift_remove(&key);
                    Some(built)
                })?;
                Some((name, answer))
            })
            .collect()
    }
}
