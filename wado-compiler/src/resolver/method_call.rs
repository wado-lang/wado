//! Method call and static method call resolution.

use crate::ast::{self, Item};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName};
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, SubstitutionContext, TirExpr, TirExprKind,
    TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::callee::StaticMethodRef;
use super::method_lookup::MethodInferenceInput;
use super::types::{FunctionContext, MethodInfo, TypeError};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Check for IndexMut desugaring: container[i].method() where method needs &mut self
        // We need to detect this BEFORE resolving the receiver, because resolve_index
        // would otherwise generate Index::index instead of IndexMut::index_mut
        if let ast::Expr::Index(index_expr) = &method_call.receiver
            && let Some(result) =
                self.try_resolve_index_mut_method_call(index_expr, method_call, ctx)
        {
            return result;
        }

        let mut receiver = self.resolve_expr(&method_call.receiver, ctx, None);
        // NOTE: args are resolved later (after method lookup) to enable literal coercion
        // using the method's parameter types as expected types.

        // Resolve explicit type arguments (method-level type args)
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Get the base (non-ref) type for method lookup and struct name extraction
        let base_type_id = self.get_base_type(receiver.type_id);

        // Get struct name and module source from base type
        // The struct_module is where the struct is defined (and inherent methods live)
        let (struct_name, struct_module) = match self.type_table.borrow().get(base_type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            // Primitive types have impl blocks in core:prelude/primitive
            ResolvedType::Primitive(_) => (
                self.type_table.borrow().mangle_type_name(base_type_id),
                ModuleSource::primitive(),
            ),
            // Unit type () has impl blocks in core:prelude/primitive
            ResolvedType::Unit => (
                TypeTable::UNIT_TYPE_NAME.to_string(),
                ModuleSource::primitive(),
            ),
            // Enum types - use enum name and its defining module
            ResolvedType::Enum {
                name,
                module_source,
            } => (name.clone(), module_source.clone()),
            // Generic resource types (Future<T>, Stream<T>, etc.) - use resource name and module
            ResolvedType::GenericResource {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            // Newtype/Flags - use the type's own name and defining module
            ResolvedType::Newtype {
                name,
                module_source,
                ..
            }
            | ResolvedType::Flags {
                name,
                module_source,
            } => (name.clone(), module_source.clone()),
            _ => (
                self.type_table.borrow().mangle_type_name(base_type_id),
                self.current_module_source.clone(),
            ),
        };

        // Extract receiver type args for generic types (used for resolving associated types)
        let receiver_type_args_for_trait: Option<Vec<TypeId>> =
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance { type_args, .. } if !type_args.is_empty() => {
                    Some(type_args)
                }
                _ => None,
            };

        let mut method_info: Option<MethodInfo> = None;
        let mut trait_name: Option<String> = None;
        let mut trait_impl_module_source: Option<ModuleSource> = None;
        let mut blanket_type_param: Option<String> = None;
        let mut trait_impl_struct_name: Option<String> = None;

        // If receiver is a reference type, try ref-type trait impls first.
        // e.g., impl IntoIterator for &Array<T> takes priority over impl IntoIterator for Array<T>.
        // Only specific ref impls are preferred (not blanket impls like impl Inspect for &T).
        {
            let is_ref = matches!(
                self.type_table.borrow().get(receiver.type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            );
            if is_ref {
                let ref_struct_name = if matches!(
                    self.type_table.borrow().get(receiver.type_id),
                    ResolvedType::Ref(_)
                ) {
                    "&"
                } else {
                    "&mut"
                };
                let result = self.find_trait_method_for_type(
                    ref_struct_name,
                    &method_call.method,
                    &struct_module,
                    receiver_type_args_for_trait.as_deref(),
                    Some(base_type_id),
                );
                // Only use ref-type impls that target a concrete container type
                // (e.g., impl IntoIterator for &Array<T>), NOT blanket ref impls
                // (e.g., impl Inspect for &T where the inner type is just a type param).
                if let Some(trait_match) = result
                    && !trait_match.is_blanket_ref_impl
                {
                    trait_impl_struct_name = Some(trait_match.impl_struct_name);
                    trait_name = Some(trait_match.trait_name);
                    let mut info = trait_match.method_info;
                    info.is_ref_impl = true;
                    method_info = Some(info);
                    trait_impl_module_source = Some(trait_match.impl_module_source);
                    blanket_type_param = trait_match.blanket_type_param;
                }
            }
        }

        // Look up method info based on receiver type (inherent + base type trait methods)
        if method_info.is_none() {
            method_info = self.lookup_method_info(receiver.type_id, &method_call.method);
        }

        // Fall back to base type trait methods
        if method_info.is_none()
            && let Some(trait_match) = self.find_trait_method_for_type(
                &struct_name,
                &method_call.method,
                &struct_module,
                receiver_type_args_for_trait.as_deref(),
                Some(base_type_id),
            )
        {
            if trait_match.impl_struct_name != struct_name {
                trait_impl_struct_name = Some(trait_match.impl_struct_name);
            }
            trait_name = Some(trait_match.trait_name);
            method_info = Some(trait_match.method_info);
            trait_impl_module_source = Some(trait_match.impl_module_source);
            blanket_type_param = trait_match.blanket_type_param;
        }

        // If still not found and receiver is a TypeParam, try trait bounds
        // e.g., T: Ord -> look up cmp() in Ord trait declaration
        if method_info.is_none() {
            let type_param_name = {
                let resolved = self.type_table.borrow().get(base_type_id).clone();
                if let ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } =
                    resolved
                {
                    Some(name)
                } else {
                    None
                }
            };
            if let Some(name) = type_param_name
                && let Some(bounds) = self.trait_ctx.type_param_bounds.get(&name).cloned()
                && let Some((found_trait, info)) = {
                    let bound_names: Vec<String> = bounds.iter().map(|b| b.name.clone()).collect();
                    self.find_method_in_trait_bounds(
                        &bound_names,
                        &method_call.method,
                        base_type_id,
                    )
                }
            {
                trait_name = Some(found_trait);
                method_info = Some(info);
            }
        }

        // If still not found and receiver is an AssocTypeProjection, try its bounds
        // e.g., S::SeqSerializer: SerializeSeq -> look up element() in SerializeSeq
        if method_info.is_none() {
            let assoc_bounds = {
                let resolved = self.type_table.borrow().get(base_type_id).clone();
                if let ResolvedType::AssocTypeProjection { bounds, .. } = resolved {
                    if bounds.is_empty() {
                        None
                    } else {
                        Some(bounds)
                    }
                } else {
                    None
                }
            };
            if let Some(bounds) = assoc_bounds
                && let Some((found_trait, info)) =
                    self.find_method_in_trait_bounds(&bounds, &method_call.method, base_type_id)
            {
                trait_name = Some(found_trait);
                method_info = Some(info);
            }
        }

        // Get method info (error if method not found)
        let MethodInfo {
            mut return_type,
            self_kind,
            param_types,
            param_is_mut: _,
            inherited_from_base,
            cm_name,
            is_ref_impl,
            method_type_param_ids: _,
            param_defaults,
            param_names,
        } = if let Some(info) = method_info {
            info
        } else {
            let type_name = self.type_table.borrow().type_name(base_type_id);
            let _ = self.logger.error(TypeError::MethodNotFound {
                type_name,
                method_name: method_call.method.clone(),
                hint: String::new(),
                span: method_call.span,
            });
            // Default to Unknown type for error recovery
            MethodInfo {
                return_type: TypeTable::UNKNOWN,
                self_kind: ast::SelfKind::Ref,
                param_types: vec![],
                param_is_mut: vec![],
                inherited_from_base: None,
                cm_name: None,
                is_ref_impl: false,
                method_type_param_ids: vec![],
                param_defaults: vec![],
                param_names: vec![],
            }
        };

        // Tuple.len() is a compile-time constant — return immediately without a function call.
        if method_call.method == "len" && self.type_table.borrow().is_tuple(base_type_id) {
            let len = self
                .type_table
                .borrow()
                .as_tuple(base_type_id)
                .unwrap()
                .len() as i64;
            return TirExpr::new(
                TirExprKind::IntLiteral {
                    value: len as u64,
                    repr: len.to_string(),
                },
                TypeTable::I32,
                method_call.span,
            );
        }

        // Tuple.zip() transposes a tuple-of-tuples.
        // [[A0, A1], [B0, B1]].zip() → [[A0, B0], [A1, B1]]
        if method_call.method == "zip" && self.type_table.borrow().is_tuple(base_type_id) {
            let has_type_pack = self.type_contains_pack(base_type_id);
            if has_type_pack {
                // TypePack present: defer expansion to monomorphization.
                return TirExpr::new(
                    TirExprKind::TupleZip {
                        expr: Box::new(receiver),
                    },
                    return_type,
                    method_call.span,
                );
            }
            // Concrete tuples: expand inline now.
            let outer_elems = self.type_table.borrow().as_tuple(base_type_id).unwrap();
            let inner_arities: Vec<Vec<TypeId>> = outer_elems
                .iter()
                .map(|e| self.type_table.borrow().as_tuple(*e).unwrap())
                .collect();
            let arity = inner_arities[0].len();
            let num_rows = outer_elems.len();
            let mut col_exprs = Vec::with_capacity(arity);
            for col in 0..arity {
                let mut row_exprs = Vec::with_capacity(num_rows);
                for (row, row_types) in inner_arities.iter().enumerate() {
                    let row_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(receiver.clone()),
                            field_index: row as u32,
                            field_name: row.to_string(),
                        },
                        outer_elems[row],
                        method_call.span,
                    );
                    let cell = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(row_access),
                            field_index: col as u32,
                            field_name: col.to_string(),
                        },
                        row_types[col],
                        method_call.span,
                    );
                    row_exprs.push(cell);
                }
                let col_types: Vec<TypeId> = inner_arities.iter().map(|row| row[col]).collect();
                let col_tuple_type = self.type_table.borrow_mut().make_tuple(col_types);
                col_exprs.push(TirExpr::new(
                    TirExprKind::TupleLiteral {
                        elements: row_exprs,
                    },
                    col_tuple_type,
                    method_call.span,
                ));
            }
            return TirExpr::new(
                TirExprKind::TupleLiteral {
                    elements: col_exprs,
                },
                return_type,
                method_call.span,
            );
        }

        // Static methods (no self parameter) cannot be called with instance method syntax.
        // e.g., `obj.static_method()` should be `Type::static_method()` instead.
        if self_kind == ast::SelfKind::None {
            let type_name = self.type_table.borrow().type_name(base_type_id);
            let _ = self.logger.error(TypeError::MethodNotFound {
                type_name: type_name.clone(),
                method_name: method_call.method.clone(),
                hint: format!(
                    "'{}' is a static method; use {}::{}() instead",
                    method_call.method, type_name, method_call.method
                ),
                span: method_call.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, method_call.span);
        }

        // Type check method arguments against expected parameter types (newtype-aware)
        // If method was inherited from a newtype's base type, substitute base->newtype in params
        let expected_param_types: Vec<TypeId> = if let Some(base_type_id) = inherited_from_base {
            // Get the newtype that the method is being called on
            let newtype_id = self.get_base_type(receiver.type_id);
            // Substitute base type with newtype in all parameter types
            param_types
                .iter()
                .map(|&ty| self.substitute_newtype_in_type(ty, base_type_id, newtype_id))
                .collect()
        } else {
            param_types
        };

        // Resolve arguments with coercion using method parameter types
        let mut args: Vec<TirExpr> = method_call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = expected_param_types.get(i).copied();
                self.resolve_expr(arg, ctx, expected_type)
            })
            .collect();

        // Pad missing trailing args with declared parameter defaults.
        // Earlier-parameter references inside a default (e.g. `fn f(w, h = w)`)
        // are handled by substituting the caller's arg ASTs for those parameter
        // names before resolving, mirroring the free-function path in
        // `pad_args_with_defaults`.
        if args.len() < expected_param_types.len() && !param_defaults.is_empty() {
            let mut subs: crate::hashmap::IndexMap<String, ast::Expr> =
                crate::hashmap::IndexMap::default();
            for (i, arg_ast) in method_call.args.iter().enumerate() {
                if let Some(name) = param_names.get(i) {
                    subs.insert(name.clone(), arg_ast.clone());
                }
            }
            for i in args.len()..expected_param_types.len() {
                let Some(Some(default_ast)) = param_defaults.get(i) else {
                    break;
                };
                let expected_type = expected_param_types[i];
                let mut default_expr = default_ast.clone();
                default_expr.substitute_idents(&subs);
                let resolved = self.resolve_expr(&default_expr, ctx, Some(expected_type));
                args.push(resolved);
                if let Some(name) = param_names.get(i) {
                    subs.insert(name.clone(), default_expr);
                }
            }
        }

        // Check each argument against expected parameter type
        for (i, (arg, &expected_type)) in args.iter().zip(expected_param_types.iter()).enumerate() {
            let span = method_call
                .args
                .get(i)
                .map_or(method_call.span, super::ast::Expr::span);
            self.typecheck(arg.type_id, expected_type, span);
        }

        // Substitute return type for inherited newtype methods
        // e.g., Point::clone_point() -> Point becomes Location::clone_point() -> Location
        if let Some(base_type_id) = inherited_from_base {
            let newtype_id = self.get_base_type(receiver.type_id);
            return_type = self.substitute_newtype_in_type(return_type, base_type_id, newtype_id);
        }

        // Track implicit `&mut self` borrowing for primitive local receivers.
        // Primitive values are copied by default in Wasm GC, so `x.bump()`
        // must mark `x` as address-taken to preserve mutation semantics.
        let needs_implicit_mut_borrow_on_primitive_local = !is_ref_impl
            && matches!(self_kind, ast::SelfKind::MutRef)
            && !matches!(
                self.type_table.borrow().get(receiver.type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
            && matches!(
                self.type_table
                    .borrow()
                    .get(self.get_base_type(receiver.type_id)),
                ResolvedType::Primitive(_)
            );
        if needs_implicit_mut_borrow_on_primitive_local
            && let TirExprKind::Local { index, .. } = &receiver.kind
        {
            ctx.address_taken_locals.insert(*index);
        }

        // Adjust receiver based on what the method expects (self_kind)
        receiver =
            self.adjust_receiver_for_self_kind(receiver, self_kind, is_ref_impl, method_call.span);

        // Build unified substitution context for double generics
        // Type param indices are assigned as follows:
        // - Impl type params (from struct): 0, 1, 2, ...
        // - Method type params: offset, offset+1, ... (where offset = impl_type_params.len())
        let mut subst_ctx = SubstitutionContext::new();
        let mut impl_offset = 0u32;

        // First, add impl-level type args from receiver's generic type (use base type)
        // IMPORTANT: Skip this for trait methods because find_trait_method_for_type already
        // resolved the return type using associated type bindings. Adding impl_args here would
        // incorrectly substitute TypeParams from the OUTER context (e.g., TreeMap's K, V) that
        // happen to have the same indices as this impl's type params (e.g., Array's T).
        if trait_name.is_none() {
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance {
                    type_args: receiver_type_args,
                    ..
                }
                | ResolvedType::GenericResource {
                    type_args: receiver_type_args,
                    ..
                } if !receiver_type_args.is_empty() => {
                    impl_offset = receiver_type_args.len() as u32;
                    subst_ctx = subst_ctx.with_impl_args(&receiver_type_args);
                }
                _ => {}
            }
        } else {
            // For trait methods, just compute impl_offset for method type args
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance { type_args, .. }
                | ResolvedType::GenericResource { type_args, .. }
                    if !type_args.is_empty() =>
                {
                    impl_offset = type_args.len() as u32;
                }
                _ => {}
            }
        }

        // Then add method-level type args with the correct offset
        // If no explicit type args, try to infer from arguments
        let method_type_args = if type_args.is_empty() {
            // Try to infer method type args from actual arguments and expected return type
            self.infer_method_type_args(MethodInferenceInput {
                receiver_type: receiver.type_id,
                method_name: &method_call.method,
                impl_offset,
                param_types: &expected_param_types,
                args: &args,
                raw_args: &method_call.args,
                decl_return_type: return_type,
                expected_return_type: expected_type,
            })
        } else {
            type_args
        };

        if !method_type_args.is_empty() {
            subst_ctx = subst_ctx.with_method_args(&method_type_args, impl_offset);
        }

        // Apply unified substitution
        if !subst_ctx.is_empty() {
            return_type = subst_ctx.substitute(return_type, &mut self.type_table.borrow_mut());
        }

        // Re-coerce literal-number args and typecheck each arg against the substituted
        // parameter type. This catches inference conflicts such as
        // `h.two_method<T>(1 as i64, 2 as i32)` where `T` cannot be both `i64` and `i32`.
        // The pre-inference typecheck at line ~380 only sees TypeParam (a wildcard),
        // so the conflict must be caught after substitution.
        if !method_type_args.is_empty() {
            let substituted_param_types: Vec<TypeId> = expected_param_types
                .iter()
                .map(|&t| subst_ctx.substitute(t, &mut self.type_table.borrow_mut()))
                .collect();
            self.recoerce_literal_args(&method_call.args, &mut args, &substituted_param_types);
            for (i, arg) in args.iter().enumerate() {
                if let Some(&expected) = substituted_param_types.get(i) {
                    self.typecheck(
                        arg.type_id,
                        expected,
                        method_call
                            .args
                            .get(i)
                            .map_or(method_call.span, super::ast::Expr::span),
                    );
                }
            }
        }

        // Get struct name and monomorph info from base type for mangled method name.
        // For inherited methods (Newtype/Flags), use the actual implementation type's name,
        // since the function is defined on the base type (e.g., Point::sum, not Location::sum).
        let method_impl_type_id = inherited_from_base.unwrap_or(base_type_id);
        let (
            mut receiver_struct_name,
            mut base_struct_name,
            impl_type_arg_names,
            receiver_type_args,
        ) = match self.type_table.borrow().get(method_impl_type_id).clone() {
            ResolvedType::GenericInstance {
                name, type_args, ..
            }
            | ResolvedType::GenericResource {
                name, type_args, ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.borrow().mangle_type_name(*t))
                    .collect();
                let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                (mangled, name, type_arg_names, Some(type_args))
            }
            _ => {
                let name = self
                    .type_table
                    .borrow()
                    .mangle_type_name(method_impl_type_id);
                (name.clone(), name, vec![], None)
            }
        };

        // For trait methods found through the newtype chain, override with the actual impl struct.
        // E.g., `loc.describe()` where `loc: Location`, `impl Describable for Point` →
        // use "Point" so the call resolves to "Point^Describable::describe".
        if let Some(impl_name) = trait_impl_struct_name {
            receiver_struct_name.clone_from(&impl_name);
            base_struct_name = impl_name;
        }

        let mangled_method_name = MethodName::format_local(
            &receiver_struct_name,
            trait_name.as_deref(),
            &method_call.method,
        );

        // Build monomorph_info for method calls on generic types or with method type args
        let monomorph_info = if let Some(ref blanket_param) = blanket_type_param {
            // For blanket impls, the template function uses the type param name (e.g., "I").
            // The call site uses the concrete receiver (e.g., "ArrayIter<i32>").
            // monomorph_info maps from the concrete name back to the template.
            let generic_name =
                MethodName::format_local(blanket_param, trait_name.as_deref(), &method_call.method);
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: vec![base_type_id],
                method_type_args: vec![],
                is_blanket: true,
            })
        } else if receiver_type_args.is_some() || !method_type_args.is_empty() {
            let generic_name =
                MethodName::format_local(&base_struct_name, None, &method_call.method);
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: receiver_type_args.unwrap_or_default(),
                method_type_args: method_type_args.clone(),
                is_blanket: false,
            })
        } else {
            None
        };

        // Convert method type args to string names for method_info
        // Use inferred type args if available, otherwise use explicit type args
        let method_type_arg_names: Vec<String> = method_type_args
            .iter()
            .map(|t| self.type_table.borrow().mangle_type_name(*t))
            .collect();

        // Build method_info with base struct name, then apply impl and method type args
        let is_type_param_receiver = matches!(
            self.type_table.borrow().get(base_type_id),
            ResolvedType::TypeParam { .. }
                | ResolvedType::TypePack { .. }
                | ResolvedType::AssocTypeProjection { .. }
        );
        let param_is_mut = self.lookup_method_param_is_mut(&base_struct_name, &method_call.method);
        let mut method_info = LocalMethodName::new(
            base_struct_name.clone(), // Use base struct name without type params
            trait_name,
            method_call.method.clone(),
        )
        .with_type_args(&impl_type_arg_names, &method_type_arg_names);
        method_info.is_type_param_receiver = is_type_param_receiver;
        method_info.is_ref_impl = is_ref_impl;
        method_info.cm_name = cm_name;

        // Use trait impl module source if this is a trait method,
        // otherwise use the struct's module (where inherent methods are defined)
        let method_module_source = trait_impl_module_source
            .or(Some(struct_module.clone()))
            .unwrap_or_else(|| self.current_module_source.clone());

        // Record use->def for jump-to-definition on the method name token.
        if let Some(method_ast_id) = self.find_impl_method_ast_id(
            &method_module_source,
            &base_struct_name,
            &method_call.method,
        ) {
            self.record_reference_to_decl(
                method_call.method_id,
                &method_module_source,
                method_ast_id,
            );
        }

        Self::build_tir_method_call(
            receiver,
            FunctionRef {
                module_source: method_module_source,
                name: mangled_method_name,
                monomorph_info,
                method_info: Some(method_info),
            },
            method_type_args, // Use inferred type args
            args.into_iter()
                .zip(param_is_mut.into_iter().chain(std::iter::repeat(false)))
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
            return_type,
            method_call.span,
        )
    }

    /// Resolve a static method call: `Array::<i32>::with_capacity(100)` or `Point::origin()`
    pub(super) fn resolve_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Resolve the target type first to get struct name for parameter type lookup
        let target_type_id = self.resolve_type(&static_call.target_type);

        // Extract struct name for parameter type lookup (follow newtypes to base)
        let struct_name_for_lookup = {
            let mut current_type = target_type_id;
            loop {
                match self.type_table.borrow().get(current_type).clone() {
                    ResolvedType::Struct { name, .. } => break Some(name),
                    ResolvedType::GenericInstance { name, .. } => break Some(name),
                    ResolvedType::Newtype { base_type, .. } => current_type = base_type,
                    ResolvedType::Flags { .. } => {
                        current_type = TypeTable::U32;
                    }
                    _ => break None,
                }
            }
        };

        // Look up parameter types for coercion
        let mut param_types = struct_name_for_lookup
            .as_ref()
            .map(|name| self.lookup_static_method_param_types(name, &static_call.method))
            .unwrap_or_default();

        // For generic variant constructors (e.g., Option::<Array<u8>>::Some([])),
        // compute substituted payload type so literal coercion works on first resolve.
        if param_types.is_empty() {
            let generic_data = {
                let resolved = self.type_table.borrow().get(target_type_id).clone();
                if let ResolvedType::GenericInstance {
                    name,
                    type_args: instance_type_args,
                    ..
                } = resolved
                {
                    Some((name, instance_type_args))
                } else {
                    None
                }
            };
            if let Some((name, instance_type_args)) = generic_data
                && let Some(variant_info) = self.lookup_variant_case(&name).cloned()
                && let Some((_, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
            {
                let payload_is_unit = matches!(
                    self.type_table.borrow().get(case_data.payload),
                    ResolvedType::Unit
                );
                if !payload_is_unit {
                    let mut payload_type = case_data.payload;
                    if !instance_type_args.is_empty() {
                        payload_type =
                            self.substitute_type_params(payload_type, &instance_type_args);
                    }
                    param_types.push(payload_type);
                }
            }
        }

        // Resolve method-level type arguments
        let method_type_args: Vec<TypeId> = static_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Re-resolve param types with concrete type args in scope for literal coercion.
        // lookup_static_method_param_types resolves without type params in scope, so
        // generic params (T, U, Array<T>, etc.) resolve to UNKNOWN or contain UNKNOWN.
        // We re-resolve them by temporarily mapping type param names directly to concrete
        // types from the call-site turbofish, then resolving the AST param types.
        // NOTE: We cannot change lookup_static_method_param_types itself to add type params,
        // because that would cause variant constructor param types to become non-empty,
        // bypassing the variant payload substitution path (line ~494).
        {
            let has_type_args = matches!(&static_call.target_type, ast::Type::Generic(_))
                || !method_type_args.is_empty();
            if has_type_args && !param_types.is_empty() {
                let method_def = self.find_static_method_def(
                    struct_name_for_lookup.as_deref().unwrap_or(""),
                    &static_call.method,
                );
                if let Some((impl_type_param_names, method_def)) = method_def {
                    let call_site_impl_args: Vec<TypeId> = match &static_call.target_type {
                        ast::Type::Generic(g) => {
                            g.args.iter().map(|t| self.resolve_type(t)).collect()
                        }
                        _ => vec![],
                    };

                    // Temporarily map type param names directly to concrete types
                    // (inherited scope; only `type_params` is replaced).
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.trait_ctx.type_params.clear();
                    for (i, name) in impl_type_param_names.iter().enumerate() {
                        if let Some(&concrete) = call_site_impl_args.get(i) {
                            scope
                                .trait_ctx
                                .type_params
                                .insert(name.clone(), (i as u32, concrete));
                        }
                    }
                    let impl_offset = impl_type_param_names.len();
                    for (i, tp) in method_def.type_params.iter().enumerate() {
                        if let Some(&concrete) = method_type_args.get(i) {
                            scope
                                .trait_ctx
                                .type_params
                                .insert(tp.name.clone(), ((impl_offset + i) as u32, concrete));
                        }
                    }

                    // Re-resolve all param types from AST with concrete type mappings
                    let non_self_params: Vec<_> = method_def
                        .params
                        .iter()
                        .filter(|p| p.self_kind == ast::SelfKind::None)
                        .collect();
                    for (i, param_type) in param_types.iter_mut().enumerate() {
                        if let Some(param) = non_self_params.get(i) {
                            *param_type = scope.resolve_type(&param.ty);
                        }
                    }

                    drop(scope);
                }
            }
        }

        // Resolve arguments with expected types for coercion
        let args: Vec<TirExpr> = static_call
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let expected_type = param_types.get(i).copied();
                self.resolve_expr(a, ctx, expected_type)
            })
            .collect();

        // Option::Some and Option::None are handled by the generic variant
        // construction path below (line ~686). No special case needed.

        // Handle flags type static methods: none() and all()
        {
            let flags_name = match self.type_table.borrow().get(target_type_id).clone() {
                ResolvedType::Flags { ref name, .. } => Some(name.clone()),
                _ => None,
            };
            if let Some(ref name) = flags_name
                && let Some(flags_info) = self.lookup_flags_case(name).cloned()
            {
                match static_call.method.as_str() {
                    "none" => {
                        if !args.is_empty() {
                            let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                                expected: 0,
                                found: args.len(),
                                span: static_call.span,
                            });
                            return TirExpr::new(
                                TirExprKind::Unit,
                                TypeTable::ERROR,
                                static_call.span,
                            );
                        }
                        return TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: 0,
                                repr: "0".to_string(),
                            },
                            flags_info.type_id,
                            static_call.span,
                        );
                    }
                    "all" => {
                        if !args.is_empty() {
                            let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                                expected: 0,
                                found: args.len(),
                                span: static_call.span,
                            });
                            return TirExpr::new(
                                TirExprKind::Unit,
                                TypeTable::ERROR,
                                static_call.span,
                            );
                        }
                        let member_count = flags_info.members.len();
                        let all_bits = if member_count == 0 {
                            0u32
                        } else if member_count >= 32 {
                            u32::MAX
                        } else {
                            (1u32 << member_count) - 1
                        };
                        return TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: u64::from(all_bits),
                                repr: all_bits.to_string(),
                            },
                            flags_info.type_id,
                            static_call.span,
                        );
                    }
                    _ => {}
                }
            }
        }

        // Handle custom variant construction: Shape::Circle(5.0) or MyVariant::Unit
        if let ResolvedType::Variant {
            name,
            module_source: _,
        } = self.type_table.borrow().get(target_type_id).clone()
        {
            // Look up the variant case info
            if let Some(variant_info) = self.lookup_variant_case(&name) {
                // Find the case by name
                if let Some((case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                {
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }

                    // Payload is the single argument, or None for unit variants
                    let payload = args.into_iter().next().map(Box::new);

                    // Create VariantConstruct expression
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type: target_type_id,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload,
                        },
                        target_type_id,
                        static_call.span,
                    );
                }
                // If no matching case, fall through to general method lookup
                // (e.g., trait methods like `AppError::from(e)`)
            }
        }

        // Handle generic variant construction: Result::<i32, String>::Ok(42)
        let generic_name = {
            let tt = self.type_table.borrow();
            if let ResolvedType::GenericInstance { name, .. } = tt.get(target_type_id) {
                Some(name.clone())
            } else {
                None
            }
        };
        if let Some(name) = generic_name {
            // Check if the base type is a variant
            if let Some(variant_info) = self.lookup_variant_case(&name).cloned() {
                // This is a generic variant like Result<T, E>
                // Find the case by name
                if let Some((case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                {
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }

                    // Check payload type against the variant case's expected type
                    if !args.is_empty()
                        && let Some(&expected_type) = param_types.first()
                    {
                        let span = static_call
                            .args
                            .first()
                            .map_or(static_call.span, super::ast::Expr::span);
                        self.typecheck(args[0].type_id, expected_type, span);
                    }

                    // Payload was already resolved with the correct expected type
                    // (substituted in the param_types computation above).
                    let payload = args.into_iter().next().map(Box::new);

                    // Create VariantConstruct expression
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type: target_type_id,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload,
                        },
                        target_type_id,
                        static_call.span,
                    );
                }
                // If no matching case, fall through to general method lookup
                // (e.g., trait methods like `Result::<T, E>::from(e)`)
            }
        }

        // Handle From<T>::from calls resolved via bodyless `impl From<T> for Type;`
        // The synthesized function doesn't exist during resolution, so we generate the call inline.
        if static_call.method == "from"
            && args.len() == 1
            && self.has_from_synthesis_request(&static_call.target_type, &args[0].type_id)
        {
            return self.resolve_from_call(
                target_type_id,
                args[0].type_id,
                args.into_iter().next().unwrap(),
                static_call.span,
            );
        }

        // Reflexive identity: From<T> for T — return the value unchanged.
        if static_call.method == "from" && args.len() == 1 && args[0].type_id == target_type_id {
            return args.into_iter().next().unwrap();
        }

        // Newtype From conversions: From<Base> for Newtype and From<Newtype> for Base.
        // Newtypes share the same representation as their base type, so this is a Cast.
        if static_call.method == "from" && args.len() == 1 {
            let arg_type = args[0].type_id;
            let base_of_target = self.type_table.borrow().get_newtype_base(target_type_id);
            let base_of_arg = self.type_table.borrow().get_newtype_base(arg_type);
            if base_of_target == Some(arg_type) || base_of_arg == Some(target_type_id) {
                let arg = args.into_iter().next().unwrap();
                return TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(arg),
                        target_type: target_type_id,
                    },
                    target_type_id,
                    static_call.span,
                );
            }
        }

        let (struct_name, struct_module, mangled_struct_name, struct_type_args) =
            match self.type_table.borrow().get(target_type_id) {
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                } => (name.clone(), module_source.clone(), name.clone(), vec![]),
                ResolvedType::Resource {
                    name,
                    module_source,
                } => (name.clone(), module_source.clone(), name.clone(), vec![]),
                // Generic resource types (Future<T>, Stream<T>, etc.) - handle like generic structs
                // for static method resolution: use the base name and type args for substitution.
                ResolvedType::GenericResource {
                    name,
                    module_source,
                    type_args,
                } => {
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| self.type_table.borrow().mangle_type_name(*t))
                        .collect();
                    let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                    (
                        name.clone(),
                        module_source.clone(),
                        mangled,
                        type_args.clone(),
                    )
                }
                ResolvedType::Primitive(prim) => {
                    let name = prim.as_str().to_string();
                    (name.clone(), ModuleSource::primitive(), name, vec![])
                }
                ResolvedType::Enum {
                    name,
                    module_source,
                } => (name.clone(), module_source.clone(), name.clone(), vec![]),
                ResolvedType::Variant {
                    name,
                    module_source,
                } => (name.clone(), module_source.clone(), name.clone(), vec![]),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } => {
                    // Build mangled name for generic type: Array<i32>
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| self.type_table.borrow().mangle_type_name(*t))
                        .collect();
                    let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                    (
                        name.clone(),
                        module_source.clone(),
                        mangled,
                        type_args.clone(),
                    )
                }
                ResolvedType::Newtype {
                    name,
                    module_source,
                    base_type,
                } => {
                    // First try the newtype's own name (for methods defined via `impl NewtypeName`)
                    let newtype_name = name.clone();
                    let newtype_module = module_source.clone();

                    // Check if the newtype itself has the static method
                    if self.has_static_method_direct(&newtype_name, &static_call.method) {
                        (newtype_name.clone(), newtype_module, newtype_name, vec![])
                    } else {
                        // Fall back to the base type for inherited methods
                        match self.type_table.borrow().get(*base_type).clone() {
                            ResolvedType::Struct {
                                name,
                                module_source,
                                ..
                            } => (name.clone(), module_source, name, vec![]),
                            ResolvedType::GenericInstance {
                                name,
                                module_source,
                                type_args,
                            } => {
                                let type_arg_names: Vec<String> = type_args
                                    .iter()
                                    .map(|t| self.type_table.borrow().mangle_type_name(*t))
                                    .collect();
                                let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                                (name, module_source, mangled, type_args)
                            }
                            ResolvedType::Newtype {
                                base_type: inner_base,
                                ..
                            } => {
                                let mut current = inner_base;
                                loop {
                                    match self.type_table.borrow().get(current).clone() {
                                        ResolvedType::Struct {
                                            name,
                                            module_source,
                                            ..
                                        } => {
                                            break (name.clone(), module_source, name, vec![]);
                                        }
                                        ResolvedType::Newtype {
                                            base_type: next, ..
                                        } => current = next,
                                        _ => {
                                            break (
                                                newtype_name.clone(),
                                                newtype_module,
                                                newtype_name,
                                                vec![],
                                            );
                                        }
                                    }
                                }
                            }
                            ResolvedType::Primitive(prim) => {
                                let prim_name = prim.as_str().to_string();
                                (
                                    prim_name.clone(),
                                    ModuleSource::primitive(),
                                    prim_name,
                                    vec![],
                                )
                            }
                            _ => (newtype_name.clone(), newtype_module, newtype_name, vec![]),
                        }
                    }
                }
                ResolvedType::Flags {
                    name,
                    module_source,
                } => {
                    // First try the flags' own name, then fall back to u32
                    let flags_name = name.clone();
                    let flags_module = module_source.clone();
                    if self.has_static_method_direct(&flags_name, &static_call.method) {
                        (flags_name.clone(), flags_module, flags_name, vec![])
                    } else {
                        (
                            "u32".to_string(),
                            ModuleSource::primitive(),
                            "u32".to_string(),
                            vec![],
                        )
                    }
                }
                _ => {
                    // Unknown type - return error expression
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                }
            };

        // Find trait name: if the static method belongs to a trait impl, include the
        // trait name in the mangled function name so WIR can resolve it correctly.
        // For From/TryFrom, disambiguate by matching the first argument's type.
        let arg_type_hint = if (static_call.method == "from" || static_call.method == "try_from")
            && args.len() == 1
        {
            Some(self.type_table.borrow().type_name(args[0].type_id))
        } else {
            None
        };
        let trait_name_opt = self.find_static_method_trait_with_arg(
            &struct_name,
            &static_call.method,
            arg_type_hint.as_deref(),
        );

        let mangled_func_name = MethodName::format_local(
            &mangled_struct_name,
            trait_name_opt.as_deref(),
            &static_call.method,
        );

        let method_ref = StaticMethodRef::new(
            struct_module.clone(),
            struct_name.clone(),
            static_call.method.clone(),
            trait_name_opt.clone(),
        );

        // Look up return type
        let mut return_type =
            self.lookup_static_method_return_type(&method_ref, &mangled_func_name);

        // Emit a compile error if the static method was not found anywhere
        if return_type == TypeTable::UNKNOWN {
            let _ = self.logger.error(TypeError::UnknownFunction {
                name: format!("{}::{}", struct_name, static_call.method),
                span: static_call.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
        }

        // Substitute type parameters in the return type using SubstitutionContext
        {
            let mut subst_ctx = SubstitutionContext::new();
            if !struct_type_args.is_empty() {
                subst_ctx = subst_ctx.with_impl_args(&struct_type_args);
            }
            let impl_offset = struct_type_args.len() as u32;
            if !method_type_args.is_empty() {
                subst_ctx = subst_ctx.with_method_args(&method_type_args, impl_offset);
            }
            if !subst_ctx.is_empty() {
                return_type = subst_ctx.substitute(return_type, &mut self.type_table.borrow_mut());
            }
        }

        // Build monomorph_info for generic instantiations
        let monomorph_info = if struct_type_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            let generic_name = MethodName::format_local(
                &struct_name,
                trait_name_opt.as_deref(),
                &static_call.method,
            );
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: struct_type_args.clone(),
                method_type_args: method_type_args.clone(),
                is_blanket: false,
            })
        };

        let method_type_arg_names: Vec<String> = method_type_args
            .iter()
            .map(|t| self.type_table.borrow().mangle_type_name(*t))
            .collect();
        let impl_only_type_arg_names: Vec<String> = struct_type_args
            .iter()
            .map(|t| self.type_table.borrow().mangle_type_name(*t))
            .collect();

        let param_is_mut = struct_name_for_lookup
            .as_deref()
            .map(|name| self.lookup_static_method_param_is_mut(name, &static_call.method))
            .unwrap_or_default();

        // Build method_info with base struct name and trait name (if applicable)
        let mut method_info = LocalMethodName::new(
            struct_name.clone(),
            trait_name_opt,
            static_call.method.clone(),
        )
        .with_type_args(&impl_only_type_arg_names, &method_type_arg_names);

        // Propagate #[cm("...")] from resource static methods for CM binding synthesis.
        method_info.cm_name =
            self.lookup_resource_static_cm(&struct_name, &struct_module, &static_call.method);

        // Record use->def for jump-to-definition on the method name token.
        if let Some(method_ast_id) =
            self.find_impl_method_ast_id(&struct_module, &struct_name, &static_call.method)
        {
            self.record_reference_to_decl(static_call.method_id, &struct_module, method_ast_id);
        }

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: struct_module,
                    name: mangled_func_name,
                    monomorph_info,
                    method_info: Some(method_info),
                },
                type_args: method_type_args,
                args: args
                    .into_iter()
                    .zip(param_is_mut.into_iter().chain(std::iter::repeat(false)))
                    .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                    .collect(),
            },
            return_type,
            static_call.span,
        )
    }

    /// Look up `#[cm("...")]` for a static (no-self) method on a resource type in a module.
    fn lookup_resource_static_cm(
        &self,
        struct_name: &str,
        struct_module: &ModuleSource,
        method_name: &str,
    ) -> Option<String> {
        let module = self.loaded_modules.get(struct_module)?;
        for item in &module.items {
            if let crate::ast::Item::Resource(resource) = item
                && resource.name == struct_name
            {
                for method in &resource.methods {
                    let has_self = method.params.iter().any(|p| {
                        matches!(&p.ty, crate::ast::Type::Reference(r) | crate::ast::Type::MutReference(r)
                            if matches!(&**r, crate::ast::Type::Named(n) if n.name == "Self" || n.name == resource.name))
                            || matches!(&p.ty, crate::ast::Type::Named(n) if n.name == "Self" || n.name == resource.name)
                    });
                    if method.name == method_name && !has_self {
                        return method
                            .attrs
                            .iter()
                            .find(|a| a.name == "cm")
                            .and_then(|a| a.args.first())
                            .map(|a| a.as_str().to_string());
                    }
                }
            }
        }
        None
    }

    /// Check if a static method exists directly for a given type name (no newtype fallback).
    fn has_static_method_direct(&self, struct_name: &str, method_name: &str) -> bool {
        let mangled = MethodName::format_local(struct_name, None, method_name);
        if self.function_return_types.contains_key(&mangled) {
            return true;
        }
        // Also check with trait-qualified name
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name) {
            let trait_mangled =
                MethodName::format_local(struct_name, Some(&trait_name), method_name);
            if self.function_return_types.contains_key(&trait_mangled) {
                return true;
            }
        }
        // Check current module's impl blocks
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item {
                let impl_struct_name = self.get_type_name(&impl_block.ty);
                if impl_struct_name == struct_name {
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if method.name == method_name && !has_self {
                            return true;
                        }
                    }
                }
            }
        }
        // Check loaded modules' impl blocks
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = Self::get_type_name_static(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Look up static method return type based on struct name and method name
    pub(super) fn lookup_static_method_return_type(
        &mut self,
        method_ref: &StaticMethodRef,
        mangled_func_name: &str,
    ) -> TypeId {
        let struct_name = method_ref.type_name.as_str();
        let struct_module = &method_ref.module;
        let method_name = method_ref.method_name.as_str();
        // First check locally registered function_return_types
        if let Some(&return_type) = self.function_return_types.get(mangled_func_name) {
            return return_type;
        }

        // Also try with just StructName::method (for non-generic types)
        let simple_name = MethodName::format_local(struct_name, None, method_name);
        if let Some(&return_type) = self.function_return_types.get(&simple_name) {
            return return_type;
        }

        // Try with trait-qualified name (StructName^TraitName::method)
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name) {
            let trait_mangled =
                MethodName::format_local(struct_name, Some(&trait_name), method_name);
            if let Some(&return_type) = self.function_return_types.get(&trait_mangled) {
                return return_type;
            }
        }

        // Try looking up in loaded modules
        if !struct_module.is_entry_point()
            && let Some(module) = self.loaded_modules.get(struct_module)
        {
            for item in &module.items {
                // Check impl blocks
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            // Static methods have no self parameter
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                // Set up type parameters from impl block before resolving.
                                // Inherited scope; only `type_params` is replaced.
                                let mut scope = self.enter_inherited_type_param_scope();
                                scope.trait_ctx.type_params.clear();

                                // Extract type params from impl block type (e.g., impl Array<T>)
                                if let ast::Type::Generic(generic) = &impl_block.ty {
                                    for (i, arg) in generic.args.iter().enumerate() {
                                        if let ast::Type::Named(named) = arg {
                                            let name = &named.name;
                                            if !scope.trait_ctx.type_params.contains_key(name) {
                                                let type_id = scope
                                                    .type_table
                                                    .borrow_mut()
                                                    .make_type_param(name.clone(), i as u32);
                                                scope
                                                    .trait_ctx
                                                    .type_params
                                                    .insert(name.clone(), (i as u32, type_id));
                                            }
                                        }
                                    }
                                }

                                // Method-level type params (e.g. fn make<T>(...) -> T)
                                let m_offset = scope.trait_ctx.type_params.len();
                                for (i, tp) in method
                                    .type_params
                                    .iter()
                                    .filter(|p| !p.is_effect)
                                    .enumerate()
                                {
                                    if scope.trait_ctx.type_params.contains_key(&tp.name) {
                                        continue;
                                    }
                                    let idx = (m_offset + i) as u32;
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

                                let result = scope.resolve_return_type_in_module(
                                    struct_module,
                                    method.return_type.as_ref(),
                                );

                                drop(scope);

                                return result;
                            }
                        }
                    }
                }

                // Check resource declarations
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                {
                    for method in &resource.methods {
                        // Static methods have no self parameter (no &TcpSocket or &Self)
                        let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                            });
                        if method.name == method_name && !has_self {
                            // Set up type parameters from resource declaration before resolving.
                            // Inherited scope; only `type_params` is replaced.
                            let mut scope = self.enter_inherited_type_param_scope();
                            scope.trait_ctx.type_params.clear();

                            for (i, param) in resource.type_params.iter().enumerate() {
                                let name = &param.name;
                                if !scope.trait_ctx.type_params.contains_key(name) {
                                    let type_id = scope
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), i as u32);
                                    scope
                                        .trait_ctx
                                        .type_params
                                        .insert(name.clone(), (i as u32, type_id));
                                }
                            }

                            let result = scope.resolve_return_type_in_module(
                                struct_module,
                                method.return_type.as_ref(),
                            );

                            drop(scope);

                            return result;
                        }
                    }
                }
            }
        }

        // Search via pre-built index (handles impls defined outside the struct's defining module)
        if let Some(methods) = self.trait_env.static_method_index.get(struct_name) {
            for (name, ms, item_idx, method_idx) in methods {
                if name == method_name
                    && let Some(module) = self.loaded_modules.get(ms)
                    && let Item::Impl(impl_block) = &module.items[*item_idx]
                {
                    let method = &impl_block.methods[*method_idx];

                    // Inherited scope; only `type_params` is replaced.
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.trait_ctx.type_params.clear();

                    if let ast::Type::Generic(generic) = &impl_block.ty {
                        for (i, arg) in generic.args.iter().enumerate() {
                            if let ast::Type::Named(named) = arg {
                                let name = &named.name;
                                if !scope.trait_ctx.type_params.contains_key(name) {
                                    let type_id = scope
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), i as u32);
                                    scope
                                        .trait_ctx
                                        .type_params
                                        .insert(name.clone(), (i as u32, type_id));
                                }
                            }
                        }
                    }

                    // Method-level type params (e.g. fn make<T>(...) -> T)
                    let m_offset = scope.trait_ctx.type_params.len();
                    for (i, tp) in method
                        .type_params
                        .iter()
                        .filter(|p| !p.is_effect)
                        .enumerate()
                    {
                        if scope.trait_ctx.type_params.contains_key(&tp.name) {
                            continue;
                        }
                        let idx = (m_offset + i) as u32;
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

                    let result = method
                        .return_type
                        .as_ref()
                        .map(|t| scope.resolve_type(t))
                        .unwrap_or(TypeTable::UNIT);

                    drop(scope);
                    return result;
                }
            }
        }

        // Search resource declarations via pre-built index
        if let Some(methods) = self.trait_env.resource_static_method_index.get(struct_name) {
            for (name, ms, item_idx, method_idx) in methods {
                if name == method_name
                    && let Some(module) = self.loaded_modules.get(ms)
                    && let Item::Resource(resource) = &module.items[*item_idx]
                {
                    let method = &resource.methods[*method_idx];

                    // Inherited scope; only `type_params` is replaced.
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.trait_ctx.type_params.clear();

                    for (i, param) in resource.type_params.iter().enumerate() {
                        let name = &param.name;
                        if !scope.trait_ctx.type_params.contains_key(name) {
                            let type_id = scope
                                .type_table
                                .borrow_mut()
                                .make_type_param(name.clone(), i as u32);
                            scope
                                .trait_ctx
                                .type_params
                                .insert(name.clone(), (i as u32, type_id));
                        }
                    }

                    let result = method
                        .return_type
                        .as_ref()
                        .map(|t| scope.resolve_type(t))
                        .unwrap_or(TypeTable::UNIT);

                    drop(scope);

                    return result;
                }
            }
        }

        // Auto-derived `Default::default()` returns the struct type itself.
        if method_name == "default"
            && let Some(struct_type) = self.auto_derive_default_struct_type(struct_name)
        {
            return struct_type;
        }

        // Fall back to a trait default method body. When
        // `impl Trait for Type` does not override a static method that the
        // trait provides a default for, concrete `Type::method()` calls
        // must still resolve — this mirrors how generic dispatch
        // (`T::method()`) already reaches the trait default via
        // `find_method_type_param_names`.
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name)
            && let Some(trait_methods) = self.find_trait_decl_methods(&trait_name)
        {
            for default_method in &trait_methods {
                if default_method.name != method_name || default_method.body.is_none() {
                    continue;
                }
                let has_self = default_method
                    .params
                    .iter()
                    .any(|p| p.self_kind != ast::SelfKind::None);
                if has_self {
                    continue;
                }
                let mut scope = self.enter_inherited_type_param_scope();
                scope.trait_ctx.type_params.clear();
                scope.trait_ctx.assoc_type_bindings.clear();
                // Bind `Self::AssocName` projections that may appear in the
                // trait default body's return type (e.g. FromStr's
                // `Result<Self, Self::Err>`). Pull the bindings from the
                // impl block that connects this trait to this type.
                let impl_assoc_types = scope.find_impl_assoc_types(struct_name, &trait_name);
                for binding in &impl_assoc_types {
                    let type_id = scope.resolve_type(&binding.ty);
                    scope
                        .trait_ctx
                        .assoc_type_bindings
                        .insert(binding.name.clone(), type_id);
                }
                // Resolve `Self` to the concrete type at the call site.
                // `resolve_named_type` maps primitives to their canonical
                // TypeTable id rather than a struct wrapper.
                let self_type_id = scope.resolve_named_type(struct_name, Span::default());
                let old_self = scope.trait_ctx.self_type;
                scope.trait_ctx.self_type = Some(self_type_id);
                let result = default_method
                    .return_type
                    .as_ref()
                    .map(|t| scope.resolve_type(t))
                    .unwrap_or(TypeTable::UNIT);
                scope.trait_ctx.self_type = old_self;
                drop(scope);
                return result;
            }
        }

        TypeTable::UNKNOWN
    }

    /// Look up the associated-type bindings on the impl block that
    /// connects `trait_name` to `struct_name`. Returns an empty vec when
    /// the impl is auto-derived or otherwise has no bindings.
    fn find_impl_assoc_types(
        &self,
        struct_name: &str,
        trait_name: &str,
    ) -> Vec<ast::AssociatedTypeBinding> {
        let scan = |items: &[Item]| -> Option<Vec<ast::AssociatedTypeBinding>> {
            for item in items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                    && Self::get_type_name_static(trait_type) == trait_name
                    && Self::get_type_name_static(&impl_block.ty) == struct_name
                {
                    return Some(impl_block.associated_types.clone());
                }
            }
            None
        };
        if let Some(found) = scan(self.current_module_items) {
            return found;
        }
        if let Some(entries) = self.trait_env.impl_index.get(struct_name) {
            for (module_source, item_idx) in entries {
                if let Some(module) = self.loaded_modules.get(module_source)
                    && let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_type) = &impl_block.trait_type
                    && Self::get_type_name_static(trait_type) == trait_name
                {
                    return impl_block.associated_types.clone();
                }
            }
        }
        Vec::new()
    }

    /// Look up static method parameter types for coercion.
    ///
    /// Sets up impl-level and method-level type parameters in scope so that
    /// generic parameter types resolve to `TypeParam(...)` instead of `Unknown`.
    /// Callers can then substitute these with concrete types after inference.
    pub(super) fn lookup_static_method_param_types(
        &mut self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<TypeId> {
        // Check in current module's impl blocks first (highest priority)
        let found: Option<(ast::Type, ast::Function)> =
            self.current_module_items.iter().find_map(|item| {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                return Some((impl_block.ty.clone(), method.clone()));
                            }
                        }
                    }
                }
                None
            });

        if let Some((impl_ty, method)) = found {
            return self.resolve_static_method_params_in_scope(&impl_ty, &method);
        }

        // O(1) lookup via pre-built static method index
        let indexed: Option<(ast::Type, ast::Function)> =
            if let Some(methods) = self.trait_env.static_method_index.get(struct_name) {
                let mut found = None;
                for (name, module_source, item_idx, method_idx) in methods {
                    if name == method_name
                        && let Some(module) = self.loaded_modules.get(module_source)
                        && let Item::Impl(impl_block) = &module.items[*item_idx]
                    {
                        let method = &impl_block.methods[*method_idx];
                        found = Some((impl_block.ty.clone(), method.clone()));
                        break;
                    }
                }
                found
            } else {
                None
            };
        if let Some((impl_ty, method)) = indexed {
            return self.resolve_static_method_params_in_scope(&impl_ty, &method);
        }

        Vec::new()
    }

    /// Resolve a static method's parameter types with impl-level and method-level
    /// type parameters set up in `trait_ctx.type_params`. Restores the original
    /// scope before returning.
    fn resolve_static_method_params_in_scope(
        &mut self,
        impl_ty: &ast::Type,
        method: &ast::Function,
    ) -> Vec<TypeId> {
        // Inherited scope; only `type_params` is replaced.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();

        // Impl-level type params (e.g. `impl Box<T>` -> register T)
        if let ast::Type::Generic(generic) = impl_ty {
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
                }
            }
        }

        // Method-level type params (e.g. `fn make<T>(x: T)` -> register T)
        let offset = scope.trait_ctx.type_params.len();
        for (i, tp) in method
            .type_params
            .iter()
            .filter(|p| !p.is_effect)
            .enumerate()
        {
            if scope.trait_ctx.type_params.contains_key(&tp.name) {
                continue;
            }
            let idx = (offset + i) as u32;
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

        let result: Vec<TypeId> = method
            .params
            .iter()
            .map(|p| scope.resolve_type(&p.ty))
            .collect();

        drop(scope);
        result
    }

    /// Find the AST definition of a static method for a given struct.
    /// Returns the impl block's type param names and the method definition.
    fn find_static_method_def(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<(Vec<String>, ast::Function)> {
        let extract_impl_type_param_names = |ty: &ast::Type| -> Vec<String> {
            match ty {
                ast::Type::Generic(g) => g
                    .args
                    .iter()
                    .filter_map(|arg| match arg {
                        ast::Type::Named(n) => Some(n.name.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            }
        };

        // Check current module
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                for method in &impl_block.methods {
                    let has_self = method
                        .params
                        .iter()
                        .any(|p| p.self_kind != ast::SelfKind::None);
                    if method.name == method_name && !has_self {
                        let names = extract_impl_type_param_names(&impl_block.ty);
                        return Some((names, method.clone()));
                    }
                }
            }
        }
        // Check indexed modules
        if let Some(methods) = self.trait_env.static_method_index.get(struct_name) {
            for (name, module_source, item_idx, method_idx) in methods {
                if name == method_name
                    && let Some(module) = self.loaded_modules.get(module_source)
                    && let Item::Impl(impl_block) = &module.items[*item_idx]
                {
                    let names = extract_impl_type_param_names(&impl_block.ty);
                    return Some((names, impl_block.methods[*method_idx].clone()));
                }
            }
        }
        None
    }

    /// Look up whether each non-self parameter of an instance method is `mut`.
    /// Returns empty vec (conservative) for unknown methods.
    fn lookup_method_param_is_mut(&self, struct_name: &str, method_name: &str) -> Vec<bool> {
        let find_in_items = |items: &[Item]| -> Option<Vec<bool>> {
            items.iter().find_map(|item| {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = Self::get_type_name_static(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            if method.name == method_name {
                                let is_muts: Vec<bool> = method
                                    .params
                                    .iter()
                                    .filter(|p| p.self_kind == ast::SelfKind::None)
                                    .map(|p| p.is_mut)
                                    .collect();
                                return Some(is_muts);
                            }
                        }
                    }
                }
                None
            })
        };

        if let Some(result) = find_in_items(self.current_module_items) {
            return result;
        }
        for module in self.loaded_modules.values() {
            if let Some(result) = find_in_items(&module.items) {
                return result;
            }
        }
        Vec::new()
    }

    /// Look up whether each parameter of a static method is `mut`.
    /// Returns empty vec (conservative) for unknown methods.
    pub(super) fn lookup_static_method_param_is_mut(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<bool> {
        let find_in_items = |items: &[Item]| -> Option<Vec<bool>> {
            items.iter().find_map(|item| {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = Self::get_type_name_static(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                return Some(method.params.iter().map(|p| p.is_mut).collect());
                            }
                        }
                    }
                }
                None
            })
        };

        if let Some(result) = find_in_items(self.current_module_items) {
            return result;
        }
        for module in self.loaded_modules.values() {
            if let Some(result) = find_in_items(&module.items) {
                return result;
            }
        }
        Vec::new()
    }

    /// Find the trait name for a static method on a struct, if the method belongs to a trait impl.
    /// Returns `None` for inherent static methods, `Some(trait_name)` for trait static methods.
    pub(super) fn has_from_synthesis_request(
        &self,
        target_type: &ast::Type,
        arg_type_id: &crate::tir::TypeId,
    ) -> bool {
        let target_name = Self::get_type_name_static(target_type);
        let arg_type_name = self.type_table.borrow().type_name(*arg_type_id);
        let from_trait_name = self
            .type_table
            .borrow()
            .compiler_items()
            .trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let check_impl = |impl_block: &ast::ImplBlock| -> bool {
            if !impl_block.is_synthesize_request {
                return false;
            }
            let Some(trait_type) = &impl_block.trait_type else {
                return false;
            };
            if Self::get_type_name_static(trait_type) != from_trait_name
                || Self::get_type_name_static(&impl_block.ty) != target_name
            {
                return false;
            }
            if let ast::Type::Generic(generic) = trait_type
                && generic.args.len() == 1
            {
                self.get_type_name_full(&generic.args[0]) == arg_type_name
            } else {
                false
            }
        };
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && check_impl(impl_block)
            {
                return true;
            }
        }
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && check_impl(impl_block)
                {
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn find_static_method_trait(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<String> {
        self.locate_static_method_impl(struct_name, method_name, None)
            .and_then(|r| r.trait_name)
    }

    pub(super) fn find_static_method_trait_with_arg(
        &self,
        struct_name: &str,
        method_name: &str,
        arg_type_name: Option<&str>,
    ) -> Option<String> {
        self.locate_static_method_impl(struct_name, method_name, arg_type_name)
            .and_then(|r| r.trait_name)
    }

    /// Locate a static trait method impl, returning the resolved identity
    /// (`module`, `type_name`, `method_name`, `trait_name`). Used so that
    /// `FunctionRef` gets the correct `module_source` — especially when a
    /// user defines `impl From<MyType> for i32` in the entry module (or
    /// another module), so DCE and WIR building can find it.
    pub(super) fn locate_static_method_impl(
        &self,
        struct_name: &str,
        method_name: &str,
        arg_type_name: Option<&str>,
    ) -> Option<StaticMethodRef> {
        let from_trait_name = self
            .type_table
            .borrow()
            .compiler_items()
            .trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let is_from_or_try_from =
            |base: &str| -> bool { base == from_trait_name || base == "TryFrom" };
        let resolve_trait_name = |trait_type: &ast::Type| -> String {
            let base = Self::get_type_name_static(trait_type);
            if is_from_or_try_from(&base) {
                self.get_type_name_full(trait_type)
            } else {
                base
            }
        };

        let matches_arg_type = |trait_type: &ast::Type| -> bool {
            let Some(expected) = arg_type_name else {
                return true;
            };
            let base = Self::get_type_name_static(trait_type);
            if is_from_or_try_from(&base)
                && let ast::Type::Generic(g) = trait_type
                && let Some(arg) = g.args.first()
            {
                return Self::get_type_name_static(arg) == expected;
            }
            !is_from_or_try_from(&base)
        };

        let check_impl = |impl_block: &ast::ImplBlock| -> Option<String> {
            let trait_type = impl_block.trait_type.as_ref()?;
            if Self::get_type_name_static(&impl_block.ty) != struct_name
                || !matches_arg_type(trait_type)
            {
                return None;
            }
            for method in &impl_block.methods {
                let has_self = method
                    .params
                    .iter()
                    .any(|p| p.self_kind != ast::SelfKind::None);
                if method.name == method_name && !has_self {
                    return Some(resolve_trait_name(trait_type));
                }
            }
            // Fall back to the trait declaration's default methods: when
            // `impl Trait for Type` does not override a defaulted static
            // method, the trait still provides the body, so `Type::method`
            // (called concretely, not via a generic bound) must resolve to
            // the trait's default. This mirrors how generic dispatch
            // (`T::method()`) already finds default methods in
            // `find_method_type_param_names`.
            let trait_name_base = Self::get_type_name_static(trait_type);
            if let Some(trait_methods) = self.find_trait_decl_methods(&trait_name_base) {
                for default_method in &trait_methods {
                    if default_method.name != method_name || default_method.body.is_none() {
                        continue;
                    }
                    let has_self = default_method
                        .params
                        .iter()
                        .any(|p| p.self_kind != ast::SelfKind::None);
                    if !has_self {
                        return Some(resolve_trait_name(trait_type));
                    }
                }
            }
            None
        };

        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_name) = check_impl(impl_block)
            {
                return Some(StaticMethodRef::new(
                    self.current_module_source.clone(),
                    struct_name,
                    method_name,
                    Some(trait_name),
                ));
            }
        }

        // Use trait_env.impl_index for O(1) lookup instead of scanning all modules
        if let Some(entries) = self.trait_env.impl_index.get(struct_name) {
            for (module_source, item_idx) in entries {
                if let Some(module) = self.loaded_modules.get(module_source)
                    && let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_name) = check_impl(impl_block)
                {
                    return Some(StaticMethodRef::new(
                        module_source.clone(),
                        struct_name,
                        method_name,
                        Some(trait_name),
                    ));
                }
            }
        }

        // Auto-derived `Default::default()` — no user impl block exists, but
        // the synthesis pass emits one in the struct's own module.
        if method_name == "default" && self.auto_derive_default_struct_type(struct_name).is_some() {
            return Some(StaticMethodRef::new(
                self.find_struct_module_source(struct_name),
                struct_name,
                method_name,
                Some("Default".to_string()),
            ));
        }

        None
    }

    /// Get the operator trait and method name for a binary operator.
    pub(super) fn is_static_method(&self, struct_name: &str, method_name: &str) -> bool {
        let mangled_name = MethodName::format_local(struct_name, None, method_name);

        // Check if it's registered in function_return_types (static methods are registered there)
        if self.function_return_types.contains_key(&mangled_name) {
            return true;
        }

        // O(1) lookup via pre-built static method index (impl blocks)
        if let Some(methods) = self.trait_env.static_method_index.get(struct_name)
            && methods.iter().any(|(name, ..)| name == method_name)
        {
            return true;
        }

        // Check current module's impl blocks (not in the pre-built index)
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item {
                let impl_struct_name = self.get_type_name(&impl_block.ty);
                if impl_struct_name == struct_name {
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if method.name == method_name && !has_self {
                            return true;
                        }
                    }
                }
            }
        }

        // O(1) lookup via pre-built resource static method index
        if let Some(methods) = self.trait_env.resource_static_method_index.get(struct_name)
            && methods.iter().any(|(name, ..)| name == method_name)
        {
            return true;
        }

        // For newtypes/flags, check if the base type has the static method
        if let Some(newtype_id) = self.lookup_newtype(struct_name) {
            let base_name = match self.type_table.borrow().get(newtype_id).clone() {
                ResolvedType::Newtype { base_type, .. } => {
                    Some(self.type_table.borrow().type_name(base_type))
                }
                ResolvedType::Flags { .. } => Some("u32".to_string()),
                _ => None,
            };
            if let Some(base_name) = base_name
                && self.is_static_method(&base_name, method_name)
            {
                return true;
            }
        }

        // Auto-derived `Default::default()` for structs whose fields all have
        // default expressions. No user impl exists (previous checks would have
        // caught it), but `synthesis::traits` will emit the body.
        if method_name == "default" && self.auto_derive_default_struct_type(struct_name).is_some() {
            return true;
        }

        // Defaulted trait method: when `impl Trait for Type` does not
        // override a static method that the trait provides a default for,
        // `Type::method` must still resolve. `locate_static_method_impl`
        // applies the same fallback to find the trait name and module.
        if self
            .locate_static_method_impl(struct_name, method_name, None)
            .is_some()
        {
            return true;
        }

        false
    }

    /// Resolve a static method call from a qualified name like `Point::origin()`
    pub(super) fn resolve_static_method_call_from_qualified(
        &mut self,
        struct_name: &str,
        method_name: &str,
        mangled_func_name: &str,
        args: &[TirExpr],
        impl_type_args: &[TypeId],
        method_type_args: &[TypeId],
        span: Span,
        _ctx: &mut FunctionContext,
    ) -> TirExpr {
        // For newtypes, check if the newtype itself has the method first,
        // then fall back to the base type's static method
        let (actual_struct_name, actual_mangled_name) =
            if let Some(newtype_id) = self.lookup_newtype(struct_name) {
                // First check if the newtype itself has this static method
                if self.has_static_method_direct(struct_name, method_name) {
                    (struct_name.to_string(), mangled_func_name.to_string())
                } else {
                    let base_name = match self.type_table.borrow().get(newtype_id).clone() {
                        ResolvedType::Newtype { base_type, .. } => {
                            Some(self.get_ultimate_base_struct_name(base_type))
                        }
                        ResolvedType::Flags { .. } => Some("u32".to_string()),
                        _ => None,
                    };
                    if let Some(base_name) = base_name {
                        let mangled = MethodName::format_local(&base_name, None, method_name);
                        (base_name, mangled)
                    } else {
                        (struct_name.to_string(), mangled_func_name.to_string())
                    }
                }
            } else {
                (struct_name.to_string(), mangled_func_name.to_string())
            };

        // Find trait name and the module where the impl block lives.
        // For From/TryFrom, disambiguate by matching the first argument's type so that
        // user-defined `impl From<MyType> for i32` is resolved to its actual defining module
        // rather than the default `ModuleSource::primitive()` for `i32`.
        let arg_type_hint =
            if (method_name == "from" || method_name == "try_from") && args.len() == 1 {
                Some(self.type_table.borrow().type_name(args[0].type_id))
            } else {
                None
            };
        let resolved = self.locate_static_method_impl(
            &actual_struct_name,
            method_name,
            arg_type_hint.as_deref(),
        );
        let method_ref = resolved.unwrap_or_else(|| {
            StaticMethodRef::new(
                self.find_struct_module_source(&actual_struct_name),
                &actual_struct_name,
                method_name,
                None,
            )
        });

        // Use trait-qualified mangled name if this is a trait method
        let final_mangled_name = if let Some(ref trait_name) = method_ref.trait_name {
            MethodName::format_local(&actual_struct_name, Some(trait_name), method_name)
        } else {
            actual_mangled_name
        };

        // Look up return type using the actual struct name
        let mut return_type =
            self.lookup_static_method_return_type(&method_ref, &final_mangled_name);

        // Substitute impl-level + method-level type parameters in return type.
        // `lookup_static_method_return_type` registers impl params at indices
        // 0..impl_count and method params at indices impl_count..total, so a
        // single flat substitution list `[impl_args.., method_args..]` lines
        // up correctly with `substitute_type_params` (which substitutes by index).
        if !impl_type_args.is_empty() || !method_type_args.is_empty() {
            let mut combined = impl_type_args.to_vec();
            combined.extend_from_slice(method_type_args);
            return_type = self.substitute_type_params(return_type, &combined);
        }

        // Build monomorph_info for impl-level and/or method-level generic instantiation
        let monomorph_info = if impl_type_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            Some(MonomorphInfo {
                generic_name: final_mangled_name.clone(),
                impl_type_args: impl_type_args.to_vec(),
                method_type_args: method_type_args.to_vec(),
                is_blanket: false,
            })
        };

        let param_is_mut = self.lookup_static_method_param_is_mut(&actual_struct_name, method_name);

        // Propagate #[cm("...")] from resource static methods
        let cm_name =
            self.lookup_resource_static_cm(&actual_struct_name, &method_ref.module, method_name);

        let StaticMethodRef {
            module: struct_module,
            trait_name: trait_name_opt,
            ..
        } = method_ref;

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: struct_module,
                    name: final_mangled_name,
                    monomorph_info,
                    method_info: Some({
                        let mut m = LocalMethodName::new(
                            actual_struct_name,
                            trait_name_opt,
                            method_name.to_string(),
                        );
                        m.cm_name = cm_name;
                        m
                    }),
                },
                type_args: vec![],
                args: args
                    .iter()
                    .zip(param_is_mut.iter().copied().chain(std::iter::repeat(false)))
                    .map(|(expr, is_mut)| CallArg::new(expr.clone(), is_mut))
                    .collect(),
            },
            return_type,
            span,
        )
    }
}
