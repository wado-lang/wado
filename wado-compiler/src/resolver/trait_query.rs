//! Trait query functions: checking trait implementations, bounds validation,
//! and associated type resolution.

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, Item, Type};
use crate::compiler_host::CompilerHost;
use crate::name::ModuleSource;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Resolver;
use super::types::{MethodInfo, TypeError};

impl<H: CompilerHost> Resolver<'_, H> {
    /// Find a trait declaration by name across all modules.
    /// Returns the trait's methods (cloned) if found.
    pub(super) fn find_trait_decl_methods(&self, trait_name: &str) -> Option<Vec<ast::Function>> {
        // Fast O(1) lookup via pre-built index instead of scanning all modules.
        if let Some((module_src, item_idx)) = self.trait_env.decl_index.get(trait_name) {
            let module = &self.loaded_modules[module_src];
            if let Item::Trait(trait_decl) = &module.items[*item_idx] {
                return Some(trait_decl.methods.clone());
            }
        }
        // Check current module items (not covered by the index).
        for item in self.current_module_items {
            if let Item::Trait(trait_decl) = item
                && trait_decl.name == trait_name
            {
                return Some(trait_decl.methods.clone());
            }
        }
        None
    }

    /// Check if a type implements a specific trait (for trait bound checking)
    pub(super) fn type_implements_trait(&self, type_id: TypeId, trait_name: &str) -> bool {
        let resolved = self.type_table.borrow().get(type_id).clone();

        // Recursion guard: if we're already checking this (type, trait) pair,
        // optimistically return true to break infinite recursion on recursive types.
        // This is sound because auto-derived Eq/Ord on recursive types is well-founded
        // (Wasm GC types are heap-allocated, so the comparison terminates on structural equality).
        // If any non-recursive field fails the trait check, it will be caught on that path.
        {
            let key = (type_id, trait_name.to_string());
            let stack = self.trait_check_stack.borrow();
            if stack.contains(&key) {
                return true;
            }
        }
        self.trait_check_stack
            .borrow_mut()
            .push((type_id, trait_name.to_string()));

        let result = self.type_implements_trait_inner(&resolved, trait_name);

        self.trait_check_stack.borrow_mut().pop();

        result
    }

    fn type_implements_trait_inner(&self, resolved: &ResolvedType, trait_name: &str) -> bool {
        // Type parameters satisfy bounds declared on them (e.g., T: Describable
        // means T implements Describable within the scope of that declaration)
        if let ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } = resolved
        {
            if let Some(bounds) = self.trait_ctx.type_param_bounds.get(name) {
                return bounds.iter().any(|b| b.name == trait_name);
            }
            return false;
        }

        // Primitives have built-in implementations for certain traits
        if let ResolvedType::Primitive(prim) = &resolved {
            match trait_name {
                // All primitives implement Eq and Ord
                "Eq" | "Ord" => return true,
                // Numeric primitives implement arithmetic traits
                "Add" | "Sub" | "Mul" | "Div" | "Rem"
                    if !matches!(prim, PrimitiveType::Bool | PrimitiveType::Char) =>
                {
                    return true;
                }
                _ => {}
            }
            // For other traits, check the type name
            let type_name = format!("{prim:?}").to_lowercase();
            return self.find_trait_impl_for_type(&type_name, trait_name);
        }

        // All enums automatically implement Eq and Ord
        if let ResolvedType::Enum { .. } = &resolved {
            match trait_name {
                "Eq" | "Ord" => return true,
                _ => {}
            }
        }

        // Variants auto-implement Eq when all payload types implement Eq
        if let ResolvedType::Variant { name, .. } = &resolved
            && trait_name == "Eq"
            && let Some(info) = self.variant_cases.get(name)
        {
            let all_impl = info.cases.iter().all(|c| {
                c.payload == TypeTable::UNIT || self.type_implements_trait(c.payload, trait_name)
            });
            if all_impl {
                return true;
            }
        }

        // Structs auto-implement Eq/Ord when all fields implement the trait
        if let ResolvedType::Struct { name, .. } = &resolved
            && matches!(trait_name, "Eq" | "Ord")
            && let Some(info) = self.struct_fields.get(name)
        {
            let field_types: Vec<TypeId> = info.fields.iter().map(|(_, tid, _)| *tid).collect();
            let all_impl = field_types
                .iter()
                .all(|tid| self.type_implements_trait(*tid, trait_name));
            if all_impl {
                return true;
            }
        }

        // Generic structs auto-implement Eq/Ord when all fields implement the trait
        // (with type params substituted by concrete type args)
        if let ResolvedType::GenericInstance {
            name, type_args, ..
        } = &resolved
            && matches!(trait_name, "Eq" | "Ord")
            && let Some(info) = self.struct_fields.get(name)
        {
            // Build type param -> concrete type arg mapping
            let param_map: IndexMap<TypeId, TypeId> = info
                .type_param_type_ids
                .iter()
                .zip(type_args.iter())
                .map(|(param, arg)| (*param, *arg))
                .collect();
            let all_impl = info.fields.iter().all(|(_, field_tid, _)| {
                let concrete_tid = param_map.get(field_tid).copied().unwrap_or(*field_tid);
                self.type_implements_trait(concrete_tid, trait_name)
            });
            if all_impl {
                return true;
            }
        }

        // Generic variants auto-implement Eq when all payload types implement Eq
        // (with type params substituted by concrete type args)
        if let ResolvedType::GenericInstance {
            name, type_args, ..
        } = &resolved
            && trait_name == "Eq"
            && let Some(info) = self.variant_cases.get(name)
        {
            let param_map: IndexMap<TypeId, TypeId> = info
                .type_param_type_ids
                .iter()
                .zip(type_args.iter())
                .map(|(param, arg)| (*param, *arg))
                .collect();
            let all_impl = info.cases.iter().all(|c| {
                if c.payload == TypeTable::UNIT {
                    return true;
                }
                let concrete_tid = param_map.get(&c.payload).copied().unwrap_or(c.payload);
                self.type_implements_trait(concrete_tid, trait_name)
            });
            if all_impl {
                return true;
            }
        }

        // Get the type name and type args for looking up implementations
        let (type_name, type_args) = match &resolved {
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. } => (name.clone(), None),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                if TypeTable::is_tuple_type(name, module_source) {
                    // Tuples implement a trait when all elements implement it
                    let elems = type_args.clone();
                    return elems
                        .iter()
                        .all(|e| self.type_implements_trait(*e, trait_name));
                }
                (
                    name.clone(),
                    if type_args.is_empty() {
                        None
                    } else {
                        Some(type_args.clone())
                    },
                )
            }
            ResolvedType::Ref(inner) => {
                // References always implement Eq via ref.eq (identity comparison)
                if trait_name == "Eq" {
                    return true;
                }
                // Check for a specific impl Trait for &T first (e.g., impl Inspect for &T)
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args("&", trait_name, Some(&[inner_id])) {
                    return true;
                }
                return self.type_implements_trait(inner_id, trait_name);
            }
            ResolvedType::MutRef(inner) => {
                // Mutable references always implement Eq via ref.eq (identity comparison)
                if trait_name == "Eq" {
                    return true;
                }
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args("&mut", trait_name, Some(&[inner_id])) {
                    return true;
                }
                return self.type_implements_trait(inner_id, trait_name);
            }
            ResolvedType::AssocTypeProjection { bounds, .. } => {
                // An associated type projection T::Assoc implements a trait if
                // the trait declaration for Assoc declares that bound.
                // e.g., I::Iter: Iterator when IntoIterator::Iter: Iterator
                return bounds.iter().any(|b| b == trait_name);
            }
            ResolvedType::Newtype {
                name, base_type, ..
            } => {
                // Check for a direct impl on the newtype first (e.g., impl Describe for Meters)
                if self.find_trait_impl_for_type(name, trait_name) {
                    return true;
                }
                // Fall back to base type's trait implementation
                let base_id = *base_type;
                return self.type_implements_trait(base_id, trait_name);
            }
            ResolvedType::Flags { name, .. } => {
                // Check for a direct impl on the flags type first (e.g., impl Serialize for Perms)
                if self.find_trait_impl_for_type(name, trait_name) {
                    return true;
                }
                // Fall back to u32's trait implementation
                return self.type_implements_trait(TypeTable::U32, trait_name);
            }
            _ => return false,
        };

        self.find_trait_impl_for_type_with_args(&type_name, trait_name, type_args.as_deref())
    }

    /// Helper to check if there's an impl block for a type implementing a trait
    pub(super) fn find_trait_impl_for_type(&self, type_name: &str, trait_name: &str) -> bool {
        self.find_trait_impl_for_type_with_args(type_name, trait_name, None)
    }

    /// Check if there's a trait impl for a type, with optional type args for bounds checking.
    /// For `impl<T: Eq> Eq for Array<T>`, when checking `Array<Foo>`, passes `[Foo]` as `type_args`.
    pub(super) fn find_trait_impl_for_type_with_args(
        &self,
        type_name: &str,
        trait_name: &str,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        // Use pre-built index for O(1) lookup by type name.
        if let Some(entries) = self.trait_env.impl_index.get(type_name) {
            for (module_src, item_idx) in entries {
                let module = &self.loaded_modules[module_src];
                if let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    let impl_trait_name = self.get_type_name(trait_type);
                    if impl_trait_name == trait_name
                        && self.check_impl_block_bounds(impl_block, type_args)
                    {
                        return true;
                    }
                }
            }
        }

        // Also check current module items (not covered by the index).
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
                && Self::get_type_name_static(&impl_block.ty) == type_name
            {
                let impl_trait_name = self.get_type_name(trait_type);
                if impl_trait_name == trait_name
                    && self.check_impl_block_bounds(impl_block, type_args)
                {
                    return true;
                }
            }
        }

        // Blanket impl fallback: check `impl<T: Bound> Trait for T` where the
        // concrete type satisfies the bound.
        for (module_src, item_idx) in &self.trait_env.blanket_impl_index {
            let module = &self.loaded_modules[module_src];
            if let Item::Impl(impl_block) = &module.items[*item_idx]
                && let Some(trait_type) = &impl_block.trait_type
            {
                let impl_trait_name = self.get_type_name(trait_type);
                if impl_trait_name == trait_name {
                    let impl_type_name = Self::get_type_name_static(&impl_block.ty);
                    let matching_param = impl_block
                        .type_params
                        .iter()
                        .find(|tp| tp.name == impl_type_name);
                    if let Some(param) = matching_param {
                        let bounds_satisfied = param
                            .bounds
                            .iter()
                            .all(|bound| self.find_trait_impl_for_type(type_name, &bound.name));
                        if bounds_satisfied {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    /// Compute `assoc_type_bindings` for an `AssocTypeProjection` by resolving `Self::X`
    /// references in the trait bound's associated type constraints.
    ///
    /// Example: `IntoIterator::Iter` has bound `Iterator<Item = Self::Item>`.
    /// With `I: IntoIterator<Item = u8>` and `self_type = I`, `Self::Item = I::Item = u8`.
    /// Result: `[("Item", u8_typeid)]`, stored in the `I::Iter` projection.
    ///
    /// This enables `I::Iter::Item` to resolve to `u8` when `Iterator::next` is called.
    fn compute_assoc_type_bindings_from_trait_bounds(
        &mut self,
        self_type_id: TypeId,
        self_type_param_name: Option<&str>,
        assoc_bounds: &[crate::ast::TraitBound],
    ) -> Vec<(String, TypeId)> {
        let mut bindings = Vec::new();
        let Some(type_param_name) = self_type_param_name else {
            // Also handle AssocTypeProjection self_type: propagate bindings from its bindings
            let resolved = self.type_table.borrow().get(self_type_id).clone();
            if let ResolvedType::AssocTypeProjection {
                assoc_type_bindings,
                ..
            } = resolved
            {
                // Reuse existing bindings from the source projection
                return assoc_type_bindings;
            }
            return bindings;
        };
        // For each bound, check its associated type constraints and resolve Self::X
        for bound in assoc_bounds {
            for assoc in &bound.assoc_types.clone() {
                if let crate::ast::Type::NamespacedGeneric(ns) = &assoc.ty
                    && ns.namespace == "Self"
                {
                    // Self::ns.name → type_param_name::ns.name
                    // Look in current_type_param_bounds[type_param_name] for direct binding
                    if let Some(param_bounds) = self
                        .trait_ctx
                        .type_param_bounds
                        .get(type_param_name)
                        .cloned()
                    {
                        for pb in &param_bounds {
                            for ab in &pb.assoc_types {
                                if ab.name == ns.name {
                                    let resolved_ty = self.resolve_type(&ab.ty.clone());
                                    bindings.push((assoc.name.clone(), resolved_ty));
                                }
                            }
                        }
                    }
                }
            }
        }
        bindings
    }

    /// Find a method in the trait declarations given by the bound names.
    /// For example, if T: Ord, look up the "cmp" method in the Ord trait declaration.
    /// Returns (`trait_name`, `MethodInfo`) with the method's return type, `self_kind`, and `param_types`,
    /// where Self is substituted with the `TypeParam`'s type.
    pub(super) fn find_method_in_trait_bounds(
        &mut self,
        bounds: &[String],
        method_name: &str,
        self_type_id: TypeId,
    ) -> Option<(String, MethodInfo)> {
        // Collect trait declarations from all modules
        for trait_name in bounds {
            // Search all loaded modules for the trait declaration
            let mut found_trait_method: Option<(
                ast::Function,
                Vec<ast::AssociatedTypeDecl>,
                ModuleSource,
            )> = None;

            for (module_src, module) in self.loaded_modules {
                for item in &module.items {
                    if let Item::Trait(trait_decl) = item
                        && trait_decl.name == *trait_name
                    {
                        for method in &trait_decl.methods {
                            if method.name == method_name {
                                found_trait_method = Some((
                                    method.clone(),
                                    trait_decl.associated_types.clone(),
                                    module_src.clone(),
                                ));
                                break;
                            }
                        }
                    }
                }
                if found_trait_method.is_some() {
                    break;
                }
            }

            // Also check current module items
            if found_trait_method.is_none() {
                for item in self.current_module_items {
                    if let Item::Trait(trait_decl) = item
                        && trait_decl.name == *trait_name
                    {
                        for method in &trait_decl.methods {
                            if method.name == method_name {
                                found_trait_method = Some((
                                    method.clone(),
                                    trait_decl.associated_types.clone(),
                                    self.current_module_source.clone(),
                                ));
                                break;
                            }
                        }
                    }
                }
            }

            if let Some((method, trait_assoc_types, _module_source)) = found_trait_method {
                // Save the entire trait context; we'll modify self_type, assoc_type_bindings,
                // type_params, and type_param_bounds during this resolution scope.
                let mut scope = self.enter_inherited_type_param_scope();
                scope.trait_ctx.self_type = Some(self_type_id);
                scope.trait_ctx.assoc_type_bindings.clear();

                // Set up associated type bindings as projections so that
                // Self::AssocType resolves to AssocTypeProjection(self_type_id, "AssocType").
                // We also compute assoc_type_bindings to propagate concrete types through
                // associated type chains.
                // Determine the TypeParam name for Self, if self_type is a TypeParam.
                let self_type_param_name = {
                    let resolved = scope.type_table.borrow().get(self_type_id).clone();
                    if let ResolvedType::TypeParam { name, .. } = resolved {
                        Some(name)
                    } else {
                        None
                    }
                };
                for assoc_decl in &trait_assoc_types {
                    // Check if self_type has a direct assoc_type_binding for this name.
                    // This handles the case: self_type = I::Iter which has ("Item", u8_typeid).
                    let directly_bound = {
                        let resolved = scope.type_table.borrow().get(self_type_id).clone();
                        if let ResolvedType::AssocTypeProjection {
                            assoc_type_bindings,
                            ..
                        } = resolved
                        {
                            assoc_type_bindings
                                .iter()
                                .find(|(name, _)| *name == assoc_decl.name)
                                .map(|(_, type_id)| *type_id)
                        } else {
                            None
                        }
                    };
                    let projection = directly_bound.unwrap_or_else(|| {
                        let bound_names: Vec<String> =
                            assoc_decl.bounds.iter().map(|b| b.name.clone()).collect();
                        // Compute assoc_type_bindings by resolving Self::X references in the
                        // assoc type's bounds. e.g., Iterator<Item = Self::Item> with Self = I
                        // and I: IntoIterator<Item = u8> gives [("Item", u8)].
                        let atb = scope.compute_assoc_type_bindings_from_trait_bounds(
                            self_type_id,
                            self_type_param_name.as_deref(),
                            &assoc_decl.bounds,
                        );
                        scope.type_table.borrow_mut().make_assoc_type_projection(
                            self_type_id,
                            assoc_decl.name.clone(),
                            bound_names,
                            atb,
                        )
                    });
                    scope
                        .trait_ctx
                        .assoc_type_bindings
                        .insert(assoc_decl.name.clone(), projection);
                }

                // Register the method's own type parameters so that they resolve to
                // TypeParam{index: N} instead of UNKNOWN. This is needed for generic methods
                // like `fn next_element<T: Deserialize>(&mut self) -> Result<Option<T>, ...>`
                // where `T` must be a proper TypeParam to allow substitution at the call site.
                // We use index 0, 1, ... because find_method_in_trait_bounds is only called
                // for TypeParam/AssocTypeProjection receivers, where impl_offset = 0.
                let mut method_type_param_ids: Vec<TypeId> = Vec::new();
                for (index, param) in method.type_params.iter().enumerate() {
                    let type_id = scope
                        .type_table
                        .borrow_mut()
                        .make_type_param(param.name.clone(), index as u32);
                    scope
                        .trait_ctx
                        .type_params
                        .insert(param.name.clone(), (index as u32, type_id));
                    if !param.bounds.is_empty() {
                        scope
                            .trait_ctx
                            .type_param_bounds
                            .insert(param.name.clone(), param.bounds.clone());
                    }
                    if !param.is_effect {
                        method_type_param_ids.push(type_id);
                    }
                }

                let return_type = method
                    .return_type
                    .as_ref()
                    .map(|t| scope.resolve_type(t))
                    .unwrap_or(TypeTable::UNIT);
                let self_kind = method
                    .params
                    .first()
                    .map(|p| p.self_kind)
                    .unwrap_or(ast::SelfKind::None);
                let param_types = scope.extract_param_types(&method.params);
                let param_is_mut: Vec<bool> = method
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.is_mut)
                    .collect();
                let param_defaults: Vec<Option<ast::Expr>> = method
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.default.clone())
                    .collect();

                drop(scope);

                return Some((
                    trait_name.clone(),
                    MethodInfo {
                        return_type,
                        self_kind,
                        param_types,
                        param_is_mut,
                        inherited_from_base: None,
                        cm_name: None,
                        is_ref_impl: false,
                        method_type_param_ids,
                        param_defaults,
                    },
                ));
            }
        }

        None
    }

    /// Check if an impl block's type parameter bounds are satisfied by the given type args.
    /// For `impl<T: Ord> Array<T>`, checks that the concrete type substituted for T implements Ord.
    pub(super) fn check_impl_block_bounds(
        &self,
        impl_block: &ast::ImplBlock,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        // No type params with bounds → always OK
        if impl_block.type_params.iter().all(|p| p.bounds.is_empty()) {
            return true;
        }

        let Some(type_args) = type_args else {
            // No type args to check (non-generic receiver) → skip bounds check
            return true;
        };

        // Build name → bounds map from impl block type params (trait names only)
        let bounds_map: IndexMap<&str, Vec<String>> = impl_block
            .type_params
            .iter()
            .filter(|p| !p.bounds.is_empty())
            .map(|p| {
                (
                    p.name.as_str(),
                    p.bounds.iter().map(|b| b.name.clone()).collect(),
                )
            })
            .collect();

        // Match type params to receiver type args via generic type arg positions
        let inner_type_name: Option<&str> =
            if let ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) = &impl_block.ty {
                if let ast::Type::Named(inner) = boxed.as_ref() {
                    Some(&inner.name)
                } else {
                    None
                }
            } else {
                None
            };

        if let ast::Type::Generic(generic) = &impl_block.ty {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg
                    && let Some(bounds) = bounds_map.get(named.name.as_str())
                    && let Some(&type_arg) = type_args.get(i)
                {
                    // If the type arg is itself a type parameter (e.g., T in a generic context),
                    // skip the bounds check. Within a bounded impl block, type params are assumed
                    // to satisfy bounds; concrete types are checked at call sites.
                    if matches!(
                        self.type_table.borrow().get(type_arg),
                        ResolvedType::TypeParam { .. }
                    ) {
                        continue;
                    }
                    for bound in bounds {
                        if !self.type_implements_trait(type_arg, bound) {
                            return false;
                        }
                    }
                }
            }
        } else if let Some(inner_name) = inner_type_name {
            // Handle `impl<T: Bound> Trait for &T` / `impl<T: Bound> Trait for &mut T`:
            // type_args[0] is the inner type T.
            if let Some(bounds) = bounds_map.get(inner_name)
                && let Some(&type_arg) = type_args.first()
                && !matches!(
                    self.type_table.borrow().get(type_arg),
                    ResolvedType::TypeParam { .. }
                )
            {
                for bound in bounds {
                    if !self.type_implements_trait(type_arg, bound) {
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Check trait bounds on a generic function's type arguments.
    /// Looks up the function's type params and validates bounds against the provided type args.
    pub(super) fn check_function_type_arg_bounds(
        &mut self,
        callee_module: &ModuleSource,
        func_name: &str,
        type_args: &[TypeId],
        span: Span,
    ) {
        // Look up function's type params from AST
        let type_params = self.lookup_function_type_params(callee_module, func_name);
        for (i, param) in type_params.iter().enumerate() {
            if let Some(&type_arg) = type_args.get(i) {
                for bound in &param.bounds {
                    if self.type_implements_trait(type_arg, &bound.name) {
                        // Register associated type resolutions so the monomorphizer can
                        // substitute e.g. I::Iter → ArrayIter<u8> when I = Array<u8>.
                        self.register_assoc_types_for_concrete_type_and_trait(
                            type_arg,
                            &bound.name.clone(),
                        );
                    } else {
                        let type_name = self.type_id_to_string(type_arg);
                        let _ = self.logger.error(TypeError::TraitBoundNotSatisfied {
                            type_name,
                            trait_name: bound.name.clone(),
                            param_name: param.name.clone(),
                            span,
                        });
                    }
                }
            }
        }
    }

    /// Register associated type resolutions for a concrete type instantiating a trait.
    /// For example, when `Array<u8>` implements `IntoIterator`, registers:
    /// - (Array<u8>, "Item") → u8
    /// - (Array<u8>, "Iter") → `ArrayIter`<u8>
    ///
    /// This enables the monomorphizer to resolve `I::Iter` → `ArrayIter<u8>` when `I = Array<u8>`.
    pub(super) fn register_assoc_types_for_concrete_type_and_trait(
        &mut self,
        concrete_type_id: TypeId,
        trait_name: &str,
    ) {
        // Get the base type name and concrete type args for impl block lookup.
        // For newtypes, follow the chain to the underlying type to find the trait impl,
        // but registration (below) still uses concrete_type_id so the monomorphizer can
        // resolve e.g. `MyBytes::Iter` when `MyBytes` is a newtype over `Array<u8>`.
        let (type_name, concrete_type_args) = {
            let tt = self.type_table.borrow();
            let effective_id = tt.get_ultimate_base_type(concrete_type_id);
            match tt.get(effective_id).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } => (name, type_args),
                ResolvedType::Struct { name, .. } => (name, vec![]),
                ResolvedType::BuiltinArray(elem) => ("Array".to_string(), vec![elem]),
                _ => return,
            }
        };

        // Collect matching impl block info (avoids borrow conflicts during resolution)
        struct ImplInfo {
            type_params: Vec<ast::GenericParam>,
            impl_ty_param_names: Vec<String>,
            assoc_types: Vec<ast::AssociatedTypeBinding>,
        }
        let impl_infos: Vec<ImplInfo> = {
            let mut result = vec![];
            if let Some(entries) = self.trait_env.impl_index.get(&type_name) {
                let entries = entries.clone();
                for (module_src, item_idx) in entries {
                    let module = &self.loaded_modules[&module_src];
                    if let Item::Impl(impl_block) = &module.items[item_idx]
                        && let Some(trait_type) = &impl_block.trait_type
                        && self.get_type_name(trait_type) == trait_name
                        && !impl_block.associated_types.is_empty()
                    {
                        let impl_ty_param_names: Vec<String> = match &impl_block.ty {
                            ast::Type::Generic(g) => g
                                .args
                                .iter()
                                .filter_map(|arg| {
                                    if let ast::Type::Named(named) = arg {
                                        Some(named.name.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                            _ => vec![],
                        };
                        result.push(ImplInfo {
                            type_params: impl_block.type_params.clone(),
                            impl_ty_param_names,
                            assoc_types: impl_block.associated_types.clone(),
                        });
                    }
                }
            }
            result
        };

        for info in impl_infos {
            let mut scope = self.enter_inherited_type_param_scope();

            // Bind impl type params to concrete type args.
            // For `impl<T> IntoIterator for Array<T>` with Array<u8>:
            // impl_ty_param_names = ["T"], concrete_type_args = [u8_typeid]
            // → set current_type_params["T"] = (0, u8_typeid)
            for (i, tp_name) in info.impl_ty_param_names.iter().enumerate() {
                if let Some(&concrete_arg) = concrete_type_args.get(i) {
                    scope
                        .trait_ctx
                        .type_params
                        .insert(tp_name.clone(), (i as u32, concrete_arg));
                }
            }
            // Add bounds from type param declarations
            for param in &info.type_params {
                if !param.bounds.is_empty() {
                    scope
                        .trait_ctx
                        .type_param_bounds
                        .entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.clone());
                }
            }

            // Resolve and register each associated type in this substituted context
            for binding in &info.assoc_types {
                let resolved_id = scope.resolve_type(&binding.ty);
                if !scope.type_table.borrow().contains_type_param(resolved_id) {
                    scope
                        .type_table
                        .borrow_mut()
                        .register_assoc_type_resolution(
                            concrete_type_id,
                            binding.name.clone(),
                            resolved_id,
                        );
                }
            }

            drop(scope);
        }

        // Also check blanket impls: `impl<I: Trait> OtherTrait for I`.
        // For example, `impl<I: Iterator> IntoIterator for I` applies to StrUtf8ByteIter.
        struct BlanketImplInfo {
            blanket_param_name: String,
            blanket_param_bounds: Vec<ast::TraitBound>,
            assoc_types: Vec<ast::AssociatedTypeBinding>,
        }
        let blanket_infos: Vec<BlanketImplInfo> = {
            let mut result = vec![];
            for (module_src, item_idx) in &self.trait_env.blanket_impl_index {
                let module = &self.loaded_modules[module_src];
                if let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_type) = &impl_block.trait_type
                    && self.get_type_name(trait_type) == trait_name
                    && !impl_block.associated_types.is_empty()
                {
                    let impl_type_name = Self::get_type_name_static(&impl_block.ty);
                    if let Some(blanket_param) = impl_block
                        .type_params
                        .iter()
                        .find(|tp| tp.name == impl_type_name && !tp.bounds.is_empty())
                    {
                        // Check if the concrete type satisfies the blanket param's bounds
                        let bounds_ok = blanket_param
                            .bounds
                            .iter()
                            .all(|bound| self.type_implements_trait(concrete_type_id, &bound.name));
                        if bounds_ok {
                            result.push(BlanketImplInfo {
                                blanket_param_name: blanket_param.name.clone(),
                                blanket_param_bounds: blanket_param.bounds.clone(),
                                assoc_types: impl_block.associated_types.clone(),
                            });
                        }
                    }
                }
            }
            result
        };

        for info in blanket_infos {
            let mut scope = self.enter_inherited_type_param_scope();

            // Bind the blanket type param to the concrete type
            // For `impl<I: Iterator> IntoIterator for I` with StrUtf8ByteIter:
            // → set current_type_params["I"] = (0, StrUtf8ByteIter_typeid)
            scope
                .trait_ctx
                .type_params
                .insert(info.blanket_param_name.clone(), (0, concrete_type_id));
            scope
                .trait_ctx
                .type_param_bounds
                .insert(info.blanket_param_name.clone(), info.blanket_param_bounds);

            // Resolve and register each associated type
            for binding in &info.assoc_types {
                let resolved_id = scope.resolve_type(&binding.ty);
                if !scope.type_table.borrow().contains_type_param(resolved_id) {
                    scope
                        .type_table
                        .borrow_mut()
                        .register_assoc_type_resolution(
                            concrete_type_id,
                            binding.name.clone(),
                            resolved_id,
                        );
                }
            }

            drop(scope);
        }
    }

    /// Build a mapping from type parameter names to concrete type IDs.
    /// For `impl Trait for Container<T>` with concrete type `Container<i32>`,
    /// returns `{"T" -> i32's TypeId}`.
    ///
    /// When `declared_type_params` is non-empty, only names in that set are
    /// treated as type parameters. This prevents concrete types (e.g., `String` in
    /// `impl Trait for Map<String, V>`) from being incorrectly mapped.
    /// When empty, all `Named` types are assumed to be type parameters (legacy behavior).
    pub(super) fn build_type_param_mapping(
        impl_ty: &Type,
        concrete_type_args: &[TypeId],
        declared_type_params: &IndexSet<String>,
    ) -> IndexMap<String, TypeId> {
        let mut mapping = IndexMap::default();

        // Extract type parameter names from impl_ty, tracking positions
        // Position tracking is needed to map type params to the correct concrete arg
        if let Type::Generic(g) = impl_ty {
            for (concrete_idx, arg) in g.args.iter().enumerate() {
                if let Type::Named(n) = arg {
                    let is_type_param = if declared_type_params.is_empty() {
                        true // legacy: treat all Named as type params
                    } else {
                        declared_type_params.contains(&n.name)
                    };
                    if is_type_param && let Some(&type_id) = concrete_type_args.get(concrete_idx) {
                        mapping.insert(n.name.clone(), type_id);
                    }
                }
            }
        }

        mapping
    }

    /// Check that concrete type args at non-type-parameter positions match the impl type.
    /// e.g., `impl KeyValueLiteral for TreeMap<String, V>` with `TreeMap<i32, String>` should fail
    /// because position 0 expects String but got i32.
    pub(super) fn verify_impl_type_compatibility(
        impl_ty: &Type,
        concrete_type_args: &[TypeId],
        declared_type_params: &IndexSet<String>,
        type_table: &std::cell::RefCell<TypeTable>,
    ) -> bool {
        if declared_type_params.is_empty() {
            return true; // No filtering available, assume compatible
        }
        let Type::Generic(g) = impl_ty else {
            return true;
        };
        let tt = type_table.borrow();
        for (i, arg) in g.args.iter().enumerate() {
            let Some(&concrete_id) = concrete_type_args.get(i) else {
                continue;
            };
            if !Self::impl_type_matches_concrete(arg, concrete_id, declared_type_params, &tt) {
                return false;
            }
        }
        true
    }

    /// Recursively check whether an impl type argument matches a concrete type ID.
    /// - `Type::Named` that is a declared type param → always matches (free type param)
    /// - `Type::Named` not in type params → concrete name must equal `type_table.type_name()`
    /// - `Type::Generic` → concrete must be a `GenericInstance` with same outer name; inner args checked recursively
    /// - Other types → not validated (return true)
    pub(super) fn impl_type_matches_concrete(
        impl_ty: &Type,
        concrete_id: TypeId,
        declared_type_params: &IndexSet<String>,
        type_table: &TypeTable,
    ) -> bool {
        match impl_ty {
            Type::Named(n) => {
                if declared_type_params.contains(&n.name) {
                    true // free type param — matches anything
                } else {
                    type_table.type_name(concrete_id) == n.name
                }
            }
            Type::Generic(g) => {
                let resolved = type_table.get(concrete_id).clone();
                match resolved {
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        if name != g.name {
                            return false;
                        }
                        for (i, inner) in g.args.iter().enumerate() {
                            let Some(&inner_id) = type_args.get(i) else {
                                return false;
                            };
                            if !Self::impl_type_matches_concrete(
                                inner,
                                inner_id,
                                declared_type_params,
                                type_table,
                            ) {
                                return false;
                            }
                        }
                        true
                    }
                    _ => false,
                }
            }
            _ => true,
        }
    }
}
