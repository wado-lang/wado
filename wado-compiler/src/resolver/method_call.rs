//! Method call and static method call resolution.

use crate::ast::{self, Item};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    FunctionRef, MonomorphInfo, ResolvedType, SubstitutionContext, TirExpr, TirExprKind, TypeId,
    TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::types::{FunctionContext, MethodInfo, TypeError};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
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
            // Primitive types have impl blocks in core:prelude/primitives
            ResolvedType::Primitive(_) => (
                self.type_table.borrow().mangle_type_name(base_type_id),
                ModuleSource::primitives(),
            ),
            // Enum types - use enum name and its defining module
            ResolvedType::Enum {
                name,
                module_source,
            } => (name.clone(), module_source.clone()),
            // Stream<T> - resource methods declared in core:prelude/types.wado
            ResolvedType::Stream(_) => ("Stream".to_string(), ModuleSource::types()),
            // StreamWritable<T> - resource methods declared in core:prelude/types.wado
            ResolvedType::StreamWritable(_) => {
                ("StreamWritable".to_string(), ModuleSource::types())
            }
            // FutureWritable<T> - resource methods declared in core:prelude/types.wado
            ResolvedType::FutureWritable(_) => {
                ("FutureWritable".to_string(), ModuleSource::types())
            }
            _ => (
                self.type_table.borrow().mangle_type_name(base_type_id),
                self.current_module_source.clone(),
            ),
        };

        // Look up method info based on receiver type
        // First try inherent method, then trait methods
        let mut method_info = self.lookup_method_info(receiver.type_id, &method_call.method);
        let mut trait_name: Option<String> = None;

        // Extract receiver type args for generic types (used for resolving associated types)
        let receiver_type_args_for_trait: Option<Vec<TypeId>> =
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance { type_args, .. } if !type_args.is_empty() => {
                    Some(type_args)
                }
                _ => None,
            };

        // If inherent method not found, try trait methods
        // Track the module source where the trait impl was found
        let mut trait_impl_module_source: Option<ModuleSource> = None;
        let mut blanket_type_param: Option<String> = None;
        if method_info.is_none()
            && let Some(trait_match) = self.find_trait_method_for_type(
                &struct_name,
                &method_call.method,
                &struct_module,
                receiver_type_args_for_trait.as_deref(),
                Some(base_type_id),
            )
        {
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
                if let ResolvedType::TypeParam { name, .. } = resolved {
                    Some(name)
                } else {
                    None
                }
            };
            if let Some(name) = type_param_name
                && let Some(bounds) = self.current_type_param_bounds.get(&name).cloned()
                && let Some((found_trait, info)) =
                    self.find_method_in_trait_bounds(&bounds, &method_call.method, base_type_id)
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
            inherited_from_base,
        } = if let Some(info) = method_info {
            info
        } else {
            let type_name = self.type_table.borrow().type_name(base_type_id);
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: format!(
                    "type '{}' to have method '{}'",
                    type_name, method_call.method
                ),
                found: format!("no method '{}' found", method_call.method),
                span: method_call.span,
            });
            // Default to Unknown type for error recovery
            MethodInfo {
                return_type: TypeTable::UNKNOWN,
                self_kind: ast::SelfKind::Ref,
                param_types: vec![],
                inherited_from_base: None,
            }
        };

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
        let args: Vec<TirExpr> = method_call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = expected_param_types.get(i).copied();
                self.resolve_expr(arg, ctx, expected_type)
            })
            .collect();

        // Check each argument against expected parameter type
        for (i, (arg, &expected_type)) in args.iter().zip(expected_param_types.iter()).enumerate() {
            if let Some((expected_name, actual_name)) =
                self.check_newtype_arg_mismatch(arg.type_id, expected_type)
            {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: format!("argument {} to be {}", i + 1, expected_name),
                    found: actual_name,
                    span: method_call
                        .args
                        .get(i)
                        .map_or(method_call.span, super::ast::Expr::span),
                });
            }
        }

        // Substitute return type for inherited newtype methods
        // e.g., Point::clone_point() -> Point becomes Location::clone_point() -> Location
        if let Some(base_type_id) = inherited_from_base {
            let newtype_id = self.get_base_type(receiver.type_id);
            return_type = self.substitute_newtype_in_type(return_type, base_type_id, newtype_id);
        }

        // Adjust receiver based on what the method expects (self_kind)
        receiver = self.adjust_receiver_for_self_kind(receiver, self_kind, method_call.span);

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
                } if !receiver_type_args.is_empty() => {
                    impl_offset = receiver_type_args.len() as u32;
                    subst_ctx = subst_ctx.with_impl_args(&receiver_type_args);
                }
                // Stream<T> has one type param T
                ResolvedType::Stream(inner) => {
                    impl_offset = 1;
                    subst_ctx = subst_ctx.with_impl_args(&[inner]);
                }
                // StreamWritable<T> has one type param T
                ResolvedType::StreamWritable(inner) => {
                    impl_offset = 1;
                    subst_ctx = subst_ctx.with_impl_args(&[inner]);
                }
                // FutureWritable<T> has one type param T
                ResolvedType::FutureWritable(inner) => {
                    impl_offset = 1;
                    subst_ctx = subst_ctx.with_impl_args(&[inner]);
                }
                _ => {}
            }
        } else {
            // For trait methods, just compute impl_offset for method type args
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance { type_args, .. } if !type_args.is_empty() => {
                    impl_offset = type_args.len() as u32;
                }
                ResolvedType::Stream(_)
                | ResolvedType::StreamWritable(_)
                | ResolvedType::FutureWritable(_) => {
                    impl_offset = 1;
                }
                _ => {}
            }
        }

        // Then add method-level type args with the correct offset
        // If no explicit type args, try to infer from arguments
        let method_type_args = if type_args.is_empty() {
            // Try to infer method type args from actual arguments
            self.infer_method_type_args(receiver.type_id, &method_call.method, &args, impl_offset)
        } else {
            type_args.clone()
        };

        if !method_type_args.is_empty() {
            subst_ctx = subst_ctx.with_method_args(&method_type_args, impl_offset);
        }

        // Apply unified substitution
        if !subst_ctx.is_empty() {
            return_type = subst_ctx.substitute(return_type, &mut self.type_table.borrow_mut());
        }

        // Get struct name and monomorph info from base type for mangled method name
        let (receiver_struct_name, base_struct_name, impl_type_arg_names, receiver_type_args) =
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } => {
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| self.type_table.borrow().mangle_type_name(*t))
                        .collect();
                    let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                    (
                        mangled,
                        name.clone(),
                        type_arg_names,
                        Some(type_args.clone()),
                    )
                }
                // Stream<T>: base name is "Stream", one type arg
                ResolvedType::Stream(inner) => {
                    let inner_name = self.type_table.borrow().mangle_type_name(inner);
                    let mangled = format!("Stream<{inner_name}>");
                    (
                        mangled,
                        "Stream".to_string(),
                        vec![inner_name],
                        Some(vec![inner]),
                    )
                }
                // StreamWritable<T>: base name is "StreamWritable", one type arg
                ResolvedType::StreamWritable(inner) => {
                    let inner_name = self.type_table.borrow().mangle_type_name(inner);
                    let mangled = format!("StreamWritable<{inner_name}>");
                    (
                        mangled,
                        "StreamWritable".to_string(),
                        vec![inner_name],
                        Some(vec![inner]),
                    )
                }
                // FutureWritable<T>: base name is "FutureWritable", one type arg
                ResolvedType::FutureWritable(inner) => {
                    let inner_name = self.type_table.borrow().mangle_type_name(inner);
                    let mangled = format!("FutureWritable<{inner_name}>");
                    (
                        mangled,
                        "FutureWritable".to_string(),
                        vec![inner_name],
                        Some(vec![inner]),
                    )
                }
                _ => {
                    let name = self.type_table.borrow().mangle_type_name(base_type_id);
                    (name.clone(), name, vec![], None)
                }
            };

        let mangled_method_name = MethodName::format_local(
            &receiver_struct_name,
            trait_name.as_deref(),
            &method_call.method,
        );

        // Build monomorph_info for method calls on generic types
        let monomorph_info = if let Some(ref blanket_param) = blanket_type_param {
            // For blanket impls, the template function uses the type param name (e.g., "I").
            // The call site uses the concrete receiver (e.g., "ArrayIter<i32>").
            // monomorph_info maps from the concrete name back to the template.
            let generic_name =
                MethodName::format_local(blanket_param, trait_name.as_deref(), &method_call.method);
            Some(MonomorphInfo {
                generic_name,
                type_args: vec![base_type_id],
                is_blanket: true,
            })
        } else {
            receiver_type_args.map(|type_args| {
                let generic_name =
                    MethodName::format_local(&base_struct_name, None, &method_call.method);
                MonomorphInfo {
                    generic_name,
                    type_args,
                    is_blanket: false,
                }
            })
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
            ResolvedType::TypeParam { .. } | ResolvedType::AssocTypeProjection { .. }
        );
        let mut method_info = LocalMethodName::new(
            base_struct_name, // Use base struct name without type params
            trait_name,
            method_call.method.clone(),
        )
        .with_type_args(&impl_type_arg_names, &method_type_arg_names);
        method_info.is_type_param_receiver = is_type_param_receiver;

        // Use trait impl module source if this is a trait method,
        // otherwise use the struct's module (where inherent methods are defined)
        let method_module_source = trait_impl_module_source
            .or(Some(struct_module.clone()))
            .unwrap_or_else(|| self.current_module_source.clone());

        TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef::External {
                    module_source: method_module_source,
                    name: mangled_method_name,
                    monomorph_info,
                    method_info: Some(method_info),
                },
                type_args: method_type_args, // Use inferred type args
                args,
            },
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
                && let Some(variant_info) = self.variant_cases.get(&name).cloned()
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
                ResolvedType::Newtype { ref name, .. } => Some(name.clone()),
                _ => None,
            };
            if let Some(ref name) = flags_name
                && let Some(flags_info) = self.flags_cases.get(name).cloned()
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

        // Handle Future::<T>::new() and Stream::<T>::new()
        // Creates a handle pair:
        //   Future<T>::new() -> [Future<T>, FutureWritable<T>]
        //   Stream<T>::new() -> [Stream<T>, StreamWritable<T>]
        {
            let target_resolved = self.type_table.borrow().get(target_type_id).clone();
            let pair_info = match &target_resolved {
                ResolvedType::Future(inner) if static_call.method == "new" && args.is_empty() => {
                    Some(("future_create_pair", target_type_id, *inner, true))
                }
                ResolvedType::Stream(inner) if static_call.method == "new" && args.is_empty() => {
                    Some(("stream_create_pair", target_type_id, *inner, false))
                }
                _ => None,
            };

            if let Some((builtin_name, handle_type, inner, is_future)) = pair_info {
                let tx_type = if is_future {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::FutureWritable(inner))
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::StreamWritable(inner))
                };
                let tuple_type = self
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::Tuple(vec![handle_type, tx_type]));

                return TirExpr::new(
                    TirExprKind::Call {
                        func: FunctionRef::External {
                            module_source: ModuleSource::builtin(),
                            name: builtin_name.to_string(),
                            monomorph_info: None,
                            method_info: None,
                        },
                        type_args: vec![],
                        args: vec![],
                    },
                    tuple_type,
                    static_call.span,
                );
            }
        }

        // Handle custom variant construction: Shape::Circle(5.0) or MyVariant::Unit
        if let ResolvedType::Variant {
            name,
            module_source: _,
        } = self.type_table.borrow().get(target_type_id).clone()
        {
            // Look up the variant case info
            if let Some(variant_info) = self.variant_cases.get(&name) {
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
                } else {
                    // Unknown case name
                    let _ = self.logger.error(TypeError::UnknownFunction {
                        name: format!("{}::{}", name, static_call.method),
                        span: static_call.span,
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                }
            } else {
                // Variant not found in variant_cases (shouldn't happen)
                let _ = self.logger.error(TypeError::UnknownType {
                    name: name.clone(),
                    span: static_call.span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
            }
        }

        // Handle generic variant construction: Result::<i32, String>::Ok(42)
        if let ResolvedType::GenericInstance {
            name,
            module_source: _,
            type_args: _,
        } = self.type_table.borrow().get(target_type_id).clone()
        {
            // Check if the base type is a variant
            if let Some(variant_info) = self.variant_cases.get(&name).cloned() {
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
                } else {
                    // Unknown case name
                    let _ = self.logger.error(TypeError::UnknownFunction {
                        name: format!("{}::{}", name, static_call.method),
                        span: static_call.span,
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                }
            }
        }

        let (struct_name, struct_module, mangled_struct_name, struct_type_args) = match self
            .type_table
            .borrow()
            .get(target_type_id)
        {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone(), name.clone(), vec![]),
            ResolvedType::Resource {
                name,
                module_source,
            } => (name.clone(), module_source.clone(), name.clone(), vec![]),
            ResolvedType::Primitive(prim) => {
                let name = prim.as_str().to_string();
                (name.clone(), ModuleSource::primitives(), name, vec![])
            }
            ResolvedType::Enum {
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
            ResolvedType::Newtype { base_type, .. } => {
                // For newtypes, look through to the base type for static method lookup
                match self.type_table.borrow().get(*base_type).clone() {
                    ResolvedType::Struct {
                        name,
                        module_source,
                        ..
                    } => (name.clone(), module_source.clone(), name.clone(), vec![]),
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
                        (
                            name.clone(),
                            module_source.clone(),
                            mangled,
                            type_args.clone(),
                        )
                    }
                    // Handle chained newtypes recursively
                    ResolvedType::Newtype {
                        base_type: inner_base,
                        ..
                    } => {
                        // Follow the chain to find the ultimate struct
                        let mut current = inner_base;
                        loop {
                            match self.type_table.borrow().get(current).clone() {
                                ResolvedType::Struct {
                                    name,
                                    module_source,
                                    ..
                                } => {
                                    break (
                                        name.clone(),
                                        module_source.clone(),
                                        name.clone(),
                                        vec![],
                                    );
                                }
                                ResolvedType::Newtype {
                                    base_type: next, ..
                                } => current = next,
                                _ => {
                                    return TirExpr::new(
                                        TirExprKind::Unit,
                                        TypeTable::ERROR,
                                        static_call.span,
                                    );
                                }
                            }
                        }
                    }
                    _ => {
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }
                }
            }
            _ => {
                // Unknown type - return error expression
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
            }
        };

        // Find trait name: if the static method belongs to a trait impl, include the
        // trait name in the mangled function name so WIR can resolve it correctly.
        let trait_name_opt = self.find_static_method_trait(&struct_name, &static_call.method);

        let mangled_func_name = MethodName::format_local(
            &mangled_struct_name,
            trait_name_opt.as_deref(),
            &static_call.method,
        );

        // Look up return type
        let mut return_type = self.lookup_static_method_return_type(
            &struct_name,
            &struct_module,
            &static_call.method,
            &mangled_func_name,
        );

        // If we have type arguments from a generic type, substitute type parameters in the return type
        if !struct_type_args.is_empty() {
            return_type = self.substitute_type_params(return_type, &struct_type_args);
        }

        // Build monomorph_info for generic instantiations
        let (monomorph_info, impl_type_arg_names): (Option<MonomorphInfo>, Vec<String>) =
            if struct_type_args.is_empty() {
                (None, vec![])
            } else {
                // Generic static method: track the original generic name (with trait if applicable)
                let generic_name = MethodName::format_local(
                    &struct_name,
                    trait_name_opt.as_deref(),
                    &static_call.method,
                );
                let type_arg_names: Vec<String> = struct_type_args
                    .iter()
                    .map(|t| self.type_table.borrow().mangle_type_name(*t))
                    .collect();
                (
                    Some(MonomorphInfo {
                        generic_name,
                        type_args: struct_type_args,
                        is_blanket: false,
                    }),
                    type_arg_names,
                )
            };

        // Build method_info with base struct name and trait name (if applicable)
        let method_info =
            LocalMethodName::new(struct_name, trait_name_opt, static_call.method.clone())
                .with_struct_type_args(&impl_type_arg_names);

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: struct_module,
                    name: mangled_func_name,
                    monomorph_info,
                    method_info: Some(method_info),
                },
                args,
            },
            return_type,
            static_call.span,
        )
    }

    /// Look up static method return type based on struct name and method name
    pub(super) fn lookup_static_method_return_type(
        &mut self,
        struct_name: &str,
        struct_module: &ModuleSource,
        method_name: &str,
        mangled_func_name: &str,
    ) -> TypeId {
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
                                // Set up type parameters from impl block before resolving
                                let old_type_params = std::mem::take(&mut self.current_type_params);

                                // Extract type params from impl block type (e.g., impl Array<T>)
                                if let ast::Type::Generic(generic) = &impl_block.ty {
                                    for (i, arg) in generic.args.iter().enumerate() {
                                        if let ast::Type::Named(named) = arg {
                                            let name = &named.name;
                                            if !self.current_type_params.contains_key(name) {
                                                let type_id = self
                                                    .type_table
                                                    .borrow_mut()
                                                    .make_type_param(name.clone(), i as u32);
                                                self.current_type_params
                                                    .insert(name.clone(), (i as u32, type_id));
                                            }
                                        }
                                    }
                                }

                                let result = method
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_type(t))
                                    .unwrap_or(TypeTable::UNIT);

                                // Restore type parameters
                                self.current_type_params = old_type_params;

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
                            // Set up type parameters from resource declaration before resolving
                            let old_type_params = std::mem::take(&mut self.current_type_params);

                            for (i, param) in resource.type_params.iter().enumerate() {
                                let name = &param.name;
                                if !self.current_type_params.contains_key(name) {
                                    let type_id = self
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), i as u32);
                                    self.current_type_params
                                        .insert(name.clone(), (i as u32, type_id));
                                }
                            }

                            let result = method
                                .return_type
                                .as_ref()
                                .map(|t| self.resolve_type(t))
                                .unwrap_or(TypeTable::UNIT);

                            // Restore type parameters
                            self.current_type_params = old_type_params;

                            return result;
                        }
                    }
                }
            }
        }

        // Search all loaded modules if struct_module is entry point
        if struct_module.is_entry_point() {
            for module in self.loaded_modules.values() {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item {
                        let impl_struct_name = self.get_type_name(&impl_block.ty);
                        if impl_struct_name == struct_name {
                            for method in &impl_block.methods {
                                let has_self = method
                                    .params
                                    .iter()
                                    .any(|p| p.self_kind != ast::SelfKind::None);
                                if method.name == method_name && !has_self {
                                    // Set up type parameters from impl block before resolving
                                    let old_type_params =
                                        std::mem::take(&mut self.current_type_params);

                                    // Extract type params from impl block type (e.g., impl Array<T>)
                                    if let ast::Type::Generic(generic) = &impl_block.ty {
                                        for (i, arg) in generic.args.iter().enumerate() {
                                            if let ast::Type::Named(named) = arg {
                                                let name = &named.name;
                                                if !self.current_type_params.contains_key(name) {
                                                    let type_id = self
                                                        .type_table
                                                        .borrow_mut()
                                                        .make_type_param(name.clone(), i as u32);
                                                    self.current_type_params
                                                        .insert(name.clone(), (i as u32, type_id));
                                                }
                                            }
                                        }
                                    }

                                    let result = method
                                        .return_type
                                        .as_ref()
                                        .map(|t| self.resolve_type(t))
                                        .unwrap_or(TypeTable::UNIT);

                                    // Restore type parameters
                                    self.current_type_params = old_type_params;

                                    return result;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Search resource declarations in all modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                {
                    // Find the method in the resource
                    for method in &resource.methods {
                        // Static methods have no self parameter
                        let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                            });
                        if method.name == method_name && !has_self {
                            // Set up type parameters from resource declaration before resolving
                            let old_type_params = std::mem::take(&mut self.current_type_params);

                            for (i, param) in resource.type_params.iter().enumerate() {
                                let name = &param.name;
                                if !self.current_type_params.contains_key(name) {
                                    let type_id = self
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), i as u32);
                                    self.current_type_params
                                        .insert(name.clone(), (i as u32, type_id));
                                }
                            }

                            let result = method
                                .return_type
                                .as_ref()
                                .map(|t| self.resolve_type(t))
                                .unwrap_or(TypeTable::UNIT);

                            // Restore type parameters
                            self.current_type_params = old_type_params;

                            return result;
                        }
                    }
                }
            }
        }

        TypeTable::UNKNOWN
    }

    /// Look up static method parameter types for coercion
    pub(super) fn lookup_static_method_param_types(
        &mut self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<TypeId> {
        // Check in current module's impl blocks
        let params: Option<Vec<_>> = self.current_module_items.iter().find_map(|item| {
            if let Item::Impl(impl_block) = item {
                let impl_struct_name = self.get_type_name(&impl_block.ty);
                if impl_struct_name == struct_name {
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if method.name == method_name && !has_self {
                            return Some(method.params.clone());
                        }
                    }
                }
            }
            None
        });

        if let Some(params) = params {
            return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
        }

        // Check loaded modules' impl blocks
        for module in self.loaded_modules.values() {
            let params: Option<Vec<_>> = module.items.iter().find_map(|item| {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                return Some(method.params.clone());
                            }
                        }
                    }
                }
                None
            });

            if let Some(params) = params {
                return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
            }
        }

        Vec::new()
    }

    /// Find the trait name for a static method on a struct, if the method belongs to a trait impl.
    /// Returns `None` for inherent static methods, `Some(trait_name)` for trait static methods.
    pub(super) fn find_static_method_trait(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<String> {
        // Check current module items first
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                for method in &impl_block.methods {
                    let has_self = method
                        .params
                        .iter()
                        .any(|p| p.self_kind != ast::SelfKind::None);
                    if method.name == method_name && !has_self {
                        return Some(Self::get_type_name_static(trait_type));
                    }
                }
            }
        }

        // Check loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                    && Self::get_type_name_static(&impl_block.ty) == struct_name
                {
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if method.name == method_name && !has_self {
                            return Some(Self::get_type_name_static(trait_type));
                        }
                    }
                }
            }
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

        // Also check in loaded modules' impl blocks
        for module in self.loaded_modules.values() {
            for item in &module.items {
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
                                return true;
                            }
                        }
                    }
                }
            }
        }

        // Check current module's impl blocks
        for item in &self.current_module_items {
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

        // Check resource declarations in loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                {
                    for method in &resource.methods {
                        // Static methods have no self parameter
                        let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                            });
                        if method.name == method_name && !has_self {
                            return true;
                        }
                    }
                }
            }
        }

        // For newtypes, check if the base type has the static method
        if let Some(&newtype_id) = self.newtypes.get(struct_name)
            && let ResolvedType::Newtype { base_type, .. } =
                self.type_table.borrow().get(newtype_id).clone()
        {
            // Get the base type's name and recursively check
            let base_name = self.type_table.borrow().type_name(base_type);
            if self.is_static_method(&base_name, method_name) {
                return true;
            }
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
        method_type_args: &[TypeId],
        span: Span,
        _ctx: &mut FunctionContext,
    ) -> TirExpr {
        // For newtypes, resolve to the base type's static method
        let (actual_struct_name, actual_mangled_name) =
            if let Some(&newtype_id) = self.newtypes.get(struct_name) {
                if let ResolvedType::Newtype { base_type, .. } =
                    self.type_table.borrow().get(newtype_id).clone()
                {
                    // Follow the chain to find the ultimate struct
                    let base_name = self.get_ultimate_base_struct_name(base_type);
                    let mangled = MethodName::format_local(&base_name, None, method_name);
                    (base_name, mangled)
                } else {
                    (struct_name.to_string(), mangled_func_name.to_string())
                }
            } else {
                (struct_name.to_string(), mangled_func_name.to_string())
            };

        // Determine module source for the actual struct
        let struct_module = self.find_struct_module_source(&actual_struct_name);

        // Find trait name for trait static methods
        let trait_name_opt = self.find_static_method_trait(&actual_struct_name, method_name);

        // Use trait-qualified mangled name if this is a trait method
        let final_mangled_name = if let Some(ref trait_name) = trait_name_opt {
            MethodName::format_local(&actual_struct_name, Some(trait_name), method_name)
        } else {
            actual_mangled_name
        };

        // Look up return type using the actual struct name
        let mut return_type = self.lookup_static_method_return_type(
            &actual_struct_name,
            &struct_module,
            method_name,
            &final_mangled_name,
        );

        // Substitute method-level type parameters in return type
        if !method_type_args.is_empty() {
            return_type = self.substitute_type_params(return_type, method_type_args);
        }

        // Build monomorph_info for method-level generic instantiation
        let monomorph_info = if method_type_args.is_empty() {
            None
        } else {
            Some(MonomorphInfo {
                generic_name: final_mangled_name.clone(),
                type_args: method_type_args.to_vec(),
                is_blanket: false,
            })
        };

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: struct_module,
                    name: final_mangled_name,
                    monomorph_info,
                    method_info: Some(LocalMethodName::new(
                        actual_struct_name,
                        trait_name_opt,
                        method_name.to_string(),
                    )),
                },
                args: args.to_vec(),
            },
            return_type,
            span,
        )
    }
}
