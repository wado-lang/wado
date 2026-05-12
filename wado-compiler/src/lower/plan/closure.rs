use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName};
use crate::tir;
use crate::tir::{
    CallArg, ClosureFunctor, FunctionKind, FunctionRef, InlineHint, ResolvedType, TirBlock,
    TirCapture, TirExpr, TirExprKind, TirField, TirFunction, TirLocal, TirParam, TirPattern,
    TirStmt, TirStmtKind, TirStruct, TirStructField, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};
use crate::token::Span;

/// Per-local entry recording that a function parameter has been
/// specialized to a functor type. Populated for synthesized
/// fn-param-specialized callees. The translator uses these entries to:
///
/// - Retag `Local` reads of the param to the functor `&__Closure_N`
///   type instead of the original `fn(...)` declared type.
/// - Rewrite `IndirectCall { callee: Local(param), .. }` into
///   `MethodCall` on the functor's `__call` method.
/// - Wrap `Local(param)` args to `Call` / `MethodCall` slots that
///   still expect `fn(...)` in `NirExprKind::ClosureToCanonical`,
///   converting the specialized value back to its canonical
///   function-shaped view.
pub struct SpecializedLocal {
    pub local_index: u32,
    pub functor_id: u32,
    pub functor_ref_type: tir::TypeId,
    pub original_fn_type: tir::TypeId,
}

/// Result of closure planning.
///
/// `functor_infos` is consumed by the TIR → NIR translator to populate
/// `NirPackage::closure_functors`. It carries per-functor metadata
/// (struct name, `__call` method body, captures, canonical signature)
/// that the optimizer uses for closure inlining.
///
/// `specialized_locals` lists, for each synthesized specialized
/// callee, the params that were re-bound to functor types. See
/// [`SpecializedLocal`].
pub struct ClosurePlan {
    pub functor_infos: Vec<ClosureFunctor>,
    pub specialized_locals: IndexMap<(ModuleSource, String), Vec<SpecializedLocal>>,
}

/// Run the closure planner.
///
/// TIR-mutating: rewrites `Closure` / `FuncRef` / `Capture` /
/// `IndirectCall` nodes in place, generates `__Closure_N` functor
/// structs and `__call` methods, and produces specialized callees for
/// fn-typed parameters. Remaining `Closure` nodes survive in TIR until
/// the translator emits `NirExprKind::ClosureToCanonical` from them.
pub fn plan(flat: &mut FlatPackage) -> ClosurePlan {
    let mut closure_lowerer = ClosureLowerer::new(&flat.entry_module_source);
    closure_lowerer.lower_module(flat);
    ClosurePlan {
        functor_infos: std::mem::take(&mut closure_lowerer.functor_infos),
        specialized_locals: std::mem::take(&mut closure_lowerer.specialized_locals),
    }
}

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
/// without disturbing the synthesised `__call` body. The body is also
/// re-unparsed by `generate_functor_items` to bake the per-literal source
/// string into `__Closure_N^InspectAlt::inspect_alt`.
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
struct ClosureLowerer {
    /// Counter for the Phase 1 walk. Each visited `Closure` has its
    /// `functor_id` populated from this — that ID is the stable index
    /// every later pass uses to look up its `ClosureFunctor`.
    next_closure_id: u32,
    module_source: ModuleSource,
    collected_closures: Vec<CollectedClosure>,
    /// Indexed by `functor_id`. Taken out of `ClosureLowerer` by
    /// [`plan`] and handed to [`ClosurePlan::functor_infos`], where the
    /// translator reads it to populate `NirPackage::closure_functors`
    /// (the optimizer uses that for `__call`-body inlining).
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
    /// For each synthesized fn-param-specialized callee, the locals
    /// that were re-bound from `fn(...)` to functor types. The
    /// translator consumes this map via `ClosurePlan::specialized_locals`
    /// to rewrite call sites and `Local` reads inside the specialized
    /// callee body.
    specialized_locals: IndexMap<(ModuleSource, String), Vec<SpecializedLocal>>,
}

impl ClosureLowerer {
    fn new(module_source: &ModuleSource) -> Self {
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
            specialized_locals: IndexMap::default(),
        }
    }

    /// Lower all closures in a module
    fn lower_module(&mut self, flat: &mut FlatPackage) {
        // Phase 0: Convert FuncRef used as values to zero-capture Closures.
        // Named functions used as values (e.g., `&double` or `double` passed to fn-type params)
        // need to become Closure nodes so the existing closure pipeline handles them.
        self.convert_func_refs_to_closures(flat);

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

        let func_refs: Vec<_> = flat.functions.clone();
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

        // Generate functor structs and __call methods. Each `__call` body is
        // a clone of the closure body taken AFTER `collect_closures_in_block`
        // assigned IDs to nested closures, so those clones carry the same
        // stable functor IDs as the originals.
        self.generate_functor_items(&mut flat.type_table.borrow_mut());

        // Phase 2: analyse which closures are safe to specialise. Reads
        // `functor_id` directly off the Closure node — no counter.
        for func_rc in &func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.local_to_closure.clear();
                ClosureSafetyAnalyzer {
                    local_to_closure: &mut self.local_to_closure,
                    specializable: &mut self.specializable,
                    in_callee_position: false,
                }
                .visit_block(body);
            }
        }

        // Phase 2.5: Generate specialized functions for fn-param monomorphization.
        // For closures passed as fn-type arguments, generate specialized callees.
        self.generate_fn_param_specializations(&func_refs, &mut flat.type_table.borrow_mut());

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
            // Snapshot param `(local_index, type_id)` so the seed pass
            // can run while `func.body` is borrowed mutably.
            let param_locals: Vec<(u32, TypeId)> = func
                .params
                .iter()
                .map(|p| (p.local_index, p.type_id))
                .collect();
            if let Some(body) = &mut func.body {
                self.local_to_closure.clear();
                let mut tt = flat.type_table.borrow_mut();
                self.seed_local_to_closure_from_params(&param_locals, &tt);
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

        // Remaining `Closure` nodes (those not turned into
        // `StructLiteral` at a specialized `Let` site) are now
        // rewritten by the TIR → NIR translator's `Closure` arm using
        // `ClosurePlan::functor_infos`, which produces
        // `NirExprKind::ClosureToCanonical` directly.

        // Functor metadata stays on `self.functor_infos`; the planner
        // moves it directly into `ClosurePlan` instead of staging it
        // on `FlatPackage`.

        // Add ALL generated structs and functions to the module
        flat.structs
            .extend(std::mem::take(&mut self.generated_structs));
        flat.functions
            .extend(std::mem::take(&mut self.generated_functions));
    }

    /// Convert `FuncRef` nodes (used as values, not in Call/MethodCall func positions) to
    /// zero-capture `Closure` nodes. This enables named functions to be passed as function-type
    /// arguments (e.g., `apply(double, 21)` or `apply(&double, 21)`).
    fn convert_func_refs_to_closures(&self, flat: &mut FlatPackage) {
        // Build a map from function name to (param_types, return_type).
        //
        // `lower::lower` operates on a flattened `FlatPackage` — all functions
        // from every module (entry, prelude, stdlib, transitively-loaded user
        // modules) are placed into a single `module.functions` before this
        // runs, so a bare-name lookup here covers imported function refs like
        // `&println` from `core:cli` without an additional cross-module pass.
        let mut func_sigs: IndexMap<String, FuncSig> = IndexMap::default();
        for func_rc in &flat.functions {
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

        let mut type_table = flat.type_table.borrow_mut();
        let mut rewriter = FuncRefToClosureRewriter {
            func_sigs: &func_sigs,
            type_table: &mut type_table,
        };
        for func_rc in &flat.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                rewriter.visit_block(body);
            }
        }
    }

    /// Update `locals` in a function after closure transformation
    /// Seed `local_to_closure` for any function parameter whose declared
    /// type is `&__Closure_N` (the result of fn-param specialisation in
    /// `generate_fn_param_specializations`). Without this, the
    /// inspect-redirect in `try_redirect_inspect_to_functor` would miss
    /// specialised parameters — they aren't `let`-bound, so the safety
    /// analyser doesn't put them in the map — and `f.Fn^Inspect::inspect`
    /// inside a specialised function body would fall through to the
    /// canonical-vtable dispatch on a `&__Closure_N` value, where the
    /// `ref.cast` to `$canonical_inspectable_base` traps.
    fn seed_local_to_closure_from_params(
        &mut self,
        param_locals: &[(u32, TypeId)],
        type_table: &TypeTable,
    ) {
        for (local_index, type_id) in param_locals {
            let bare = type_table.peel_refs(*type_id);
            for (idx, functor) in self.functor_infos.iter().enumerate() {
                if functor.struct_type_id == bare {
                    let closure_id = u32::try_from(idx).expect("functor id fits in u32");
                    self.local_to_closure.insert(*local_index, closure_id);
                    break;
                }
            }
        }
    }

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

                return_abi: crate::tir::ReturnAbi::default(),
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
                canonical_user_params: collected.params.clone(),
                canonical_return: return_type,
            });

            // Synthesize per-functor Inspect / InspectAlt impls so trait
            // dispatch on the specialised `&__Closure_N` value writes the
            // per-literal signature / TIR-unparsed source. Template
            // expansion routes `{f:?}` / `{f:#?}` for fn-typed receivers
            // through the same `Fn<N, Ret>^Inspect[Alt]::inspect[_alt]`
            // call shape that user-written `f.inspect(&mut formatter)`
            // produces, so these impls are reachable through three
            // routes: explicit user calls, the `ClosureCallSiteLowerer`
            // redirect added below (specialised path), and the
            // canonical-vtable inspect slots (indirect path). Standard
            // DCE removes them when none of these reach.
            let signature = format_closure_signature(&collected.params, return_type, type_table);
            // Recover the per-literal source body (`|x: i32| x + 1`)
            // by unparsing the captured TIR closure form. The TIR is
            // post-resolve so type annotations are concrete; output
            // may differ from the original source byte-for-byte but
            // remains the canonical inspect-debug representation per
            // WEP: Inspect (Debug Output) > Closure Inspect via
            // Runtime Dispatch.
            let source = crate::unparse::unparse_tir_closure_source(
                &collected.params,
                &collected.captures,
                &collected.body,
                type_table,
            );
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

            return_abi: crate::tir::ReturnAbi::default(),
        }
    }

    /// Phase 2.5: when a function takes a `fn(A) -> B` parameter and is
    /// called with a closure literal, generate a specialised callee whose
    /// fn-type parameters are the functor struct types and whose
    /// `IndirectCall`s become direct `MethodCall`s on `__call`.
    fn generate_fn_param_specializations(
        &mut self,
        func_refs: &[Rc<RefCell<TirFunction>>],
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
        // Record per-local metadata for the translator and apply the
        // type rewrite to the param / local decls.
        let mut spec_locals: Vec<SpecializedLocal> = Vec::new();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let slot = (*arg_idx + param_offset) as usize;
            let functor_ref_type = type_table.make_ref(functor_type);
            // Capture the original `fn(...)` type before we overwrite
            // the local's declared type — the translator needs it as
            // `target_fn_type` when re-wrapping a fn-param `Local`
            // into `NirExprKind::ClosureToCanonical` at a Call/MethodCall
            // arg slot that still expects `fn(...)`.
            let original_fn_type = new_locals
                .get(slot)
                .map(|l| l.type_id)
                .or_else(|| new_params.get(slot).map(|p| p.type_id))
                .unwrap_or(functor_ref_type);
            if slot < new_params.len() {
                new_params[slot].type_id = functor_ref_type;
            }
            if slot < new_locals.len() {
                new_locals[slot].type_id = functor_ref_type;
            }
            if let Some(functor) = self
                .functor_infos
                .iter()
                .find(|f| f.struct_type_id == functor_type)
            {
                spec_locals.push(SpecializedLocal {
                    local_index: slot as u32,
                    functor_id: functor.id,
                    functor_ref_type,
                    original_fn_type,
                });
            }
        }

        // The body is cloned as-is. The TIR → NIR translator handles
        // the per-function rewrites (`Local` retag, `IndirectCall` →
        // `MethodCall` on `__call`, fn-param-`Local` arg-slot wrap in
        // `NirExprKind::ClosureToCanonical`) by consulting
        // `ClosurePlan::specialized_locals` keyed on
        // `(self.module_source, specialized_name)`.
        let new_body = callee.body.clone();

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

            return_abi: crate::tir::ReturnAbi::default(),
        };

        self.generated_functions
            .push(Rc::new(RefCell::new(specialized_func)));
        if !spec_locals.is_empty() {
            self.specialized_locals.insert(
                (self.module_source.clone(), specialized_name.clone()),
                spec_locals,
            );
        }
        specialized_name
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

/// Phase 2 visitor: decide which closures are safe to specialise.
///
/// A closure is **safe to specialise** when its value never escapes its
/// declaring local: every use is either a direct call (`local(args)` /
/// `local.method(args)`) or a redirect-eligible trait dispatch on it.
/// Otherwise the value must travel as a `CanonicalClosure_K` so it can be
/// stored in struct fields, passed as a function argument, returned, or
/// assigned to globals — all positions where a specialised
/// `__Closure_N` would be incompatible with the receiver's canonical fn
/// type.
///
/// The escape decision is made by tracking `in_callee_position`: only the
/// callee of `IndirectCall` / receiver of `MethodCall` / func of `Call` is
/// a non-escape position. Every other slot (struct literal field, return
/// value, global assignment, let-rebinding, …) is an escape, and finding a
/// closure-bearing `Local` or a `Closure` literal there marks it
/// non-specialisable.
///
/// `Let { value: Closure }` is special-cased: the literal is what binds
/// the local, not an escape. Its body is walked under a fresh
/// `local_to_closure` (closure bodies have their own local-index namespace).
struct ClosureSafetyAnalyzer<'a> {
    local_to_closure: &'a mut IndexMap<u32, u32>,
    specializable: &'a mut IndexSet<u32>,
    /// True only when the current expression is the callee of a call
    /// (or receiver of a method call). Default false.
    in_callee_position: bool,
}

impl TirRefVisitor for ClosureSafetyAnalyzer<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        // Let-binding a closure literal: mark the local as carrying that
        // closure, mark the closure specialisable, then walk the body
        // (which owns a fresh local-index namespace).
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let TirExprKind::Closure {
                body,
                functor_id: Some(closure_id),
                ..
            } = &value.kind
        {
            self.local_to_closure.insert(*local_index, *closure_id);
            self.specializable.insert(*closure_id);
            let saved_l2c = std::mem::take(self.local_to_closure);
            let prev = std::mem::replace(&mut self.in_callee_position, false);
            self.visit_expr(body);
            self.in_callee_position = prev;
            *self.local_to_closure = saved_l2c;
            return;
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Closure {
                body,
                functor_id: Some(closure_id),
                ..
            } => {
                // A closure literal that isn't the immediate value of a
                // `let` (handled in visit_stmt) escapes unless it's the
                // direct callee of a call.
                if !self.in_callee_position {
                    self.specializable.swap_remove(closure_id);
                }
                let saved_l2c = std::mem::take(self.local_to_closure);
                let prev = std::mem::replace(&mut self.in_callee_position, false);
                self.visit_expr(body);
                self.in_callee_position = prev;
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
                if !self.in_callee_position
                    && let Some(closure_id) = self.local_to_closure.get(index)
                {
                    self.specializable.swap_remove(closure_id);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                let prev = self.in_callee_position;
                self.in_callee_position = true;
                self.visit_expr(callee);
                self.in_callee_position = false;
                for arg in args {
                    self.visit_expr(arg);
                }
                self.in_callee_position = prev;
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                let prev = self.in_callee_position;
                self.in_callee_position = true;
                self.visit_expr(receiver);
                self.in_callee_position = false;
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
                self.in_callee_position = prev;
            }
            TirExprKind::Unary { expr: inner, .. } => {
                // `&local` / `&mut local` / `*expr` in callee/receiver
                // position is still callee/receiver — the unary is just a
                // reference-projection of the same value. Template-string
                // expansion routinely wraps the receiver in `Unary::Ref`,
                // so resetting here would demote every literal-site
                // `{f:?}` / `{f:#?}` to canonical and defeat the
                // specialised `__Closure_N` path.
                self.visit_expr(inner);
            }
            _ => {
                // All other positions are escape positions: reset
                // in_callee_position to false and walk recursively.
                let prev = std::mem::replace(&mut self.in_callee_position, false);
                self.walk_expr(expr);
                self.in_callee_position = prev;
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

    /// When a `MethodCall` resolves to `Fn<N, Ret>^Inspect`,
    /// `^InspectAlt`, `^Display`, or `^DisplayAlt` with a receiver that
    /// is a specialised closure local — either let-bound by the closure
    /// safety analyser, or a fn-param specialised parameter
    /// (`generate_fn_param_specializations` rewrites the param type to
    /// `&__Closure_N`) — redirect the call to the per-functor
    /// `__Closure_N^Inspect / InspectAlt` impl. `Display` / `DisplayAlt`
    /// auto-derived fallbacks delegate to `Inspect` / `InspectAlt`
    /// respectively, so all four trait-method shapes terminate at the
    /// per-functor impl on the specialised path. Skipping the redirect
    /// for `Display`/`DisplayAlt` would let the value reach the
    /// canonical-vtable dispatch stub through the fallback's
    /// `self.Inspect::inspect(f)` body, where `ref.cast self to
    /// $canonical_inspectable_base` traps because the receiver is a
    /// specialised `&__Closure_N`, not a canonical struct.
    ///
    /// Cross-module note: when the closure was defined in module A and
    /// the redirect fires inside a body that was inlined / specialised
    /// into module B, the per-functor impl still lives in module A
    /// (where the closure literal was lowered). The rewritten
    /// `FunctionRef` uses the functor's `module_source`, not the
    /// surrounding body's module.
    ///
    /// Detection: `local_to_closure` is seeded by the safety analyser
    /// for let-bound closures and by `seed_local_to_closure_from_params`
    /// at body-walk start for fn-param specialised parameters. A
    /// receiver that doesn't peel down to a local (or peels to one not
    /// in the map, i.e. a canonicalised value) falls through to the
    /// generic dispatch stub.
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
        // Map each formatting trait to the per-functor impl that
        // produces the right output. `Display`/`DisplayAlt` fall back
        // to `Inspect`/`InspectAlt` so the redirect targets are the
        // same per-functor method either way.
        let (target_trait, target_method) = match base_trait {
            "Inspect" | "Display" => ("Inspect", "inspect"),
            "InspectAlt" | "DisplayAlt" => ("InspectAlt", "inspect_alt"),
            _ => return,
        };

        let local_idx = match peel_ref_to_local(receiver) {
            Some(idx) => idx,
            None => return,
        };
        let Some(closure_id) = self.local_to_closure.get(&local_idx).copied() else {
            return;
        };
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
            Some(target_trait.to_string()),
            target_method.to_string(),
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
            // Use the functor's *defining* module, not the surrounding
            // body's module: the per-functor `__Closure_N^Inspect[Alt]`
            // impl was synthesised alongside the closure literal in
            // `lower::plan::closure::ClosureLowerer::lower_module`, so it
            // lives in `functor.module_source`. After cross-module
            // inlining (`generate_fn_param_specializations` clones a
            // helper into another module) the body may be visited from
            // a different module, but the impl itself doesn't move.
            module_source: functor.module_source.clone(),
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

// `SpecializerTransformer` removed: the per-function rewrites it used
// to apply (`Local` retag, `IndirectCall` → `MethodCall`, fn-param
// `Local` arg-slot wrap in `ClosureToCanonical`) are now performed by
// the TIR → NIR translator, which reads
// `ClosurePlan::specialized_locals` per converted function.
