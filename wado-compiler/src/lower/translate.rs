//! TIR → NIR translator: a single fold that consumes a [`FlatPackage`]
//! together with a [`LowerPlan`] and produces a [`NirPackage`].
//!
//! Pre-lower-only TIR variants (`Closure`, `Capture`, `FuncRef`, `IfLet`,
//! `TupleSpread`, …) must have been eliminated by earlier lower
//! sub-passes before the translator runs — they `unreachable!()` here.
//!
//! Some translator arms consume facts from `plan` to rewrite TIR
//! markers into resolved NIR forms (the value-copy marker rewrite in
//! the `Call` arm; the `i128` / `u128` match → if-else chain in the
//! `Match` arm).
//!
//! See `docs/wep-2026-05-11-nir.md`.

mod wide_int;

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::lower::plan::{LowerPlan, closure, value_copy};
use crate::name::{LocalMethodName, MethodName};
use crate::nir;
use crate::nir::{
    NirBlock, NirCapture, NirEnum, NirEnumCase, NirExpr, NirExprKind, NirField, NirFlags,
    NirFlagsMember, NirFunction, NirGlobal, NirImport, NirLiteralPattern, NirLocal, NirMatchArm,
    NirParam, NirPattern, NirStmt, NirStmtKind, NirStruct, NirStructField, NirStructPatternField,
    NirTest, NirTypeParam, NirVariantCase, NirVariantDecl,
};
use crate::nir_package::NirPackage;
use crate::tir;
use crate::tir::{
    CallArg, ClosureFunctor, FunctionRef, MonomorphInfo, TirBlock, TirCapture, TirEnum,
    TirEnumCase, TirExpr, TirExprKind, TirField, TirFlags, TirFlagsMember, TirFunction, TirGlobal,
    TirImport, TirLiteralPattern, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirStructField, TirStructPatternField, TirTest, TirTypeParam,
    TirVariantCase, TirVariantDecl, TypeTable,
};

/// Translate a [`FlatPackage`] (TIR-shaped) into a [`NirPackage`] (NIR-shaped).
///
/// Takes ownership of `flat` so owned containers (`Vec`s, `IndexMap`s,
/// `String`s, `BuiltinRegistry`, the trait env, …) move straight into the
/// `NirPackage` instead of being cloned. Body-shape Vecs (`structs`,
/// `enums`, …) iterate by reference because the per-element conversion
/// helpers take `&T` and clone the few owned `String` fields they keep —
/// a per-helper rewrite to fully-move plumbing is out of scope here.
/// Function `Rc`s are destructured via `.borrow().clone()` because each
/// one is shared with one or more `ClosureFunctor::call_method`; the
/// closure-functor conversion looks up the fresh `NirFunction` `Rc` in
/// `func_map` so the optimizer's `Rc::ptr_eq`-based closure-type DCE
/// pass keeps matching.
pub fn translate(flat: FlatPackage, plan: LowerPlan) -> NirPackage {
    let LowerPlan {
        closure,
        value_copy,
        strings,
    } = plan;
    let FlatPackage {
        entry_module_source,
        type_table,
        functions,
        structs,
        enums,
        variants,
        variant_index,
        flags,
        globals,
        imports,
        tests,
        wasm_module_sources,
        module_name,
        wasi_registry,
        world_registry,
        used_wasi_functions,
        strip_names,
        skip_validation,
        target_world,
        has_http_handler_export,
        export_binding_names,
        component_plan,
        builtin_registry,
        task_return_flat_params,
        wasm_assets,
        trait_env,
    } = flat;

    let translator = Translator {
        value_copy: &value_copy,
        closure: &closure,
        type_table: Rc::clone(&type_table),
    };

    let mut func_map: IndexMap<*const RefCell<TirFunction>, Rc<RefCell<NirFunction>>> =
        IndexMap::with_capacity_and_hasher(functions.len(), rustc_hash::FxBuildHasher);
    let functions: Vec<Rc<RefCell<NirFunction>>> = functions
        .into_iter()
        .map(|func_rc| {
            let ptr = Rc::as_ptr(&func_rc);
            let nir_rc = Rc::new(RefCell::new(translator.convert_function(&func_rc.borrow())));
            func_map.insert(ptr, Rc::clone(&nir_rc));
            nir_rc
        })
        .collect();
    NirPackage {
        entry_module_source,
        type_table,
        functions,
        structs: structs
            .iter()
            .map(|s| translator.convert_struct(s))
            .collect(),
        enums: enums.iter().map(convert_enum).collect(),
        variants: variants.iter().map(convert_variant_decl).collect(),
        variant_index,
        flags: flags.iter().map(convert_flags).collect(),
        globals: globals
            .iter()
            .map(|g| translator.convert_global(g))
            .collect(),
        imports: imports.iter().map(convert_import).collect(),
        tests: tests.iter().map(convert_test).collect(),
        string_literals: strings.string_literals,
        bytes_literals: strings.bytes_literals,
        closure_functors: closure
            .functor_infos
            .iter()
            .map(|cf| translator.convert_closure_functor(cf, &func_map))
            .collect(),
        function_strings: strings.function_strings,
        function_method_info: strings.function_method_info,
        wasm_module_sources,
        module_name,
        wasi_registry,
        world_registry,
        used_wasi_functions,
        strip_names,
        skip_validation,
        target_world,
        has_http_handler_export,
        export_binding_names,
        component_plan,
        builtin_registry,
        task_return_flat_params,
        wasm_assets,
        trait_env,
    }
}

struct Translator<'a> {
    value_copy: &'a value_copy::ValueCopyPlan,
    closure: &'a closure::ClosurePlan,
    /// Shared with the consumed `FlatPackage`; used by translator arms
    /// that need to inspect resolved types during the walk (e.g.
    /// detecting wide-int `Match` scrutinees for the if-else rewrite).
    type_table: Rc<RefCell<TypeTable>>,
}

/// Per-function translation context. Created fresh for each function
/// the translator walks (top-level functions, generated `__call`
/// methods, fn-param specialized callees). Holds the per-function
/// state that the per-arm rewrites need; the previous `Cell`-based
/// shared slot on `Translator` is gone.
struct FunctionTranslator<'a, 'p> {
    base: &'a Translator<'p>,
    /// Specialized-locals entries for this function if it is a
    /// synthesized fn-param-specialized callee, otherwise `None`. Read
    /// by `Local` retag, `IndirectCall` → `MethodCall`, and
    /// fn-param-`Local` arg-slot wrap.
    specialized: Option<&'p [closure::SpecializedLocal]>,
    /// Locals to append onto the resulting `NirFunction`. The wide-int
    /// `Match` rewrite uses this to hoist side-effecting scrutinees
    /// into a fresh slot. `None` for `for_top_level` contexts that
    /// don't own a function's local list.
    extra: Option<ExtraLocals>,
}

/// Per-function local extension state. Indices issued by `alloc_local`
/// are `base_count + extra.len()`, so they don't collide with the
/// function's existing locals.
struct ExtraLocals {
    base_count: u32,
    locals: RefCell<Vec<TirLocal>>,
}

impl<'a, 'p> FunctionTranslator<'a, 'p> {
    fn new(base: &'a Translator<'p>, func: &TirFunction) -> Self {
        let key = (func.module_source.clone(), func.name.clone());
        let specialized = base
            .closure
            .specialized_locals
            .get(&key)
            .map(std::vec::Vec::as_slice);
        Self {
            base,
            specialized,
            extra: Some(ExtraLocals {
                base_count: func.local_count,
                locals: RefCell::new(Vec::new()),
            }),
        }
    }

    /// A function context for places where no function is being
    /// translated (global initializers, struct field defaults).
    /// `specialized` is `None`; per-function arms degrade to their
    /// non-specialized path. `alloc_local` panics here because the
    /// only caller (wide-int `Match` rewrite) is only reachable
    /// inside a function body.
    fn for_top_level(base: &'a Translator<'p>) -> Self {
        Self {
            base,
            specialized: None,
            extra: None,
        }
    }

    fn specialized_for_local(&self, local_index: u32) -> Option<&'p closure::SpecializedLocal> {
        self.specialized?
            .iter()
            .find(|s| s.local_index == local_index)
    }

    /// Allocate a fresh local slot, register a `TirLocal` for it, and
    /// return the new index. Must be called from within a function
    /// translation; panics for top-level contexts.
    fn alloc_local(&self, type_id: tir::TypeId, name: String) -> u32 {
        let extra = self
            .extra
            .as_ref()
            .expect("alloc_local outside function translation");
        let mut locals = extra.locals.borrow_mut();
        let index = extra.base_count + u32::try_from(locals.len()).unwrap();
        locals.push(TirLocal {
            name,
            type_id,
            is_mut: false,
        });
        index
    }

    /// Drain any locals allocated during the walk so the caller can
    /// append them to the output `NirFunction`'s local list.
    fn take_extra_locals(&self) -> Vec<TirLocal> {
        match &self.extra {
            Some(extra) => std::mem::take(&mut *extra.locals.borrow_mut()),
            None => Vec::new(),
        }
    }
}

impl Translator<'_> {
    fn convert_function(&self, func: &TirFunction) -> NirFunction {
        let fctx = FunctionTranslator::new(self, func);
        // Walk the body first so any locals allocated by per-arm
        // rewrites (currently only the wide-int `Match` scrutinee
        // hoist) are visible when we materialize `locals` /
        // `local_count` below.
        let body = func.body.as_ref().map(|b| fctx.convert_block(b));
        let params = func.params.iter().map(|p| fctx.convert_param(p)).collect();
        let extra_locals = fctx.take_extra_locals();
        let local_count = func.local_count + u32::try_from(extra_locals.len()).unwrap();
        let mut locals: Vec<NirLocal> = func.locals.iter().map(convert_local).collect();
        locals.extend(extra_locals.iter().map(convert_local));
        NirFunction {
            name: func.name.clone(),
            module_source: func.module_source.clone(),
            is_pub: func.is_pub,
            is_export: func.is_export,
            is_async: func.is_async,
            type_params: func.type_params.iter().map(convert_type_param).collect(),
            impl_type_params: func
                .impl_type_params
                .iter()
                .map(convert_type_param)
                .collect(),
            monomorph_info: func.monomorph_info.as_ref().map(convert_monomorph_info),
            method_info: func.method_info.clone(),
            params,
            return_type: func.return_type,
            task_return_type: func.task_return_type,
            effects: func.effects.clone(),
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count,
            locals,
            address_taken_locals: func.address_taken_locals.clone(),
            stores_aliased_locals: func.stores_aliased_locals.clone(),
            is_cm_binding: func.is_cm_binding,
            is_dispatch_wrapper: func.is_dispatch_wrapper,
            is_cm_export: func.is_cm_export,
            is_ambient: func.is_ambient,
            inline_hint: convert_inline_hint(func.inline_hint),
            comp_features: func.comp_features,
            export_name: func.export_name.clone(),
            allocator_tag: func.allocator_tag.clone(),
            kind: convert_function_kind(&func.kind),
            return_abi: convert_return_abi(&func.return_abi),
        }
    }

    fn convert_global(&self, global: &TirGlobal) -> NirGlobal {
        let fctx = FunctionTranslator::for_top_level(self);
        NirGlobal {
            name: global.name.clone(),
            ty: global.ty,
            initializer: fctx.convert_expr(&global.initializer),
            mutable: global.mutable,
            wado_mutable: global.wado_mutable,
            is_pub: global.is_pub,
            module_source: global.module_source.clone(),
            span: global.span,
            is_nullable: global.is_nullable,
            lazy_init: global.lazy_init,
            locals: global.locals.iter().map(convert_local).collect(),
        }
    }

    fn convert_struct(&self, s: &TirStruct) -> NirStruct {
        let fctx = FunctionTranslator::for_top_level(self);
        NirStruct {
            name: s.name.clone(),
            module_source: s.module_source.clone(),
            is_pub: s.is_pub,
            type_params: s.type_params.iter().map(convert_type_param).collect(),
            monomorph_info: s.monomorph_info.as_ref().map(convert_monomorph_info),
            fields: s.fields.iter().map(|f| fctx.convert_field(f)).collect(),
            span: s.span,
            serde_rename_all: s.serde_rename_all.clone(),
        }
    }

    fn convert_closure_functor(
        &self,
        cf: &ClosureFunctor,
        func_map: &IndexMap<*const RefCell<TirFunction>, Rc<RefCell<NirFunction>>>,
    ) -> nir::ClosureFunctor {
        // Reuse the converted function `Rc` when the functor's `call_method`
        // shares its allocation with one of the package's top-level functions
        // (the common case). Fall back to a fresh conversion only if the
        // functor's call_method is not present in the package's function list.
        let call_method = func_map
            .get(&Rc::as_ptr(&cf.call_method))
            .cloned()
            .unwrap_or_else(|| {
                Rc::new(RefCell::new(
                    self.convert_function(&cf.call_method.borrow()),
                ))
            });
        nir::ClosureFunctor {
            module_source: cf.module_source.clone(),
            id: cf.id,
            struct_name: cf.struct_name.clone(),
            struct_type_id: cf.struct_type_id,
            ref_type_id: cf.ref_type_id,
            call_method,
            captures: cf.captures.iter().map(convert_capture).collect(),
            canonical_user_params: cf.canonical_user_params.clone(),
            canonical_return: cf.canonical_return,
        }
    }
}

impl FunctionTranslator<'_, '_> {
    fn convert_block(&self, block: &TirBlock) -> NirBlock {
        NirBlock {
            stmts: block.stmts.iter().map(|s| self.convert_stmt(s)).collect(),
            span: block.span,
        }
    }

    fn convert_stmt(&self, stmt: &TirStmt) -> NirStmt {
        NirStmt {
            kind: self.convert_stmt_kind(&stmt.kind),
            span: stmt.span,
        }
    }

    fn convert_stmt_kind(&self, kind: &TirStmtKind) -> NirStmtKind {
        match kind {
            TirStmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
                skip_value_copy,
            } => NirStmtKind::Let {
                name: name.clone(),
                local_index: *local_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.convert_expr(value),
                skip_value_copy: *skip_value_copy,
            },
            TirStmtKind::Expr(expr) => NirStmtKind::Expr(self.convert_expr(expr)),
            TirStmtKind::Return { value } => NirStmtKind::Return {
                value: value.as_ref().map(|v| self.convert_expr(v)),
            },
            TirStmtKind::TaskReturn { .. } => unreachable!(
                "TirStmtKind::TaskReturn should be eliminated by synthesis::cm_binding before lower::translate runs"
            ),
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => NirStmtKind::If {
                condition: self.convert_expr(condition),
                then_block: self.convert_block(then_block),
                else_block: else_block.as_ref().map(|b| self.convert_block(b)),
            },
            TirStmtKind::Loop { body } => NirStmtKind::Loop {
                body: self.convert_block(body),
            },
            TirStmtKind::Break { label, value } => NirStmtKind::Break {
                label: label.clone(),
                value: value.as_ref().map(|v| self.convert_expr(v)),
            },
            TirStmtKind::Continue => NirStmtKind::Continue,
            TirStmtKind::LabeledBlock { label, block } => NirStmtKind::LabeledBlock {
                label: label.clone(),
                block: self.convert_block(block),
            },
            TirStmtKind::IfLet { .. } => unreachable!(
                "TirStmtKind::IfLet should be lowered by lower::pattern before lower::translate runs"
            ),
            TirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => NirStmtKind::LetDestructure {
                pattern: self.convert_pattern(pattern),
                is_mut: *is_mut,
                value: self.convert_expr(value),
            },
            TirStmtKind::VariadicForOf { .. } => unreachable!(
                "TirStmtKind::VariadicForOf should be expanded by monomorphize before lower::translate runs"
            ),
        }
    }

    fn convert_expr(&self, expr: &TirExpr) -> NirExpr {
        // Wide-int (`i128` / `u128`) match → if-else chain. Done before
        // dispatching `convert_expr_kind` so the rewrite can use the
        // outer expression's `type_id` / `span` for the synthesized
        // `If` nodes; arm bodies and nested wide-int matches are then
        // processed by the recursive `convert_expr` call.
        if let TirExprKind::Match {
            expr: scrutinee,
            arms,
        } = &expr.kind
            && wide_int::should_rewrite(scrutinee.type_id, arms, &self.base.type_table.borrow())
        {
            // Hoist the scrutinee into a fresh local so it evaluates
            // exactly once even though `build_if_chain` clones it into
            // every arm's `Eq::eq` condition (and the `Binding` arm's
            // `Let` value). Without this hoist a side-effecting
            // scrutinee like `match observe() { 10 => ..., 20 => ... }`
            // re-runs `observe()` per arm. `wir_build::pattern_match`
            // does the same hoist for normal `Match` expressions.
            let scrut_idx = self.alloc_local(scrutinee.type_id, "__wide_scrut".to_string());
            let scrut_local = TirExpr::new(
                TirExprKind::Local {
                    index: scrut_idx,
                    name: "__wide_scrut".to_string(),
                },
                scrutinee.type_id,
                scrutinee.span,
            );
            let if_chain = wide_int::build_if_chain(
                &scrut_local,
                arms,
                expr.type_id,
                expr.span,
                &self.base.type_table,
            );
            let let_stmt = TirStmt::new(
                TirStmtKind::Let {
                    name: "__wide_scrut".to_string(),
                    local_index: scrut_idx,
                    is_mut: false,
                    is_reactive: false,
                    type_id: scrutinee.type_id,
                    value: (**scrutinee).clone(),
                    skip_value_copy: false,
                },
                scrutinee.span,
            );
            let payload_stmt = TirStmt::new(TirStmtKind::Expr(if_chain), expr.span);
            let block_expr = TirExpr::new(
                TirExprKind::Block(TirBlock {
                    stmts: vec![let_stmt, payload_stmt],
                    span: expr.span,
                }),
                expr.type_id,
                expr.span,
            );
            return self.convert_expr(&block_expr);
        }
        // Remaining `Closure` nodes (those the closure planner's
        // specialized call-site rewriter did not turn into a
        // `StructLiteral` at a `Let` binding) become
        // `NirExprKind::ClosureToCanonical` wrapping a `StructLiteral`
        // of the functor's captures. Do not recurse into the closure
        // body — the body lives in the generated `__call` method,
        // which `convert_function` walks separately.
        if let TirExprKind::Closure {
            functor_id: Some(closure_id),
            captures,
            ..
        } = &expr.kind
            && let Some(functor) = self.base.closure.functor_infos.get(*closure_id as usize)
        {
            let nir_struct = NirExpr {
                kind: NirExprKind::StructLiteral {
                    struct_type: functor.struct_type_id,
                    struct_name: functor.struct_name.clone(),
                    fields: build_nir_capture_fields(captures, expr.span),
                },
                type_id: functor.ref_type_id,
                span: expr.span,
            };
            return NirExpr {
                kind: NirExprKind::ClosureToCanonical {
                    functor: Box::new(nir_struct),
                    functor_id: *closure_id,
                    target_fn_type: expr.type_id,
                    closure_module: functor.module_source.clone(),
                },
                type_id: expr.type_id,
                span: expr.span,
            };
        }
        // Inside a synthesized fn-param-specialized callee body, a
        // `Local` read of one of the specialized params surfaces in
        // NIR as a `Local` retagged to the functor `&__Closure_N`
        // type (mirrors the in-place rewrite the old
        // `SpecializerTransformer` applied).
        if let TirExprKind::Local { index, name } = &expr.kind
            && let Some(spec) = self.specialized_for_local(*index)
        {
            return NirExpr {
                kind: NirExprKind::Local {
                    index: *index,
                    name: name.clone(),
                },
                type_id: spec.functor_ref_type,
                span: expr.span,
            };
        }
        // `IndirectCall` whose callee resolves to a specialized
        // fn-param `Local` is dispatched directly to the functor's
        // `__call` method.
        if let TirExprKind::IndirectCall { callee, args } = &expr.kind
            && let TirExprKind::Local { index, .. } = &callee.kind
            && let Some(spec) = self.specialized_for_local(*index)
            && let Some(functor) = self
                .base
                .closure
                .functor_infos
                .get(spec.functor_id as usize)
        {
            let nir_receiver = self.convert_expr(callee);
            let call_method_name = MethodName::format_local(&functor.struct_name, None, "__call");
            let call_method_info =
                LocalMethodName::new(functor.struct_name.clone(), None, "__call".to_string());
            let call_method_borrow = functor.call_method.borrow();
            let params_is_mut: Vec<bool> = call_method_borrow
                .params
                .iter()
                .skip(1)
                .map(|p| p.is_mut)
                .collect();
            let nir_args: Vec<nir::CallArg> = args
                .iter()
                .zip(params_is_mut.into_iter().chain(std::iter::repeat(false)))
                .map(|(arg, is_mut)| nir::CallArg {
                    expr: self.convert_expr(arg),
                    is_mut,
                })
                .collect();
            return NirExpr {
                kind: NirExprKind::method_call(
                    Box::new(nir_receiver),
                    nir::FunctionRef {
                        module_source: functor.module_source.clone(),
                        name: call_method_name,
                        monomorph_info: None,
                        method_info: Some(call_method_info),
                    },
                    Vec::new(),
                    nir_args,
                ),
                type_id: expr.type_id,
                span: expr.span,
            };
        }
        NirExpr {
            kind: self.convert_expr_kind(&expr.kind),
            type_id: expr.type_id,
            span: expr.span,
        }
    }

    /// Convert a `Call` / `MethodCall` argument. When the argument is
    /// a specialized fn-param `Local` and the slot still expects
    /// `fn(...)`, wrap the converted `Local` in
    /// `NirExprKind::ClosureToCanonical` so the callee sees the
    /// original function-shaped view.
    fn convert_specialized_arg_expr(&self, arg: &TirExpr) -> NirExpr {
        if let TirExprKind::Local { index, .. } = &arg.kind
            && let Some(spec) = self.specialized_for_local(*index)
            && matches!(
                self.base.type_table.borrow().get(spec.original_fn_type),
                tir::ResolvedType::Function { .. }
            )
            && let Some(functor) = self
                .base
                .closure
                .functor_infos
                .get(spec.functor_id as usize)
        {
            let inner = self.convert_expr(arg);
            return NirExpr {
                kind: NirExprKind::ClosureToCanonical {
                    functor: Box::new(inner),
                    functor_id: spec.functor_id,
                    target_fn_type: spec.original_fn_type,
                    closure_module: functor.module_source.clone(),
                },
                type_id: spec.original_fn_type,
                span: arg.span,
            };
        }
        self.convert_expr(arg)
    }

    fn convert_expr_kind(&self, kind: &TirExprKind) -> NirExprKind {
        match kind {
            TirExprKind::IntLiteral { value, repr } => NirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            TirExprKind::FloatLiteral { value, repr } => NirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            TirExprKind::BoolLiteral(b) => NirExprKind::BoolLiteral(*b),
            TirExprKind::CharLiteral(c) => NirExprKind::CharLiteral(*c),
            TirExprKind::StringLiteral(s) => NirExprKind::StringLiteral(s.clone()),
            TirExprKind::BytesLiteral(b) => NirExprKind::BytesLiteral(b.clone()),
            TirExprKind::Null => NirExprKind::Null,
            TirExprKind::Unit => NirExprKind::Unit,
            TirExprKind::Local { index, name } => NirExprKind::Local {
                index: *index,
                name: name.clone(),
            },
            TirExprKind::FuncRef { .. } => unreachable!(
                "TirExprKind::FuncRef should be wrapped in a Closure by lower::closure before lower::translate runs"
            ),
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => NirExprKind::GlobalVarGet {
                module_source: module_source.clone(),
                name: name.clone(),
            },
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => NirExprKind::GlobalVarSet {
                module_source: module_source.clone(),
                name: name.clone(),
                value: Box::new(self.convert_expr(value)),
            },
            TirExprKind::Binary { left, op, right } => NirExprKind::Binary {
                left: Box::new(self.convert_expr(left)),
                op: convert_binary_op(*op),
                right: Box::new(self.convert_expr(right)),
            },
            TirExprKind::Unary { op, expr } => NirExprKind::Unary {
                op: convert_unary_op(*op),
                expr: Box::new(self.convert_expr(expr)),
            },
            TirExprKind::Assign { target, value } => NirExprKind::Assign {
                target: Box::new(self.convert_expr(target)),
                value: Box::new(self.convert_expr(value)),
            },
            TirExprKind::Cast { expr, target_type } => NirExprKind::Cast {
                expr: Box::new(self.convert_expr(expr)),
                target_type: *target_type,
            },
            TirExprKind::Call {
                func,
                type_args,
                args,
            } => self.convert_call(func, type_args, args),
            TirExprKind::CmRawCall { local_name, args } => NirExprKind::CmRawCall {
                local_name: local_name.clone(),
                args: args.iter().map(|a| self.convert_expr(a)).collect(),
            },
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => NirExprKind::method_call(
                Box::new(self.convert_expr(receiver)),
                convert_function_ref(func),
                type_args.clone(),
                args.iter().map(|a| self.convert_call_arg(a)).collect(),
            ),
            TirExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => NirExprKind::FieldAccess {
                expr: Box::new(self.convert_expr(expr)),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            TirExprKind::Index { expr, index } => NirExprKind::Index {
                expr: Box::new(self.convert_expr(expr)),
                index: Box::new(self.convert_expr(index)),
            },
            TirExprKind::Block(block) => NirExprKind::Block(self.convert_block(block)),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => NirExprKind::If {
                condition: Box::new(self.convert_expr(condition)),
                then_branch: self.convert_block(then_branch),
                else_branch: else_branch.as_ref().map(|b| self.convert_block(b)),
            },
            TirExprKind::Match { expr, arms } => NirExprKind::Match {
                expr: Box::new(self.convert_expr(expr)),
                arms: arms.iter().map(|a| self.convert_match_arm(a)).collect(),
            },
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => NirExprKind::StructLiteral {
                struct_type: *struct_type,
                struct_name: struct_name.clone(),
                fields: fields
                    .iter()
                    .map(|f| self.convert_struct_field(f))
                    .collect(),
            },
            TirExprKind::TupleLiteral { elements } => NirExprKind::TupleLiteral {
                elements: elements.iter().map(|e| self.convert_expr(e)).collect(),
            },
            TirExprKind::TupleSpread { .. } => unreachable!(
                "TirExprKind::TupleSpread should be expanded by monomorphize before lower::translate runs"
            ),
            TirExprKind::TupleZip { .. } => unreachable!(
                "TirExprKind::TupleZip should be expanded by monomorphize before lower::translate runs"
            ),
            TirExprKind::TypePackExpansion { .. } => unreachable!(
                "TirExprKind::TypePackExpansion should be expanded by monomorphize before lower::translate runs"
            ),
            TirExprKind::Capture { .. } => unreachable!(
                "TirExprKind::Capture should be lowered to FieldAccess by lower::plan::closure before lower::translate runs"
            ),
            // The translator handles closures intentionally just above
            // this `match` (see the `TirExprKind::Closure` arm at the
            // top of `convert_expr` that emits
            // `NirExprKind::ClosureToCanonical`). Falling through to
            // this arm means we hit a `Closure` node without a
            // `functor_id` assigned by `lower::plan::closure`, or with
            // a `functor_id` not present in `ClosurePlan::functor_infos`
            // — both indicate the closure planner missed this node.
            TirExprKind::Closure { .. } => unreachable!(
                "TirExprKind::Closure reached lower::translate without a functor_id assigned by lower::plan::closure"
            ),
            TirExprKind::IndirectCall { callee, args } => NirExprKind::IndirectCall {
                callee: Box::new(self.convert_expr(callee)),
                args: args.iter().map(|a| self.convert_expr(a)).collect(),
            },
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => NirExprKind::VariantConstruct {
                variant_type: *variant_type,
                case_index: *case_index,
                case_name: case_name.clone(),
                payload: payload.as_ref().map(|p| Box::new(self.convert_expr(p))),
            },
            TirExprKind::EnumConstruct {
                enum_type,
                case_index,
                case_name,
            } => NirExprKind::EnumConstruct {
                enum_type: *enum_type,
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => NirExprKind::LabeledBlock {
                label: label.clone(),
                block: self.convert_block(block),
                result_type: *result_type,
            },
            TirExprKind::VariantTag { expr } => NirExprKind::VariantTag {
                expr: Box::new(self.convert_expr(expr)),
            },
            TirExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => NirExprKind::VariantTest {
                expr: Box::new(self.convert_expr(expr)),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TirExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => NirExprKind::VariantPayload {
                expr: Box::new(self.convert_expr(expr)),
                case_index: *case_index,
                payload_type: *payload_type,
            },
            TirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => NirExprKind::Switch {
                scrutinee: Box::new(self.convert_expr(scrutinee)),
                min_value: *min_value,
                arms: arms.iter().map(|b| self.convert_block(b)).collect(),
                default: self.convert_block(default),
            },
            TirExprKind::TemplateString { .. } => unreachable!(
                "TirExprKind::TemplateString should be expanded by synthesis::template before lower::translate runs"
            ),
            TirExprKind::WithHandler { .. } => unreachable!(
                "TirExprKind::WithHandler should be desugared by synthesis::effect_dispatch before lower::translate runs"
            ),
            TirExprKind::Resume { .. } => unreachable!(
                "TirExprKind::Resume should be desugared by synthesis::effect_dispatch before lower::translate runs"
            ),
        }
    }

    /// Translate a TIR `Call`, rewriting `builtin::copy_value::<T>(x)`
    /// markers (placed by [`crate::lower::plan::value_copy::insert`]) into
    /// direct calls to the `$value_copy$T<id>` helper registered in
    /// [`crate::lower::plan::value_copy::synthesize`].
    fn convert_call(
        &self,
        func: &FunctionRef,
        type_args: &[tir::TypeId],
        args: &[CallArg],
    ) -> NirExprKind {
        if func.module_source.is_core_builtin()
            && func.name == "copy_value"
            && args.len() == 1
            && let Some(type_id) = func
                .monomorph_info
                .as_ref()
                .and_then(|mi| mi.impl_type_args.first().copied())
            && let Some((helper_module, helper_name)) =
                self.base.value_copy.name_for_type.get(&type_id)
        {
            return NirExprKind::Call {
                func: nir::FunctionRef {
                    module_source: helper_module.clone(),
                    name: helper_name.clone(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: args.iter().map(|a| self.convert_call_arg(a)).collect(),
            };
        }
        NirExprKind::Call {
            func: convert_function_ref(func),
            type_args: type_args.to_vec(),
            args: args.iter().map(|a| self.convert_call_arg(a)).collect(),
        }
    }

    fn convert_pattern(&self, pattern: &TirPattern) -> NirPattern {
        match pattern {
            TirPattern::Wildcard => NirPattern::Wildcard,
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => NirPattern::Binding {
                name: name.clone(),
                local_index: *local_index,
                type_id: *type_id,
            },
            TirPattern::Literal(lit) => NirPattern::Literal(convert_literal_pattern(lit)),
            TirPattern::Tuple(patterns, has_rest) => NirPattern::Tuple(
                patterns.iter().map(|p| self.convert_pattern(p)).collect(),
                *has_rest,
            ),
            TirPattern::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => NirPattern::Variant {
                enum_type: *enum_type,
                variant_name: variant_name.clone(),
                bindings: bindings.iter().map(|p| self.convert_pattern(p)).collect(),
                payload_type: *payload_type,
            },
            TirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => NirPattern::Enum {
                enum_type: *enum_type,
                case_name: case_name.clone(),
                case_index: *case_index,
            },
            TirPattern::Struct {
                struct_type,
                fields,
                has_rest,
            } => NirPattern::Struct {
                struct_type: *struct_type,
                fields: fields
                    .iter()
                    .map(|f| self.convert_struct_pattern_field(f))
                    .collect(),
                has_rest: *has_rest,
            },
            TirPattern::Or(patterns) => {
                NirPattern::Or(patterns.iter().map(|p| self.convert_pattern(p)).collect())
            }
            TirPattern::ConstantValue { expr } => NirPattern::ConstantValue {
                expr: Box::new(self.convert_expr(expr)),
            },
            TirPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => NirPattern::Range {
                start: *start,
                end: *end,
                inclusive: *inclusive,
                is_unsigned: *is_unsigned,
            },
        }
    }

    fn convert_struct_pattern_field(&self, field: &TirStructPatternField) -> NirStructPatternField {
        NirStructPatternField {
            field_name: field.field_name.clone(),
            field_index: field.field_index,
            pattern: self.convert_pattern(&field.pattern),
        }
    }

    fn convert_match_arm(&self, arm: &TirMatchArm) -> NirMatchArm {
        NirMatchArm {
            pattern: self.convert_pattern(&arm.pattern),
            guard: arm.guard.as_ref().map(|g| self.convert_expr(g)),
            body: self.convert_expr(&arm.body),
            span: arm.span,
        }
    }

    fn convert_struct_field(&self, field: &TirStructField) -> NirStructField {
        NirStructField {
            name: field.name.clone(),
            value: self.convert_expr(&field.value),
            field_index: field.field_index,
        }
    }

    fn convert_call_arg(&self, arg: &CallArg) -> nir::CallArg {
        // Specialized callee bodies need fn-param `Local` arg slots
        // (expected to be `fn(...)` at the callee) to be wrapped in
        // `ClosureToCanonical`.
        nir::CallArg {
            expr: self.convert_specialized_arg_expr(&arg.expr),
            is_mut: arg.is_mut,
        }
    }

    fn convert_field(&self, field: &TirField) -> NirField {
        NirField {
            name: field.name.clone(),
            is_pub: field.is_pub,
            type_id: field.type_id,
            index: field.index,
            span: field.span,
            is_hidden: field.is_hidden,
            serde_rename: field.serde_rename.clone(),
            serde_default: field.serde_default,
            default_expr: field
                .default_expr
                .as_ref()
                .map(|e| Box::new(self.convert_expr(e))),
        }
    }

    fn convert_param(&self, param: &TirParam) -> NirParam {
        NirParam {
            name: param.name.clone(),
            type_id: param.type_id,
            local_index: param.local_index,
            is_mut: param.is_mut,
            default_expr: param
                .default_expr
                .as_ref()
                .map(|e| Box::new(self.convert_expr(e))),
            span: param.span,
        }
    }
}

/// Build NIR struct-field values for a closure's captures. Each field
/// is a `Local` reading the captured value from the outer scope at
/// `cap.outer_index`. Mirrors the TIR-side `build_capture_fields` that
/// the closure planner uses for specialized closures at `Let`
/// bindings.
fn build_nir_capture_fields(
    captures: &[TirCapture],
    span: crate::token::Span,
) -> Vec<NirStructField> {
    captures
        .iter()
        .enumerate()
        .map(|(i, cap)| NirStructField {
            name: format!("__capture_{i}"),
            value: NirExpr {
                kind: NirExprKind::Local {
                    index: cap.outer_index,
                    name: cap.name.clone(),
                },
                type_id: cap.type_id,
                span,
            },
            field_index: i as u32,
        })
        .collect()
}

fn convert_test(test: &TirTest) -> NirTest {
    NirTest {
        name: test.name.clone(),
        function_name: test.function_name.clone(),
        line: test.line,
        span: test.span,
        expect_trap: test.expect_trap,
        is_todo: test.is_todo,
        timeout_ms: test.timeout_ms,
    }
}

fn convert_enum(e: &TirEnum) -> NirEnum {
    NirEnum {
        name: e.name.clone(),
        module_source: e.module_source.clone(),
        is_pub: e.is_pub,
        type_params: e.type_params.iter().map(convert_type_param).collect(),
        monomorph_info: e.monomorph_info.as_ref().map(convert_monomorph_info),
        cases: e.cases.iter().map(convert_enum_case).collect(),
        span: e.span,
    }
}

fn convert_flags(f: &TirFlags) -> NirFlags {
    NirFlags {
        name: f.name.clone(),
        module_source: f.module_source.clone(),
        is_pub: f.is_pub,
        type_id: f.type_id,
        members: f.members.iter().map(convert_flags_member).collect(),
        span: f.span,
    }
}

fn convert_variant_decl(v: &TirVariantDecl) -> NirVariantDecl {
    NirVariantDecl {
        name: v.name.clone(),
        module_source: v.module_source.clone(),
        is_pub: v.is_pub,
        type_params: v.type_params.iter().map(convert_type_param).collect(),
        cases: v.cases.iter().map(convert_variant_case).collect(),
        comp_features: v.comp_features,
        span: v.span,
    }
}

fn convert_import(i: &TirImport) -> NirImport {
    NirImport {
        namespace: i.namespace.clone(),
        canonical_name: i.canonical_name.clone(),
        func_name: i.func_name.clone(),
        params: i.params.clone(),
        return_type: i.return_type,
    }
}

fn convert_literal_pattern(lit: &TirLiteralPattern) -> NirLiteralPattern {
    match lit {
        TirLiteralPattern::I128(v) => NirLiteralPattern::I128(*v),
        TirLiteralPattern::U128(v) => NirLiteralPattern::U128(*v),
        TirLiteralPattern::Bool(b) => NirLiteralPattern::Bool(*b),
        TirLiteralPattern::Char(c) => NirLiteralPattern::Char(*c),
        TirLiteralPattern::String(s) => NirLiteralPattern::String(s.clone()),
        TirLiteralPattern::Null => NirLiteralPattern::Null,
    }
}

fn convert_capture(c: &TirCapture) -> NirCapture {
    NirCapture {
        name: c.name.clone(),
        outer_index: c.outer_index,
        type_id: c.type_id,
        is_mut: c.is_mut,
    }
}

fn convert_binary_op(op: tir::TirBinaryOp) -> nir::NirBinaryOp {
    match op {
        tir::TirBinaryOp::Add => nir::NirBinaryOp::Add,
        tir::TirBinaryOp::Sub => nir::NirBinaryOp::Sub,
        tir::TirBinaryOp::Mul => nir::NirBinaryOp::Mul,
        tir::TirBinaryOp::Div => nir::NirBinaryOp::Div,
        tir::TirBinaryOp::Mod => nir::NirBinaryOp::Mod,
        tir::TirBinaryOp::Eq => nir::NirBinaryOp::Eq,
        tir::TirBinaryOp::NotEq => nir::NirBinaryOp::NotEq,
        tir::TirBinaryOp::Lt => nir::NirBinaryOp::Lt,
        tir::TirBinaryOp::LtEq => nir::NirBinaryOp::LtEq,
        tir::TirBinaryOp::Gt => nir::NirBinaryOp::Gt,
        tir::TirBinaryOp::GtEq => nir::NirBinaryOp::GtEq,
        tir::TirBinaryOp::And => nir::NirBinaryOp::And,
        tir::TirBinaryOp::Or => nir::NirBinaryOp::Or,
        tir::TirBinaryOp::BitAnd => nir::NirBinaryOp::BitAnd,
        tir::TirBinaryOp::BitOr => nir::NirBinaryOp::BitOr,
        tir::TirBinaryOp::BitXor => nir::NirBinaryOp::BitXor,
        tir::TirBinaryOp::Shl => nir::NirBinaryOp::Shl,
        tir::TirBinaryOp::Shr => nir::NirBinaryOp::Shr,
        tir::TirBinaryOp::RefEq => nir::NirBinaryOp::RefEq,
        tir::TirBinaryOp::RefNotEq => nir::NirBinaryOp::RefNotEq,
    }
}

fn convert_unary_op(op: tir::TirUnaryOp) -> nir::NirUnaryOp {
    match op {
        tir::TirUnaryOp::Neg => nir::NirUnaryOp::Neg,
        tir::TirUnaryOp::Not => nir::NirUnaryOp::Not,
        tir::TirUnaryOp::BitNot => nir::NirUnaryOp::BitNot,
        tir::TirUnaryOp::Ref => nir::NirUnaryOp::Ref,
        tir::TirUnaryOp::MutRef => nir::NirUnaryOp::MutRef,
        tir::TirUnaryOp::Deref => nir::NirUnaryOp::Deref,
    }
}

fn convert_local(local: &TirLocal) -> NirLocal {
    NirLocal {
        name: local.name.clone(),
        type_id: local.type_id,
        is_mut: local.is_mut,
    }
}

fn convert_type_param(tp: &TirTypeParam) -> NirTypeParam {
    NirTypeParam {
        name: tp.name.clone(),
        is_effect: tp.is_effect,
        is_pack: tp.is_pack,
        bounds: tp.bounds.clone(),
        default: tp.default,
        index: tp.index,
    }
}

fn convert_monomorph_info(info: &MonomorphInfo) -> nir::MonomorphInfo {
    nir::MonomorphInfo {
        generic_name: info.generic_name.clone(),
        impl_type_args: info.impl_type_args.clone(),
        method_type_args: info.method_type_args.clone(),
        is_blanket: info.is_blanket,
    }
}

fn convert_function_ref(func: &FunctionRef) -> nir::FunctionRef {
    nir::FunctionRef {
        module_source: func.module_source.clone(),
        name: func.name.clone(),
        monomorph_info: func.monomorph_info.as_ref().map(convert_monomorph_info),
        method_info: func.method_info.clone(),
    }
}

fn convert_function_kind(kind: &tir::FunctionKind) -> nir::FunctionKind {
    match kind {
        tir::FunctionKind::Regular => nir::FunctionKind::Regular,
        tir::FunctionKind::ValueCopy { type_id } => {
            nir::FunctionKind::ValueCopy { type_id: *type_id }
        }
        tir::FunctionKind::FnCanonicalDispatch {
            trait_kind,
            arity,
            return_type,
        } => nir::FunctionKind::FnCanonicalDispatch {
            trait_kind: convert_fn_dispatch_trait(*trait_kind),
            arity: *arity,
            return_type: *return_type,
        },
    }
}

fn convert_inline_hint(hint: tir::InlineHint) -> nir::InlineHint {
    match hint {
        tir::InlineHint::Auto => nir::InlineHint::Auto,
        tir::InlineHint::Hint => nir::InlineHint::Hint,
        tir::InlineHint::Always => nir::InlineHint::Always,
        tir::InlineHint::Never => nir::InlineHint::Never,
    }
}

fn convert_return_abi(abi: &tir::ReturnAbi) -> nir::ReturnAbi {
    match abi {
        tir::ReturnAbi::Single => nir::ReturnAbi::Single,
        tir::ReturnAbi::MultiValue {
            result_types,
            field_names,
        } => nir::ReturnAbi::MultiValue {
            result_types: result_types.clone(),
            field_names: field_names.clone(),
        },
    }
}

fn convert_fn_dispatch_trait(kind: tir::FnDispatchTrait) -> nir::FnDispatchTrait {
    match kind {
        tir::FnDispatchTrait::Inspect => nir::FnDispatchTrait::Inspect,
        tir::FnDispatchTrait::InspectAlt => nir::FnDispatchTrait::InspectAlt,
    }
}

fn convert_enum_case(case: &TirEnumCase) -> NirEnumCase {
    NirEnumCase {
        name: case.name.clone(),
        index: case.index,
        span: case.span,
    }
}

fn convert_flags_member(m: &TirFlagsMember) -> NirFlagsMember {
    NirFlagsMember {
        name: m.name.clone(),
        bitmask: m.bitmask,
        span: m.span,
    }
}

fn convert_variant_case(case: &TirVariantCase) -> NirVariantCase {
    NirVariantCase {
        name: case.name.clone(),
        index: case.index,
        payload: case.payload,
        span: case.span,
    }
}
