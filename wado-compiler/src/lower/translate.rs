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
use crate::name::{FqTypeName, LocalMethodName, MethodName};
use cranelift_entity::EntityRef;

use crate::nir;
use crate::nir::{
    NirCapture, NirEnum, NirEnumCase, NirField, NirFlags, NirFlagsMember, NirFunction, NirGlobal,
    NirImport, NirLiteralPattern, NirLocal, NirParam, NirStruct, NirTest, NirTypeParam,
    NirVariantCase, NirVariantDecl,
};
use crate::nir_arena::{
    ArenaCallArg, ArenaStructField, ArenaStructPatternField, ArmData, BlockId, BlockNode, Body,
    ExprBody, ExprId, ExprKind, ExprNode, Operand, PatId, PatKind, PatNode, StmtId, StmtKind,
    StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir;
use crate::tir::{
    CallArg, ClosureFunctor, FunctionRef, GlobalInit, MonomorphInfo, TirBlock, TirCapture, TirEnum,
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
    // Lower the `assert_failed` marker before string planning, so a bare-asserts
    // build never collects the dropped diagnostic literals into the data section
    // (and a default build routes the marker back to a plain `panic`).
    crate::lower::bare_asserts::lower(&flat, flat.codegen_flags.bare_asserts);
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
        export_binding_names,
        component_plan,
        builtin_registry,
        task_return_flat_params,
        wasm_assets,
        trait_env,
        moved_local_spans,
    } = flat;

    // For `try_expand_deref_aggregate_assign`.
    let mut struct_fields_map: IndexMap<
        (String, crate::module_source::ModuleSource),
        Vec<crate::tir::TirField>,
    > = IndexMap::default();
    for s in &structs {
        struct_fields_map.insert((s.name.clone(), s.module_source.clone()), s.fields.clone());
    }
    // Pre-register every in-package function's canonical `FuncId` (its position,
    // 1:1 with the final function list). Calls stamp against this at construction.
    let mut ids: IndexMap<crate::name::FunctionId, crate::nir::FuncId> = IndexMap::default();
    for (i, func_rc) in functions.iter().enumerate() {
        let key = tir_function_key(&func_rc.borrow());
        let prev = ids.insert(key, crate::nir::FuncId::new(i));
        // Load-bearing: two functions sharing a canonical key would share a
        // FuncId (a miscompile). The check is O(1); keep it always-on.
        assert!(
            prev.is_none(),
            "duplicate canonical function key: function_id must be unique: {:?}",
            tir_function_key(&func_rc.borrow())
        );
    }
    let base_len = functions.len();
    let translator = Translator {
        box_plan: &box_plan,
        value_copy: &value_copy,
        closure: &closure,
        type_table: Rc::clone(&type_table),
        struct_fields_map,
        interner: RefCell::new(Interner {
            ids,
            stubs: Vec::new(),
            base_len,
            #[cfg(debug_assertions)]
            shadowed: Vec::new(),
        }),
        moved_local_spans,
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
    let mut nir = NirPackage {
        entry_module_source,
        type_table,
        functions,
        func_index: IndexMap::default(),
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
        export_binding_names,
        component_plan,
        builtin_registry,
        task_return_flat_params,
        wasm_assets,
        trait_env,
    };
    // Finalize the born-resolved callee ids: append the interned extern stubs,
    // set every function's `id` to its store position (`FuncId == position`), and
    // publish the reverse index. Replaces the former post-pass `assign_func_ids`.
    let Interner {
        ids,
        stubs,
        #[cfg(debug_assertions)]
        shadowed,
        ..
    } = translator.interner.into_inner();
    #[cfg(debug_assertions)]
    assert!(
        shadowed.is_empty(),
        "a call minted an extern stub for a name the package defines: {shadowed:#?}"
    );
    nir.functions.extend(stubs);
    for (i, func_rc) in nir.functions.iter().enumerate() {
        func_rc.borrow_mut().id = Some(crate::nir::FuncId::new(i));
    }
    nir.func_index = ids;
    nir
}

struct Translator<'a> {
    box_plan: &'a crate::lower::plan::boxing::BoxPlan,
    value_copy: &'a value_copy::ValueCopyPlan,
    closure: &'a closure::ClosurePlan,
    type_table: Rc<RefCell<TypeTable>>,
    struct_fields_map:
        IndexMap<(String, crate::module_source::ModuleSource), Vec<crate::tir::TirField>>,
    /// Mints each call's canonical `FuncId` at construction ("born resolved"),
    /// so the call node never carries a `FunctionRef`. In-package callees resolve
    /// against the pre-built `FunctionId → FuncId` map (positions in
    /// `flat.functions`, 1:1 with the final function list); extern / builtin
    /// callees are interned on first sight as `extern_stub`s appended after the
    /// in-package functions. Replaces the former post-pass `assign_func_ids`.
    interner: RefCell<Interner>,
    /// Per-module last-use spans (WEP 2026-05-21). A `Local` read whose span is
    /// listed is a move-eligible local's final use, so its defensive value copy
    /// is elided.
    moved_local_spans: IndexMap<crate::module_source::ModuleSource, IndexSet<Span>>,
}

/// Construction-time callee-id minting (see [`Translator::interner`]).
struct Interner {
    ids: IndexMap<crate::name::FunctionId, crate::nir::FuncId>,
    stubs: Vec<Rc<RefCell<NirFunction>>>,
    base_len: usize,
    /// Stubs minted for a name the package defines — see `resolve`.
    #[cfg(debug_assertions)]
    shadowed: Vec<String>,
}

impl Interner {
    /// The `FuncId` of `func_ref`: its pre-registered in-package id, or a freshly
    /// interned extern stub's id.
    fn resolve(&mut self, func_ref: &nir::FunctionRef) -> crate::nir::FuncId {
        let key = func_ref.function_id();
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        // A stub is for a callee outside the package. Minting one for a name the
        // package *does* define means the call and the definition disagree on
        // some other part of the identity — the call then binds to a body-less
        // stub and only surfaces as an unsatisfied trait bound at WIR build.
        #[cfg(debug_assertions)]
        {
            let name = &func_ref.name;
            let same_name = |k: &crate::name::FunctionId| match k {
                crate::name::FunctionId::Free(f) => f.name == *name,
                crate::name::FunctionId::Method(_) => false,
            };
            if let Some(defined) = self.ids.keys().find(|k| same_name(k)) {
                self.shadowed
                    .push(format!("{key:?} vs defined {defined:?}"));
            }
        }
        let id = crate::nir::FuncId::new(self.base_len + self.stubs.len());
        let mut stub = NirFunction::extern_stub(func_ref);
        stub.id = Some(id);
        self.stubs.push(Rc::new(RefCell::new(stub)));
        self.ids.insert(key, id);
        id
    }
}

/// The canonical `FunctionId` of a TIR function — the same identity its
/// converted `NirFunction` yields (`FunctionRef::function_id`), so the
/// pre-built id map agrees with every call site's `convert_function_ref`.
fn tir_function_key(f: &TirFunction) -> crate::name::FunctionId {
    nir::FunctionRef {
        module_source: f.module_source.clone(),
        name: f.name.clone(),
        monomorph_info: f.monomorph_info.as_ref().map(convert_monomorph_info),
        method_info: f.method_info.clone(),
    }
    .function_id()
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
    /// Last-use spans for this function's module (WEP 2026-05-21). A `Local`
    /// read whose span is present is a final use, so its copy is elided.
    func_moved_spans: Option<&'a IndexSet<Span>>,
    /// TIR-level move-eligible locals for this function (WEP 2026-05-21):
    /// backward liveness plus a freshness fixpoint proves each read is a final
    /// use of a local that exclusively owns fresh storage. Reaches synthesized
    /// bodies the AST-keyed `func_moved_spans` cannot see (serde de/serialize,
    /// derives). Unioned with the span check.
    move_eligible_locals: IndexSet<u32>,
    /// Spans of field / whole-value materializations that alias out of a *dead*
    /// aggregate at a struct/tuple literal (place-level move): the copy is elided
    /// exactly as for a whole-local final-use move, but for a projection.
    move_eligible_place_spans: IndexSet<crate::token::Span>,
    /// Locals whose binding copy is elided by sharing the source storage
    /// (WEP 2026-05-21 read-only-share): a read-only local bound from a
    /// projection whose storage is provably never mutated while it is live.
    share_eligible_locals: IndexSet<u32>,
    /// Locals a last-use move can hand to a new owner
    /// ([`value_copy::last_use::compute_moved_roots`]).
    moved_roots: IndexSet<u32>,
    /// May-alias components for this function, so a confined by-value argument
    /// keeps its copy exactly when it aliases a mutated sibling (WEP 2026-05-21).
    alias_components: value_copy::last_use::AliasComponents,
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
        let func_moved_spans = base.moved_local_spans.get(&func.module_source);
        // The move/share/alias analyses only ever mark copyable-value locals; a
        // function with none has nothing to elide, so all three are empty. Skip
        // them — running them is otherwise pure per-function allocation, and most
        // functions (scalar/reference-only) hit this path.
        let needs_copy_analysis = {
            let tt = base.type_table.borrow();
            func.params
                .iter()
                .map(|p| p.type_id)
                .chain(func.locals.iter().map(|l| l.type_id))
                .any(|tid| value_copy::needs_value_copy(tid, &tt))
        };
        let move_eligible = if needs_copy_analysis {
            let oracle = value_copy::ownership::OwnedCalls::new(
                &base.value_copy.returns_owned,
                &base.value_copy.returns_self_projection,
            )
            .with_indirect(&base.value_copy.indirect_owned_returns);
            value_copy::last_use::compute_move_eligible(
                func,
                &oracle,
                &base.type_table.borrow(),
                &base.value_copy.stored_params,
                &base.value_copy.mut_receiver_methods,
            )
        } else {
            value_copy::last_use::MoveEligible::default()
        };
        let moved_roots = if needs_copy_analysis {
            value_copy::last_use::compute_moved_roots(func, &move_eligible, func_moved_spans)
        } else {
            IndexSet::default()
        };
        let move_eligible_locals = move_eligible.locals;
        let move_eligible_place_spans = move_eligible.place_spans;
        let share_eligible_locals = if needs_copy_analysis {
            value_copy::last_use::compute_share_eligible(
                func,
                &move_eligible_locals,
                &base.value_copy.mut_receiver_methods,
                &base.value_copy.ref_receiver_methods,
                &base.value_copy.returns_receiver_alias,
            )
        } else {
            IndexSet::default()
        };
        let alias_components = if needs_copy_analysis {
            value_copy::last_use::AliasComponents::build(func)
        } else {
            value_copy::last_use::AliasComponents::empty()
        };
        Self {
            base,
            specialized,
            extra: Some(ExtraLocals {
                base_count: func.local_count,
                locals: RefCell::new(Vec::new()),
            }),
            immutable_locals,
            address_taken,
            func_moved_spans,
            move_eligible_locals,
            move_eligible_place_spans,
            share_eligible_locals,
            moved_roots,
            alias_components,
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
            func_moved_spans: None,
            move_eligible_locals: IndexSet::default(),
            move_eligible_place_spans: IndexSet::default(),
            share_eligible_locals: IndexSet::default(),
            moved_roots: IndexSet::default(),
            alias_components: value_copy::last_use::AliasComponents::empty(),
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
        let mut locals: Vec<NirLocal> = func.locals.iter().map(convert_local).collect();
        locals.extend(extra_locals.iter().map(convert_local));
        let body = root.map(move |r| {
            let mut arena = fctx.arena.into_inner();
            arena.root = r;
            arena
        });
        NirFunction {
            id: None,
            is_dead: false,
            name: func.name.clone(),
            module_source: func.module_source.clone(),
            visibility: func.visibility,
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
        let slot = global.init.slot_expr();
        let span = slot.span;
        let init_op = fctx.convert_operand(slot);
        let init_stmt = fctx.alloc_stmt(StmtKind::Expr(init_op), span);
        let init_root = fctx.alloc_block(vec![init_stmt], span);
        let body = ExprBody::from_body({
            let mut body = fctx.arena.into_inner();
            body.root = init_root;
            body
        });
        let init = match global.init {
            GlobalInit::Direct(_) => GlobalInit::Direct(body),
            GlobalInit::Deferred(_) => GlobalInit::Deferred(body),
        };
        NirGlobal {
            name: global.name.clone(),
            ty: global.ty,
            init,
            wado_mutable: global.wado_mutable,
            visibility: global.visibility,
            module_source: global.module_source.clone(),
            span: global.span,
            locals: global.locals.iter().map(convert_local).collect(),
            prefer_fixed_string_repr: false,
        }
    }

    fn convert_struct(&self, s: &TirStruct) -> NirStruct {
        let fctx = FunctionTranslator::for_top_level(self);
        NirStruct {
            name: s.name.clone(),
            module_source: s.module_source.clone(),
            visibility: s.visibility,
            type_params: s.type_params.iter().map(convert_type_param).collect(),
            monomorph_info: s.monomorph_info.as_ref().map(convert_monomorph_info),
            fields: s.fields.iter().map(|f| fctx.convert_field(f)).collect(),
            span: s.span,
            wire_name_policy: s.wire_name_policy.clone(),
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
        // A move-eligible local at its final use (WEP 2026-05-21) transfers its
        // storage: no defensive copy is needed. Sound because last-use liveness
        // proved the source dead afterward.
        if self.is_last_use_move(value) {
            return false;
        }
        let oracle = value_copy::ownership::OwnedCalls::new(
            &self.base.value_copy.returns_owned,
            &self.base.value_copy.returns_self_projection,
        )
        .with_indirect(&self.base.value_copy.indirect_owned_returns);
        value_copy::analyze::should_wrap(value, &self.base.type_table.borrow(), &oracle)
    }

    /// Whether `value` is a move rather than a copy: a whole-local read at its
    /// final use, or a field / whole-value materialization that aliases out of a
    /// dead aggregate at a literal (place-level move, keyed by span).
    /// Whether an immutable binding may alias `value`'s storage instead of
    /// copying it: the source must be rooted at an immutable local whose
    /// storage is never moved to a new owner.
    fn source_shares_immutable_storage(&self, value: &TirExpr) -> bool {
        if !value_copy::analyze::is_source_immutable(value, &self.immutable_locals) {
            return false;
        }
        value_copy::analyze::source_root(value)
            .is_some_and(|root| !self.moved_roots.contains(&root))
    }

    fn is_last_use_move(&self, value: &TirExpr) -> bool {
        // A newtype cast hands over the same storage (see
        // `last_use::strip_casts`), so it must not hide the materialization
        // underneath it.
        let value = value_copy::last_use::strip_casts(value);
        // Place-level move: the literal scan proved this exact materialization
        // aliases a dead aggregate. Covers both `base.field` and a whole `base`.
        if self.move_eligible_place_spans.contains(&value.span) {
            return true;
        }
        let TirExprKind::Local { index, .. } = &value.kind else {
            return false;
        };
        self.move_eligible_locals.contains(index)
            || self
                .func_moved_spans
                .is_some_and(|spans| spans.contains(&value.span))
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
                        expr: local_expr.into(),
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
        let inner_nir = self.convert_operand(inner);
        Some(self.wrap_in_box(
            inner_nir,
            *self.base.box_plan.box_struct_types.get(&inner.type_id)?,
            inner.span,
        ))
    }

    fn wrap_in_box(&self, value: Operand, box_type: tir::TypeId, span: Span) -> ExprId {
        let box_struct_name = self
            .base
            .type_table
            .borrow()
            .struct_list_name(box_type)
            .expect("Box type should be a struct");
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

    /// Expand `*ref = value` for an in-place aggregate (non-Box ref) into
    /// field-by-field assignments through two fresh temp locals — a `struct`
    /// (`String`) via `struct_fields_map`, or `List<T>` via its `SeqField`
    /// layout. Box-shaped deref-assigns lower as a single statement via the
    /// regular `try_boxing_rewrite` `Deref` arm.
    fn try_expand_deref_aggregate_assign(&self, stmt: &TirStmt) -> Option<Vec<StmtId>> {
        use crate::compiler_item::SeqField;
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

        // The referent must be an in-place aggregate, so `*ref = v` writes each
        // field of the shared handle. Replace-on-assign referents (variant /
        // enum / fn) are boxed and were filtered by the `box_type_ids` check
        // above. Three in-place shapes reach here:
        //   - a plain `struct` (`String`, monomorphized generics): fields from
        //     `struct_fields_map`.
        //   - `List<T>`: an in-place `GenericInstance` never monomorphized into
        //     its own struct, so its canonical `{repr, used}` layout comes from
        //     `SeqField` with the concrete element type.
        //   - a tuple (`[A, B, …]`): also an in-place `GenericInstance`, with
        //     positional fields `0..n` typed by the tuple's element types.
        // Any other type falls through to the default single-statement lowering.
        let inner_resolved = self.base.type_table.borrow().get(inner_type_id).clone();
        let fields: Vec<(String, u32, tir::TypeId)> = if let crate::tir::ResolvedType::Struct {
            decl_name,
            module_source,
            type_args,
        } = inner_resolved
        {
            let name = self
                .base
                .type_table
                .borrow()
                .struct_rendered_name(&decl_name, &type_args);
            self.base
                .struct_fields_map
                .get(&(name, module_source))?
                .iter()
                .map(|f| (f.name.clone(), f.index, f.type_id))
                .collect()
        } else {
            let (list_elem, tuple_elems) = {
                let tt = self.base.type_table.borrow();
                (tt.as_list(inner_type_id), tt.as_tuple(inner_type_id))
            };
            if let Some(elem) = list_elem {
                let repr_ty = self.base.type_table.borrow_mut().make_builtin_array(elem);
                vec![
                    (
                        SeqField::Backing.field_name().to_string(),
                        SeqField::Backing.index(),
                        repr_ty,
                    ),
                    (
                        SeqField::Len.field_name().to_string(),
                        SeqField::Len.index(),
                        crate::tir::TypeTable::I32,
                    ),
                ]
            } else {
                let elems = tuple_elems?;
                elems
                    .into_iter()
                    .enumerate()
                    .map(|(i, ty)| (i.to_string(), i as u32, ty))
                    .collect()
            }
        };
        if fields.is_empty() {
            return None;
        }

        let span = expr.span;
        let ref_idx = self.alloc_local(ref_type_id, "__deref_ref".to_string());
        let val_idx = self.alloc_local(inner_type_id, "__deref_val".to_string());

        let ref_nir = self.convert_expr(ref_expr);
        // Copy the RHS unless it is fresh/moved: the per-field write-back would
        // otherwise alias the RHS's storage. The seed walker registers a helper
        // for the deref-target RHS type so the wrap resolves.
        let converted_val = self.convert_operand(value);
        let val_nir = if self.should_wrap_value_copy(value) {
            self.wrap_value_copy_operand(converted_val, inner_type_id)
        } else {
            converted_val
        };

        let mut out: Vec<StmtId> = Vec::with_capacity(2 + fields.len());
        out.push(self.alloc_stmt(
            StmtKind::Let {
                name: format!("__deref_ref_{ref_idx}"),
                local_index: ref_idx,
                is_mut: false,
                is_reactive: false,
                type_id: ref_type_id,
                value: ref_nir.into(),
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
                // The copy decision is made above (`should_wrap_value_copy`), so
                // this synthesized binding must not re-wrap.
                skip_value_copy: true,
            },
            span,
        ));
        for (field_name, field_index, field_type) in &fields {
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
                    expr: ref_local.into(),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            let value_field = self.alloc_expr(
                ExprKind::FieldAccess {
                    expr: val_local.into(),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            let assign = self.alloc_expr(
                ExprKind::Assign {
                    target: target_field,
                    value: value_field.into(),
                },
                *field_type,
                span,
            );
            out.push(self.alloc_stmt(StmtKind::Expr(assign.into()), span));
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
                expr: inner_nir.into(),
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
        let func = nir::FunctionRef {
            module_source: helper_module.clone(),
            name: helper_name.clone(),
            monomorph_info: None,
            method_info: None,
        };
        let func_id = self.base.interner.borrow_mut().resolve(&func);
        self.alloc_expr(
            ExprKind::Call {
                func_id,
                type_args: vec![],
                args: vec![ArenaCallArg {
                    expr: value.into(),
                    is_mut: false,
                }],
                has_receiver: false,
            },
            type_id,
            span,
        )
    }

    /// [`Self::wrap_value_copy`] over an operand: a promoted scalar
    /// (`Operand::Value`) is never value-semantic, so it passes through; only a
    /// skeleton aggregate is wrapped.
    fn wrap_value_copy_operand(&self, value: Operand, type_id: tir::TypeId) -> Operand {
        match value {
            Operand::Expr(e) => self.wrap_value_copy(e, type_id).into(),
            Operand::Value(_) => value,
        }
    }

    fn convert_block(&self, block: &TirBlock) -> BlockId {
        let mut stmts: Vec<StmtId> = Vec::with_capacity(block.stmts.len());
        for s in &block.stmts {
            if let Some(expanded) = self.try_expand_deref_aggregate_assign(s) {
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
                // Copy first, then box: a boxed local changes only its
                // storage cell, not its value semantics.
                let (effective_type, box_wrap_type) = if self.address_taken.contains(local_index)
                    && let Some(&box_type) = self.base.box_plan.box_struct_types.get(type_id)
                {
                    (box_type, Some(box_type))
                } else {
                    (*type_id, None)
                };
                let needs_value_copy_wrap = !*skip_value_copy
                    && !self.share_eligible_locals.contains(local_index)
                    && (*is_mut || !self.source_shares_immutable_storage(value))
                    && self.should_wrap_value_copy(value);
                let value_op = self.convert_operand(value);
                let value_op = if needs_value_copy_wrap {
                    self.wrap_value_copy_operand(value_op, *type_id)
                } else {
                    value_op
                };
                let value_op = if let Some(box_type) = box_wrap_type {
                    self.wrap_in_box(value_op, box_type, value.span).into()
                } else {
                    value_op
                };
                StmtKind::Let {
                    name: name.clone(),
                    local_index: *local_index,
                    is_mut: *is_mut,
                    is_reactive: *is_reactive,
                    type_id: effective_type,
                    value: value_op,
                    skip_value_copy: *skip_value_copy,
                }
            }
            TirStmtKind::Expr(expr) => StmtKind::Expr(self.convert_operand(expr)),
            TirStmtKind::Return { value } => StmtKind::Return {
                value: value.as_ref().map(|v| self.convert_operand(v)),
            },
            TirStmtKind::TaskReturn { .. } => unreachable!(
                "TirStmtKind::TaskReturn should be eliminated by synthesis::cm_binding before lower::translate runs"
            ),
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => StmtKind::If {
                condition: self.convert_operand(condition),
                then_block: self.convert_block(then_block),
                else_block: else_block.as_ref().map(|b| self.convert_block(b)),
            },
            TirStmtKind::Loop { body } => StmtKind::Loop {
                body: self.convert_block(body),
            },
            TirStmtKind::Break { label, value } => StmtKind::Break {
                label: label.clone(),
                value: value.as_ref().map(|v| self.convert_operand(v)),
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
                let value_op = self.convert_operand(value);
                let value_op = if needs_wrap {
                    self.wrap_value_copy_operand(value_op, value_type)
                } else {
                    value_op
                };
                StmtKind::LetDestructure {
                    pattern: self.convert_pattern(pattern),
                    is_mut: *is_mut,
                    value: value_op,
                }
            }
            TirStmtKind::VariadicForOf { .. } => unreachable!(
                "TirStmtKind::VariadicForOf should be expanded by monomorphize before lower::translate runs"
            ),
        }
    }

    /// Lower an expression in an operand (rvalue) position. Phase A wraps the
    /// skeleton subtree as `Operand::Expr`; Phase B interns pure expressions into
    /// the function's `ValuePool` and returns `Operand::Value` here instead.
    fn convert_operand(&self, expr: &TirExpr) -> Operand {
        // Pure scalar literals are born directly as `Operand::Value` in the
        // function's value pool — they never exist as an `ExprKind` (WEP: The
        // Live ValueGraph; pure scalars live only in the graph). `alloc_unshared`
        // keeps each constant's source width (a type-erased `7` of `i32` vs
        // `i64` must not collide).
        use crate::nir_value_graph::ValueKind;
        let vk = match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => Some(ValueKind::Int(*value, expr.type_id)),
            TirExprKind::FloatLiteral { value, .. } => {
                Some(ValueKind::Float(value.to_bits(), expr.type_id))
            }
            TirExprKind::BoolLiteral(b) => Some(ValueKind::Bool(*b)),
            TirExprKind::CharLiteral(c) => Some(ValueKind::Char(*c)),
            // Pure constants whose WIR depends only on type/bytes (read back
            // from the pool by the extractor): `Null` → `None`/`ref.null`,
            // string → `translate_string_literal`, unit → no runtime value.
            TirExprKind::Null => Some(ValueKind::Null),
            // String / bytes literals are no longer atomic pool values; they
            // lower to a `StructLiteral` over a packed `Array<u8>` repr in
            // `convert_expr` (`seq_literal`), so they fall through to the
            // skeleton path here.
            TirExprKind::Unit => Some(ValueKind::Unit),
            _ => None,
        };
        match vk {
            Some(vk) => {
                let vid = self
                    .arena
                    .borrow_mut()
                    .values
                    .alloc_unshared(vk, expr.type_id);
                Operand::Value(vid)
            }
            None => Operand::Expr(self.convert_expr(expr)),
        }
    }

    fn convert_expr(&self, expr: &TirExpr) -> ExprId {
        // String / bytes literals lower to a `StructLiteral` over a packed
        // `Array<u8>` repr (`seq_literal`), so the generic aggregate machinery
        // (`.used` field-projection, body globalization) applies.
        match &expr.kind {
            TirExprKind::StringLiteral(s) => {
                return self.seq_literal(expr.type_id, s.as_bytes().to_vec(), expr.span);
            }
            TirExprKind::BytesLiteral(b) => {
                return self.seq_literal(expr.type_id, b.clone(), expr.span);
            }
            _ => {}
        }
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
                    functor: nir_struct.into(),
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
            let functor_fq = FqTypeName::declared(&functor.module_source, &functor.struct_name);
            let call_method_name =
                MethodName::format_local(&functor_fq, None, crate::name::CLOSURE_CALL_METHOD);
            let call_method_info = LocalMethodName::new(
                functor_fq,
                None,
                crate::name::CLOSURE_CALL_METHOD.to_string(),
            );
            let call_method_borrow = functor.call_method.borrow();
            // `ArenaCallArg::is_mut` means "the callee may write the caller's
            // storage through this slot", which is `is_mut_ref` — the same
            // source `call_args_in_param_order` reads. A declared-`mut` binding
            // is the callee's own local and says nothing about the caller's.
            let params_is_mut: Vec<bool> = call_method_borrow
                .params
                .iter()
                .map(|p| p.is_mut_ref)
                .collect();
            // The receiver is the functor; it heads `args` so the list lines up
            // with `call_method`'s full parameter list including `self`.
            let mut nir_args: Vec<ArenaCallArg> = vec![ArenaCallArg {
                expr: nir_receiver.into(),
                is_mut: params_is_mut.first().copied().unwrap_or(false),
            }];
            nir_args.extend(
                args.iter()
                    .zip(
                        params_is_mut
                            .into_iter()
                            .skip(1)
                            .chain(std::iter::repeat(false)),
                    )
                    .map(|(arg, is_mut)| ArenaCallArg {
                        expr: self.convert_operand(arg),
                        is_mut,
                    }),
            );
            let func = nir::FunctionRef {
                module_source: functor.module_source.clone(),
                name: call_method_name,
                monomorph_info: None,
                method_info: Some(call_method_info),
            };
            let func_id = self.base.interner.borrow_mut().resolve(&func);
            return self.alloc_expr(
                ExprKind::Call {
                    func_id,
                    type_args: Vec::new(),
                    args: nir_args,
                    has_receiver: true,
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

    /// Convert a call argument. When the argument is
    /// a specialized fn-param `Local` and the slot still expects
    /// `fn(...)`, wrap the converted `Local` in
    /// `ExprKind::ClosureToCanonical` so the callee sees the
    /// original function-shaped view.
    fn convert_specialized_arg_operand(&self, arg: &TirExpr) -> Operand {
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
            return self
                .alloc_expr(
                    ExprKind::ClosureToCanonical {
                        functor: inner.into(),
                        functor_id: spec.functor_id,
                        target_fn_type: spec.original_fn_type,
                        closure_module: functor.module_source.clone(),
                    },
                    spec.original_fn_type,
                    arg.span,
                )
                .into();
        }
        self.convert_operand(arg)
    }

    fn convert_expr_kind(&self, kind: &TirExprKind) -> ExprKind {
        match kind {
            // Pure scalar literals are interned into the `ValuePool` and born as
            // `Operand::Value` by `convert_operand`; every literal-bearing
            // position routes through `convert_operand`, so `convert_expr` is
            // never entered on one (WEP: The Live ValueGraph).
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_) => {
                unreachable!("scalar literals are interned via convert_operand, never convert_expr")
            }
            TirExprKind::StringLiteral(_) => {
                unreachable!("string literals are interned via convert_operand, never convert_expr")
            }
            TirExprKind::BytesLiteral(_) => {
                unreachable!("bytes literals are lowered via seq_literal in convert_expr")
            }
            TirExprKind::Null => {
                unreachable!("Null is interned via convert_operand, never convert_expr")
            }
            TirExprKind::Unit => {
                unreachable!("unit is interned via convert_operand, never convert_expr")
            }
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
                value: self.convert_operand(value),
            },
            TirExprKind::Binary { left, op, right } => ExprKind::Binary {
                left: self.convert_operand(left),
                op: convert_binary_op(*op),
                right: self.convert_operand(right),
            },
            TirExprKind::Unary { op, expr } => ExprKind::Unary {
                op: convert_unary_op(*op),
                expr: self.convert_operand(expr),
            },
            TirExprKind::Assign { target, value } => {
                // Only `Local` targets receive a defensive copy.
                // `FieldAccess` / `Index` writes mutate an existing
                // aggregate slot — the WIR-side semantics let the
                // reference flow through without an extra wrap.
                let needs_wrap = matches!(&target.kind, TirExprKind::Local { .. })
                    && self.should_wrap_value_copy(value);
                let value_type = value.type_id;
                let value_op = self.convert_operand(value);
                let value_op = if needs_wrap {
                    self.wrap_value_copy_operand(value_op, value_type)
                } else {
                    value_op
                };
                ExprKind::Assign {
                    target: self.convert_expr(target),
                    value: value_op,
                }
            }
            TirExprKind::Cast { expr, target_type } => ExprKind::Cast {
                expr: self.convert_operand(expr),
                target_type: *target_type,
            },
            TirExprKind::Call {
                func,
                type_args,
                args,
                has_receiver,
            } => self.convert_call(func, type_args, args, *has_receiver),
            TirExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name: local_name.clone(),
                args: args.iter().map(|a| self.convert_operand(a)).collect(),
            },
            TirExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => ExprKind::FieldAccess {
                expr: self.convert_operand(expr),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            TirExprKind::Index { expr, index } => ExprKind::Index {
                expr: self.convert_operand(expr),
                index: self.convert_operand(index),
            },
            TirExprKind::Block(block) => ExprKind::Block(self.convert_block(block)),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: self.convert_operand(condition),
                then_branch: self.convert_block(then_branch),
                else_branch: else_branch.as_ref().map(|b| self.convert_block(b)),
            },
            TirExprKind::Match { expr, arms } => ExprKind::Match {
                expr: self.convert_operand(expr),
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
                elements: elements
                    .iter()
                    .map(|e| self.convert_literal_element(e))
                    .collect(),
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
                callee: self.convert_operand(callee),
                // Indirect-call args take an unconditional defensive
                // copy when the value semantics require it: the callee
                // signature is opaque here, so the wrap predicate is
                // applied to every arg regardless of an `is_mut`
                // marker.
                args: args
                    .iter()
                    .map(|a| {
                        let needs_wrap = self.should_wrap_value_copy(a);
                        let op = self.convert_operand(a);
                        if needs_wrap {
                            self.wrap_value_copy_operand(op, a.type_id)
                        } else {
                            op
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
                payload: payload.as_ref().map(|p| self.convert_literal_element(p)),
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
                expr: self.convert_operand(expr),
            },
            TirExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => ExprKind::VariantTest {
                expr: self.convert_operand(expr),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TirExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => ExprKind::VariantPayload {
                expr: self.convert_operand(expr),
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
        has_receiver: bool,
    ) -> ExprKind {
        if func.module_source.is_core_builtin()
            && crate::tir::matches_builtin(&func.name, func.monomorph_info.as_ref(), "copy_value")
            && args.len() == 1
            && let Some(type_id) = func
                .monomorph_info
                .as_ref()
                .and_then(|mi| mi.impl_type_args.first().copied())
            && let Some((helper_module, helper_name)) =
                self.base.value_copy.name_for_type.get(&type_id)
        {
            let func = nir::FunctionRef {
                module_source: helper_module.clone(),
                name: helper_name.clone(),
                monomorph_info: None,
                method_info: None,
            };
            let func_id = self.base.interner.borrow_mut().resolve(&func);
            // Bypass `convert_call_arg_at`: wrapping a copy helper's own
            // argument would emit copy(copy(x)).
            return ExprKind::Call {
                func_id,
                type_args: vec![],
                args: args
                    .iter()
                    .map(|a| ArenaCallArg {
                        expr: self.convert_specialized_arg_operand(&a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
                has_receiver: false,
            };
        }
        if func.module_source.is_core_builtin()
            && let Some(rewritten) = self.convert_case_bridge_call(func, type_args, args)
        {
            return rewritten;
        }
        let ordered = self.call_args_in_param_order(func, has_receiver, args);
        let mut_roots = self.call_mut_roots(func, &ordered);
        let nir_func = convert_function_ref(func);
        let func_id = self.base.interner.borrow_mut().resolve(&nir_func);
        ExprKind::Call {
            func_id,
            type_args: type_args.to_vec(),
            args: ordered
                .iter()
                .enumerate()
                .map(|(i, (e, is_mut))| {
                    if has_receiver && i == 0 {
                        self.convert_receiver_arg(e, *is_mut)
                    } else {
                        self.convert_call_arg_at(e, *is_mut, Some(func), i, &mut_roots)
                    }
                })
                .collect(),
            has_receiver,
        }
    }

    /// Rewrites the `Case<V, P>` bridge markers (WEP 2026-06-13 §3e):
    /// `builtin::variant_tag` becomes a direct tag read, and
    /// `builtin::variant_case_extract` / `_construct` become calls to the
    /// per-(variant, payload-type) helpers synthesized with `ReflectVariant`.
    /// Returns `None` for every other callee.
    fn convert_case_bridge_call(
        &self,
        func: &FunctionRef,
        type_args: &[tir::TypeId],
        args: &[CallArg],
    ) -> Option<ExprKind> {
        use crate::tir::matches_builtin;
        let mi = func.monomorph_info.as_ref();

        if matches_builtin(&func.name, mi, "variant_tag") && args.len() == 1 {
            // The `&V` receiver matches the marker's argument, so `discriminant`
            // is the single lowering for the tag read.
            let arg_ty = args[0].expr.type_id;
            let (helper_name, helper_module) = {
                let tt = self.base.type_table.borrow();
                // The marker's `&T` argument may already be lowered to `Box<V>`.
                let peeled = tt.peel_refs(arg_ty);
                let box_name = tt
                    .compiler_items()
                    .struct_name(crate::compiler_item::CompilerItem::Box)
                    .to_string();
                let unboxed = self
                    .base
                    .box_plan
                    .get_box_inner_type(peeled)
                    .or_else(|| match tt.get(peeled) {
                        tir::ResolvedType::Struct {
                            decl_name: base,
                            module_source,
                            type_args,
                            ..
                        } if !type_args.is_empty() && *base == box_name => self
                            .base
                            .struct_fields_map
                            .get(&(
                                self.base
                                    .type_table
                                    .borrow()
                                    .struct_rendered_name(base, type_args),
                                module_source.clone(),
                            ))
                            .and_then(|fields| fields.first())
                            .map(|f| f.type_id),
                        _ => None,
                    })
                    .unwrap_or(peeled);
                let variant_ty = tt.peel_refs(unboxed);
                // A plain variant carries the tag read as a trait method on its
                // declaration. A generic one has no instantiated declaration,
                // so it uses the per-instance helper minted post-monomorphize.
                let module = match tt.get(variant_ty) {
                    tir::ResolvedType::Variant { module_source, .. }
                    | tir::ResolvedType::GenericInstance { module_source, .. } => {
                        module_source.clone()
                    }
                    other => panic!("variant_tag marker on non-variant type: {other:?}"),
                };
                let items = tt.compiler_items();
                let name = match tt.get(variant_ty) {
                    tir::ResolvedType::GenericInstance { .. } => {
                        crate::name::variant_tag_helper_name(
                            &tt.mangle_type_arg_for_generic(variant_ty),
                        )
                    }
                    _ => crate::name::MethodName::format_local(
                        &tt.fq_type_name(variant_ty),
                        Some(items.trait_name(crate::compiler_item::CompilerItem::ReflectVariant)),
                        items.method_name(
                            crate::compiler_item::CompilerItem::ReflectVariantDiscriminant,
                        ),
                    ),
                };
                (name, module)
            };
            let nir_func = nir::FunctionRef {
                module_source: helper_module,
                name: helper_name,
                monomorph_info: None,
                method_info: None,
            };
            let func_id = self.base.interner.borrow_mut().resolve(&nir_func);
            return Some(ExprKind::Call {
                func_id,
                type_args: vec![],
                args: args
                    .iter()
                    .map(|a| ArenaCallArg {
                        expr: self.convert_specialized_arg_operand(&a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
                has_receiver: false,
            });
        }

        if matches_builtin(&func.name, mi, "struct_field_get") {
            let (struct_ty, field_ty) = if type_args.len() == 2 {
                (type_args[0], type_args[1])
            } else {
                let mi = mi.expect("struct_field_get marker without type args or monomorph info");
                let ta = if mi.method_type_args.len() == 2 {
                    &mi.method_type_args
                } else {
                    &mi.impl_type_args
                };
                assert!(
                    ta.len() == 2,
                    "struct_field_get marker expects [struct, field] type args, got {ta:?}"
                );
                (ta[0], ta[1])
            };

            let (helper_name, helper_module) = {
                let tt = self.base.type_table.borrow();
                let name = crate::name::field_get_helper_name(
                    &tt.mangle_type_arg_for_generic(struct_ty),
                    &tt.mangle_type_arg_for_generic(field_ty),
                );
                let module = match tt.get(struct_ty) {
                    tir::ResolvedType::Struct { module_source, .. } => module_source.clone(),
                    other => panic!("struct_field_get marker on non-struct type: {other:?}"),
                };
                (name, module)
            };

            let nir_func = nir::FunctionRef {
                module_source: helper_module,
                name: helper_name,
                monomorph_info: None,
                method_info: None,
            };
            let func_id = self.base.interner.borrow_mut().resolve(&nir_func);
            return Some(ExprKind::Call {
                func_id,
                type_args: vec![],
                args: args
                    .iter()
                    .map(|a| ArenaCallArg {
                        expr: self.convert_specialized_arg_operand(&a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
                has_receiver: false,
            });
        }

        let helper_name_for: fn(&str, &str) -> String =
            if matches_builtin(&func.name, mi, "variant_case_extract") {
                crate::name::case_extract_helper_name
            } else if matches_builtin(&func.name, mi, "variant_case_construct") {
                crate::name::case_construct_helper_name
            } else {
                return None;
            };

        let (variant_ty, payload_ty) = if type_args.len() == 2 {
            (type_args[0], type_args[1])
        } else {
            let mi = mi.expect("case bridge marker without type args or monomorph info");
            let ta = if mi.method_type_args.len() == 2 {
                &mi.method_type_args
            } else {
                &mi.impl_type_args
            };
            assert!(
                ta.len() == 2,
                "case bridge marker expects [variant, payload] type args, got {ta:?}"
            );
            (ta[0], ta[1])
        };

        let (helper_name, helper_module) = {
            let tt = self.base.type_table.borrow();
            let name = helper_name_for(
                &tt.mangle_type_arg_for_generic(variant_ty),
                &tt.mangle_type_arg_for_generic(payload_ty),
            );
            // An instantiated generic variant stays spelled as a
            // `GenericInstance`; its bridges are homed in the declaration's
            // module.
            let module = match tt.get(variant_ty) {
                tir::ResolvedType::Variant { module_source, .. }
                | tir::ResolvedType::GenericInstance { module_source, .. } => module_source.clone(),
                other => panic!("case bridge marker on non-variant type: {other:?}"),
            };
            (name, module)
        };

        let nir_func = nir::FunctionRef {
            module_source: helper_module,
            name: helper_name,
            monomorph_info: None,
            method_info: None,
        };
        let func_id = self.base.interner.borrow_mut().resolve(&nir_func);
        Some(ExprKind::Call {
            func_id,
            type_args: vec![],
            args: args
                .iter()
                .map(|a| ArenaCallArg {
                    expr: self.convert_specialized_arg_operand(&a.expr),
                    is_mut: a.is_mut,
                })
                .collect(),
            has_receiver: false,
        })
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
                expr: self.convert_operand(expr),
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
        let guard = arm.guard.as_ref().map(|g| self.convert_operand(g));
        let body = self.convert_operand(&arm.body);
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
            value: self.convert_literal_element(&field.value),
            field_index: field.field_index,
        }
    }

    /// Convert a value stored into an aggregate literal (a struct field or tuple
    /// element), deep-copying it when it names an existing value — building a
    /// literal from a variable must not share the variable's interior.
    fn convert_literal_element(&self, value: &TirExpr) -> Operand {
        let converted = self.convert_operand(value);
        if self.should_wrap_value_copy(value) {
            self.wrap_value_copy_operand(converted, value.type_id)
        } else {
            converted
        }
    }

    /// Build a `String` / `List<u8>` literal as a `StructLiteral { repr:
    /// PackedArray(bytes), used: <len> }` over a raw packed `Array<u8>`, so the
    /// literal flows through the generic aggregate machinery (field-projection
    /// of `.used`, body globalization) instead of being an opaque value. The
    /// `repr` field's raw-array type and the field indices come from the
    /// sequence struct's definition.
    /// The single `Array<u8>` type that every `String` / `List<u8>` literal uses
    /// for its `repr` field. `String` and `List<u8>` share one canonical backing
    /// type, so it is read off the always-loaded `String` struct, keyed by the
    /// compiler-item registry's canonical name/module rather than a `"String"`
    /// magic literal — a bytes-only program may never monomorphize `List<u8>`
    /// into `struct_fields_map`, but `String` is guaranteed present.
    fn seq_u8_repr_type(&self) -> tir::TypeId {
        use crate::compiler_item::{CompilerItem, SeqField};
        let (string_module, string_name) = self
            .base
            .type_table
            .borrow()
            .compiler_struct_owned(CompilerItem::String);
        self.base
            .struct_fields_map
            .get(&(string_name, string_module))
            .and_then(|fields| {
                fields
                    .iter()
                    .find(|f| f.name == SeqField::Backing.field_name())
            })
            .map(|f| f.type_id)
            .expect("String struct (repr field) is always loaded")
    }

    fn seq_literal(&self, seq_type_id: tir::TypeId, bytes: Vec<u8>, span: Span) -> ExprId {
        use crate::compiler_item::SeqField;
        let len = i32::try_from(bytes.len()).expect("seq literal length fits i32");
        let array_u8_ty = self.seq_u8_repr_type();
        let packed = self.alloc_expr(ExprKind::PackedArray(bytes), array_u8_ty, span);
        let used_val = self.arena.borrow_mut().values.alloc_unshared(
            crate::nir_value_graph::ValueKind::Int(
                i64::from(len) as u64,
                crate::tir::TypeTable::I32,
            ),
            crate::tir::TypeTable::I32,
        );
        let kind = ExprKind::StructLiteral {
            struct_type: seq_type_id,
            struct_name: self.base.type_table.borrow().type_name(seq_type_id),
            fields: vec![
                ArenaStructField {
                    name: SeqField::Backing.field_name().to_string(),
                    value: Operand::Expr(packed),
                    field_index: SeqField::Backing.index(),
                },
                ArenaStructField {
                    name: SeqField::Len.field_name().to_string(),
                    value: Operand::Value(used_val),
                    field_index: SeqField::Len.index(),
                },
            ],
        };
        self.alloc_expr(kind, seq_type_id, span)
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
                    value: value.into(),
                    field_index: i as u32,
                }
            })
            .collect()
    }

    /// Convert a method call's receiver. It occupies `args[0]` like any other
    /// argument but is a *place*: wrapping it in `$value_copy$T` would hand the
    /// callee a throwaway copy and discard the mutation the call exists to
    /// perform (a `String` builder's `push_str` would append to the copy). Nor
    /// may it be re-wrapped as a canonical closure the way a specialized
    /// fn-param argument is — the method resolved against the receiver's own
    /// type, not `fn(...)`.
    fn convert_receiver_arg(&self, receiver: &TirExpr, is_mut: bool) -> ArenaCallArg {
        ArenaCallArg {
            expr: self.convert_operand(receiver),
            is_mut,
        }
    }

    /// Convert one call argument, wrapping it in `$value_copy$T` unless
    /// `should_wrap_value_copy` says no or the callee parameter is confined.
    /// `param_index` indexes the callee's full parameter list.
    fn convert_call_arg_at(
        &self,
        arg: &TirExpr,
        is_mut: bool,
        callee: Option<&FunctionRef>,
        param_index: usize,
        mut_roots: &[u32],
    ) -> ArenaCallArg {
        let needs_value_copy = self.should_wrap_value_copy(arg)
            && !self.arg_confined(arg, is_mut, callee, param_index, mut_roots);
        let value_type = arg.type_id;
        let converted = self.convert_specialized_arg_operand(arg);
        let expr = if needs_value_copy {
            self.wrap_value_copy_operand(converted, value_type)
        } else {
            converted
        };
        ArenaCallArg { expr, is_mut }
    }

    /// Whether a by-value argument into a confined parameter can skip its copy:
    /// the parameter is not `mut`-declared and the argument aliases no `mut_root`.
    fn arg_confined(
        &self,
        arg: &TirExpr,
        is_mut: bool,
        callee: Option<&FunctionRef>,
        param_index: usize,
        mut_roots: &[u32],
    ) -> bool {
        if is_mut {
            return false;
        }
        let confined = callee.is_some_and(|c| {
            self.base
                .value_copy
                .confined_params
                .is_confined(c, param_index)
        });
        if !confined {
            return false;
        }
        match value_copy::last_use::alias_root(arg) {
            Some(r) => !mut_roots
                .iter()
                .any(|m| self.alias_components.may_alias(*m, r)),
            None => mut_roots.is_empty(),
        }
    }

    /// The storage roots a call mutates: the referent of each `&mut` parameter.
    /// `args` is in the callee's parameter order, so a method's receiver is
    /// `args[0]` and needs no offset.
    fn call_mut_roots(&self, callee: &FunctionRef, args: &[(&TirExpr, bool)]) -> Vec<u32> {
        let mut_ref_params = self
            .base
            .value_copy
            .mut_ref_params
            .get(&callee.module_source, &callee.name);
        let mut roots = Vec::new();
        for (i, (expr, _)) in args.iter().enumerate() {
            if mut_ref_params
                .and_then(|v| v.get(i))
                .copied()
                .unwrap_or(false)
                && let Some(r) = value_copy::last_use::alias_root(expr)
            {
                roots.push(r);
            }
        }
        roots
    }

    /// A call's arguments paired with their declared `mut`-ness, in the callee's
    /// parameter order. `receiver`, when present, heads the list as parameter 0
    /// and takes its mutability from the callee's `self` parameter.
    fn call_args_in_param_order<'b>(
        &self,
        callee: &FunctionRef,
        has_receiver: bool,
        args: &'b [CallArg],
    ) -> Vec<(&'b TirExpr, bool)> {
        // TIR leaves a receiver's `is_mut` unset (see `TirExprKind::method_call`);
        // the callee's declared `self` mode is the authority, and this is its
        // only consumer.
        let self_is_mut_ref = has_receiver
            && self
                .base
                .value_copy
                .mut_ref_params
                .get(&callee.module_source, &callee.name)
                .and_then(|v| v.first())
                .copied()
                .unwrap_or(false);
        args.iter()
            .enumerate()
            .map(|(i, a)| {
                let is_mut = if has_receiver && i == 0 {
                    self_is_mut_ref
                } else {
                    a.is_mut
                };
                (&a.expr, is_mut)
            })
            .collect()
    }

    fn convert_field(&self, field: &TirField) -> NirField {
        // NIR carries no field default: defaults are resolved into struct
        // literals by the elaborator before lowering, so the NIR copy was
        // write-only.
        NirField {
            name: field.name.clone(),
            visibility: field.visibility,
            type_id: field.type_id,
            index: field.index,
            span: field.span,
            is_secret: field.is_secret,
            wire_name_override: field.wire_name_override.clone(),
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
            is_mut_ref: param.is_mut_ref,
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
        visibility: e.visibility,
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
        visibility: f.visibility,
        type_id: f.type_id,
        members: f.members.iter().map(convert_flags_member).collect(),
        span: f.span,
    }
}

fn convert_variant_decl(v: &TirVariantDecl) -> NirVariantDecl {
    NirVariantDecl {
        name: v.name.clone(),
        module_source: v.module_source.clone(),
        visibility: v.visibility,
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
