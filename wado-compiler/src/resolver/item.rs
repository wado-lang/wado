//! Item-level resolution (structs, functions, methods, globals, variants, tests).

use crate::ast::{self, Function, GlobalDecl, SelfKind, Type};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexSet;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    TirEffect, TirEffectOp, TirFunction, TirGlobal, TirParam, TirResource, TirStruct, TirTest,
    TirVariantCase, TirVariantDecl, TypeId, TypeTable,
};

use super::Resolver;
use super::types::{FunctionContext, TypeError};

/// Extract compiler feature bitflags from `#[comp_feature("...")]` attributes.
pub(super) fn extract_comp_features(attrs: &[crate::ast::Attribute]) -> u32 {
    let mut features = 0u32;
    for attr in attrs {
        if attr.name == "comp_feature" {
            for arg in &attr.args {
                match arg.as_str() {
                    "array_push" => features |= crate::wir::COMP_FEATURE_ARRAY_PUSH,
                    "string_push_str" => features |= crate::wir::COMP_FEATURE_STRING_PUSH_STR,
                    "string_push_char" => {
                        features |= crate::wir::COMP_FEATURE_STRING_PUSH_CHAR;
                    }
                    "option" => features |= crate::wir::COMP_FEATURE_OPTION,
                    "result" => features |= crate::wir::COMP_FEATURE_RESULT,
                    "default" => features |= crate::wir::COMP_FEATURE_DEFAULT,
                    "from" => features |= crate::wir::COMP_FEATURE_FROM,
                    "tuple" => features |= crate::wir::COMP_FEATURE_TUPLE,
                    "box" => features |= crate::wir::COMP_FEATURE_BOX,
                    "range_exclusive" => {
                        features |= crate::wir::COMP_FEATURE_RANGE_EXCLUSIVE;
                    }
                    "range_inclusive" => {
                        features |= crate::wir::COMP_FEATURE_RANGE_INCLUSIVE;
                    }
                    _ => {}
                }
            }
        }
    }
    features
}

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        // Set up type parameters in scope before resolving fields. Use an
        // inherited scope so that any caller-provided `assoc_type_bindings` or
        // `self_type` remain visible — only `type_params` are replaced, matching
        // the original `mem::take(&mut self.trait_ctx.type_params)` semantics.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(&struct_decl.type_params, 0);

        // Resolve field defaults in the struct's module scope. A field default
        // is standalone (no self, no other fields in scope) and must be pure;
        // the purity check runs in `effect_check`.
        let mut field_ctx =
            FunctionContext::new(TypeTable::UNIT, format!("struct:{}", struct_decl.name));
        let mut fields = Vec::new();
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = scope.resolve_type(&field.ty);
            let serde_rename = field.attrs.iter().find_map(|a| {
                if a.name == "serde" {
                    a.kv_value("rename").map(str::to_string)
                } else {
                    None
                }
            });
            let default_expr = field.default.as_ref().map(|default_ast| {
                let resolved = scope.resolve_expr(default_ast, &mut field_ctx, Some(type_id));
                scope.typecheck(resolved.type_id, type_id, default_ast.span());
                Box::new(resolved)
            });
            // A field with a declared default is implicitly non-required at
            // deserialization time: the synthesized Deserialize uses the
            // default expression when the field is absent (WEP 2026-04-11).
            let serde_default = default_expr.is_some()
                || field
                    .attrs
                    .iter()
                    .any(|a| a.name == "serde" && a.has_arg("default"));
            fields.push(crate::tir::TirField {
                name: field.name.clone(),
                is_pub: field.is_pub,
                type_id,
                index: index as u32,
                span: field.span,
                is_hidden: field.attrs.iter().any(|a| a.name == "hidden"),
                serde_rename,
                serde_default,
                default_expr,
            });
        }

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = struct_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        drop(scope);

        let serde_rename_all = struct_decl.attrs.iter().find_map(|a| {
            if a.name == "serde" {
                a.kv_value("rename_all").map(str::to_string)
            } else {
                None
            }
        });

        TirStruct {
            name: struct_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: struct_decl.is_pub,
            type_params,
            monomorph_info: None, // Not from monomorphization
            fields,
            span: struct_decl.span,
            serde_rename_all,
        }
    }

    /// Resolve the `methods` list of an effect or resource declaration into
    /// TIR operations. Generic type parameters declared on the enclosing
    /// resource (e.g. `resource Stream<T>`) are brought into scope before
    /// resolving each method's params and return type so that `T` maps to a
    /// proper `TypeParam` rather than `UNKNOWN`.
    fn resolve_effect_ops(
        &mut self,
        type_params: &[ast::GenericParam],
        methods: &[ast::EffectMethod],
    ) -> Vec<TirEffectOp> {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(type_params, 0);

        let mut ops = Vec::with_capacity(methods.len());
        for method in methods {
            let mut params = Vec::with_capacity(method.params.len());
            for (idx, p) in method.params.iter().enumerate() {
                if !matches!(p.self_kind, SelfKind::None) {
                    continue;
                }
                let type_id = scope.resolve_type(&p.ty);
                params.push(TirParam {
                    name: p.name.clone(),
                    type_id,
                    local_index: idx as u32,
                    is_mut: p.is_mut,
                    default_expr: None,
                    span: p.span,
                });
            }
            let return_type = method
                .return_type
                .as_ref()
                .map(|ty| scope.resolve_type(ty))
                .unwrap_or(TypeTable::UNIT);
            ops.push(TirEffectOp {
                name: method.name.clone(),
                params,
                return_type,
                span: method.span,
            });
        }
        ops
    }

    pub(super) fn resolve_effect_decl(&mut self, decl: &ast::EffectDecl) -> TirEffect {
        let operations = self.resolve_effect_ops(&[], &decl.methods);
        TirEffect {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
        }
    }

    pub(super) fn resolve_resource_decl(&mut self, decl: &ast::ResourceDecl) -> TirResource {
        let operations = self.resolve_effect_ops(&decl.type_params, &decl.methods);
        TirResource {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
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
        self.typecheck(initializer.type_id, ty, global_decl.initializer.span());

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
        // Set up type parameters in scope before resolving field types. Use an
        // inherited scope so any caller-provided `assoc_type_bindings`/`self_type`
        // stay visible — only `type_params` are replaced, matching the original
        // `mem::take(&mut self.trait_ctx.type_params)` semantics.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(&variant_decl.type_params, 0);

        // Resolve each case - each variant case has exactly one payload type
        let mut cases = Vec::new();
        for (index, case) in variant_decl.cases.iter().enumerate() {
            // Unit variants have `()` (unit type) payload
            let payload = if let Some(payload_ty) = &case.payload {
                scope.resolve_type(payload_ty)
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
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        drop(scope);

        let comp_features = extract_comp_features(&variant_decl.attrs);
        if comp_features != 0 {
            self.type_table
                .borrow_mut()
                .register_comp_feature_variant(comp_features, self.current_module_source.clone());
        }

        TirVariantDecl {
            name: variant_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: variant_decl.is_pub,
            type_params,
            cases,
            comp_features,
            span: variant_decl.span,
        }
    }

    /// Extract custom wasm export name from `#[export_name("...")]` attribute.
    pub(super) fn extract_export_name(attrs: &[crate::ast::Attribute]) -> Option<String> {
        attrs
            .iter()
            .find(|a| a.name == "export_name")
            .and_then(|a| a.args.first())
            .map(|a| a.as_str().to_string())
    }

    /// Extract allocator tag from `#[allocator("...")]` attribute.
    fn extract_allocator_tag(attrs: &[crate::ast::Attribute]) -> Option<String> {
        attrs
            .iter()
            .find(|a| a.name == "allocator")
            .and_then(|a| a.args.first())
            .map(|a| a.as_str().to_string())
    }

    /// Extract `#[ambient]` marker from function attributes.
    /// Ambient functions bypass effect-check propagation to callers: they may
    /// declare effects for implementation purposes, but callers don't need to
    /// list those effects. Intended for low-level helpers like `log_stdout`
    /// that use an implicit ambient capability.
    pub(super) fn extract_is_ambient(attrs: &[crate::ast::Attribute]) -> bool {
        attrs.iter().any(|a| a.name == "ambient")
    }

    pub(super) fn extract_inline_hint(attrs: &[crate::ast::Attribute]) -> crate::tir::InlineHint {
        let Some(attr) = attrs.iter().find(|a| a.name == "inline") else {
            return crate::tir::InlineHint::Auto;
        };
        match attr.args.first().map(super::super::ast::AttrArg::as_str) {
            Some("always") => crate::tir::InlineHint::Always,
            Some("never") => crate::tir::InlineHint::Never,
            None => crate::tir::InlineHint::Hint,
            _ => crate::tir::InlineHint::Auto,
        }
    }

    /// Validate that stores declarations reference valid reference parameters.
    fn validate_stores(&self, stores: &[String], params: &[TirParam], span: crate::token::Span) {
        let tt = self.type_table.borrow();
        for store_name in stores {
            if let Some(param) = params.iter().find(|p| p.name == *store_name) {
                let resolved = tt.get(param.type_id);
                // Allow stores on: reference types (&T, &mut T) and type parameters (T may be &U)
                if !matches!(
                    resolved,
                    crate::tir::ResolvedType::Ref(_)
                        | crate::tir::ResolvedType::MutRef(_)
                        | crate::tir::ResolvedType::TypeParam { .. }
                ) {
                    let type_name = tt.type_name(param.type_id);
                    let _ = self.logger.error(TypeError::InvalidStores {
                        message: format!(
                            "stores[{store_name}]: parameter '{store_name}' has type '{type_name}', \
                             but only reference parameters (&T or &mut T) or type parameters can be stored"
                        ),
                        span,
                    });
                }
            } else {
                let _ = self.logger.error(TypeError::InvalidStores {
                    message: format!("stores[{store_name}]: no parameter named '{store_name}'"),
                    span,
                });
            }
        }
    }

    /// Populate the generic-function inference caches (type params,
    /// resolved param types, resolved return type) for `func` without
    /// resolving its body. Used as a pre-pass before body resolution so
    /// that same-module forward references to other generic functions
    /// (e.g. `outer<T>` defined before `inner<T>` in the same file) can
    /// run argument-derived type inference at the call site.
    ///
    /// Idempotent: may be called multiple times. Uses fresh `TypeId`s
    /// each time; subsequent overwrites inside `resolve_function` keep
    /// the cache consistent with the body's own `TypeId`s.
    pub(super) fn precompute_generic_function_cache(&mut self, func: &Function) {
        if !func.type_params.iter().any(|p| !p.is_effect) {
            return;
        }
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.trait_ctx.type_param_bounds.clear();
        scope.register_generic_params(&func.type_params, 0);
        scope.populate_generic_function_cache(func);
    }

    /// Populate the three generic-function inference caches
    /// (`generic_function_params`, `generic_function_resolved_param_types`,
    /// `generic_function_resolved_return_types`) for `func`, keyed by its
    /// name. Type parameters must already be registered in
    /// `self.trait_ctx.type_params` before calling this.
    ///
    /// Returns the resolved declared return type so callers that need it
    /// (e.g. `resolve_function` for `task_return_type`) can avoid resolving
    /// it a second time.
    fn populate_generic_function_cache(&mut self, func: &Function) -> TypeId {
        let type_param_list: Vec<(String, TypeId)> = func
            .type_params
            .iter()
            .filter(|p| !p.is_effect)
            .filter_map(|p| {
                self.trait_ctx
                    .type_params
                    .get(&p.name)
                    .map(|&(_, id)| (p.name.clone(), id))
            })
            .collect();
        let resolved_param_types: Vec<TypeId> = func
            .params
            .iter()
            .filter(|p| p.self_kind == SelfKind::None)
            .map(|p| self.resolve_type(&p.ty))
            .collect();
        let declared_return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);
        self.generic_function_params
            .insert(func.name.clone(), type_param_list);
        self.generic_function_resolved_param_types
            .insert(func.name.clone(), resolved_param_types);
        self.generic_function_resolved_return_types
            .insert(func.name.clone(), declared_return_type);
        declared_return_type
    }

    /// Resolve a function
    pub(super) fn resolve_function(&mut self, func: &Function) -> Option<TirFunction> {
        // Set up type parameters in scope before resolving types. Use an
        // inherited scope so any caller-provided `assoc_type_bindings`/`self_type`
        // stay visible — only `type_params` and `type_param_bounds` are replaced,
        // matching the original `mem::take` semantics for those two fields.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.trait_ctx.type_param_bounds.clear();
        scope.register_generic_params(&func.type_params, 0);

        // Set effect params in scope (for resolving effect names in function types)
        let old_effect_params = std::mem::take(&mut scope.current_effect_params);
        let old_effect_param_decls = std::mem::take(&mut scope.current_effect_param_decls);
        let effect_params: Vec<_> = func.type_params.iter().filter(|p| p.is_effect).collect();
        if effect_params.len() > 1 {
            let _ = scope.logger.error(TypeError::InvalidLiteral {
                message: "multiple effect parameters are not allowed; use a single effect parameter instead".to_string(),
                span: effect_params[1].span,
            });
        }
        scope.current_effect_params = effect_params.iter().map(|p| p.name.clone()).collect();
        scope.current_effect_param_decls = effect_params
            .iter()
            .map(|p| (p.name.clone(), p.id))
            .collect();

        // Populate the generic-function inference caches
        // (`generic_function_params`, `generic_function_resolved_param_types`,
        // `generic_function_resolved_return_types`). Populated before the
        // `function_return_types` update below because that map is shared
        // with non-generic callers and may be overwritten by external
        // registrations (trait methods, etc.) over time. The declared return
        // type is also used for `task_return_type` in async fns.
        let has_real_type_params = func.type_params.iter().any(|p| !p.is_effect);
        let declared_return_type = if has_real_type_params {
            scope.populate_generic_function_cache(func)
        } else {
            func.return_type
                .as_ref()
                .map(|t| scope.resolve_type(t))
                .unwrap_or(TypeTable::UNIT)
        };

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
        scope
            .function_return_types
            .insert(func.name.clone(), return_type);

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(declared_return_type);
        }

        // Resolve parameters. Each default expression is resolved in the
        // callee's lexical scope, with only the earlier parameters in scope
        // (a default cannot reference its own parameter or any later one).
        // This gives defaults access to the definition module's private items
        // and earlier parameters without needing call-site substitution.
        //
        // `export fn` is rejected: the Component Model ABI requires every
        // parameter at the boundary, so defaults cannot divergently exist
        // only on the Wado side.
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = scope.resolve_type(&param.ty);
            let default_expr = param.default.as_ref().map(|default_ast| {
                if func.is_export {
                    let _ = scope.logger.error(TypeError::DefaultInExportFn {
                        function: func.name.clone(),
                        param: param.name.clone(),
                        span: default_ast.span(),
                    });
                }
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(type_id));
                scope.typecheck(resolved.type_id, type_id, default_ast.span());
                Box::new(resolved)
            });
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
            scope.record_local_symbol(param.id, &param.name, param.name_span, param.is_mut);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                default_expr,
                span: param.span,
            });
        }

        // Validate stores declarations
        scope.validate_stores(&func.stores, &params, func.span);

        // Resolve body
        let body = func
            .body
            .as_ref()
            .map(|b| scope.resolve_block(b, &mut ctx, None));

        // Validate: non-unit return type requires explicit `return` in the body
        if return_type != TypeTable::UNIT
            && return_type != TypeTable::NEVER
            && let Some(ref body) = body
            && Self::find_return_type_in_block(body).is_none()
        {
            let _ = scope.logger.error(TypeError::MissingReturn {
                return_type: scope.type_table.borrow().type_name(return_type),
                span: func.span,
            });
        }

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Resolve effects while effect params are still in scope
        let effects = scope.resolve_effects(&func.effects, &func.effect_ids);

        // Restore effect params scope
        scope.current_effect_params = old_effect_params;
        scope.current_effect_param_decls = old_effect_param_decls;
        drop(scope);

        Some(TirFunction {
            module_source: ModuleSource::default(),
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
            effects,
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: IndexSet::default(),
            // Scratch local fields - computed by lower phase
            is_cm_binding: false,
            is_cm_export: false,
            is_ambient: Self::extract_is_ambient(&func.attrs),
            inline_hint: Self::extract_inline_hint(&func.attrs),
            comp_features: extract_comp_features(&func.attrs),
            export_name: Self::extract_export_name(&func.attrs),
            allocator_tag: Self::extract_allocator_tag(&func.attrs),
        })
    }

    /// Resolve a test declaration to a `TirFunction` and `TirTest`
    pub(super) fn resolve_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
        module_is_todo: bool,
    ) -> Option<(TirFunction, TirTest)> {
        let expect_trap = test_decl.attributes.iter().any(|a| a.name == "expect_trap");
        let is_todo = module_is_todo || test_decl.attributes.iter().any(|a| a.name == "TODO");
        let timeout_ms = test_decl.attributes.iter().find_map(|a| {
            if a.name == "timeout_ms" {
                a.args
                    .first()
                    .and_then(|arg| arg.as_str().parse::<u64>().ok())
            } else {
                None
            }
        });

        // Generate function name: __test_{index} or __test_{name_snake_case}
        // For expect_trap tests: __test_trap_{index} or __test_trap_{index}_{name_snake_case}
        // For TODO tests:        __test_todo_{index} or __test_todo_{index}_{name_snake_case}
        // For custom timeout:    __test_tm{ms}_{index} or __test_trap_tm{ms}_{index}_{name}
        let prefix = match (is_todo, expect_trap, timeout_ms) {
            (true, _, Some(ms)) => format!("__test_todo_tm{ms}"),
            (true, _, None) => "__test_todo".to_string(),
            (_, true, Some(ms)) => format!("__test_trap_tm{ms}"),
            (_, true, None) => "__test_trap".to_string(),
            (_, _, Some(ms)) => format!("__test_tm{ms}"),
            (_, _, None) => "__test".to_string(),
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
            module_source: ModuleSource::default(),
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
            stores: vec![],
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: IndexSet::default(),
            is_cm_binding: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: crate::tir::InlineHint::Auto,
            comp_features: 0,
            export_name: None,
            allocator_tag: None,
        };

        let tir_test = TirTest {
            name: test_decl.name.clone(),
            function_name,
            line: test_decl.span.line,
            span: test_decl.span,
            expect_trap,
            is_todo,
            timeout_ms,
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
        trait_type: Option<&Type>,
    ) -> Option<TirFunction> {
        // Use an inherited scope so the caller's `assoc_type_bindings` (set up
        // for the surrounding impl block) remain visible — `Self::Output` etc.
        // must still resolve while we're inside this method body. Type params
        // and bounds get rebuilt below to match the original `mem::take`
        // behavior.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        let mut type_param_list = Vec::new();

        // First, collect type params from impl block's generic type (e.g., impl Box<T>)
        // Also build impl_type_params for the TirFunction
        // For ref-type impls (e.g., impl Trait for &Container<T>), unwrap the reference first.
        let mut impl_type_params = Vec::new();
        let impl_type_inner = match impl_type {
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => inner.as_ref(),
            other => other,
        };
        if let ast::Type::Generic(generic) = impl_type_inner {
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
                        // Store impl type param info for later monomorphization
                        impl_type_params.push(crate::tir::TirTypeParam {
                            name: name.clone(),
                            is_effect: false,
                            is_pack: false,
                            bounds: vec![],
                            default: None, // Impl type params don't have defaults
                            index: i as u32,
                        });
                    }
                }
            }
        } else if let ast::Type::Named(named) = impl_type {
            // Blanket impl case: `impl<I: Iterator> IntoIterator for I`
            // The impl type is a type parameter itself, registered by the caller,
            // now living in the saved (parent) scope.
            if let Some(&(idx, _)) = scope.saved().type_params.get(&named.name) {
                let type_id = scope
                    .type_table
                    .borrow_mut()
                    .make_type_param(named.name.clone(), idx);
                scope
                    .trait_ctx
                    .type_params
                    .insert(named.name.clone(), (idx, type_id));
                let bounds = scope
                    .saved()
                    .type_param_bounds
                    .get(&named.name)
                    .map(|bs| bs.iter().map(|b| b.name.clone()).collect())
                    .unwrap_or_default();
                impl_type_params.push(crate::tir::TirTypeParam {
                    name: named.name.clone(),
                    is_effect: false,
                    is_pack: false,
                    bounds,
                    default: None,
                    index: idx,
                });
            }
        } else if let ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) = impl_type {
            // Reference impl case: `impl<T: Bound> Trait for &T` / `impl<T: Bound> Trait for &mut T`
            // The inner type T is a type parameter registered by the caller.
            if let ast::Type::Named(named) = boxed.as_ref()
                && let Some(&(idx, _)) = scope.saved().type_params.get(&named.name)
            {
                let type_id = scope
                    .type_table
                    .borrow_mut()
                    .make_type_param(named.name.clone(), idx);
                scope
                    .trait_ctx
                    .type_params
                    .insert(named.name.clone(), (idx, type_id));
                let bounds = scope
                    .saved()
                    .type_param_bounds
                    .get(&named.name)
                    .map(|bs| bs.iter().map(|b| b.name.clone()).collect())
                    .unwrap_or_default();
                impl_type_params.push(crate::tir::TirTypeParam {
                    name: named.name.clone(),
                    is_effect: false,
                    is_pack: false,
                    bounds,
                    default: None,
                    index: idx,
                });
            }
        } else if let ast::Type::Tuple(elements) = impl_type {
            // Variadic tuple impl: `impl<..T: Trait> Trait for [..T]`
            // Extract type pack params from the tuple's TypePackSpread elements.
            for elem in elements {
                if let ast::Type::TypePackSpread(name, _) = elem
                    && let Some(&(idx, _)) = scope.saved().type_params.get(name)
                {
                    let type_id = scope
                        .type_table
                        .borrow_mut()
                        .make_type_pack(name.clone(), idx);
                    scope
                        .trait_ctx
                        .type_params
                        .insert(name.clone(), (idx, type_id));
                    let bounds = scope
                        .saved()
                        .type_param_bounds
                        .get(name)
                        .map(|bs| bs.iter().map(|b| b.name.clone()).collect())
                        .unwrap_or_default();
                    impl_type_params.push(crate::tir::TirTypeParam {
                        name: name.clone(),
                        is_effect: false,
                        is_pack: true,
                        bounds,
                        default: None,
                        index: idx,
                    });
                }
            }
        }

        // Populate bounds from the impl block's type_params
        // (inherited from outer scope - second-pass sets these up).
        // The caller sets up bounds BEFORE calling resolve_method, so the saved
        // scope contains the caller's bounds. We start from those and add
        // method-level bounds on top.
        let saved_bounds = scope.saved().type_param_bounds.clone();
        scope.trait_ctx.type_param_bounds = saved_bounds;

        // Bind the trait's own type parameters to the impl's concrete trait
        // args so that references like `T` inside a default method body resolve
        // to the impl's instantiation (e.g., `impl Maker<i32> for IntMaker`
        // binds the trait's `T` to `i32`). Impl type params were registered
        // above, so `Maker<Container<U>>` in `impl<U> Maker<Container<U>> for
        // Foo<U>` resolves correctly.
        if let Some(trait_t) = trait_type {
            scope.bind_trait_type_params_from_impl(trait_t);
        }

        // Set effect params in scope (for resolving effect names in function types)
        let old_effect_params = std::mem::take(&mut scope.current_effect_params);
        let old_effect_param_decls = std::mem::take(&mut scope.current_effect_param_decls);
        let effect_params: Vec<_> = func.type_params.iter().filter(|p| p.is_effect).collect();
        if effect_params.len() > 1 {
            let _ = scope.logger.error(TypeError::InvalidLiteral {
                message: "multiple effect parameters are not allowed; use a single effect parameter instead".to_string(),
                span: effect_params[1].span,
            });
        }
        scope.current_effect_params = effect_params.iter().map(|p| p.name.clone()).collect();
        scope.current_effect_param_decls = effect_params
            .iter()
            .map(|p| (p.name.clone(), p.id))
            .collect();

        // Then, collect method-level type params
        let offset = scope.trait_ctx.type_params.len();
        for (index, param) in func.type_params.iter().enumerate() {
            let idx = (offset + index) as u32;
            let type_id = scope
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), idx);
            scope
                .trait_ctx
                .type_params
                .insert(param.name.clone(), (idx, type_id));
            type_param_list.push((param.name.clone(), type_id));
            if !param.bounds.is_empty() {
                scope
                    .trait_ctx
                    .type_param_bounds
                    .insert(param.name.clone(), param.bounds.clone());
            }
        }

        // Set up Self type for the impl block
        // This allows `&Self` to resolve correctly in method parameters
        let old_self_type = scope.trait_ctx.self_type;
        scope.trait_ctx.self_type = Some(scope.resolve_type(impl_type));

        // Resolve return type
        let return_type = func
            .return_type
            .as_ref()
            .map(|t| scope.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        // Update the function_return_types with the resolved return type
        // (This replaces the potentially incorrect type from static resolution)
        let mangled_name = MethodName::format_local(struct_name, trait_name, &func.name);
        scope
            .function_return_types
            .insert(mangled_name.clone(), return_type);

        // Display name for #function: StructName::method_name
        let display_name = MethodName::format_local(struct_name, None, &func.name);
        let mut ctx = FunctionContext::new(return_type, display_name);

        // Resolve parameters (including &self). Defaults are resolved in the
        // method's lexical scope with earlier parameters already bound.
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = match param.self_kind {
                ast::SelfKind::Ref => {
                    // &self: wrap impl type in immutable reference
                    let inner_type = scope.resolve_type(impl_type);
                    scope.type_table.borrow_mut().make_ref(inner_type)
                }
                ast::SelfKind::MutRef => {
                    // &mut self: wrap impl type in mutable reference
                    let inner_type = scope.resolve_type(impl_type);
                    scope.type_table.borrow_mut().make_mut_ref(inner_type)
                }
                ast::SelfKind::None => {
                    // Regular parameter
                    scope.resolve_type(&param.ty)
                }
            };
            // Reject parameter defaults on trait-impl methods: defaults live
            // on the trait declaration only (WEP 2026-04-11).
            if trait_name.is_some()
                && let Some(default_ast) = &param.default
            {
                let _ = scope.logger.error(TypeError::DefaultInTraitImpl {
                    method: func.name.clone(),
                    param: param.name.clone(),
                    span: default_ast.span(),
                });
            }
            let default_expr = param.default.as_ref().map(|default_ast| {
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(type_id));
                scope.typecheck(resolved.type_id, type_id, default_ast.span());
                Box::new(resolved)
            });
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
            scope.record_local_symbol(param.id, &param.name, param.name_span, param.is_mut);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                default_expr,
                span: param.span,
            });
        }

        // Validate stores declarations
        scope.validate_stores(&func.stores, &params, func.span);

        // Resolve body
        let body = func
            .body
            .as_ref()
            .map(|b| scope.resolve_block(b, &mut ctx, None));

        // Validate: non-unit return type requires explicit `return` in the body
        if return_type != TypeTable::UNIT
            && return_type != TypeTable::NEVER
            && let Some(ref body) = body
            && Self::find_return_type_in_block(body).is_none()
        {
            let _ = scope.logger.error(TypeError::MissingReturn {
                return_type: scope.type_table.borrow().type_name(return_type),
                span: func.span,
            });
        }

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Store resolved param types for generic methods (before restoring type params scope)
        // so TypeParams have the correct ids for later inference at call sites.
        let method_resolved_param_types: Vec<TypeId> = if func.type_params.is_empty() {
            vec![]
        } else {
            func.params
                .iter()
                .filter(|p| p.self_kind == SelfKind::None)
                .map(|p| scope.resolve_type(&p.ty))
                .collect()
        };

        // Resolve effects while effect params are still in scope
        let effects = scope.resolve_effects(&func.effects, &func.effect_ids);

        // Restore effect params and Self type. `trait_ctx` is auto-restored on
        // `drop(scope)`, which replaces everything set up above.
        scope.current_effect_params = old_effect_params;
        scope.current_effect_param_decls = old_effect_param_decls;
        scope.trait_ctx.self_type = old_self_type;
        drop(scope);

        // Store type parameters for generic methods (for call site substitution)
        if !func.type_params.is_empty() {
            self.generic_method_params
                .insert(mangled_name.clone(), type_param_list);
            self.generic_method_resolved_param_types
                .insert(mangled_name, method_resolved_param_types);
        }

        Some(TirFunction {
            module_source: ModuleSource::default(),
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
                is_ref_impl: false,
                cm_name: None,
            }),
            params,
            return_type,
            effects,
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: IndexSet::default(),
            is_cm_binding: false,
            is_cm_export: false,
            is_ambient: Self::extract_is_ambient(&func.attrs),
            inline_hint: Self::extract_inline_hint(&func.attrs),
            comp_features: extract_comp_features(&func.attrs),
            export_name: None,
            allocator_tag: None,
        })
    }
}
