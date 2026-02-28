//! Item-level resolution (structs, functions, methods, globals, variants, tests).

use crate::ast::{self, Function, GlobalDecl, Type};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName};
use crate::tir::{
    TirFunction, TirGlobal, TirParam, TirStruct, TirTest, TirVariantCase, TirVariantDecl, TypeTable,
};

use super::Resolver;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        // Set up type parameters in scope before resolving fields
        let old_type_params = std::mem::take(&mut self.current_type_params);
        for (index, param) in struct_decl.type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), index as u32);
            self.current_type_params
                .insert(param.name.clone(), (index as u32, type_id));
        }

        let mut fields = Vec::new();
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = self.resolve_type(&field.ty);
            fields.push(crate::tir::TirField {
                name: field.name.clone(),
                is_pub: field.is_pub,
                type_id,
                index: index as u32,
                span: field.span,
                is_hidden: field.attrs.iter().any(|a| a.name == "hidden"),
            });
        }

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = struct_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        TirStruct {
            name: struct_decl.name.clone(),
            is_pub: struct_decl.is_pub,
            type_params,
            monomorph_info: None, // Not from monomorphization
            fields,
            span: struct_decl.span,
        }
    }

    /// Resolve a global variable declaration
    pub(super) fn resolve_global(&mut self, global_decl: &GlobalDecl) -> Option<TirGlobal> {
        // Resolve the type
        let ty = self.resolve_type(&global_decl.ty);

        // Create a minimal function context for resolving the initializer expression
        // Global initialization has no locals, but we need the context for expression resolution
        // The function name is used for #function compile-time literal (empty for global init)
        let mut ctx = FunctionContext::new(ty, format!("global:{}", global_decl.name));

        // Resolve the initializer expression with expected type for type inference
        let initializer = self.resolve_expr(&global_decl.initializer, &mut ctx, Some(ty));

        // Type check: initializer type must match declared type.
        // `never` (bottom type) is assignable to any type.
        if initializer.type_id != ty
            && initializer.type_id != TypeTable::UNKNOWN
            && initializer.type_id != TypeTable::NEVER
            && ty != TypeTable::UNKNOWN
        {
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: self.type_table.borrow().type_name(ty),
                found: self.type_table.borrow().type_name(initializer.type_id),
                span: global_decl.initializer.span(),
            });
        }

        Some(TirGlobal {
            name: global_decl.name.clone(),
            ty,
            initializer,
            mutable: global_decl.mutable,
            wado_mutable: global_decl.mutable,
            is_pub: global_decl.is_pub,
            module_source: self.current_module_source.clone(),
            span: global_decl.span,
            is_nullable: false, // Set by lower phase for lazy-init reference types
            local_types: ctx.local_types.clone(),
        })
    }

    /// Resolve a variant declaration
    pub(super) fn resolve_variant_decl(
        &mut self,
        variant_decl: &ast::VariantDecl,
    ) -> TirVariantDecl {
        // Set up type parameters in scope before resolving field types
        let old_type_params = std::mem::take(&mut self.current_type_params);
        for (index, param) in variant_decl.type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), index as u32);
            self.current_type_params
                .insert(param.name.clone(), (index as u32, type_id));
        }

        // Resolve each case - each variant case has exactly one payload type
        let mut cases = Vec::new();
        for (index, case) in variant_decl.cases.iter().enumerate() {
            // Unit variants have `()` (unit type) payload
            let payload = if let Some(payload_ty) = &case.payload {
                self.resolve_type(payload_ty)
            } else {
                TypeTable::UNIT
            };
            cases.push(TirVariantCase {
                name: case.name.clone(),
                index: index as u32,
                payload,
                span: case.span,
            });
        }

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = variant_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        TirVariantDecl {
            name: variant_decl.name.clone(),
            is_pub: variant_decl.is_pub,
            type_params,
            cases,
            span: variant_decl.span,
        }
    }

    /// Extract inline hint from function attributes.
    pub(super) fn extract_inline_hint(attrs: &[crate::ast::Attribute]) -> crate::tir::InlineHint {
        let Some(attr) = attrs.iter().find(|a| a.name == "inline") else {
            return crate::tir::InlineHint::Auto;
        };
        match attr.args.first().map(String::as_str) {
            Some("always") => crate::tir::InlineHint::Always,
            Some("never") => crate::tir::InlineHint::Never,
            None => crate::tir::InlineHint::Hint,
            _ => crate::tir::InlineHint::Auto,
        }
    }

    /// Extract compiler feature bitflags from `#[comp_feature("...")]` attributes.
    pub(super) fn extract_comp_features(attrs: &[crate::ast::Attribute]) -> u32 {
        let mut features = 0u32;
        for attr in attrs {
            if attr.name == "comp_feature" {
                for arg in &attr.args {
                    match arg.as_str() {
                        "array_append" => features |= crate::wir::COMP_FEATURE_ARRAY_APPEND,
                        "string_append" => features |= crate::wir::COMP_FEATURE_STRING_APPEND,
                        "string_append_char" => {
                            features |= crate::wir::COMP_FEATURE_STRING_APPEND_CHAR;
                        }
                        _ => {}
                    }
                }
            }
        }
        features
    }

    /// Resolve a function
    pub(super) fn resolve_function(&mut self, func: &Function) -> Option<TirFunction> {
        // Set up type parameters in scope before resolving types
        let old_type_params = std::mem::take(&mut self.current_type_params);
        let old_type_param_bounds = std::mem::take(&mut self.current_type_param_bounds);
        let mut type_param_list = Vec::new();
        for (index, param) in func.type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), index as u32);
            self.current_type_params
                .insert(param.name.clone(), (index as u32, type_id));
            if !param.bounds.is_empty() {
                self.current_type_param_bounds.insert(
                    param.name.clone(),
                    param.bounds.iter().map(|b| b.name.clone()).collect(),
                );
            }
            type_param_list.push((param.name.clone(), type_id));
        }

        // Store type parameters for generic functions (for call site substitution)
        if !func.type_params.is_empty() {
            self.generic_function_params
                .insert(func.name.clone(), type_param_list);
        }

        // Resolve return type annotation (used for task_return_type in async fns)
        let declared_return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        // For async functions, the Wasm-level return type is unit (the result is delivered
        // via `task return`, not via the Wasm function return). The declared return type is
        // stored as `task_return_type` for validating `task return expr`.
        let return_type = if func.is_async {
            TypeTable::UNIT
        } else {
            declared_return_type
        };

        // Update the function_return_types with the resolved return type
        // (This replaces the potentially incorrect type from static resolution)
        self.function_return_types
            .insert(func.name.clone(), return_type);

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(declared_return_type);
        }

        // Resolve parameters
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = self.resolve_type(&param.ty);
            let index = ctx.add_local(param.name.clone(), type_id, false);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                span: param.span,
            });
        }

        // Resolve body
        let body = func
            .body
            .as_ref()
            .map(|b| self.resolve_block(b, &mut ctx, None));

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope
        self.current_type_params = old_type_params;
        self.current_type_param_bounds = old_type_param_bounds;

        Some(TirFunction {
            name: func.name.clone(),
            is_pub: func.is_pub,
            is_export: func.is_export,
            is_async: func.is_async,
            type_params,
            impl_type_params: vec![], // Not a method, no impl type params
            monomorph_info: None,     // Not from monomorphization
            method_info: None,        // Not a method
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            // Scratch local fields - computed by lower phase
            is_cm_adapter: false,
            inline_hint: Self::extract_inline_hint(&func.attrs),
            comp_features: Self::extract_comp_features(&func.attrs),
        })
    }

    /// Resolve a test declaration to a `TirFunction` and `TirTest`
    pub(super) fn resolve_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
    ) -> Option<(TirFunction, TirTest)> {
        let expect_trap = test_decl.attributes.iter().any(|a| a.name == "expect_trap");
        let is_todo = test_decl.attributes.iter().any(|a| a.name == "TODO");

        // Generate function name: __test_{index} or __test_{name_snake_case}
        // For expect_trap tests: __test_trap_{index} or __test_trap_{index}_{name_snake_case}
        // For TODO tests:        __test_todo_{index} or __test_todo_{index}_{name_snake_case}
        let prefix = if is_todo {
            "__test_todo"
        } else if expect_trap {
            "__test_trap"
        } else {
            "__test"
        };
        let function_name = match &test_decl.name {
            Some(name) => {
                // Convert test name to snake_case for function name
                let snake_name: String = name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect::<String>()
                    .to_lowercase();
                format!("{prefix}_{test_index}_{snake_name}")
            }
            None => format!("{prefix}_{test_index}"),
        };

        // Create function context - tests have no parameters and return unit
        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());

        // Resolve the test body
        let body = self.resolve_block(&test_decl.body, &mut ctx, None);

        let tir_func = TirFunction {
            name: function_name.clone(),
            is_pub: false,    // Tests are not public
            is_export: false, // Tests are not world exports
            is_async: false,  // Tests are never async
            type_params: vec![],
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params: vec![], // Tests have no parameters
            return_type,
            effects: vec![], // Tests can have any effects (they're allowed to do I/O)
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            is_cm_adapter: false,
            inline_hint: crate::tir::InlineHint::Auto,
            comp_features: 0,
        };

        let tir_test = TirTest {
            name: test_decl.name.clone(),
            function_name,
            line: test_decl.span.line,
            span: test_decl.span,
            expect_trap,
            is_todo,
        };

        Some((tir_func, tir_test))
    }

    /// Resolve a method (function with &self parameter)
    pub(super) fn resolve_method(
        &mut self,
        func: &Function,
        struct_name: &str,
        impl_type: &Type,
        trait_name: Option<&str>,
    ) -> Option<TirFunction> {
        // Set up type parameters in scope before resolving types
        let old_type_params = std::mem::take(&mut self.current_type_params);
        let old_type_param_bounds = std::mem::take(&mut self.current_type_param_bounds);
        let mut type_param_list = Vec::new();

        // First, collect type params from impl block's generic type (e.g., impl Box<T>)
        // Also build impl_type_params for the TirFunction
        let mut impl_type_params = Vec::new();
        if let ast::Type::Generic(generic) = impl_type {
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
                        // Store impl type param info for later monomorphization
                        impl_type_params.push(crate::tir::TirTypeParam {
                            name: name.clone(),
                            bounds: vec![],
                            default: None, // Impl type params don't have defaults
                            index: i as u32,
                        });
                    }
                }
            }
        } else if let ast::Type::Named(named) = impl_type {
            // Blanket impl case: `impl<I: Iterator> IntoIterator for I`
            // The impl type is a type parameter itself, registered by the caller.
            // old_type_params holds the caller's type params (taken via std::mem::take above).
            if let Some(&(idx, _)) = old_type_params.get(&named.name) {
                let type_id = self
                    .type_table
                    .borrow_mut()
                    .make_type_param(named.name.clone(), idx);
                self.current_type_params
                    .insert(named.name.clone(), (idx, type_id));
                let bounds = old_type_param_bounds
                    .get(&named.name)
                    .cloned()
                    .unwrap_or_default();
                impl_type_params.push(crate::tir::TirTypeParam {
                    name: named.name.clone(),
                    bounds,
                    default: None,
                    index: idx,
                });
            }
        }

        // Populate bounds from the impl block's type_params
        // (inherited from outer scope - second-pass sets these up)
        // Re-read from current_type_param_bounds which was set by the caller
        // Actually, the caller (second-pass) already set up bounds, but we took them.
        // We need to restore them from old_type_param_bounds temporarily... NO.
        // The caller sets up bounds BEFORE calling resolve_method, so old_type_param_bounds
        // contains the caller's bounds. We should use those as base and add method-level bounds.
        self.current_type_param_bounds = old_type_param_bounds.clone();

        // Then, collect method-level type params
        let offset = self.current_type_params.len();
        for (index, param) in func.type_params.iter().enumerate() {
            let idx = (offset + index) as u32;
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), idx);
            self.current_type_params
                .insert(param.name.clone(), (idx, type_id));
            type_param_list.push((param.name.clone(), type_id));
            if !param.bounds.is_empty() {
                self.current_type_param_bounds.insert(
                    param.name.clone(),
                    param.bounds.iter().map(|b| b.name.clone()).collect(),
                );
            }
        }

        // Set up Self type for the impl block
        // This allows `&Self` to resolve correctly in method parameters
        let old_self_type = self.current_self_type;
        self.current_self_type = Some(self.resolve_type(impl_type));

        // Resolve return type
        let return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        // Update the function_return_types with the resolved return type
        // (This replaces the potentially incorrect type from static resolution)
        let mangled_name = MethodName::format_local(struct_name, trait_name, &func.name);
        self.function_return_types
            .insert(mangled_name.clone(), return_type);

        // Display name for #function: StructName::method_name
        let display_name = MethodName::format_local(struct_name, None, &func.name);
        let mut ctx = FunctionContext::new(return_type, display_name);

        // Resolve parameters (including &self)
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = match param.self_kind {
                ast::SelfKind::Ref => {
                    // &self: wrap impl type in immutable reference
                    let inner_type = self.resolve_type(impl_type);
                    self.type_table.borrow_mut().make_ref(inner_type)
                }
                ast::SelfKind::MutRef => {
                    // &mut self: wrap impl type in mutable reference
                    let inner_type = self.resolve_type(impl_type);
                    self.type_table.borrow_mut().make_mut_ref(inner_type)
                }
                ast::SelfKind::None => {
                    // Regular parameter
                    self.resolve_type(&param.ty)
                }
            };
            let index = ctx.add_local(param.name.clone(), type_id, false);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                span: param.span,
            });
        }

        // Resolve body
        let body = func
            .body
            .as_ref()
            .map(|b| self.resolve_block(b, &mut ctx, None));

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope and bounds
        self.current_type_params = old_type_params;
        self.current_type_param_bounds = old_type_param_bounds;

        // Store type parameters for generic methods (for call site substitution)
        if !func.type_params.is_empty() {
            self.generic_method_params
                .insert(mangled_name, type_param_list);
        }

        // Restore Self type
        self.current_self_type = old_self_type;

        Some(TirFunction {
            name: func.name.clone(), // Will be mangled by caller
            is_pub: func.is_pub,
            is_export: false, // Methods are not world exports
            is_async: false,  // Methods are never async
            type_params,
            impl_type_params, // Type params from impl block (e.g., T from impl Counter<T>)
            monomorph_info: None, // Not from monomorphization
            method_info: Some(LocalMethodName {
                struct_name: struct_name.to_string(),
                base_struct_name: struct_name.to_string(),
                trait_name: trait_name.map(String::from),
                method_name: func.name.clone(),
                method_type_args: vec![],
                is_type_param_receiver: false,
            }),
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            is_cm_adapter: false,
            inline_hint: Self::extract_inline_hint(&func.attrs),
            comp_features: Self::extract_comp_features(&func.attrs),
        })
    }
}
