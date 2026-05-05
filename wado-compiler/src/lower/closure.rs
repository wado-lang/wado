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

/// `1` for instance methods (those whose first parameter is named `self`),
/// `0` otherwise. Used to convert a 0-based call-site argument index into
/// the corresponding 0-based parameter / local index inside the callee.
fn self_param_offset(callee: &TirFunction) -> u32 {
    u32::from(callee.params.first().is_some_and(|p| p.name == "self"))
}

/// Build the canonical signature string for a closure, e.g.
/// `"|i32, String| -> bool"`. Used as the body of
/// `__Closure_N^Inspect::inspect` so the per-literal Inspect output
/// matches WEP: Inspect (Debug Output) regardless of how the closure
/// is later dispatched (specialized or canonical).
fn format_closure_signature(
    params: &[(String, TypeId)],
    return_type: TypeId,
    type_table: &TypeTable,
) -> String {
    let param_names: Vec<String> = params
        .iter()
        .map(|(_, ty)| type_table.type_name(*ty))
        .collect();
    let ret_name = type_table.type_name(return_type);
    format!("|{}| -> {}", param_names.join(", "), ret_name)
}

/// Build the `__capture_<i>` struct-field list for a functor literal from
/// the closure's captures. Each field reads the captured value from the
/// outer scope at `cap.outer_index`.
fn build_capture_fields(captures: &[TirCapture], span: Span) -> Vec<TirStructField> {
    captures
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
        .collect()
}

/// Build the `$__Closure_N` mangled-name suffix appended to a callee that
/// has been specialised over the listed functor struct types.
fn build_functor_suffix(functor_types: &[(u32, TypeId)], type_table: &TypeTable) -> String {
    functor_types
        .iter()
        .fold(String::new(), |mut acc, (_, tid)| {
            let _ = write!(acc, "${}", type_table.type_name(*tid));
            acc
        })
}

/// Pad raw call args out to a `CallArg` list whose `is_mut` flags match
/// the `__call` method's parameter list (skipping `self`). Extra args
/// beyond the parameter list default to `is_mut = false`.
fn make_call_method_args(args: Vec<TirExpr>, call_method: &TirFunction) -> Vec<CallArg> {
    let params_is_mut: Vec<bool> = call_method
        .params
        .iter()
        .skip(1)
        .map(|p| p.is_mut)
        .collect();
    args.into_iter()
        .zip(params_is_mut.into_iter().chain(std::iter::repeat(false)))
        .map(|(e, is_mut)| CallArg::new(e, is_mut))
        .collect()
}

/// Build a `LocalMethodName` for a specialised method by appending the
/// functor suffix to the original method's full name. The `method_type_args`
/// are emptied because the type args have already been baked into the
/// method name.
fn build_specialized_method_info(info: &LocalMethodName, functor_suffix: &str) -> LocalMethodName {
    LocalMethodName {
        struct_name: info.struct_name.clone(),
        base_struct_name: info.base_struct_name.clone(),
        trait_name: info.trait_name.clone(),
        base_trait_name: info.base_trait_name.clone(),
        trait_type_args: info.trait_type_args.clone(),
        method_name: format!("{}{}", info.full_method_name(), functor_suffix),
        method_type_args: Vec::new(),
        is_type_param_receiver: info.is_type_param_receiver,
        is_ref_impl: false,
        cm_name: info.cm_name.clone(),
    }
}

/// Snapshot taken in Phase 1 for the functor-generation pass. `body` is a
/// deep clone so the original AST can be mutated in place by later passes
/// without disturbing the synthesised `__call` body.
#[derive(Debug, Clone)]
struct CollectedClosure {
    id: u32,
    params: Vec<(String, TypeId)>,
    body: TirExpr,
    captures: Vec<TirCapture>,
    /// `body.type_id`, retained for closures whose `func_type_id` resolves
    /// to a non-Function (a fallback path in `generate_functor_items`).
    return_type: TypeId,
    /// The closure expression's own type — a `fn(...)` type that carries
    /// the canonical return type even when the body is a Block expr.
    func_type_id: TypeId,
    /// TIR-unparsed source of the closure (set in `desugar.rs`). Consumed
    /// when synthesising `__Closure_N^InspectAlt::inspect_alt` so the
    /// per-literal source body becomes a compile-time string constant in
    /// the impl. `None` for synthesised closures that have no source.
    source_text: Option<String>,
    span: Span,
}

/// Signature of a top-level function or impl method, used by Phase 0 to
/// turn a bare `FuncRef` into a forwarding zero-capture closure.
struct FuncSig {
    params: Vec<(String, TypeId)>,
    return_type: TypeId,
}

/// Identifies a specialised callee: original name plus the functor struct
/// type bound at each fn-type parameter. Used as both a dedup key
/// (Phase 2.5) and a lookup key at the call site (Phase 3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FnParamSpecKey {
    callee_name: String,
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
    /// Counter for the Phase 1 walk. Each visited `Closure` has its
    /// `functor_id` populated from this — that ID is the stable index
    /// every later pass uses to look up its `ClosureFunctor`.
    next_closure_id: u32,
    module_source: ModuleSource,
    collected_closures: Vec<CollectedClosure>,
    /// Indexed by `functor_id`. Moved into `module.closure_functors` at the
    /// end of `lower_module` so the optimizer can inline `__call` bodies.
    functor_infos: Vec<ClosureFunctor>,
    /// Per-function map (cleared between functions) recording which locals
    /// hold which closure. Read by Phase 2 (safety analysis) and Phase 3
    /// (IndirectCall→MethodCall + `update_local_types`).
    local_to_closure: IndexMap<u32, u32>,
    /// Closure IDs safe for direct specialisation (the closure is stored
    /// in a local and only used by direct calls). Non-specialisable ones
    /// route through `ClosureToCanonical` instead.
    specializable: IndexSet<u32>,
    generated_structs: Vec<TirStruct>,
    generated_functions: Vec<Rc<RefCell<TirFunction>>>,
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

        // Phase 3: lower closure call sites. We walk the generated `__call`
        // methods alongside the original module functions because a nested
        // closure's body was cloned into its parent's `__call` and that
        // copy still contains call sites that need lowering. Each function
        // gets a fresh `local_to_closure` map (keyed by per-function local
        // indices). `ClosureCallSiteLowerer` deliberately does NOT recurse
        // into Closure bodies — those are visited via the corresponding
        // `__call` method.
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
                ClosureCallSiteLowerer {
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
                    ClosureCallSiteLowerer {
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
        // Sort by id so `functor_infos[id]` is the functor with that id —
        // every later pass uses index-by-id lookups.
        //
        // Move out of `self.collected_closures` to dodge a long borrow:
        // the loop body needs `&mut self` to push to `generated_structs`
        // and `functor_infos`. The list isn't read after this pass.
        let mut collected_closures = std::mem::take(&mut self.collected_closures);
        collected_closures.sort_by_key(|c| c.id);
        for collected in &collected_closures {
            // `collected.body.type_id` is unreliable for block bodies
            // (it's the block's type, not the closure's), so prefer the
            // function type's return slot when available.
            let return_type = match type_table.get(collected.func_type_id) {
                ResolvedType::Function { return_type, .. } => *return_type,
                _ => collected.return_type,
            };

            let struct_name = format!("__Closure_{}", collected.id);
            let struct_type_id =
                type_table.make_struct(struct_name.clone(), self.module_source.clone());

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

            self.generated_structs.push(TirStruct {
                name: struct_name.clone(),
                module_source: self.module_source.clone(),
                is_pub: false,
                type_params: Vec::new(),
                monomorph_info: None,
                fields,
                span: collected.span,
                serde_rename_all: None,
            });

            // Use a qualified name to avoid collisions in the inliner's
            // candidate map. `LocalMethodName` stays unqualified so codegen
            // re-mangles it consistently with other methods.
            let qualified_method_name = MethodName::format_local(&struct_name, None, "__call");
            let self_ref_type = type_table.make_ref(struct_type_id);

            let mut params = Vec::with_capacity(1 + collected.params.len());
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

            // Rewrite Capture→FieldAccess and shift every Local / Let /
            // Binding index by 1 to make room for the synthetic `self`.
            let mut transformed_body = collected.body.clone();
            ClosureBodyTransformer {
                captures: &collected.captures,
                self_ref_type,
                self_span: collected.span,
            }
            .visit_expr(&mut transformed_body);

            // Block bodies: keep the inner statements as-is so that any
            // Return inside survives. The inliner's `remap_stmt_with_label`
            // turns Return into Break only at the statement level, so a
            // Return wrapped inside a Block expression would be missed.
            let body_stmts = match &transformed_body.kind {
                TirExprKind::Block(block) => block.stmts.clone(),
                _ if return_type == TypeTable::UNIT => vec![TirStmt::new(
                    TirStmtKind::Expr(transformed_body),
                    collected.span,
                )],
                _ => vec![TirStmt::new(
                    TirStmtKind::Return {
                        value: Some(transformed_body),
                    },
                    collected.span,
                )],
            };

            let body_block = TirBlock::new(body_stmts, collected.span);

            // Locals layout: 0=self, 1..=params.len()=closure params,
            // then any further locals introduced by `Let`s in the body.
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

            let mut body_locals: Vec<(u32, TypeId)> = Vec::new();
            LocalCollector {
                locals: &mut body_locals,
            }
            .visit_block(&body_block);

            // Body locals use synthetic placeholders here; `wir_build`
            // recovers source names from `TirFunction::locals[idx].name`.
            body_locals.sort_by_key(|(idx, _)| *idx);
            for (idx, type_id) in &body_locals {
                if *idx >= param_count {
                    // Pad sparse indices with placeholders so the slot the
                    // body actually uses lands at the right index.
                    while locals.len() <= *idx as usize {
                        let placeholder_idx = locals.len() as u32;
                        locals.push(TirLocal::synth(placeholder_idx, TypeTable::UNKNOWN, false));
                    }
                    locals[*idx as usize] = TirLocal::synth(*idx, *type_id, false);
                }
            }

            let local_count = locals.len() as u32;

            // `method_info` carries the unmangled (struct, trait, method)
            // triple so codegen can produce the canonical mangled name.
            let method_info = LocalMethodName::new(struct_name.clone(), None, "__call".to_string());

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
                struct_name: struct_name.clone(),
                struct_type_id,
                ref_type_id: self_ref_type,
                call_method: call_method_rc,
                captures: collected.captures.clone(),
            });

            // Synthesize per-functor Inspect / InspectAlt impls so trait
            // dispatch on the specialized `&__Closure_N` value writes the
            // per-literal signature / TIR-unparsed source. The template
            // expansion's `Function` short-circuit is still present, so
            // these impls are reachable only from user-written
            // `closure.inspect(&mut f)` style calls and from the
            // ClosureCallSiteLowerer redirect added below; standard DCE
            // removes them when neither is reached.
            let signature = format_closure_signature(&collected.params, return_type, type_table);
            let source = collected
                .source_text
                .clone()
                .unwrap_or_else(|| signature.clone());
            self.generate_functor_format_methods(
                &struct_name,
                self_ref_type,
                &signature,
                &source,
                type_table,
                collected.span,
            );
        }
    }

    /// Synthesize `__Closure_N^Inspect::inspect` and
    /// `__Closure_N^InspectAlt::inspect_alt` for a single functor.
    ///
    /// Both methods take `(&self: &__Closure_N, f: &mut Formatter)` and
    /// emit a single `f.write_str(<constant>)` body. The signature string
    /// (`"|i32, i32| -> i32"`) and the source body string
    /// (`"|x: i32, y: i32| x + y"`) are computed by the caller from
    /// `CollectedClosure` so this helper stays focused on TIR
    /// construction.
    fn generate_functor_format_methods(
        &mut self,
        struct_name: &str,
        self_ref_type: TypeId,
        signature: &str,
        source: &str,
        type_table: &mut TypeTable,
        span: Span,
    ) {
        let formatter_type =
            type_table.make_struct("Formatter".to_string(), ModuleSource::format());
        let formatter_mut_ref = type_table.make_mut_ref(formatter_type);
        let string_type = type_table.make_struct("String".to_string(), ModuleSource::string());

        for (trait_name, method_name, payload) in [
            ("Inspect", "inspect", signature),
            ("InspectAlt", "inspect_alt", source),
        ] {
            let func = self.build_functor_format_method(
                struct_name,
                trait_name,
                method_name,
                payload,
                self_ref_type,
                formatter_mut_ref,
                string_type,
                span,
            );
            self.generated_functions.push(Rc::new(RefCell::new(func)));
        }
    }

    /// Build a single `__Closure_N^TraitName::method_name(&self, &mut Formatter)`
    /// whose body is `f.write_str("<payload>")`. Shared by both `Inspect` and
    /// `InspectAlt` synthesis.
    #[allow(clippy::too_many_arguments)]
    fn build_functor_format_method(
        &self,
        struct_name: &str,
        trait_name: &str,
        method_name: &str,
        payload: &str,
        self_ref_type: TypeId,
        formatter_mut_ref: TypeId,
        string_type: TypeId,
        span: Span,
    ) -> TirFunction {
        let method_info = LocalMethodName::new(
            struct_name.to_string(),
            Some(trait_name.to_string()),
            method_name.to_string(),
        );
        let qualified_name = MethodName::format_local(struct_name, Some(trait_name), method_name);

        let fmt_local = TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            formatter_mut_ref,
            span,
        );
        let write_str_call = TirExpr::new(
            TirExprKind::method_call(
                Box::new(fmt_local),
                FunctionRef {
                    module_source: ModuleSource::format(),
                    name: "Formatter::write_str".to_string(),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        "Formatter".to_string(),
                        None,
                        "write_str".to_string(),
                    )),
                },
                vec![],
                vec![CallArg::new(
                    TirExpr::new(
                        TirExprKind::StringLiteral(payload.to_string()),
                        string_type,
                        span,
                    ),
                    false,
                )],
            ),
            TypeTable::UNIT,
            span,
        );
        let body = TirBlock::new(
            vec![TirStmt::new(TirStmtKind::Expr(write_str_call), span)],
            span,
        );

        TirFunction {
            module_source: self.module_source.clone(),
            is_async: false,
            name: qualified_name,
            is_pub: false,
            is_export: false,
            type_params: Vec::new(),
            impl_type_params: Vec::new(),
            monomorph_info: None,
            method_info: Some(method_info),
            params: vec![
                TirParam {
                    name: "self".to_string(),
                    type_id: self_ref_type,
                    local_index: 0,
                    is_mut: false,
                    span,
                    default_expr: None,
                },
                TirParam {
                    name: "f".to_string(),
                    type_id: formatter_mut_ref,
                    local_index: 1,
                    is_mut: false,
                    span,
                    default_expr: None,
                },
            ],
            return_type: TypeTable::UNIT,
            task_return_type: None,
            effects: Vec::new(),
            stores: vec![],
            body: Some(body),
            span,
            local_count: 2,
            locals: vec![
                TirLocal {
                    name: "self".to_string(),
                    type_id: self_ref_type,
                    is_mut: false,
                },
                TirLocal {
                    name: "f".to_string(),
                    type_id: formatter_mut_ref,
                    is_mut: false,
                },
            ],
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
        }
    }

    /// Phase 2.5: when a function takes a `fn(A) -> B` parameter and is
    /// called with a closure literal, generate a specialised callee whose
    /// fn-type parameters are the functor struct types and whose
    /// `IndirectCall`s become direct `MethodCall`s on `__call`.
    fn generate_fn_param_specializations(
        &mut self,
        func_refs: &[Rc<RefCell<TirFunction>>],
        impls: &[TirImpl],
        type_table: &mut TypeTable,
    ) {
        let mut func_by_name: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
        for func_rc in func_refs {
            let func = func_rc.borrow();
            func_by_name.insert(func.name.clone(), Rc::clone(func_rc));
        }
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

        for (key, callee_rc) in spec_requests {
            if self.fn_param_specializations.contains_key(&key) {
                continue;
            }

            let callee = callee_rc.borrow();
            // Static methods have `method_info` but no `self`, so we sniff
            // the first parameter name instead of inspecting `method_info`.
            let param_offset = self_param_offset(&callee);
            let fn_param_indices: Vec<u32> = key
                .functor_types
                .iter()
                .map(|(arg_idx, _)| arg_idx + param_offset)
                .collect();

            // Bail when a fn-param flows into a struct field — the field
            // type stays `fn(...)`, so we can't retype the param to
            // `&__Closure_N`.
            if let Some(body) = &callee.body {
                let mut check = StructFieldFnParamCheck {
                    fn_param_indices: &fn_param_indices,
                    found: false,
                };
                check.visit_block(body);
                if check.found {
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

        let functor_suffix = build_functor_suffix(&key.functor_types, type_table);
        let specialized_name = format!("{}{}", callee.name, functor_suffix);

        // `key.functor_types` keys are argument indices (0 = first arg
        // after the receiver for methods); shift by `param_offset` to land
        // on the corresponding parameter / local slot.
        let param_offset = self_param_offset(&callee);
        let arg_to_functor: IndexMap<u32, TypeId> = key.functor_types.iter().copied().collect();

        let mut new_params = callee.params.clone();
        let mut new_locals = callee.locals.clone();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let slot = (*arg_idx + param_offset) as usize;
            if slot < new_params.len() {
                new_params[slot].type_id = type_table.make_ref(functor_type);
            }
            if slot < new_locals.len() {
                new_locals[slot].type_id = type_table.make_ref(functor_type);
            }
        }

        let local_to_functor: IndexMap<u32, TypeId> = arg_to_functor
            .iter()
            .map(|(arg_idx, functor_type)| (arg_idx + param_offset, *functor_type))
            .collect();

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

        let specialized_method_info = callee
            .method_info
            .as_ref()
            .map(|info| build_specialized_method_info(info, &functor_suffix));

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
        let struct_literal = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: functor.struct_type_id,
                struct_name: functor.struct_name.clone(),
                fields: build_capture_fields(&captures, span),
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
            source_text,
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
                source_text: source_text.clone(),
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

/// Phase 3: rewrite every closure call site:
/// - `Closure` literal (specialisable) → struct literal of `__Closure_N`
/// - `IndirectCall` on a closure-bearing local → `MethodCall` on `__call`
/// - `Call` / `MethodCall` whose closure args have a matching specialised
///   callee → redirected to that callee
///
/// Closure bodies live in their own local-index namespace, so this pass
/// does NOT recurse into `Closure { body, .. }` — those bodies live in the
/// generated `__call` methods, which `lower_module` walks separately.
struct ClosureCallSiteLowerer<'a> {
    local_to_closure: &'a mut IndexMap<u32, u32>,
    specializable: &'a IndexSet<u32>,
    functor_infos: &'a [ClosureFunctor],
    fn_param_specializations: &'a IndexMap<FnParamSpecKey, String>,
    module_source: &'a ModuleSource,
    type_table: &'a mut TypeTable,
}

impl ClosureCallSiteLowerer<'_> {
    fn try_redirect_to_specialized_callee(&mut self, func: &mut FunctionRef, args: &mut [CallArg]) {
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
                arg.expr.kind = TirExprKind::StructLiteral {
                    struct_type: functor.struct_type_id,
                    struct_name: functor.struct_name.clone(),
                    fields: build_capture_fields(captures, arg.expr.span),
                };
                arg.expr.type_id = functor.ref_type_id;
            }
        }

        let functor_suffix = build_functor_suffix(&functor_types, self.type_table);
        let specialized_method_info = func
            .method_info
            .as_ref()
            .map(|info| build_specialized_method_info(info, &functor_suffix));

        // Preserve monomorph_info so DCE can trace the call graph.
        let orig_monomorph_info = func.monomorph_info.clone();
        *func = FunctionRef {
            module_source: self.module_source.clone(),
            name: specialized_name.clone(),
            monomorph_info: orig_monomorph_info,
            method_info: specialized_method_info,
        };
    }

    /// When a `MethodCall` resolves to `Fn<N, Ret>^Inspect::inspect` or
    /// `Fn<N, Ret>^InspectAlt::inspect_alt` with a receiver that is a
    /// specialized closure local, redirect the call to the per-functor
    /// `__Closure_N^Inspect / InspectAlt` impl synthesised in
    /// `generate_functor_items`. This is what makes the specialized
    /// path produce the per-literal signature/source rather than the
    /// generic canonical-vtable indirection.
    ///
    /// The receiver is rewritten from `Unary::Ref(Local(idx, type=Fn))`
    /// to `Local(idx, type=&__Closure_N)` since the per-functor impl
    /// expects `&self: &__Closure_N` directly (the local already holds
    /// a ref after specialization). When no redirect applies (the call
    /// is on a canonical closure value, or the trait isn't a closure
    /// inspect trait), this is a no-op and the call keeps its
    /// `Fn<N, Ret>^...` target for the canonical-vtable path.
    fn try_redirect_inspect_to_functor(&self, receiver: &mut Box<TirExpr>, func: &mut FunctionRef) {
        let info = match &func.method_info {
            Some(info) => info,
            None => return,
        };
        if info.base_struct_name != "Fn" {
            return;
        }
        let Some(base_trait) = info.base_trait_name.as_deref() else {
            return;
        };
        if base_trait != "Inspect" && base_trait != "InspectAlt" {
            return;
        }

        let local_idx = match peel_ref_to_local(receiver) {
            Some(idx) => idx,
            None => return,
        };
        let Some(closure_id) = self.local_to_closure.get(&local_idx).copied() else {
            return;
        };
        if !self.specializable.contains(&closure_id) {
            return;
        }
        let Some(functor) = self.functor_infos.get(closure_id as usize) else {
            return;
        };

        let local_name = match &receiver.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr,
            } => match &expr.kind {
                TirExprKind::Local { name, .. } => name.clone(),
                _ => "self".to_string(),
            },
            TirExprKind::Local { name, .. } => name.clone(),
            _ => "self".to_string(),
        };

        let new_method_info = LocalMethodName::new(
            functor.struct_name.clone(),
            Some(base_trait.to_string()),
            info.method_name.clone(),
        );
        let new_name = new_method_info.to_mangled_name();

        let span = receiver.span;
        **receiver = TirExpr::new(
            TirExprKind::Local {
                index: local_idx,
                name: local_name,
            },
            functor.ref_type_id,
            span,
        );
        *func = FunctionRef {
            module_source: self.module_source.clone(),
            name: new_name,
            monomorph_info: None,
            method_info: Some(new_method_info),
        };
    }
}

/// Walk through `Ref` / `MutRef` wrappers to find an inner `Local` and
/// return its index. Returns `None` if the receiver isn't a ref-of-local
/// or a bare local.
fn peel_ref_to_local(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } => peel_ref_to_local(inner),
        _ => None,
    }
}

impl TirMutVisitor for ClosureCallSiteLowerer<'_> {
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
                    expr.kind = TirExprKind::StructLiteral {
                        struct_type: functor.struct_type_id,
                        struct_name: functor.struct_name.clone(),
                        fields: build_capture_fields(captures, expr.span),
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
                    let call_args =
                        make_call_method_args(args_owned, &functor.call_method.borrow());
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
                self.try_redirect_to_specialized_callee(func, args);
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
                self.try_redirect_to_specialized_callee(func, args);
                self.try_redirect_inspect_to_functor(receiver, func);
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

                    let call_args =
                        make_call_method_args(args_owned, &functor.call_method.borrow());
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
