//! Type substitution and type rewriting for monomorphization.
//!
//! Contains `substitute_type` (replacing type params with concrete types),
//! `rewrite_type_id` (rewriting `GenericInstance` to concrete struct types),
//! and `rewrite_types_in_module` (applying rewrites across a module).

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::mangle_generic_name;
use crate::tir::{
    ResolvedType, TirExpr, TirExprKind, TirModule, TirPattern, TirStmt, TirStmtKind, TypeId,
    TypeTable,
};
use crate::tir_visitor::TirMutVisitor;

use super::state::Monomorphizer;

impl Monomorphizer {
    /// Rewrite all `GenericInstance` `type_ids` in expressions to use monomorphized struct types
    pub fn rewrite_types_in_module(&self, module: &mut TirModule) {
        let mut rewriter = TypeRewriter {
            mono: self,
            type_table: &mut module.type_table.borrow_mut(),
        };

        // Rewrite struct field types
        for strct in &mut module.structs {
            for field in &mut strct.fields {
                field.type_id = rewriter.rewrite_type_id(field.type_id);
            }
        }

        // Rewrite function signatures and bodies
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            Self::rewrite_types_in_function_inner(&mut rewriter, &mut func);
        }

        // Rewrite global variable initializers
        for global in &mut module.globals {
            global.ty = rewriter.rewrite_type_id(global.ty);
            rewriter.visit_expr(&mut global.initializer);
        }
    }

    /// Rewrite all `GenericInstance` `type_ids` in a single `TirFunction` —
    /// the per-function dual of [`Self::rewrite_types_in_module`].
    ///
    /// Phase 9's inner loop calls this on every newly-instantiated function
    /// (after struct monomorphisation has run to fixpoint over the types
    /// reachable from its body) so that, by the time
    /// `collect_function_instantiation_sites` walks the body, every
    /// `TypeId` it consumes — including `Call`/`MethodCall::type_args` —
    /// is in canonical `Struct` form. Without this, function-call queues
    /// produced by Phase 9 carry pre-monomorphisation `GenericInstance`
    /// `TypeId`s that mangle identically to their post-monomorphisation
    /// `Struct` counterparts, violating `function_id_for` injectivity
    /// over `project.functions`.
    pub fn rewrite_types_in_function(
        &self,
        func: &mut crate::tir::TirFunction,
        type_table: &mut TypeTable,
    ) {
        let mut rewriter = TypeRewriter {
            mono: self,
            type_table,
        };
        Self::rewrite_types_in_function_inner(&mut rewriter, func);
    }

    fn rewrite_types_in_function_inner(
        rewriter: &mut TypeRewriter<'_>,
        func: &mut crate::tir::TirFunction,
    ) {
        for param in &mut func.params {
            param.type_id = rewriter.rewrite_type_id(param.type_id);
        }
        func.return_type = rewriter.rewrite_type_id(func.return_type);
        for local in &mut func.locals {
            local.type_id = rewriter.rewrite_type_id(local.type_id);
        }
        if let Some(body) = &mut func.body {
            rewriter.visit_block(body);
        }
    }

    /// Rewrite a single `type_id`: if it's a `GenericInstance`, return the concrete struct `type_id`.
    /// Also handles container types (Array, Option, Tuple) that may contain `GenericInstance`.
    pub fn rewrite_type_id(&self, type_id: TypeId, type_table: &mut TypeTable) -> TypeId {
        // First check direct substitution
        if let Some(&new_id) = self.structs.type_substitutions.get(&type_id) {
            return new_id;
        }

        // Handle container types that may contain GenericInstance
        match type_table.get(type_id).clone() {
            ResolvedType::BuiltinArray(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.intern(ResolvedType::BuiltinArray(new_inner_id))
                }
            }
            ResolvedType::Ref(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.make_ref(new_inner_id)
                }
            }
            ResolvedType::MutRef(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.make_mut_ref(new_inner_id)
                }
            }
            // Handle GenericInstance types that weren't in the direct substitution map
            // This can happen when function substitution creates new GenericInstance types
            // with different TypeIds for the type arguments
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // Skip Array and Tuple - they have special codegen handling and should remain
                // as GenericInstance, not be rewritten to Struct
                if name == "Array" || TypeTable::is_tuple_type(&name, &module_source) {
                    let new_args: Vec<TypeId> = type_args
                        .iter()
                        .map(|&id| self.rewrite_type_id(id, type_table))
                        .collect();
                    return if new_args == type_args {
                        type_id
                    } else if TypeTable::is_tuple_type(&name, &module_source) {
                        type_table.make_tuple(new_args)
                    } else {
                        type_table.make_generic_instance(name, module_source, new_args)
                    };
                }

                // Build the mangled name using type names (not TypeIds).
                // Use `mangle_type_arg_for_generic` so the lookup key matches
                // what `instantiation_name` registered the struct under.
                let type_names: Vec<String> = type_args
                    .iter()
                    .map(|&arg| type_table.mangle_type_arg_for_generic(arg))
                    .collect();
                let mangled_name = mangle_generic_name(&name, &type_names);

                // Look for existing Struct with this mangled name via O(1) index
                if let Some(tid) = type_table.find_struct_by_name(&mangled_name, &module_source) {
                    return tid;
                }
                // If not found, return original type_id
                type_id
            }
            _ => type_id,
        }
    }

    /// Collect all `GenericInstance` types from the type table
    /// Only collects types whose base struct is in `valid_struct_names`
    pub fn collect_instantiation_sites(
        &mut self,
        type_table: &TypeTable,
        valid_struct_names: &IndexSet<String>,
    ) {
        for id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } = type_table.get(id)
            {
                // Skip empty type_args (invalid generic instances)
                if type_args.is_empty() {
                    continue;
                }

                // Skip Array - it has special codegen handling and should not be
                // monomorphized as a regular struct
                if name == "Array" {
                    continue;
                }

                // Only collect if the struct is in our valid set
                // This prevents library modules from trying to instantiate entry module's structs
                if !valid_struct_names.contains(name) {
                    continue;
                }

                // Only process if all type args are concrete (no TypeParams)
                let all_concrete = type_args
                    .iter()
                    .all(|&arg| !type_table.contains_type_param(arg));

                if all_concrete {
                    let key = crate::tir::InstantiationKey {
                        name: name.clone(),
                        module_source: module_source.clone(),
                        impl_type_args: type_args.clone(),
                        method_type_args: vec![],
                        method_info: None, // Struct instantiation,
                    };

                    let mangled = self.instantiation_name(&key, type_table);
                    self.try_queue_struct(key, mangled);
                }
            }
        }
    }

    /// Substitute type parameters in a type with concrete types.
    ///
    /// Walks `type_id` recursively, delegating the leaf cases (`TypeParam`,
    /// `TypePack`, `AssocTypeProjection`, `GenericResource`) to
    /// [`TypeTable::substitute_type_params`] and handling container /
    /// `GenericInstance` cases inline so that a monomorphize-specific
    /// struct-rewrite happens at every level: after recursively
    /// substituting a `GenericInstance`'s type arguments, if an
    /// already-monomorphized struct with the resulting mangled name exists
    /// in the type table, the type is rewritten to that struct directly.
    /// This keeps later phases (WIR build, variant-case registration)
    /// looking up the same mangled name as was registered at
    /// struct-instantiation time, including for nested references such as
    /// `Option<&mut TreeMapNode<K>>`.
    ///
    /// A `GenericInstance` whose `type_args` are empty — which can occur
    /// when e.g. `Container { .. }` inside `Container<T>::new()` is typed
    /// as `GenericInstance` without spelling out its type arguments — is
    /// resolved to an existing monomorphized struct using the substitution
    /// map itself.
    pub fn substitute_type(
        &self,
        type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type(elem, substitution, type_table);
                type_table.intern(ResolvedType::BuiltinArray(new_elem))
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_mut_ref(new_inner)
            }
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
                stores,
            } => {
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute_type(p, substitution, type_table))
                    .collect();
                let new_return_type = self.substitute_type(return_type, substitution, type_table);
                type_table.make_function_with_mut(
                    is_mut,
                    new_params,
                    new_return_type,
                    effects,
                    stores,
                )
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                if type_args.is_empty() {
                    if !substitution.is_empty() {
                        let mut indexed_args: Vec<(u32, TypeId)> =
                            substitution.iter().map(|(&idx, &tid)| (idx, tid)).collect();
                        indexed_args.sort_by_key(|(idx, _)| *idx);
                        let type_names: Vec<String> = indexed_args
                            .iter()
                            .map(|(_, arg_id)| type_table.mangle_type_arg_for_generic(*arg_id))
                            .collect();
                        let mangled_name = mangle_generic_name(&name, &type_names);
                        if let Some(tid) =
                            type_table.find_struct_by_name(&mangled_name, &module_source)
                        {
                            return tid;
                        }
                    }
                    if let Some(tid) = type_table.find_struct_by_name(&name, &module_source) {
                        return tid;
                    }
                    return type_id;
                }

                // Tuples need TypePack expansion: splice pack elements
                // into the type args. Non-pack args are substituted
                // recursively through `self.substitute_type` so that
                // nested `GenericInstance`s inside the tuple still get
                // their monomorphized-struct rewrite.
                if TypeTable::is_tuple_type(&name, &module_source) {
                    let mut new_elems: Vec<TypeId> = Vec::new();
                    for &e in &type_args {
                        match type_table.get(e).clone() {
                            ResolvedType::TypePack {
                                index,
                                name: pack_name,
                            } => {
                                let &pack_type =
                                    substitution.get(&index).unwrap_or_else(|| {
                                        panic!(
                                            "TypePack `..{pack_name}` (index {index}) not found in substitution map"
                                        )
                                    });
                                if let Some(pack_elems) = type_table.as_tuple(pack_type) {
                                    new_elems.extend_from_slice(&pack_elems);
                                } else {
                                    new_elems.push(pack_type);
                                }
                            }
                            _ => {
                                new_elems.push(self.substitute_type(e, substitution, type_table));
                            }
                        }
                    }
                    return type_table.make_tuple(new_elems);
                }

                // Recursively substitute nested type args so inner
                // GenericInstances get rewritten to their monomorphized
                // Struct forms when available.
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute_type(arg, substitution, type_table))
                    .collect();

                let type_names: Vec<String> = new_args
                    .iter()
                    .map(|&arg| type_table.mangle_type_arg_for_generic(arg))
                    .collect();
                let mangled_name = mangle_generic_name(&name, &type_names);
                if let Some(tid) = type_table.find_struct_by_name(&mangled_name, &module_source) {
                    return tid;
                }

                type_table.make_generic_instance(name, module_source, new_args)
            }
            // Delegate leaf cases (TypeParam, TypePack, GenericResource,
            // AssocTypeProjection, and other non-composite types) to the
            // shared implementation.
            _ => type_table.substitute_type_params(type_id, substitution),
        }
    }

    /// Substitute type parameters in a pattern
    pub fn substitute_types_in_pattern(
        &self,
        pattern: &mut crate::tir::TirPattern,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        use crate::tir::TirPattern;
        match pattern {
            TirPattern::Wildcard | TirPattern::Literal(_) | TirPattern::Range { .. } => {}
            TirPattern::Binding { type_id, .. } => {
                // Substitute the binding's type (e.g., type parameter T -> i32)
                *type_id = self.substitute_type(*type_id, substitution, type_table);
            }
            TirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.substitute_types_in_pattern(p, substitution, type_table);
                }
            }
            TirPattern::Variant {
                enum_type,
                bindings,
                payload_type,
                ..
            } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
                // Also substitute the payload type (e.g., type parameter U -> i32)
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
                for binding in bindings {
                    self.substitute_types_in_pattern(binding, substitution, type_table);
                }
            }
            TirPattern::Enum { enum_type, .. } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
            }
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);
                for field in fields {
                    self.substitute_types_in_pattern(&mut field.pattern, substitution, type_table);
                }
            }
            TirPattern::Or(alternatives) => {
                for p in alternatives {
                    self.substitute_types_in_pattern(p, substitution, type_table);
                }
            }
            TirPattern::ConstantValue { expr } => {
                expr.type_id = self.substitute_type(expr.type_id, substitution, type_table);
            }
        }
    }
}

/// Rewrites `GenericInstance` `type_ids` throughout a TIR tree to use concrete struct types.
/// Used by `rewrite_types_in_module`.
pub(super) struct TypeRewriter<'a> {
    pub mono: &'a Monomorphizer,
    pub type_table: &'a mut TypeTable,
}

impl TypeRewriter<'_> {
    pub fn rewrite_type_id(&mut self, type_id: TypeId) -> TypeId {
        self.mono.rewrite_type_id(type_id, self.type_table)
    }
}

impl TirMutVisitor for TypeRewriter<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        if let TirStmtKind::Let { type_id, .. } = &mut stmt.kind {
            *type_id = self.rewrite_type_id(*type_id);
        }
        self.walk_stmt(stmt);
    }

    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        match pattern {
            TirPattern::Variant { enum_type, .. } | TirPattern::Enum { enum_type, .. } => {
                *enum_type = self.rewrite_type_id(*enum_type);
            }
            TirPattern::Struct { struct_type, .. } => {
                *struct_type = self.rewrite_type_id(*struct_type);
            }
            _ => {}
        }
        self.walk_pattern(pattern);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        expr.type_id = self.rewrite_type_id(expr.type_id);

        // Rewrite explicit `type_args` on `Call`/`MethodCall` so post-monomorphisation
        // `InstantiationKey`s built from them use the canonical (substituted)
        // `Struct` form rather than the resolver-assigned `GenericInstance`
        // form. Without this, two queue paths for the same logical
        // instantiation — one driven by call-site `type_args` (left as
        // `GenericInstance` by the resolver) and another driven by
        // receiver/struct_key paths (already rewritten to `Struct`) —
        // would hash distinct yet mangle identically, producing two
        // `TirFunction`s sharing one `function_id_for`.
        //
        // The default `walk_expr` does not descend into `type_args` (see
        // `tir_visitor.rs`'s `Call`/`MethodCall` patterns, which only
        // bind `args`), so rewrite them explicitly here. `Call`'s own
        // body rewrites (param substitution, etc.) happen via
        // `substitute_types_in_expr` at `instantiate_function` time and
        // are independent of this whole-module rewrite.
        match &mut expr.kind {
            TirExprKind::Call { type_args, .. } | TirExprKind::MethodCall { type_args, .. } => {
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.rewrite_type_id(*type_arg);
                }
            }
            _ => {}
        }

        if let TirExprKind::StructLiteral {
            struct_type,
            struct_name,
            ..
        } = &mut expr.kind
        {
            let original_type_id = *struct_type;
            let new_type_id = self.rewrite_type_id(original_type_id);
            *struct_type = new_type_id;
            if let Some(mangled_name) = self
                .mono
                .structs
                .type_to_mangled_name
                .get(&original_type_id)
            {
                struct_name.clone_from(mangled_name);
            } else {
                match self.type_table.get(new_type_id) {
                    ResolvedType::Struct { name, .. } => {
                        struct_name.clone_from(name);
                    }
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        let type_names: Vec<String> = type_args
                            .iter()
                            .map(|&arg| self.type_table.mangle_type_arg_for_generic(arg))
                            .collect();
                        *struct_name = mangle_generic_name(name, &type_names);
                    }
                    _ => {}
                }
            }
        }

        self.walk_expr(expr);
    }
}
