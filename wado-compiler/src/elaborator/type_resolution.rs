//! AST Type to `TypeId` resolution.

use crate::ast::{AstId, GenericType, Type};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::types::TypeError;
use crate::symbol::SymbolKind;

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

    /// The trait arguments an impl writes beyond the declared defaults, as the
    /// identity its associated types register under. `Rhs = Self` makes
    /// `impl Add<Cm> for Cm` the defaulted instantiation a bare bound reaches,
    /// while `impl Add<Inch> for Cm` keys its own.
    pub(super) fn non_default_trait_args(
        &mut self,
        trait_type: &crate::ast::Type,
        target: &crate::ast::Type,
        trait_decl: crate::defs::DefId,
    ) -> Vec<TypeId> {
        let Some(params) = self
            .tysys
            .trait_env
            .trait_decl_headers
            .get(&trait_decl)
            .map(|header| header.type_params.clone())
        else {
            return Vec::new();
        };
        let kept = super::trait_env::non_default_arg_count(
            trait_type,
            target,
            &params,
            &self.tysys.resolutions,
        );
        let written: Vec<crate::ast::Type> = match trait_type {
            Type::Generic(generic) => generic.args.clone(),
            Type::NamespacedGeneric(ns) => ns.args.clone(),
            _ => Vec::new(),
        };
        written
            .iter()
            .take(kept)
            .map(|arg| self.resolve_type(arg))
            .collect()
    }

    /// Report `T::Output` where two of `T`'s bounds declare `Output`, and say
    /// whether it did. [`Self::bound_declaring_assoc_type`] would answer with
    /// the first, which is a coin toss the writer never made. Supertraits are
    /// not counted: a direct bound redeclaring an inherited name wins there.
    fn report_ambiguous_assoc_type(&self, param_name: &str, assoc_name: &str, span: Span) -> bool {
        let Some(bounds) = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(param_name)
        else {
            return false;
        };
        let declaring: Vec<String> = bounds
            .iter()
            .filter(|bound| {
                self.trait_assoc_type_decl(&bound.name, assoc_name)
                    .is_some()
            })
            .map(|bound| bound.name.clone())
            .collect();
        if declaring.len() < 2 {
            return false;
        }
        let _ = self.emit(TypeError::AmbiguousAssocType {
            assoc: assoc_name.to_string(),
            param: param_name.to_string(),
            traits: declaring,
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

            // First, check if the current bounds directly specify this assoc type.
            // e.g., I: IntoIterator<Item = u8> → I::Item resolves directly to u8.
            if let Some(direct_type) =
                self.frame_projection(param_type_id, &namespaced.namespace, &namespaced.name)
            {
                return direct_type;
            }

            let base_name = namespaced.namespace.clone();
            if self.report_ambiguous_assoc_type(&base_name, &namespaced.name, namespaced.span) {
                return TypeTable::ERROR;
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
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(base_name)?
            .clone();
        bounds
            .iter()
            .flat_map(|bound| &bound.assoc_types)
            .find(|binding| binding.name == assoc)
            .map(|binding| self.resolve_type(&binding.ty.clone()))
    }

    /// The projection `base::assoc` as this frame builds it, where `base_name`
    /// is the name the frame files `base`'s bounds under. The single builder,
    /// so a projection written in a signature and one synthesized for an
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

    /// [`Self::frame_projection`] scoped to one trait: `T: Mul<Output = T>`
    /// answers `T::Output` where a bare `T: Mul` leaves it abstract. A caller
    /// that knows which trait it dispatched through asks this one, so a second
    /// bound declaring the same name never answers for it.
    pub(super) fn frame_projection_of_trait(
        &mut self,
        base_name: &str,
        trait_: crate::defs::DefId,
        assoc: &str,
    ) -> Option<TypeId> {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(base_name)?
            .clone();
        let written = bounds.iter().find_map(|bound| {
            let fq = self.fq_trait_name_at(bound.id, &bound.name);
            (self.tysys.trait_env.trait_def_of_fq(&fq) == Some(trait_))
                .then(|| bound.assoc_types.iter().find(|b| b.name == assoc))
                .flatten()
        })?;
        Some(self.resolve_type(&written.ty.clone()))
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
                Some((name, self.frame_projection(base, base_name, &assoc)?))
            })
            .collect()
    }
}
