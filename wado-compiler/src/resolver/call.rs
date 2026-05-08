//! Function call resolution.

use crate::hashmap::IndexMap;

use crate::ast::{self, Expr, Item, Type};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, TirExpr, TirExprKind, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::callee::{CalleeRef, StaticMethodRef};
use super::infer::InferCtx;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_call(
        &mut self,
        call: &ast::CallExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Check if this is a closure call (calling a local variable with function type)
        if let Expr::Ident(ident) = &call.callee {
            // No :: means it could be a local variable
            if !ident.name.contains("::")
                && let Some(local) = ctx.lookup(&ident.name)
            {
                // Check if the local has a function type (possibly behind references
                // or fn-type newtypes such as `type Handler = fn(...);`).
                let peeled_ref = self.type_table.borrow().peel_refs(local.type_id);
                let peeled_type_id = self.type_table.borrow().get_ultimate_base_type(peeled_ref);
                let peeled_type = self.type_table.borrow().get(peeled_type_id).clone();
                if let ResolvedType::Function {
                    params: fn_params,
                    return_type,
                    ..
                } = peeled_type
                {
                    let local_index = local.index;
                    let local_type_id = local.type_id;
                    let fn_return_type = return_type;

                    // Resolve arguments with coercion awareness based on closure param types
                    let mut args: Vec<TirExpr> = call
                        .args
                        .iter()
                        .enumerate()
                        .map(|(i, arg)| {
                            let expected_type = fn_params.get(i).copied();
                            self.resolve_expr(arg, ctx, expected_type)
                        })
                        .collect();

                    // Pad missing trailing args with closure defaults declared at `let` site.
                    if args.len() < fn_params.len() {
                        self.pad_args_with_defaults(
                            &call.callee,
                            &call.args,
                            &mut args,
                            &fn_params,
                            ctx,
                        );
                    }

                    // Arity check: function-typed variables have their defaults
                    // erased (WEP 2026-04-11), so an under-saturated indirect
                    // call is an error even if the underlying function had
                    // defaults at its definition site.
                    if args.len() != fn_params.len() {
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                            expected: fn_params.len(),
                            found: args.len(),
                            span: call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
                    }

                    // Check each argument type against expected parameter type
                    for (i, arg) in args.iter().enumerate() {
                        if let Some(&expected) = fn_params.get(i) {
                            self.typecheck(
                                arg.type_id,
                                expected,
                                call.args.get(i).map_or(call.span, ast::Expr::span),
                            );
                        }
                    }

                    // Create callee expression, auto-dereferencing if needed
                    let local_expr = TirExpr::new(
                        TirExprKind::Local {
                            index: local_index,
                            name: ident.name.clone(),
                        },
                        local_type_id,
                        ident.span,
                    );
                    let callee_expr = self.deref_to_value(local_expr, ident.span);

                    return TirExpr::new(
                        TirExprKind::IndirectCall {
                            callee: Box::new(callee_expr),
                            args,
                        },
                        fn_return_type,
                        call.span,
                    );
                }
            }
        }

        // Check if this is a field access to a function-typed field (e.g., (self.f)(arg))
        // This handles calling closures stored in struct fields
        if let Expr::FieldAccess(_field_access) = &call.callee {
            // Resolve the callee expression to get the field type
            let callee_expr = self.resolve_expr(&call.callee, ctx, None);
            let peeled_ref = self.type_table.borrow().peel_refs(callee_expr.type_id);
            let peeled_type_id = self.type_table.borrow().get_ultimate_base_type(peeled_ref);
            let peeled_type = self.type_table.borrow().get(peeled_type_id).clone();

            if let ResolvedType::Function {
                params: fn_params,
                return_type,
                ..
            } = peeled_type
            {
                let fn_return_type = return_type;

                // Resolve arguments with coercion awareness based on function param types
                let args: Vec<TirExpr> = call
                    .args
                    .iter()
                    .enumerate()
                    .map(|(i, arg)| {
                        let expected_type = fn_params.get(i).copied();
                        self.resolve_expr(arg, ctx, expected_type)
                    })
                    .collect();

                // Check each argument type against expected parameter type
                for (i, arg) in args.iter().enumerate() {
                    if let Some(&expected) = fn_params.get(i) {
                        self.typecheck(
                            arg.type_id,
                            expected,
                            call.args.get(i).map_or(call.span, ast::Expr::span),
                        );
                    }
                }

                // Auto-dereference if the callee is a reference to a function
                let callee_expr = self.deref_to_value(callee_expr, call.span);

                return TirExpr::new(
                    TirExprKind::IndirectCall {
                        callee: Box::new(callee_expr),
                        args,
                    },
                    fn_return_type,
                    call.span,
                );
            }
        }

        // First, determine expected parameter types to handle coercion
        let mut param_types = self.lookup_function_param_types(&call.callee);

        // For variant constructors with type args (e.g., Option::<Array<u8>>::Some([])),
        // compute substituted payload type so literal coercion works on first resolve.
        if param_types.is_empty()
            && let Expr::Ident(ident) = &call.callee
            && let Some(pos) = ident.name.find("::")
        {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];
            if let Some(variant_info) = self.lookup_variant_case(prefix).cloned()
                && let Some((_, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == suffix)
            {
                let payload_is_unit = matches!(
                    self.type_table.borrow().get(case_data.payload),
                    ResolvedType::Unit
                );
                if !payload_is_unit {
                    let variant_type_args: Vec<TypeId> = call
                        .type_args
                        .iter()
                        .map(|ty| self.resolve_type(ty))
                        .collect();
                    let mut payload_type = case_data.payload;
                    if !variant_type_args.is_empty() {
                        payload_type =
                            self.substitute_type_params(payload_type, &variant_type_args);
                    } else if let Some(expected) = expected_type {
                        // Infer type args from expected type (e.g. Option::Some(null) expecting Option<Option<i32>>)
                        let expected_resolved = self.type_table.borrow().get(expected).clone();
                        if let ResolvedType::GenericInstance {
                            name: expected_name,
                            type_args: expected_args,
                            ..
                        } = expected_resolved
                            && expected_name == prefix
                            && expected_args.len() == variant_info.type_param_type_ids.len()
                        {
                            payload_type =
                                self.substitute_type_params(payload_type, &expected_args);
                        }
                    }
                    param_types.push(payload_type);
                }
            }
        }

        // Resolve arguments with coercion awareness
        let mut args: Vec<TirExpr> = call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = param_types.get(i).copied();
                self.resolve_expr(arg, ctx, expected_type)
            })
            .collect();

        // Resolve the callee's identity. `Some(CalleeRef)` means we know
        // both the defining module and the name-as-defined; `None` means the
        // call target could not be resolved and we'll emit `UnknownFunction`.
        // `display_name` is always populated for diagnostics.
        let (callee_opt, display_name): (Option<CalleeRef>, String) = match &call.callee {
            Expr::Ident(ident) => {
                // Check for qualified name with :: (e.g., "Stdout::write_via_stream")
                // Parser creates a single ident for Effect::operation syntax
                if let Some(pos) = ident.name.find("::") {
                    let prefix = &ident.name[..pos];
                    let suffix = &ident.name[pos + 2..];

                    // Builtin functions: resolve through core:builtin module
                    if prefix == "builtin" {
                        (
                            Some(CalleeRef::new(ModuleSource::builtin(), suffix)),
                            ident.name.clone(),
                        )
                    }
                    // Self::method() — resolve Self to the concrete type name
                    else if prefix == "Self"
                        && let Some(self_type_id) = self.trait_ctx.self_type
                    {
                        let self_name = self.type_table.borrow().type_name(self_type_id);
                        let synthetic_call = ast::CallExpr {
                            id: call.id,
                            callee: Expr::Ident(ast::IdentExpr {
                                id: ident.id,
                                name: format!("{self_name}::{suffix}"),
                                segments: Vec::new(),
                                span: ident.span,
                            }),
                            type_args: call.type_args.clone(),
                            args: call.args.clone(),
                            has_trailing_comma: call.has_trailing_comma,
                            span: call.span,
                        };
                        return self.resolve_call(&synthetic_call, ctx, expected_type);
                    }
                    // Check if this is a type parameter static call (T::method where T: Trait)
                    else if let Some(&(_param_idx, type_param_type_id)) =
                        self.trait_ctx.type_params.get(prefix)
                    {
                        // If the "type parameter" is actually bound to a concrete
                        // type (e.g., trait default method synthesized for an
                        // impl binds the trait's `T` to the impl's concrete arg),
                        // dispatch statically on that concrete type instead of
                        // emitting a `T^Trait::method` call that would need a
                        // monomorphizer substitution that never arrives.
                        let is_abstract_type_param = matches!(
                            self.type_table.borrow().get(type_param_type_id),
                            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
                        );
                        if !is_abstract_type_param {
                            let concrete_name = self
                                .type_table
                                .borrow()
                                .mangle_type_name(type_param_type_id);
                            let new_name = format!("{concrete_name}::{suffix}");
                            let synthetic_call = ast::CallExpr {
                                id: call.id,
                                callee: Expr::Ident(ast::IdentExpr {
                                    id: ident.id,
                                    name: new_name,
                                    segments: Vec::new(),
                                    span: ident.span,
                                }),
                                type_args: call.type_args.clone(),
                                args: call.args.clone(),
                                has_trailing_comma: call.has_trailing_comma,
                                span: call.span,
                            };
                            return self.resolve_call(&synthetic_call, ctx, expected_type);
                        }
                        return self.resolve_type_param_static_call(
                            prefix,
                            suffix,
                            type_param_type_id,
                            &args,
                            call,
                            ctx,
                        );
                    }
                    // Check if this is a static method call (Type::method)
                    // Static methods are registered with mangled names "Type::method"
                    else if self.is_static_method(prefix, suffix) {
                        // Record the receiver-type segment (prefix) as a reference to
                        // the type's decl.
                        if let Some(prefix_seg) = ident.segments.first() {
                            self.record_item_reference_by_name(prefix_seg.id, prefix);
                        }
                        // Record the method segment (suffix) as a reference to the
                        // impl-block method's AstId in its defining module. Try the
                        // actual impl module first (trait-qualified resolution),
                        // falling back to the struct's own module.
                        if let Some(suffix_seg) = ident.segments.get(1) {
                            let arg_hint =
                                if (suffix == "from" || suffix == "try_from") && args.len() == 1 {
                                    Some(self.type_table.borrow().type_name(args[0].type_id))
                                } else {
                                    None
                                };
                            let method_module = self
                                .locate_static_method_impl(prefix, suffix, arg_hint.as_deref())
                                .map_or_else(
                                    || self.find_struct_module_source(prefix),
                                    |r| r.module,
                                );
                            if let Some(method_ast_id) =
                                self.find_impl_method_ast_id(&method_module, prefix, suffix)
                            {
                                self.record_reference_to_decl(
                                    suffix_seg.id,
                                    &method_module,
                                    method_ast_id,
                                );
                            }
                        }
                        // Resolve method-level type args (e.g., i32::deserialize::<MockDeserializer>)
                        let mut method_type_args: Vec<TypeId> = call
                            .type_args
                            .iter()
                            .map(|ty| self.resolve_type(ty))
                            .collect();
                        // Impl-level type args inferred from the LHS / receiver type.
                        // Only populated by `infer_static_method_type_args`; the
                        // explicit `call.type_args` only carries method-level args.
                        let mut impl_type_args_inferred: Vec<TypeId> = Vec::new();
                        // If no explicit type args, try to infer from argument types.
                        // This is an exploratory probe: the `suffix` may match a
                        // builtin-registry entry (e.g. `builtin::foo`) keyed by bare
                        // name; otherwise the local-module lookup almost never
                        // succeeds and `infer_static_method_type_args` below runs.
                        let mangled_name = MethodName::format_local(prefix, None, suffix);
                        if method_type_args.is_empty() {
                            let probe =
                                CalleeRef::local(&self.current_module_source, suffix.to_string());
                            method_type_args =
                                self.infer_fn_type_args(&probe, &call.args, &args, expected_type);
                        }
                        if method_type_args.is_empty() {
                            let (impl_args, method_args) = self.infer_static_method_type_args(
                                prefix,
                                suffix,
                                &call.args,
                                &args,
                                expected_type,
                            );
                            impl_type_args_inferred = impl_args;
                            method_type_args = method_args;
                        }
                        // Check trait bounds and register assoc type resolutions for inferred type args
                        if !method_type_args.is_empty() {
                            let mtype_params =
                                self.lookup_static_method_type_params(prefix, suffix);
                            for (i, param) in mtype_params.iter().enumerate() {
                                if let Some(&type_arg) = method_type_args.get(i) {
                                    for bound in &param.bounds {
                                        if self.type_implements_trait(type_arg, &bound.name) {
                                            self.register_assoc_types_for_concrete_type_and_trait(
                                                type_arg,
                                                &bound.name.clone(),
                                            );
                                        } else {
                                            let type_name = self.type_id_to_string(type_arg);
                                            let _ = self.logger.error(
                                                TypeError::TraitBoundNotSatisfied {
                                                    type_name,
                                                    trait_name: bound.name.clone(),
                                                    param_name: param.name.clone(),
                                                    span: call.span,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        // Handle From conversions with no explicit impl: reflexive and newtype.
                        if suffix == "from" && args.len() == 1 {
                            let arg_type = args[0].type_id;
                            let arg_type_name = self.type_table.borrow().type_name(arg_type);

                            // Reflexive: T::from(T_val) — identity conversion
                            if arg_type_name == prefix {
                                return args[0].clone();
                            }

                            // Newtype→Base: u64::from(UserId_val) where type UserId = u64
                            let base_of_arg = self.type_table.borrow().get_newtype_base(arg_type);
                            if let Some(base_id) = base_of_arg
                                && self.type_table.borrow().type_name(base_id) == prefix
                            {
                                return TirExpr::new(
                                    TirExprKind::Cast {
                                        expr: Box::new(args[0].clone()),
                                        target_type: base_id,
                                    },
                                    base_id,
                                    call.span,
                                );
                            }

                            // Base→Newtype: UserId::from(u64_val) where type UserId = u64
                            if let Some(newtype_type_id) = self.lookup_newtype(prefix) {
                                let base_opt =
                                    self.type_table.borrow().get_newtype_base(newtype_type_id);
                                if let Some(base_id) = base_opt
                                    && self.type_table.borrow().type_name(base_id) == arg_type_name
                                {
                                    return TirExpr::new(
                                        TirExprKind::Cast {
                                            expr: Box::new(args[0].clone()),
                                            target_type: newtype_type_id,
                                        },
                                        newtype_type_id,
                                        call.span,
                                    );
                                }
                            }
                        }

                        // Re-coerce literal-number args to inferred parameter types and
                        // typecheck each arg against the substituted parameter type.
                        // Before inference, the literal args were resolved with `TypeParam`
                        // (or `Unknown`) as the expected type, so they fell back to defaults
                        // (i32/f64). Now that we know the concrete substitution, retry
                        // coercion and verify the inferred type-arg binding is consistent
                        // with every arg (e.g. `two_static<T>(1 as u8, 2 as u32)` must
                        // fail because `T` cannot be both `u8` and `u32`).
                        if !method_type_args.is_empty() || !impl_type_args_inferred.is_empty() {
                            let raw_param_types =
                                self.lookup_static_method_param_types(prefix, suffix);
                            let mut combined_type_args = impl_type_args_inferred.clone();
                            combined_type_args.extend_from_slice(&method_type_args);
                            let substituted: Vec<TypeId> = raw_param_types
                                .iter()
                                .map(|&t| self.substitute_type_params(t, &combined_type_args))
                                .collect();
                            self.recoerce_literal_args(&call.args, &mut args, &substituted);
                            for (i, arg) in args.iter().enumerate() {
                                if let Some(&expected) = substituted.get(i) {
                                    self.typecheck(
                                        arg.type_id,
                                        expected,
                                        call.args.get(i).map_or(call.span, ast::Expr::span),
                                    );
                                }
                            }
                        }

                        return self.resolve_static_method_call_from_qualified(
                            prefix,
                            suffix,
                            &mangled_name,
                            &args,
                            &impl_type_args_inferred,
                            &method_type_args,
                            call.span,
                            ctx,
                        );
                    }
                    // Check if this is a flags type method call: Perms::none(), Perms::all()
                    else if let Some(flags_info) = self.lookup_flags_case(prefix).cloned()
                        && matches!(suffix, "none" | "all")
                    {
                        if let Some(prefix_seg) = ident.segments.first() {
                            self.record_item_reference_by_name(prefix_seg.id, prefix);
                        }
                        let member_count = flags_info.members.len();
                        let value: u64 = match suffix {
                            "none" => 0,
                            "all" => u64::from((1u32 << member_count) - 1),
                            _ => unreachable!(),
                        };
                        return TirExpr::new(
                            TirExprKind::IntLiteral {
                                value,
                                repr: value.to_string(),
                            },
                            flags_info.type_id,
                            call.span,
                        );
                    }
                    // Check if this is a variant case construction (Color::Red)
                    else if let Some(variant_info) = self.lookup_variant_case(prefix) {
                        // Clone needed data to release the borrow on self
                        let variant_info = variant_info.clone();
                        let case_match = variant_info
                            .cases
                            .iter()
                            .enumerate()
                            .find(|(_, c)| c.name == suffix)
                            .map(|(i, c)| (i, c.clone()));
                        let prefix_owned = prefix.to_string();

                        // Find the case by name
                        if let Some((case_index, case_data)) = case_match {
                            self.record_qualified_case(
                                ident,
                                &prefix_owned,
                                &variant_info.module_source,
                                case_data.ast_id,
                            );
                            // Each variant case has exactly one payload.
                            // Unit variants expect 0 args, non-unit variants expect 1 arg.
                            let payload_is_unit = matches!(
                                self.type_table.borrow().get(case_data.payload),
                                ResolvedType::Unit
                            );
                            let expected_args = usize::from(!payload_is_unit);

                            if args.len() != expected_args {
                                let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                                    expected: expected_args,
                                    found: args.len(),
                                    span: call.span,
                                });
                                return TirExpr::new(
                                    TirExprKind::Unit,
                                    TypeTable::ERROR,
                                    call.span,
                                );
                            }

                            let payload = args.into_iter().next().map(Box::new);

                            // Infer variant type: use GenericInstance for generic variants
                            let variant_type = if variant_info.type_params.is_empty() {
                                self.type_table.borrow_mut().make_variant(
                                    variant_info.name.clone(),
                                    variant_info.module_source.clone(),
                                )
                            } else {
                                self.infer_variant_type_args(
                                    &prefix_owned,
                                    &variant_info,
                                    &case_data,
                                    payload.as_deref(),
                                    expected_type,
                                )
                            };

                            return TirExpr::new(
                                TirExprKind::VariantConstruct {
                                    variant_type,
                                    case_index: case_index as u32,
                                    case_name: case_data.name.clone(),
                                    payload,
                                },
                                variant_type,
                                call.span,
                            );
                        }
                        // If no matching case, check for From<T> synthesis requests
                        else if suffix == "from" && args.len() == 1 {
                            let target_type_id = self.type_table.borrow_mut().make_variant(
                                variant_info.name.clone(),
                                variant_info.module_source.clone(),
                            );
                            let from_type = args[0].type_id;
                            let from_type_name = self.type_table.borrow().type_name(from_type);
                            let matching_impl = self.current_module_items.iter().any(|item| {
                                if let Item::Impl(impl_block) = item
                                    && impl_block.is_synthesize_request
                                    && let Some(trait_type) = &impl_block.trait_type
                                    && Self::get_type_name_static(trait_type) == "From"
                                    && Self::get_type_name_static(&impl_block.ty) == prefix
                                {
                                    if let ast::Type::Generic(generic) = trait_type
                                        && generic.args.len() == 1
                                    {
                                        self.get_type_name_full(&generic.args[0]) == from_type_name
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            });
                            if matching_impl {
                                return self.resolve_from_call(
                                    target_type_id,
                                    from_type,
                                    args.into_iter().next().unwrap(),
                                    call.span,
                                );
                            }
                            let _ = self.logger.error(TypeError::UnknownFunction {
                                name: format!("{prefix}::{suffix}"),
                                span: call.span,
                            });
                            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
                        } else {
                            // Unknown case name
                            let _ = self.logger.error(TypeError::UnknownFunction {
                                name: format!("{prefix}::{suffix}"),
                                span: call.span,
                            });
                            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
                        }
                    }
                    // If prefix is a known type (struct/enum/newtype/flags) with no matching
                    // static method, emit a compile error.
                    else if self.is_known_type_name(prefix) {
                        let _ = self.logger.error(TypeError::UnknownFunction {
                            name: format!("{prefix}::{suffix}"),
                            span: call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
                    }
                    // Namespace import: `use ns from "..."` then `ns::Type::method()`
                    // or `ns::VariantType::Case(...)`.
                    else if let Some(ns_source) = self.namespace_imports.get(prefix).cloned() {
                        // suffix may be "Type::method" or plain "func"
                        if let Some(inner_pos) = suffix.find("::") {
                            let type_name = &suffix[..inner_pos];
                            let method_name = &suffix[inner_pos + 2..];

                            // Check if this is a variant construction in the namespace
                            let ns_variant = self
                                .all_variant_cases
                                .get(&ns_source)
                                .and_then(|m| m.get(type_name))
                                .cloned();
                            if let Some(variant_info) = ns_variant {
                                let case_match = variant_info
                                    .cases
                                    .iter()
                                    .enumerate()
                                    .find(|(_, c)| c.name == method_name)
                                    .map(|(i, c)| (i, c.clone()));
                                if let Some((case_index, case_data)) = case_match {
                                    self.record_namespaced_case(
                                        ident,
                                        &variant_info.module_source,
                                        case_data.ast_id,
                                    );
                                    let payload_is_unit = matches!(
                                        self.type_table.borrow().get(case_data.payload),
                                        ResolvedType::Unit
                                    );
                                    let expected_args = usize::from(!payload_is_unit);
                                    if args.len() != expected_args {
                                        let _ =
                                            self.logger.error(TypeError::ArgumentCountMismatch {
                                                expected: expected_args,
                                                found: args.len(),
                                                span: call.span,
                                            });
                                        return TirExpr::new(
                                            TirExprKind::Unit,
                                            TypeTable::ERROR,
                                            call.span,
                                        );
                                    }
                                    let payload = args.into_iter().next().map(Box::new);
                                    let variant_type = if variant_info.type_params.is_empty() {
                                        self.type_table.borrow_mut().make_variant(
                                            variant_info.name.clone(),
                                            variant_info.module_source.clone(),
                                        )
                                    } else {
                                        self.infer_variant_type_args(
                                            type_name,
                                            &variant_info,
                                            &case_data,
                                            payload.as_deref(),
                                            expected_type,
                                        )
                                    };
                                    return TirExpr::new(
                                        TirExprKind::VariantConstruct {
                                            variant_type,
                                            case_index: case_index as u32,
                                            case_name: case_data.name.clone(),
                                            payload,
                                        },
                                        variant_type,
                                        call.span,
                                    );
                                }
                            }

                            // Static method call on a type from the namespace module.
                            // Use the namespace module source so codegen finds the right impl.
                            let mangled_name =
                                MethodName::format_local(type_name, None, method_name);
                            let method_type_args: Vec<TypeId> = call
                                .type_args
                                .iter()
                                .map(|ty| self.resolve_type(ty))
                                .collect();

                            // Find the impl module via the trait env (global index)
                            let arg_type_hint = if (method_name == "from"
                                || method_name == "try_from")
                                && args.len() == 1
                            {
                                Some(self.type_table.borrow().type_name(args[0].type_id))
                            } else {
                                None
                            };
                            let resolved = self.locate_static_method_impl(
                                type_name,
                                method_name,
                                arg_type_hint.as_deref(),
                            );
                            let method_ref = resolved.unwrap_or_else(|| {
                                StaticMethodRef::new(
                                    ns_source.clone(),
                                    type_name,
                                    method_name,
                                    None,
                                )
                            });
                            let trait_name = method_ref.trait_name.clone();
                            let struct_module = method_ref.module.clone();

                            let final_mangled = if let Some(ref tn) = method_ref.trait_name {
                                MethodName::format_local(type_name, Some(tn), method_name)
                            } else {
                                mangled_name
                            };

                            let mut return_type =
                                self.lookup_static_method_return_type(&method_ref, &final_mangled);
                            if !method_type_args.is_empty() {
                                return_type =
                                    self.substitute_type_params(return_type, &method_type_args);
                            }

                            let monomorph_info = if method_type_args.is_empty() {
                                None
                            } else {
                                Some(MonomorphInfo {
                                    generic_name: final_mangled.clone(),
                                    impl_type_args: vec![],
                                    method_type_args: method_type_args.clone(),
                                    is_blanket: false,
                                })
                            };

                            let param_is_mut =
                                self.lookup_static_method_param_is_mut(type_name, method_name);
                            let call_args: Vec<CallArg> = args
                                .into_iter()
                                .zip(param_is_mut.into_iter().chain(std::iter::repeat(false)))
                                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                                .collect();

                            return TirExpr::new(
                                TirExprKind::Call {
                                    func: FunctionRef {
                                        module_source: struct_module,
                                        name: final_mangled,
                                        monomorph_info,
                                        method_info: Some(LocalMethodName::new(
                                            type_name.to_string(),
                                            trait_name,
                                            method_name.to_string(),
                                        )),
                                    },
                                    type_args: vec![],
                                    args: call_args,
                                },
                                return_type,
                                call.span,
                            );
                        }
                        (Some(CalleeRef::new(ns_source, suffix)), ident.name.clone())
                    }
                    // Effect operations and module namespace calls - pass through to codegen.
                    // This covers Stdout::write(), etc.
                    else {
                        (
                            Some(CalleeRef::local_namespace(
                                &mut self.interner.borrow_mut(),
                                prefix,
                                suffix,
                            )),
                            ident.name.clone(),
                        )
                    }
                }
                // Check if it's a local function (defined in this module) or
                // a built-in type constructor (Ok, Err, Some, None)
                else if self.function_return_types.contains_key(&ident.name)
                    || matches!(ident.name.as_str(), "Ok" | "Err" | "Some" | "None")
                {
                    self.record_item_reference_by_name(ident.id, &ident.name);
                    (
                        Some(CalleeRef::local(
                            &self.current_module_source,
                            ident.name.clone(),
                        )),
                        ident.name.clone(),
                    )
                }
                // Check for prelude functions (panic, unreachable)
                // These are defined in core:internal and re-exported by core:prelude
                else if matches!(ident.name.as_str(), "panic" | "unreachable") {
                    (
                        Some(CalleeRef::internal_prelude(&ident.name)),
                        ident.name.clone(),
                    )
                }
                // Check if this is an imported function (per-module imports).
                // We go through the `Symbol` directly so both the use→def edge
                // and the `CalleeRef` come from the same resolution — this is
                // the single place the alias→defining-name translation happens.
                else if self.imported_functions.contains(&ident.name) {
                    if let Some(symbol) = self.symbols.lookup(&ident.name) {
                        self.record_reference_to_key(ident.id, symbol.defined_at.clone());
                        (
                            Some(CalleeRef::from_imported_symbol(symbol)),
                            ident.name.clone(),
                        )
                    } else {
                        // Imported but not in symbols - shouldn't happen but allow
                        (
                            Some(CalleeRef::local(
                                &self.current_module_source,
                                ident.name.clone(),
                            )),
                            ident.name.clone(),
                        )
                    }
                } else {
                    // Unknown function - will report error
                    (None, ident.name.clone())
                }
            }
            Expr::FieldAccess(field_access) => {
                // e.g., Stdout.write (unlikely but possible)
                // These are always considered known - validated elsewhere
                if let Expr::Ident(ident) = &field_access.expr {
                    (
                        Some(CalleeRef::local_namespace(
                            &mut self.interner.borrow_mut(),
                            &ident.name,
                            field_access.field.clone(),
                        )),
                        format!("{}.{}", ident.name, field_access.field),
                    )
                } else {
                    (None, String::from("unknown"))
                }
            }
            _ => (None, String::from("unknown")),
        };

        // Resolve the callee down to a single `CalleeRef`. For unknown
        // callees we emit `UnknownFunction` and fall back to a sentinel in
        // the current module so downstream lookups return empty safely.
        let callee = if let Some(c) = callee_opt {
            c
        } else {
            let _ = self.logger.error(TypeError::UnknownFunction {
                name: display_name.clone(),
                span: call.span,
            });
            CalleeRef::local(&self.current_module_source, display_name)
        };

        // Resolve explicit type arguments
        let mut type_args: Vec<TypeId> = call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        // If no explicit type args, try to infer from argument types
        if type_args.is_empty() {
            type_args = self.infer_fn_type_args(&callee, &call.args, &args, expected_type);
        }

        // Check trait bounds on function type arguments
        if !type_args.is_empty() {
            self.check_function_type_arg_bounds(&callee, &type_args, call.span);
        }

        // Look up function return type
        let mut return_type = self.lookup_function_return_type(&callee);

        // If we have explicit type args, substitute type parameters in the return type
        if !type_args.is_empty() {
            return_type = self.substitute_type_params(return_type, &type_args);
        }

        // Check each argument: reject &T/&mut T passed where non-ref is expected.
        // For generic functions with explicit type args, rebuild param types with
        // type params substituted so UNKNOWN params become concrete types.
        let check_param_types = if type_args.is_empty() {
            param_types
        } else {
            self.lookup_function_param_types_with_type_args(&call.callee, &type_args)
        };
        if !check_param_types.is_empty() && args.len() < check_param_types.len() {
            self.pad_args_with_defaults(
                &call.callee,
                &call.args,
                &mut args,
                &check_param_types,
                ctx,
            );
        }
        if !check_param_types.is_empty() && args.len() != check_param_types.len() {
            let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                expected: check_param_types.len(),
                found: args.len(),
                span: call.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
        }
        // Re-coerce literal-number args to inferred parameter types. This catches
        // calls like `two<T>(1 as u8, 2)` where the literal `2` was first resolved
        // as the default i32 because the original expected type was a TypeParam.
        if !type_args.is_empty() {
            self.recoerce_literal_args(&call.args, &mut args, &check_param_types);
        }
        for (i, arg) in args.iter().enumerate() {
            if let Some(&expected) = check_param_types.get(i) {
                self.typecheck(
                    arg.type_id,
                    expected,
                    call.args.get(i).map_or(call.span, ast::Expr::span),
                );
            }
        }

        let param_is_mut = self.lookup_function_param_is_mut(&call.callee);
        let call_args: Vec<CallArg> = args
            .into_iter()
            .zip(param_is_mut.into_iter().chain(std::iter::repeat(false)))
            .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
            .collect();
        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: callee.module,
                    name: callee.name,
                    monomorph_info: None,
                    method_info: None, // Free function call,
                },
                type_args,
                args: call_args,
            },
            return_type,
            call.span,
        )
    }

    /// Look up the return type of a function
    pub(super) fn lookup_function_return_type(&mut self, callee: &CalleeRef) -> TypeId {
        let callee_module = &callee.module;
        let func_name = callee.name.as_str();
        // Handle builtin functions
        if callee_module.is_core_builtin() {
            return self.get_builtin_return_type(func_name);
        }
        // Legacy: builtin::name pattern
        if let Some(builtin_name) = func_name.strip_prefix("builtin::") {
            return self.get_builtin_return_type(builtin_name);
        }

        // Handle WASI effect operations (e.g., Environment::get_arguments)
        if callee_module.is_effect_like()
            && let Some(interface_name) = callee_module.interface_name()
            && let Some(return_type) = self.get_wasi_effect_return_type(&interface_name, func_name)
        {
            return return_type;
        }

        // Handle user-defined effect operations (e.g., `Counter::next` where
        // `effect Counter { fn next() -> i32; }` is declared in the project).
        // The resolver routes these through `CalleeRef::local_namespace`,
        // which produces `ModuleSource::Local { path: "Counter" }` — also
        // matched by `is_effect_like()`. Without this lookup the call would
        // fall through to the default `Unit` return type and the dispatch
        // synthesis would have to patch up every Let / template that depends
        // on the value.
        if callee_module.is_effect_like()
            && let Some(interface_name) = callee_module.interface_name()
            && let Some(return_type) = self.get_user_effect_return_type(&interface_name, func_name)
        {
            return return_type;
        }

        // First, try local functions (entry point module)
        if callee_module.is_entry_point()
            && let Some(&return_type) = self.function_return_types.get(func_name)
        {
            return return_type;
        }

        // Try looking up in loaded modules
        if !callee_module.is_entry_point() {
            // Clone the return type AST and type params to avoid borrow issues
            let func_info = Self::lookup_func_in_loaded_module(
                self.loaded_modules,
                &self.loaded_module_func_indices,
                callee_module,
                func_name,
            )
            .map(|func| (func.return_type.clone(), func.type_params.clone()));

            if let Some((ty, type_params)) = func_info
                && let Some(return_type_ast) = ty
            {
                // Build the callee module's import context so type names in the
                // callee's signature resolve to the callee's types, not the
                // caller's (which may have same-named different types).
                let callee_module_ast = self.loaded_modules.get(callee_module);
                let (callee_imported, callee_original_names) = callee_module_ast.map_or_else(
                    || (IndexMap::default(), IndexMap::default()),
                    |m| {
                        Self::build_imported_type_sources(
                            &mut self.interner.borrow_mut(),
                            m,
                            callee_module,
                            Some(&self.entry_module_source),
                            &self.invocations,
                        )
                    },
                );

                // Set up the function's type parameters in an inherited scope so we
                // can resolve type parameter references (like T -> TypeParam { index: 0 }).
                let mut scope = self.enter_inherited_type_param_scope();
                scope.trait_ctx.type_params.clear();
                scope.register_generic_params(&type_params, 0);

                // Swap the resolver's "current module" perspective onto the
                // callee for the duration of `resolve_type`. Locals are cleared
                // because they only ever describe the active resolution, not
                // the callee's pre-existing definitions.
                let resolved = scope.with_module_perspective(
                    callee_module.clone(),
                    callee_imported,
                    callee_original_names,
                    |s| s.resolve_type(&return_type_ast),
                );
                drop(scope);

                return resolved;
            }
        }

        // Default to UNIT for unknown functions (they might be external/builtin)
        TypeTable::UNIT
    }

    /// Get the return type of a WASI effect operation from the registry.
    ///
    /// The registry stores the CM-ABI return type (with any `AsyncCall<T>`
    /// wrapper stripped at registration). For async imports the Wado
    /// surface type is `AsyncCall<T>`, so re-wrap before returning.
    /// Look up the declared return type of a user-defined effect's
    /// operation, e.g. `Counter::next` for `effect Counter { fn next()
    /// -> i32; }`. The effect must be a project-declared effect (in
    /// `trait_env.effect_decl_index`); WASI effects flow through
    /// `get_wasi_effect_return_type` instead.
    pub(super) fn get_user_effect_return_type(
        &mut self,
        effect: &str,
        operation: &str,
    ) -> Option<TypeId> {
        let (module_source, item_idx) = self.trait_env.effect_decl_index.get(effect)?.clone();
        let module = self.loaded_modules.get(&module_source)?;
        let crate::ast::Item::Interface(decl) = module.items.get(item_idx)? else {
            return None;
        };
        let method = decl.methods.iter().find(|m| m.name == operation)?;
        let return_ast = method.return_type.as_ref()?;
        // Resolve in the effect's defining module so any types it
        // references are looked up in the right scope.
        let saved = std::mem::replace(&mut self.current_module_source, module_source);
        let return_type = self.resolve_type(return_ast);
        self.current_module_source = saved;
        Some(return_type)
    }

    pub(super) fn get_wasi_effect_return_type(
        &mut self,
        effect: &str,
        operation: &str,
    ) -> Option<TypeId> {
        let func_key = format!("{effect}::{operation}");
        let func = self.wasi_registry.get_function(&func_key)?;
        let is_async = func.is_async;
        let package = func.package.clone();

        let inner = match func.return_type.clone() {
            Some(ty) => self.resolve_wasi_type_scoped(&ty, Some(&package)),
            None => TypeTable::UNIT,
        };

        if is_async {
            Some(self.type_table.borrow_mut().make_async_call(inner))
        } else if func.return_type.is_some() {
            Some(inner)
        } else {
            None
        }
    }

    /// Resolve a WASI AST type to a `TypeId`
    /// Resolve a WASI AST type to a `TypeId`, with optional WASI package scope.
    pub(super) fn resolve_wasi_type_scoped(
        &mut self,
        ty: &Type,
        wasi_package: Option<&str>,
    ) -> TypeId {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "String" => self.get_string_struct_type(),
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
                "v128" => TypeTable::V128,
                "()" => TypeTable::UNIT,
                // Type aliases from WASI (e.g., Mark, Instant, Duration)
                _ => {
                    // First check if it's a registered newtype in newtypes
                    if let Some(newtype_id) = self.lookup_newtype(&named.name) {
                        return newtype_id;
                    }
                    // Otherwise, try to resolve via WASI registry's newtypes.
                    // Scoped to `wasi:` to keep this resolver path WASI-only.
                    let aliased = self
                        .wasi_registry
                        .find_wasi_newtype_source(&named.name)
                        .and_then(|src| self.wasi_registry.get_newtype_by_source(src, &named.name))
                        .cloned();
                    if let Some(aliased) = aliased {
                        // Create a newtype for this WASI newtype
                        let base_type = self.resolve_wasi_type_scoped(&aliased, wasi_package);
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            named.name.clone(),
                            ModuleSource::wasi_clocks(),
                            base_type,
                        );
                        // Cache the newtype for future lookups
                        self.local_newtypes.insert(named.name.clone(), newtype_id);
                        newtype_id
                    } else if self
                        .wasi_registry
                        .find_wasi_resource_source(&named.name)
                        .is_some()
                    {
                        // WASI resource type - create a proper Resource TypeId so that
                        // method calls on the returned handle resolve correctly.
                        // Look up the actual module source from all_resource_types.
                        let module_source = self
                            .all_resource_types
                            .iter()
                            .find_map(|(ms, map)| {
                                if map.contains_key(&named.name) {
                                    Some(ms.clone())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_else(ModuleSource::wasi_filesystem);
                        self.type_table
                            .borrow_mut()
                            .make_resource(named.name.clone(), module_source)
                    } else if let Some(pkg) = wasi_package {
                        // Scoped lookup: use type table's package-aware search
                        let tt = self.type_table.borrow();
                        if let Some(tid) = tt.find_named_type_by_cm_package(&named.name, pkg) {
                            tid
                        } else {
                            drop(tt);
                            // Fallback: enum → struct → i32. Scoped to wasi:
                            // via find_wasi_*_source so same-named core:*
                            // types cannot leak in.
                            if self
                                .wasi_registry
                                .find_wasi_enum_source(&named.name)
                                .is_some()
                            {
                                let module_source = self
                                    .all_enum_cases
                                    .iter()
                                    .find_map(|(ms, map)| {
                                        map.contains_key(&named.name).then(|| ms.clone())
                                    })
                                    .unwrap_or_else(ModuleSource::wasi_cli);
                                self.type_table
                                    .borrow_mut()
                                    .make_enum(named.name.clone(), module_source)
                            } else if self
                                .wasi_registry
                                .find_wasi_struct_source(&named.name)
                                .is_some()
                            {
                                let module_source = self
                                    .all_struct_fields
                                    .iter()
                                    .find_map(|(ms, map)| {
                                        map.contains_key(&named.name).then(|| ms.clone())
                                    })
                                    .unwrap_or_else(ModuleSource::wasi_clocks);
                                self.type_table
                                    .borrow_mut()
                                    .make_struct(named.name.clone(), module_source)
                            } else {
                                TypeTable::I32
                            }
                        }
                    } else if self
                        .wasi_registry
                        .find_wasi_enum_source(&named.name)
                        .is_some()
                    {
                        // WASI enum type - look up the module source from all_enum_cases
                        let module_source = self
                            .all_enum_cases
                            .iter()
                            .find_map(|(ms, map)| {
                                if map.contains_key(&named.name) {
                                    Some(ms.clone())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_else(ModuleSource::wasi_cli);
                        self.type_table
                            .borrow_mut()
                            .make_enum(named.name.clone(), module_source)
                    } else if self
                        .wasi_registry
                        .find_wasi_struct_source(&named.name)
                        .is_some()
                    {
                        // WASI struct (record) type - look up module source from all_struct_fields
                        let module_source = self
                            .all_struct_fields
                            .iter()
                            .find_map(|(ms, map)| {
                                if map.contains_key(&named.name) {
                                    Some(ms.clone())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_else(ModuleSource::wasi_clocks);
                        self.type_table
                            .borrow_mut()
                            .make_struct(named.name.clone(), module_source)
                    } else {
                        // Unknown type - represent as i32 handle
                        TypeTable::I32
                    }
                }
            },
            Type::Generic(generic) => match generic.name.as_str() {
                "Array" if generic.args.len() == 1 => {
                    let elem_type = self.resolve_wasi_type_scoped(&generic.args[0], wasi_package);
                    self.type_table.borrow_mut().make_array(elem_type)
                }
                "Option" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type_scoped(&generic.args[0], wasi_package);
                    self.type_table.borrow_mut().make_option(inner_type)
                }
                "Stream" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type_scoped(&generic.args[0], wasi_package);
                    self.type_table.borrow_mut().make_stream(inner_type)
                }
                "Future" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type_scoped(&generic.args[0], wasi_package);
                    self.type_table.borrow_mut().make_future(inner_type)
                }
                "AsyncCall" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type_scoped(&generic.args[0], wasi_package);
                    self.type_table.borrow_mut().make_async_call(inner_type)
                }
                "Result" if generic.args.len() == 2 => {
                    let ok_type = self.resolve_wasi_type_scoped(&generic.args[0], wasi_package);
                    let err_type = self.resolve_wasi_type_scoped(&generic.args[1], wasi_package);
                    // Look up the module source where Result variant is defined
                    let module_source = self
                        .all_variant_cases
                        .iter()
                        .find_map(|(ms, map)| {
                            if map.contains_key("Result") {
                                Some(ms.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(ModuleSource::prelude);
                    self.type_table.borrow_mut().make_generic_instance(
                        "Result".to_string(),
                        module_source,
                        vec![ok_type, err_type],
                    )
                }
                _ => TypeTable::UNIT,
            },
            Type::Tuple(types) => {
                if types.is_empty() {
                    return TypeTable::UNIT;
                }
                let resolved: Vec<TypeId> = types
                    .iter()
                    .map(|t| self.resolve_wasi_type_scoped(t, wasi_package))
                    .collect();
                self.type_table.borrow_mut().make_tuple(resolved)
            }
            _ => TypeTable::UNIT,
        }
    }

    /// Get the String struct type (from core:prelude/string.wado)
    pub(super) fn get_string_struct_type(&mut self) -> TypeId {
        self.type_table
            .borrow_mut()
            .make_struct("String".to_string(), ModuleSource::string())
    }

    /// Build a `from_pair` call for i128/u128 large literal construction
    pub(super) fn build_from_pair_call(
        &self,
        type_name: &str,
        low: u64,
        high: i64,
        target_type: TypeId,
        span: Span,
    ) -> TirExpr {
        let low_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: low,
                repr: low.to_string(),
            },
            TypeTable::U64,
            span,
        );
        let high_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: high.cast_unsigned(),
                repr: high.to_string(),
            },
            if type_name == "u128" {
                TypeTable::U64
            } else {
                TypeTable::I64
            },
            span,
        );

        let method_info =
            LocalMethodName::new(type_name.to_string(), None, "from_pair".to_string());
        let mangled_func_name = method_info.to_mangled_name();

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::int128(),
                    name: mangled_func_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                type_args: vec![],
                args: vec![
                    CallArg::new(low_literal, false),
                    CallArg::new(high_literal, false),
                ],
            },
            target_type,
            span,
        )
    }

    /// Get the return type of a builtin function
    ///
    /// Returns the pre-resolved `TypeId` from the `BuiltinRegistry`.
    /// For generic builtins like `array_new<T>`, returns a type containing
    /// `TypeParam` placeholders that get substituted during monomorphization.
    pub(super) fn get_builtin_return_type(&self, name: &str) -> TypeId {
        self.builtin_registry
            .get_return_type(name)
            .unwrap_or(TypeTable::UNIT)
    }

    /// Look up function parameter types from callee expression
    pub(super) fn lookup_function_param_types(&mut self, callee: &Expr) -> Vec<TypeId> {
        match callee {
            Expr::Ident(ident) => {
                // Check for qualified name (Type::method or Effect::operation)
                if let Some(pos) = ident.name.find("::") {
                    let prefix = &ident.name[..pos];
                    let suffix = &ident.name[pos + 2..];
                    // Check if it's a static method
                    if self.is_static_method(prefix, suffix) {
                        return self.lookup_static_method_param_types(prefix, suffix);
                    }

                    // Builtin functions: look up param types from core:builtin module
                    if prefix == "builtin" {
                        let module_source = ModuleSource::builtin();
                        let params = Self::lookup_func_in_loaded_module(
                            self.loaded_modules,
                            &self.loaded_module_func_indices,
                            &module_source,
                            suffix,
                        )
                        .map(|func| func.params.clone());
                        if let Some(params) = params {
                            return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
                        }
                    }

                    return Vec::new(); // Effect operations handled separately
                }

                // Check if it's a local function (defined in this module)
                if self.function_return_types.contains_key(&ident.name) {
                    // Clone params and type_params to avoid borrow issues
                    let func_info: Option<(Vec<ast::Param>, Vec<ast::GenericParam>)> = self
                        .lookup_current_func(&ident.name)
                        .map(|func| (func.params.clone(), func.type_params.clone()));

                    if let Some((params, type_params)) = func_info {
                        // Set up type params so resolve_type can find pack params
                        // (inherited scope; only `type_params` is replaced, matching
                        // the original `mem::take` semantics).
                        let mut scope = self.enter_inherited_type_param_scope();
                        scope.trait_ctx.type_params.clear();
                        scope.register_generic_params(&type_params, 0);
                        let result = params.iter().map(|p| scope.resolve_type(&p.ty)).collect();
                        drop(scope);
                        return result;
                    }
                }

                // Check imported functions — resolve param types using
                // the definition module's newtypes to avoid same-name collisions
                if let Some(symbol) = self.symbols.lookup(&ident.name) {
                    let src = symbol.module_source().clone();
                    let name = symbol.name.clone();
                    let params = Self::lookup_func_in_loaded_module(
                        self.loaded_modules,
                        &self.loaded_module_func_indices,
                        &src,
                        &name,
                    )
                    .map(|func| func.params.clone());
                    if let Some(params) = params {
                        // Resolve types in the definition module's perspective
                        // so that type names resolve to the correct module's types
                        // (e.g., "Direction" resolves to module B's Direction, not module A's)
                        let callee_module = self
                            .loaded_modules
                            .iter()
                            .find(|(ms, _)| **ms == src)
                            .map(|(_, m)| m);
                        let (imported_type_sources, import_original_names) =
                            if let Some(module) = callee_module {
                                Self::build_imported_type_sources(
                                    &mut self.interner.borrow_mut(),
                                    module,
                                    &src,
                                    Some(&self.entry_module_source),
                                    &self.invocations,
                                )
                            } else {
                                (IndexMap::default(), IndexMap::default())
                            };
                        return self.with_module_perspective(
                            src.clone(),
                            imported_type_sources,
                            import_original_names,
                            |s| params.iter().map(|p| s.resolve_type(&p.ty)).collect(),
                        );
                    }
                }

                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Fill missing trailing arguments with the callee's declared default
    /// expressions, resolving each default in the caller's function context.
    ///
    /// A default may reference earlier parameters by name (e.g. `fn rect(w, h = w)`).
    /// To make the caller's supplied value visible to the default expression,
    /// we textually substitute param-name [`Expr::Ident`] occurrences inside
    /// the default's cloned AST with the corresponding argument's AST before
    /// resolving it. The resolver then type-checks the result against the
    /// declared parameter type.
    ///
    /// Appends newly-synthesized arguments to `args`. Leaves `args` unchanged
    /// for positions without a declared default; the caller's arity check
    /// reports the remaining shortfall as a mismatch.
    pub(super) fn pad_args_with_defaults(
        &mut self,
        callee: &Expr,
        call_args_ast: &[Expr],
        args: &mut Vec<TirExpr>,
        param_types: &[TypeId],
        ctx: &mut FunctionContext,
    ) {
        let (defaults, callee_module) = self.lookup_function_param_defaults(callee, ctx);
        if defaults.is_empty() {
            return;
        }
        let mut subs: IndexMap<String, Expr> = IndexMap::default();
        for (i, arg_ast) in call_args_ast.iter().enumerate() {
            if let Some((name, _)) = defaults.get(i) {
                subs.insert(name.clone(), arg_ast.clone());
            }
        }
        let saved_fallback = self.default_scope_module.take();
        self.default_scope_module = callee_module;
        for i in args.len()..param_types.len() {
            let (name, default_ast) = match defaults.get(i) {
                Some((n, Some(d))) => (n.clone(), d.clone()),
                _ => break,
            };
            let mut default_expr = default_ast;
            default_expr.substitute_idents(&subs);
            let expected_type = param_types[i];
            let resolved = self.resolve_expr(&default_expr, ctx, Some(expected_type));
            if resolved.type_id == TypeTable::UNIT
                && expected_type != TypeTable::UNIT
                && expected_type != TypeTable::ERROR
                && expected_type != TypeTable::UNKNOWN
            {
                let expected_name = self.type_table.borrow().type_name(expected_type);
                panic!(
                    "compiler bug: default expression for parameter '{name}' \
                     re-resolved to () at call site but parameter expects '{expected_name}'. \
                     Likely cause: the default references callee-only scope \
                     (e.g. a callee type parameter like `T::default()`) that is \
                     invisible during call-site re-resolution. \
                     Resolving defaults per-monomorphization is deferred work; \
                     see WEP 2026-04-11 `docs/wep-2026-04-11-default-arguments.md`. \
                     Default span: {:?}",
                    default_expr.span()
                );
            }
            args.push(resolved);
            subs.insert(name, default_expr);
        }
        self.default_scope_module = saved_fallback;
    }

    /// Look up the default-value AST and parameter name for each parameter of a
    /// free function callee, in declaration order, along with the callee's
    /// defining [`ModuleSource`] (for resolving defaults in the callee's
    /// lexical scope). Returns `(Vec::new(), None)` for unknown/builtin
    /// functions. Used to synthesize missing trailing arguments at the call
    /// site.
    pub(super) fn lookup_function_param_defaults(
        &mut self,
        callee: &Expr,
        ctx: &FunctionContext,
    ) -> (Vec<(String, Option<Expr>)>, Option<ModuleSource>) {
        let Expr::Ident(ident) = callee else {
            return (Vec::new(), None);
        };
        if ident.name.contains("::") {
            return (Vec::new(), None);
        }
        if let Some(defaults) = ctx.closure_defaults.get(&ident.name) {
            return (defaults.clone(), None);
        }
        if self.function_return_types.contains_key(&ident.name)
            && let Some(func) = self.lookup_current_func(&ident.name)
        {
            return (
                func.params
                    .iter()
                    .map(|p| (p.name.clone(), p.default.clone()))
                    .collect(),
                Some(self.current_module_source.clone()),
            );
        }
        if let Some(symbol) = self.symbols.lookup(&ident.name) {
            let src = symbol.module_source().clone();
            let name = symbol.name.clone();
            if let Some(func) = Self::lookup_func_in_loaded_module(
                self.loaded_modules,
                &self.loaded_module_func_indices,
                &src,
                &name,
            ) {
                return (
                    func.params
                        .iter()
                        .map(|p| (p.name.clone(), p.default.clone()))
                        .collect(),
                    Some(src),
                );
            }
        }
        (Vec::new(), None)
    }

    /// Look up whether each parameter of a free function is `mut`.
    /// Returns empty vec for unknown/builtin functions (conservative: always copy).
    pub(super) fn lookup_function_param_is_mut(&mut self, callee: &Expr) -> Vec<bool> {
        let Expr::Ident(ident) = callee else {
            return Vec::new();
        };

        // Qualified names (Type::method, static calls) handled elsewhere
        if ident.name.contains("::") {
            return Vec::new();
        }

        // Local function
        if self.function_return_types.contains_key(&ident.name)
            && let Some(func) = self.lookup_current_func(&ident.name)
        {
            return func.params.iter().map(|p| p.is_mut).collect();
        }

        // Imported function
        if let Some(symbol) = self.symbols.lookup(&ident.name) {
            let src = symbol.module_source().clone();
            let name = symbol.name.clone();
            if let Some(func) = Self::lookup_func_in_loaded_module(
                self.loaded_modules,
                &self.loaded_module_func_indices,
                &src,
                &name,
            ) {
                return func.params.iter().map(|p| p.is_mut).collect();
            }
        }

        Vec::new()
    }

    /// Infer type arguments for a generic function call from the actual argument types.
    ///
    /// Uses pre-resolved param types from the current-module cache when available,
    /// otherwise falls back to looking up imported functions in loaded modules and
    /// resolving their param types in a temporary type-param scope.
    ///
    /// Inference runs in three tiers via [`InferCtx`]:
    /// 1. Typed (non-literal) argument types are unified first.
    /// 2. Numeric-literal argument types are unified after, so a literal's
    ///    default type cannot clobber a stronger binding from a typed neighbour.
    /// 3. If `expected_type` is supplied (e.g. `let x: i32 = foo()`), the
    ///    declaration's return type is unified against it to fill any
    ///    parameters that never appeared in the argument list.
    ///
    /// Returns the inferred type args in declaration order, or empty vec if
    /// any parameter remains unbound (legacy strict all-or-nothing behaviour
    /// preserved for plain function calls).
    pub(super) fn infer_fn_type_args(
        &mut self,
        callee: &CalleeRef,
        raw_args: &[Expr],
        args: &[TirExpr],
        expected_type: Option<TypeId>,
    ) -> Vec<TypeId> {
        let func_name = callee.name.as_str();
        // Builtin functions: pull type-param / param / return info from the
        // BuiltinRegistry so that calls like `builtin::select(a, b, c)` and
        // `builtin::array_new(n)` infer their generic type parameters from
        // argument types or LHS annotations the same way ordinary generic
        // functions do.
        if let Some(info) = self.builtin_registry.get(func_name) {
            let real_type_params: Vec<&String> = info.type_params.iter().collect();
            if real_type_params.is_empty() {
                return vec![];
            }
            let param_ids: Vec<TypeId> = real_type_params
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::TypeParam {
                            index: i as u32,
                            name: (*name).clone(),
                        })
                })
                .collect();
            let resolved_param_types: Vec<TypeId> = info.params.iter().map(|(_, t)| *t).collect();
            let decl_return_type = info.return_type;

            let mut infer = InferCtx::new(&self.type_table, param_ids.clone());
            for (i, (param_type, arg)) in resolved_param_types.iter().zip(args.iter()).enumerate() {
                if Self::is_literal_number_arg(raw_args.get(i)) {
                    infer.add_deferred(*param_type, arg.type_id);
                } else {
                    infer.add(*param_type, arg.type_id);
                }
            }
            if let Some(expected) = expected_type {
                infer.add_expected_return(decl_return_type, expected);
            }
            let (inferred, bindings) = infer.solve_with_bindings();
            let any_bound = param_ids.iter().any(|p| bindings.contains_key(p));
            if !any_bound {
                return vec![];
            }
            return inferred;
        }

        // Fast path: current-module cache (populated during item resolution).
        let cached = if let (Some(tp), Some(rp)) = (
            self.generic_function_params.get(func_name).cloned(),
            self.generic_function_resolved_param_types
                .get(func_name)
                .cloned(),
        ) {
            let decl_return = self
                .generic_function_resolved_return_types
                .get(func_name)
                .copied();
            Some((tp, rp, decl_return))
        } else {
            None
        };

        let (type_param_list, resolved_param_types, decl_return_type) = if let Some(v) = cached {
            v
        } else {
            // Fallback: cross-module lookup via imported functions / loaded modules.
            let Some(info) = self.lookup_generic_func_for_inference(callee) else {
                return vec![];
            };
            info
        };

        if type_param_list.is_empty() {
            return vec![];
        }

        let param_ids: Vec<TypeId> = type_param_list.iter().map(|(_, id)| *id).collect();
        let mut infer = InferCtx::new(&self.type_table, param_ids.clone());
        for (i, (param_type, arg)) in resolved_param_types.iter().zip(args.iter()).enumerate() {
            if Self::is_literal_number_arg(raw_args.get(i)) {
                infer.add_deferred(*param_type, arg.type_id);
            } else {
                infer.add(*param_type, arg.type_id);
            }
        }
        if let (Some(decl_ret), Some(expected)) = (decl_return_type, expected_type) {
            infer.add_expected_return(decl_ret, expected);
        }

        // Use the permissive solver: inside a generic scope, a caller may
        // forward its own type parameter (e.g. `outer<T>` calling `inner(g)`
        // where `g: &mut T`) so the returned type args contain the caller's
        // `TypeParam` ids, which then get substituted to concrete types
        // during monomorphization. Fall back to the legacy empty result
        // only when *no* param was ever touched by a unification
        // constraint, since two TypeParams named `T` at index `0` intern
        // to the same `TypeId` and the only reliable signal of "something
        // was inferred" is the bindings map itself.
        let (inferred, bindings) = infer.solve_with_bindings();
        let any_bound = param_ids.iter().any(|p| bindings.contains_key(p));
        if !any_bound {
            return vec![];
        }
        inferred
    }

    /// Look up a generic function (current or imported) and produce a temporary
    /// `(type_param_list, resolved_param_types, decl_return_type)` triple suitable
    /// for type-arg inference. Sets up the function's type params in scope while
    /// resolving param and return types.
    fn lookup_generic_func_for_inference(
        &mut self,
        callee: &CalleeRef,
    ) -> Option<(Vec<(String, TypeId)>, Vec<TypeId>, Option<TypeId>)> {
        let func_info: Option<(Vec<ast::GenericParam>, Vec<ast::Param>, Option<ast::Type>)> =
            Self::lookup_func_in_loaded_module(
                self.loaded_modules,
                &self.loaded_module_func_indices,
                &callee.module,
                &callee.name,
            )
            .map(|func| {
                (
                    func.type_params.clone(),
                    func.params.clone(),
                    func.return_type.clone(),
                )
            });
        let (type_params, params, return_type_ast) = func_info?;

        if type_params.iter().all(|p| p.is_effect) {
            return Some((vec![], vec![], None));
        }

        // Temporarily register the function's type params so resolve_type produces
        // TypeParam ids for parameter types like `T`. Inherited scope; only
        // `type_params` is replaced, matching the original `mem::take` semantics.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(&type_params, 0);
        let type_param_list: Vec<(String, TypeId)> = scope
            .trait_ctx
            .type_params
            .iter()
            .map(|(name, &(_, id))| (name.clone(), id))
            .collect();

        let resolved_param_types: Vec<TypeId> = params
            .iter()
            .filter(|p| p.self_kind == ast::SelfKind::None)
            .map(|p| scope.resolve_type(&p.ty))
            .collect();

        let decl_return_type = return_type_ast.as_ref().map(|t| scope.resolve_type(t));

        drop(scope);
        Some((type_param_list, resolved_param_types, decl_return_type))
    }

    /// Returns true if `arg` is a numeric literal expression (`123`, `3.14`, `-5`)
    /// with no explicit type annotation. These args participate in literal coercion
    /// and should be deferred during type-arg inference's first phase.
    pub(super) fn is_literal_number_arg(arg: Option<&Expr>) -> bool {
        match arg {
            Some(Expr::Literal(lit)) => matches!(lit.value, ast::Literal::Number(_)),
            Some(Expr::Unary(unary)) if unary.op == ast::UnaryOp::Neg => matches!(
                &unary.expr,
                Expr::Literal(inner) if matches!(inner.value, ast::Literal::Number(_))
            ),
            _ => false,
        }
    }

    /// Infer impl-level and method-level type arguments for a generic static method
    /// by looking up the method in loaded modules. Works cross-module unlike
    /// `infer_fn_type_args` (which requires same-module data).
    ///
    /// Shares the three-tier constraint model with [`Self::infer_fn_type_args`]
    /// via [`InferCtx`]: typed args, then literal-number args, then
    /// expected-return back-inference.
    ///
    /// Returns `(impl_args, method_args)` with the inferred type arguments
    /// split into impl-level (from `impl Container<T>` / `impl<T> Container<T>`)
    /// and method-level (from `fn make<U>()`). Either side may be empty if
    /// nothing on that level was inferable.
    fn infer_static_method_type_args(
        &mut self,
        struct_name: &str,
        method_name: &str,
        raw_args: &[Expr],
        args: &[TirExpr],
        expected_type: Option<TypeId>,
    ) -> (Vec<TypeId>, Vec<TypeId>) {
        // Find the impl block + method (regardless of whether the method has
        // its own type params — we also want to infer impl-level params from
        // the LHS for static methods like `Container::make()`).
        let method_info: Option<(
            ast::Type,
            Vec<ast::GenericParam>,
            Vec<ast::Param>,
            Option<ast::Type>,
        )> = {
            let mut found = None;
            'outer: for (_, module) in self.loaded_modules {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item
                        && Self::get_type_name_static(&impl_block.ty) == struct_name
                    {
                        for method in &impl_block.methods {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                found = Some((
                                    impl_block.ty.clone(),
                                    method.type_params.clone(),
                                    method.params.clone(),
                                    method.return_type.clone(),
                                ));
                                break 'outer;
                            }
                        }
                    }
                }
            }
            // Also check current module items
            if found.is_none() {
                'outer2: for item in self.current_module_items {
                    if let Item::Impl(impl_block) = item
                        && Self::get_type_name_static(&impl_block.ty) == struct_name
                    {
                        for method in &impl_block.methods {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                found = Some((
                                    impl_block.ty.clone(),
                                    method.type_params.clone(),
                                    method.params.clone(),
                                    method.return_type.clone(),
                                ));
                                break 'outer2;
                            }
                        }
                    }
                }
            }
            found
        };
        let Some((impl_ty, method_type_params, params, return_type_ast)) = method_info else {
            return (vec![], vec![]);
        };

        // Extract impl-level type param names (e.g. `impl Container<T>` -> ["T"]).
        let impl_type_param_names: Vec<String> = match &impl_ty {
            ast::Type::Generic(g) => g
                .args
                .iter()
                .filter_map(|arg| match arg {
                    ast::Type::Named(n) => Some(n.name.clone()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };

        // Nothing generic to infer.
        if impl_type_param_names.is_empty() && method_type_params.is_empty() {
            return (vec![], vec![]);
        }

        // Set up scope: impl params at indices 0..impl_count, method params
        // at indices impl_count..(impl_count + method_count). This matches the
        // layout used by `lookup_static_method_return_type` and
        // `resolve_static_method_params_in_scope`, so substitution by a single
        // flat `[impl_args.., method_args..]` list lines up correctly.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();

        let mut impl_param_ids: Vec<TypeId> = Vec::with_capacity(impl_type_param_names.len());
        for (i, name) in impl_type_param_names.iter().enumerate() {
            let type_id = scope
                .type_table
                .borrow_mut()
                .make_type_param(name.clone(), i as u32);
            scope
                .trait_ctx
                .type_params
                .insert(name.clone(), (i as u32, type_id));
            impl_param_ids.push(type_id);
        }

        let m_offset = impl_type_param_names.len();
        let mut method_param_ids: Vec<TypeId> = Vec::with_capacity(method_type_params.len());
        for (i, tp) in method_type_params
            .iter()
            .filter(|p| !p.is_effect)
            .enumerate()
        {
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
            method_param_ids.push(type_id);
        }

        let resolved_param_types: Vec<TypeId> = params
            .iter()
            .filter(|p| p.self_kind == ast::SelfKind::None)
            .map(|p| scope.resolve_type(&p.ty))
            .collect();

        let decl_return_type = return_type_ast.as_ref().map(|t| scope.resolve_type(t));

        drop(scope);

        let mut all_param_ids = impl_param_ids.clone();
        all_param_ids.extend_from_slice(&method_param_ids);

        let mut infer = InferCtx::new(&self.type_table, all_param_ids.clone());
        for (i, (param_type, arg)) in resolved_param_types.iter().zip(args.iter()).enumerate() {
            if Self::is_literal_number_arg(raw_args.get(i)) {
                infer.add_deferred(*param_type, arg.type_id);
            } else {
                infer.add(*param_type, arg.type_id);
            }
        }
        if let (Some(decl_ret), Some(expected)) = (decl_return_type, expected_type) {
            infer.add_expected_return(decl_ret, expected);
        }

        // Permissive solve — see `infer_fn_type_args` for the
        // TypeParam-forwarding rationale.
        let (inferred, bindings) = infer.solve_with_bindings();
        let any_bound = all_param_ids.iter().any(|p| bindings.contains_key(p));
        if !any_bound {
            return (vec![], vec![]);
        }

        let impl_count = impl_param_ids.len();
        let impl_args = inferred[..impl_count].to_vec();
        let method_args = inferred[impl_count..].to_vec();
        (impl_args, method_args)
    }

    /// Look up function parameter types with type args substituted.
    /// For generic functions like `fn identity<T>(x: T)` called as `identity::<i32>(...)`,
    /// this temporarily registers `T` as a `TypeParam`, resolves param types, then
    /// substitutes `T` → `i32` to get `[i32]`.
    fn lookup_function_param_types_with_type_args(
        &mut self,
        callee: &Expr,
        type_args: &[TypeId],
    ) -> Vec<TypeId> {
        let Expr::Ident(ident) = callee else {
            return Vec::new();
        };

        let func_info: Option<(Vec<ast::GenericParam>, Vec<ast::Param>)> =
            if self.function_return_types.contains_key(&ident.name) {
                self.lookup_current_func(&ident.name)
                    .map(|func| (func.type_params.clone(), func.params.clone()))
            } else if let Some(symbol) = self.symbols.lookup(&ident.name) {
                let src = symbol.module_source().clone();
                let name = symbol.name.clone();
                Self::lookup_func_in_loaded_module(
                    self.loaded_modules,
                    &self.loaded_module_func_indices,
                    &src,
                    &name,
                )
                .map(|func| (func.type_params.clone(), func.params.clone()))
            } else {
                None
            };

        let Some((fn_type_params, fn_params)) = func_info else {
            return Vec::new();
        };

        // Temporarily register type params so resolve_type can find them
        // (inherited scope; only `type_params` is replaced, matching the
        // original `mem::take` semantics).
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(&fn_type_params, 0);
        let param_types: Vec<TypeId> = fn_params
            .iter()
            .map(|p| scope.resolve_type(&p.ty))
            .collect();
        drop(scope);

        // Substitute type params with explicit type args
        param_types
            .iter()
            .map(|&pt| self.substitute_type_params(pt, type_args))
            .collect()
    }

    /// Infer type arguments for a variant constructor `Variant::Case(payload)`.
    ///
    /// Uses [`InferCtx`] with:
    /// * a strong constraint from the payload expression (forward inference), and
    /// * expected-return constraints from the declaration-site's type parameters,
    ///   unified against the caller's expected generic-instance type args.
    ///
    /// Falls back to a bare `Variant` type if any type parameter remains unbound
    /// and we are in a non-generic context — preserving the legacy behaviour.
    pub(super) fn infer_variant_type_args(
        &mut self,
        variant_name: &str,
        variant_info: &super::types::VariantInfo,
        case_data: &super::types::VariantCaseData,
        payload: Option<&TirExpr>,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Track the canonical module_source from expected_type if available.
        // This ensures the created GenericInstance uses the same module_source
        // as the type annotation (e.g., ModuleSource::prelude() for Option/Result),
        // which may differ from variant_info.module_source (e.g., prelude/types.wado).
        let mut canonical_module_source = None;

        let mut infer = InferCtx::new(&self.type_table, variant_info.type_param_type_ids.clone());

        // Forward inference: unify the case payload's declared type against
        // the actual payload expression's type.
        if let Some(payload_expr) = payload {
            infer.add(case_data.payload, payload_expr.type_id);
        }

        // Backward inference: extract type args from the caller's expected type.
        // Queued through `add_expected_return` so it runs after the payload pass,
        // preserving the "stronger constraint wins" policy via `or_insert`.
        if let Some(expected) = expected_type {
            let expected_resolved = self.type_table.borrow().get(expected).clone();
            if let ResolvedType::GenericInstance {
                name,
                module_source,
                type_args: expected_args,
            } = expected_resolved
                && name == variant_name
                && expected_args.len() == variant_info.type_param_type_ids.len()
            {
                canonical_module_source = Some(module_source);
                for (&param_id, &expected_arg) in variant_info
                    .type_param_type_ids
                    .iter()
                    .zip(expected_args.iter())
                {
                    infer.add_expected_return(param_id, expected_arg);
                }
            }
        }

        let (type_args, _) = infer.solve_with_phantoms();

        let module_source =
            canonical_module_source.unwrap_or_else(|| variant_info.module_source.clone());

        // If unresolved type params remain in concrete code, fall back to bare Variant
        let has_unresolved = type_args
            .iter()
            .any(|&t| self.type_table.borrow().contains_type_param(t));

        if has_unresolved && self.trait_ctx.type_params.is_empty() {
            return self.type_table.borrow_mut().make_variant(
                variant_info.name.clone(),
                variant_info.module_source.clone(),
            );
        }

        self.type_table.borrow_mut().make_generic_instance(
            variant_info.name.clone(),
            module_source,
            type_args,
        )
    }

    /// Resolve a static call through a type parameter: `T::method(args)`
    /// where T is bound by a trait (e.g., `T: Constructable`).
    fn resolve_type_param_static_call(
        &mut self,
        type_param_name: &str,
        method_name: &str,
        type_param_type_id: TypeId,
        args: &[TirExpr],
        call: &ast::CallExpr,
        _ctx: &mut FunctionContext,
    ) -> TirExpr {
        let bounds = self
            .trait_ctx
            .type_param_bounds
            .get(type_param_name)
            .cloned();

        if let Some(bounds) = bounds
            && let Some((found_trait, method_info_result)) = {
                let bound_names: Vec<String> = bounds.iter().map(|b| b.name.clone()).collect();
                self.find_method_in_trait_bounds(&bound_names, method_name, type_param_type_id)
            }
        {
            let return_type = method_info_result.return_type;

            // Resolve method-level type args (e.g., T::deserialize::<JsonDeserializer>)
            let mut method_type_args: Vec<TypeId> = call
                .type_args
                .iter()
                .map(|ty| self.resolve_type(ty))
                .collect();

            // If no explicit type args, infer method-level type params from argument
            // types via the shared `InferCtx` solver. `find_method_in_trait_bounds`
            // already registered the method's own type params inside its resolution
            // scope and handed them back via `method_type_param_ids`; we feed those
            // to the solver so it knows exactly which TypeIds to bind. Outer-scope
            // type params (`Self`, forwarded caller params) will not appear in
            // `method_type_param_ids` and therefore cannot contaminate the result
            // even when interning assigns them the same index as a method-level
            // one.
            if method_type_args.is_empty()
                && !method_info_result.param_types.is_empty()
                && !method_info_result.method_type_param_ids.is_empty()
            {
                let method_param_ids = method_info_result.method_type_param_ids.clone();
                let mut infer = InferCtx::new(&self.type_table, method_param_ids.clone());
                for (i, (param_type, arg)) in method_info_result
                    .param_types
                    .iter()
                    .zip(args.iter())
                    .enumerate()
                {
                    if Self::is_literal_number_arg(call.args.get(i)) {
                        infer.add_deferred(*param_type, arg.type_id);
                    } else {
                        infer.add(*param_type, arg.type_id);
                    }
                }
                let (inferred, bindings) = infer.solve_with_bindings();
                // Only adopt the inferred vector if the solver actually bound at
                // least one method-level parameter; otherwise leave
                // `method_type_args` empty so downstream code treats the call as
                // having no inferred method args (matching the legacy behaviour).
                if method_param_ids.iter().any(|p| bindings.contains_key(p)) {
                    method_type_args = inferred;
                }
            }

            // Don't substitute method-level type params in the return type here.
            // The return type contains function-level TypeParams (e.g., Self -> TypeParam{T})
            // which will be substituted during monomorphization. Method-level type params
            // (e.g., D in deserialize<D>) are handled by the monomorphizer.
            let final_return_type = return_type;

            // Build method_info with is_type_param_receiver = true
            let method_type_arg_names: Vec<String> = method_type_args
                .iter()
                .map(|t| self.type_table.borrow().mangle_type_name(*t))
                .collect();
            let mut method_info = LocalMethodName::new(
                type_param_name.to_string(),
                Some(found_trait),
                method_name.to_string(),
            );
            method_info.is_type_param_receiver = true;
            if !method_type_arg_names.is_empty() {
                method_info.method_type_args = method_type_arg_names;
            }

            let mangled_name = method_info.to_mangled_name();

            // Build monomorph_info for method-level type args so the
            // monomorphizer can substitute TypeParam type args and
            // queue instantiations.
            let monomorph_info = if method_type_args.is_empty() {
                None
            } else {
                Some(MonomorphInfo {
                    generic_name: mangled_name.clone(),
                    impl_type_args: vec![],
                    method_type_args,
                    is_blanket: false,
                })
            };

            return TirExpr::new(
                TirExprKind::Call {
                    func: FunctionRef {
                        module_source: self.current_module_source.clone(),
                        name: mangled_name,
                        monomorph_info,
                        method_info: Some(method_info),
                    },
                    type_args: vec![],
                    args: args
                        .iter()
                        .map(|e| CallArg::new(e.clone(), false))
                        .collect(),
                },
                final_return_type,
                call.span,
            );
        }

        let _ = self.logger.error(TypeError::UnknownFunction {
            name: format!("{type_param_name}::{method_name}"),
            span: call.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span)
    }
}
