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
                self.resolve_named_type(&named.name, named.span)
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
                if let Some((index, _type_id)) = self.trait_ctx.type_params.get(name) {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_pack(name.clone(), *index)
                } else {
                    let _ = self.logger.error(TypeError::UnknownType {
                        name: format!("..{name}"),
                        span: *span,
                    });
                    TypeTable::ERROR
                }
            }
            // Parser error-recovery placeholder: the syntax error was already
            // reported, so resolve to the error type to suppress cascades.
            Type::Error(_) => TypeTable::ERROR,
        }
    }

    /// Resolve a namespaced generic type like `builtin::array<T>` or `Self::Output`
    pub(super) fn resolve_namespaced_generic_type(
        &mut self,
        namespaced: &crate::ast::NamespacedGenericType,
    ) -> TypeId {
        // Handle Self::AssociatedType
        if namespaced.namespace.as_str() == "Self" {
            // Look up the associated type binding
            if let Some(&type_id) = self.trait_ctx.assoc_type_bindings.get(&namespaced.name) {
                return type_id;
            }
            // If not found, it's an unknown associated type
            let _ = self.logger.error(TypeError::UnknownType {
                name: format!("Self::{}", namespaced.name),
                span: namespaced.span,
            });
            return TypeTable::ERROR;
        }

        // Handle T::AssociatedType where T is a type parameter in scope
        if let Some(&(_, param_type_id)) = self.trait_ctx.type_params.get(&namespaced.namespace) {
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
                // resolve_generic_assoc_type can derive i32 from ("ListIter", "Item") → TypeParam(0).
                if let Some(resolved) = self
                    .tysys
                    .type_table
                    .borrow()
                    .resolve_generic_assoc_type(param_type_id, &namespaced.name)
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

            return self
                .tysys
                .type_table
                .borrow_mut()
                .make_assoc_type_projection(
                    param_type_id,
                    namespaced.name.clone(),
                    bound_names,
                    assoc_type_bindings,
                );
        }

        if namespaced.namespace.as_str() == "builtin" {
            if namespaced.name.as_str() == "array" {
                if namespaced.args.len() != 1 {
                    let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                        expected: 1,
                        found: namespaced.args.len(),
                        span: namespaced.span,
                    });
                    return TypeTable::ERROR;
                }
                let element_type = self.resolve_type(&namespaced.args[0]);
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_builtin_array(element_type)
            } else {
                let _ = self.logger.error(TypeError::UnknownType {
                    name: format!("builtin::{}", namespaced.name),
                    span: namespaced.span,
                });
                TypeTable::ERROR
            }
        } else {
            let _ = self.logger.error(TypeError::UnknownType {
                name: format!("{}::{}", namespaced.namespace, namespaced.name),
                span: namespaced.span,
            });
            TypeTable::ERROR
        }
    }

    /// Resolve a named type
    pub(super) fn resolve_named_type(&mut self, name: &str, _span: Span) -> TypeId {
        // Handle `Self` type reference in impl blocks
        if name == "Self" {
            if let Some(self_type) = self.trait_ctx.self_type {
                return self_type;
            }
            // Self used outside of impl block - return Unknown
            return TypeTable::UNKNOWN;
        }

        // First check if it's a type parameter in scope
        if let Some(&(_, type_id)) = self.trait_ctx.type_params.get(name) {
            return type_id;
        }

        match name {
            // Primitives
            "i8" => TypeTable::I8,
            "i16" => TypeTable::I16,
            "i32" => TypeTable::I32,
            "i64" => TypeTable::I64,
            "u8" => TypeTable::U8,
            "u16" => TypeTable::U16,
            "u32" => TypeTable::U32,
            "u64" => TypeTable::U64,
            "f32" => TypeTable::F32,
            "f64" => TypeTable::F64,
            "bool" => TypeTable::BOOL,
            "char" => TypeTable::CHAR,
            "v128" => TypeTable::V128,
            "()" => TypeTable::UNIT,
            "!" => TypeTable::NEVER,

            // Check newtypes, struct definitions, and variants
            _ => {
                if let Some(type_id) = self.lookup_newtype(name) {
                    type_id
                } else if let Some(struct_info) = self.lookup_struct_fields(name) {
                    // Use canonical name (not import alias) so interning produces the same TypeId
                    let (n, src) = (struct_info.name.clone(), struct_info.module_source.clone());
                    self.tysys.type_table.borrow_mut().make_struct(n, src)
                } else if let Some(variant_info) = self.lookup_variant_case(name) {
                    let (n, src) = (
                        variant_info.name.clone(),
                        variant_info.module_source.clone(),
                    );
                    self.tysys.type_table.borrow_mut().make_variant(n, src)
                } else if let Some(enum_info) = self.lookup_enum_case(name) {
                    let (n, src) = (enum_info.name.clone(), enum_info.module_source.clone());
                    self.tysys.type_table.borrow_mut().make_enum(n, src)
                } else if let Some(resource_info) = self.lookup_resource_type(name) {
                    let (n, src) = (
                        resource_info.name.clone(),
                        resource_info.module_source.clone(),
                    );
                    self.tysys.type_table.borrow_mut().make_resource(n, src)
                } else {
                    // Unknown type
                    TypeTable::UNKNOWN
                }
            }
        }
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
                    .lookup("Option")
                    .or_else(|| self.symbols.lookup_in_module(&prelude_source, "Option"))
                    .is_some_and(|s| matches!(s.kind, SymbolKind::Variant(_)));

                if !found_as_variant {
                    // Option not found as a variant - likely #![no_prelude] without explicit import
                    let _ = self.logger.error(TypeError::UnknownType {
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
            "Array" => {
                if args.len() != 1 {
                    let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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
                // Check if it's a user-defined generic struct
                if self.sem.decls.generic_struct_names.contains(name) {
                    // Resolve type arguments
                    let type_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();

                    // Get struct info for module source and bounds checking
                    let struct_info = self.lookup_struct_fields(name).cloned();
                    let module_source = struct_info
                        .as_ref()
                        .map(|info| info.module_source.clone())
                        .unwrap_or_else(|| self.current_module_source.clone());

                    // Check trait bounds for each type argument
                    if let Some(info) = &struct_info {
                        for (i, (param_name, bounds)) in info.type_param_bounds.iter().enumerate() {
                            if let Some(&type_arg) = type_args.get(i) {
                                for bound in bounds {
                                    if !self.type_implements_trait(type_arg, bound) {
                                        // Get the type name for the error message
                                        let type_name = self.type_id_to_string(type_arg);
                                        let reason =
                                            self.trait_unimpl_reason_chain(type_arg, bound);
                                        let _ =
                                            self.logger.error(TypeError::TraitBoundNotSatisfied {
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
                        name.to_string(),
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
                        .map(|&tid| self.type_id_to_string(tid))
                        .collect();
                    let display_name = format!("{}<{}>", name, arg_names.join(", "));
                    self.tysys.type_table.borrow_mut().make_newtype(
                        display_name,
                        gn_info.module_source,
                        base_type_id,
                    )
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

        // Search all trait declarations for one containing this associated type
        for (_, module) in self.loaded_modules {
            for item in &module.items {
                if let crate::ast::Item::Trait(trait_decl) = item {
                    for assoc in &trait_decl.associated_types {
                        if assoc.name == assoc_name {
                            return assoc.bounds.clone();
                        }
                    }
                }
            }
        }
        for item in self.current_module_items {
            if let crate::ast::Item::Trait(trait_decl) = item {
                for assoc in &trait_decl.associated_types {
                    if assoc.name == assoc_name {
                        return assoc.bounds.clone();
                    }
                }
            }
        }

        Vec::new()
    }

    /// Check if type parameter `param_name` has a bound that directly specifies `assoc_name`.
    /// e.g., I: `IntoIterator`<Item = u8> → `find_direct_assoc_type_binding("I`", "Item") = Some(u8)
    fn find_direct_assoc_type_binding(
        &mut self,
        param_name: &str,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let bounds = self.trait_ctx.type_param_bounds.get(param_name)?.clone();
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
