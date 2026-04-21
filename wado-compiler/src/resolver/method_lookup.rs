//! Method lookup, operator resolution, and indexing trait dispatch.

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, BinaryOp, Expr, Item, Literal, Type, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirExpr, TirExprKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::infer::InferCtx;
use super::types::{
    ArithmeticTraitInfo, FunctionContext, IndexAssignTraitInfo, IndexMutTraitInfo, IndexTraitInfo,
    IndexValueTraitInfo, KeyValueLiteralTraitInfo, MethodInfo, SequenceLiteralTraitInfo,
};

/// Lightweight reference to an impl block, avoiding deep clones.
/// Stores just enough info to re-access the impl block's fields on demand.
enum ImplBlockRef {
    /// From `loaded_modules[module_source].items[item_idx]`
    Loaded(ModuleSource, usize),
    /// From `current_module_items[item_idx]`, with the current module source
    CurrentModule(usize),
}

/// Inputs for [`Resolver::infer_method_type_args`].
///
/// Groups everything the caller has already resolved about the method
/// (signature fields in `TypeParam`-based form, impl-level offset) with the
/// call-site context (argument list in both typed and raw forms, expected
/// return type) that the inference solver needs.
pub(super) struct MethodInferenceInput<'a> {
    /// Receiver's `TypeId` at the call site (any reference level; the
    /// helper strips references internally).
    pub receiver_type: TypeId,
    /// Method name — used to look up the method's AST for the list of
    /// method-level type parameter names.
    pub method_name: &'a str,
    /// Number of impl-level type parameters already in scope (e.g. 2 for
    /// `impl<A, B> Container<A, B>`). Method-level type parameters are
    /// numbered starting from this offset.
    pub impl_offset: u32,
    /// Method parameter types in their `TypeParam`-based (uninstantiated)
    /// form; parallel to `args` / `raw_args`.
    pub param_types: &'a [TypeId],
    /// Already-resolved argument expressions, in order.
    pub args: &'a [TirExpr],
    /// Raw AST expressions for `args`, used to detect literal-number
    /// arguments that participate in deferred-literal unification.
    pub raw_args: &'a [Expr],
    /// Method declared return type in its `TypeParam`-based form; used for
    /// back-inference from `expected_return_type`.
    pub decl_return_type: TypeId,
    /// Expected return type at the call site (from a type annotation or
    /// surrounding call), used for back-inference.
    pub expected_return_type: Option<TypeId>,
}

impl<H: CompilerHost> Resolver<'_, H> {
    /// Get a reference to the `ImplBlock` from an `ImplBlockRef`.
    fn get_impl_block<'b>(&'b self, r: &ImplBlockRef) -> &'b ast::ImplBlock {
        match r {
            ImplBlockRef::Loaded(module_src, item_idx) => {
                let module = &self.loaded_modules[module_src];
                match &module.items[*item_idx] {
                    Item::Impl(impl_block) => impl_block,
                    _ => unreachable!("ImplBlockRef::Loaded points to non-impl item"),
                }
            }
            ImplBlockRef::CurrentModule(item_idx) => match &self.current_module_items[*item_idx] {
                Item::Impl(impl_block) => impl_block,
                _ => unreachable!("ImplBlockRef::CurrentModule points to non-impl item"),
            },
        }
    }

    /// Get the module source for an `ImplBlockRef`.
    fn impl_block_module_source(&self, r: &ImplBlockRef) -> ModuleSource {
        match r {
            ImplBlockRef::Loaded(module_src, _) => module_src.clone(),
            ImplBlockRef::CurrentModule(_) => self.current_module_source.clone(),
        }
    }

    /// Collect trait impl block references for a given type name.
    /// Returns lightweight `ImplBlockRef` values instead of cloning impl block data.
    fn collect_trait_impl_refs(&self, type_name: &str) -> Vec<ImplBlockRef> {
        let mut refs = Vec::new();
        if let Some(entries) = self.trait_env.impl_index.get(type_name) {
            for (module_src, item_idx) in entries {
                let module = &self.loaded_modules[module_src];
                if let Item::Impl(impl_block) = &module.items[*item_idx]
                    && impl_block.trait_type.is_some()
                {
                    refs.push(ImplBlockRef::Loaded(module_src.clone(), *item_idx));
                }
            }
        }
        for (idx, item) in self.current_module_items.iter().enumerate() {
            if let Item::Impl(impl_block) = item
                && impl_block.trait_type.is_some()
                && Self::get_type_name_static(&impl_block.ty) == type_name
            {
                refs.push(ImplBlockRef::CurrentModule(idx));
            }
        }
        refs
    }

    /// Collect trait impl block references for multiple type names.
    fn collect_trait_impl_refs_multi(&self, type_names: &[String]) -> Vec<ImplBlockRef> {
        let mut refs = Vec::new();
        for name in type_names {
            if let Some(entries) = self.trait_env.impl_index.get(name.as_str()) {
                for (module_src, item_idx) in entries {
                    let module = &self.loaded_modules[module_src];
                    if let Item::Impl(impl_block) = &module.items[*item_idx]
                        && impl_block.trait_type.is_some()
                    {
                        refs.push(ImplBlockRef::Loaded(module_src.clone(), *item_idx));
                    }
                }
            }
        }
        for (idx, item) in self.current_module_items.iter().enumerate() {
            if let Item::Impl(impl_block) = item
                && impl_block.trait_type.is_some()
            {
                let impl_struct_name = Self::get_type_name_static(&impl_block.ty);
                if type_names.contains(&impl_struct_name) {
                    refs.push(ImplBlockRef::CurrentModule(idx));
                }
            }
        }
        refs
    }

    /// Build declared type params for an impl block, filtering out known type names.
    fn build_declared_type_params(&self, impl_block: &ast::ImplBlock) -> IndexSet<String> {
        let mut declared: IndexSet<String> = impl_block
            .type_params
            .iter()
            .map(|p| p.name.clone())
            .filter(|name| !self.is_known_type_name(name))
            .collect();
        if let Type::Generic(g) = &impl_block.ty {
            for arg in &g.args {
                if let Type::Named(n) = arg
                    && !self.is_known_type_name(&n.name)
                {
                    declared.insert(n.name.clone());
                }
            }
        }
        declared
    }

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

    /// Get the struct name from a type ID, if it's a struct, generic instance, newtype, or flags.
    pub(super) fn struct_name_for_type(&self, type_id: TypeId) -> Option<String> {
        match self.type_table.borrow().get(type_id) {
            ResolvedType::Struct { name, .. }
            | ResolvedType::GenericInstance { name, .. }
            | ResolvedType::Newtype { name, .. }
            | ResolvedType::Flags { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    /// For newtypes, get the base type name and ID for trait impl lookup fallback.
    /// Returns (`base_name`, `base_type_id`) if the type is a newtype; otherwise returns the same name/id.
    pub(super) fn newtype_base_lookup(&self, name: &str, type_id: TypeId) -> (String, TypeId) {
        let tt = self.type_table.borrow();
        if let Some(base_id) = tt.get_newtype_base(type_id) {
            drop(tt);
            if let Some(base_name) = self.struct_name_for_type(base_id) {
                return (base_name, base_id);
            }
        }
        (name.to_string(), type_id)
    }

    /// Check if a name refers to a known type (struct, variant, enum, flags, newtype, or primitive).
    /// Uses pre-built cache for O(1) lookup instead of scanning all module maps.
    pub(super) fn is_known_type_name(&self, name: &str) -> bool {
        self.known_type_names_cache.contains(name)
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
                ResolvedType::Flags { .. } => return "u32".to_string(),
                _ => return self.type_table.borrow().type_name(current),
            }
        }
    }

    /// Find the module source for a struct by name
    pub(super) fn find_struct_module_source(&self, struct_name: &str) -> ModuleSource {
        // Check if it's a primitive type - impl blocks live in core:prelude/primitive.wado
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
            return ModuleSource::primitive();
        }

        // Check current module
        for item in self.current_module_items {
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

        // Check newtypes/flags — the impl block may live in the module that defines the type
        if let Some(&type_id) = self.newtypes.get(struct_name) {
            let ms = match self.type_table.borrow().get(type_id).clone() {
                ResolvedType::Newtype { module_source, .. }
                | ResolvedType::Flags { module_source, .. } => Some(module_source),
                _ => None,
            };
            if let Some(module_source) = ms {
                return module_source;
            }
        }

        // Check loaded modules for newtype definitions
        for (module_source, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Newtype(alias) = item
                    && alias.name == struct_name
                {
                    return module_source.clone();
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

        // Check cache
        let cache_key = (base_type_id, method_name.to_string());
        if let Some(cached) = self.method_info_cache.get(&cache_key) {
            return cached.clone();
        }

        let result = self.lookup_method_info_uncached(base_type_id, method_name);
        self.method_info_cache.insert(cache_key, result.clone());
        result
    }

    fn lookup_method_info_uncached(
        &mut self,
        base_type_id: TypeId,
        method_name: &str,
    ) -> Option<MethodInfo> {
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
            // Generic instances like Box<i32> use the base name "Box" for method lookup.
            // Tuples (GenericInstance with name "Tuple") have special built-in methods.
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                if TypeTable::is_tuple_type(name, module_source) {
                    let elems = type_args;
                    if method_name == "len" {
                        return Some(MethodInfo {
                            return_type: TypeTable::I32,
                            self_kind: ast::SelfKind::Ref,
                            param_types: vec![],
                            param_is_mut: vec![],
                            inherited_from_base: None,
                            cm_name: None,
                            is_ref_impl: false,
                            method_type_param_ids: vec![],
                            param_defaults: vec![],
                            param_names: vec![],
                        });
                    }
                    if method_name == "zip" {
                        if elems.is_empty() {
                            return None;
                        }
                        let inner_arities: Vec<Vec<TypeId>> = elems
                            .iter()
                            .filter_map(|e| self.type_table.borrow().as_tuple(*e))
                            .collect();
                        if inner_arities.len() != elems.len() {
                            return None;
                        }
                        let arity = inner_arities[0].len();
                        if !inner_arities.iter().all(|a| a.len() == arity) {
                            return None;
                        }
                        let mut transposed = Vec::with_capacity(arity);
                        for col in 0..arity {
                            let col_types: Vec<TypeId> =
                                inner_arities.iter().map(|row| row[col]).collect();
                            let col_tuple = self.type_table.borrow_mut().make_tuple(col_types);
                            transposed.push(col_tuple);
                        }
                        let return_type = self.type_table.borrow_mut().make_tuple(transposed);
                        return Some(MethodInfo {
                            return_type,
                            self_kind: ast::SelfKind::Ref,
                            param_types: vec![],
                            param_is_mut: vec![],
                            inherited_from_base: None,
                            cm_name: None,
                            is_ref_impl: false,
                            method_type_param_ids: vec![],
                            param_defaults: vec![],
                            param_names: vec![],
                        });
                    }
                    (
                        TypeTable::TUPLE_TYPE_NAME.to_string(),
                        None,
                        Some(elems.clone()),
                        None,
                    )
                } else {
                    (
                        name.clone(),
                        Some(module_source.clone()),
                        if type_args.is_empty() {
                            None
                        } else {
                            Some(type_args.clone())
                        },
                        None,
                    )
                }
            }
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
            // Flags: first try looking up methods on the flags type itself,
            // then fall back to u32 for method inheritance
            ResolvedType::Flags {
                name,
                module_source,
            } => (
                name.clone(),
                Some(module_source.clone()),
                None,
                Some(TypeTable::U32),
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
            if let Some((self_kind, param_types, param_is_mut, param_defaults, param_names)) = self
                .find_local_method_info(&struct_name, method_name, receiver_type_args.as_deref())
            {
                return Some(MethodInfo {
                    return_type,
                    self_kind,
                    param_types,
                    param_is_mut,
                    inherited_from_base: None,
                    cm_name: None,
                    is_ref_impl: false,
                    method_type_param_ids: vec![],
                    param_defaults,
                    param_names,
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
                                    // Set up type params for generic impls (e.g., impl Array<T>).
                                    // Inherited scope; only `type_params` is replaced.
                                    let mut scope = self.enter_inherited_type_param_scope();
                                    scope.trait_ctx.type_params.clear();
                                    let mut impl_offset = 0u32;
                                    if let Some(ref type_args) = receiver_type_args
                                        && let Type::Generic(generic) = &impl_block.ty
                                    {
                                        impl_offset = type_args.len() as u32;
                                        for (i, arg) in generic.args.iter().enumerate() {
                                            if let Type::Named(named) = arg
                                                && i < type_args.len()
                                            {
                                                scope.trait_ctx.type_params.insert(
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
                                        let type_param_id = scope.type_table.borrow_mut().intern(
                                            ResolvedType::TypeParam {
                                                name: type_param.name.clone(),
                                                index,
                                            },
                                        );
                                        scope.trait_ctx.type_params.insert(
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
                                    let mut cached = scope
                                        .module_type_maps_cache
                                        .shift_remove(module_source)
                                        .expect("cache populated by ensure_module_maps_cached");
                                    std::mem::swap(
                                        &mut scope.struct_fields,
                                        &mut cached.struct_fields,
                                    );
                                    std::mem::swap(
                                        &mut scope.variant_cases,
                                        &mut cached.variant_cases,
                                    );
                                    std::mem::swap(&mut scope.enum_cases, &mut cached.enum_cases);
                                    std::mem::swap(&mut scope.flags_cases, &mut cached.flags_cases);
                                    std::mem::swap(&mut scope.newtypes, &mut cached.newtypes);
                                    std::mem::swap(
                                        &mut scope.resource_types,
                                        &mut cached.resource_types,
                                    );

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
                                    let param_names: Vec<String> = method
                                        .params
                                        .iter()
                                        .filter(|p| p.name != "self")
                                        .map(|p| p.name.clone())
                                        .collect();

                                    std::mem::swap(
                                        &mut scope.struct_fields,
                                        &mut cached.struct_fields,
                                    );
                                    std::mem::swap(
                                        &mut scope.variant_cases,
                                        &mut cached.variant_cases,
                                    );
                                    std::mem::swap(&mut scope.enum_cases, &mut cached.enum_cases);
                                    std::mem::swap(&mut scope.flags_cases, &mut cached.flags_cases);
                                    std::mem::swap(&mut scope.newtypes, &mut cached.newtypes);
                                    std::mem::swap(
                                        &mut scope.resource_types,
                                        &mut cached.resource_types,
                                    );
                                    scope
                                        .module_type_maps_cache
                                        .insert(module_source.clone(), cached);
                                    drop(scope);

                                    return Some(MethodInfo {
                                        return_type,
                                        self_kind,
                                        param_types,
                                        param_is_mut,
                                        inherited_from_base: None,
                                        cm_name: None,
                                        is_ref_impl: false,
                                        method_type_param_ids: vec![],
                                        param_defaults,
                                        param_names,
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
                                    // Set up type params for generic impls (e.g., impl Array<T>).
                                    // Inherited scope; only `type_params` is replaced.
                                    let mut scope = self.enter_inherited_type_param_scope();
                                    scope.trait_ctx.type_params.clear();
                                    let mut impl_offset = 0u32;
                                    if let Some(ref type_args) = receiver_type_args
                                        && let Type::Generic(generic) = &impl_block.ty
                                    {
                                        impl_offset = type_args.len() as u32;
                                        for (i, arg) in generic.args.iter().enumerate() {
                                            if let Type::Named(named) = arg
                                                && i < type_args.len()
                                            {
                                                scope.trait_ctx.type_params.insert(
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
                                        let type_param_id = scope.type_table.borrow_mut().intern(
                                            ResolvedType::TypeParam {
                                                name: type_param.name.clone(),
                                                index,
                                            },
                                        );
                                        scope.trait_ctx.type_params.insert(
                                            type_param.name.clone(),
                                            (index, type_param_id),
                                        );
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
                                    let param_names: Vec<String> = method
                                        .params
                                        .iter()
                                        .filter(|p| p.name != "self")
                                        .map(|p| p.name.clone())
                                        .collect();

                                    drop(scope);

                                    return Some(MethodInfo {
                                        return_type,
                                        self_kind,
                                        param_types,
                                        param_is_mut,
                                        inherited_from_base: None,
                                        cm_name: None,
                                        is_ref_impl: false,
                                        method_type_param_ids: vec![],
                                        param_defaults,
                                        param_names,
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

            // Set up type params for generic resources (e.g., resource Stream<T>).
            // Inherited scope; only `type_params` is replaced.
            let mut scope = self.enter_inherited_type_param_scope();
            scope.trait_ctx.type_params.clear();
            if let Some(type_args) = receiver_type_args {
                for (i, param) in resource.type_params.iter().enumerate() {
                    if i < type_args.len() {
                        scope
                            .trait_ctx
                            .type_params
                            .insert(param.name.clone(), (i as u32, type_args[i]));
                    }
                }
            }

            let return_type = method
                .return_type
                .as_ref()
                .map(|t| scope.resolve_type(t))
                .unwrap_or(TypeTable::UNIT);
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
            let param_names: Vec<String> = method
                .params
                .iter()
                .filter(|p| p.name != "self")
                .map(|p| p.name.clone())
                .collect();

            drop(scope);

            // Extract CM canonical name from #[cm("...")] attribute
            let cm_name = method
                .attrs
                .iter()
                .find(|a| a.name == "cm")
                .and_then(|a| a.args.first())
                .map(|a| a.as_str().to_string());

            return Some(MethodInfo {
                return_type,
                self_kind: ast::SelfKind::Ref,
                param_types,
                param_is_mut,
                inherited_from_base: None,
                cm_name,
                is_ref_impl: false,
                method_type_param_ids: vec![],
                param_defaults,
                param_names,
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
    ) -> Option<(
        ast::SelfKind,
        Vec<TypeId>,
        Vec<bool>,
        Vec<Option<ast::Expr>>,
        Vec<String>,
    )> {
        // First collect method info without resolving types. We also capture the
        // impl block's type AST and the method's type params so that param-type
        // resolution can run with both impl- and method-level type params in scope
        // (otherwise `T` resolves to `UNKNOWN` and downstream typecheck loses the
        // ability to detect generic-arg conflicts after inference).
        let mut found_method: Option<(
            ast::SelfKind,
            Vec<ast::Type>,
            Vec<bool>,
            Vec<Option<ast::Expr>>,
            Vec<String>,
            ast::Type,
            Vec<ast::GenericParam>,
        )> = None;

        for item in self.current_module_items {
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
                            let param_defaults: Vec<Option<ast::Expr>> =
                                non_self.iter().map(|p| p.default.clone()).collect();
                            let param_names: Vec<String> =
                                non_self.iter().map(|p| p.name.clone()).collect();
                            found_method = Some((
                                self_kind,
                                param_types,
                                param_is_mut,
                                param_defaults,
                                param_names,
                                impl_block.ty.clone(),
                                method.type_params.clone(),
                            ));
                            break;
                        }
                    }
                }
            }
            if found_method.is_some() {
                break;
            }
        }

        // Now resolve the types (needs mutable borrow). Set up impl-level and
        // method-level type params in scope so that references to `T` resolve
        // to `TypeParam` rather than `UNKNOWN`.
        found_method.map(
            |(
                self_kind,
                param_types_ast,
                param_is_mut,
                param_defaults,
                param_names,
                impl_ty,
                method_type_params,
            )| {
                // Inherited scope; only `type_params` is replaced.
                let mut scope = self.enter_inherited_type_param_scope();
                scope.trait_ctx.type_params.clear();

                // Impl-level type params (e.g. `impl Box<T>` -> register T at index 0)
                let mut impl_offset = 0u32;
                if let ast::Type::Generic(generic) = &impl_ty {
                    for (i, arg) in generic.args.iter().enumerate() {
                        if let ast::Type::Named(named) = arg
                            && !scope.trait_ctx.type_params.contains_key(&named.name)
                        {
                            let type_id = scope
                                .type_table
                                .borrow_mut()
                                .make_type_param(named.name.clone(), i as u32);
                            scope
                                .trait_ctx
                                .type_params
                                .insert(named.name.clone(), (i as u32, type_id));
                            impl_offset = (i as u32) + 1;
                        }
                    }
                }

                // Method-level type params (e.g. `fn make<T>(x: T)` -> register T)
                for (i, tp) in method_type_params
                    .iter()
                    .filter(|p| !p.is_effect)
                    .enumerate()
                {
                    if scope.trait_ctx.type_params.contains_key(&tp.name) {
                        continue;
                    }
                    let idx = impl_offset + i as u32;
                    let type_id = if tp.is_pack {
                        scope
                            .type_table
                            .borrow_mut()
                            .make_type_pack(tp.name.clone(), idx)
                    } else {
                        scope
                            .type_table
                            .borrow_mut()
                            .make_type_param(tp.name.clone(), idx)
                    };
                    scope
                        .trait_ctx
                        .type_params
                        .insert(tp.name.clone(), (idx, type_id));
                }

                let param_types: Vec<TypeId> = param_types_ast
                    .iter()
                    .map(|ty| scope.resolve_type(ty))
                    .collect();

                drop(scope);

                (
                    self_kind,
                    param_types,
                    param_is_mut,
                    param_defaults,
                    param_names,
                )
            },
        )
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

            // Generic instance (e.g., Option<T>, Array<T>): substitute in type args
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute_newtype_in_type(arg, base_type, newtype))
                    .collect();
                if new_args == type_args {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::GenericInstance {
                            name,
                            module_source,
                            type_args: new_args,
                        })
                }
            }

            // Other types: no substitution
            _ => type_id,
        }
    }

    /// Infer method-level type arguments for an instance method call using
    /// the method's already-resolved parameter and return types.
    ///
    /// `expected_param_types` and `decl_return_type` must come from a method
    /// lookup (`lookup_method_info` / `find_trait_method_for_type`) so that
    /// any `TypeParam` ids they contain already use the same indexing
    /// convention as downstream substitution via
    /// `SubstitutionContext::with_method_args(args, impl_offset)`.
    ///
    /// This intentionally does **not** re-resolve the method's AST: doing so
    /// in a fresh scope would emit spurious errors for references like
    /// `Self::Item` that depend on assoc-type bindings the outer resolver
    /// context has but that `infer_method_type_args` cannot easily
    /// reconstruct.
    ///
    /// Returns a vector sized to the method's own non-effect type parameters
    /// in declaration order. Unbound parameters fall back to their original
    /// `TypeParam` ids; an empty vector is returned when there is nothing to
    /// infer (no method type params, or the receiver is not a struct /
    /// generic instance).
    pub(super) fn infer_method_type_args(
        &mut self,
        input: MethodInferenceInput<'_>,
    ) -> Vec<TypeId> {
        let MethodInferenceInput {
            receiver_type,
            method_name,
            impl_offset,
            param_types,
            args,
            raw_args,
            decl_return_type,
            expected_return_type,
        } = input;

        let base_type_id = self.get_base_type(receiver_type);
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        // Locate the method's AST just to recover the list of type parameter
        // names (excluding effect params). We use these names together with
        // `impl_offset` to materialise the `TypeParam` ids the solver needs
        // to track, without re-resolving the method signature.
        let method_type_param_names = match &base_type {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            }
            | ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => self.find_method_type_param_names(name, Some(module_source), method_name),
            ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } => self
                .trait_ctx
                .type_param_bounds
                .get(name)
                .cloned()
                .and_then(|bounds| {
                    self.find_method_type_param_names_in_trait_bounds(
                        &bounds.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
                        method_name,
                    )
                }),
            ResolvedType::AssocTypeProjection { bounds, .. } => {
                self.find_method_type_param_names_in_trait_bounds(bounds, method_name)
            }
            _ => None,
        };
        let Some(method_type_param_names) = method_type_param_names else {
            return vec![];
        };

        let method_type_param_ids: Vec<TypeId> = {
            let mut tt = self.type_table.borrow_mut();
            method_type_param_names
                .into_iter()
                .enumerate()
                .map(|(i, name)| tt.make_type_param(name, impl_offset + i as u32))
                .collect()
        };

        let mut infer = InferCtx::new(&self.type_table, method_type_param_ids);
        for (i, (&param_type, arg)) in param_types.iter().zip(args.iter()).enumerate() {
            if Self::is_literal_number_arg(raw_args.get(i)) {
                infer.add_deferred(param_type, arg.type_id);
            } else {
                infer.add(param_type, arg.type_id);
            }
        }
        if let Some(expected) = expected_return_type {
            infer.add_expected_return(decl_return_type, expected);
        }

        // Accept an inference result when each method type param resolved
        // either to a concrete type, or to an outer-scope `TypeParam` (one
        // already registered in `trait_ctx.type_params`). Outer-scope params
        // are fine because monomorphization's index-based substitution will
        // rewrite them alongside the surrounding generics. Anything else —
        // including a fresh method-level `TypeParam` that no outer binding
        // knows about — would leave a dangling id at the call site, so
        // drop the inference and let the caller fall back to `vec![]`.
        let scope_params: Vec<TypeId> = self
            .trait_ctx
            .type_params
            .values()
            .map(|&(_, tid)| tid)
            .collect();
        let (inferred, all_concrete) = infer.solve_with_phantoms();
        if !all_concrete {
            let all_outer = inferred.iter().all(|tid| scope_params.contains(tid));
            if !all_outer {
                return vec![];
            }
        }
        inferred
    }

    /// Find the non-effect method type parameter names by searching the
    /// declarations of the given trait bounds. Used when the receiver is a
    /// type parameter or an associated-type projection whose concrete type is
    /// unknown at inference time.
    fn find_method_type_param_names_in_trait_bounds(
        &self,
        trait_names: &[String],
        method_name: &str,
    ) -> Option<Vec<String>> {
        let extract_names = |method: &ast::Function| -> Option<Vec<String>> {
            let names: Vec<String> = method
                .type_params
                .iter()
                .filter(|p| !p.is_effect)
                .map(|tp| tp.name.clone())
                .collect();
            if names.is_empty() { None } else { Some(names) }
        };
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Trait(trait_decl) = item
                    && trait_names.iter().any(|n| n == &trait_decl.name)
                {
                    for trait_method in &trait_decl.methods {
                        if trait_method.name == method_name
                            && let Some(names) = extract_names(trait_method)
                        {
                            return Some(names);
                        }
                    }
                }
            }
        }
        for item in self.current_module_items {
            if let Item::Trait(trait_decl) = item
                && trait_names.iter().any(|n| n == &trait_decl.name)
            {
                for trait_method in &trait_decl.methods {
                    if trait_method.name == method_name
                        && let Some(names) = extract_names(trait_method)
                    {
                        return Some(names);
                    }
                }
            }
        }
        None
    }

    /// Find the non-effect type parameter names of an instance method, in
    /// declaration order, for use by `infer_method_type_args`.
    ///
    /// Searches in priority order:
    /// 1. Inherent impls on `struct_name` in the struct's own module.
    /// 2. Inherent impls on `struct_name` in any other loaded module.
    /// 3. Inherent impls on `struct_name` in the current module.
    /// 4. Trait default methods (in any loaded module) — these have no
    ///    enclosing impl block, so "impl type params" are empty and
    ///    `impl_offset` is already correct.
    ///
    /// Returns `None` when no matching method exists or when the matched
    /// method has no non-effect type parameters (nothing to infer).
    /// Returning just the names (rather than cloning the whole
    /// `ast::Function`) keeps this cheap, since the names are all the
    /// solver needs to materialise the method-level `TypeParam` ids.
    fn find_method_type_param_names(
        &self,
        struct_name: &str,
        struct_module_source: Option<&ModuleSource>,
        method_name: &str,
    ) -> Option<Vec<String>> {
        let extract_names = |method: &ast::Function| -> Option<Vec<String>> {
            let names: Vec<String> = method
                .type_params
                .iter()
                .filter(|p| !p.is_effect)
                .map(|tp| tp.name.clone())
                .collect();
            if names.is_empty() { None } else { Some(names) }
        };

        let impl_matches_struct = |impl_block: &ast::ImplBlock, include_trait: bool| -> bool {
            if !include_trait && impl_block.trait_type.is_some() {
                return false;
            }
            let impl_type_name = self.get_type_name(&impl_block.ty);
            let impl_base_name = impl_type_name.split('<').next().unwrap_or(&impl_type_name);
            impl_type_name == struct_name || impl_base_name == struct_name
        };

        let search_items = |items: &[Item], include_trait: bool| -> Option<Vec<String>> {
            for item in items {
                if let Item::Impl(impl_block) = item
                    && impl_matches_struct(impl_block, include_trait)
                {
                    for method in &impl_block.methods {
                        if method.name == method_name
                            && let Some(names) = extract_names(method)
                        {
                            return Some(names);
                        }
                    }
                }
            }
            None
        };

        if let Some(module_source) = struct_module_source
            && let Some(module) = self.loaded_modules.get(module_source)
            && let Some(names) = search_items(&module.items, false)
        {
            return Some(names);
        }

        for module in self.loaded_modules.values() {
            if let Some(names) = search_items(&module.items, false) {
                return Some(names);
            }
        }

        if let Some(names) = search_items(self.current_module_items, false) {
            return Some(names);
        }

        // Fallback: trait default methods. These have no enclosing impl, so
        // their "impl type params" are empty.
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Trait(trait_decl) = item {
                    for default_method in &trait_decl.methods {
                        if default_method.name == method_name
                            && default_method.body.is_some()
                            && let Some(names) = extract_names(default_method)
                        {
                            return Some(names);
                        }
                    }
                }
            }
        }

        // Fallback: trait impls on the struct. When the method is defined in
        // `impl Trait for Struct`, it still has its own type parameters
        // (e.g. `fn put<T: Display>(&mut self, v: &T)`), which the inference
        // solver needs to materialise.
        if let Some(module_source) = struct_module_source
            && let Some(module) = self.loaded_modules.get(module_source)
            && let Some(names) = search_items(&module.items, true)
        {
            return Some(names);
        }

        for module in self.loaded_modules.values() {
            if let Some(names) = search_items(&module.items, true) {
                return Some(names);
            }
        }

        if let Some(names) = search_items(self.current_module_items, true) {
            return Some(names);
        }

        // Fallback: trait method declarations without default body. These can
        // be called through a trait impl that reuses the declared type params.
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Trait(trait_decl) = item {
                    for trait_method in &trait_decl.methods {
                        if trait_method.name == method_name
                            && let Some(names) = extract_names(trait_method)
                        {
                            return Some(names);
                        }
                    }
                }
            }
        }

        None
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
    /// Adjust the receiver expression to match what the method's self parameter expects.
    ///
    /// When `is_ref_impl` is true, the method was found on a reference type impl
    /// (e.g., `impl Trait for &T`). In this case, Self is `&T`, so `&self` means `&&T`.
    /// The receiver (which is `&T`) needs an additional `&` wrapping.
    pub(super) fn adjust_receiver_for_self_kind(
        &mut self,
        receiver: TirExpr,
        self_kind: ast::SelfKind,
        is_ref_impl: bool,
        span: Span,
    ) -> TirExpr {
        if is_ref_impl {
            // For ref-type impls, Self is &T (or &mut T).
            // &self means &&T, &mut self means &mut &T.
            // The receiver is already &T, so we need to add an extra reference layer.
            return match self_kind {
                ast::SelfKind::Ref => {
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
                ast::SelfKind::MutRef => {
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
                ast::SelfKind::None => self.deref_to_value(receiver, span),
            };
        }

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
                loop {
                    match self.type_table.borrow().get(current).clone() {
                        ResolvedType::Newtype { base_type, .. } => {
                            let base_name = self.type_table.borrow().type_name(base_type);
                            names.push(base_name);
                            current = base_type;
                        }
                        ResolvedType::Flags { .. } => {
                            names.push("u32".to_string());
                            break;
                        }
                        _ => break,
                    }
                }
            }
            names
        };

        // Collect lightweight impl block references (avoiding deep clones).
        let mut impl_refs = self.collect_trait_impl_refs_multi(&names_to_check);

        // Blanket impl fallback: check `impl<T: Bound> Trait for T` where the receiver
        // type satisfies the bound.  e.g., `impl<I: Iterator> IntoIterator for I` matches
        // any concrete type that implements Iterator.
        for (module_src, item_idx) in &self.trait_env.blanket_impl_index {
            let module = &self.loaded_modules[module_src];
            if let Item::Impl(impl_block) = &module.items[*item_idx]
                && impl_block.trait_type.is_some()
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
                        impl_refs.push(ImplBlockRef::Loaded(module_src.clone(), *item_idx));
                    }
                }
            }
        }

        // Now process the collected impl blocks with mutable access.
        // Re-access impl block fields via index to avoid cloning.
        for impl_ref in &impl_refs {
            let impl_block = self.get_impl_block(impl_ref);
            let impl_struct_name = self.get_type_name(&impl_block.ty);
            // Accept if the type matches by name, or if it's a blanket impl type parameter.
            let is_blanket_type_param = matches!(&impl_block.ty, Type::Named(named) if !self.is_known_type_name(&named.name));
            if !names_to_check.contains(&impl_struct_name) && !is_blanket_type_param {
                continue;
            }

            // If this impl block is a blanket impl (its target type is one of its own type params
            // with bounds), verify the receiver satisfies those bounds. This prevents incorrectly
            // using e.g. `impl<I: Iterator> IntoIterator for I` for a TypeParam `I: IntoIterator`.
            {
                let impl_ty_name = Self::get_type_name_static(&impl_block.ty);
                let blanket_param = impl_block
                    .type_params
                    .iter()
                    .find(|tp| tp.name == impl_ty_name && !tp.bounds.is_empty());
                if let Some(param) = blanket_param {
                    let bounds_ok = param.bounds.iter().all(|bound| {
                        receiver_type_id
                            .is_some_and(|rt| self.type_implements_trait(rt, &bound.name))
                    });
                    if !bounds_ok {
                        continue;
                    }
                }
            }

            // Extract type param mappings from the impl block before mutating self.
            let impl_block = self.get_impl_block(impl_ref);
            // Track variadic type pack spreads: (pack_name, param_index)
            let mut variadic_pack_entry: Option<(String, u32)> = None;
            let type_param_entries: Vec<(String, u32)> =
                if let Type::Generic(generic) = &impl_block.ty {
                    generic
                        .args
                        .iter()
                        .enumerate()
                        .filter_map(|(i, arg)| {
                            if let Type::Named(named) = arg {
                                Some((named.name.clone(), i as u32))
                            } else {
                                None
                            }
                        })
                        .collect()
                } else if let Type::Reference(boxed) | Type::MutReference(boxed) = &impl_block.ty {
                    if let Type::Named(inner) = boxed.as_ref() {
                        // impl<T: Bound> Trait for &T / &mut T: T is at position 0
                        vec![(inner.name.clone(), 0u32)]
                    } else if let Type::Generic(generic) = boxed.as_ref() {
                        // impl<T> Trait for &Container<T>: extract type params from generic args
                        generic
                            .args
                            .iter()
                            .enumerate()
                            .filter_map(|(i, arg)| {
                                if let Type::Named(named) = arg {
                                    Some((named.name.clone(), i as u32))
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else if let Type::Tuple(elems) = &impl_block.ty {
                    // Handle variadic tuple impls: impl<..T> Trait for [..T]
                    // TypePackSpread elements map the pack name to its index.
                    let mut entries = Vec::new();
                    for (i, elem) in elems.iter().enumerate() {
                        match elem {
                            Type::TypePackSpread(name, _) => {
                                // Find the pack's index from the impl block's type_params
                                let pack_idx = impl_block
                                    .type_params
                                    .iter()
                                    .position(|tp| tp.name == *name)
                                    .map(|p| p as u32)
                                    .unwrap_or(i as u32);
                                variadic_pack_entry = Some((name.clone(), pack_idx));
                            }
                            Type::Named(named) => {
                                entries.push((named.name.clone(), i as u32));
                            }
                            _ => {}
                        }
                    }
                    entries
                } else {
                    Vec::new()
                };
            // Extract blanket type param info
            let blanket_name = if let Type::Named(named) = &impl_block.ty {
                Some(named.name.clone())
            } else {
                None
            };
            // Extract associated type bindings (lightweight: just name+type, not methods)
            let assoc_bindings: Vec<(String, ast::Type)> = impl_block
                .associated_types
                .iter()
                .map(|b| (b.name.clone(), b.ty.clone()))
                .collect();
            let impl_module_source = self.impl_block_module_source(impl_ref);

            // Save trait context for this impl block scope. We use an inherited
            // scope (saves the full ctx via clone) and then selectively clear
            // just type_params and assoc_type_bindings — other parts (bounds,
            // self_type, …) are kept from the parent for this lookup.
            let mut scope = self.enter_inherited_type_param_scope();
            scope.trait_ctx.type_params.clear();
            scope.trait_ctx.assoc_type_bindings.clear();

            // Set up type parameters for resolving generic associated types
            if let Some(type_args) = receiver_type_args {
                for (name, idx) in &type_param_entries {
                    let i = *idx as usize;
                    if i < type_args.len() {
                        scope
                            .trait_ctx
                            .type_params
                            .insert(name.clone(), (*idx, type_args[i]));
                    }
                }
                // For variadic pack params (..T in impl<..T> Trait for [..T]),
                // map the pack to a TypePack so that the method body can reference it.
                if let Some((pack_name, pack_idx)) = &variadic_pack_entry {
                    let pack_type = scope.type_table.borrow_mut().make_tuple(type_args.to_vec());
                    scope
                        .trait_ctx
                        .type_params
                        .insert(pack_name.clone(), (*pack_idx, pack_type));
                }
            }

            // For blanket impls where impl_ty is a free type parameter
            if let Some(ref name) = blanket_name
                && !scope.trait_ctx.type_params.contains_key(name)
                && !scope.is_known_type_name(name)
            {
                if let Some(recv_id) = receiver_type_id {
                    scope
                        .trait_ctx
                        .type_params
                        .insert(name.clone(), (0, recv_id));
                } else {
                    let type_id = scope
                        .type_table
                        .borrow_mut()
                        .make_type_param(name.clone(), 0);
                    scope
                        .trait_ctx
                        .type_params
                        .insert(name.clone(), (0, type_id));
                }
            }

            // Set up associated type bindings for resolving Self::* types
            for (name, ty) in &assoc_bindings {
                let type_id = scope.resolve_type(ty);
                scope
                    .trait_ctx
                    .assoc_type_bindings
                    .insert(name.clone(), type_id);
            }

            let blanket_type_param = if is_blanket_type_param {
                Some(impl_struct_name.clone())
            } else {
                None
            };

            // Detect blanket ref impls: `impl<T: Bound> Trait for &T` where the inner type
            // is a type parameter. These should NOT override base-type methods.
            let is_blanket_ref_impl = {
                let impl_block = scope.get_impl_block(impl_ref);
                match &impl_block.ty {
                    Type::Reference(inner) | Type::MutReference(inner) => {
                        if let Type::Named(named) = inner.as_ref() {
                            // Inner is a bare name — check if it's a type parameter
                            impl_block
                                .type_params
                                .iter()
                                .any(|tp| tp.name == named.name)
                        } else {
                            false
                        }
                    }
                    _ => false,
                }
            };

            // Extract method info from impl block before calling &mut self methods
            let impl_block = scope.get_impl_block(impl_ref);
            let method_data: Option<(
                Option<ast::Type>,
                ast::SelfKind,
                Vec<ast::Param>,
                Vec<ast::GenericParam>,
            )> = impl_block
                .methods
                .iter()
                .find(|m| m.name == method_name)
                .map(|m| {
                    (
                        m.return_type.clone(),
                        m.params
                            .first()
                            .map(|p| p.self_kind)
                            .unwrap_or(ast::SelfKind::None),
                        m.params.clone(),
                        m.type_params.clone(),
                    )
                });
            let trait_type_for_name = impl_block.trait_type.as_ref().unwrap().clone();

            let mut method_found = false;
            if let Some((return_type_ast, self_kind, params, method_type_params)) = method_data {
                let trait_name = scope.get_type_name_full(&trait_type_for_name);

                // Set up method-level type params (e.g., V in deserialize_any<V: Visitor>)
                let impl_offset = scope.trait_ctx.type_params.len() as u32;
                for (i, type_param) in method_type_params.iter().enumerate() {
                    let index = impl_offset + i as u32;
                    let type_param_id =
                        scope
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::TypeParam {
                                name: type_param.name.clone(),
                                index,
                            });
                    scope
                        .trait_ctx
                        .type_params
                        .insert(type_param.name.clone(), (index, type_param_id));
                    if !type_param.bounds.is_empty() {
                        scope
                            .trait_ctx
                            .type_param_bounds
                            .insert(type_param.name.clone(), type_param.bounds.clone());
                    }
                }

                let return_type = return_type_ast
                    .as_ref()
                    .map(|t| scope.resolve_type(t))
                    .unwrap_or(TypeTable::UNIT);

                // Extract param_types while method-level type params are still
                // in scope — otherwise `&T` in a parameter would not resolve to
                // the proper `TypeParam` id that inference expects.
                let param_types = scope.extract_param_types(&params);

                // Remove method-level type params from scope
                for type_param in &method_type_params {
                    scope.trait_ctx.type_params.shift_remove(&type_param.name);
                    scope
                        .trait_ctx
                        .type_param_bounds
                        .shift_remove(&type_param.name);
                }
                let param_is_mut: Vec<bool> = params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.is_mut)
                    .collect();
                let param_names: Vec<String> = params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.name.clone())
                    .collect();
                // Parameter defaults live on the trait declaration only (WEP
                // 2026-04-11). Pull them from the trait's method, keyed by
                // parameter name, instead of the impl's re-specified params.
                let trait_name_base = scope.get_type_name(&trait_type_for_name);
                let param_defaults: Vec<Option<ast::Expr>> = {
                    let trait_method_params: Option<Vec<ast::Param>> = scope
                        .find_trait_decl_methods(&trait_name_base)
                        .and_then(|methods| {
                            methods
                                .into_iter()
                                .find(|m| m.name == method_name)
                                .map(|m| m.params)
                        });
                    param_names
                        .iter()
                        .map(|name| {
                            trait_method_params.as_ref().and_then(|tp| {
                                tp.iter()
                                    .find(|p| &p.name == name)
                                    .and_then(|p| p.default.clone())
                            })
                        })
                        .collect()
                };
                found_traits.push(TraitMethodMatch {
                    trait_name,
                    method_info: MethodInfo {
                        return_type,
                        self_kind,
                        param_types,
                        param_is_mut,
                        inherited_from_base: None,
                        cm_name: None,
                        is_ref_impl: false,
                        method_type_param_ids: vec![],
                        param_defaults,
                        param_names,
                    },
                    impl_module_source: impl_module_source.clone(),
                    blanket_type_param: blanket_type_param.clone(),
                    impl_struct_name: impl_struct_name.clone(),
                    is_blanket_ref_impl,
                });
                method_found = true;
            }

            // If the method wasn't found in the impl block, check the trait
            // declaration for a default method with that name
            if !method_found {
                let trait_name_base = scope.get_type_name(&trait_type_for_name);
                let trait_name_str = scope.get_type_name_full(&trait_type_for_name);
                if let Some(trait_methods) = scope.find_trait_decl_methods(&trait_name_base) {
                    for default_method in &trait_methods {
                        if default_method.name == method_name && default_method.body.is_some() {
                            // Set up Self type so that `Self` in the default method's
                            // return type resolves to the concrete receiver type
                            let old_self_type = scope.trait_ctx.self_type;
                            if let Some(recv_id) = receiver_type_id {
                                scope.trait_ctx.self_type = Some(recv_id);
                            }

                            // Bind the trait's own type parameters to the impl's
                            // concrete trait args so that a default method's
                            // return/param types written in terms of the trait's
                            // `T` resolve to the concrete type at the call site
                            // (e.g., `Maker<i32>::make_with_default` returns i32,
                            // not the unresolved T).
                            scope.bind_trait_type_params_from_impl(&trait_type_for_name);

                            // Set up method-level type params (e.g., U in map<U>)
                            let impl_offset = scope.trait_ctx.type_params.len() as u32;
                            for (i, type_param) in default_method.type_params.iter().enumerate() {
                                let index = impl_offset + i as u32;
                                let type_param_id =
                                    scope
                                        .type_table
                                        .borrow_mut()
                                        .intern(ResolvedType::TypeParam {
                                            name: type_param.name.clone(),
                                            index,
                                        });
                                scope
                                    .trait_ctx
                                    .type_params
                                    .insert(type_param.name.clone(), (index, type_param_id));
                                if !type_param.bounds.is_empty() {
                                    scope
                                        .trait_ctx
                                        .type_param_bounds
                                        .insert(type_param.name.clone(), type_param.bounds.clone());
                                }
                            }

                            let return_type = default_method
                                .return_type
                                .as_ref()
                                .map(|t| scope.resolve_type(t))
                                .unwrap_or(TypeTable::UNIT);
                            let self_kind = default_method
                                .params
                                .first()
                                .map(|p| p.self_kind)
                                .unwrap_or(ast::SelfKind::None);
                            let param_types = scope.extract_param_types(&default_method.params);
                            let param_is_mut: Vec<bool> = default_method
                                .params
                                .iter()
                                .filter(|p| p.name != "self")
                                .map(|p| p.is_mut)
                                .collect();
                            let param_defaults: Vec<Option<ast::Expr>> = default_method
                                .params
                                .iter()
                                .filter(|p| p.name != "self")
                                .map(|p| p.default.clone())
                                .collect();
                            let param_names: Vec<String> = default_method
                                .params
                                .iter()
                                .filter(|p| p.name != "self")
                                .map(|p| p.name.clone())
                                .collect();

                            // Remove method-level type params from scope
                            for type_param in &default_method.type_params {
                                scope.trait_ctx.type_params.shift_remove(&type_param.name);
                                scope
                                    .trait_ctx
                                    .type_param_bounds
                                    .shift_remove(&type_param.name);
                            }
                            scope.trait_ctx.self_type = old_self_type;

                            found_traits.push(TraitMethodMatch {
                                trait_name: trait_name_str.clone(),
                                method_info: MethodInfo {
                                    return_type,
                                    self_kind,
                                    param_types,
                                    param_is_mut,
                                    inherited_from_base: None,
                                    cm_name: None,
                                    is_ref_impl: false,
                                    method_type_param_ids: vec![],
                                    param_defaults,
                                    param_names,
                                },
                                impl_module_source: impl_module_source.clone(),
                                blanket_type_param: blanket_type_param.clone(),
                                impl_struct_name: impl_struct_name.clone(),
                                is_blanket_ref_impl,
                            });
                        }
                    }
                }
            }

            // Trait context is auto-restored on drop(scope).
            drop(scope);
        }

        // Prefer trait from the current module over cross-module traits.
        // Sort BEFORE dedup_by, since dedup_by only removes adjacent duplicates.
        let current_module = &self.current_module_source;
        found_traits.sort_by(|a, b| {
            let a_local = &a.impl_module_source == current_module;
            let b_local = &b.impl_module_source == current_module;
            b_local.cmp(&a_local)
        });

        // Remove duplicates (same trait from same module)
        found_traits.dedup_by(|a, b| {
            a.trait_name == b.trait_name && a.impl_module_source == b.impl_module_source
        });

        // Return the first one found (if there are multiple, it would be ambiguous,
        // but we'll handle that later with explicit disambiguation syntax)
        if let Some(m) = found_traits.into_iter().next() {
            return Some(m);
        }

        // Auto-derived Eq / Ord: no user-written impl exists, but the type
        // satisfies the field-wise / case-wise eligibility rules and
        // `synthesis::traits` will emit a body. Synthesize a `TraitMethodMatch`
        // so method-call resolution (and everything downstream of it) sees
        // the same view of "does this type have `.eq` / `.cmp`?" that
        // operator dispatch gets via `find_eq_trait_impl` / `find_ord_trait_impl`.
        if let Some(recv_id) = receiver_type_id {
            return self.try_auto_derived_method_match(struct_name, method_name, recv_id);
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
        self.find_indexing_trait_impl(struct_name, base_type_id, "Index", "index", "Output", None)
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
            None,
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
            None,
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

        let impl_refs = self.collect_trait_impl_refs(struct_name);

        for impl_ref in &impl_refs {
            let impl_block = self.get_impl_block(impl_ref);
            let trait_type = impl_block.trait_type.as_ref().unwrap();
            let trait_name = self.get_type_name(trait_type);
            if !trait_name.starts_with(trait_base_name) {
                continue;
            }
            let impl_block = self.get_impl_block(impl_ref);
            let binding_ty = match impl_block
                .associated_types
                .iter()
                .find(|b| b.name == assoc_name)
            {
                Some(b) => b.ty.clone(),
                None => continue,
            };
            let declared_type_params = self.build_declared_type_params(impl_block);
            let type_param_mapping = Self::build_type_param_mapping(
                &impl_block.ty,
                &concrete_type_args,
                &declared_type_params,
            );
            if !Self::verify_impl_type_compatibility(
                &impl_block.ty,
                &concrete_type_args,
                &declared_type_params,
                &self.type_table,
            ) {
                continue;
            }
            return Some(self.resolve_type_with_param_mapping(&binding_ty, &type_param_mapping));
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
                None,
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
                None,
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
            None,
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
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            "IndexMut",
            "index_mut",
            "Output",
            None,
        )
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
        index_type: TypeId,
    ) -> Option<IndexValueTraitInfo> {
        // Look for impl IndexValue<...> for StructName, matching the index type
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            "IndexValue",
            "index_value",
            "Output",
            Some(index_type),
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

    /// Find `Eq` trait implementation for a type.
    ///
    /// Falls back to auto-derived Eq for structs whose fields all implement Eq.
    pub(super) fn find_eq_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<ArithmeticTraitInfo> {
        if let Some(info) = self.find_arithmetic_trait_impl(struct_name, base_type_id, "Eq", "eq") {
            return Some(info);
        }
        // Auto-derive: check if all fields implement Eq
        if self.type_implements_trait(base_type_id, "Eq") {
            let ref_type = self
                .type_table
                .borrow_mut()
                .intern(ResolvedType::Ref(base_type_id));
            return Some(ArithmeticTraitInfo {
                output_type: TypeTable::BOOL,
                self_kind: ast::SelfKind::Ref,
                trait_name: "Eq".to_string(),
                rhs_type: Some(ref_type),
            });
        }
        None
    }

    /// Find `Ord` trait implementation for a type.
    ///
    /// Falls back to auto-derived Ord for structs whose fields all implement Ord.
    pub(super) fn find_ord_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<ArithmeticTraitInfo> {
        if let Some(info) = self.find_arithmetic_trait_impl(struct_name, base_type_id, "Ord", "cmp")
        {
            return Some(info);
        }
        // Auto-derive: check if all fields implement Ord
        if self.type_implements_trait(base_type_id, "Ord") {
            let ordering_type = self.type_table.borrow_mut().intern(ResolvedType::Enum {
                name: "Ordering".to_string(),
                module_source: ModuleSource::prelude(),
            });
            let ref_type = self
                .type_table
                .borrow_mut()
                .intern(ResolvedType::Ref(base_type_id));
            return Some(ArithmeticTraitInfo {
                output_type: ordering_type,
                self_kind: ast::SelfKind::Ref,
                trait_name: "Ord".to_string(),
                rhs_type: Some(ref_type),
            });
        }
        None
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

        let impl_refs = self.collect_trait_impl_refs(struct_name);

        for impl_ref in &impl_refs {
            let impl_block = self.get_impl_block(impl_ref);
            let impl_struct_name = self.get_type_name(&impl_block.ty);
            if impl_struct_name != struct_name {
                continue;
            }

            // Check if this is the target trait
            let impl_block = self.get_impl_block(impl_ref);
            let found_trait_name = self.get_type_name(impl_block.trait_type.as_ref().unwrap());
            if found_trait_name != trait_name {
                continue;
            }

            // Check trait bounds on type parameters (e.g., impl<T: Eq> Eq for Array<T>)
            let impl_block = self.get_impl_block(impl_ref);
            if !impl_block.type_params.iter().all(|p| p.bounds.is_empty())
                && !concrete_type_args.is_empty()
            {
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

                let mut bounds_satisfied = true;
                if let ast::Type::Generic(generic) = &impl_block.ty {
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
            let impl_block = self.get_impl_block(impl_ref);
            let mut type_param_mapping = Self::build_type_param_mapping(
                &impl_block.ty,
                &concrete_type_args,
                &IndexSet::default(),
            );
            // Map `Self` to the concrete base type so `&Self` parameters resolve correctly
            type_param_mapping.insert("Self".to_string(), base_type_id);

            // Find the method
            let impl_block = self.get_impl_block(impl_ref);
            if let Some(method) = impl_block.methods.iter().find(|m| m.name == method_name) {
                // Set up associated type bindings
                let mut assoc_type_map: IndexMap<String, TypeId> = IndexMap::default();

                // Process associated types (e.g., `type Output = Self`)
                // Clone just the associated type info we need (small, not methods)
                let assoc_types: Vec<(String, ast::Type)> = impl_block
                    .associated_types
                    .iter()
                    .map(|a| (a.name.clone(), a.ty.clone()))
                    .collect();
                let self_kind = method
                    .params
                    .first()
                    .map(|p| p.self_kind)
                    .unwrap_or(ast::SelfKind::None);
                let rhs_param_ty = method
                    .params
                    .iter()
                    .find(|p| p.self_kind == ast::SelfKind::None)
                    .map(|p| p.ty.clone());

                for (name, ty) in &assoc_types {
                    let resolved_type =
                        self.resolve_type_with_param_mapping(ty, &type_param_mapping);
                    assoc_type_map.insert(name.clone(), resolved_type);
                }

                // Get the output type from associated types
                let output_type = assoc_type_map
                    .get("Output")
                    .copied()
                    .unwrap_or(base_type_id);

                // Resolve the rhs parameter type (first non-self parameter)
                let rhs_type = rhs_param_ty
                    .as_ref()
                    .map(|ty| self.resolve_type_with_param_mapping(ty, &type_param_mapping));

                return Some(ArithmeticTraitInfo {
                    output_type,
                    self_kind,
                    trait_name: trait_name.to_string(),
                    rhs_type,
                });
            }
        }

        None
    }

    /// Look up the type parameters of a static method from its AST definition.
    /// Searches impl blocks in loaded modules for `impl StructName { fn method_name<...> }`.
    pub(super) fn lookup_static_method_type_params(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<ast::GenericParam> {
        // Search loaded modules
        for (_, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && Self::get_type_name_static(&impl_block.ty) == struct_name
                {
                    for method in &impl_block.methods {
                        if method.name == method_name && !method.type_params.is_empty() {
                            return method.type_params.clone();
                        }
                    }
                }
            }
        }
        // Search current module items
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                for method in &impl_block.methods {
                    if method.name == method_name && !method.type_params.is_empty() {
                        return method.type_params.clone();
                    }
                }
            }
        }
        vec![]
    }

    /// Look up the type parameters of a function from its AST definition.
    pub(super) fn lookup_function_type_params(
        &self,
        callee_module: &ModuleSource,
        func_name: &str,
    ) -> Vec<ast::GenericParam> {
        // Try local functions
        if callee_module.is_entry_point() {
            for item in self.current_module_items {
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
                name,
                module_source,
                type_args,
            } => {
                if TypeTable::is_tuple_type(&name, &module_source) {
                    let parts: Vec<String> = type_args
                        .iter()
                        .map(|&t| self.type_id_to_string(t))
                        .collect();
                    format!("[{}]", parts.join(", "))
                } else if type_args.is_empty() {
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
        expected_index_type: Option<TypeId>,
    ) -> Option<(TypeId, ast::SelfKind, String, crate::name::ModuleSource)> {
        // Check cache first (include expected_index_type in key)
        let cache_key = (
            struct_name.to_string(),
            base_type_id,
            trait_base_name.to_string(),
            method_name.to_string(),
            format!("{assoc_type_name}:{expected_index_type:?}"),
        );
        if let Some(cached) = self.indexing_trait_cache.get(&cache_key) {
            return cached.clone();
        }

        // Get concrete type arguments from the base type (for generic instances like Triple<i32>)
        let concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.type_table.borrow().get(base_type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };

        let impl_refs = self.collect_trait_impl_refs(struct_name);

        for impl_ref in &impl_refs {
            let impl_block = self.get_impl_block(impl_ref);
            let impl_struct_name = self.get_type_name(&impl_block.ty);
            if impl_struct_name != struct_name {
                continue;
            }

            // Check if this is the target trait (Index or IndexAssign)
            let impl_block = self.get_impl_block(impl_ref);
            let trait_base = self.get_type_name(impl_block.trait_type.as_ref().unwrap());
            if !trait_base.starts_with(trait_base_name) {
                continue;
            }
            let trait_name = self.get_type_name_full(impl_block.trait_type.as_ref().unwrap());

            let impl_block = self.get_impl_block(impl_ref);
            let declared_type_params = self.build_declared_type_params(impl_block);

            let impl_block = self.get_impl_block(impl_ref);
            let type_param_mapping = Self::build_type_param_mapping(
                &impl_block.ty,
                &concrete_type_args,
                &declared_type_params,
            );

            // If an expected index type is provided, check the trait's type argument matches.
            // e.g., for `impl IndexValue<RangeExclusive<i32>> for Array<T>`, the trait type arg
            // is `RangeExclusive<i32>` which must match the actual index expression type.
            if let Some(expected_idx_type) = expected_index_type {
                let impl_block = self.get_impl_block(impl_ref);
                let trait_index_arg = impl_block.trait_type.as_ref().and_then(|t| {
                    if let ast::Type::Generic(g) = t {
                        g.args.first().cloned()
                    } else {
                        None
                    }
                });
                if let Some(ref arg) = trait_index_arg {
                    let resolved_trait_idx =
                        self.resolve_type_with_param_mapping(arg, &type_param_mapping);
                    if resolved_trait_idx != expected_idx_type {
                        continue;
                    }
                }
            }

            // Verify non-type-parameter positions match the concrete type args
            let impl_block = self.get_impl_block(impl_ref);
            if !Self::verify_impl_type_compatibility(
                &impl_block.ty,
                &concrete_type_args,
                &declared_type_params,
                &self.type_table,
            ) {
                continue;
            }

            // Find the method
            let impl_block = self.get_impl_block(impl_ref);
            if let Some(method) = impl_block.methods.iter().find(|m| m.name == method_name) {
                let self_kind = method
                    .params
                    .first()
                    .map(|p| p.self_kind)
                    .unwrap_or(ast::SelfKind::None);
                // Clone just the lightweight associated type bindings (not methods)
                let assoc_bindings: Vec<(String, ast::Type)> = impl_block
                    .associated_types
                    .iter()
                    .map(|b| (b.name.clone(), b.ty.clone()))
                    .collect();
                let impl_source = self.impl_block_module_source(impl_ref);

                // Set up associated type bindings (auto-restored on scope drop)
                let mut scope = self.enter_inherited_type_param_scope();
                scope.trait_ctx.assoc_type_bindings.clear();
                for (name, ty) in &assoc_bindings {
                    let type_id = scope.resolve_type_with_param_mapping(ty, &type_param_mapping);
                    scope
                        .trait_ctx
                        .assoc_type_bindings
                        .insert(name.clone(), type_id);
                }

                // Get the associated type (Output or Input)
                let assoc_type = scope
                    .trait_ctx
                    .assoc_type_bindings
                    .get(assoc_type_name)
                    .copied()
                    .unwrap_or(TypeTable::UNKNOWN);

                drop(scope);

                let result = Some((assoc_type, self_kind, trait_name, impl_source));
                self.indexing_trait_cache.insert(cache_key, result.clone());
                return result;
            }
        }

        self.indexing_trait_cache.insert(cache_key, None);
        None
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

        let impl_refs = self.collect_trait_impl_refs(&struct_name);

        for impl_ref in &impl_refs {
            let impl_block = self.get_impl_block(impl_ref);
            let binding_ty = match impl_block
                .associated_types
                .iter()
                .find(|b| b.name == assoc_name)
            {
                Some(b) => b.ty.clone(),
                None => continue,
            };

            let declared_type_params = self.build_declared_type_params(impl_block);
            let type_param_mapping = Self::build_type_param_mapping(
                &impl_block.ty,
                &concrete_type_args,
                &declared_type_params,
            );

            if !Self::verify_impl_type_compatibility(
                &impl_block.ty,
                &concrete_type_args,
                &declared_type_params,
                &self.type_table,
            ) {
                continue;
            }

            return Some(self.resolve_type_with_param_mapping(&binding_ty, &type_param_mapping));
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
            cm_name: _,
            is_ref_impl: _,
            method_type_param_ids: _,
            param_defaults: _,
            param_names: _,
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
            false,
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
                        Some(index_mut_info.trait_name),
                        "index_mut".to_string(),
                    )),
                },
                type_args: vec![],
                args: vec![CallArg::new(index_resolved, false)],
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
            self.adjust_receiver_for_self_kind(index_mut_call, self_kind, false, method_call.span);

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
                },
                type_args,
                args: args
                    .into_iter()
                    .zip(
                        method_param_is_mut
                            .into_iter()
                            .chain(std::iter::repeat(false)),
                    )
                    .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                    .collect(),
            },
            return_type,
            method_call.span,
        ))
    }
}
