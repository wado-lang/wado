use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    CallArg, ClosureFunctor, FunctionKind, FunctionRef, InlineHint, ResolvedType, TirBlock,
    TirCapture, TirExpr, TirExprKind, TirField, TirFunction, TirImpl, TirLocal, TirModule,
    TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};
use crate::token::Span;

/// Information about a closure collected during the first pass
#[derive(Debug, Clone)]
struct CollectedClosure {
    /// Unique closure ID (assigned in order of collection)
    id: u32,
    /// Parameters of the closure
    params: Vec<(String, TypeId)>,
    /// The closure body expression (cloned for __call method generation)
    body: TirExpr,
    /// Captures from the closure
    captures: Vec<TirCapture>,
    /// Return type of the closure
    return_type: TypeId,
    /// Original function type (for compatibility)
    func_type_id: TypeId,
    /// Span of the original closure
    span: Span,
}

// FunctorInfo moved to tir::ClosureFunctor

/// Function signature info for converting `FuncRef` to Closure.
struct FuncSig {
    params: Vec<(String, TypeId)>,
    return_type: TypeId,
}

/// Key for fn-param specialization: (callee function name, parameter index -> functor type ID)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FnParamSpecKey {
    /// Name of the callee function
    callee_name: String,
    /// Map from parameter index to functor struct type ID (for fn-type params with closure args)
    functor_types: Vec<(u32, TypeId)>,
}

/// Lowers closures to functor structs with `__call` methods.
///
/// For each closure, this generates:
/// 1. A synthetic struct `__Closure_N` with fields for captured variables
/// 2. A `__call` method containing the transformed closure body
///
/// Transformations (selective - only for closures stored in locals and called directly):
/// - `Closure { params, body, captures }` → `StructLiteral { __Closure_N, capture_values }`
/// - `Capture { index }` (in body) → `FieldAccess { self, __capture_{index} }`
/// - `IndirectCall { callee, args }` (known closure) → `MethodCall { callee, __call, args }`
///
/// Closures passed as function arguments are transformed via fn-param specialization:
/// - A specialized version of the callee is generated with functor struct params
/// - The call is updated to use the specialized function with `StructLiteral` args
pub(super) struct ClosureLowerer {
    /// Sequential ID assigned to each `Closure` node during the pre-pass.
    /// Each Closure's `functor_id` is set once and read by all later passes;
    /// no counter is re-walked in lockstep with the AST.
    next_closure_id: u32,
    /// Module source for generated items
    module_source: ModuleSource,
    /// Collected closures during first pass
    collected_closures: Vec<CollectedClosure>,
    /// Generated functor information (populated after struct/method generation)
    /// These will be stored in `module.closure_functors` for the optimizer
    functor_infos: Vec<ClosureFunctor>,
    /// Map from local variable index to closure ID (for tracking closures stored in locals)
    local_to_closure: IndexMap<u32, u32>,
    /// Closure IDs that can be specialized (stored in locals, called directly).
    /// Non-specializable closures use `ClosureToCanonical` for type-erased representation.
    specializable: IndexSet<u32>,
    /// Generated structs to add to module
    generated_structs: Vec<TirStruct>,
    /// Generated functions to add to module
    generated_functions: Vec<Rc<RefCell<TirFunction>>>,
    /// Map from fn-param spec key to specialized function name
    fn_param_specializations: IndexMap<FnParamSpecKey, String>,
}

impl ClosureLowerer {
    pub(super) fn new(module_source: &ModuleSource) -> Self {
        Self {
            next_closure_id: 0,
            module_source: module_source.clone(),
            collected_closures: Vec::new(),
            functor_infos: Vec::new(),
            local_to_closure: IndexMap::default(),
            specializable: IndexSet::default(),
            generated_structs: Vec::new(),
            generated_functions: Vec::new(),
            fn_param_specializations: IndexMap::default(),
        }
    }

    /// Lower all closures in a module
    pub(super) fn lower_module(&mut self, module: &mut TirModule) {
        // Phase 0: Convert FuncRef used as values to zero-capture Closures.
        // Named functions used as values (e.g., `&double` or `double` passed to fn-type params)
        // need to become Closure nodes so the existing closure pipeline handles them.
        self.convert_func_refs_to_closures(module);

        // Phase 1: collect all closures and assign each a stable `functor_id`.
        //
        // The ID is written onto the `Closure` expression node itself, so every
        // later pass reads it directly off the AST. This avoids fragile
        // counter walks that have to re-traverse the module in lockstep with
        // the original collection order — counters break the moment we
        // introduce additional walks (e.g. the generated `__call` methods) or
        // skip a sub-tree.
        self.next_closure_id = 0;
        self.collected_closures.clear();

        let func_refs: Vec<_> = module.functions.clone();
        let mut collector = CollectClosuresVisitor {
            next_closure_id: &mut self.next_closure_id,
            collected_closures: &mut self.collected_closures,
        };
        for func_rc in &func_refs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                collector.visit_block(body);
            }
        }

        // Also collect from impl methods
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    collector.visit_block(body);
                }
            }
        }

        // Generate functor structs and __call methods. Each `__call` body is
        // a clone of the closure body taken AFTER `collect_closures_in_block`
        // assigned IDs to nested closures, so those clones carry the same
        // stable functor IDs as the originals.
        self.generate_functor_items(&mut module.type_table.borrow_mut());

        // Phase 2: analyse which closures are safe to specialise. Reads
        // `functor_id` directly off the Closure node — no counter.
        for func_rc in &func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.local_to_closure.clear();
                ClosureSafetyAnalyzer {
                    local_to_closure: &mut self.local_to_closure,
                    specializable: &mut self.specializable,
                    in_arg_position: false,
                }
                .visit_block(body);
            }
        }
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.local_to_closure.clear();
                    ClosureSafetyAnalyzer {
                        local_to_closure: &mut self.local_to_closure,
                        specializable: &mut self.specializable,
                        in_arg_position: false,
                    }
                    .visit_block(body);
                }
            }
        }

        // Phase 2.5: Generate specialized functions for fn-param monomorphization.
        // For closures passed as fn-type arguments, generate specialized callees.
        self.generate_fn_param_specializations(
            &func_refs,
            &module.impls,
            &mut module.type_table.borrow_mut(),
        );

        // Phase 3: transform closures to struct literals and IndirectCall to MethodCall.
        //
        // We walk the generated `__call` methods alongside the original
        // module functions: a nested closure's body lives in its parent
        // closure's __call method (cloned in by `transform_closure_body`),
        // and that copy must be lowered too. Each function uses a fresh
        // `local_to_closure` map (keyed by per-function local indices).
        // `transform_expr` deliberately does NOT recurse into Closure
        // bodies — those are visited via the corresponding __call method.
        let lowered_funcs: Vec<_> = func_refs
            .iter()
            .cloned()
            .chain(self.generated_functions.iter().cloned())
            .collect();
        for func_rc in &lowered_funcs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                self.local_to_closure.clear();
                let mut tt = module.type_table.borrow_mut();
                Phase3Transformer {
                    local_to_closure: &mut self.local_to_closure,
                    specializable: &self.specializable,
                    functor_infos: &self.functor_infos,
                    fn_param_specializations: &self.fn_param_specializations,
                    module_source: &self.module_source,
                    type_table: &mut tt,
                }
                .visit_block(body);
            }
            self.update_local_types(&mut func);
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    self.local_to_closure.clear();
                    let mut tt = module.type_table.borrow_mut();
                    Phase3Transformer {
                        local_to_closure: &mut self.local_to_closure,
                        specializable: &self.specializable,
                        functor_infos: &self.functor_infos,
                        fn_param_specializations: &self.fn_param_specializations,
                        module_source: &self.module_source,
                        type_table: &mut tt,
                    }
                    .visit_block(body);
                }
                self.update_local_types(method);
            }
        }

        // Phase 4: transform any remaining Closure nodes to ClosureToCanonical.
        // These are closures that weren't specialised (fn-param stored in
        // struct field). Walks the same set as phase 3 so cloned bodies in
        // `__call` methods are also covered.
        let mut remaining = RemainingClosuresRewriter {
            functor_infos: &self.functor_infos,
            module_source: &self.module_source,
        };
        for func_rc in &lowered_funcs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                remaining.visit_block(body);
            }
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    remaining.visit_block(body);
                }
            }
        }

        // Store functor metadata in module for the optimizer to use.
        // This enables closure inlining by providing the __call method body.
        module.closure_functors = std::mem::take(&mut self.functor_infos);

        // Add ALL generated structs and functions to the module
        module
            .structs
            .extend(std::mem::take(&mut self.generated_structs));
        module
            .functions
            .extend(std::mem::take(&mut self.generated_functions));
    }

    /// Convert `FuncRef` nodes (used as values, not in Call/MethodCall func positions) to
    /// zero-capture `Closure` nodes. This enables named functions to be passed as function-type
    /// arguments (e.g., `apply(double, 21)` or `apply(&double, 21)`).
    fn convert_func_refs_to_closures(&self, module: &mut TirModule) {
        // Build a map from function name to (param_types, return_type)
        let mut func_sigs: IndexMap<String, FuncSig> = IndexMap::default();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            func_sigs.insert(
                func.name.clone(),
                FuncSig {
                    params: func
                        .params
                        .iter()
                        .map(|p| (p.name.clone(), p.type_id))
                        .collect(),
                    return_type: func.return_type,
                },
            );
        }
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                func_sigs.insert(
                    method.name.clone(),
                    FuncSig {
                        params: method
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), p.type_id))
                            .collect(),
                        return_type: method.return_type,
                    },
                );
            }
        }

        {
            let mut type_table = module.type_table.borrow_mut();
            let mut rewriter = FuncRefToClosureRewriter {
                func_sigs: &func_sigs,
                type_table: &mut type_table,
            };
            for func_rc in &module.functions {
                let mut func = func_rc.borrow_mut();
                if let Some(body) = &mut func.body {
                    rewriter.visit_block(body);
                }
            }
        }
        for impl_block in &mut module.impls {
            let mut type_table = module.type_table.borrow_mut();
            let mut rewriter = FuncRefToClosureRewriter {
                func_sigs: &func_sigs,
                type_table: &mut type_table,
            };
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    rewriter.visit_block(body);
                }
            }
        }
    }

    /// Update `locals` in a function after closure transformation
    fn update_local_types(&self, func: &mut TirFunction) {
        // For each local that stored a closure and was transformed to a struct,
        // update its type from function type to struct type
        for (local_idx, closure_id) in &self.local_to_closure {
            if self.specializable.contains(closure_id)
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
                && let Some(local) = func.locals.get_mut(*local_idx as usize)
            {
                // Functors are reference types
                local.type_id = functor.ref_type_id;
            }
        }
    }

    fn generate_functor_items(&mut self, type_table: &mut TypeTable) {
        // The collect pass pushes nested closures before their parents (it
        // recurses into the body before pushing the outer, so the cloned
        // body carries the nested IDs). Sort by id so `functor_infos[id]`
        // is the functor for the closure with that id — every later pass
        // uses index-by-id lookups.
        //
        // Move out of `self.collected_closures` to avoid cloning the bodies:
        // the loop body needs `&mut self` to push to `generated_structs` and
        // `functor_infos`, but the borrow checker would otherwise see one
        // long borrow over `collected_closures`. The list isn't read again
        // after this pass, so we drop it at the end.
        let mut collected_closures = std::mem::take(&mut self.collected_closures);
        collected_closures.sort_by_key(|c| c.id);
        for collected in &collected_closures {
            // Extract the actual return type from the closure's function type
            // This is more reliable than body.type_id for closures with block bodies
            let return_type = match type_table.get(collected.func_type_id) {
                ResolvedType::Function { return_type, .. } => *return_type,
                _ => collected.return_type, // Fallback to body type
            };

            // Generate struct name and type
            let struct_name = format!("__Closure_{}", collected.id);
            let struct_type_id =
                type_table.make_struct(struct_name.clone(), self.module_source.clone());

            // Generate struct definition with capture fields
            let fields: Vec<TirField> = collected
                .captures
                .iter()
                .enumerate()
                .map(|(i, cap)| TirField {
                    name: format!("__capture_{i}"),
                    is_pub: false,
                    type_id: cap.type_id,
                    index: i as u32,
                    span: collected.span,
                    is_hidden: false,
                    serde_rename: None,
                    serde_default: false,
                    default_expr: None,
                })
                .collect();

            let tir_struct = TirStruct {
                name: struct_name.clone(),
                module_source: self.module_source.clone(),
                is_pub: false,
                type_params: Vec::new(),
                monomorph_info: None,
                fields,
                span: collected.span,
                serde_rename_all: None,
            };
            self.generated_structs.push(tir_struct);

            // Generate __call method
            // Use a qualified name for the function to avoid collisions in the inliner's candidate map
            let simple_method_name = "__call".to_string();
            let qualified_method_name = MethodName::format_local(&struct_name, None, "__call");
            let self_ref_type = type_table.make_ref(struct_type_id);

            // Parameters: self + closure params
            let mut params = Vec::new();
            params.push(TirParam {
                name: "self".to_string(),
                type_id: self_ref_type,
                local_index: 0,
                is_mut: false,
                span: collected.span,
                default_expr: None,
            });

            for (i, (name, type_id)) in collected.params.iter().enumerate() {
                params.push(TirParam {
                    name: name.clone(),
                    type_id: *type_id,
                    local_index: (i + 1) as u32,
                    is_mut: false,
                    span: collected.span,
                    default_expr: None,
                });
            }

            // Transform the body: Capture { index } -> FieldAccess { self, __capture_{index} }
            // and shift every Local / Let / Binding index by 1 to make room for `self`.
            let mut transformed_body = collected.body.clone();
            ClosureBodyTransformer {
                captures: &collected.captures,
                self_ref_type,
                self_span: collected.span,
            }
            .visit_expr(&mut transformed_body);

            // Handle body wrapping based on body type
            // For block bodies, extract statements directly to preserve Return handling during inlining
            // For expression bodies, wrap in Return
            let body_stmts = match &transformed_body.kind {
                TirExprKind::Block(block) => {
                    // Block body: use statements directly (they already contain Return statements)
                    // This is important for inlining: the inliner's remap_stmt_with_label
                    // converts Return to Break, but only at the statement level, not inside
                    // Block expressions.
                    block.stmts.clone()
                }
                _ => {
                    if return_type == TypeTable::UNIT {
                        // Unit return: just evaluate the expression for side effects
                        vec![TirStmt::new(
                            TirStmtKind::Expr(transformed_body),
                            collected.span,
                        )]
                    } else {
                        // Expression body that returns a value
                        vec![TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(transformed_body),
                            },
                            collected.span,
                        )]
                    }
                }
            };

            let body_block = TirBlock::new(body_stmts, collected.span);

            // Collect locals: self + params + internal locals from body.
            // Parameters are locals 0 (self) through params.len(); the
            // closure-functor's `__call` method receives the env struct as
            // self, so the first slot is the synthesised self ref.
            let param_count = 1 + collected.params.len() as u32;
            let mut locals: Vec<TirLocal> = Vec::with_capacity(param_count as usize);
            locals.push(TirLocal {
                name: "self".to_string(),
                type_id: self_ref_type,
                is_mut: false,
            });
            for (name, ty) in &collected.params {
                locals.push(TirLocal {
                    name: name.clone(),
                    type_id: *ty,
                    is_mut: false,
                });
            }

            // Collect internal locals from the body (Let statements with index >= param_count)
            let mut body_locals: Vec<(u32, TypeId)> = Vec::new();
            LocalCollector {
                locals: &mut body_locals,
            }
            .visit_block(&body_block);

            // Extend `locals` with body locals, sorted by index. Body locals
            // come from the closure body's `Let` statements; the source
            // names are recovered later in `wir_build` via
            // `TirFunction::locals[idx].name`, so here we only need stable
            // synthetic placeholders.
            body_locals.sort_by_key(|(idx, _)| *idx);
            for (idx, type_id) in &body_locals {
                // Ensure we only add locals beyond parameter range
                if *idx >= param_count {
                    // Extend vector if needed to accommodate sparse indices
                    while locals.len() <= *idx as usize {
                        let placeholder_idx = locals.len() as u32;
                        locals.push(TirLocal::synth(placeholder_idx, TypeTable::UNKNOWN, false));
                    }
                    locals[*idx as usize] = TirLocal::synth(*idx, *type_id, false);
                }
            }

            // local_count is the total number of locals
            let local_count = locals.len() as u32;

            // method_info tells codegen how to register this function with the proper mangled name
            let method_info = LocalMethodName::new(
                struct_name.clone(),        // __Closure_0
                None,                       // no trait
                simple_method_name.clone(), // __call (just the method name)
            );

            let call_method = TirFunction {
                module_source: self.module_source.clone(),
                is_async: false,
                name: qualified_method_name,
                is_pub: false,
                is_export: false, // Closure method, not a world export
                type_params: Vec::new(),
                impl_type_params: Vec::new(),
                monomorph_info: None,
                method_info: Some(method_info),
                params,
                return_type,
                task_return_type: None,
                effects: Vec::new(),
                stores: vec![],
                body: Some(body_block),
                span: collected.span,
                local_count,
                locals,
                address_taken_locals: IndexSet::default(),
                stores_aliased_locals: IndexSet::default(),
                is_cm_binding: false,
                is_dispatch_wrapper: false,
                is_cm_export: false,
                is_ambient: false,
                inline_hint: InlineHint::Auto,
                comp_features: 0,
                export_name: None,
                allocator_tag: None,
                kind: FunctionKind::Regular,
            };

            let call_method_rc = Rc::new(RefCell::new(call_method));
            self.generated_functions.push(Rc::clone(&call_method_rc));

            self.functor_infos.push(ClosureFunctor {
                module_source: self.module_source.clone(),
                id: collected.id,
                struct_name,
                struct_type_id,
                ref_type_id: self_ref_type,
                call_method: call_method_rc,
                captures: collected.captures.clone(),
            });
        }
    }

    /// Generate specialized functions for calls with closure arguments to fn-type parameters.
    ///
    /// This implements WEP Phase 3: when a function takes `fn(A) -> B` and is called with
    /// a closure, we generate a specialized version where:
    /// 1. The fn-type parameter becomes the functor struct type
    /// 2. `IndirectCall` on that parameter becomes `MethodCall` on __call
    fn generate_fn_param_specializations(
        &mut self,
        func_refs: &[Rc<RefCell<TirFunction>>],
        impls: &[TirImpl],
        type_table: &mut TypeTable,
    ) {
        // Build a map from function name to function for quick lookup
        let mut func_by_name: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
        for func_rc in func_refs {
            let func = func_rc.borrow();
            func_by_name.insert(func.name.clone(), Rc::clone(func_rc));
        }
        for impl_block in impls {
            for method in &impl_block.methods {
                let name = method.name.clone();
                // We can't get an Rc from a TirFunction reference directly
                // For impl methods, we'll handle them separately
                // For now, just process top-level functions
                drop(name);
            }
        }

        // Collect specialization requests by scanning all function bodies.
        // Each Closure node already carries its `functor_id` from the
        // collect pass, so we don't need a separate counter.
        let mut spec_requests: Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)> = Vec::new();

        let mut collector = FnParamSpecCollector {
            func_by_name: &func_by_name,
            type_table,
            functor_infos: &self.functor_infos,
            requests: &mut spec_requests,
        };
        for func_rc in func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collector.visit_block(body);
            }
        }
        for impl_block in impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    collector.visit_block(body);
                }
            }
        }

        // Generate specialized functions for each unique key
        for (key, callee_rc) in spec_requests {
            if self.fn_param_specializations.contains_key(&key) {
                continue; // Already generated
            }

            // Skip specialization if any fn-param is stored in a struct field
            // This would cause type mismatches since struct fields expect fn(...) not &__Closure_N
            let callee = callee_rc.borrow();
            // Check if this is an instance method (has self parameter)
            // Note: static methods have method_info but no self parameter
            let has_self_param = callee.params.first().is_some_and(|p| p.name == "self");
            let param_offset = u32::from(has_self_param);
            let fn_param_indices: Vec<u32> = key
                .functor_types
                .iter()
                .map(|(arg_idx, _)| arg_idx + param_offset)
                .collect();

            if let Some(body) = &callee.body {
                let mut check = StructFieldFnParamCheck {
                    fn_param_indices: &fn_param_indices,
                    found: false,
                };
                check.visit_block(body);
                if check.found {
                    // Skip this specialization - the closure is stored in
                    // a struct field, which expects fn(...) not &__Closure_N.
                    continue;
                }
            }
            drop(callee);

            let specialized_name = self.generate_specialized_function(&key, &callee_rc, type_table);
            self.fn_param_specializations.insert(key, specialized_name);
        }
    }

    /// Generate a specialized function where fn-type params become functor struct types
    fn generate_specialized_function(
        &mut self,
        key: &FnParamSpecKey,
        callee_rc: &Rc<RefCell<TirFunction>>,
        type_table: &mut TypeTable,
    ) -> String {
        let callee = callee_rc.borrow();

        // Build specialized name: callee$__Closure_0$__Closure_1...
        let functor_suffix: String =
            key.functor_types
                .iter()
                .fold(String::new(), |mut acc, (_, tid)| {
                    let name = type_table.type_name(*tid);
                    let _ = write!(acc, "${name}");
                    acc
                });
        let specialized_name = format!("{}{}", callee.name, functor_suffix);

        // Build map from argument index to functor type
        // Note: key.functor_types contains argument indices (0 = first arg after receiver for methods)
        let arg_to_functor: IndexMap<u32, TypeId> = key.functor_types.iter().copied().collect();

        // Determine if this is an instance method (has self parameter)
        // Note: static methods have method_info but no self parameter
        let has_self_param = callee.params.first().is_some_and(|p| p.name == "self");
        let param_offset = u32::from(has_self_param);

        // Clone and modify params
        // For methods: params[0] is self, so argument index i maps to params[i + 1]
        let mut new_params = callee.params.clone();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let param_idx = (*arg_idx + param_offset) as usize;
            if param_idx < new_params.len() {
                new_params[param_idx].type_id = type_table.make_ref(functor_type);
            }
        }

        // Clone and modify locals (same indexing as params)
        let mut new_locals = callee.locals.clone();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let local_idx = (*arg_idx + param_offset) as usize;
            if local_idx < new_locals.len() {
                new_locals[local_idx].type_id = type_table.make_ref(functor_type);
            }
        }

        // Build a map from param/local index to functor type for body transformation
        // Inside the function body, locals are referenced by param index
        let local_to_functor: IndexMap<u32, TypeId> = arg_to_functor
            .iter()
            .map(|(arg_idx, functor_type)| (arg_idx + param_offset, *functor_type))
            .collect();

        // Clone body and transform IndirectCall to MethodCall for fn-param locals.
        let new_body = callee.body.as_ref().map(|body| {
            let mut cloned = body.clone();
            SpecializerTransformer {
                param_to_functor: &local_to_functor,
                functor_infos: &self.functor_infos,
                module_source: &self.module_source,
                type_table,
            }
            .visit_block(&mut cloned);
            cloned
        });

        // Build specialized method name: method<TypeArgs>$__Closure_0
        // The functor suffix goes AFTER type args, so we use full_method_name()
        // and then clear method_type_args to avoid duplication
        let specialized_method_name = if let Some(ref info) = callee.method_info {
            format!("{}{}", info.full_method_name(), functor_suffix)
        } else {
            // Should not happen for method calls
            format!("{}{}", callee.name, functor_suffix)
        };

        // Update method_info with the specialized method name
        // Note: method_type_args is empty because they're now part of method_name
        let specialized_method_info = callee.method_info.as_ref().map(|info| {
            LocalMethodName {
                struct_name: info.struct_name.clone(),
                base_struct_name: info.base_struct_name.clone(),
                trait_name: info.trait_name.clone(),
                base_trait_name: info.base_trait_name.clone(),
                trait_type_args: info.trait_type_args.clone(),
                method_name: specialized_method_name.clone(),
                method_type_args: Vec::new(), // Type args are now in method_name
                is_type_param_receiver: info.is_type_param_receiver,
                is_ref_impl: false,
                cm_name: info.cm_name.clone(),
            }
        });

        let specialized_func = TirFunction {
            module_source: self.module_source.clone(),
            is_async: false,
            name: specialized_name.clone(),
            is_pub: false,    // Specialized functions are always private
            is_export: false, // Specialized functions are not world exports
            type_params: callee.type_params.clone(),
            impl_type_params: callee.impl_type_params.clone(),
            monomorph_info: callee.monomorph_info.clone(),
            method_info: specialized_method_info,
            params: new_params,
            return_type: callee.return_type,
            task_return_type: None,
            effects: callee.effects.clone(),
            stores: callee.stores.clone(),
            body: new_body,
            span: callee.span,
            local_count: callee.local_count,
            locals: new_locals,
            address_taken_locals: callee.address_taken_locals.clone(),
            stores_aliased_locals: callee.stores_aliased_locals.clone(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: callee.inline_hint,
            comp_features: callee.comp_features,
            export_name: callee.export_name.clone(),
            allocator_tag: callee.allocator_tag.clone(),
            kind: FunctionKind::Regular,
        };

        self.generated_functions
            .push(Rc::new(RefCell::new(specialized_func)));
        specialized_name
    }
}

/// Phase 4 rewriter: transform remaining `Closure` nodes (those not
/// specialised) into `ClosureToCanonical`.
struct RemainingClosuresRewriter<'a> {
    functor_infos: &'a [ClosureFunctor],
    module_source: &'a ModuleSource,
}

impl TirMutVisitor for RemainingClosuresRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // Pull captures out of the borrow before we mutate `expr.kind`.
        let closure_data = if let TirExprKind::Closure {
            captures,
            functor_id: Some(closure_id),
            ..
        } = &expr.kind
        {
            Some((*closure_id, captures.clone()))
        } else {
            None
        };

        let Some((closure_id, captures)) = closure_data else {
            // For non-closure nodes (and `Closure { functor_id: None, .. }`,
            // which the original code recursed into), use the default walk.
            self.walk_expr(expr);
            return;
        };

        // Closure with functor_id but no functor info: leave untouched and
        // do NOT recurse into the body — the original logic also stopped
        // here (the body lives in the generated `__call` method).
        let Some(functor) = self.functor_infos.get(closure_id as usize) else {
            return;
        };

        let target_fn_type = expr.type_id;
        let span = expr.span;
        let fields: Vec<TirStructField> = captures
            .iter()
            .enumerate()
            .map(|(i, cap)| TirStructField {
                name: format!("__capture_{i}"),
                value: TirExpr::new(
                    TirExprKind::Local {
                        index: cap.outer_index,
                        name: cap.name.clone(),
                    },
                    cap.type_id,
                    span,
                ),
                field_index: i as u32,
            })
            .collect();

        let struct_literal = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: functor.struct_type_id,
                struct_name: functor.struct_name.clone(),
                fields,
            },
            functor.ref_type_id,
            span,
        );

        expr.kind = TirExprKind::ClosureToCanonical {
            functor: Box::new(struct_literal),
            functor_id: closure_id,
            target_fn_type,
            closure_module: self.module_source.clone(),
        };
        // Keep expr.type_id as the original function type for type compatibility.
    }
}

/// Phase 1 visitor: assign a stable `functor_id` to every `Closure` node and
/// record a `CollectedClosure` snapshot for the functor-generation pass.
///
/// IDs are assigned in pre-order on the outer Closure but the snapshot is
/// pushed in post-order (after recursing into the body), so the cloned body
/// already carries the IDs that nested closures got assigned.
struct CollectClosuresVisitor<'a> {
    next_closure_id: &'a mut u32,
    collected_closures: &'a mut Vec<CollectedClosure>,
}

impl TirMutVisitor for CollectClosuresVisitor<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if !matches!(expr.kind, TirExprKind::Closure { .. }) {
            self.walk_expr(expr);
            return;
        }

        let closure_id = *self.next_closure_id;
        *self.next_closure_id += 1;
        let func_type_id = expr.type_id;
        let span = expr.span;

        if let TirExprKind::Closure { functor_id, .. } = &mut expr.kind {
            *functor_id = Some(closure_id);
        }

        // Recurse into the body first so nested closures get IDs assigned
        // before we clone the body for `CollectedClosure`. The clone then
        // carries the same stable IDs as the originals.
        self.walk_expr(expr);

        if let TirExprKind::Closure {
            params,
            body,
            captures,
            ..
        } = &expr.kind
        {
            self.collected_closures.push(CollectedClosure {
                id: closure_id,
                params: params.clone(),
                body: (**body).clone(),
                captures: captures.clone(),
                return_type: body.type_id,
                func_type_id,
                span,
            });
        }
    }
}

/// Phase 0 rewriter: turn `FuncRef` values into zero-capture `Closure`
/// nodes so the rest of the closure pipeline handles them uniformly.
///
/// `&FuncRef` collapses to the converted Closure (function values are GC
/// references, so `&fn(...) = fn(...)`). `FuncRefs` in callee position of
/// `Call` / `MethodCall` aren't `TirExprKind::FuncRef`, so they're left
/// alone by the default walk.
struct FuncRefToClosureRewriter<'a> {
    func_sigs: &'a IndexMap<String, FuncSig>,
    type_table: &'a mut TypeTable,
}

impl TirMutVisitor for FuncRefToClosureRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // `&FuncRef` / `&mut FuncRef` → recurse and collapse the wrapper if
        // the inner became a Closure.
        if let TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } = &mut expr.kind
            && matches!(inner.kind, TirExprKind::FuncRef { .. })
        {
            self.visit_expr(inner);
            if matches!(inner.kind, TirExprKind::Closure { .. }) {
                let inner_owned = std::mem::replace(
                    inner.as_mut(),
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                *expr = inner_owned;
            }
            return;
        }

        // Bare `FuncRef` → zero-capture Closure if the function is known.
        if let TirExprKind::FuncRef {
            name,
            module_source: func_module,
        } = &expr.kind
        {
            let Some(sig) = self.func_sigs.get(name.as_str()) else {
                return;
            };
            let span = expr.span;
            let func_name = name.clone();
            let func_module = func_module.clone();

            let closure_params: Vec<(String, TypeId)> = sig
                .params
                .iter()
                .enumerate()
                .map(|(i, (_, ty))| (format!("__fn_ref_p{i}"), *ty))
                .collect();

            let call_args: Vec<CallArg> = closure_params
                .iter()
                .enumerate()
                .map(|(i, (name, ty))| {
                    CallArg::new(
                        TirExpr::new(
                            TirExprKind::Local {
                                index: i as u32,
                                name: name.clone(),
                            },
                            *ty,
                            span,
                        ),
                        false,
                    )
                })
                .collect();

            let body = TirExpr::new(
                TirExprKind::Call {
                    func: FunctionRef {
                        module_source: func_module,
                        name: func_name,
                        monomorph_info: None,
                        method_info: None,
                    },
                    type_args: Vec::new(),
                    args: call_args,
                },
                sig.return_type,
                span,
            );

            let param_types: Vec<TypeId> = closure_params.iter().map(|(_, t)| *t).collect();
            let func_type =
                self.type_table
                    .make_function(param_types, sig.return_type, Vec::new(), Vec::new());

            expr.kind = TirExprKind::Closure {
                params: closure_params,
                body: Box::new(body),
                captures: Vec::new(),
                functor_id: None,
                source_text: None,
                address_taken_locals: IndexSet::default(),
                body_locals: Vec::new(),
            };
            expr.type_id = func_type;
            return;
        }

        self.walk_expr(expr);
    }
}

/// Phase 1.5 helper: collect `(local_index, type_id)` for every `Let`
/// statement in the closure body, including those nested in inner blocks.
struct LocalCollector<'a> {
    locals: &'a mut Vec<(u32, TypeId)>,
}

impl TirRefVisitor for LocalCollector<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index,
            type_id,
            ..
        } = &stmt.kind
        {
            self.locals.push((*local_index, *type_id));
        }
        self.walk_stmt(stmt);
    }
}

/// Phase 2 visitor: decide which closures are safe to specialise. A closure
/// is unsafe if it (or a local that holds it) appears directly in argument
/// position — those are routed through `ClosureToCanonical` instead.
///
/// `in_arg_position` is tracked manually since the visitor traits don't
/// propagate context. The flag is set true only on the direct child of an
/// argument slot in `Call` / `MethodCall` / `IndirectCall` / `CmRawCall`,
/// and reset to false everywhere else (statement boundaries, sub-expr
/// wrappers, closure bodies).
struct ClosureSafetyAnalyzer<'a> {
    local_to_closure: &'a mut IndexMap<u32, u32>,
    specializable: &'a mut IndexSet<u32>,
    in_arg_position: bool,
}

impl TirRefVisitor for ClosureSafetyAnalyzer<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let TirExprKind::Closure {
                functor_id: Some(closure_id),
                ..
            } = &value.kind
        {
            self.local_to_closure.insert(*local_index, *closure_id);
            // Initially mark as safe; will be removed if passed as argument.
            self.specializable.insert(*closure_id);
        }
        let prev = std::mem::replace(&mut self.in_arg_position, false);
        self.walk_stmt(stmt);
        self.in_arg_position = prev;
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Closure {
                body,
                functor_id: Some(closure_id),
                ..
            } => {
                if self.in_arg_position {
                    self.specializable.swap_remove(closure_id);
                }
                // The closure body uses a fresh local-index namespace, so
                // outer-scope `local_to_closure` entries must not leak in.
                // Save / restore across the descent.
                let saved_l2c = std::mem::take(self.local_to_closure);
                let prev_arg = std::mem::replace(&mut self.in_arg_position, false);
                self.visit_expr(body);
                self.in_arg_position = prev_arg;
                *self.local_to_closure = saved_l2c;
            }
            TirExprKind::Closure {
                functor_id: None, ..
            } => {
                unreachable!(
                    "Closure node missing functor_id; the collect pass should assign it (span: {:?}, kind: {:?})",
                    expr.span, &expr.kind
                )
            }
            TirExprKind::Local { index, .. } => {
                if self.in_arg_position
                    && let Some(closure_id) = self.local_to_closure.get(index)
                {
                    self.specializable.swap_remove(closure_id);
                }
            }
            TirExprKind::Call { args, .. } => {
                let prev = std::mem::replace(&mut self.in_arg_position, true);
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
                self.in_arg_position = prev;
            }
            TirExprKind::CmRawCall { args, .. } => {
                let prev = std::mem::replace(&mut self.in_arg_position, true);
                for arg in args {
                    self.visit_expr(arg);
                }
                self.in_arg_position = prev;
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                let prev = std::mem::replace(&mut self.in_arg_position, false);
                self.visit_expr(receiver);
                self.in_arg_position = true;
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
                self.in_arg_position = prev;
            }
            TirExprKind::IndirectCall { callee, args } => {
                let prev = std::mem::replace(&mut self.in_arg_position, false);
                self.visit_expr(callee);
                self.in_arg_position = true;
                for arg in args {
                    self.visit_expr(arg);
                }
                self.in_arg_position = prev;
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                // Pass-through: original preserved in_arg_position here.
                self.visit_expr(functor);
            }
            _ => {
                // Every other recursion site in the original passes
                // `in_arg_position=false`, so reset before the default walk.
                let prev = std::mem::replace(&mut self.in_arg_position, false);
                self.walk_expr(expr);
                self.in_arg_position = prev;
            }
        }
    }
}

/// Phase 3 transformer: rewrites `Closure` literals into struct literals
/// (for specializable closures), `IndirectCall` on a closure-bearing local
/// into a direct `MethodCall` on `__call`, and `Call`/`MethodCall` whose
/// closure args have a matching specialised callee into a call to the
/// specialised function.
///
/// Closure bodies live in their own local-index namespace and are visited
/// independently via the generated `__call` methods (already in
/// `lowered_funcs` at the call site), so this pass does NOT recurse into
/// `Closure { body, .. }`.
struct Phase3Transformer<'a> {
    local_to_closure: &'a mut IndexMap<u32, u32>,
    specializable: &'a IndexSet<u32>,
    functor_infos: &'a [ClosureFunctor],
    fn_param_specializations: &'a IndexMap<FnParamSpecKey, String>,
    module_source: &'a ModuleSource,
    type_table: &'a mut TypeTable,
}

impl Phase3Transformer<'_> {
    fn try_transform_fn_param_call(&mut self, func: &mut FunctionRef, args: &mut [CallArg]) {
        // Closure args with `functor_id` that map to a known functor.
        let mut functor_types = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if let TirExprKind::Closure {
                functor_id: Some(id),
                ..
            } = &arg.expr.kind
                && let Some(functor) = self.functor_infos.get(*id as usize)
            {
                functor_types.push((i as u32, functor.struct_type_id));
            }
        }
        if functor_types.is_empty() {
            return;
        }

        let key = FnParamSpecKey {
            callee_name: func.name.clone(),
            functor_types: functor_types.clone(),
        };
        let Some(specialized_name) = self.fn_param_specializations.get(&key) else {
            return;
        };

        // Closure args → struct literals.
        for (arg_idx, _) in &functor_types {
            let arg = &mut args[*arg_idx as usize];
            if let TirExprKind::Closure {
                captures,
                functor_id: Some(closure_id),
                ..
            } = &arg.expr.kind
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
            {
                let fields: Vec<TirStructField> = captures
                    .iter()
                    .enumerate()
                    .map(|(i, cap)| TirStructField {
                        name: format!("__capture_{i}"),
                        value: TirExpr::new(
                            TirExprKind::Local {
                                index: cap.outer_index,
                                name: cap.name.clone(),
                            },
                            cap.type_id,
                            arg.expr.span,
                        ),
                        field_index: i as u32,
                    })
                    .collect();
                arg.expr.kind = TirExprKind::StructLiteral {
                    struct_type: functor.struct_type_id,
                    struct_name: functor.struct_name.clone(),
                    fields,
                };
                arg.expr.type_id = functor.ref_type_id;
            }
        }

        // Functor suffix lives AFTER the type args, so use `full_method_name()`
        // and clear `method_type_args` to avoid duplication.
        let functor_suffix: String =
            functor_types
                .iter()
                .fold(String::new(), |mut acc, (_, tid)| {
                    let name = self.type_table.type_name(*tid);
                    let _ = write!(acc, "${name}");
                    acc
                });

        let specialized_method_info = func.method_info.clone().map(|info| LocalMethodName {
            struct_name: info.struct_name.clone(),
            base_struct_name: info.base_struct_name.clone(),
            trait_name: info.trait_name.clone(),
            base_trait_name: info.base_trait_name.clone(),
            trait_type_args: info.trait_type_args.clone(),
            method_name: format!("{}{}", info.full_method_name(), functor_suffix),
            method_type_args: Vec::new(),
            is_type_param_receiver: info.is_type_param_receiver,
            is_ref_impl: false,
            cm_name: info.cm_name,
        });

        // Preserve monomorph_info so DCE can trace the call graph.
        let orig_monomorph_info = func.monomorph_info.clone();
        *func = FunctionRef {
            module_source: self.module_source.clone(),
            name: specialized_name.clone(),
            monomorph_info: orig_monomorph_info,
            method_info: specialized_method_info,
        };
    }
}

impl TirMutVisitor for Phase3Transformer<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        if let TirStmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } = &mut stmt.kind
        {
            // Track if this local stores a closure (read the stable ID off
            // the Closure node).
            let closure_id = if let TirExprKind::Closure {
                functor_id: Some(id),
                ..
            } = &value.kind
            {
                self.local_to_closure.insert(*local_index, *id);
                Some(*id)
            } else {
                None
            };

            self.visit_expr(value);

            // Update the Let's `type_id` for specializable closures.
            // Non-specializable ones keep their `fn(...)` type for
            // ClosureToCanonical.
            if let Some(id) = closure_id
                && self.specializable.contains(&id)
                && let Some(functor) = self.functor_infos.get(id as usize)
            {
                *type_id = functor.ref_type_id;
            }
            return;
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Closure {
                captures,
                functor_id,
                ..
            } => {
                let closure_id = functor_id.unwrap_or_else(|| {
                    panic!(
                        "Closure node missing functor_id; the collect pass should assign it (span: {:?})",
                        expr.span,
                    )
                });
                // Don't recurse into the body — its locals belong to a
                // different namespace and are processed via the generated
                // `__call` method.
                if self.specializable.contains(&closure_id)
                    && let Some(functor) = self.functor_infos.get(closure_id as usize)
                {
                    let fields: Vec<TirStructField> = captures
                        .iter()
                        .enumerate()
                        .map(|(i, cap)| TirStructField {
                            name: format!("__capture_{i}"),
                            value: TirExpr::new(
                                TirExprKind::Local {
                                    index: cap.outer_index,
                                    name: cap.name.clone(),
                                },
                                cap.type_id,
                                expr.span,
                            ),
                            field_index: i as u32,
                        })
                        .collect();
                    expr.kind = TirExprKind::StructLiteral {
                        struct_type: functor.struct_type_id,
                        struct_name: functor.struct_name.clone(),
                        fields,
                    };
                    expr.type_id = functor.ref_type_id;
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in &mut *args {
                    self.visit_expr(arg);
                }

                // Specializable closure stored in local → MethodCall on __call.
                if let TirExprKind::Local { index, .. } = &callee.kind
                    && let Some(closure_id) = self.local_to_closure.get(index)
                    && self.specializable.contains(closure_id)
                    && let Some(functor) = self.functor_infos.get(*closure_id as usize)
                {
                    let mut callee_owned = std::mem::replace(
                        callee.as_mut(),
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                    );
                    callee_owned.type_id = functor.ref_type_id;

                    let return_type = functor.call_method.borrow().return_type;
                    let args_owned: Vec<TirExpr> = std::mem::take(args);
                    let params_is_mut: Vec<bool> = functor
                        .call_method
                        .borrow()
                        .params
                        .iter()
                        .skip(1)
                        .map(|p| p.is_mut)
                        .collect();
                    let call_args: Vec<CallArg> = args_owned
                        .into_iter()
                        .zip(params_is_mut.into_iter().chain(std::iter::repeat(false)))
                        .map(|(e, is_mut)| CallArg::new(e, is_mut))
                        .collect();
                    expr.kind = TirExprKind::method_call(
                        Box::new(callee_owned),
                        FunctionRef::from_resolved(
                            &functor.call_method.borrow(),
                            self.module_source.clone(),
                        ),
                        Vec::new(),
                        call_args,
                    );
                    expr.type_id = return_type;
                }
            }
            TirExprKind::Call { func, args, .. } => {
                for arg in &mut *args {
                    self.visit_expr(&mut arg.expr);
                }
                self.try_transform_fn_param_call(func, args);
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                self.visit_expr(receiver);
                for arg in &mut *args {
                    self.visit_expr(&mut arg.expr);
                }
                self.try_transform_fn_param_call(func, args);
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Phase 1.5 in-place body transformer for the synthesised `__call` method.
///
/// Operates on a clone of the original closure body and rewrites:
/// - `Capture { index }` → `FieldAccess { self.__capture_<index> }`
/// - `Local { index }` → `Local { index + 1 }` (shift past the synthetic `self`)
/// - `Let { local_index }` → `Let { local_index + 1 }`
/// - `Binding { local_index }` (in patterns) → shifted by +1
///
/// Nested `Closure` nodes are NOT recursed into: their body lives in a
/// separate local-index namespace and gets its own `__call` method
/// generated at the same level. Their `captures[*].outer_index` references
/// locals in the outer pre-shift scope and are not rewritten here — the
/// nested-closure construction site is rewritten by Phase 3 and reads
/// capture values through the shifted local map at that point.
struct ClosureBodyTransformer<'a> {
    captures: &'a [TirCapture],
    self_ref_type: TypeId,
    self_span: Span,
}

impl TirMutVisitor for ClosureBodyTransformer<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Capture { index, .. } => {
                let index = *index;
                let cap_type = self
                    .captures
                    .get(index as usize)
                    .map_or(TypeTable::UNKNOWN, |c| c.type_id);
                let span = self.self_span;
                let self_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: 0,
                        name: "self".to_string(),
                    },
                    self.self_ref_type,
                    span,
                );
                expr.kind = TirExprKind::FieldAccess {
                    expr: Box::new(self_expr),
                    field_index: index,
                    field_name: format!("__capture_{index}"),
                };
                expr.type_id = cap_type;
                expr.span = span;
            }
            TirExprKind::Local { index, .. } => {
                *index += 1;
            }
            TirExprKind::Closure { .. } => {
                // Don't recurse — see struct doc.
            }
            _ => self.walk_expr(expr),
        }
    }

    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        if let TirStmtKind::Let { local_index, .. } = &mut stmt.kind {
            *local_index += 1;
        }
        self.walk_stmt(stmt);
    }

    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        if let TirPattern::Binding { local_index, .. } = pattern {
            *local_index += 1;
        }
        self.walk_pattern(pattern);
    }
}

/// Phase 2.5 collector: scan function bodies and record every direct
/// call (`Call` / `MethodCall`) whose closure args want a specialised
/// callee. Closures embed their `functor_id` from Phase 1, so the key
/// is reconstructible without any traversal-order counter.
struct FnParamSpecCollector<'a> {
    func_by_name: &'a IndexMap<String, Rc<RefCell<TirFunction>>>,
    type_table: &'a TypeTable,
    functor_infos: &'a [ClosureFunctor],
    requests: &'a mut Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)>,
}

impl FnParamSpecCollector<'_> {
    fn create_key(
        &self,
        callee_name: &str,
        params: &[TirParam],
        args: &[CallArg],
    ) -> Option<FnParamSpecKey> {
        let mut functor_types = Vec::new();
        for (i, (param, arg)) in params.iter().zip(args.iter()).enumerate() {
            if let ResolvedType::Function { .. } = self.type_table.get(param.type_id)
                && let TirExprKind::Closure {
                    functor_id: Some(closure_id),
                    ..
                } = &arg.expr.kind
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
            {
                functor_types.push((i as u32, functor.struct_type_id));
            }
        }
        if functor_types.is_empty() {
            return None;
        }
        Some(FnParamSpecKey {
            callee_name: callee_name.to_string(),
            functor_types,
        })
    }
}

impl TirRefVisitor for FnParamSpecCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                if let Some(callee_rc) = self.func_by_name.get(&func.name) {
                    let callee = callee_rc.borrow();
                    if let Some(key) = self.create_key(&callee.name, &callee.params, args) {
                        self.requests.push((key, Rc::clone(callee_rc)));
                    }
                }
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                if let Some(callee_rc) = self.func_by_name.get(&func.name) {
                    let callee = callee_rc.borrow();
                    let params_without_self: Vec<TirParam> =
                        callee.params.iter().skip(1).cloned().collect();
                    if let Some(key) = self.create_key(&callee.name, &params_without_self, args) {
                        self.requests.push((key, Rc::clone(callee_rc)));
                    }
                }
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Phase 2.5 predicate visitor: report whether any of the listed
/// fn-param locals appears as a direct struct-field value somewhere in
/// the body. Such locals can't be specialised — the struct field type
/// is `fn(...)`, not `&__Closure_N`.
///
/// Recurses through nested struct literals so a fn-param wrapped inside
/// `Foo { inner: Bar { f: param } }` still counts. Once `found` flips to
/// true, subsequent visits short-circuit cheaply.
struct StructFieldFnParamCheck<'a> {
    fn_param_indices: &'a [u32],
    found: bool,
}

impl TirRefVisitor for StructFieldFnParamCheck<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.found {
            return;
        }
        if let TirExprKind::StructLiteral { fields, .. } = &expr.kind {
            for field in fields {
                if let TirExprKind::Local { index, .. } = &field.value.kind
                    && self.fn_param_indices.contains(index)
                {
                    self.found = true;
                    return;
                }
            }
        }
        self.walk_expr(expr);
    }
}

/// Phase 2.5 in-place transformer for the cloned body of a specialised
/// callee. Rewrites:
/// - `IndirectCall { callee: Local(fn-param), args }` → `MethodCall` on
///   the corresponding `__call`.
/// - `Local { fn-param }` → keep the Local but retag its `type_id` to the
///   functor `&__Closure_N` so downstream code sees the new type.
/// - `Call` / `MethodCall` arg slots: when a fn-param Local is forwarded
///   into a callee that still expects `fn(...)`, wrap it in
///   `ClosureToCanonical` so the callee sees the original function type.
struct SpecializerTransformer<'a> {
    param_to_functor: &'a IndexMap<u32, TypeId>,
    functor_infos: &'a [ClosureFunctor],
    module_source: &'a ModuleSource,
    type_table: &'a mut TypeTable,
}

impl SpecializerTransformer<'_> {
    fn wrap_arg_if_needed(&self, arg: &mut TirExpr, original_type_id: TypeId) {
        if let TirExprKind::Local { index, .. } = &arg.kind
            && let Some(&functor_type) = self.param_to_functor.get(index)
            && matches!(
                self.type_table.get(original_type_id),
                ResolvedType::Function { .. }
            )
            && let Some(functor) = self
                .functor_infos
                .iter()
                .find(|f| f.struct_type_id == functor_type)
        {
            let span = arg.span;
            let inner =
                std::mem::replace(arg, TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span));
            *arg = TirExpr::new(
                TirExprKind::ClosureToCanonical {
                    functor: Box::new(inner),
                    functor_id: functor.id,
                    target_fn_type: original_type_id,
                    closure_module: self.module_source.clone(),
                },
                original_type_id,
                span,
            );
        }
    }
}

impl TirMutVisitor for SpecializerTransformer<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in &mut *args {
                    self.visit_expr(arg);
                }
                if let TirExprKind::Local { index, .. } = &callee.kind
                    && let Some(&functor_type) = self.param_to_functor.get(index)
                    && let Some(functor) = self
                        .functor_infos
                        .iter()
                        .find(|f| f.struct_type_id == functor_type)
                {
                    let call_method_name =
                        MethodName::format_local(&functor.struct_name, None, "__call");
                    let callee_owned = std::mem::replace(
                        callee.as_mut(),
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                    );
                    let args_owned: Vec<TirExpr> = std::mem::take(args);

                    let call_method_info = LocalMethodName::new(
                        functor.struct_name.clone(),
                        None,
                        "__call".to_string(),
                    );

                    let params_is_mut: Vec<bool> = functor
                        .call_method
                        .borrow()
                        .params
                        .iter()
                        .skip(1)
                        .map(|p| p.is_mut)
                        .collect();
                    let call_args: Vec<CallArg> = args_owned
                        .into_iter()
                        .zip(params_is_mut.into_iter().chain(std::iter::repeat(false)))
                        .map(|(e, is_mut)| CallArg::new(e, is_mut))
                        .collect();
                    expr.kind = TirExprKind::method_call(
                        Box::new(callee_owned),
                        FunctionRef {
                            module_source: self.module_source.clone(),
                            name: call_method_name,
                            monomorph_info: None,
                            method_info: Some(call_method_info),
                        },
                        Vec::new(),
                        call_args,
                    );
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in &mut *args {
                    let original_type_id = arg.expr.type_id;
                    self.visit_expr(&mut arg.expr);
                    self.wrap_arg_if_needed(&mut arg.expr, original_type_id);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in &mut *args {
                    let original_type_id = arg.expr.type_id;
                    self.visit_expr(&mut arg.expr);
                    self.wrap_arg_if_needed(&mut arg.expr, original_type_id);
                }
            }
            TirExprKind::Local { index, .. } => {
                if let Some(&functor_type) = self.param_to_functor.get(index) {
                    expr.type_id = self.type_table.make_ref(functor_type);
                }
            }
            _ => self.walk_expr(expr),
        }
    }
}
