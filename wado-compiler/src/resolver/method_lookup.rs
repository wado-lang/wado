//! Method and trait lookup, trait implementation search, bounds checking.

use indexmap::{IndexMap, IndexSet};

use crate::ast::{self, BinaryOp, Expr, Function, Item, Literal, Type, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TirExpr, TirExprKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::types::{
    ArithmeticTraitInfo, FunctionContext, IndexAssignTraitInfo, IndexMutTraitInfo, IndexTraitInfo,
    IndexValueTraitInfo, KeyValueLiteralTraitInfo, MethodInfo, SequenceLiteralTraitInfo, TypeError,
};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn operator_trait_method(op: &BinaryOp) -> Option<(&'static str, &'static str)> {
        match op {
            BinaryOp::Add => Some(("Add", "add")),
            BinaryOp::Sub => Some(("Sub", "sub")),
            BinaryOp::Mul => Some(("Mul", "mul")),
            BinaryOp::Div => Some(("Div", "div")),
            BinaryOp::Mod => Some(("Rem", "rem")),
            BinaryOp::BitAnd => Some(("BitAnd", "bitand")),
            BinaryOp::BitOr => Some(("BitOr", "bitor")),
            BinaryOp::BitXor => Some(("BitXor", "bitxor")),
            BinaryOp::Shl => Some(("Shl", "shl")),
            BinaryOp::Shr => Some(("Shr", "shr")),
            BinaryOp::Eq | BinaryOp::NotEq => Some(("Eq", "eq")),
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => Some(("Ord", "cmp")),
            _ => None,
        }
    }

    /// Get the struct name from a type ID, if it's a struct or generic instance.
    pub(super) fn struct_name_for_type(&self, type_id: TypeId) -> Option<String> {
        match self.type_table.borrow().get(type_id) {
            ResolvedType::Struct { name, .. } | ResolvedType::GenericInstance { name, .. } => {
                Some(name.clone())
            }
            _ => None,
        }
    }

    /// Check if a name refers to a known type (struct, variant, enum, flags, newtype, or primitive).
    /// Used to distinguish concrete types from type parameters in impl blocks,
    /// since the parser treats all args in `<String, V>` as type params.
    pub(super) fn is_known_type_name(&self, name: &str) -> bool {
        self.struct_fields.contains_key(name)
            || self
                .all_struct_fields
                .values()
                .any(|m| m.contains_key(name))
            || self.variant_cases.contains_key(name)
            || self
                .all_variant_cases
                .values()
                .any(|m| m.contains_key(name))
            || self.enum_cases.contains_key(name)
            || self.all_enum_cases.values().any(|m| m.contains_key(name))
            || self.flags_cases.contains_key(name)
            || self.all_flags_cases.values().any(|m| m.contains_key(name))
            || self.newtypes.contains_key(name)
            || self.all_newtypes.values().any(|m| m.contains_key(name))
            || crate::tir::PrimitiveType::is_primitive_name(name)
    }

    /// Find the rhs parameter type for an operator trait on a struct type.
    /// Used to determine what type a literal rhs should be coerced to.
    pub(super) fn find_operator_rhs_type(
        &mut self,
        self_type_id: TypeId,
        op: &BinaryOp,
    ) -> Option<TypeId> {
        let struct_name = self.struct_name_for_type(self_type_id)?;
        let (trait_name, method_name) = Self::operator_trait_method(op)?;
        let trait_info =
            self.find_arithmetic_trait_impl(&struct_name, self_type_id, trait_name, method_name)?;
        // Unwrap the &T reference wrapper if present (e.g., rhs: &Self → return Self)
        trait_info.rhs_type.map(|t| {
            let resolved = self.type_table.borrow().get(t).clone();
            match resolved {
                ResolvedType::Ref(inner) => inner,
                _ => t,
            }
        })
    }

    /// Find the self type for an operator trait, given the rhs type.
    /// Used to determine what type a literal lhs should be coerced to.
    /// For most operators, the self type is the same struct type as rhs.
    pub(super) fn find_operator_self_type(
        &mut self,
        rhs_type_id: TypeId,
        op: &BinaryOp,
    ) -> Option<TypeId> {
        let struct_name = self.struct_name_for_type(rhs_type_id)?;
        let (trait_name, method_name) = Self::operator_trait_method(op)?;
        // Verify the trait impl exists; the self type is the struct type itself
        self.find_arithmetic_trait_impl(&struct_name, rhs_type_id, trait_name, method_name)?;
        Some(rhs_type_id)
    }

    /// Check if an expression is a numeric literal
    pub(super) fn is_numeric_literal(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Literal(lit) => matches!(lit.value, Literal::Number(_)),
            Expr::Unary(unary) if unary.op == UnaryOp::Neg => {
                matches!(&unary.expr, Expr::Literal(lit) if matches!(lit.value, Literal::Number(_)))
            }
            _ => false,
        }
    }

    /// Check if a qualified name `struct_name::method_name` is a static method
    pub(super) fn get_ultimate_base_struct_name(&self, type_id: TypeId) -> String {
        let mut current = type_id;
        loop {
            match self.type_table.borrow().get(current).clone() {
                ResolvedType::Struct { name, .. } => return name,
                ResolvedType::GenericInstance { name, .. } => return name,
                ResolvedType::Newtype { base_type, .. } => current = base_type,
                _ => return self.type_table.borrow().type_name(current),
            }
        }
    }

    /// Find the module source for a struct by name
    pub(super) fn find_struct_module_source(&self, struct_name: &str) -> ModuleSource {
        // Check if it's a primitive type - impl blocks live in core:prelude/primitives.wado
        // Note: i128/u128 are structs (in prelude/int128.wado), not primitives
        if matches!(
            struct_name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "f32"
                | "f64"
                | "bool"
                | "char"
        ) {
            return ModuleSource::primitives();
        }

        // Check current module
        for item in &self.current_module_items {
            match item {
                Item::Struct(s) if s.name == struct_name => {
                    return self.current_module_source.clone();
                }
                Item::Resource(r) if r.name == struct_name => {
                    return self.current_module_source.clone();
                }
                _ => {}
            }
        }

        // Check loaded modules
        for (module_source, module) in self.loaded_modules {
            for item in &module.items {
                match item {
                    Item::Struct(s) if s.name == struct_name => {
                        return module_source.clone();
                    }
                    Item::Resource(r) if r.name == struct_name => {
                        return module_source.clone();
                    }
                    _ => {}
                }
            }
        }

        // Default to current module source
        self.current_module_source.clone()
    }

    /// Look up method info based on receiver type and method name.
    /// Returns `MethodInfo` including return type and `self_kind`, or None if not found.
    pub(super) fn lookup_method_info(
        &mut self,
        receiver_type: TypeId,
        method_name: &str,
    ) -> Option<MethodInfo> {
        // First, get the base (non-reference) type for method lookup
        let base_type_id = self.get_base_type(receiver_type);
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        // Get the struct name, module source, and type args from the base type
        // For primitives, module_source is None to trigger "search all loaded modules" logic
        let (struct_name, struct_module_source, receiver_type_args, newtype_base) = match &base_type
        {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), Some(module_source.clone()), None, None),
            // Resource types use reference semantics - handle like struct for method lookup
            ResolvedType::Resource {
                name,
                module_source,
            } => (name.clone(), Some(module_source.clone()), None, None),
            // Generic instances like Box<i32> use the base name "Box" for method lookup
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (
                name.clone(),
                Some(module_source.clone()),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
                None,
            ),
            // Newtype: first try looking up methods on the newtype itself,
            // then fall back to the base type for method inheritance
            ResolvedType::Newtype {
                name,
                module_source,
                base_type,
            } => (
                name.clone(),
                Some(module_source.clone()),
                None,
                Some(*base_type),
            ),
            // Primitive types - search for impl blocks in loaded modules
            // (e.g., impl i32 { fn to_string(&self) -> String { ... } })
            ResolvedType::Primitive(prim) => {
                // Use None to trigger "search all loaded modules" logic
                (prim.as_str().to_string(), None, None, None)
            }
            // Enum types - search for impl blocks by enum name
            ResolvedType::Enum {
                name,
                module_source,
            } => (name.clone(), Some(module_source.clone()), None, None),
            // Generic resource types (Future<T>, Stream<T>, etc.)
            ResolvedType::GenericResource {
                name,
                module_source,
                type_args,
            } => (
                name.clone(),
                Some(module_source.clone()),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
                None,
            ),
            _ => return None,
        };

        let mangled_name = MethodName::format_local(&struct_name, None, method_name);
        if let Some(&return_type) = self.function_return_types.get(&mangled_name) {
            // For locally registered methods, find self_kind and param_types from the AST
            // Also checks that bounded impl block constraints are satisfied
            if let Some((self_kind, param_types, param_is_mut)) = self.find_local_method_info(
                &struct_name,
                method_name,
                receiver_type_args.as_deref(),
            ) {
                return Some(MethodInfo {
                    return_type,
                    self_kind,
                    param_types,
                    param_is_mut,
                    inherited_from_base: None,
                    canonical_name: None,
                });
            }
            // If find_local_method_info returned None, the method either doesn't exist
            // or its impl block bounds are not satisfied. Don't fall back - continue
            // searching loaded modules and trait methods.
        }

        // Try looking up in loaded modules (for imported structs)
        // Only check inherent impls (not trait impls) - trait impls are handled separately
        if let Some(ref module_source) = struct_module_source {
            // Pre-populate module type maps cache before borrowing loaded_modules
            self.ensure_module_maps_cached(module_source);
            if let Some(module) = self.loaded_modules.get(module_source) {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item {
                        // Skip trait impls - only look at inherent impls
                        if impl_block.trait_type.is_some() {
                            continue;
                        }
                        let impl_struct_name = self.get_type_name(&impl_block.ty);
                        if impl_struct_name == struct_name
                            && self
                                .check_impl_block_bounds(impl_block, receiver_type_args.as_deref())
                        {
                            for method in &impl_block.methods {
                                if method.name == method_name {
                                    // Set up type params for generic impls (e.g., impl Array<T>)
                                    let old_type_params =
                                        std::mem::take(&mut self.current_type_params);
                                    let mut impl_offset = 0u32;
                                    if let Some(ref type_args) = receiver_type_args
                                        && let Type::Generic(generic) = &impl_block.ty
                                    {
                                        impl_offset = type_args.len() as u32;
                                        for (i, arg) in generic.args.iter().enumerate() {
                                            if let Type::Named(named) = arg
                                                && i < type_args.len()
                                            {
                                                self.current_type_params.insert(
                                                    named.name.clone(),
                                                    (i as u32, type_args[i]),
                                                );
                                            }
                                        }
                                    }

                                    // Set up method-level type params (e.g., Acc in fold<Acc>)
                                    // These get TypeParam types that will be substituted at call sites
                                    for (i, type_param) in method.type_params.iter().enumerate() {
                                        let index = impl_offset + i as u32;
                                        let type_param_id = self.type_table.borrow_mut().intern(
                                            ResolvedType::TypeParam {
                                                name: type_param.name.clone(),
                                                index,
                                            },
                                        );
                                        self.current_type_params.insert(
                                            type_param.name.clone(),
                                            (index, type_param_id),
                                        );
                                    }

                                    // Resolve return type and param types in the source module's
                                    // type context, not the caller's. This prevents same-named types
                                    // from different modules being confused (e.g., both modules
                                    // define "Config" with different fields).
                                    // Use cached module type maps (O(1) swap) instead of
                                    // rebuilding maps from scratch on every call.
                                    let mut cached = self
                                        .module_type_maps_cache
                                        .shift_remove(module_source)
                                        .expect("cache populated by ensure_module_maps_cached");
                                    std::mem::swap(
                                        &mut self.struct_fields,
                                        &mut cached.struct_fields,
                                    );
                                    std::mem::swap(
                                        &mut self.variant_cases,
                                        &mut cached.variant_cases,
                                    );
                                    std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
                                    std::mem::swap(&mut self.flags_cases, &mut cached.flags_cases);
                                    std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
                                    std::mem::swap(
                                        &mut self.resource_types,
                                        &mut cached.resource_types,
                                    );

                                    let return_type = method
                                        .return_type
                                        .as_ref()
                                        .map(|t| self.resolve_type(t))
                                        .unwrap_or(TypeTable::UNIT);
                                    let self_kind = method
                                        .params
                                        .first()
                                        .map(|p| p.self_kind)
                                        .unwrap_or(ast::SelfKind::None);
                                    let param_types = self.extract_param_types(&method.params);
                                    let param_is_mut: Vec<bool> = method
                                        .params
                                        .iter()
                                        .filter(|p| p.name != "self")
                                        .map(|p| p.is_mut)
                                        .collect();

                                    std::mem::swap(
                                        &mut self.struct_fields,
                                        &mut cached.struct_fields,
                                    );
                                    std::mem::swap(
                                        &mut self.variant_cases,
                                        &mut cached.variant_cases,
                                    );
                                    std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
                                    std::mem::swap(&mut self.flags_cases, &mut cached.flags_cases);
                                    std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
                                    std::mem::swap(
                                        &mut self.resource_types,
                                        &mut cached.resource_types,
                                    );
                                    self.module_type_maps_cache
                                        .insert(module_source.clone(), cached);
                                    self.current_type_params = old_type_params;

                                    return Some(MethodInfo {
                                        return_type,
                                        self_kind,
                                        param_types,
                                        param_is_mut,
                                        inherited_from_base: None,
                                        canonical_name: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Search all loaded modules if no specific module (for prelude types)
        // Only check inherent impls (not trait impls) - trait impls are handled separately
        if struct_module_source.is_none() {
            for module in self.loaded_modules.values() {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item {
                        // Skip trait impls - only look at inherent impls
                        if impl_block.trait_type.is_some() {
                            continue;
                        }
                        let impl_struct_name = self.get_type_name(&impl_block.ty);
                        if impl_struct_name == struct_name
                            && self
                                .check_impl_block_bounds(impl_block, receiver_type_args.as_deref())
                        {
                            for method in &impl_block.methods {
                                if method.name == method_name {
                                    // Set up type params for generic impls (e.g., impl Array<T>)
                                    let old_type_params =
                                        std::mem::take(&mut self.current_type_params);
                                    let mut impl_offset = 0u32;
                                    if let Some(ref type_args) = receiver_type_args
                                        && let Type::Generic(generic) = &impl_block.ty
                                    {
                                        impl_offset = type_args.len() as u32;
                                        for (i, arg) in generic.args.iter().enumerate() {
                                            if let Type::Named(named) = arg
                                                && i < type_args.len()
                                            {
                                                self.current_type_params.insert(
                                                    named.name.clone(),
                                                    (i as u32, type_args[i]),
                                                );
                                            }
                                        }
                                    }

                                    // Set up method-level type params (e.g., Acc in fold<Acc>)
                                    // These get TypeParam types that will be substituted at call sites
                                    for (i, type_param) in method.type_params.iter().enumerate() {
                                        let index = impl_offset + i as u32;
                                        let type_param_id = self.type_table.borrow_mut().intern(
                                            ResolvedType::TypeParam {
                                                name: type_param.name.clone(),
                                                index,
                                            },
                                        );
                                        self.current_type_params.insert(
                                            type_param.name.clone(),
                                            (index, type_param_id),
                                        );
                                    }

                                    let return_type = method
                                        .return_type
                                        .as_ref()
                                        .map(|t| self.resolve_type(t))
                                        .unwrap_or(TypeTable::UNIT);
                                    let self_kind = method
                                        .params
                                        .first()
                                        .map(|p| p.self_kind)
                                        .unwrap_or(ast::SelfKind::None);
                                    let param_types = self.extract_param_types(&method.params);
                                    let param_is_mut: Vec<bool> = method
                                        .params
                                        .iter()
                                        .filter(|p| p.name != "self")
                                        .map(|p| p.is_mut)
                                        .collect();

                                    self.current_type_params = old_type_params;

                                    return Some(MethodInfo {
                                        return_type,
                                        self_kind,
                                        param_types,
                                        param_is_mut,
                                        inherited_from_base: None,
                                        canonical_name: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Search resource declarations in loaded modules for instance methods
        // Resource methods have &self or &mut self parameter (first param is reference to resource type)
        if let Some(ref module_source) = struct_module_source
            && let Some(module) = self.loaded_modules.get(module_source)
        {
            for item in &module.items {
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                    && let Some(info) = self.find_resource_method_info(
                        resource,
                        method_name,
                        receiver_type_args.as_deref(),
                    )
                {
                    return Some(info);
                }
            }
        }

        // Also search all modules for resources if no specific module
        if struct_module_source.is_none() {
            for module in self.loaded_modules.values() {
                for item in &module.items {
                    if let Item::Resource(resource) = item
                        && resource.name == struct_name
                        && let Some(info) = self.find_resource_method_info(
                            resource,
                            method_name,
                            receiver_type_args.as_deref(),
                        )
                    {
                        return Some(info);
                    }
                }
            }
        }

        // For newtypes: if method not found on the newtype itself, try the base type
        // This enables method inheritance: Location (newtype of Point) can use Point's methods
        if let Some(base_type_id) = newtype_base {
            if let Some(mut method_info) = self.lookup_method_info(base_type_id, method_name) {
                // Mark that this method was inherited from the base type
                // This enables proper type checking (e.g., Point::add expects &Point,
                // but when called on Location, it should expect &Location)
                // Only set if not already set (for chained newtypes like C -> B -> A -> Point,
                // we want to keep the innermost base type where the method is defined)
                if method_info.inherited_from_base.is_none() {
                    method_info.inherited_from_base = Some(base_type_id);
                }
                return Some(method_info);
            }
            return None;
        }

        None
    }

    /// Find a method in a resource declaration, with proper type parameter setup.
    fn find_resource_method_info(
        &mut self,
        resource: &ast::ResourceDecl,
        method_name: &str,
        receiver_type_args: Option<&[TypeId]>,
    ) -> Option<MethodInfo> {
        for method in &resource.methods {
            if method.name != method_name {
                continue;
            }
            let has_self = method.params.iter().any(|p| {
                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name))
                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name)
            });
            if !has_self {
                continue;
            }

            // Set up type params for generic resources (e.g., resource Stream<T>)
            let old_type_params = std::mem::take(&mut self.current_type_params);
            if let Some(type_args) = receiver_type_args {
                for (i, param) in resource.type_params.iter().enumerate() {
                    if i < type_args.len() {
                        self.current_type_params
                            .insert(param.name.clone(), (i as u32, type_args[i]));
                    }
                }
            }

            let return_type = method
                .return_type
                .as_ref()
                .map(|t| self.resolve_type(t))
                .unwrap_or(TypeTable::UNIT);
            let param_types = self.extract_param_types(&method.params);
            let param_is_mut: Vec<bool> = method
                .params
                .iter()
                .filter(|p| p.name != "self")
                .map(|p| p.is_mut)
                .collect();

            self.current_type_params = old_type_params;

            // Extract canonical builtin name from #[canonical("...")] attribute
            let canonical_name = method
                .attrs
                .iter()
                .find(|a| a.name == "canonical")
                .and_then(|a| a.args.first().cloned());

            return Some(MethodInfo {
                return_type,
                self_kind: ast::SelfKind::Ref,
                param_types,
                param_is_mut,
                inherited_from_base: None,
                canonical_name,
            });
        }
        None
    }

    /// Find the method info (`self_kind` and `param_types`) for a method in current module items
    pub(super) fn find_local_method_info(
        &mut self,
        struct_name: &str,
        method_name: &str,
        receiver_type_args: Option<&[TypeId]>,
    ) -> Option<(ast::SelfKind, Vec<TypeId>, Vec<bool>)> {
        // First collect method info without resolving types
        let mut found_method: Option<(ast::SelfKind, Vec<ast::Type>, Vec<bool>)> = None;

        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item {
                // Skip trait impls
                if impl_block.trait_type.is_some() {
                    continue;
                }
                let impl_struct_name = self.get_type_name(&impl_block.ty);
                if impl_struct_name == struct_name
                    && self.check_impl_block_bounds(impl_block, receiver_type_args)
                {
                    for method in &impl_block.methods {
                        if method.name == method_name {
                            let self_kind = method
                                .params
                                .first()
                                .map(|p| p.self_kind)
                                .unwrap_or(ast::SelfKind::None);
                            // Extract non-self parameter types and mut flags
                            let non_self: Vec<&ast::Param> =
                                method.params.iter().filter(|p| p.name != "self").collect();
                            let param_types: Vec<ast::Type> =
                                non_self.iter().map(|p| p.ty.clone()).collect();
                            let param_is_mut: Vec<bool> =
                                non_self.iter().map(|p| p.is_mut).collect();
                            found_method = Some((self_kind, param_types, param_is_mut));
                            break;
                        }
                    }
                }
            }
            if found_method.is_some() {
                break;
            }
        }

        // Now resolve the types (needs mutable borrow)
        found_method.map(|(self_kind, param_types_ast, param_is_mut)| {
            let param_types: Vec<TypeId> = param_types_ast
                .iter()
                .map(|ty| self.resolve_type(ty))
                .collect();
            (self_kind, param_types, param_is_mut)
        })
    }

    /// Extract parameter types (excluding self) from method parameters
    pub(super) fn extract_param_types(&mut self, params: &[ast::Param]) -> Vec<TypeId> {
        params
            .iter()
            .filter(|p| p.name != "self")
            .map(|p| self.resolve_type(&p.ty))
            .collect()
    }

    /// Substitute a base type with a newtype in a type (handles references)
    /// For example: if `base_type` is Point and newtype is Location:
    ///   - Point -> Location
    ///   - &Point -> &Location
    ///   - &mut Point -> &mut Location
    pub(super) fn substitute_newtype_in_type(
        &mut self,
        type_id: TypeId,
        base_type: TypeId,
        newtype: TypeId,
    ) -> TypeId {
        let ty = self.type_table.borrow().get(type_id).clone();
        match ty {
            // Direct match: base type -> newtype
            _ if type_id == base_type => newtype,

            // Reference: substitute the inner type
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_newtype_in_type(inner, base_type, newtype);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(new_inner))
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_newtype_in_type(inner, base_type, newtype);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::MutRef(new_inner))
                }
            }

            // Other types: no substitution
            _ => type_id,
        }
    }

    /// Check if actual argument type matches expected parameter type (newtype-aware)
    /// Returns true if there's a mismatch involving newtypes
    pub(super) fn check_newtype_arg_mismatch(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> Option<(String, String)> {
        if actual == expected {
            return None;
        }

        let type_table = self.type_table.borrow();

        // Unwrap references to get the inner types
        let actual_inner = match type_table.get(actual) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => actual,
        };
        let expected_inner = match type_table.get(expected) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => expected,
        };

        // Check if either inner type is a newtype
        let actual_is_newtype =
            matches!(type_table.get(actual_inner), ResolvedType::Newtype { .. });
        let expected_is_newtype =
            matches!(type_table.get(expected_inner), ResolvedType::Newtype { .. });

        // If either is a newtype and they're different, that's a mismatch
        if (actual_is_newtype || expected_is_newtype) && actual_inner != expected_inner {
            let actual_name = type_table.type_name(actual);
            let expected_name = type_table.type_name(expected);
            return Some((expected_name, actual_name));
        }

        // Also check if one is the base type of the other
        if let ResolvedType::Newtype { base_type, .. } = type_table.get(actual_inner)
            && *base_type == expected_inner
        {
            let actual_name = type_table.type_name(actual);
            let expected_name = type_table.type_name(expected);
            return Some((expected_name, actual_name));
        }
        if let ResolvedType::Newtype { base_type, .. } = type_table.get(expected_inner)
            && *base_type == actual_inner
        {
            let actual_name = type_table.type_name(actual);
            let expected_name = type_table.type_name(expected);
            return Some((expected_name, actual_name));
        }

        None
    }

    /// Infer method type arguments from actual argument types.
    /// Returns a list of inferred type args matching the method's type params order.
    /// Uses the position of type params in parameter types to map actual arg types.
    pub(super) fn infer_method_type_args(
        &self,
        receiver_type: TypeId,
        method_name: &str,
        args: &[TirExpr],
        impl_offset: u32,
    ) -> Vec<TypeId> {
        let base_type_id = self.get_base_type(receiver_type);
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        let (struct_name, struct_module_source) = match &base_type {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), Some(module_source.clone())),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name.clone(), Some(module_source.clone())),
            _ => return vec![],
        };

        // Search for the method in loaded modules
        let mut method_type_params: Vec<String> = Vec::new();
        let mut param_type_strs: Vec<String> = Vec::new();

        // Helper function to extract param info from method
        let extract_method_info = |method: &crate::ast::Function| -> (Vec<String>, Vec<String>) {
            let type_params: Vec<String> =
                method.type_params.iter().map(|p| p.name.clone()).collect();
            let params: Vec<String> = method
                .params
                .iter()
                // Skip self parameter (has SelfKind::Ref/MutRef and name "self")
                .filter(|p| {
                    !(matches!(
                        p.self_kind,
                        ast::SelfKind::Ref | ast::SelfKind::MutRef | ast::SelfKind::None
                    ) && p.name == "self")
                })
                .map(|p| self.get_type_name(&p.ty))
                .collect();
            (type_params, params)
        };

        // Check specific module first
        if let Some(ref module_source) = struct_module_source
            && let Some(module) = self.loaded_modules.get(module_source)
        {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && impl_block.trait_type.is_none()
                {
                    let impl_type_name = self.get_type_name(&impl_block.ty);
                    // Match impl type name: either exact match or the base name matches
                    // For generic types like ArrayIter<T>, match if base name "ArrayIter" matches
                    let impl_base_name =
                        impl_type_name.split('<').next().unwrap_or(&impl_type_name);
                    if impl_type_name == struct_name || impl_base_name == struct_name {
                        for method in &impl_block.methods {
                            if method.name == method_name && !method.type_params.is_empty() {
                                let (tp, pp) = extract_method_info(method);
                                method_type_params = tp;
                                param_type_strs = pp;
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Search all loaded modules if not found
        if method_type_params.is_empty() {
            for module in self.loaded_modules.values() {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item
                        && impl_block.trait_type.is_none()
                    {
                        let impl_type_name = self.get_type_name(&impl_block.ty);
                        let impl_base_name =
                            impl_type_name.split('<').next().unwrap_or(&impl_type_name);
                        if impl_type_name == struct_name || impl_base_name == struct_name {
                            for method in &impl_block.methods {
                                if method.name == method_name && !method.type_params.is_empty() {
                                    let (tp, pp) = extract_method_info(method);
                                    method_type_params = tp;
                                    param_type_strs = pp;
                                    break;
                                }
                            }
                        }
                    }
                }
                if !method_type_params.is_empty() {
                    break;
                }
            }
        }

        if method_type_params.is_empty() {
            return vec![];
        }

        // Infer type args by matching type param names against param types and actual arg types
        let mut inferred: Vec<TypeId> = vec![TypeTable::UNKNOWN; method_type_params.len()];

        for (i, type_param_name) in method_type_params.iter().enumerate() {
            // Find the first parameter whose type matches this type param
            for (param_idx, param_type_str) in param_type_strs.iter().enumerate() {
                if param_idx >= args.len() {
                    continue;
                }

                if param_type_str == type_param_name {
                    // This param has type T (or Acc, etc.) - use the actual arg type
                    inferred[i] = args[param_idx].type_id;
                    break;
                }

                // Check if the type param appears in a function type's return position
                // e.g., for "fn(T) -> U" we can infer U from the closure's return type
                if param_type_str.starts_with("fn(") {
                    // Parse function type to extract return type
                    // Format: "fn(param1, param2, ...) -> ReturnType"
                    if let Some(arrow_pos) = param_type_str.find(" -> ") {
                        let return_type_str = &param_type_str[arrow_pos + 4..];
                        if return_type_str == type_param_name {
                            // The return type is our type param - infer from closure's return type
                            let arg_type = self
                                .type_table
                                .borrow()
                                .get(args[param_idx].type_id)
                                .clone();
                            if let ResolvedType::Function { return_type, .. } = arg_type {
                                inferred[i] = return_type;
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Return only if we found at least some type args
        if inferred.iter().all(|&t| t == TypeTable::UNKNOWN) {
            vec![]
        } else {
            // Use impl_offset to verify - type params start after impl params
            let _ = impl_offset;
            inferred
        }
    }

    /// Get the base (non-reference) type by stripping all Ref/MutRef wrappers
    pub(super) fn get_base_type(&self, type_id: TypeId) -> TypeId {
        let mut current = type_id;
        loop {
            match self.type_table.borrow().get(current).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    current = inner;
                }
                _ => return current,
            }
        }
    }
    /// Adjust the receiver expression to match what the method's self parameter expects
    pub(super) fn adjust_receiver_for_self_kind(
        &mut self,
        receiver: TirExpr,
        self_kind: ast::SelfKind,
        span: Span,
    ) -> TirExpr {
        let receiver_type = self.type_table.borrow().get(receiver.type_id).clone();

        match self_kind {
            ast::SelfKind::None => {
                // No self parameter (static method context), deref all refs
                self.deref_to_value(receiver, span)
            }
            ast::SelfKind::Ref => {
                // Method expects &self
                match &receiver_type {
                    ResolvedType::Ref(_) => {
                        // Already &T, use as-is
                        receiver
                    }
                    ResolvedType::MutRef(_) => {
                        // &mut T can be coerced to &T, use as-is
                        receiver
                    }
                    _ => {
                        // Value T, need to add &
                        let ref_type = self.type_table.borrow_mut().make_ref(receiver.type_id);
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(receiver),
                            },
                            ref_type,
                            span,
                        )
                    }
                }
            }
            ast::SelfKind::MutRef => {
                // Method expects &mut self
                if let ResolvedType::MutRef(_) = &receiver_type {
                    // Already &mut T, use as-is
                    receiver
                } else {
                    // Value T, need to add &mut
                    let mut_ref_type = self.type_table.borrow_mut().make_mut_ref(receiver.type_id);
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(receiver),
                        },
                        mut_ref_type,
                        span,
                    )
                }
            }
        }
    }

    /// Dereference a receiver until it's a value (non-reference) type
    pub(super) fn deref_to_value(&self, mut receiver: TirExpr, span: Span) -> TirExpr {
        loop {
            match self.type_table.borrow().get(receiver.type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    receiver = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(receiver),
                        },
                        inner,
                        span,
                    );
                }
                _ => return receiver,
            }
        }
    }

    /// Find a trait method for a given type and method name.
    /// Returns (`trait_name`, `MethodInfo`, `ModuleSource`) if found, None otherwise.
    /// This is used when an inherent method is not found.
    ///
    /// `receiver_type_args` should contain the concrete type arguments for generic receivers
    /// (e.g., `[i32]` for `Box_<i32>`). This is used to substitute type parameters when
    /// resolving associated types like `type Item = T`.
    pub(super) fn find_trait_method_for_type(
        &mut self,
        struct_name: &str,
        method_name: &str,
        _struct_module: &ModuleSource,
        receiver_type_args: Option<&[TypeId]>,
        receiver_type_id: Option<TypeId>,
    ) -> Option<super::types::TraitMethodMatch> {
        use super::types::TraitMethodMatch;
        let mut found_traits: Vec<TraitMethodMatch> = Vec::new();

        // Build names_to_check first (struct name + newtype chain), then use the
        // pre-built index to fetch only the matching impl blocks instead of scanning
        // all items in all modules.
        let names_to_check: Vec<String> = {
            let mut names = vec![struct_name.to_string()];
            if let Some(&newtype_id) = self.newtypes.get(struct_name) {
                let mut current = newtype_id;
                while let ResolvedType::Newtype { base_type, .. } =
                    self.type_table.borrow().get(current).clone()
                {
                    let base_name = self.type_table.borrow().type_name(base_type);
                    names.push(base_name);
                    current = base_type;
                }
            }
            names
        };

        // Collect impl blocks to check (avoiding borrow issues with &mut self).
        // Use the pre-built index to look up only impl blocks for the matching type names —
        // O(1) per name instead of scanning every item in every loaded module.
        let mut impl_blocks_to_check: Vec<(
            Type,
            Type,
            Vec<Function>,
            Vec<crate::ast::AssociatedTypeBinding>,
            ModuleSource,
        )> = Vec::new();

        for name in &names_to_check {
            if let Some(entries) = self.trait_impl_index.get(name.as_str()) {
                for (module_src, item_idx) in entries {
                    let module = &self.loaded_modules[module_src];
                    if let Item::Impl(impl_block) = &module.items[*item_idx]
                        && let Some(trait_type) = &impl_block.trait_type
                    {
                        impl_blocks_to_check.push((
                            impl_block.ty.clone(),
                            trait_type.clone(),
                            impl_block.methods.clone(),
                            impl_block.associated_types.clone(),
                            module_src.clone(),
                        ));
                    }
                }
            }
        }

        // Also check current module items (not covered by the index, which only indexes
        // loaded_modules captured before per-module resolution began).
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
            {
                let impl_struct_name = Self::get_type_name_static(&impl_block.ty);
                if names_to_check.contains(&impl_struct_name) {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
                        self.current_module_source.clone(),
                    ));
                }
            }
        }

        // Blanket impl fallback: check `impl<T: Bound> Trait for T` where the receiver
        // type satisfies the bound.  e.g., `impl<I: Iterator> IntoIterator for I` matches
        // any concrete type that implements Iterator.
        for (module_src, item_idx) in self.blanket_trait_impl_index.as_ref() {
            let module = &self.loaded_modules[module_src];
            if let Item::Impl(impl_block) = &module.items[*item_idx]
                && let Some(trait_type) = &impl_block.trait_type
            {
                // Find the type param that is the impl target
                let impl_type_name = Self::get_type_name_static(&impl_block.ty);
                let matching_param = impl_block
                    .type_params
                    .iter()
                    .find(|tp| tp.name == impl_type_name);
                if let Some(param) = matching_param {
                    // Check if the receiver type satisfies ALL trait bounds
                    let bounds_satisfied = param.bounds.iter().all(|bound| {
                        let bound_trait_name = &bound.name;
                        names_to_check
                            .iter()
                            .any(|name| self.find_trait_impl_for_type(name, bound_trait_name))
                    });
                    if bounds_satisfied {
                        impl_blocks_to_check.push((
                            impl_block.ty.clone(),
                            trait_type.clone(),
                            impl_block.methods.clone(),
                            impl_block.associated_types.clone(),
                            module_src.clone(),
                        ));
                    }
                }
            }
        }

        // Now process the collected impl blocks with mutable access
        for (impl_ty, trait_type, methods, associated_types, impl_module_source) in
            impl_blocks_to_check
        {
            let impl_struct_name = self.get_type_name(&impl_ty);
            // Accept if the type matches by name, or if it's a blanket impl type parameter
            // (blanket impls already had their bounds checked before being added).
            let is_blanket_type_param =
                matches!(&impl_ty, Type::Named(named) if !self.is_known_type_name(&named.name));
            if names_to_check.contains(&impl_struct_name) || is_blanket_type_param {
                // Set up type parameters for resolving generic associated types
                // e.g., for `impl Container for Box_<T>` called on `Box_<i32>`,
                // we need to map T -> i32 so `type Item = T` resolves to i32
                let old_type_params = std::mem::take(&mut self.current_type_params);
                if let Some(type_args) = receiver_type_args
                    && let Type::Generic(generic) = &impl_ty
                {
                    for (i, arg) in generic.args.iter().enumerate() {
                        if let Type::Named(named) = arg
                            && i < type_args.len()
                        {
                            // Map type param name to concrete type from receiver
                            self.current_type_params
                                .insert(named.name.clone(), (i as u32, type_args[i]));
                        }
                    }
                }

                // For blanket impls where impl_ty is a free type parameter
                // (e.g., `impl<I: Iterator> IntoIterator for I`):
                // If we have the receiver's concrete type ID, map the type param to it
                // so associated types like `type Iter = I` resolve to the receiver type.
                // Otherwise, register as a TypeParam for AssocTypeProjection resolution.
                if let Type::Named(named) = &impl_ty {
                    let name = &named.name;
                    if !self.current_type_params.contains_key(name)
                        && !self.is_known_type_name(name)
                    {
                        if let Some(recv_id) = receiver_type_id {
                            self.current_type_params.insert(name.clone(), (0, recv_id));
                        } else {
                            let type_id = self
                                .type_table
                                .borrow_mut()
                                .make_type_param(name.clone(), 0);
                            self.current_type_params.insert(name.clone(), (0, type_id));
                        }
                    }
                }

                // Set up associated type bindings for resolving Self::* types
                let old_associated_type_bindings =
                    std::mem::take(&mut self.current_associated_type_bindings);
                for binding in &associated_types {
                    let type_id = self.resolve_type(&binding.ty);
                    self.current_associated_type_bindings
                        .insert(binding.name.clone(), type_id);
                }

                let blanket_type_param = if is_blanket_type_param {
                    Some(impl_struct_name.clone())
                } else {
                    None
                };

                let mut method_found = false;
                for method in &methods {
                    if method.name == method_name {
                        let trait_name = self.get_type_name(&trait_type);
                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|t| self.resolve_type(t))
                            .unwrap_or(TypeTable::UNIT);
                        let self_kind = method
                            .params
                            .first()
                            .map(|p| p.self_kind)
                            .unwrap_or(ast::SelfKind::None);
                        let param_types = self.extract_param_types(&method.params);
                        let param_is_mut: Vec<bool> = method
                            .params
                            .iter()
                            .filter(|p| p.name != "self")
                            .map(|p| p.is_mut)
                            .collect();
                        found_traits.push(TraitMethodMatch {
                            trait_name,
                            method_info: MethodInfo {
                                return_type,
                                self_kind,
                                param_types,
                                param_is_mut,
                                inherited_from_base: None,
                                canonical_name: None,
                            },
                            impl_module_source: impl_module_source.clone(),
                            blanket_type_param: blanket_type_param.clone(),
                        });
                        method_found = true;
                    }
                }

                // If the method wasn't found in the impl block, check the trait
                // declaration for a default method with that name
                if !method_found {
                    let trait_name_str = self.get_type_name(&trait_type);
                    if let Some(trait_methods) = self.find_trait_decl_methods(&trait_name_str) {
                        for default_method in &trait_methods {
                            if default_method.name == method_name && default_method.body.is_some() {
                                let return_type = default_method
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_type(t))
                                    .unwrap_or(TypeTable::UNIT);
                                let self_kind = default_method
                                    .params
                                    .first()
                                    .map(|p| p.self_kind)
                                    .unwrap_or(ast::SelfKind::None);
                                let param_types = self.extract_param_types(&default_method.params);
                                let param_is_mut: Vec<bool> = default_method
                                    .params
                                    .iter()
                                    .filter(|p| p.name != "self")
                                    .map(|p| p.is_mut)
                                    .collect();
                                found_traits.push(TraitMethodMatch {
                                    trait_name: trait_name_str.clone(),
                                    method_info: MethodInfo {
                                        return_type,
                                        self_kind,
                                        param_types,
                                        param_is_mut,
                                        inherited_from_base: None,
                                        canonical_name: None,
                                    },
                                    impl_module_source: impl_module_source.clone(),
                                    blanket_type_param: blanket_type_param.clone(),
                                });
                            }
                        }
                    }
                }

                // Restore associated type bindings and type params
                self.current_associated_type_bindings = old_associated_type_bindings;
                self.current_type_params = old_type_params;
            }
        }

        // Remove duplicates
        found_traits.dedup_by(|a, b| a.trait_name == b.trait_name);

        // Return the first one found (if there are multiple, it would be ambiguous,
        // but we'll handle that later with explicit disambiguation syntax)
        found_traits.into_iter().next()
    }

    /// Find a trait declaration by name across all modules.
    /// Returns the trait's methods (cloned) if found.
    pub(super) fn find_trait_decl_methods(&self, trait_name: &str) -> Option<Vec<ast::Function>> {
        // Fast O(1) lookup via pre-built index instead of scanning all modules.
        if let Some((module_src, item_idx)) = self.trait_decl_index.get(trait_name) {
            let module = &self.loaded_modules[module_src];
            if let Item::Trait(trait_decl) = &module.items[*item_idx] {
                return Some(trait_decl.methods.clone());
            }
        }
        // Check current module items (not covered by the index).
        for item in &self.current_module_items {
            if let Item::Trait(trait_decl) = item
                && trait_decl.name == trait_name
            {
                return Some(trait_decl.methods.clone());
            }
        }
        None
    }

    /// Find Index trait implementation for a type
    pub(super) fn find_index_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexTraitInfo> {
        // Look for impl Index<...> for StructName
        self.find_indexing_trait_impl(struct_name, base_type_id, "Index", "index", "Output")
            .map(
                |(output_type, self_kind, trait_name, impl_module_source)| IndexTraitInfo {
                    output_type,
                    self_kind,
                    trait_name,
                    impl_module_source,
                },
            )
    }

    /// Find `KeyValueLiteralBuilder` trait implementation for a type.
    ///
    /// Checks first for an explicit `impl KeyValueLiteralBuilder for T` (with `Output = T`
    /// check for blanket-style self-as-builder usage), then falls back to checking whether
    /// `T` implements the `KeyValueLiteral` trait (separate builder pattern).
    pub(super) fn find_key_value_literal_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<KeyValueLiteralTraitInfo> {
        // Primary: explicit impl KeyValueLiteralBuilder for T (self-as-builder pattern)
        if let Some((value_type, self_kind, trait_name, _)) = self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            "KeyValueLiteralBuilder",
            "insert_literal",
            "Value",
        ) {
            // Check if Output = Self (self-as-builder pattern)
            let output_type = self
                .find_assoc_type_in_trait_impl(
                    struct_name,
                    base_type_id,
                    "KeyValueLiteralBuilder",
                    "Output",
                )
                .unwrap_or(TypeTable::UNKNOWN);
            // Accept if no Output constraint mismatch (output == Self or unknown)
            if output_type == TypeTable::UNKNOWN || output_type == base_type_id {
                return Some(KeyValueLiteralTraitInfo {
                    value_type,
                    builder_type: base_type_id,
                    self_kind,
                    trait_name,
                });
            }
        }

        // Secondary: explicit impl KeyValueLiteral for T with type Builder (separate builder
        // pattern for immutable output types).
        let builder_type = self.find_assoc_type_in_trait_impl(
            struct_name,
            base_type_id,
            "KeyValueLiteral",
            "Builder",
        )?;
        let builder_name = self.struct_name_for_type(builder_type)?;
        if let Some((value_type, self_kind, trait_name, _)) = self.find_indexing_trait_impl(
            &builder_name,
            builder_type,
            "KeyValueLiteralBuilder",
            "insert_literal",
            "Value",
        ) {
            return Some(KeyValueLiteralTraitInfo {
                value_type,
                builder_type,
                self_kind,
                trait_name,
            });
        }

        None
    }

    /// Find the value of a specific associated type in a trait impl for a given struct.
    fn find_assoc_type_in_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_base_name: &str,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.type_table.borrow().get(base_type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };

        let mut impls_to_check: Vec<(
            crate::ast::Type,
            crate::ast::Type,
            Vec<crate::ast::AssociatedTypeBinding>,
            Vec<crate::ast::GenericParam>,
        )> = Vec::new();

        if let Some(entries) = self.trait_impl_index.get(struct_name) {
            for (module_src, item_idx) in entries {
                let module = &self.loaded_modules[module_src];
                if let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impls_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.associated_types.clone(),
                        impl_block.type_params.clone(),
                    ));
                }
            }
        }
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                impls_to_check.push((
                    impl_block.ty.clone(),
                    trait_type.clone(),
                    impl_block.associated_types.clone(),
                    impl_block.type_params.clone(),
                ));
            }
        }

        for (impl_ty, trait_type, associated_types, impl_type_params) in impls_to_check {
            let trait_name = self.get_type_name(&trait_type);
            if !trait_name.starts_with(trait_base_name) {
                continue;
            }
            let binding = match associated_types.iter().find(|b| b.name == assoc_name) {
                Some(b) => b.clone(),
                None => continue,
            };
            let mut declared_type_params: IndexSet<String> = impl_type_params
                .iter()
                .map(|p| p.name.clone())
                .filter(|name| !self.is_known_type_name(name))
                .collect();
            if let Type::Generic(g) = &impl_ty {
                for arg in &g.args {
                    if let Type::Named(n) = arg
                        && !self.is_known_type_name(&n.name)
                    {
                        declared_type_params.insert(n.name.clone());
                    }
                }
            }
            let type_param_mapping = Self::build_type_param_mapping(
                &impl_ty,
                &concrete_type_args,
                &declared_type_params,
            );
            if !Self::verify_impl_type_compatibility(
                &impl_ty,
                &concrete_type_args,
                &declared_type_params,
                &self.type_table,
            ) {
                continue;
            }
            return Some(self.resolve_type_with_param_mapping(&binding.ty, &type_param_mapping));
        }
        None
    }

    /// Find `SequenceLiteralBuilder` trait implementation for a type.
    ///
    /// Checks for an explicit `impl SequenceLiteralBuilder for T` (self-as-builder) first.
    /// If not found, checks for `impl SequenceLiteral for T` with `type Builder` (separate
    /// builder pattern for immutable output types).
    pub(super) fn find_sequence_literal_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<SequenceLiteralTraitInfo> {
        // Primary: self-as-builder (impl SequenceLiteralBuilder for T)
        if let Some((element_type, self_kind, trait_name, impl_source)) = self
            .find_indexing_trait_impl(
                struct_name,
                base_type_id,
                "SequenceLiteralBuilder",
                "push_literal",
                "Element",
            )
        {
            let output_type = self
                .find_assoc_type_in_trait_impl(
                    struct_name,
                    base_type_id,
                    "SequenceLiteralBuilder",
                    "Output",
                )
                .unwrap_or(base_type_id);
            return Some(SequenceLiteralTraitInfo {
                element_type,
                builder_type: base_type_id,
                output_type,
                self_kind,
                trait_name,
                impl_module_source: impl_source,
            });
        }

        // Secondary: separate builder (impl SequenceLiteral for T with type Builder)
        let builder_type = self.find_assoc_type_in_trait_impl(
            struct_name,
            base_type_id,
            "SequenceLiteral",
            "Builder",
        )?;
        let builder_name = self.struct_name_for_type(builder_type)?;
        if let Some((element_type, self_kind, trait_name, impl_source)) = self
            .find_indexing_trait_impl(
                &builder_name,
                builder_type,
                "SequenceLiteralBuilder",
                "push_literal",
                "Element",
            )
        {
            let output_type = self
                .find_assoc_type_in_trait_impl(
                    &builder_name,
                    builder_type,
                    "SequenceLiteralBuilder",
                    "Output",
                )
                .unwrap_or(base_type_id);
            return Some(SequenceLiteralTraitInfo {
                element_type,
                builder_type,
                output_type,
                self_kind,
                trait_name,
                impl_module_source: impl_source,
            });
        }

        None
    }

    /// Find `IndexAssign` trait implementation for a type
    pub(super) fn find_index_assign_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexAssignTraitInfo> {
        // Look for impl IndexAssign<...> for StructName
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            "IndexAssign",
            "index_assign",
            "Input",
        )
        .map(
            |(input_type, self_kind, trait_name, impl_module_source)| IndexAssignTraitInfo {
                input_type,
                self_kind,
                trait_name,
                impl_module_source,
            },
        )
    }

    /// Find `IndexMut` trait implementation for a type
    pub(super) fn find_index_mut_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexMutTraitInfo> {
        // Look for impl IndexMut<...> for StructName
        self.find_indexing_trait_impl(struct_name, base_type_id, "IndexMut", "index_mut", "Output")
            .map(
                |(output_type, self_kind, trait_name, impl_module_source)| IndexMutTraitInfo {
                    output_type,
                    self_kind,
                    trait_name,
                    impl_module_source,
                },
            )
    }

    /// Find `IndexValue` trait implementation for a type
    pub(super) fn find_index_value_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexValueTraitInfo> {
        // Look for impl IndexValue<...> for StructName
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            "IndexValue",
            "index_value",
            "Output",
        )
        .map(
            |(output_type, self_kind, trait_name, impl_module_source)| IndexValueTraitInfo {
                output_type,
                self_kind,
                trait_name,
                impl_module_source,
            },
        )
    }

    /// Find `Eq` trait implementation for a type
    pub(super) fn find_eq_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<ArithmeticTraitInfo> {
        self.find_arithmetic_trait_impl(struct_name, base_type_id, "Eq", "eq")
    }

    /// Find `Ord` trait implementation for a type
    pub(super) fn find_ord_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<ArithmeticTraitInfo> {
        self.find_arithmetic_trait_impl(struct_name, base_type_id, "Ord", "cmp")
    }

    /// Find operator trait implementation
    pub(super) fn find_arithmetic_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_name: &str,
        method_name: &str,
    ) -> Option<ArithmeticTraitInfo> {
        // Get concrete type arguments from the base type (for generic instances)
        let concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.type_table.borrow().get(base_type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };

        // Collect impl blocks to check — use pre-built index for O(1) lookup by type name.
        let mut impl_blocks_to_check: Vec<(
            Type,
            Type,
            Vec<Function>,
            Vec<crate::ast::AssociatedTypeBinding>,
            Vec<crate::ast::GenericParam>,
        )> = Vec::new();

        if let Some(entries) = self.trait_impl_index.get(struct_name) {
            for (module_src, item_idx) in entries {
                let module = &self.loaded_modules[module_src];
                if let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
                        impl_block.type_params.clone(),
                    ));
                }
            }
        }

        // Also check current module items (not covered by the index).
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                impl_blocks_to_check.push((
                    impl_block.ty.clone(),
                    trait_type.clone(),
                    impl_block.methods.clone(),
                    impl_block.associated_types.clone(),
                    impl_block.type_params.clone(),
                ));
            }
        }

        // Process collected impl blocks
        for (impl_ty, trait_type, methods, associated_types, type_params) in impl_blocks_to_check {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if impl_struct_name != struct_name {
                continue;
            }

            // Check if this is the target trait
            let found_trait_name = self.get_type_name(&trait_type);
            if found_trait_name != trait_name {
                continue;
            }

            // Check trait bounds on type parameters (e.g., impl<T: Eq> Eq for Array<T>)
            if !type_params.iter().all(|p| p.bounds.is_empty()) && !concrete_type_args.is_empty() {
                let bounds_map: IndexMap<&str, Vec<String>> = type_params
                    .iter()
                    .filter(|p| !p.bounds.is_empty())
                    .map(|p| {
                        (
                            p.name.as_str(),
                            p.bounds.iter().map(|b| b.name.clone()).collect(),
                        )
                    })
                    .collect();

                let mut bounds_satisfied = true;
                if let ast::Type::Generic(generic) = &impl_ty {
                    for (i, arg) in generic.args.iter().enumerate() {
                        if let ast::Type::Named(named) = arg
                            && let Some(bounds) = bounds_map.get(named.name.as_str())
                            && let Some(&type_arg) = concrete_type_args.get(i)
                        {
                            if matches!(
                                self.type_table.borrow().get(type_arg),
                                ResolvedType::TypeParam { .. }
                            ) {
                                continue;
                            }
                            for bound in bounds {
                                if !self.type_implements_trait(type_arg, bound) {
                                    bounds_satisfied = false;
                                    break;
                                }
                            }
                        }
                        if !bounds_satisfied {
                            break;
                        }
                    }
                }
                if !bounds_satisfied {
                    continue;
                }
            }

            // Build type parameter mapping from impl_ty to concrete types
            let mut type_param_mapping =
                Self::build_type_param_mapping(&impl_ty, &concrete_type_args, &IndexSet::new());
            // Map `Self` to the concrete base type so `&Self` parameters resolve correctly
            type_param_mapping.insert("Self".to_string(), base_type_id);

            // Find the method
            for method in &methods {
                if method.name == method_name {
                    // Set up associated type bindings
                    let mut assoc_type_map: IndexMap<String, TypeId> = IndexMap::new();

                    // Process associated types (e.g., `type Output = Self`)
                    for assoc in &associated_types {
                        let resolved_type =
                            self.resolve_type_with_param_mapping(&assoc.ty, &type_param_mapping);
                        assoc_type_map.insert(assoc.name.clone(), resolved_type);
                    }

                    // Get the output type from associated types
                    let output_type = assoc_type_map
                        .get("Output")
                        .copied()
                        .unwrap_or(base_type_id);

                    let self_kind = method
                        .params
                        .first()
                        .map(|p| p.self_kind)
                        .unwrap_or(ast::SelfKind::None);

                    // Resolve the rhs parameter type (first non-self parameter)
                    let rhs_type = method
                        .params
                        .iter()
                        .find(|p| p.self_kind == ast::SelfKind::None)
                        .map(|p| self.resolve_type_with_param_mapping(&p.ty, &type_param_mapping));

                    return Some(ArithmeticTraitInfo {
                        output_type,
                        self_kind,
                        trait_name: trait_name.to_string(),
                        rhs_type,
                    });
                }
            }
        }

        None
    }

    /// Check if a type implements a specific trait (for trait bound checking)
    pub(super) fn type_implements_trait(&self, type_id: TypeId, trait_name: &str) -> bool {
        let resolved = self.type_table.borrow().get(type_id).clone();

        // Type parameters satisfy bounds declared on them (e.g., T: Describable
        // means T implements Describable within the scope of that declaration)
        if let ResolvedType::TypeParam { name, .. } = &resolved {
            if let Some(bounds) = self.current_type_param_bounds.get(name) {
                return bounds.iter().any(|b| b == trait_name);
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

        // Get the type name and type args for looking up implementations
        let (type_name, type_args) = match &resolved {
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. } => (name.clone(), None),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => (
                name.clone(),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
            ),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For references, check if the inner type implements the trait
                return self.type_implements_trait(*inner, trait_name);
            }
            ResolvedType::Tuple(elems) => {
                // Tuples implement a trait when all elements implement it
                let elems = elems.clone();
                return elems
                    .iter()
                    .all(|e| self.type_implements_trait(*e, trait_name));
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
        if let Some(entries) = self.trait_impl_index.get(type_name) {
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
        for item in &self.current_module_items {
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
        for (module_src, item_idx) in self.blanket_trait_impl_index.as_ref() {
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
                for item in &self.current_module_items {
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
                // Resolve the method signature with Self = self_type_id (the TypeParam)
                let old_self_type = self.current_self_type;
                self.current_self_type = Some(self_type_id);

                // Set up associated type bindings as projections so that
                // Self::AssocType resolves to AssocTypeProjection(self_type_id, "AssocType")
                let old_bindings = std::mem::take(&mut self.current_associated_type_bindings);
                for assoc_decl in &trait_assoc_types {
                    let projection = self.type_table.borrow_mut().make_assoc_type_projection(
                        self_type_id,
                        assoc_decl.name.clone(),
                        assoc_decl.bounds.clone(),
                    );
                    self.current_associated_type_bindings
                        .insert(assoc_decl.name.clone(), projection);
                }

                let return_type = method
                    .return_type
                    .as_ref()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNIT);
                let self_kind = method
                    .params
                    .first()
                    .map(|p| p.self_kind)
                    .unwrap_or(ast::SelfKind::None);
                let param_types = self.extract_param_types(&method.params);
                let param_is_mut: Vec<bool> = method
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.is_mut)
                    .collect();

                self.current_associated_type_bindings = old_bindings;
                self.current_self_type = old_self_type;

                return Some((
                    trait_name.clone(),
                    MethodInfo {
                        return_type,
                        self_kind,
                        param_types,
                        param_is_mut,
                        inherited_from_base: None,
                        canonical_name: None,
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
                    if !self.type_implements_trait(type_arg, &bound.name) {
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

    /// Look up the type parameters of a function from its AST definition.
    pub(super) fn lookup_function_type_params(
        &self,
        callee_module: &ModuleSource,
        func_name: &str,
    ) -> Vec<ast::GenericParam> {
        // Try local functions
        if callee_module.is_entry_point() {
            for item in &self.current_module_items {
                if let ast::Item::Function(func) = item
                    && func.name == func_name
                {
                    return func.type_params.clone();
                }
            }
        }

        // Try loaded modules
        if let Some(module) = self.loaded_modules.get(callee_module) {
            for item in &module.items {
                if let ast::Item::Function(func) = item
                    && func.name == func_name
                {
                    return func.type_params.clone();
                }
            }
        }

        Vec::new()
    }

    /// Convert a `TypeId` to a human-readable string for error messages
    pub(super) fn type_id_to_string(&self, type_id: TypeId) -> String {
        let resolved = self.type_table.borrow().get(type_id).clone();
        match resolved {
            ResolvedType::Primitive(prim) => format!("{prim:?}").to_lowercase(),
            ResolvedType::Struct { name, .. } => name,
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                if type_args.is_empty() {
                    name
                } else {
                    let args: Vec<String> = type_args
                        .iter()
                        .map(|&t| self.type_id_to_string(t))
                        .collect();
                    format!("{}<{}>", name, args.join(", "))
                }
            }
            ResolvedType::BuiltinArray(elem) => {
                format!("builtin::array<{}>", self.type_id_to_string(elem))
            }
            ResolvedType::Ref(inner) => format!("&{}", self.type_id_to_string(inner)),
            ResolvedType::MutRef(inner) => format!("&mut {}", self.type_id_to_string(inner)),
            ResolvedType::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(|&t| self.type_id_to_string(t)).collect();
                format!("[{}]", parts.join(", "))
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                let param_strs: Vec<String> =
                    params.iter().map(|&t| self.type_id_to_string(t)).collect();
                let ret_str = self.type_id_to_string(return_type);
                format!("fn({}) -> {}", param_strs.join(", "), ret_str)
            }
            ResolvedType::TypeParam { name, .. } => name,
            ResolvedType::Unit => "()".to_string(),
            ResolvedType::Never => "!".to_string(),
            ResolvedType::Unknown => "<unknown>".to_string(),
            ResolvedType::Error => "<error>".to_string(),
            _ => format!("{resolved:?}"),
        }
    }

    /// Helper to find indexing trait implementations (Index, `IndexMut`, or `IndexAssign`)
    pub(super) fn find_indexing_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_base_name: &str,
        method_name: &str,
        assoc_type_name: &str,
    ) -> Option<(TypeId, ast::SelfKind, String, crate::name::ModuleSource)> {
        // Get concrete type arguments from the base type (for generic instances like Triple<i32>)
        let concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.type_table.borrow().get(base_type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };
        // Collect impl blocks to check — use pre-built index for O(1) lookup by type name.
        let mut impl_blocks_to_check: Vec<(
            Type,
            Type,
            Vec<Function>,
            Vec<crate::ast::AssociatedTypeBinding>,
            Vec<crate::ast::GenericParam>,
            crate::name::ModuleSource,
        )> = Vec::new();

        if let Some(entries) = self.trait_impl_index.get(struct_name) {
            for (module_src, item_idx) in entries {
                let module = &self.loaded_modules[module_src];
                if let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
                        impl_block.type_params.clone(),
                        module_src.clone(),
                    ));
                }
            }
        }

        // Also check current module items (not covered by the index).
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                impl_blocks_to_check.push((
                    impl_block.ty.clone(),
                    trait_type.clone(),
                    impl_block.methods.clone(),
                    impl_block.associated_types.clone(),
                    impl_block.type_params.clone(),
                    self.current_module_source.clone(),
                ));
            }
        }

        // Process collected impl blocks
        for (impl_ty, trait_type, methods, associated_types, impl_type_params, impl_source) in
            impl_blocks_to_check
        {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if impl_struct_name != struct_name {
                continue;
            }

            // Check if this is the target trait (Index or IndexAssign)
            // Use base trait name (e.g., "Index" not "Index<i32>") for method mangling
            let trait_name = self.get_type_name(&trait_type);
            if !trait_name.starts_with(trait_base_name) {
                continue;
            }

            // Collect declared type parameter names for this impl block.
            // The parser treats all args in `<String, V>` as type params,
            // so filter out names that are known types (structs, primitives).
            let mut declared_type_params: IndexSet<String> = impl_type_params
                .iter()
                .map(|p| p.name.clone())
                .filter(|name| !self.is_known_type_name(name))
                .collect();
            // Also infer type params from impl type args when no explicit type params clause.
            // e.g., for `impl Foo for TreeMap<String, V>`, infer V (not String) as a type param.
            if let Type::Generic(g) = &impl_ty {
                for arg in &g.args {
                    if let Type::Named(n) = arg
                        && !self.is_known_type_name(&n.name)
                    {
                        declared_type_params.insert(n.name.clone());
                    }
                }
            }

            // Build type parameter mapping from impl_ty to concrete types
            // e.g., for `impl IndexValue<i32> for Triple<T>` with concrete type `Triple<i32>`
            // we build the mapping: {"T" -> i32}
            // Only map names that are actual type parameters (not concrete types like String)
            let type_param_mapping = Self::build_type_param_mapping(
                &impl_ty,
                &concrete_type_args,
                &declared_type_params,
            );

            // Verify non-type-parameter positions match the concrete type args
            if !Self::verify_impl_type_compatibility(
                &impl_ty,
                &concrete_type_args,
                &declared_type_params,
                &self.type_table,
            ) {
                continue;
            }

            // Find the method
            for method in &methods {
                if method.name == method_name {
                    // Set up associated type bindings
                    let old_bindings = std::mem::take(&mut self.current_associated_type_bindings);
                    for binding in &associated_types {
                        // Resolve the associated type, substituting type parameters
                        let type_id =
                            self.resolve_type_with_param_mapping(&binding.ty, &type_param_mapping);
                        self.current_associated_type_bindings
                            .insert(binding.name.clone(), type_id);
                    }

                    // Get the associated type (Output or Input)
                    let assoc_type = self
                        .current_associated_type_bindings
                        .get(assoc_type_name)
                        .copied()
                        .unwrap_or(TypeTable::UNKNOWN);

                    let self_kind = method
                        .params
                        .first()
                        .map(|p| p.self_kind)
                        .unwrap_or(ast::SelfKind::None);

                    // Restore associated type bindings
                    self.current_associated_type_bindings = old_bindings;

                    // trait_name is already base name (get_type_name returns name without type args)
                    return Some((assoc_type, self_kind, trait_name, impl_source));
                }
            }
        }

        None
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
        let mut mapping = IndexMap::new();

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
    fn verify_impl_type_compatibility(
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
    fn impl_type_matches_concrete(
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

    /// Resolve a type, substituting type parameters using the provided mapping.
    pub(super) fn resolve_type_with_param_mapping(
        &mut self,
        ty: &Type,
        type_param_mapping: &IndexMap<String, TypeId>,
    ) -> TypeId {
        match ty {
            Type::Named(n) => {
                // Check if this is a type parameter that should be substituted
                if let Some(&type_id) = type_param_mapping.get(&n.name) {
                    return type_id;
                }
                // Otherwise, resolve normally
                self.resolve_type(ty)
            }
            Type::Generic(g) => {
                // Resolve generic type with substituted arguments
                let resolved_args: Vec<TypeId> = g
                    .args
                    .iter()
                    .map(|arg| self.resolve_type_with_param_mapping(arg, type_param_mapping))
                    .collect();

                // Special-case Option to use its dedicated type
                let base_name = &g.name;
                if base_name == "Option" {
                    let inner = resolved_args.first().copied().unwrap_or(TypeTable::UNKNOWN);
                    self.type_table.borrow_mut().make_option(inner)
                } else {
                    // For generic types, create a generic instance.
                    // Use the defining module source of the struct/variant to ensure the
                    // resulting TypeId matches what resolve_type produces for the same type.
                    // Falling back to current_module_source causes type identity mismatches when
                    // the struct is defined in a different module.
                    let module_source = self
                        .variant_cases
                        .get(base_name.as_str())
                        .map(|info| info.module_source.clone())
                        .or_else(|| {
                            self.struct_fields
                                .get(base_name.as_str())
                                .map(|info| info.module_source.clone())
                        })
                        .unwrap_or_else(|| self.current_module_source.clone());
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::GenericInstance {
                            name: base_name.clone(),
                            module_source,
                            type_args: resolved_args,
                        })
                }
            }
            Type::Reference(inner) => {
                let inner_id = self.resolve_type_with_param_mapping(inner, type_param_mapping);
                self.type_table.borrow_mut().make_ref(inner_id)
            }
            Type::MutReference(inner) => {
                let inner_id = self.resolve_type_with_param_mapping(inner, type_param_mapping);
                self.type_table.borrow_mut().make_mut_ref(inner_id)
            }
            Type::NamespacedGeneric(n) => {
                // T::AssocType where T maps to a concrete type → resolve the assoc type
                if let Some(&concrete_type_id) = type_param_mapping.get(&n.namespace)
                    && let Some(assoc_id) =
                        self.resolve_assoc_type_from_concrete(concrete_type_id, &n.name)
                {
                    return assoc_id;
                }
                self.resolve_type(ty)
            }
            // For other types, fall back to normal resolution
            _ => self.resolve_type(ty),
        }
    }

    /// Resolve an associated type name from a concrete type's trait implementations.
    /// Searches all trait impls for the struct and returns the `TypeId` of the associated type
    /// binding with the given name, with type parameters substituted.
    pub(super) fn resolve_assoc_type_from_concrete(
        &mut self,
        type_id: TypeId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let struct_name = self.struct_name_for_type(type_id)?;
        let concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.type_table.borrow().get(type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };

        let mut impls_to_check: Vec<(
            crate::ast::Type,
            Vec<crate::ast::AssociatedTypeBinding>,
            Vec<crate::ast::GenericParam>,
        )> = Vec::new();

        if let Some(entries) = self.trait_impl_index.get(&struct_name) {
            for (module_src, item_idx) in entries {
                let module = &self.loaded_modules[module_src];
                if let crate::ast::Item::Impl(impl_block) = &module.items[*item_idx]
                    && impl_block.trait_type.is_some()
                {
                    impls_to_check.push((
                        impl_block.ty.clone(),
                        impl_block.associated_types.clone(),
                        impl_block.type_params.clone(),
                    ));
                }
            }
        }
        for item in &self.current_module_items {
            if let crate::ast::Item::Impl(impl_block) = item
                && impl_block.trait_type.is_some()
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                impls_to_check.push((
                    impl_block.ty.clone(),
                    impl_block.associated_types.clone(),
                    impl_block.type_params.clone(),
                ));
            }
        }

        for (impl_ty, associated_types, impl_type_params) in impls_to_check {
            let binding = match associated_types.iter().find(|b| b.name == assoc_name) {
                Some(b) => b.clone(),
                None => continue,
            };

            let mut declared_type_params: indexmap::IndexSet<String> = impl_type_params
                .iter()
                .map(|p| p.name.clone())
                .filter(|name| !self.is_known_type_name(name))
                .collect();
            if let Type::Generic(g) = &impl_ty {
                for arg in &g.args {
                    if let Type::Named(n) = arg
                        && !self.is_known_type_name(&n.name)
                    {
                        declared_type_params.insert(n.name.clone());
                    }
                }
            }

            let type_param_mapping = Self::build_type_param_mapping(
                &impl_ty,
                &concrete_type_args,
                &declared_type_params,
            );

            if !Self::verify_impl_type_compatibility(
                &impl_ty,
                &concrete_type_args,
                &declared_type_params,
                &self.type_table,
            ) {
                continue;
            }

            return Some(self.resolve_type_with_param_mapping(&binding.ty, &type_param_mapping));
        }

        None
    }

    /// Try to resolve a method call on an index expression using `IndexMut`.
    /// Returns Some(TirExpr) if the method needs &mut self and the type implements `IndexMut`.
    /// Returns None if we should fall back to normal resolution (using Index).
    pub(super) fn try_resolve_index_mut_method_call(
        &mut self,
        index_expr: &ast::IndexExpr,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TirExpr> {
        // First, resolve the indexed container to get its type
        let container_expr = self.resolve_expr(&index_expr.expr, ctx, None);

        // Check if this is an Array type (Arrays use optimized direct access, not traits)
        let is_array = self
            .type_table
            .borrow()
            .as_array(container_expr.type_id)
            .is_some();
        if is_array {
            return None; // Use normal resolution for arrays
        }

        // Get base type (unwrap reference if needed)
        let base_type_id = match self.type_table.borrow().get(container_expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => container_expr.type_id,
        };

        // Get struct name from base type
        let struct_name = match self.type_table.borrow().get(base_type_id).clone() {
            ResolvedType::Struct { name, .. } => name,
            ResolvedType::GenericInstance { name, .. } => name,
            _ => return None, // Not a struct type
        };

        // Check if the type implements IndexMut
        let index_resolved = self.resolve_expr(&index_expr.index, ctx, None);
        let index_type = index_resolved.type_id;

        let index_mut_info =
            self.find_index_mut_trait_impl(&struct_name, base_type_id, index_type)?;

        // Now we need to check if the method being called requires &mut self
        // First, look up method info on the OUTPUT type (what IndexMut returns)
        let output_type = index_mut_info.output_type;
        let output_base_type_id = match self.type_table.borrow().get(output_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => output_type,
        };

        let (output_struct_name, output_module_source, output_type_args) =
            match self.type_table.borrow().get(output_base_type_id).clone() {
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                } => (name, module_source, None),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } => (
                    name,
                    module_source,
                    if type_args.is_empty() {
                        None
                    } else {
                        Some(type_args)
                    },
                ),
                _ => (
                    self.type_table
                        .borrow()
                        .mangle_type_name(output_base_type_id),
                    self.current_module_source.clone(),
                    None,
                ),
            };

        // Look up method info to check if it needs &mut self
        let mut method_info = self.lookup_method_info(output_type, &method_call.method);
        let mut method_trait_name: Option<String> = None;
        let mut method_trait_impl_source: Option<ModuleSource> = None;

        if method_info.is_none()
            && let Some(trait_match) = self.find_trait_method_for_type(
                &output_struct_name,
                &method_call.method,
                &output_module_source,
                output_type_args.as_deref(),
                Some(output_type),
            )
        {
            method_trait_name = Some(trait_match.trait_name);
            method_info = Some(trait_match.method_info);
            method_trait_impl_source = Some(trait_match.impl_module_source);
        }

        let MethodInfo {
            return_type,
            self_kind,
            param_types,
            param_is_mut: method_param_is_mut,
            inherited_from_base: _,
            canonical_name: _,
        } = method_info?;

        // Only use IndexMut if the method requires &mut self
        if self_kind != ast::SelfKind::MutRef {
            return None; // Method doesn't need &mut, fall back to Index
        }

        // Generate: container.index_mut(index).method(args)
        // Step 1: Create container.index_mut(index) call
        let receiver_for_index_mut = self.adjust_receiver_for_self_kind(
            container_expr,
            index_mut_info.self_kind,
            index_expr.span,
        );

        let mangled_index_mut_name =
            MethodName::format_local(&struct_name, Some(&index_mut_info.trait_name), "index_mut");

        // IndexMut returns &mut Output
        let mut_ref_output_type = self
            .type_table
            .borrow_mut()
            .make_mut_ref(index_mut_info.output_type);

        let index_mut_call = TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver_for_index_mut),
                func: FunctionRef {
                    module_source: index_mut_info.impl_module_source.clone(),
                    name: mangled_index_mut_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        struct_name.clone(),
                        Some(index_mut_info.trait_name.clone()),
                        "index_mut".to_string(),
                    )),
                    is_cm_adapter: false,
                },
                type_args: vec![],
                args: vec![index_resolved],
                param_is_mut: vec![false],
            },
            mut_ref_output_type,
            index_expr.span,
        );

        // Step 2: Resolve method args with expected parameter types for literal coercion
        let args: Vec<TirExpr> = method_call
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let expected = param_types.get(i).copied();
                self.resolve_expr(a, ctx, expected)
            })
            .collect();

        // Step 3: Resolve method type args
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Step 4: Create the method call on the result of index_mut
        // The receiver for the method is index_mut_call (which has type &mut Output)
        let receiver_for_method =
            self.adjust_receiver_for_self_kind(index_mut_call, self_kind, method_call.span);

        let mangled_method_name = MethodName::format_local(
            &output_struct_name,
            method_trait_name.as_deref(),
            &method_call.method,
        );

        // Use trait impl module source if this is a trait method, otherwise current module
        let method_call_module_source =
            method_trait_impl_source.unwrap_or_else(|| self.current_module_source.clone());

        Some(TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver_for_method),
                func: FunctionRef {
                    module_source: method_call_module_source,
                    name: mangled_method_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        output_struct_name,
                        method_trait_name,
                        method_call.method.clone(),
                    )),
                    is_cm_adapter: false,
                },
                type_args,
                args,
                param_is_mut: method_param_is_mut,
            },
            return_type,
            method_call.span,
        ))
    }
}
