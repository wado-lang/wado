//! Function call resolution.

use indexmap::IndexMap;

use crate::ast::{self, Expr, Item, Type};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{FunctionRef, ResolvedType, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

use super::Resolver;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_call(
        &mut self,
        call: &ast::CallExpr,
        ctx: &mut FunctionContext,
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
        let param_types = self.lookup_function_param_types(&call.callee);

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
                    // Check if this is a static method call (Type::method)
                    // Static methods are registered with mangled names "Type::method"
                    else if self.is_static_method(prefix, suffix) {
                        // Return as a static method call - will be converted to StaticCall below
                        let mangled_name = MethodName::format_local(prefix, None, suffix);
                        return self.resolve_static_method_call_from_qualified(
                            prefix,
                            suffix,
                            &mangled_name,
                            &args,
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
                        // Find the case by name
                        if let Some((case_index, case_data)) = variant_info
                            .cases
                            .iter()
                            .enumerate()
                            .find(|(_, c)| c.name == suffix)
                        {
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

                            // Create variant type
                            let variant_type = self.type_table.borrow_mut().make_variant(
                                prefix.to_string(),
                                variant_info.module_source.clone(),
                            );

                            // Payload is the single argument, or None for unit variants
                            let payload = args.into_iter().next().map(Box::new);

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
                        } else {
                            // Unknown case name
                            let _ = self.logger.error(TypeError::UnknownFunction {
                                name: format!("{prefix}::{suffix}"),
                                span: call.span,
                            });
                            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
                        }
                    }
                    // Effect operations and other qualified calls - always allowed
                    // (validated by effect system/codegen)
                    else {
                        // Effect-like modules (e.g., "Stdout") use Local module source
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
        let type_args: Vec<TypeId> = call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

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

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef::External {
                    module_source: callee_module,
                    name: func_name,
                    monomorph_info: None,
                    method_info: None, // Free function call
                },
                type_args,
                args,
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
                let old_type_params = std::mem::take(&mut self.current_type_params);
                for (i, type_param) in type_params.iter().enumerate() {
                    let type_id = self
                        .type_table
                        .borrow_mut()
                        .make_type_param(type_param.name.clone(), i as u32);
                    self.current_type_params
                        .insert(type_param.name.clone(), (i as u32, type_id));
                }

                // Resolve the return type in the callee module's context, not the caller's.
                // Build the callee module's flat maps so that type names resolve to the
                // callee's types, not the caller's (which may have same-named different types).
                let callee_module_ast = self.loaded_modules.get(callee_module);
                let (callee_imported, callee_original_names) = callee_module_ast.map_or_else(
                    || (IndexMap::new(), IndexMap::new()),
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
                self.current_type_params = old_type_params;

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
        let return_type = self
            .wasi_registry
            .get_function(&func_key)?
            .return_type
            .clone()?;

        // Resolve the AST type to a TypeId
        Some(self.resolve_wasi_type(&return_type))
    }

    /// Resolve a WASI AST type to a `TypeId`
    pub(super) fn resolve_wasi_type(&mut self, ty: &Type) -> TypeId {
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
                        let base_type = self.resolve_wasi_type(&aliased);
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
                    } else {
                        // Unknown type - represent as i32 handle
                        TypeTable::I32
                    }
                }
            },
            Type::Generic(generic) => match generic.name.as_str() {
                "Array" if generic.args.len() == 1 => {
                    let elem_type = self.resolve_wasi_type(&generic.args[0]);
                    self.type_table.borrow_mut().make_array(elem_type)
                }
                "Option" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type(&generic.args[0]);
                    self.type_table.borrow_mut().make_option(inner_type)
                }
                "Stream" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type(&generic.args[0]);
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::Stream(inner_type))
                }
                "Future" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type(&generic.args[0]);
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::Future(inner_type))
                }
                "Result" if generic.args.len() == 2 => {
                    let ok_type = self.resolve_wasi_type(&generic.args[0]);
                    let err_type = self.resolve_wasi_type(&generic.args[1]);
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
                    types.iter().map(|t| self.resolve_wasi_type(t)).collect();
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
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: ModuleSource::int128(),
                    name: mangled_func_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                args: vec![low_literal, high_literal],
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
}
