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
                // Check if the local has a function type
                let local_type = self.type_table.borrow().get(local.type_id).clone();
                if let ResolvedType::Function {
                    params: fn_params,
                    return_type,
                    ..
                } = local_type
                {
                    // This is a closure call!
                    let local_index = local.index;
                    let local_type_id = local.type_id;
                    let fn_return_type = return_type;
                    // Clone fn_params to avoid borrow conflict
                    let fn_params = fn_params.clone();

                    // Resolve arguments with coercion awareness based on closure param types
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
                            self.check_ref_type_mismatch(
                                arg.type_id,
                                expected,
                                call.args.get(i).map_or(call.span, ast::Expr::span),
                            );
                        }
                    }

                    // Create closure expression (Local reference)
                    let closure_expr = TirExpr::new(
                        TirExprKind::Local {
                            index: local_index,
                            name: ident.name.clone(),
                        },
                        local_type_id,
                        ident.span,
                    );

                    return TirExpr::new(
                        TirExprKind::IndirectCall {
                            callee: Box::new(closure_expr),
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
            let callee_type = self.type_table.borrow().get(callee_expr.type_id).clone();

            if let ResolvedType::Function {
                params: fn_params,
                return_type,
                ..
            } = callee_type
            {
                // This is calling a function stored in a field!
                let fn_return_type = return_type;
                let fn_params = fn_params.clone();

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
                        self.check_ref_type_mismatch(
                            arg.type_id,
                            expected,
                            call.args.get(i).map_or(call.span, ast::Expr::span),
                        );
                    }
                }

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
            if let Some(variant_info) = self.variant_cases.get(prefix).cloned()
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
        let args: Vec<TirExpr> = call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = param_types.get(i).copied();
                self.resolve_expr(arg, ctx, expected_type)
            })
            .collect();

        // Get function name from callee
        // callee_module_source is None for local calls (uses current module), Some for external calls
        let (callee_module_source, func_name, is_known) = match &call.callee {
            Expr::Ident(ident) => {
                // Check for qualified name with :: (e.g., "Stdout::write_via_stream")
                // Parser creates a single ident for Effect::operation syntax
                if let Some(pos) = ident.name.find("::") {
                    let prefix = &ident.name[..pos];
                    let suffix = &ident.name[pos + 2..];

                    // Builtin functions: resolve through core:builtin module
                    if prefix == "builtin" {
                        (Some(ModuleSource::builtin()), suffix.to_string(), true)
                    }
                    // Check if this is a type parameter static call (T::method where T: Trait)
                    else if let Some(&(_param_idx, type_param_type_id)) =
                        self.trait_ctx.type_params.get(prefix)
                    {
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
                        // Resolve method-level type args (e.g., i32::deserialize::<MockDeserializer>)
                        let mut method_type_args: Vec<TypeId> = call
                            .type_args
                            .iter()
                            .map(|ty| self.resolve_type(ty))
                            .collect();
                        // If no explicit type args, try to infer from argument types
                        let mangled_name = MethodName::format_local(prefix, None, suffix);
                        if method_type_args.is_empty() {
                            method_type_args = self.infer_type_args_from_args(suffix, &args);
                        }
                        if method_type_args.is_empty() {
                            method_type_args =
                                self.infer_type_args_from_method(prefix, suffix, &args);
                        }
                        // Register assoc type resolutions for inferred type args
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
                            if let Some(&newtype_type_id) = self.newtypes.get(prefix) {
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

                        return self.resolve_static_method_call_from_qualified(
                            prefix,
                            suffix,
                            &mangled_name,
                            &args,
                            &method_type_args,
                            call.span,
                            ctx,
                        );
                    }
                    // Check if this is a flags type method call: Perms::none(), Perms::all()
                    else if let Some(flags_info) = self.flags_cases.get(prefix).cloned()
                        && matches!(suffix, "none" | "all")
                    {
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
                    else if let Some(variant_info) = self.variant_cases.get(prefix) {
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
                                self.type_table
                                    .borrow_mut()
                                    .make_variant(prefix_owned, variant_info.module_source.clone())
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
                                prefix.to_string(),
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
                    // Effect operations and module namespace calls - pass through to codegen.
                    // This covers Stdout::write(), etc.
                    else {
                        (
                            Some(ModuleSource::Local {
                                path: prefix.to_string(),
                            }),
                            suffix.to_string(),
                            true,
                        )
                    }
                }
                // Check if it's a local function (defined in this module) or
                // a built-in type constructor (Ok, Err, Some, None)
                else if self.function_return_types.contains_key(&ident.name)
                    || matches!(ident.name.as_str(), "Ok" | "Err" | "Some" | "None")
                {
                    (None, ident.name.clone(), true)
                }
                // Check for prelude functions (panic, unreachable)
                // These are defined in core:internal and re-exported by core:prelude
                else if matches!(ident.name.as_str(), "panic" | "unreachable") {
                    (Some(ModuleSource::internal()), ident.name.clone(), true)
                }
                // Check if this is an imported function (per-module imports)
                else if self.imported_functions.contains(&ident.name) {
                    // Get module source from symbol table for codegen
                    if let Some(symbol) = self.symbols.lookup(&ident.name) {
                        (
                            Some(symbol.module_source.clone()),
                            symbol.name.clone(),
                            true,
                        )
                    } else {
                        // Imported but not in symbols - shouldn't happen but allow
                        (None, ident.name.clone(), true)
                    }
                } else {
                    // Unknown function - will report error
                    (None, ident.name.clone(), false)
                }
            }
            Expr::FieldAccess(field_access) => {
                // e.g., Stdout.write (unlikely but possible)
                // These are always considered known - validated elsewhere
                if let Expr::Ident(ident) = &field_access.expr {
                    (
                        Some(ModuleSource::Local {
                            path: ident.name.clone(),
                        }),
                        field_access.field.clone(),
                        true,
                    )
                } else {
                    (None, String::from("unknown"), false)
                }
            }
            _ => (None, String::from("unknown"), false),
        };

        // Report error for unknown functions
        if !is_known {
            let _ = self.logger.error(TypeError::UnknownFunction {
                name: func_name.clone(),
                span: call.span,
            });
        }

        // Resolve explicit type arguments
        let mut type_args: Vec<TypeId> = call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        // If no explicit type args, try to infer from argument types
        if type_args.is_empty() {
            type_args = self.infer_type_args_from_args(&func_name, &args);
        }

        // For local function calls (None), use the current module source
        // to ensure DCE and codegen can find the function correctly
        let callee_module =
            callee_module_source.unwrap_or_else(|| self.current_module_source.clone());

        // Check trait bounds on function type arguments
        if !type_args.is_empty() {
            self.check_function_type_arg_bounds(&callee_module, &func_name, &type_args, call.span);
        }

        // Look up function return type
        let mut return_type = self.lookup_function_return_type(&callee_module, &func_name);

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
        for (i, arg) in args.iter().enumerate() {
            if let Some(&expected) = check_param_types.get(i) {
                self.check_ref_type_mismatch(
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
                    module_source: callee_module,
                    name: func_name,
                    monomorph_info: None,
                    method_info: None, // Free function call,
                    is_cm_adapter: false,
                },
                type_args,
                args: call_args,
            },
            return_type,
            call.span,
        )
    }

    /// Look up the return type of a function
    pub(super) fn lookup_function_return_type(
        &mut self,
        callee_module: &ModuleSource,
        func_name: &str,
    ) -> TypeId {
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
            && let Some(effect_name) = callee_module.effect_name()
            && let Some(return_type) = self.get_wasi_effect_return_type(&effect_name, func_name)
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
            let func_info = self.loaded_modules.get(callee_module).and_then(|module| {
                module.items.iter().find_map(|item| {
                    if let Item::Function(func) = item
                        && func.name == func_name
                    {
                        Some((func.return_type.clone(), func.type_params.clone()))
                    } else {
                        None
                    }
                })
            });

            if let Some((ty, type_params)) = func_info
                && let Some(return_type_ast) = ty
            {
                // Set up the function's type parameters in scope so we can resolve
                // type parameter references (like T -> TypeParam { index: 0 })
                let old_type_params = std::mem::take(&mut self.trait_ctx.type_params);
                for (i, type_param) in type_params.iter().enumerate() {
                    let type_id = self
                        .type_table
                        .borrow_mut()
                        .make_type_param(type_param.name.clone(), i as u32);
                    self.trait_ctx
                        .type_params
                        .insert(type_param.name.clone(), (i as u32, type_id));
                }

                // Resolve the return type in the callee module's context, not the caller's.
                // Build the callee module's flat maps so that type names resolve to the
                // callee's types, not the caller's (which may have same-named different types).
                let callee_module_ast = self.loaded_modules.get(callee_module);
                let (callee_imported, callee_original_names) = callee_module_ast.map_or_else(
                    || (IndexMap::default(), IndexMap::default()),
                    |m| Self::build_imported_type_sources(m, callee_module),
                );
                let callee_newtypes = Self::build_module_map(
                    &self.all_newtypes,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_struct_fields = Self::build_module_map(
                    &self.all_struct_fields,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_variant_cases = Self::build_module_map(
                    &self.all_variant_cases,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_enum_cases = Self::build_module_map(
                    &self.all_enum_cases,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_flags_cases = Self::build_module_map(
                    &self.all_flags_cases,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_resource_types = Self::build_module_map(
                    &self.all_resource_types,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );

                // Temporarily swap in callee's flat maps
                let old_newtypes = std::mem::replace(&mut self.newtypes, callee_newtypes);
                let old_struct_fields =
                    std::mem::replace(&mut self.struct_fields, callee_struct_fields);
                let old_variant_cases =
                    std::mem::replace(&mut self.variant_cases, callee_variant_cases);
                let old_enum_cases = std::mem::replace(&mut self.enum_cases, callee_enum_cases);
                let old_flags_cases = std::mem::replace(&mut self.flags_cases, callee_flags_cases);
                let old_resource_types =
                    std::mem::replace(&mut self.resource_types, callee_resource_types);

                let resolved = self.resolve_type(&return_type_ast);

                // Restore caller's flat maps and type params
                self.newtypes = old_newtypes;
                self.struct_fields = old_struct_fields;
                self.variant_cases = old_variant_cases;
                self.enum_cases = old_enum_cases;
                self.flags_cases = old_flags_cases;
                self.resource_types = old_resource_types;
                self.trait_ctx.type_params = old_type_params;

                return resolved;
            }
        }

        // Default to UNIT for unknown functions (they might be external/builtin)
        TypeTable::UNIT
    }

    /// Get the return type of a WASI effect operation from the registry
    pub(super) fn get_wasi_effect_return_type(
        &mut self,
        effect: &str,
        operation: &str,
    ) -> Option<TypeId> {
        // Look up the function in the WASI registry and clone the return type
        // to avoid borrow checker issues
        let func_key = format!("{effect}::{operation}");
        let func = self.wasi_registry.get_function(&func_key)?;
        let return_type = func.return_type.clone()?;
        let package = func.package.clone();

        // Resolve the AST type to a TypeId, scoped to the function's WASI package
        Some(self.resolve_wasi_type_scoped(&return_type, Some(&package)))
    }

    /// Resolve a WASI AST type to a `TypeId`
    pub(super) fn resolve_wasi_type(&mut self, ty: &Type) -> TypeId {
        self.resolve_wasi_type_scoped(ty, None)
    }

    /// Resolve a WASI AST type to a `TypeId`, with optional WASI package scope.
    pub(super) fn resolve_wasi_type_scoped(&mut self, ty: &Type, wasi_package: Option<&str>) -> TypeId {
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
                    if let Some(&newtype_id) = self.newtypes.get(&named.name) {
                        return newtype_id;
                    }
                    // Otherwise, try to resolve via WASI registry's newtypes
                    let aliased = self.wasi_registry.get_newtype(&named.name).cloned();
                    if let Some(aliased) = aliased {
                        // Create a newtype for this WASI newtype
                        let base_type = self.resolve_wasi_type_scoped(&aliased, wasi_package);
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            named.name.clone(),
                            ModuleSource::wasi("clocks"),
                            base_type,
                        );
                        // Cache the newtype for future lookups
                        self.newtypes.insert(named.name.clone(), newtype_id);
                        newtype_id
                    } else if self.wasi_registry.is_resource(&named.name) {
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
                            .unwrap_or_else(|| ModuleSource::wasi("filesystem"));
                        self.type_table
                            .borrow_mut()
                            .make_resource(named.name.clone(), module_source)
                    } else if let Some(pkg) = wasi_package {
                        // Scoped lookup: use type table's package-aware search
                        let tt = self.type_table.borrow();
                        if let Some(tid) = tt.find_named_type_by_wasi_package(&named.name, pkg) {
                            tid
                        } else {
                            drop(tt);
                            // Fallback: enum → struct → i32
                            if self.wasi_registry.is_enum(&named.name) {
                                let module_source = self.all_enum_cases.iter()
                                    .find_map(|(ms, map)| map.contains_key(&named.name).then(|| ms.clone()))
                                    .unwrap_or_else(|| ModuleSource::wasi("cli"));
                                self.type_table.borrow_mut().make_enum(named.name.clone(), module_source)
                            } else if self.wasi_registry.is_struct(&named.name) {
                                let module_source = self.all_struct_fields.iter()
                                    .find_map(|(ms, map)| map.contains_key(&named.name).then(|| ms.clone()))
                                    .unwrap_or_else(|| ModuleSource::wasi("clocks"));
                                self.type_table.borrow_mut().make_struct(named.name.clone(), module_source)
                            } else {
                                TypeTable::I32
                            }
                        }
                    } else if self.wasi_registry.is_enum(&named.name) {
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
                            .unwrap_or_else(|| ModuleSource::wasi("cli"));
                        self.type_table
                            .borrow_mut()
                            .make_enum(named.name.clone(), module_source)
                    } else if self.wasi_registry.is_struct(&named.name) {
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
                            .unwrap_or_else(|| ModuleSource::wasi("clocks"));
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
                let resolved: Vec<TypeId> =
                    types.iter().map(|t| self.resolve_wasi_type_scoped(t, wasi_package)).collect();
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Tuple(resolved))
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
                    is_cm_adapter: false,
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
                        if let Some(module) = self.loaded_modules.get(&module_source) {
                            let params: Option<Vec<_>> = module.items.iter().find_map(|item| {
                                if let Item::Function(func) = item
                                    && func.name == suffix
                                {
                                    return Some(func.params.clone());
                                }
                                None
                            });
                            if let Some(params) = params {
                                return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
                            }
                        }
                    }

                    return Vec::new(); // Effect operations handled separately
                }

                // Check if it's a local function (defined in this module)
                if self.function_return_types.contains_key(&ident.name) {
                    // Clone params to avoid borrow issues
                    let params: Option<Vec<_>> =
                        self.current_module_items.iter().find_map(|item| {
                            if let Item::Function(func) = item
                                && func.name == ident.name
                            {
                                return Some(func.params.clone());
                            }
                            None
                        });

                    if let Some(params) = params {
                        return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
                    }
                }

                // Check imported functions
                if let Some(symbol) = self.symbols.lookup(&ident.name)
                    && let Some(module) = self.loaded_modules.get(&symbol.module_source)
                {
                    // Clone params to avoid borrow issues
                    let params: Option<Vec<_>> = module.items.iter().find_map(|item| {
                        if let Item::Function(func) = item
                            && func.name == symbol.name
                        {
                            return Some(func.params.clone());
                        }
                        None
                    });

                    if let Some(params) = params {
                        return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
                    }
                }

                Vec::new()
            }
            _ => Vec::new(),
        }
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
        if self.function_return_types.contains_key(&ident.name) {
            let result: Option<Vec<bool>> = self.current_module_items.iter().find_map(|item| {
                if let Item::Function(func) = item
                    && func.name == ident.name
                {
                    return Some(func.params.iter().map(|p| p.is_mut).collect());
                }
                None
            });
            if let Some(is_muts) = result {
                return is_muts;
            }
        }

        // Imported function
        if let Some(symbol) = self.symbols.lookup(&ident.name)
            && let Some(module) = self.loaded_modules.get(&symbol.module_source)
        {
            let result: Option<Vec<bool>> = module.items.iter().find_map(|item| {
                if let Item::Function(func) = item
                    && func.name == symbol.name
                {
                    return Some(func.params.iter().map(|p| p.is_mut).collect());
                }
                None
            });
            if let Some(is_muts) = result {
                return is_muts;
            }
        }

        Vec::new()
    }

    /// Infer type arguments for a generic function call from the actual argument types.
    /// Uses pre-resolved param types (stored during function resolution in the function's
    /// own type param scope, so `TypeParam` ids are correct).
    /// Returns the inferred type args in declaration order, or empty vec if inference fails.
    fn infer_type_args_from_args(&self, func_name: &str, args: &[TirExpr]) -> Vec<TypeId> {
        let Some(type_param_list) = self.generic_function_params.get(func_name) else {
            return vec![];
        };
        let Some(resolved_param_types) = self.generic_function_resolved_param_types.get(func_name)
        else {
            return vec![];
        };
        let mut type_param_map: IndexMap<TypeId, TypeId> = IndexMap::default();
        for (param_type, arg) in resolved_param_types.iter().zip(args.iter()) {
            self.unify_types_for_inference(*param_type, arg.type_id, &mut type_param_map);
        }
        let inferred: Vec<TypeId> = type_param_list
            .iter()
            .map(|(_, id)| type_param_map.get(id).copied().unwrap_or(*id))
            .collect();
        let all_concrete = inferred.iter().all(|&id| {
            !matches!(
                self.type_table.borrow().get(id),
                ResolvedType::TypeParam { .. }
            )
        });
        if all_concrete && !inferred.is_empty() {
            inferred
        } else {
            vec![]
        }
    }

    /// Infer type arguments for a generic static method by looking up the method in loaded modules.
    /// Works cross-module unlike `infer_type_args_from_args` (which requires same-module data).
    fn infer_type_args_from_method(
        &mut self,
        struct_name: &str,
        method_name: &str,
        args: &[TirExpr],
    ) -> Vec<TypeId> {
        // Find the method in loaded modules
        let method_info: Option<(Vec<ast::GenericParam>, Vec<ast::Param>)> = {
            let mut found = None;
            'outer: for (_, module) in self.loaded_modules {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item
                        && Self::get_type_name_static(&impl_block.ty) == struct_name
                    {
                        for method in &impl_block.methods {
                            if method.name == method_name && !method.type_params.is_empty() {
                                found = Some((method.type_params.clone(), method.params.clone()));
                                break 'outer;
                            }
                        }
                    }
                }
            }
            // Also check current module items
            if found.is_none() {
                'outer2: for item in &self.current_module_items.clone() {
                    if let Item::Impl(impl_block) = item
                        && Self::get_type_name_static(&impl_block.ty) == struct_name
                    {
                        for method in &impl_block.methods {
                            if method.name == method_name && !method.type_params.is_empty() {
                                found = Some((method.type_params.clone(), method.params.clone()));
                                break 'outer2;
                            }
                        }
                    }
                }
            }
            found
        };
        let Some((type_params, params)) = method_info else {
            return vec![];
        };

        // Temporarily create TypeParam TypeIds for this method's type params
        let saved_trait_ctx = self.trait_ctx.clone();
        let mut type_param_list: Vec<(String, TypeId)> = vec![];
        for (i, tp) in type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(tp.name.clone(), i as u32);
            self.trait_ctx
                .type_params
                .insert(tp.name.clone(), (i as u32, type_id));
            if !tp.bounds.is_empty() {
                self.trait_ctx
                    .type_param_bounds
                    .insert(tp.name.clone(), tp.bounds.clone());
            }
            type_param_list.push((tp.name.clone(), type_id));
        }

        // Resolve non-self param types in the context of the method's type params
        let resolved_param_types: Vec<TypeId> = params
            .iter()
            .filter(|p| p.self_kind == ast::SelfKind::None)
            .map(|p| self.resolve_type(&p.ty))
            .collect();

        self.trait_ctx = saved_trait_ctx;

        // Unify resolved param types against actual arg types
        let mut type_param_map: IndexMap<TypeId, TypeId> = IndexMap::default();
        for (param_type, arg) in resolved_param_types.iter().zip(args.iter()) {
            self.unify_types_for_inference(*param_type, arg.type_id, &mut type_param_map);
        }
        let inferred: Vec<TypeId> = type_param_list
            .iter()
            .map(|(_, id)| type_param_map.get(id).copied().unwrap_or(*id))
            .collect();
        let all_concrete = inferred.iter().all(|&id| {
            !matches!(
                self.type_table.borrow().get(id),
                ResolvedType::TypeParam { .. }
            )
        });
        if all_concrete && !inferred.is_empty() {
            inferred
        } else {
            vec![]
        }
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
                self.current_module_items.iter().find_map(|item| {
                    if let Item::Function(func) = item
                        && func.name == ident.name
                    {
                        Some((func.type_params.clone(), func.params.clone()))
                    } else {
                        None
                    }
                })
            } else if let Some(symbol) = self.symbols.lookup(&ident.name)
                && let Some(module) = self.loaded_modules.get(&symbol.module_source)
            {
                module.items.iter().find_map(|item| {
                    if let Item::Function(func) = item
                        && func.name == symbol.name
                    {
                        Some((func.type_params.clone(), func.params.clone()))
                    } else {
                        None
                    }
                })
            } else {
                None
            };

        let Some((fn_type_params, fn_params)) = func_info else {
            return Vec::new();
        };

        // Temporarily register type params so resolve_type can find them
        let saved = std::mem::take(&mut self.trait_ctx.type_params);
        for (i, tp) in fn_type_params.iter().enumerate() {
            let idx = i as u32;
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(tp.name.clone(), idx);
            self.trait_ctx
                .type_params
                .insert(tp.name.clone(), (idx, type_id));
        }

        let param_types: Vec<TypeId> = fn_params.iter().map(|p| self.resolve_type(&p.ty)).collect();

        self.trait_ctx.type_params = saved;

        // Substitute type params with explicit type args
        param_types
            .iter()
            .map(|&pt| self.substitute_type_params(pt, type_args))
            .collect()
    }

    pub(super) fn infer_variant_type_args(
        &mut self,
        variant_name: &str,
        variant_info: &super::types::VariantInfo,
        case_data: &super::types::VariantCaseData,
        payload: Option<&TirExpr>,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        let mut type_param_map: IndexMap<TypeId, TypeId> = IndexMap::default();
        // Track the canonical module_source from expected_type if available.
        // This ensures the created GenericInstance uses the same module_source
        // as the type annotation (e.g., ModuleSource::prelude() for Option/Result),
        // which may differ from variant_info.module_source (e.g., prelude/types.wado).
        let mut canonical_module_source = None;

        // Forward inference: unify payload type with actual arg type
        if let Some(payload_expr) = payload {
            self.unify_types_for_inference(
                case_data.payload,
                payload_expr.type_id,
                &mut type_param_map,
            );
        }

        // Backward inference: extract type args from expected_type
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
                for (i, &expected_arg) in expected_args.iter().enumerate() {
                    let type_param_id = variant_info.type_param_type_ids[i];
                    type_param_map.entry(type_param_id).or_insert(expected_arg);
                }
            }
        }

        let module_source =
            canonical_module_source.unwrap_or_else(|| variant_info.module_source.clone());

        // Build type_args in declaration order
        let type_args: Vec<TypeId> = variant_info
            .type_param_type_ids
            .iter()
            .enumerate()
            .map(|(i, &param_id)| {
                type_param_map
                    .get(&param_id)
                    .copied()
                    .unwrap_or(variant_info.type_param_type_ids[i])
            })
            .collect();

        // If unresolved type params remain in concrete code, fall back to bare Variant
        let has_unresolved = type_args
            .iter()
            .any(|&t| self.type_table.borrow().contains_type_param(t));

        if has_unresolved && self.trait_ctx.type_params.is_empty() {
            return self
                .type_table
                .borrow_mut()
                .make_variant(variant_name.to_string(), variant_info.module_source.clone());
        }

        self.type_table.borrow_mut().make_generic_instance(
            variant_name.to_string(),
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
            let method_type_args: Vec<TypeId> = call
                .type_args
                .iter()
                .map(|ty| self.resolve_type(ty))
                .collect();

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
                    type_args: method_type_args,
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
                        is_cm_adapter: false,
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
