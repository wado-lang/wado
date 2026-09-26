//! AST Type to `TypeId` resolution.

use crate::ast::{AstId, Type};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{ResolvedType, SlotProjections, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::scope::{BinderInScope, ScopedBound};
use super::trait_query::SelfBinding;
use super::types::{TypeError, forward_type_param_defaults};
use crate::ast;
use crate::ast::{NamespacedGenericType, TraitBound};
use crate::defs::{DefId, DefKind};
use crate::elaborator::trait_env::{
    ViaClause, non_default_arg_count, written_arg_nodes, written_type_arg,
};
use crate::name::{FqTraitName, FqTypeName, namespace_member_alias};
use crate::symbol::SymbolKind;
use crate::tir::TraitRef;

/// What a trait's declared parameters stand for at a frame, by name.
pub(super) type ParamSpace = Vec<(String, TypeId)>;

/// A bound reachable from a frame, paired with the space its written types are
/// read in — empty for one the frame wrote itself.
type FrameBound = (ScopedBound, ParamSpace);

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
                // Resolve effect names in function type position
                let effects = self.resolve_effects(&func_ty.effects);
                self.tysys.type_table.borrow_mut().make_function_with_mut(
                    func_ty.is_mut,
                    params,
                    return_type,
                    effects,
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
                if let Some(BinderInScope { index, .. }) =
                    self.annotate_ctx.trait_ctx.type_params.get(name)
                {
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
    fn bound_declaring_assoc_type(&self, param_name: &str, assoc_name: &str) -> Option<DefId> {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(param_name)?;
        self.tysys
            .trait_env
            .bound_declaring_assoc_type(bounds, assoc_name, &self.tysys.resolutions)
    }

    /// The identity an impl header names: the trait, plus the arguments it
    /// writes beyond the declared defaults — `impl Add<Cm> for Cm` is the bare
    /// instantiation a bound reaches, `impl Add<Inch> for Cm` keys its own.
    pub(super) fn impl_trait_ref(
        &mut self,
        trait_type: &ast::Type,
        target: &ast::Type,
        trait_decl: DefId,
    ) -> TraitRef {
        let Some(params) = self
            .tysys
            .trait_env
            .decl_header_of(&trait_decl)
            .map(|header| header.type_params.clone())
        else {
            return TraitRef::bare(trait_decl);
        };
        let written = written_arg_nodes(trait_type);
        let kept = non_default_arg_count(written, Some(target), &params, &self.tysys.resolutions);
        let args = written
            .iter()
            .take(kept)
            .map(|arg| self.resolve_type(arg))
            .collect();
        TraitRef::new(trait_decl, args)
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
        // One bound cannot be a coin toss, and the walk below reads every
        // bound's declaration.
        if bounds.len() < 2 {
            return false;
        }
        let declaring: Vec<&ScopedBound> = bounds
            .iter()
            .filter(|bound| {
                self.trait_decl_of(bound)
                    .is_some_and(|decl| self.tysys.trait_env.declares_assoc_type(&decl, assoc_name))
            })
            .collect();
        if declaring.len() < 2 {
            return false;
        }
        // Bounds that all pin the name to one type name one answer between
        // them: `T: Add<Output = T> + Mul<Output = T>` is not a coin toss,
        // where `Add<Output = Cm> + Mul<Output = Area>` is.
        let pins: Vec<Option<FqTypeName>> = declaring
            .iter()
            .map(|bound| {
                bound
                    .assoc_types
                    .iter()
                    .find(|constraint| constraint.name == assoc_name)
                    .map(|constraint| written_type_arg(&constraint.ty, &self.tysys.resolutions))
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
    fn self_trait_declaring_assoc_type(&self, assoc_name: &str) -> Option<DefId> {
        let self_trait = self.annotate_ctx.trait_ctx.self_trait?;
        self.tysys
            .trait_env
            .trait_declaring_assoc_type(&self_trait, assoc_name)
    }

    /// Resolve a namespaced generic type like `ns::Type<T>` or `Self::Output`
    pub(super) fn resolve_namespaced_generic_type(
        &mut self,
        namespaced: &NamespacedGenericType,
    ) -> TypeId {
        // Handle Self::AssociatedType
        if namespaced.namespace == "Self" {
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
                && let Some(resolved) = self.assoc_bound_by_type(self_type, &namespaced.name)
            {
                return resolved;
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
            // A parameter standing in for `Self` carries the frame's bounds under
            // its own name, so `Self::Base` at `T: Constrained` asks what `T::Base` asks.
            let self_param = self
                .annotate_ctx
                .trait_ctx
                .self_type
                .and_then(|id| Some((id, self.tysys.binder_name(id)?)));
            if let Some((self_type, param_name)) = self_param
                && let Some(projection) =
                    self.make_frame_projection(self_type, &param_name, &namespaced.name)
            {
                return projection;
            }
            return self.unknown_namespaced_type("Self", &namespaced.name, namespaced.span);
        }

        // Handle T::AssociatedType where T is a type parameter in scope
        if let Some(&BinderInScope {
            type_id: param_type_id,
            ..
        }) = self
            .annotate_ctx
            .trait_ctx
            .type_params
            .get(&namespaced.namespace)
        {
            let base_name = namespaced.namespace.clone();
            return self.project_off(param_type_id, &base_name, namespaced);
        }

        // The alias belongs to whichever module wrote this node, so a type a
        // travelled expression spells `ns::Type` reads its author's `use ns`.
        if self
            .namespace_alias_source(&namespaced.namespace, namespaced.id)
            .is_some()
        {
            // `ns::Type` / `ns::Type<args>` (`ns` is a namespace-import alias):
            // resolve the `ns$Type` alias, which the import tier scopes to the
            // namespace's own module. Mirrors `canonical_ns_ref` for idents.
            let alias = namespace_member_alias(&namespaced.namespace, &namespaced.name);
            if namespaced.args.is_empty() {
                self.resolve_named_type(namespaced.id, &alias, namespaced.span, true)
            } else {
                self.resolve_generic_type(namespaced.id, &alias, &namespaced.args, namespaced.span)
            }
        } else {
            self.unknown_namespaced_type(&namespaced.namespace, &namespaced.name, namespaced.span)
        }
    }

    /// What a concrete `base` itself binds `assoc` to. A base still carrying a
    /// type parameter binds nothing yet, so the frame's bounds answer instead.
    fn assoc_bound_by_type(&mut self, base: TypeId, assoc: &str) -> Option<TypeId> {
        if self.tysys.type_table.borrow().contains_type_param(base) {
            return None;
        }
        self.tysys
            .type_table
            .borrow_mut()
            .resolve_assoc_type_of_instance(base, assoc)
    }

    /// The associated type `namespaced` names, projected off `base_type_id`,
    /// which the frame files under `base_name`.
    fn project_off(
        &mut self,
        base_type_id: TypeId,
        base_name: &str,
        namespaced: &NamespacedGenericType,
    ) -> TypeId {
        if let Some(resolved) = self.assoc_bound_by_type(base_type_id, &namespaced.name) {
            return resolved;
        }
        if self.report_ambiguous_assoc_type(base_name, &namespaced.name, namespaced.span) {
            return TypeTable::ERROR;
        }
        // What the frame's bounds bind it to, where they say: `I:
        // IntoIterator<Item = u8>` answers `I::Item` directly.
        if let Some(direct_type) = self.frame_projection(base_type_id, base_name, &namespaced.name)
        {
            return direct_type;
        }
        if let Some(projection) =
            self.make_frame_projection(base_type_id, base_name, &namespaced.name)
        {
            return projection;
        }
        // The frame files no bounds under that name, so what the base itself
        // carries answers: a projection travels with its own.
        if let Some(projected) = self.project_off_projection(base_type_id, &namespaced.name) {
            return projected;
        }
        self.unknown_namespaced_type(base_name, &namespaced.name, namespaced.span)
    }

    /// `base::assoc` where `base` is itself a projection, so the frame files no
    /// bounds under a name for it. Its own bounds travel with it, and the trait
    /// declaring `assoc` is found among those.
    fn project_off_projection(&mut self, base: TypeId, assoc: &str) -> Option<TypeId> {
        let ResolvedType::AssocTypeProjection { bounds, .. } =
            self.tysys.type_table.borrow().get(base).clone()
        else {
            return None;
        };
        let owning = bounds.iter().find_map(|bound| {
            let decl = bound.canonical()?;
            self.tysys
                .trait_env
                .trait_declaring_assoc_type(&decl, assoc)
        })?;
        Some(self.make_frame_projection_of_trait(base, "", owning, assoc))
    }

    /// What a type position's name denotes where it denotes no type: `an
    /// interface` or `a trait`, both of which share the type namespace.
    fn non_type_decl_kind(&self, site: AstId, name: &str) -> Option<&'static str> {
        if name == "Self" || self.annotate_ctx.trait_ctx.type_params.contains_key(name) {
            return None;
        }
        if let Some(def) = self.decl_key_at(Some(site), name) {
            return match self.tysys.resolutions.defs().kind(def) {
                DefKind::Effect => Some("an interface"),
                DefKind::Trait => Some("a trait"),
                _ => None,
            };
        }
        // The module being walked is not in the environment yet, so its own
        // symbol table is what answers for what it declares itself.
        match self.symbol_named(&self.current_module_source, name)?.kind {
            SymbolKind::Effect(_) => Some("an interface"),
            SymbolKind::Trait(_) => Some("a trait"),
            _ => None,
        }
    }

    /// Report a type position naming an `interface` or a `trait`, and say
    /// whether it did.
    pub(super) fn reject_non_type_decl(&mut self, site: AstId, name: &str, span: Span) -> bool {
        let Some(kind) = self.non_type_decl_kind(site, name) else {
            return false;
        };
        let _ = self.emit(TypeError::NotAType {
            name: name.to_string(),
            kind,
            span,
        });
        true
    }

    /// The types a turbofish supplies. One naming an `interface` or a `trait`
    /// is rejected here, since `unknown` would satisfy every bound in silence.
    pub(super) fn resolve_turbofish_args(&mut self, args: &[Type]) -> Vec<TypeId> {
        args.iter()
            .map(|ty| {
                // A name no declaration answers is left to the position's own
                // resolution, which reports it where an annotation would not.
                self.walk_type_heads(ty, &mut |scope, id, name, span, _| {
                    scope.reject_non_type_decl(id, name, span)
                });
                self.resolve_type(ty)
            })
            .collect()
    }

    /// Walk the named heads a written type reaches, outermost first. `head`
    /// takes a head's site, name, span and whether it carries arguments, and
    /// answers whether the walk stops there.
    pub(super) fn walk_type_heads(
        &mut self,
        ty: &Type,
        head: &mut impl FnMut(&mut Self, AstId, &str, Span, bool) -> bool,
    ) {
        match ty {
            Type::Named(named) => {
                head(self, named.id, &named.name, named.span, false);
            }
            Type::Generic(generic) => {
                if head(self, generic.id, &generic.name, generic.span, true) {
                    return;
                }
                for arg in &generic.args {
                    self.walk_type_heads(arg, head);
                }
            }
            Type::NamespacedGeneric(namespaced) => {
                // `Self::Assoc` and `T::Assoc` project through a type rather
                // than naming a declaration, and the projection is what
                // answers for them.
                let ns = &namespaced.namespace;
                let projects =
                    ns == "Self" || self.annotate_ctx.trait_ctx.type_params.contains_key(ns);
                if !projects
                    && head(
                        self,
                        namespaced.id,
                        &namespaced.name,
                        namespaced.name_span,
                        !namespaced.args.is_empty(),
                    )
                {
                    return;
                }
                for arg in &namespaced.args {
                    self.walk_type_heads(arg, head);
                }
            }
            Type::Function(func_ty) => {
                for param in &func_ty.params {
                    self.walk_type_heads(param, head);
                }
                self.walk_type_heads(&func_ty.return_type, head);
            }
            Type::Reference(inner) | Type::MutReference(inner) => {
                self.walk_type_heads(inner, head);
            }
            Type::Tuple(elements) => {
                for element in elements {
                    self.walk_type_heads(element, head);
                }
            }
            Type::TypePackSpread(_, _) | Type::Infer(_) | Type::Error(_) => {}
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

    /// Report an application writing more type arguments than `params` declares,
    /// and say whether it did. Fewer is [`Self::type_args_of_application`]'s: a
    /// turbofish may stop short, and the slots it does not name are inferred.
    fn reject_surplus_type_args(
        &mut self,
        name: &str,
        params: &[ast::GenericParam],
        args: &[Type],
        span: Span,
    ) -> bool {
        // A pack swallows every argument past the scalars ahead of it, so an
        // arity is only a ceiling when the declaration has none.
        if params.iter().any(|p| p.is_pack) {
            return false;
        }
        let expected = params.iter().filter(|p| !p.is_effect).count();
        self.reject_surplus_turbofish(name, expected, args.len(), span)
    }

    /// The type arguments an application of `def` supplies, each slot the site
    /// left out taken from the declaration's default.
    ///
    /// A written argument resolves at the use site and a default under the
    /// declaration, which is the whole point of filling them apart.
    pub(super) fn type_args_of_application(&mut self, def: DefId, args: &[Type]) -> Vec<TypeId> {
        let mut resolved: Vec<TypeId> = args.iter().map(|t| self.resolve_type(t)).collect();
        let arity = self
            .type_lookup()
            .declared_generic_params(def)
            .map_or(0, <[ast::GenericParam]>::len);
        if !self.type_param_defaults_are_ordered(def) {
            resolved.resize(arity.max(resolved.len()), TypeTable::ERROR);
            return resolved;
        }
        for slot in resolved.len()..arity {
            // A slot whose default does not resolve still takes the parameter it
            // declared: an application short of its arity reads as a different
            // type and buries the one diagnostic that names the cause.
            let filled = self
                .declared_default_type_arg(def, slot, &resolved)
                .or_else(|| {
                    self.type_lookup()
                        .declared_type_param_ids(def)?
                        .get(slot)
                        .copied()
                })
                .unwrap_or(TypeTable::ERROR);
            resolved.push(filled);
        }
        resolved
    }

    /// Report each default naming its own or a later parameter, which no
    /// argument has settled when the default is read.
    pub(super) fn report_forward_type_param_defaults(&mut self, params: &[ast::GenericParam]) {
        for (slot, referenced) in forward_type_param_defaults(params) {
            let _ = self.emit(TypeError::ForwardTypeParamDefault {
                param: params[slot].name.clone(),
                referenced,
                span: params[slot]
                    .default
                    .as_ref()
                    .expect("only a default names a parameter")
                    .span(),
            });
        }
    }

    /// Whether `def`'s declared defaults can be expanded at all: each names
    /// only parameters to its left, and the walk they set off terminates.
    pub(super) fn type_param_defaults_are_ordered(&mut self, def: DefId) -> bool {
        if let Some(&ordered) = self.checked_type_param_defaults.get(&def) {
            return ordered;
        }
        let Some(params) = self
            .type_lookup()
            .declared_generic_params(def)
            .map(<[ast::GenericParam]>::to_vec)
        else {
            return true;
        };
        if !self.type_lookup().type_param_defaults_terminate(def) {
            let name = self.tysys.resolutions.defs().name(def).to_string();
            let span = params
                .iter()
                .filter_map(|p| p.default.as_ref())
                .map(ast::Type::span)
                .next()
                .expect("a cycle through defaults needs a default");
            let _ = self.emit(TypeError::RecursiveTypeParamDefault { name, span });
            self.checked_type_param_defaults.insert(def, false);
            return false;
        }
        let ordered = forward_type_param_defaults(&params).is_empty();
        self.checked_type_param_defaults.insert(def, ordered);
        ordered
    }

    /// The type `def`'s slot takes from the `= Default` it declared, given the
    /// arguments `settled` ahead of it. `None` when the slot declares none, or
    /// the default names nothing this declaration can answer.
    ///
    /// Resolved with only the parameters `settled` covers in scope, so a
    /// default naming a parameter to its left (`struct Pair<A, B = A>`) stands
    /// for that parameter's argument and never for what the use site happens to
    /// call it.
    pub(super) fn declared_default_type_arg(
        &mut self,
        def: DefId,
        slot: usize,
        settled: &[TypeId],
    ) -> Option<TypeId> {
        // Asked here rather than by each caller: an ill-ordered declaration
        // answers nothing, and the one diagnostic that names why belongs to it
        // however the application reached it.
        if !self.type_param_defaults_are_ordered(def) {
            return None;
        }
        let params = self.type_lookup().declared_generic_params(def)?;
        let default = params.get(slot)?.default.clone()?;
        let names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
        let resolved = self.with_type_param_args(&names, settled, |e| e.resolve_type(&default));
        if resolved == TypeTable::ERROR || resolved == TypeTable::UNKNOWN {
            // Nothing else reports a default: no application writes it, so the
            // name that reached nothing is named at the declaration instead.
            let _ = self.emit(TypeError::UnknownType {
                name: self.get_type_name(&default),
                span: default.span(),
            });
            return None;
        }
        Some(resolved)
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
        if let Some(&BinderInScope { type_id, .. }) =
            self.annotate_ctx.trait_ctx.type_params.get(name)
        {
            return type_id;
        }

        if let Some(def) = self.decl_key_at(site, name) {
            if let Some(primitive) =
                TypeTable::primitive_of_decl(self.tysys.resolutions.defs(), def)
            {
                return primitive;
            }
            if let Some(expected) = self.bare_generic_type_arity(def) {
                // Every parameter declaring a default makes the bare name the
                // defaulted instantiation; otherwise the site must write them.
                // The application writes no argument, so every slot is filled
                // from the declaration by the one path that fills them.
                if self.type_lookup().every_type_param_defaults(def) {
                    return self.resolve_generic_type_at(site, name, &[], span);
                }
                if enforce_arity {
                    let _ = self.emit(TypeError::MissingTypeArguments {
                        name: name.to_string(),
                        expected,
                        span,
                    });
                    return TypeTable::ERROR;
                }
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
        TypeTable::UNKNOWN
    }

    /// The instance of the generic newtype `def` over `type_args`. Its base is a
    /// type the declaration wrote, so it resolves against the arguments.
    pub(super) fn generic_newtype_instance(
        &mut self,
        def: DefId,
        type_args: Vec<TypeId>,
    ) -> TypeId {
        let gn_info = self
            .lookup_generic_newtype_of_decl(def)
            .cloned()
            .expect("`def` declares a generic newtype");
        let names: Vec<String> = gn_info.type_params.iter().map(|p| p.name.clone()).collect();
        let base_type_id = self.with_type_param_args(&names, &type_args, |e| {
            e.resolve_type(&gn_info.base_type_ast)
        });
        self.tysys
            .type_table
            .borrow_mut()
            .make_newtype_instance(def, type_args, base_type_id)
    }

    /// How many type arguments the declaration `def` requires, when it requires
    /// any. The three kinds are asked of one declaration, so "is this generic"
    /// and "whose parameters are these" can never be about two of them.
    pub(super) fn bare_generic_type_arity(&self, def: DefId) -> Option<usize> {
        Some(self.type_lookup().declared_generic_params(def)?.len())
    }

    /// The type a generic application resolves to. `site` names the head's
    /// declaration; `name` is its spelling, for the diagnostics.
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
        let def = self.decl_key_at(site, name);
        let builder =
            def.and_then(|def| self.tysys.type_table.borrow().compiler_generic_builder(def));
        if let Some(make) = builder {
            let [arg] = args else {
                let _ = self.emit(TypeError::ArgumentCountMismatch {
                    expected: 1,
                    found: args.len(),
                    span,
                });
                return TypeTable::ERROR;
            };
            let elem = self.resolve_type(arg);
            return make(&mut self.tysys.type_table.borrow_mut(), elem);
        }
        // The kind of the declaration the head names decides which shape is built.
        let Some(def) = def else {
            return self.resolve_generic_type_out_of_scope(site, name, args, span);
        };
        let struct_info = self.lookup_struct_fields_of_decl(def).cloned();
        // A trait head (`impl IndexValue<i32> for T`) keeps its parameters on
        // its own declaration, so only a type declaration's list is a ceiling.
        let declared = struct_info
            .as_ref()
            .map(|info| info.type_params.clone())
            .or_else(|| {
                self.lookup_variant_case_of_decl(def)
                    .map(|info| info.type_params.clone())
            })
            .or_else(|| {
                self.lookup_generic_newtype_of_decl(def)
                    .map(|info| info.type_params.clone())
            });
        if let Some(params) = declared
            && self.reject_surplus_type_args(name, &params, args, span)
        {
            return TypeTable::ERROR;
        }
        let instance_params = match struct_info {
            Some(info) if !info.type_params.is_empty() => Some(info.type_params),
            _ => self
                .lookup_variant_case_of_decl(def)
                .map(|info| info.type_params.clone()),
        };
        if let Some(params) = instance_params {
            if params.is_empty() {
                TypeTable::UNKNOWN
            } else {
                let type_args = self.type_args_of_application(def, args);
                self.check_type_decl_arg_bounds(def, &type_args, span);
                // Named by the declaration its head resolved to, the arguments beside
                // it rather than fused into a rendered `Box<i32>` no `impl` header writes.
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_generic_instance(def, type_args)
            }
        } else if self.lookup_generic_newtype_of_decl(def).is_some() {
            let type_args = self.type_args_of_application(def, args);
            self.check_type_decl_arg_bounds(def, &type_args, span);
            self.generic_newtype_instance(def, type_args)
        } else {
            self.resolve_generic_type_out_of_scope(site, name, args, span)
        }
    }

    /// The retry a generic application gets when its head names nothing here.
    /// A travelled expression may spell a type only its own module imports
    /// (`entries: TreeMap<K, V> = TreeMap::new()` taken cross-module), and the
    /// aliases that spelling needs are that module's.
    fn resolve_generic_type_out_of_scope(
        &mut self,
        site: Option<AstId>,
        name: &str,
        args: &[Type],
        span: Span,
    ) -> TypeId {
        let Some(home) = self.annotate_ctx.resolving_home.clone() else {
            return TypeTable::UNKNOWN;
        };
        if home == self.current_module_source {
            return TypeTable::UNKNOWN;
        }
        self.with_module_perspective_for(&home, |s| {
            s.resolve_generic_type_at(site, name, args, span)
        })
    }

    /// What this frame knows the projection `base::assoc` to be, where
    /// `base_name` is the name the frame files `base`'s bounds under.
    ///
    /// Two sources answer, in order: the bindings a projection carries
    /// (`S::SeqSerializer` knowing its `Ok`), then the enclosing bound
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
        let resolved = self.frame_assoc_bindings_of(base_name, assoc);
        // Two bounds binding it differently is the coin toss the caller's
        // ambiguity check reports; answering with the first would hide it.
        let first = *resolved.first()?;
        resolved.iter().all(|t| *t == first).then_some(first)
    }

    /// Every bound on `base_name` a projection may be answered from, each with
    /// the parameter space it was written in answered at this frame. A
    /// supertrait binds an assoc type too, so one walk serves every lookup.
    fn bound_closure_of(&mut self, base_name: &str) -> Option<Vec<FrameBound>> {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(base_name)?
            .clone();
        // A bound's arguments are read while this closure is built, so
        // `T: Uses<T::Item>` asks for it again. The closure cannot answer
        // itself: the re-entrant ask gets nothing, and `T::Item` stays the
        // abstract projection its instantiation settles.
        let binder = self
            .annotate_ctx
            .trait_ctx
            .type_params
            .get(base_name)?
            .type_id;
        self.unless_on_walk(
            |scope| &mut scope.bound_closure_stack,
            binder,
            |e| {
                let mut out: Vec<FrameBound> = Vec::new();
                for bound in bounds {
                    let Some(decl) = e.trait_decl_of(&bound) else {
                        out.push((bound, Vec::new()));
                        continue;
                    };
                    let written: Vec<TypeId> = e.in_bound_frame(&bound, &ParamSpace::new(), |e| {
                        bound
                            .type_args
                            .iter()
                            .map(|ty| e.resolve_type(ty))
                            .collect()
                    });
                    let at_decl = e.param_space_of(decl, &written);
                    let args: Vec<TypeId> = at_decl.iter().map(|(_, id)| *id).collect();
                    let closure = e
                        .tysys
                        .trait_env
                        .supertrait_closure_declared(&decl)
                        .1
                        .to_vec();
                    out.push((bound, at_decl));
                    for inherited in closure {
                        let space = e.inherited_space(decl, &args, &inherited.via);
                        // An inherited clause is written in `decl`'s own space,
                        // where `Self` is the bounded type — `binder` here.
                        let self_binding = SelfBinding {
                            type_id: binder,
                            declaring_trait: Some(decl),
                        };
                        out.push((ScopedBound::new(inherited.bound, Some(self_binding)), space));
                    }
                }
                Some(out)
            },
        )
    }

    /// The space an inherited clause's own bound is written in, reached from
    /// `root`'s parameters standing at `args`.
    ///
    /// Walking the chain is what carries a site's arguments down to the trait
    /// that declared the clause: each step is written in the one before it, so
    /// resolving them in order is the substitution — there is no spelling to
    /// rewrite (WEP-2026-08-12).
    pub(super) fn inherited_space(
        &mut self,
        root: DefId,
        args: &[TypeId],
        via: &[ViaClause],
    ) -> ParamSpace {
        let mut space = self.param_space_of(root, args);
        for step in via {
            let args = self.resolve_in_space(&space, &step.bound.type_args);
            space = self.param_space_of(step.decl, &args);
        }
        space
    }

    /// `decl`'s declared parameters paired with what `args` answers for them.
    /// A position `args` leaves out stands at its declared default, read against
    /// the positions settled before it: `B<X, Y = i32>` written `B<T>` answers
    /// `Y` with `i32`, and `P<V, W = V>` answers `W` with `V`'s.
    ///
    /// Every parameter enters the space, an unanswered one as [`TypeTable::UNKNOWN`]:
    /// it stands for no type here, and leaving it out would let a same-named
    /// declaration at the reading site answer in its place.
    fn param_space_of(&mut self, decl: DefId, args: &[TypeId]) -> ParamSpace {
        let params = self.tysys.trait_env.trait_decl_params(decl).to_vec();
        let mut space = ParamSpace::new();
        for (index, param) in params.iter().enumerate() {
            let arg = match (args.get(index), param.default.clone()) {
                (Some(&arg), _) => arg,
                (None, Some(default)) => {
                    self.resolve_in_space(&space, std::slice::from_ref(&default))[0]
                }
                (None, None) => TypeTable::UNKNOWN,
            };
            space.push((param.name.clone(), arg));
        }
        space
    }

    /// Run `body` with `space` answering for the names its types were written
    /// in, layered on this frame.
    pub(super) fn in_space<R>(
        &mut self,
        space: &ParamSpace,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let (names, args): (Vec<String>, Vec<TypeId>) = space.iter().cloned().unzip();
        self.with_type_params_bound(&names, &args, body)
    }

    /// Run `body` in the frame `scoped` was written in: its parameter space and
    /// the `Self` it meant. Resolving a bound's own types needs both.
    pub(super) fn in_bound_frame<R>(
        &mut self,
        scoped: &ScopedBound,
        space: &ParamSpace,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let binding = scoped.self_binding;
        self.in_space(space, |e| e.under_self_binding(binding, body))
    }

    /// `types` resolved with `space` answering for the names it was written in.
    fn resolve_in_space(&mut self, space: &ParamSpace, types: &[ast::Type]) -> Vec<TypeId> {
        let types = types.to_vec();
        self.in_space(space, |e| {
            types.iter().map(|ty| e.resolve_type(ty)).collect()
        })
    }

    /// What every bound in the closure binds `assoc` to, each resolved in the
    /// space it was written in. A binding whose space leaves its right-hand side
    /// unanswered is dropped, so the projection stays abstract rather than
    /// standing at a type nothing named.
    fn frame_assoc_bindings_of(&mut self, base_name: &str, assoc: &str) -> Vec<TypeId> {
        let mut out = Vec::new();
        for (bound, space) in self.bound_closure_of(base_name).unwrap_or_default() {
            for binding in bound.assoc_types.iter().filter(|b| b.name == assoc) {
                let ty = binding.ty.clone();
                let resolved = self.in_bound_frame(&bound, &space, |e| e.resolve_type(&ty));
                if resolved != TypeTable::UNKNOWN {
                    out.push(resolved);
                }
            }
        }
        out
    }

    /// Report `base::member` as a name that denotes no type.
    fn unknown_namespaced_type(&mut self, base: &str, member: &str, span: Span) -> TypeId {
        let _ = self.emit(TypeError::UnknownType {
            name: format!("{base}::{member}"),
            span,
        });
        TypeTable::ERROR
    }

    /// The projection `base::assoc` as this frame builds it, or `None` when no
    /// bound on `base` declares `assoc`.
    // The single builder, so one written in a signature and one synthesized for
    // an expression intern to the same type.
    pub(super) fn make_frame_projection(
        &mut self,
        base: TypeId,
        base_name: &str,
        assoc: &str,
    ) -> Option<TypeId> {
        let owning_trait = self.bound_declaring_assoc_type(base_name, assoc)?;
        Some(self.make_frame_projection_of_trait(base, base_name, owning_trait, assoc))
    }

    /// [`Self::make_frame_projection`] for a caller that already knows which
    /// trait declares `assoc`. `T: Add + Mul` declares `Output` twice, and only
    /// the site that dispatched can say which one `a * b` yields.
    // The one place a projection is built from a trait. It interns by its
    // bounds, so those can only be the declaration's (WEP 2026-08-12).
    pub(super) fn make_frame_projection_of_trait(
        &mut self,
        base: TypeId,
        base_name: &str,
        owning_trait: DefId,
        assoc: &str,
    ) -> TypeId {
        let assoc_bounds = self
            .tysys
            .trait_env
            .assoc_type_decl(&owning_trait, assoc)
            .map_or_else(Vec::new, |decl| decl.bounds.clone());
        let bound_names: Vec<FqTraitName> = assoc_bounds
            .iter()
            .map(|b| self.fq_trait_name_of(b))
            .collect();
        let assoc_type_bindings = self.frame_assoc_bindings(base, base_name, &assoc_bounds);
        self.tysys
            .type_table
            .borrow_mut()
            .make_assoc_type_projection(
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
        trait_: DefId,
        assoc: &str,
    ) -> Option<TypeId> {
        let (written, space, scoped) =
            self.bound_closure_of(base_name)?
                .into_iter()
                .find_map(|(bound, space)| {
                    (self.trait_decl_of(&bound) == Some(trait_))
                        .then(|| bound.assoc_types.iter().find(|b| b.name == assoc).cloned())
                        .flatten()
                        .map(|binding| (binding.ty, space, bound))
                })?;
        let resolved = self.in_bound_frame(&scoped, &space, |e| e.resolve_type(&written));
        (resolved != TypeTable::UNKNOWN).then_some(resolved)
    }

    /// `ty` with each projection over this frame's parameters answered by its
    /// bounds: a use site handing `C` to `T` in `&T::Value` reads `C::Value`.
    pub(super) fn answer_frame_projections(&mut self, ty: TypeId) -> TypeId {
        if self.annotate_ctx.trait_ctx.type_param_bounds.is_empty() {
            return ty;
        }
        let asked: Vec<(u32, String, DefId, String)> = {
            let table = self.tysys.type_table.borrow();
            let binders = &self.annotate_ctx.trait_ctx.type_params;
            table
                .assoc_type_projections(ty)
                .into_iter()
                .filter_map(|p| {
                    let ResolvedType::AssocTypeProjection {
                        param_id,
                        owning_trait,
                        assoc_name,
                        ..
                    } = table.get(p)
                    else {
                        unreachable!("assoc_type_projections answers projections");
                    };
                    let ResolvedType::TypeParam { index, name } = table.get(*param_id) else {
                        return None;
                    };
                    // A callee's parameter the substitution left behind may
                    // share a name with one of this frame's binders.
                    (binders.get(name)?.type_id == *param_id)
                        .then(|| (*index, name.clone(), *owning_trait, assoc_name.clone()))
                })
                .collect()
        };
        let mut answers = SlotProjections::default();
        for (slot, base_name, trait_, assoc) in asked {
            if let Some(answer) = self.frame_projection_of_trait(&base_name, trait_, &assoc) {
                answers
                    .entry(slot)
                    .or_default()
                    .push((trait_, assoc, answer));
            }
        }
        if answers.is_empty() {
            return ty;
        }
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_type_params_with(ty, &IndexMap::default(), &answers)
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
        bounds: &[TraitBound],
    ) -> Vec<(String, TypeId)> {
        let projections: Vec<(String, String)> = bounds
            .iter()
            .flat_map(|bound| &bound.assoc_types)
            .filter_map(|binding| match &binding.ty {
                ast::Type::NamespacedGeneric(ns) if ns.namespace == "Self" => {
                    Some((binding.name.clone(), ns.name.clone()))
                }
                _ => None,
            })
            .collect();
        projections
            .into_iter()
            .filter_map(|(name, assoc)| {
                // A frame that binds `Self::X` answers with the binding, one
                // that does not with the projection — which is built from
                // `assoc`'s own bounds, so a pair already on the walk recurses.
                let answer = self.frame_projection(base, base_name, &assoc).or_else(|| {
                    self.unless_on_walk(
                        |scope| &mut scope.assoc_binding_stack,
                        (base, assoc.clone()),
                        |e| e.make_frame_projection(base, base_name, &assoc),
                    )
                })?;
                Some((name, answer))
            })
            .collect()
    }
}
