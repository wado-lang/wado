//! TIR → NIR translator: a single fold consuming a [`FlatPackage`]
//! plus a [`LowerPlan`], producing a [`NirPackage`].
//!
//! Every expression-shape rewrite (value-copy wraps, boxing, closure
//! literals, specialised-callee plumbing, wide-int `Match`) lives in
//! this fold. Pattern lowering and string-literal collection run
//! over each function body before the fold proper.
//!
//! See `docs/wep-2026-05-11-nir.md`.

pub(super) mod pattern;
mod wide_int;

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::{LowerPlan, closure, value_copy};
use crate::name::{LocalMethodName, MethodName};
use crate::nir;
use crate::nir::{
    NirCapture, NirEnum, NirEnumCase, NirField, NirFlags, NirFlagsMember, NirFunction, NirGlobal,
    NirImport, NirLiteralPattern, NirLocal, NirParam, NirStruct, NirTest, NirTypeParam,
    NirVariantCase, NirVariantDecl,
};
use crate::nir_arena::{
    ArenaCallArg, ArenaStructField, ArenaStructPatternField, ArmData, BlockId, BlockNode, Body,
    ExprBody, ExprId, ExprKind, ExprNode, PatId, PatKind, PatNode, StmtId, StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir;
use crate::tir::{
    CallArg, ClosureFunctor, FunctionRef, MonomorphInfo, TirBlock, TirCapture, TirEnum,
    TirEnumCase, TirExpr, TirExprKind, TirField, TirFlags, TirFlagsMember, TirFunction, TirGlobal,
    TirImport, TirLiteralPattern, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirStructField, TirStructPatternField, TirTest, TirTypeParam,
    TirUnaryOp, TirVariantCase, TirVariantDecl, TypeTable,
};
use crate::token::Span;

/// Translate a [`FlatPackage`] (TIR-shaped) into a [`NirPackage`] (NIR-shaped).
///
/// Takes ownership of `flat` so owned containers move straight into
/// the `NirPackage`. The closure-functor conversion looks up the
/// fresh `NirFunction` `Rc` in `func_map` so the optimizer's
/// `Rc::ptr_eq`-based closure-type DCE pass keeps matching.
pub fn translate(flat: FlatPackage, plan: LowerPlan) -> NirPackage {
    let LowerPlan {
        box_plan,
        closure,
        value_copy,
    } = plan;
    // Pattern lowering runs before string collection: it synthesises
    // string-literal expressions (string-literal pattern guards) the
    // data section must register.
    {
        let pattern = pattern::Lowering::new(&flat);
        let type_table = flat.type_table.borrow();
        for func_rc in &flat.functions {
            pattern.lower_function(&mut func_rc.borrow_mut(), &type_table);
        }
    }
    let strings = crate::lower::plan::string::plan(&flat);
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
        cm_interface_registry,
        world_registry,
        used_wasi_functions,
        strip_names,
        codegen_flags,
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

    // For `try_expand_deref_struct_assign`.
    let mut struct_fields_map: IndexMap<
        (String, crate::module_source::ModuleSource),
        Vec<crate::tir::TirField>,
    > = IndexMap::default();
    for s in &structs {
        struct_fields_map.insert((s.name.clone(), s.module_source.clone()), s.fields.clone());
    }
    let translator = Translator {
        box_plan: &box_plan,
        value_copy: &value_copy,
        closure: &closure,
        type_table: Rc::clone(&type_table),
        struct_fields_map,
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
        cm_interface_registry,
        world_registry,
        used_wasi_functions,
        strip_names,
        codegen_flags,
        // Conservative default; `optimize` overrides per opt level.
        string_inline_max_bytes: NirPackage::DEFAULT_STRING_INLINE_MAX_BYTES,
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
    box_plan: &'a crate::lower::plan::boxing::BoxPlan,
    value_copy: &'a value_copy::ValueCopyPlan,
    closure: &'a closure::ClosurePlan,
    type_table: Rc<RefCell<TypeTable>>,
    struct_fields_map:
        IndexMap<(String, crate::module_source::ModuleSource), Vec<crate::tir::TirField>>,
}

/// Per-function translation context. Created fresh for each function
/// the translator walks (top-level functions, generated `__call`
/// methods, fn-param specialized callees).
struct FunctionTranslator<'a, 'p> {
    base: &'a Translator<'p>,
    /// `Some` only inside a synthesized fn-param-specialized callee.
    specialized: Option<&'p [closure::SpecializedLocal]>,
    /// `None` for global initializers / struct field defaults, where
    /// `alloc_local` would have no function to attach to.
    extra: Option<ExtraLocals>,
    immutable_locals: IndexSet<u32>,
    address_taken: IndexSet<u32>,
    /// The arena every converter pushes nodes into. `convert_function` takes it
    /// (`into_inner`) as the function's `Body`; `convert_global` wraps the
    /// initializer it builds into a single-statement global-init `Body`.
    arena: RefCell<Body>,
}

/// `alloc_local` issues indices at `base_count + locals.len()` so
/// they don't collide with the function's existing locals.
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
        let immutable_locals = func
            .locals
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_mut)
            .map(|(i, _)| u32::try_from(i).unwrap())
            .collect();
        let address_taken = func.address_taken_locals.clone();
        Self {
            base,
            specialized,
            extra: Some(ExtraLocals {
                base_count: func.local_count,
                locals: RefCell::new(Vec::new()),
            }),
            immutable_locals,
            address_taken,
            arena: RefCell::new(Body::empty()),
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
            immutable_locals: IndexSet::default(),
            address_taken: IndexSet::default(),
            arena: RefCell::new(Body::empty()),
        }
    }

    /// Push an expression node into the arena, returning its stable id.
    fn alloc_expr(&self, kind: ExprKind, type_id: tir::TypeId, span: Span) -> ExprId {
        self.arena.borrow_mut().exprs.push(ExprNode {
            kind,
            type_id,
            span,
        })
    }

    /// Push a statement node into the arena.
    fn alloc_stmt(&self, kind: StmtKind, span: Span) -> StmtId {
        self.arena.borrow_mut().stmts.push(StmtNode { kind, span })
    }

    /// Push a block node into the arena.
    fn alloc_block(&self, stmts: Vec<StmtId>, span: Span) -> BlockId {
        self.arena
            .borrow_mut()
            .blocks
            .push(BlockNode { stmts, span })
    }

    /// Push a pattern node into the arena. Patterns carry no span of their own
    /// (the tree form had none), so the arena reuses the default span — keeping
    /// the result identical to the tree → arena lowering it replaces.
    fn alloc_pat(&self, kind: PatKind) -> PatId {
        self.arena.borrow_mut().pats.push(PatNode {
            kind,
            span: Span::default(),
        })
    }

    /// The span of an already-built expression node.
    fn expr_span(&self, id: ExprId) -> Span {
        self.arena.borrow().exprs[id].span
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
        // `local_count` below. The converters push straight into
        // `fctx.arena`; the root block id is taken as the `Body`'s root.
        let root = func.body.as_ref().map(|b| fctx.convert_block(b));
        let params = func.params.iter().map(|p| fctx.convert_param(p)).collect();
        let extra_locals = fctx.take_extra_locals();
        let local_count = func.local_count + u32::try_from(extra_locals.len()).unwrap();
        let mut locals: Vec<NirLocal> = func.locals.iter().map(convert_local).collect();
        locals.extend(extra_locals.iter().map(convert_local));
        let body = root.map(move |r| {
            let mut arena = fctx.arena.into_inner();
            arena.root = r;
            arena
        });
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
            compiler_item: func.compiler_item,
            export_name: func.export_name.clone(),
            allocator_tag: func.allocator_tag.clone(),
            kind: convert_function_kind(&func.kind),
            return_abi: convert_return_abi(&func.return_abi),
        }
    }

    fn convert_global(&self, global: &TirGlobal) -> NirGlobal {
        let fctx = FunctionTranslator::for_top_level(self);
        // Build the initializer directly into the arena, wrapped in a
        // single-`Expr`-statement block — the canonical global-init `Body`
        // shape the optimizer and `wir_build` read via `Body::sole_expr`.
        let span = global.initializer.span;
        let init_id = fctx.convert_expr(&global.initializer);
        let init_stmt = fctx.alloc_stmt(StmtKind::Expr(init_id), span);
        let init_root = fctx.alloc_block(vec![init_stmt], span);
        let initializer = ExprBody::from_body({
            let mut body = fctx.arena.into_inner();
            body.root = init_root;
            body
        });
        NirGlobal {
            name: global.name.clone(),
            ty: global.ty,
            initializer,
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
        // The functor's `call_method` is normally shared with a
        // top-level function (`Rc::ptr_eq` keyed); reuse the already-
        // converted `Rc` so the optimizer's closure-type DCE still
        // matches. The fallback covers methods that never made the
        // top-level list (e.g. inline-only).
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
    fn should_wrap_value_copy(&self, value: &TirExpr) -> bool {
        value_copy::analyze::should_wrap(value, &self.base.type_table.borrow())
    }

    /// Apply a boxing-derived rewrite to `expr`, returning `Some` if
    /// one fires. Matches raw TIR shapes — see the per-case helpers
    /// for the four rewrites (`Local` retag, `&local` collapse, `&x`
    /// wrap, `*box` projection).
    fn try_boxing_rewrite(&self, expr: &TirExpr) -> Option<ExprId> {
        match &expr.kind {
            TirExprKind::Local { index, name } if self.address_taken.contains(index) => {
                let original_type = expr.type_id;
                let box_type_id = *self.base.box_plan.box_struct_types.get(&original_type)?;
                let local_expr = self.alloc_expr(
                    ExprKind::Local {
                        index: *index,
                        name: name.clone(),
                    },
                    box_type_id,
                    expr.span,
                );
                Some(self.alloc_expr(
                    ExprKind::FieldAccess {
                        expr: local_expr,
                        field_index: 0,
                        field_name: "value".to_string(),
                    },
                    original_type,
                    expr.span,
                ))
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: inner,
            } => self.try_boxing_ref(inner, expr.span),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => self.try_boxing_deref(inner, expr.type_id, expr.span),
            _ => None,
        }
    }

    fn try_boxing_ref(&self, inner: &TirExpr, span: Span) -> Option<ExprId> {
        // `&local` of an address-taken primitive: the Box IS the
        // address, so collapse to the Local without re-wrapping.
        if let TirExprKind::Local { index, name } = &inner.kind
            && self.address_taken.contains(index)
            && let Some(&box_type_id) = self.base.box_plan.box_struct_types.get(&inner.type_id)
        {
            return Some(self.alloc_expr(
                ExprKind::Local {
                    index: *index,
                    name: name.clone(),
                },
                box_type_id,
                span,
            ));
        }
        // `&primitive_expr` → fresh `Box<T> { value: expr }`.
        let inner_nir = self.convert_expr(inner);
        Some(self.wrap_in_box(
            inner_nir,
            *self.base.box_plan.box_struct_types.get(&inner.type_id)?,
        ))
    }

    fn wrap_in_box(&self, value: ExprId, box_type: tir::TypeId) -> ExprId {
        let span = self.expr_span(value);
        let box_struct_name = if let crate::tir::ResolvedType::Struct { name, .. } =
            self.base.type_table.borrow().get(box_type)
        {
            name.clone()
        } else {
            panic!("Box type should be a struct");
        };
        self.alloc_expr(
            ExprKind::StructLiteral {
                struct_type: box_type,
                struct_name: box_struct_name,
                fields: vec![ArenaStructField {
                    name: "value".to_string(),
                    value,
                    field_index: 0,
                }],
            },
            box_type,
            span,
        )
    }

    /// Expand `*ref_to_struct = value` (non-Box ref) into field-by-
    /// field assignments through two fresh temp locals. Box-shaped
    /// deref-assigns lower as a single statement via the regular
    /// `try_boxing_rewrite` `Deref` arm.
    fn try_expand_deref_struct_assign(&self, stmt: &TirStmt) -> Option<Vec<StmtId>> {
        let TirStmtKind::Expr(expr) = &stmt.kind else {
            return None;
        };
        let TirExprKind::Assign { target, value } = &expr.kind else {
            return None;
        };
        let TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: ref_expr,
        } = &target.kind
        else {
            return None;
        };

        // Box-typed Deref-Assigns are handled by the regular fold path
        // (Unary(Deref, Box) → FieldAccess(.value)). Only non-Box struct
        // refs reach the expansion.
        let ref_type_id = ref_expr.type_id;
        if self.base.box_plan.box_type_ids.contains(&ref_type_id) {
            return None;
        }
        let inner_type_id = match self.base.type_table.borrow().get(ref_type_id) {
            crate::tir::ResolvedType::MutRef(inner) => *inner,
            _ => return None,
        };

        let (struct_name, struct_module) = if let crate::tir::ResolvedType::Struct {
            name,
            module_source,
            ..
        } = self.base.type_table.borrow().get(inner_type_id)
        {
            (name.clone(), module_source.clone())
        } else {
            return None;
        };
        let fields = self
            .base
            .struct_fields_map
            .get(&(struct_name, struct_module))?
            .clone();
        if fields.is_empty() {
            return None;
        }

        let span = expr.span;
        let ref_idx = self.alloc_local(ref_type_id, "__deref_ref".to_string());
        let val_idx = self.alloc_local(inner_type_id, "__deref_val".to_string());

        let ref_nir = self.convert_expr(ref_expr);
        let val_nir = self.convert_expr(value);

        let mut out: Vec<StmtId> = Vec::with_capacity(2 + fields.len());
        out.push(self.alloc_stmt(
            StmtKind::Let {
                name: format!("__deref_ref_{ref_idx}"),
                local_index: ref_idx,
                is_mut: false,
                is_reactive: false,
                type_id: ref_type_id,
                value: ref_nir,
                // Translator-synthesized binding — never a user-visible
                // defensive copy. See the equivalent flag on the
                // wide-int rewrite's `__wide_scrut` binding.
                skip_value_copy: true,
            },
            span,
        ));
        out.push(self.alloc_stmt(
            StmtKind::Let {
                name: format!("__deref_val_{val_idx}"),
                local_index: val_idx,
                is_mut: false,
                is_reactive: false,
                type_id: inner_type_id,
                value: val_nir,
                skip_value_copy: true,
            },
            span,
        ));
        for field in &fields {
            let ref_local = self.alloc_expr(
                ExprKind::Local {
                    index: ref_idx,
                    name: format!("__deref_ref_{ref_idx}"),
                },
                ref_type_id,
                span,
            );
            let val_local = self.alloc_expr(
                ExprKind::Local {
                    index: val_idx,
                    name: format!("__deref_val_{val_idx}"),
                },
                inner_type_id,
                span,
            );
            let target_field = self.alloc_expr(
                ExprKind::FieldAccess {
                    expr: ref_local,
                    field_index: field.index,
                    field_name: field.name.clone(),
                },
                field.type_id,
                span,
            );
            let value_field = self.alloc_expr(
                ExprKind::FieldAccess {
                    expr: val_local,
                    field_index: field.index,
                    field_name: field.name.clone(),
                },
                field.type_id,
                span,
            );
            let assign = self.alloc_expr(
                ExprKind::Assign {
                    target: target_field,
                    value: value_field,
                },
                field.type_id,
                span,
            );
            out.push(self.alloc_stmt(StmtKind::Expr(assign), span));
        }
        Some(out)
    }

    fn try_boxing_deref(
        &self,
        inner: &TirExpr,
        outer_type_id: tir::TypeId,
        span: Span,
    ) -> Option<ExprId> {
        let inner_type_id = inner.type_id;
        if !self.base.box_plan.box_type_ids.contains(&inner_type_id) {
            // For non-box refs (struct refs, etc.) `Deref` is a
            // transparent no-op at the Wasm level — leave the NIR
            // `Unary(Deref)` in place via the default path.
            return None;
        }
        let result_type = self
            .base
            .box_plan
            .get_box_inner_type(inner_type_id)
            .unwrap_or(outer_type_id);
        let inner_nir = self.convert_expr(inner);
        Some(self.alloc_expr(
            ExprKind::FieldAccess {
                expr: inner_nir,
                field_index: 0,
                field_name: "value".to_string(),
            },
            result_type,
            span,
        ))
    }

    /// Emit a call to the `$value_copy$T(...)` helper. Returns the
    /// value unchanged when the helper is not registered — this
    /// mirrors the pre-Phase-A silent fall-through, where
    /// `value_copy::insert` only wrapped at sites it walked
    /// (pattern-lowered / deref-expansion / wide-int `Let`s are
    /// synthesised after that walk, so they were never wrapped).
    fn wrap_value_copy(&self, value: ExprId, type_id: tir::TypeId) -> ExprId {
        let span = self.expr_span(value);
        let Some((helper_module, helper_name)) = self.base.value_copy.name_for_type.get(&type_id)
        else {
            return value;
        };
        self.alloc_expr(
            ExprKind::Call {
                func: nir::FunctionRef {
                    module_source: helper_module.clone(),
                    name: helper_name.clone(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: vec![ArenaCallArg {
                    expr: value,
                    is_mut: false,
                }],
            },
            type_id,
            span,
        )
    }

    fn convert_block(&self, block: &TirBlock) -> BlockId {
        let mut stmts: Vec<StmtId> = Vec::with_capacity(block.stmts.len());
        for s in &block.stmts {
            if let Some(expanded) = self.try_expand_deref_struct_assign(s) {
                stmts.extend(expanded);
            } else {
                stmts.push(self.convert_stmt(s));
            }
        }
        self.alloc_block(stmts, block.span)
    }

    fn convert_stmt(&self, stmt: &TirStmt) -> StmtId {
        let kind = self.convert_stmt_kind(&stmt.kind);
        self.alloc_stmt(kind, stmt.span)
    }

    fn convert_stmt_kind(&self, kind: &TirStmtKind) -> StmtKind {
        match kind {
            TirStmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
                skip_value_copy,
            } => {
                // Address-taken primitive locals are box-typed at the
                // declaration site (via `shadow_params`); retype the
                // Let to match and wrap its initial value. Mutually
                // exclusive with the value-copy wrap below: primitives
                // are not value-semantic.
                let (effective_type, box_wrap_type) = if self.address_taken.contains(local_index)
                    && let Some(&box_type) = self.base.box_plan.box_struct_types.get(type_id)
                {
                    (box_type, Some(box_type))
                } else {
                    (*type_id, None)
                };
                let needs_value_copy_wrap = box_wrap_type.is_none()
                    && !*skip_value_copy
                    && (*is_mut
                        || !value_copy::analyze::is_source_immutable(
                            value,
                            &self.immutable_locals,
                        ))
                    && self.should_wrap_value_copy(value);
                let value_nir = self.convert_expr(value);
                let value_nir = if let Some(box_type) = box_wrap_type {
                    self.wrap_in_box(value_nir, box_type)
                } else if needs_value_copy_wrap {
                    self.wrap_value_copy(value_nir, *type_id)
                } else {
                    value_nir
                };
                StmtKind::Let {
                    name: name.clone(),
                    local_index: *local_index,
                    is_mut: *is_mut,
                    is_reactive: *is_reactive,
                    type_id: effective_type,
                    value: value_nir,
                    skip_value_copy: *skip_value_copy,
                }
            }
            TirStmtKind::Expr(expr) => StmtKind::Expr(self.convert_expr(expr)),
            TirStmtKind::Return { value } => StmtKind::Return {
                value: value.as_ref().map(|v| self.convert_expr(v)),
            },
            TirStmtKind::TaskReturn { .. } => unreachable!(
                "TirStmtKind::TaskReturn should be eliminated by synthesis::cm_binding before lower::translate runs"
            ),
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => StmtKind::If {
                condition: self.convert_expr(condition),
                then_block: self.convert_block(then_block),
                else_block: else_block.as_ref().map(|b| self.convert_block(b)),
            },
            TirStmtKind::Loop { body } => StmtKind::Loop {
                body: self.convert_block(body),
            },
            TirStmtKind::Break { label, value } => StmtKind::Break {
                label: label.clone(),
                value: value.as_ref().map(|v| self.convert_expr(v)),
            },
            TirStmtKind::Continue => StmtKind::Continue,
            TirStmtKind::LabeledBlock { label, block } => StmtKind::LabeledBlock {
                label: label.clone(),
                block: self.convert_block(block),
            },
            TirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => {
                let needs_wrap = self.should_wrap_value_copy(value);
                let value_type = value.type_id;
                let value_nir = self.convert_expr(value);
                let value_nir = if needs_wrap {
                    self.wrap_value_copy(value_nir, value_type)
                } else {
                    value_nir
                };
                StmtKind::LetDestructure {
                    pattern: self.convert_pattern(pattern),
                    is_mut: *is_mut,
                    value: value_nir,
                }
            }
            TirStmtKind::VariadicForOf { .. } => unreachable!(
                "TirStmtKind::VariadicForOf should be expanded by monomorphize before lower::translate runs"
            ),
        }
    }

    fn convert_expr(&self, expr: &TirExpr) -> ExprId {
        // Wide-int (`i128` / `u128`) `match` → if-else chain.
        if let TirExprKind::Match {
            expr: scrutinee,
            arms,
        } = &expr.kind
            && wide_int::should_rewrite(scrutinee.type_id, arms, &self.base.type_table.borrow())
        {
            // Hoist into a fresh local; `build_if_chain` clones the
            // scrutinee into each arm's `Eq::eq` condition, so a
            // side-effecting expression would re-run per arm.
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
                    // Translator-synthesized; invisible to
                    // `value_copy::analyze`'s seed walker, so any
                    // wrap would look up a helper that was never
                    // registered.
                    skip_value_copy: true,
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
        // `Closure` → raw `StructLiteral` (specialisable) or
        // `ClosureToCanonical` wrap (otherwise). The body is never
        // recursed into; it lives in the synthesized `__call`
        // method which `convert_function` walks separately.
        if let TirExprKind::Closure {
            functor_id: Some(closure_id),
            captures,
            ..
        } = &expr.kind
            && let Some(functor) = self.base.closure.functor_infos.get(*closure_id as usize)
        {
            let nir_struct = self.alloc_expr(
                ExprKind::StructLiteral {
                    struct_type: functor.struct_type_id,
                    struct_name: functor.struct_name.clone(),
                    fields: self.build_arena_capture_fields(captures, expr.span),
                },
                functor.ref_type_id,
                expr.span,
            );
            if self.base.closure.specializable.contains(closure_id) {
                return nir_struct;
            }
            return self.alloc_expr(
                ExprKind::ClosureToCanonical {
                    functor: nir_struct,
                    functor_id: *closure_id,
                    target_fn_type: expr.type_id,
                    closure_module: functor.module_source.clone(),
                },
                expr.type_id,
                expr.span,
            );
        }
        // Inside a synthesized fn-param-specialized callee body, a
        // `Local` read of one of the specialized params surfaces in
        // NIR as a `Local` retagged to the functor `&__Closure_N`
        // type (mirrors the in-place rewrite the old
        // `SpecializerTransformer` applied).
        if let TirExprKind::Local { index, name } = &expr.kind
            && let Some(spec) = self.specialized_for_local(*index)
        {
            return self.alloc_expr(
                ExprKind::Local {
                    index: *index,
                    name: name.clone(),
                },
                spec.functor_ref_type,
                expr.span,
            );
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
            let call_method_name = MethodName::format_local(
                &functor.struct_name,
                None,
                crate::name::CLOSURE_CALL_METHOD,
            );
            let call_method_info = LocalMethodName::new(
                functor.struct_name.clone(),
                None,
                crate::name::CLOSURE_CALL_METHOD.to_string(),
            );
            let call_method_borrow = functor.call_method.borrow();
            let params_is_mut: Vec<bool> = call_method_borrow
                .params
                .iter()
                .skip(1)
                .map(|p| p.is_mut)
                .collect();
            let nir_args: Vec<ArenaCallArg> = args
                .iter()
                .zip(params_is_mut.into_iter().chain(std::iter::repeat(false)))
                .map(|(arg, is_mut)| ArenaCallArg {
                    expr: self.convert_expr(arg),
                    is_mut,
                })
                .collect();
            return self.alloc_expr(
                ExprKind::MethodCall {
                    receiver: nir_receiver,
                    func: nir::FunctionRef {
                        module_source: functor.module_source.clone(),
                        name: call_method_name,
                        monomorph_info: None,
                        method_info: Some(call_method_info),
                    },
                    type_args: Vec::new(),
                    args: nir_args,
                },
                expr.type_id,
                expr.span,
            );
        }
        if let Some(nir) = self.try_boxing_rewrite(expr) {
            return nir;
        }
        let kind = self.convert_expr_kind(&expr.kind);
        self.alloc_expr(kind, expr.type_id, expr.span)
    }

    /// Convert a `Call` / `MethodCall` argument. When the argument is
    /// a specialized fn-param `Local` and the slot still expects
    /// `fn(...)`, wrap the converted `Local` in
    /// `ExprKind::ClosureToCanonical` so the callee sees the
    /// original function-shaped view.
    fn convert_specialized_arg_expr(&self, arg: &TirExpr) -> ExprId {
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
            return self.alloc_expr(
                ExprKind::ClosureToCanonical {
                    functor: inner,
                    functor_id: spec.functor_id,
                    target_fn_type: spec.original_fn_type,
                    closure_module: functor.module_source.clone(),
                },
                spec.original_fn_type,
                arg.span,
            );
        }
        self.convert_expr(arg)
    }

    fn convert_expr_kind(&self, kind: &TirExprKind) -> ExprKind {
        match kind {
            TirExprKind::IntLiteral { value, repr } => ExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            TirExprKind::FloatLiteral { value, repr } => ExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            TirExprKind::BoolLiteral(b) => ExprKind::BoolLiteral(*b),
            TirExprKind::CharLiteral(c) => ExprKind::CharLiteral(*c),
            TirExprKind::StringLiteral(s) => ExprKind::StringLiteral(s.clone()),
            TirExprKind::BytesLiteral(b) => ExprKind::BytesLiteral(b.clone()),
            TirExprKind::Null => ExprKind::Null,
            TirExprKind::Unit => ExprKind::Unit,
            TirExprKind::Local { index, name } => ExprKind::Local {
                index: *index,
                name: name.clone(),
            },
            TirExprKind::FuncRef { .. } => unreachable!(
                "TirExprKind::FuncRef should be wrapped in a Closure by lower::closure before lower::translate runs"
            ),
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => ExprKind::GlobalVarGet {
                module_source: module_source.clone(),
                name: name.clone(),
            },
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => ExprKind::GlobalVarSet {
                module_source: module_source.clone(),
                name: name.clone(),
                value: self.convert_expr(value),
            },
            TirExprKind::Binary { left, op, right } => ExprKind::Binary {
                left: self.convert_expr(left),
                op: convert_binary_op(*op),
                right: self.convert_expr(right),
            },
            TirExprKind::Unary { op, expr } => ExprKind::Unary {
                op: convert_unary_op(*op),
                expr: self.convert_expr(expr),
            },
            TirExprKind::Assign { target, value } => {
                // Only `Local` targets receive a defensive copy.
                // `FieldAccess` / `Index` writes mutate an existing
                // aggregate slot — the WIR-side semantics let the
                // reference flow through without an extra wrap.
                let needs_wrap = matches!(&target.kind, TirExprKind::Local { .. })
                    && self.should_wrap_value_copy(value);
                let value_type = value.type_id;
                let value_nir = self.convert_expr(value);
                let value_nir = if needs_wrap {
                    self.wrap_value_copy(value_nir, value_type)
                } else {
                    value_nir
                };
                ExprKind::Assign {
                    target: self.convert_expr(target),
                    value: value_nir,
                }
            }
            TirExprKind::Cast { expr, target_type } => ExprKind::Cast {
                expr: self.convert_expr(expr),
                target_type: *target_type,
            },
            TirExprKind::Call {
                func,
                type_args,
                args,
            } => self.convert_call(func, type_args, args),
            TirExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name: local_name.clone(),
                args: args.iter().map(|a| self.convert_expr(a)).collect(),
            },
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => ExprKind::MethodCall {
                receiver: self.convert_expr(receiver),
                func: convert_function_ref(func),
                type_args: type_args.clone(),
                args: args.iter().map(|a| self.convert_call_arg(a)).collect(),
            },
            TirExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => ExprKind::FieldAccess {
                expr: self.convert_expr(expr),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            TirExprKind::Index { expr, index } => ExprKind::Index {
                expr: self.convert_expr(expr),
                index: self.convert_expr(index),
            },
            TirExprKind::Block(block) => ExprKind::Block(self.convert_block(block)),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: self.convert_expr(condition),
                then_branch: self.convert_block(then_branch),
                else_branch: else_branch.as_ref().map(|b| self.convert_block(b)),
            },
            TirExprKind::Match { expr, arms } => ExprKind::Match {
                expr: self.convert_expr(expr),
                arms: arms.iter().map(|a| self.convert_match_arm(a)).collect(),
            },
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => ExprKind::StructLiteral {
                struct_type: *struct_type,
                struct_name: struct_name.clone(),
                fields: fields
                    .iter()
                    .map(|f| self.convert_struct_field(f))
                    .collect(),
            },
            TirExprKind::TupleLiteral { elements } => ExprKind::TupleLiteral {
                elements: elements.iter().map(|e| self.convert_expr(e)).collect(),
            },
            TirExprKind::TupleSpread { .. } => unreachable!(
                "TirExprKind::TupleSpread should be expanded by monomorphize before lower::translate runs"
            ),
            TirExprKind::TupleZip { .. } => unreachable!(
                "TirExprKind::TupleZip should be expanded by monomorphize before lower::translate runs"
            ),
            TirExprKind::TupleLen { .. } => unreachable!(
                "TirExprKind::TupleLen should be expanded by monomorphize before lower::translate runs"
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
            // `ExprKind::ClosureToCanonical`). Falling through to
            // this arm means we hit a `Closure` node without a
            // `functor_id` assigned by `lower::plan::closure`, or with
            // a `functor_id` not present in `ClosurePlan::functor_infos`
            // — both indicate the closure planner missed this node.
            TirExprKind::Closure { .. } => unreachable!(
                "TirExprKind::Closure reached lower::translate without a functor_id assigned by lower::plan::closure"
            ),
            TirExprKind::IndirectCall { callee, args } => ExprKind::IndirectCall {
                callee: self.convert_expr(callee),
                // Indirect-call args take an unconditional defensive
                // copy when the value semantics require it: the callee
                // signature is opaque here, so the wrap predicate is
                // applied to every arg regardless of an `is_mut`
                // marker.
                args: args
                    .iter()
                    .map(|a| {
                        let needs_wrap = self.should_wrap_value_copy(a);
                        let nir = self.convert_expr(a);
                        if needs_wrap {
                            self.wrap_value_copy(nir, a.type_id)
                        } else {
                            nir
                        }
                    })
                    .collect(),
            },
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => ExprKind::VariantConstruct {
                variant_type: *variant_type,
                case_index: *case_index,
                case_name: case_name.clone(),
                payload: payload.as_ref().map(|p| self.convert_expr(p)),
            },
            TirExprKind::EnumConstruct {
                enum_type,
                case_index,
                case_name,
            } => ExprKind::EnumConstruct {
                enum_type: *enum_type,
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => ExprKind::LabeledBlock {
                label: label.clone(),
                block: self.convert_block(block),
                result_type: *result_type,
            },
            TirExprKind::VariantTag { expr } => ExprKind::VariantTag {
                expr: self.convert_expr(expr),
            },
            TirExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => ExprKind::VariantTest {
                expr: self.convert_expr(expr),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TirExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => ExprKind::VariantPayload {
                expr: self.convert_expr(expr),
                case_index: *case_index,
                payload_type: *payload_type,
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

    /// Rewrites `builtin::copy_value::<T>(x)` markers, which only
    /// appear inside synthesized helper bodies (user-program TIR
    /// carries no markers — the fold emits the helper call directly
    /// at each wrap site via [`Self::wrap_value_copy`]).
    fn convert_call(
        &self,
        func: &FunctionRef,
        type_args: &[tir::TypeId],
        args: &[CallArg],
    ) -> ExprKind {
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
            return ExprKind::Call {
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
        ExprKind::Call {
            func: convert_function_ref(func),
            type_args: type_args.to_vec(),
            args: args.iter().map(|a| self.convert_call_arg(a)).collect(),
        }
    }

    fn convert_pattern(&self, pattern: &TirPattern) -> PatId {
        let kind = match pattern {
            TirPattern::Wildcard => PatKind::Wildcard,
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => PatKind::Binding {
                name: name.clone(),
                local_index: *local_index,
                type_id: *type_id,
            },
            TirPattern::Literal(lit) => PatKind::Literal(convert_literal_pattern(lit)),
            TirPattern::Tuple(patterns, has_rest) => PatKind::Tuple(
                patterns.iter().map(|p| self.convert_pattern(p)).collect(),
                *has_rest,
            ),
            TirPattern::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => PatKind::Variant {
                enum_type: *enum_type,
                variant_name: variant_name.clone(),
                bindings: bindings.iter().map(|p| self.convert_pattern(p)).collect(),
                payload_type: *payload_type,
            },
            TirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => PatKind::Enum {
                enum_type: *enum_type,
                case_name: case_name.clone(),
                case_index: *case_index,
            },
            TirPattern::Struct {
                struct_type,
                fields,
                has_rest,
            } => PatKind::Struct {
                struct_type: *struct_type,
                fields: fields
                    .iter()
                    .map(|f| self.convert_struct_pattern_field(f))
                    .collect(),
                has_rest: *has_rest,
            },
            TirPattern::Or(patterns) => {
                PatKind::Or(patterns.iter().map(|p| self.convert_pattern(p)).collect())
            }
            TirPattern::ConstantValue { expr } => PatKind::ConstantValue {
                expr: self.convert_expr(expr),
            },
            TirPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => PatKind::Range {
                start: *start,
                end: *end,
                inclusive: *inclusive,
                is_unsigned: *is_unsigned,
            },
        };
        self.alloc_pat(kind)
    }

    fn convert_struct_pattern_field(
        &self,
        field: &TirStructPatternField,
    ) -> ArenaStructPatternField {
        ArenaStructPatternField {
            field_name: field.field_name.clone(),
            field_index: field.field_index,
            pattern: self.convert_pattern(&field.pattern),
        }
    }

    fn convert_match_arm(&self, arm: &TirMatchArm) -> ArmData {
        // Match the tree → arena lowering's child order (pattern, guard,
        // body) so node ids land identically to the path this replaces.
        let pattern = self.convert_pattern(&arm.pattern);
        let guard = arm.guard.as_ref().map(|g| self.convert_expr(g));
        let body = self.convert_expr(&arm.body);
        ArmData {
            pattern,
            guard,
            body,
            span: arm.span,
        }
    }

    fn convert_struct_field(&self, field: &TirStructField) -> ArenaStructField {
        ArenaStructField {
            name: field.name.clone(),
            value: self.convert_expr(&field.value),
            field_index: field.field_index,
        }
    }

    /// Build arena struct-field values for a closure's captures. Each field is
    /// a `Local` reading the captured value from the outer scope at
    /// `cap.outer_index`. Mirrors the TIR-side `build_capture_fields` that the
    /// closure planner uses for specialized closures at `Let` bindings.
    fn build_arena_capture_fields(
        &self,
        captures: &[TirCapture],
        span: Span,
    ) -> Vec<ArenaStructField> {
        captures
            .iter()
            .enumerate()
            .map(|(i, cap)| {
                let value = self.alloc_expr(
                    ExprKind::Local {
                        index: cap.outer_index,
                        name: cap.name.clone(),
                    },
                    cap.type_id,
                    span,
                );
                ArenaStructField {
                    name: format!("__capture_{i}"),
                    value,
                    field_index: i as u32,
                }
            })
            .collect()
    }

    fn convert_call_arg(&self, arg: &CallArg) -> ArenaCallArg {
        // `is_mut` value-semantic args get a defensive
        // `$value_copy$T` wrap; specialised-callee fn-param `Local`
        // args get a `ClosureToCanonical` wrap. They don't interact:
        // the value-copy predicate matches on the raw TIR, the
        // specialised wrap on the converted NIR (always non-value-
        // semantic).
        let needs_value_copy = arg.is_mut && self.should_wrap_value_copy(&arg.expr);
        let value_type = arg.expr.type_id;
        let converted = self.convert_specialized_arg_expr(&arg.expr);
        let expr = if needs_value_copy {
            self.wrap_value_copy(converted, value_type)
        } else {
            converted
        };
        ArenaCallArg {
            expr,
            is_mut: arg.is_mut,
        }
    }

    fn convert_field(&self, field: &TirField) -> NirField {
        // NIR carries no field default: defaults are resolved into struct
        // literals by the elaborator before lowering, so the NIR copy was
        // write-only.
        NirField {
            name: field.name.clone(),
            is_pub: field.is_pub,
            type_id: field.type_id,
            index: field.index,
            span: field.span,
            is_hidden: field.is_hidden,
            serde_rename: field.serde_rename.clone(),
            serde_default: field.serde_default,
        }
    }

    fn convert_param(&self, param: &TirParam) -> NirParam {
        // NIR carries no param default: defaults are resolved into arguments
        // at call sites by the elaborator before lowering, so the NIR copy
        // was write-only.
        NirParam {
            name: param.name.clone(),
            type_id: param.type_id,
            local_index: param.local_index,
            is_mut: param.is_mut,
            span: param.span,
        }
    }
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
