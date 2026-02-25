//! AST Type to `TypeId` resolution.

use crate::ast::Type;
use crate::compiler_host::CompilerHost;
use crate::name::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Resolver;
use super::types::TypeError;
use crate::symbol::SymbolKind;

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_type(&mut self, ty: &Type) -> TypeId {
        match ty {
            Type::Named(named) => self.resolve_named_type(&named.name, named.span),
            Type::Generic(generic) => {
                self.resolve_generic_type(&generic.name, &generic.args, generic.span)
            }
            Type::Function(func_ty) => {
                let params: Vec<TypeId> = func_ty
                    .params
                    .iter()
                    .map(|p| self.resolve_type(p))
                    .collect();
                let return_type = self.resolve_type(&func_ty.return_type);
                self.type_table.borrow_mut().make_function(
                    params,
                    return_type,
                    func_ty.effects.clone(),
                )
            }
            Type::Tuple(elements) => {
                let elem_types: Vec<TypeId> =
                    elements.iter().map(|e| self.resolve_type(e)).collect();
                self.type_table.borrow_mut().make_tuple(elem_types)
            }
            Type::Reference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.type_table.borrow_mut().make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.type_table.borrow_mut().make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => self.resolve_namespaced_generic_type(namespaced),
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
            if let Some(&type_id) = self.current_associated_type_bindings.get(&namespaced.name) {
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
        if let Some(&(_, param_type_id)) = self.current_type_params.get(&namespaced.namespace) {
            return self
                .type_table
                .borrow_mut()
                .make_assoc_type_projection(param_type_id, namespaced.name.clone());
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
                self.type_table
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
            if let Some(self_type) = self.current_self_type {
                return self_type;
            }
            // Self used outside of impl block - return Unknown
            return TypeTable::UNKNOWN;
        }

        // First check if it's a type parameter in scope
        if let Some(&(_, type_id)) = self.current_type_params.get(name) {
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
            "()" => TypeTable::UNIT,
            "!" => TypeTable::NEVER,

            // Check newtypes, struct definitions, and variants
            _ => {
                if let Some(&type_id) = self.newtypes.get(name) {
                    type_id
                } else if let Some(struct_info) = self.struct_fields.get(name) {
                    // It's a struct - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_struct(name.to_string(), struct_info.module_source.clone())
                } else if let Some(variant_info) = self.variant_cases.get(name) {
                    // It's a variant - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_variant(name.to_string(), variant_info.module_source.clone())
                } else if let Some(enum_info) = self.enum_cases.get(name) {
                    // It's an enum - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_enum(name.to_string(), enum_info.module_source.clone())
                } else if let Some(resource_info) = self.resource_types.get(name) {
                    // It's a resource - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_resource(name.to_string(), resource_info.module_source.clone())
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
                self.type_table.borrow_mut().make_option(inner)
            }
            "Stream" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Stream(elem))
            }
            "StreamWritable" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::StreamWritable(elem))
            }
            "Future" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Future(elem))
            }
            "FutureWritable" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::FutureWritable(elem))
            }
            _ => {
                // Check if it's a user-defined generic struct
                if self.generic_struct_names.contains(name) {
                    // Resolve type arguments
                    let type_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();

                    // Get struct info for module source and bounds checking
                    let struct_info = self.struct_fields.get(name).cloned();
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
                                        let _ =
                                            self.logger.error(TypeError::TraitBoundNotSatisfied {
                                                type_name,
                                                trait_name: bound.clone(),
                                                param_name: param_name.clone(),
                                                span,
                                            });
                                    }
                                }
                            }
                        }
                    }

                    // Create a GenericInstance type
                    self.type_table.borrow_mut().make_generic_instance(
                        name.to_string(),
                        module_source,
                        type_args,
                    )
                } else if let Some(variant_info) = self.variant_cases.get(name).cloned() {
                    // Check if it's a generic variant (like Result<T, E>)
                    if variant_info.type_params.is_empty() {
                        TypeTable::UNKNOWN
                    } else {
                        let type_args: Vec<TypeId> =
                            args.iter().map(|t| self.resolve_type(t)).collect();
                        self.type_table.borrow_mut().make_generic_instance(
                            name.to_string(),
                            variant_info.module_source,
                            type_args,
                        )
                    }
                } else {
                    TypeTable::UNKNOWN
                }
            }
        }
    }
}
