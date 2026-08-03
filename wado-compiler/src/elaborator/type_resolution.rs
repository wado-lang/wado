//! AST Type to `TypeId` resolution.

use crate::ast::{GenericType, Type};
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
                self.resolve_named_type(&named.name, named.span, true)
            }
            Type::Generic(generic) => {
                self.record_type_name_reference(generic.id, &generic.name);
                self.resolve_generic_type(&generic.name, &generic.args, generic.span)
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
    fn bound_declaring_assoc_type(&self, param_name: &str, assoc_name: &str) -> Option<String> {
        self.annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(param_name)
            .into_iter()
            .flatten()
            .map(|bound| bound.name.clone())
            .find(|trait_name| {
                self.find_trait_decl_assoc_type_decls(trait_name)
                    .is_some_and(|decls| decls.iter().any(|d| d.name == assoc_name))
            })
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
            if let Some(self_type) = self.annotate_ctx.trait_ctx.self_type
                && matches!(
                    self.tysys.type_table.borrow().get(self_type),
                    ResolvedType::TypeParam { .. }
                )
            {
                let assoc_bounds = self.find_assoc_type_bounds(self_type, &namespaced.name);
                let bound_names: Vec<String> =
                    assoc_bounds.iter().map(|b| b.name.clone()).collect();
                let assoc_type_bindings = self.compute_assoc_type_bindings("Self", &assoc_bounds);
                let owning_trait = self.bound_declaring_assoc_type("Self", &namespaced.name);
                return self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_assoc_type_projection_of_trait(
                        self_type,
                        owning_trait,
                        namespaced.name.clone(),
                        bound_names,
                        assoc_type_bindings,
                    );
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
                self.find_direct_assoc_type_binding(&namespaced.namespace, &namespaced.name)
            {
                return direct_type;
            }

            // Look up trait bounds on the associated type from the trait declaration.
            let assoc_bounds = self.find_assoc_type_bounds(param_type_id, &namespaced.name);
            let bound_names: Vec<String> = assoc_bounds.iter().map(|b| b.name.clone()).collect();

            // Compute assoc_type_bindings by resolving Self::X in the assoc type's bounds.
            // e.g., IntoIterator::Iter has bound Iterator<Item = Self::Item>.
            // With I: IntoIterator<Item = u8>, Self::Item = I::Item = u8,
            // so I::Iter.assoc_type_bindings = [("Item", u8_typeid)].
            let assoc_type_bindings =
                self.compute_assoc_type_bindings(&namespaced.namespace.clone(), &assoc_bounds);

            let owning_trait =
                self.bound_declaring_assoc_type(&namespaced.namespace, &namespaced.name);
            return self
                .tysys
                .type_table
                .borrow_mut()
                .make_assoc_type_projection_of_trait(
                    param_type_id,
                    owning_trait,
                    namespaced.name.clone(),
                    bound_names,
                    assoc_type_bindings,
                );
        }

        if self
            .sem
            .imports
            .namespace_imports
            .contains_key(namespaced.namespace.as_str())
        {
            // `ns::Type` / `ns::Type<args>` (`ns` is a namespace-import alias):
            // resolve the `ns$Type` alias, which `imported_type_sources` scopes
            // to the namespace's own module. Mirrors `canonical_ns_ref` for
            // idents.
            let alias =
                crate::name::namespace_member_alias(&namespaced.namespace, &namespaced.name);
            if namespaced.args.is_empty() {
                self.resolve_named_type(&alias, namespaced.span, true)
            } else {
                self.resolve_generic_type(&alias, &namespaced.args, namespaced.span)
            }
        } else {
            let _ = self.emit(TypeError::UnknownType {
                name: format!("{}::{}", namespaced.namespace, namespaced.name),
                span: namespaced.span,
            });
            TypeTable::ERROR
        }
    }

    /// Resolve a named type
    pub(super) fn resolve_named_type(
        &mut self,
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

        if enforce_arity && let Some(expected) = self.bare_generic_type_arity(name) {
            let _ = self.emit(TypeError::MissingTypeArguments {
                name: name.to_string(),
                expected,
                span,
            });
            return TypeTable::ERROR;
        }
        if let Some(type_id) = self.lookup_newtype(name) {
            type_id
        } else if let Some(struct_info) = self.lookup_struct_fields(name) {
            self.tysys
                .type_table
                .borrow()
                .type_id_of_decl(struct_info.defined_at)
        } else if let Some(variant_info) = self.lookup_variant_case(name) {
            self.tysys
                .type_table
                .borrow()
                .type_id_of_decl(variant_info.defined_at)
        } else if let Some(enum_info) = self.lookup_enum_case(name) {
            self.tysys
                .type_table
                .borrow()
                .type_id_of_decl(enum_info.defined_at)
        } else if let Some(resource_info) = self.lookup_resource_type(name) {
            self.tysys
                .type_table
                .borrow()
                .type_id_of_decl(resource_info.defined_at)
        } else if let Some(scope_mod) = self.annotate_ctx.default_scope_module.clone()
            && scope_mod != self.current_module_source
        {
            // A default re-resolved at the caller may name a type
            // private to the callee's module (`fn f<T = Priv>()` called
            // cross-module); the caller can't name it, so retry in the
            // callee's perspective. Mirrors the ident / call fallback.
            self.with_module_perspective_for(&scope_mod, |s| {
                s.resolve_named_type(name, span, enforce_arity)
            })
        } else {
            TypeTable::UNKNOWN
        }
    }

    fn bare_generic_type_arity(&self, name: &str) -> Option<usize> {
        // `lookup_struct_fields` alone decides whether `name` is generic:
        // it already applies the correct precedence (a local struct —
        // `Stmt::Item`, see `resolve_local_struct` — shadows a same-named
        // module-level one), so basing this on `info.type_param_bounds`
        // directly keeps the "is this generic" question and "which struct's
        // info is this" question about the *same* struct. A separate
        // name-keyed gate lets the two disagree: dispatch enters on the
        // module-level struct's registration while `lookup_struct_fields`
        // returns a same-named local, non-generic shadow's info.
        if let Some(info) = self.lookup_struct_fields(name)
            && !info.type_param_bounds.is_empty()
        {
            return Some(info.type_param_bounds.len());
        }
        if let Some(info) = self.lookup_variant_case(name)
            && !info.type_params.is_empty()
        {
            return Some(info.type_params.len());
        }
        if let Some(info) = self.lookup_generic_newtype(name)
            && !info.type_params.is_empty()
        {
            return Some(info.type_params.len());
        }
        None
    }

    /// Resolve a generic type
    pub(super) fn resolve_generic_type(&mut self, name: &str, args: &[Type], span: Span) -> TypeId {
        // Prelude module path for looking up Option/Result
        let prelude_source = ModuleSource::prelude();

        match name {
            "Option" => {
                // Verify Option variant exists in symbol table (declared in prelude)
                // First check local imports, then fall back to prelude module
                let found_as_variant = self
                    .symbols
                    .lookup(&self.current_module_source, "Option")
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
                // Check if it's a user-defined generic struct.
                // `lookup_struct_fields` alone decides this (see
                // `bare_generic_type_arity` for why a separate name-keyed
                // gate is wrong: it can name a different struct than the one
                // this lookup returns when a local struct — `Stmt::Item`, see
                // `resolve_local_struct` — shadows a same-named
                // module-level generic one).
                let struct_info = self.lookup_struct_fields(name).cloned();
                if struct_info
                    .as_ref()
                    .is_some_and(|info| !info.type_param_bounds.is_empty())
                {
                    // Resolve type arguments
                    let type_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();

                    // `info.name` is the type's storage identity — the bare
                    // declared name for a module-level struct, or the
                    // internal mangled name for a local one (so the
                    // `GenericInstance` below carries a name that
                    // `struct_fields_in` can find later, the same way a
                    // concrete local struct's `TypeId` does — see
                    // `resolve_local_struct`).
                    let identity_name = struct_info
                        .as_ref()
                        .map(|info| info.name.clone())
                        .unwrap_or_else(|| name.to_string());
                    let module_source = struct_info
                        .as_ref()
                        .map(|info| info.module_source.clone())
                        .unwrap_or_else(|| self.current_module_source.clone());

                    // Check trait bounds for each type argument
                    if let Some(info) = &struct_info {
                        for (i, (param_name, bounds)) in info.type_param_bounds.iter().enumerate() {
                            if let Some(&type_arg) = type_args.get(i) {
                                for bound in bounds {
                                    if !self.tysys.type_implements_trait(
                                        &self.annotate_ctx,
                                        &self.type_lookup(),
                                        type_arg,
                                        bound,
                                    ) {
                                        // Get the type name for the error message
                                        let type_name = self.tysys.type_id_to_string(type_arg);
                                        let reason = self.tysys.trait_unimpl_reason_chain(
                                            &self.annotate_ctx,
                                            &self.type_lookup(),
                                            type_arg,
                                            bound,
                                        );
                                        let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                                            type_name,
                                            trait_name: bound.clone(),
                                            param_name: param_name.clone(),
                                            reason,
                                            span,
                                        });
                                    }
                                }
                            }
                        }
                    }

                    // Create a GenericInstance type
                    self.tysys.type_table.borrow_mut().make_generic_instance(
                        identity_name,
                        module_source,
                        type_args,
                    )
                } else if let Some(variant_info) = self.lookup_variant_case(name).cloned() {
                    // Check if it's a generic variant (like Result<T, E>)
                    if variant_info.type_params.is_empty() {
                        TypeTable::UNKNOWN
                    } else {
                        let type_args: Vec<TypeId> =
                            args.iter().map(|t| self.resolve_type(t)).collect();
                        self.tysys.type_table.borrow_mut().make_generic_instance(
                            name.to_string(),
                            variant_info.module_source,
                            type_args,
                        )
                    }
                } else if let Some(gn_info) = self.lookup_generic_newtype(name).cloned() {
                    // Generic newtype instantiation: type MyArray<T> = List<T>
                    // Substitute type params in the base type AST, then resolve
                    let concrete_base_ast =
                        substitute_type_params(&gn_info.base_type_ast, &gn_info.type_params, args);
                    let base_type_id = self.resolve_type(&concrete_base_ast);
                    // Build a display name like "MyArray<i32>"
                    let resolved_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();
                    let arg_names: Vec<String> = resolved_args
                        .iter()
                        .map(|&tid| self.tysys.type_id_to_string(tid))
                        .collect();
                    let display_name = format!("{}<{}>", name, arg_names.join(", "));
                    self.tysys.type_table.borrow_mut().make_newtype(
                        display_name,
                        gn_info.module_source,
                        base_type_id,
                    )
                } else if let Some(scope_mod) = self.annotate_ctx.default_scope_module.clone()
                    && scope_mod != self.current_module_source
                {
                    // A foreign default re-resolved at the caller may name a
                    // generic type the callee's module imports but the caller
                    // does not (`entries: TreeMap<K, V> = TreeMap::new()`
                    // omitted cross-module); retry in the callee's perspective.
                    // Mirrors the `resolve_named_type` fallback for bare names.
                    self.with_module_perspective_for(&scope_mod, |s| {
                        s.resolve_generic_type(name, args, span)
                    })
                } else {
                    TypeTable::UNKNOWN
                }
            }
        }
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

    /// Check if type parameter `param_name` has a bound that directly specifies `assoc_name`.
    /// e.g., I: `IntoIterator`<Item = u8> → `find_direct_assoc_type_binding("I`", "Item") = Some(u8)
    fn find_direct_assoc_type_binding(
        &mut self,
        param_name: &str,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(param_name)?
            .clone();
        for bound in &bounds {
            for assoc in &bound.assoc_types {
                if assoc.name == assoc_name {
                    return Some(self.resolve_type(&assoc.ty));
                }
            }
        }
        None
    }

    /// Compute `assoc_type_bindings` for an `AssocTypeProjection` by resolving `Self::X` references.
    /// e.g., `IntoIterator::Iter` has bound Iterator<Item = `Self::Item`>.
    /// With I: `IntoIterator`<Item = u8>, Self = I, so `Self::Item` = `I::Item` = u8.
    /// Result: [("Item", `u8_typeid`)].
    fn compute_assoc_type_bindings(
        &mut self,
        source_param_name: &str,
        assoc_bounds: &[crate::ast::TraitBound],
    ) -> Vec<(String, TypeId)> {
        let mut bindings = Vec::new();
        for bound in assoc_bounds {
            for assoc in &bound.assoc_types.clone() {
                // Resolve Self::X in the context of source_param: Self = source_param
                if let crate::ast::Type::NamespacedGeneric(ns) = &assoc.ty
                    && ns.namespace == "Self"
                    && let Some(direct) =
                        self.find_direct_assoc_type_binding(source_param_name, &ns.name)
                {
                    bindings.push((assoc.name.clone(), direct));
                }
            }
        }
        bindings
    }
}
