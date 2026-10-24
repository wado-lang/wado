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
use std::sync::Arc;

use crate::call_args::CallArgs;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::place;
use crate::lower::plan::value_copy::points_to::PointsTo;
use crate::lower::plan::{LowerPlan, closure, value_copy};
use crate::name::MethodName;
use crate::synthesis::common::builtin_call;
use cranelift_entity::EntityRef;

use crate::compiler_item::{CompilerItem, SeqField};
use crate::lower::plan::boxing::BoxPlan;
use crate::lower::plan::string;
use crate::lower::wide_int_literal::literal_from_repr;
use crate::lower::{bare_asserts, wide_arith, wide_int_literal};
use crate::name::{
    FunctionId, case_construct_helper_name, case_extract_helper_name, closure_capture_field,
    field_get_helper_name, field_ref_helper_name, hole_fmt_helper_name, hole_get_helper_name,
    template_segment_helper_name, variant_tag_helper_name,
};
use crate::nir::{
    FuncCell, FuncId, FuncRef, NirEnum, NirEnumCase, NirField, NirFlags, NirFlagsMember,
    NirFunction, NirGlobal, NirLiteralPattern, NirLocal, NirParam, NirStruct, NirTest,
    NirTypeParam, NirVariantCase, NirVariantDecl, ParamAbi,
};
use crate::nir_arena::{
    ArenaCallArg, ArenaStructField, ArenaStructPatternField, ArmData, BlockId, BlockNode,
    Body, ExprBody, ExprId, ExprKind, ExprNode, Operand, PackedData, PatId, PatKind,
    PatNode, StmtId, StmtKind, StmtNode, Tracked,
};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{ValueId, ValueKind, ValuePool};
use crate::primitive::PrimitiveType;
use crate::tir::{
    CallArg, CaptureSource, ClosureFunctor, FunctionRef, GlobalInit, LetStorage, MonomorphInfo,
    ResolvedType, StructDef, StructFields, TirBinaryOp, TirBlock, TirCapture, TirEnum, TirEnumCase,
    TirExpr, TirExprKind, TirField, TirFlags, TirFlagsMember, TirFunction, TirGlobal,
    TirLiteralPattern, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirStruct, TirStructField, TirStructPatternField, TirTest, TirTypeParam, TirUnaryOp,
    TirVariantCase, TirVariantDecl, receiver_value, struct_fields,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;
use crate::type_table::TypeTable;
use crate::{nir, tir};

/// Translate a [`FlatPackage`] (TIR-shaped) into a [`NirPackage`] (NIR-shaped).
///
/// Takes ownership of `flat` so owned containers move straight into
/// the `NirPackage`. The closure-functor conversion looks up the
/// fresh [`FuncRef`] in `func_map`, so a functor shares its methods with
/// the package rather than holding copies.
pub fn translate(flat: FlatPackage, plan: LowerPlan) -> NirPackage {
    let LowerPlan {
        box_plan,
        closure,
        value_copy,
    } = plan;
    // Before pattern lowering, which keeps a destructure of a multi-value
    // builtin whole: a software form returns its tuple like any call, and its
    // destructure is lowered like any other.
    wide_arith::lower(&flat, flat.codegen_flags.wide_arithmetic);
    // Pattern lowering runs before string collection: it synthesises
    // string-literal expressions (string-literal pattern guards) the
    // data section must register.
    {
        let pattern = pattern::Lowering::new(&flat, &value_copy.returns_receiver_alias);
        let type_table = flat.type_table.borrow();
        for func_rc in &flat.functions {
            pattern.lower_function(&mut func_rc.borrow_mut(), &type_table);
        }
    }
    // Lower the `assert_failed` marker before string planning, so a bare-asserts
    // build never collects the dropped diagnostic literals into the data section
    // (and a default build routes the marker back to a plain `panic`).
    bare_asserts::lower(&flat, flat.codegen_flags.bare_asserts);
    let strings = string::plan(&flat);
    // After the last rewrite of a body, which would move the arguments it keys.
    let struct_fields_map = struct_fields(&flat.structs);
    let points_to =
        value_copy::points_to::compute(&flat, &struct_fields_map, &closure.functor_infos);
    let FlatPackage {
        entry_module_source,
        type_table,
        interner: _,
        resource_wrappers: _,
        functions,
        structs,
        enums,
        variants,
        variant_index,
        flags,
        globals,
        tests,
        wasm_module_sources,
        builtin_declarations,
        module_name,
        cm_interface_registry,
        world_registry,
        used_wasi_functions,
        strip_names,
        coverage_section,
        codegen_flags,
        skip_validation,
        target_world,
        export_binding_names,
        component_plan,
        builtin_registry,
        wasm_assets,
        trait_env,
        #[cfg(debug_assertions)]
        pruned,
    } = flat;

    // Pre-register every in-package function's canonical `FuncId` (its position,
    // 1:1 with the final function list). Calls stamp against this at construction.
    let mut ids: IndexMap<FunctionId, FuncId> = IndexMap::default();
    for (i, func_rc) in functions.iter().enumerate() {
        let key = tir_function_key(&func_rc.borrow());
        let prev = ids.insert(key, FuncId::new(i));
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
        points_to,
        closure: &closure,
        type_table: Rc::clone(&type_table),
        struct_fields_map,
        interner: RefCell::new(Interner {
            ids,
            stubs: Vec::new(),
            base_len,
            #[cfg(debug_assertions)]
            shadowed: Vec::new(),
            #[cfg(debug_assertions)]
            pruned,
        }),
    };

    let mut func_map: IndexMap<*const RefCell<TirFunction>, FuncRef> =
        IndexMap::with_capacity_and_hasher(functions.len(), rustc_hash::FxBuildHasher);
    let functions: Vec<FuncRef> = functions
        .into_iter()
        .map(|func_rc| {
            let ptr = Rc::as_ptr(&func_rc);
            let nir_rc = FuncCell::new(translator.convert_function(&func_rc.borrow()));
            func_map.insert(ptr, Arc::clone(&nir_rc));
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
        imports: Vec::new(),
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
        builtin_declarations,
        module_name,
        cm_interface_registry,
        world_registry,
        used_wasi_functions,
        ambient_stdio: None,
        strip_names,
        coverage_section,
        codegen_flags,
        // Conservative default; `optimize` overrides per opt level.
        string_inline_max_bytes: NirPackage::DEFAULT_STRING_INLINE_MAX_BYTES,
        skip_validation,
        target_world,
        export_binding_names,
        component_plan,
        builtin_registry,
        wasm_assets,
        reserved_memory_pages: None,
        trait_env,
        // Before the interner is drained: a helper only `array_clone::<T>`
        // reaches is never resolved by a wrap, and DCE still has to root it.
        value_copy_helpers: value_copy.helpers.map(|(module_source, name)| {
            translator.interner.borrow_mut().resolve(&nir::FunctionRef {
                module_source: module_source.clone(),
                name: name.clone(),
                monomorph_info: None,
                method_info: None,
            })
        }),
        sroa_param_clones: IndexSet::default(),
        reshapes: IndexMap::default(),
        renamed: Vec::new(),
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
        func_rc.borrow_mut().id = Some(FuncId::new(i));
    }
    nir.func_index = ids;
    nir
}

struct Translator<'a> {
    box_plan: &'a BoxPlan,
    value_copy: &'a value_copy::ValueCopyPlan,
    /// Computed on the TIR as the fold reads it: [`PointsTo::handed`] keys an
    /// argument by its expression.
    points_to: PointsTo,
    closure: &'a closure::ClosurePlan,
    type_table: Rc<RefCell<TypeTable>>,
    struct_fields_map: StructFields,
    /// Mints each call's canonical `FuncId` at construction ("born resolved"),
    /// so the call node never carries a `FunctionRef`. In-package callees resolve
    /// against the pre-built `FunctionId → FuncId` map (positions in
    /// `flat.functions`, 1:1 with the final function list); extern / builtin
    /// callees are interned on first sight as `extern_stub`s appended after the
    /// in-package functions. Replaces the former post-pass `assign_func_ids`.
    interner: RefCell<Interner>,
}

/// Construction-time callee-id minting (see [`Translator::interner`]).
struct Interner {
    ids: IndexMap<FunctionId, FuncId>,
    stubs: Vec<FuncRef>,
    base_len: usize,
    /// Stubs minted for a name the package defines — see `resolve`.
    #[cfg(debug_assertions)]
    shadowed: Vec<String>,
    /// What `prelower_reach` dropped before this phase — see `resolve`.
    #[cfg(debug_assertions)]
    pruned: IndexSet<FunctionId>,
}

impl Interner {
    /// The `FuncId` of `func_ref`: its pre-registered in-package id, or a freshly
    /// interned extern stub's id.
    fn resolve(&mut self, func_ref: &nir::FunctionRef) -> FuncId {
        let key = func_ref.function_id();
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        #[cfg(debug_assertions)]
        assert!(
            !self.pruned.contains(&key),
            "[lower] `{key:?}` was pruned before `lower` and then minted here: \
             this phase spelled a callee that no TIR body names, so \
             `prelower_reach::is_root` owes it a root (WEP 2026-05-26)"
        );
        // A stub is for a callee outside the package. Minting one for a name the
        // package *does* define means the call and the definition disagree on
        // some other part of the identity — the call then binds to a body-less
        // stub and only surfaces as an unsatisfied trait bound at WIR build.
        // A module of intrinsics or imports defines nothing, so a call into one
        // is a stub whatever the package names its own functions.
        #[cfg(debug_assertions)]
        if !func_ref.module_source.is_builtin() && !func_ref.module_source.is_binding() {
            let name = &func_ref.name;
            let same_name = |k: &FunctionId| match k {
                FunctionId::Free(f) => f.name == *name,
                FunctionId::Method(_) => false,
            };
            if let Some(defined) = self.ids.keys().find(|k| same_name(k)) {
                self.shadowed
                    .push(format!("{key:?} vs defined {defined:?}"));
            }
        }
        let id = FuncId::new(self.base_len + self.stubs.len());
        let mut stub = NirFunction::extern_stub(func_ref);
        stub.id = Some(id);
        self.stubs.push(FuncCell::new(stub));
        self.ids.insert(key, id);
        id
    }
}

/// The canonical `FunctionId` of a TIR function — the same identity its
/// converted `NirFunction` yields (`FunctionRef::function_id`), so the
/// pre-built id map agrees with every call site's `convert_function_ref`.
pub(super) fn tir_function_key(f: &TirFunction) -> FunctionId {
    nir::FunctionRef {
        module_source: f.module_source.clone(),
        name: f.name.clone(),
        monomorph_info: f.monomorph_info.as_ref().map(convert_monomorph_info),
        method_info: f.method_info.clone(),
    }
    .function_id()
}

/// The locals declared without a value (`let x: T;`, a unit initializer) that an
/// assignment writing in place replaces whole, or that live in a `Box`.
fn guarded_locals(func: &TirFunction, boxed: impl Fn(u32, tir::TypeId) -> bool) -> IndexSet<u32> {
    struct Scan {
        deferred: IndexMap<u32, tir::TypeId>,
        replaced: IndexSet<u32>,
    }
    impl TirRefVisitor for Scan {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if let TirStmtKind::Let {
                local_index,
                type_id,
                value,
                ..
            } = &stmt.kind
                && *type_id != TypeTable::UNIT
                && value.type_id == TypeTable::UNIT
            {
                self.deferred.insert(*local_index, *type_id);
            }
            self.walk_stmt(stmt);
        }
        fn visit_expr(&mut self, expr: &TirExpr) {
            if let TirExprKind::Assign { target, .. } = &expr.kind
                && let TirExprKind::Local { index, .. } = target.kind
                && expr.marks().in_place
            {
                self.replaced.insert(index);
            }
            self.walk_expr(expr);
        }
    }
    let Some(body) = &func.body else {
        return IndexSet::default();
    };
    let mut scan = Scan {
        deferred: IndexMap::default(),
        replaced: IndexSet::default(),
    };
    scan.visit_block(body);
    scan.deferred
        .into_iter()
        .filter(|&(local, ty)| scan.replaced.contains(&local) || boxed(local, ty))
        .map(|(local, _)| local)
        .collect()
}

/// Per-function translation context. Created fresh for each function
/// the translator walks (top-level functions, generated `$call`
/// methods, fn-param specialized callees).
struct FunctionTranslator<'a, 'p> {
    base: &'a Translator<'p>,
    /// This function's own environment — the `self` local and its type — `Some`
    /// only inside a `$call` method. A nested closure reads through it.
    enclosing_env: Option<(u32, tir::TypeId)>,
    /// `Some` only inside a synthesized fn-param-specialized callee.
    specialized: Option<&'p [closure::SpecializedLocal]>,
    /// `None` for global initializers / struct field defaults, where
    /// `alloc_local` would have no function to attach to.
    extra: Option<ExtraLocals>,
    immutable_locals: IndexSet<u32>,
    address_taken: IndexSet<u32>,
    /// The reads whose copy is elided because they hand their storage over.
    moves: value_copy::last_use::MoveEligible,
    /// Whether this function hands a returned variant's payload out uncopied,
    /// so `return Some(place)` delivers the place uncopied exactly as `return place`
    /// does ([`value_copy::hands_out_payload`]).
    hands_out_payload: bool,
    /// Locals whose binding copy is elided by sharing the source storage
    /// (WEP 2026-05-21 read-only-share): a read-only local bound from a
    /// projection whose storage is provably never mutated while it is live.
    share_eligible_locals: IndexSet<u32>,
    /// The root each reference local is taken over, so an immutable-root rule
    /// asks about the place that owns it rather than the reference.
    ref_targets: value_copy::last_use::RefTargets,
    /// Locals a last-use move can hand to a new owner
    /// ([`value_copy::last_use::MoveEligible::roots`]).
    moved_roots: IndexSet<u32>,
    /// Whether the ownership walk marked which arguments are written
    /// ([`tir::ExprMarks::written`]). Where it did not run, every argument is
    /// copied.
    args_checked: bool,
    /// Locals declared without a value that an assignment writing in place
    /// ([`tir::ExprMarks::in_place`]) replaces whole.
    /// One holds no storage before its first assignment, so a flag local says
    /// whether to write in place or to store.
    guarded_locals: IndexSet<u32>,
    /// The flag local of each guarded local, minted at its declaration.
    assigned_flags: RefCell<IndexMap<u32, u32>>,
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
        let enclosing_env = base.closure.call_method_envs.get(&key).copied();
        let immutable_locals = func
            .locals
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_mut)
            .map(|(i, _)| u32::try_from(i).unwrap())
            .collect();
        let address_taken = func.address_taken_locals.clone();
        // The move/share/alias analyses only ever mark copyable-value locals; a
        // function with none has nothing to elide, so all three are empty. Skip
        // them — running them is otherwise pure per-function allocation, and most
        // functions (scalar/reference-only) hit this path. A referenced local is
        // already retyped to its box, and the value it holds is what counts, as
        // is what a reference leads to, which `*r` hands to a call.
        let needs_copy_analysis = {
            let tt = base.type_table.borrow();
            func.params
                .iter()
                .map(|p| p.type_id)
                .chain(func.locals.iter().map(|l| l.type_id))
                .map(|tid| {
                    tt.peel_refs_and_box(base.box_plan.get_box_inner_type(tid).unwrap_or(tid))
                })
                .any(|tid| value_copy::needs_value_copy(tid, &tt))
        };
        // One resolver for the body, shared by every question about what an
        // expression names: the ownership walk and the reference-root table.
        let (ownership, ref_targets) = if needs_copy_analysis {
            let type_table = base.type_table.borrow();
            let resolver = place::Resolver::new(
                func,
                &type_table,
                &base.value_copy.return_paths,
                &base.value_copy.returns_owned,
                &base.value_copy.builtins,
            );
            let oracle = value_copy::ownership::OwnedCalls::new(
                &base.value_copy.returns_owned,
                &base.value_copy.returns_projection,
                &base.value_copy.builtins,
                &base.value_copy.retention,
            )
            .with_indirect(&base.value_copy.indirect_owned_returns);
            (
                value_copy::last_use::analyze_ownership(
                    func,
                    &oracle,
                    &type_table,
                    &resolver,
                    base.value_copy,
                    &base.points_to,
                    &value_copy::reach::StorageReach::new(
                        &type_table,
                        &base.struct_fields_map,
                        &base.value_copy.closure_captures,
                    ),
                ),
                value_copy::last_use::compute_ref_targets(func, &resolver),
            )
        } else {
            (
                value_copy::last_use::Ownership::default(),
                value_copy::last_use::RefTargets::default(),
            )
        };
        let analyzed = needs_copy_analysis
            && func
                .body
                .as_ref()
                .is_some_and(|b| !value_copy::last_use::has_unsupported_form(b));
        if !analyzed {
            value_copy::last_use::mark_unanalyzed_in_place_assigns(
                func,
                &base.type_table.borrow(),
                base.value_copy,
            );
        }
        let guarded_locals = guarded_locals(func, |local, ty| {
            address_taken.contains(&local) && base.box_plan.box_struct_types.contains_key(&ty)
        });
        let share_eligible_locals = ownership.share_eligible;
        let moves = ownership.move_eligible;
        let moved_roots = if needs_copy_analysis {
            moves.roots(func)
        } else {
            IndexSet::default()
        };
        // Keyed on the *return* type rather than `needs_copy_analysis`: the
        // payload is read out of the receiver, so it need not be any local's
        // type.
        let hands_out_payload = value_copy::hands_out_payload(func, &base.value_copy.return_paths)
            && value_copy::needs_value_copy(func.return_type, &base.type_table.borrow());
        Self {
            base,
            enclosing_env,
            specialized,
            extra: Some(ExtraLocals {
                base_count: func.local_count,
                locals: RefCell::new(Vec::new()),
            }),
            immutable_locals,
            address_taken,
            moves,
            hands_out_payload,
            share_eligible_locals,
            ref_targets,
            moved_roots,
            args_checked: analyzed,
            guarded_locals,
            assigned_flags: RefCell::new(IndexMap::default()),
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
            enclosing_env: None,
            specialized: None,
            extra: None,
            immutable_locals: IndexSet::default(),
            address_taken: IndexSet::default(),
            moves: value_copy::last_use::MoveEligible::default(),
            hands_out_payload: false,
            share_eligible_locals: IndexSet::default(),
            ref_targets: value_copy::last_use::RefTargets::default(),
            moved_roots: IndexSet::default(),
            args_checked: false,
            guarded_locals: IndexSet::default(),
            assigned_flags: RefCell::new(IndexMap::default()),
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
        self.alloc_local_with(type_id, name, false)
    }

    fn alloc_local_with(&self, type_id: tir::TypeId, name: String, is_mut: bool) -> u32 {
        let extra = self
            .extra
            .as_ref()
            .expect("alloc_local outside function translation");
        let mut locals = extra.locals.borrow_mut();
        let index = extra.base_count + u32::try_from(locals.len()).unwrap();
        locals.push(TirLocal {
            name,
            type_id,
            is_mut,
            span: Span::default(),
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
    /// The parameters `func` keeps past its return, as the NIR passes ask it.
    ///
    /// The fixpoint's answer, not the declaration's: a function with a body
    /// declares no `#[storage(...)]`, so reading the declaration here would tell
    /// every NIR pass that every bodied function keeps nothing.
    fn retained_param_names(&self, func: &TirFunction) -> Vec<String> {
        let Some(retained) = self.value_copy.retention.of_body(func) else {
            return Vec::new();
        };
        func.params
            .iter()
            .enumerate()
            .filter(|(i, _)| retained.keeps(u32::try_from(*i).unwrap()))
            .map(|(_, p)| p.name.clone())
            .collect()
    }

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
            arena.set_root(r);
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
            retains: self.retained_param_names(func),
            body,
            span: func.span,
            locals: Tracked::new(locals),
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
            scalarized_from: None,
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
            body.set_root(init_root);
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
            // A TIR global holds no body, so its NIR slot starts with no locals;
            // `const_object_globalization` allocates them if it hoists one here.
            locals: Vec::new(),
            prefer_fixed_string_repr: false,
            param_name: global.param.as_ref().map(|p| p.name.clone()),
        }
    }

    fn convert_struct(&self, s: &TirStruct) -> NirStruct {
        let fctx = FunctionTranslator::for_top_level(self);
        NirStruct {
            def: s.def,
            name: s.name.clone(),
            module_source: s.module_source.clone(),
            visibility: s.visibility,
            type_params: s.type_params.iter().map(convert_type_param).collect(),
            monomorph_info: s.monomorph_info.as_ref().map(convert_monomorph_info),
            fields: s.fields.iter().map(|f| fctx.convert_field(f)).collect(),
            span: s.span,
            wire_name_policy: s.wire_name_policy,
        }
    }

    fn convert_closure_functor(
        &self,
        cf: &ClosureFunctor,
        func_map: &IndexMap<*const RefCell<TirFunction>, FuncRef>,
    ) -> nir::ClosureFunctor {
        // A functor's methods are the package's own functions (keyed by
        // pointer), so the optimizer reaches them by the id they carry there.
        let shared = |method: &Rc<RefCell<TirFunction>>| {
            Arc::clone(
                func_map
                    .get(&Rc::as_ptr(method))
                    .expect("the closure plan generates a functor's methods with the functor"),
            )
        };
        nir::ClosureFunctor {
            module_source: cf.module_source.clone(),
            id: cf.id,
            struct_name: cf.struct_name.clone(),
            struct_type_id: cf.struct_type_id,
            ref_type_id: cf.ref_type_id,
            call_method: shared(&cf.call_method),
            inspect_method: shared(&cf.inspect_method),
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
        if self.moves.is_move(value) {
            return false;
        }
        let oracle = value_copy::ownership::OwnedCalls::new(
            &self.base.value_copy.returns_owned,
            &self.base.value_copy.returns_projection,
            &self.base.value_copy.builtins,
            &self.base.value_copy.retention,
        )
        .with_indirect(&self.base.value_copy.indirect_owned_returns);
        let type_table = self.base.type_table.borrow();
        let wrap = |fresh: &IndexSet<u32>| {
            value_copy::analyze::should_wrap(value, fresh, &type_table, &oracle)
        };
        // `dead.unwrap()` is as fresh as `dead`: a call handing its argument
        // back hands over whatever the argument's read hands over.
        wrap(&IndexSet::default()) && wrap(&self.moves.moved_reads(value, &type_table))
    }

    /// Whether an immutable binding may alias `value`'s storage instead of
    /// copying it: the source must be rooted at an immutable local whose
    /// storage is never moved to a new owner.
    fn source_shares_immutable_storage(&self, local_index: u32, value: &TirExpr) -> bool {
        // The alias the binding takes outlives the binding: a move hands its
        // storage to a new — possibly mutable — owner, and a write through that
        // owner is one the source observes. Either end being moved keeps the
        // copy.
        if self.moved_roots.contains(&local_index) {
            return false;
        }
        let type_table = self.base.type_table.borrow();
        if !value_copy::analyze::is_source_immutable(
            value,
            &self.immutable_locals,
            &type_table,
            &self.ref_targets,
        ) {
            return false;
        }
        value_copy::analyze::source_root(value, &type_table, &self.ref_targets)
            .is_some_and(|root| !self.moved_roots.contains(&root))
    }

    /// Apply a boxing-derived rewrite to `expr`, returning `Some` if
    /// one fires. Matches raw TIR shapes — see the per-case helpers
    /// for the four rewrites (`Local` retag, `&local` collapse, `&x`
    /// wrap, `*box` projection).
    fn try_boxing_rewrite(&self, expr: &TirExpr) -> Option<ExprId> {
        match &expr.kind {
            TirExprKind::Local { index, name } if self.address_taken.contains(index) => {
                self.base.box_plan.box_struct_types.get(&expr.type_id)?;
                Some(self.read_local(*index, name, expr.type_id, expr.span))
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
        // A slot holding an affine variant holds it in a box, which is the
        // place: a fresh box would detach it. `&*r` is `r`, and `&s.f` the box
        // the field holds.
        if self.base.type_table.borrow().boxes_slot(inner.type_id) {
            match &inner.kind {
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: reference,
                } => return Some(self.convert_expr(reference)),
                // A builtin rather than `Unary::Ref`, which the optimizer reads
                // as the referent itself.
                TirExprKind::FieldAccess {
                    expr: receiver,
                    field_index,
                    ..
                } => {
                    let is_reference = matches!(
                        self.base.type_table.borrow().get(receiver.type_id),
                        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                    );
                    let holder = if is_reference {
                        (**receiver).clone()
                    } else {
                        let holder_type = self
                            .base
                            .type_table
                            .borrow_mut()
                            .make_mut_ref(receiver.type_id);
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::MutRef,
                                expr: receiver.clone(),
                            },
                            holder_type,
                            span,
                        )
                    };
                    let field = TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: u64::from(*field_index),
                            repr: field_index.to_string(),
                        },
                        TypeTable::I32,
                        span,
                    );
                    let slot_box = self
                        .base
                        .type_table
                        .borrow()
                        .slot_box(inner.type_id)
                        .expect("the boxing plan registers a box for every slot that boxes");
                    return Some(self.convert_expr(&builtin_call(
                        "field_slot",
                        vec![holder, field],
                        slot_box,
                    )));
                }
                _ => {}
            }
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

    /// The fields an in-place aggregate (`TypeTable::is_in_place_aggregate`) is
    /// written through: a plain struct's from `struct_fields_map`, a `List<T>`'s
    /// `{repr, used}` from `SeqField`, and a tuple's positional ones.
    fn aggregate_fields(&self, ty: tir::TypeId) -> Vec<(String, u32, tir::TypeId)> {
        let resolved = self.base.type_table.borrow().get(ty).clone();
        if let ResolvedType::Struct { def, type_args } = resolved {
            return self.base.struct_fields_map[&(def, type_args)]
                .iter()
                .map(|f| (f.name.clone(), f.index, f.type_id))
                .collect();
        }
        let (list_elem, tuple_elems) = {
            let tt = self.base.type_table.borrow();
            (tt.list_element(ty), tt.as_tuple(ty))
        };
        if let Some(elem) = list_elem {
            let repr_ty = self.base.type_table.borrow_mut().make_builtin_array(elem);
            return [
                (SeqField::Backing, repr_ty),
                (SeqField::Len, TypeTable::I32),
            ]
            .into_iter()
            .map(|(field, ty)| (field.field_name().to_string(), field.index(), ty))
            .collect();
        }
        tuple_elems
            .expect("an in-place aggregate is a struct, a `List` or a tuple")
            .into_iter()
            .enumerate()
            .map(|(i, ty)| (i.to_string(), u32::try_from(i).expect("tuple arity"), ty))
            .collect()
    }

    /// The scalar and reference fields of an in-place `ty`, each as the chain
    /// of fields reaching it: a field of an in-place type is followed into its
    /// own fields. Writing every leaf writes a value into the storage already
    /// there at every depth, which any reference into it then reads.
    fn leaf_paths(&self, ty: tir::TypeId) -> Vec<Vec<(String, u32, tir::TypeId)>> {
        fn collect(
            this: &FunctionTranslator<'_, '_>,
            ty: tir::TypeId,
            path: &mut Vec<(String, u32, tir::TypeId)>,
            out: &mut Vec<Vec<(String, u32, tir::TypeId)>>,
        ) {
            for field in this.aggregate_fields(ty) {
                let field_type = field.2;
                path.push(field);
                if this
                    .base
                    .type_table
                    .borrow()
                    .is_in_place_aggregate(field_type)
                {
                    collect(this, field_type, path, out);
                } else {
                    out.push(path.clone());
                }
                path.pop();
            }
        }
        let mut out = Vec::new();
        collect(self, ty, &mut Vec::new(), &mut out);
        out
    }

    /// The statements an assignment lowers to where it does not lower to one
    /// node: one that writes in place, and one to a guarded local. Every
    /// position reaches it through [`Self::convert_expr`], so a mark the
    /// ownership walk made is expanded wherever the assignment sits.
    fn expand_assignment(&self, expr: &TirExpr) -> Option<Vec<StmtId>> {
        match &expr.kind {
            TirExprKind::Assign { target, .. } => {
                if let TirExprKind::Local { index, .. } = target.kind
                    && let Some(&flag) = self.assigned_flags.borrow().get(&index)
                {
                    return Some(self.guarded_assign(expr, flag));
                }
                self.try_expand_in_place_assign(expr)
            }
            TirExprKind::Call { .. } => self.try_expand_in_place_element_assign(expr),
            _ => None,
        }
    }

    /// Expand an assignment that writes into the storage its place holds:
    /// `*ref = value` through an unboxed `&mut` to an in-place aggregate, and
    /// `place = value` where the ownership walk found a holder of that storage
    /// (WEP 2026-10-13). A Box-shaped deref-assign lowers as a single statement
    /// via the regular `try_boxing_rewrite` `Deref` arm.
    fn try_expand_in_place_assign(&self, expr: &TirExpr) -> Option<Vec<StmtId>> {
        let TirExprKind::Assign { target, value } = &expr.kind else {
            return None;
        };
        let handle = match &target.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: ref_expr,
            } if !self.base.box_plan.box_type_ids.contains(&ref_expr.type_id) => {
                let ResolvedType::MutRef(inner) =
                    *self.base.type_table.borrow().get(ref_expr.type_id)
                else {
                    return None;
                };
                if !self.base.type_table.borrow().is_in_place_aggregate(inner) {
                    return None;
                }
                (**ref_expr).clone()
            }
            _ if expr.marks().in_place => {
                // A handle of a reference type, so every later pass reads the
                // writes below as going through it.
                let ref_type = self
                    .base
                    .type_table
                    .borrow_mut()
                    .make_mut_ref(target.type_id);
                TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::MutRef,
                        expr: target.clone(),
                    },
                    ref_type,
                    target.span,
                )
            }
            _ => return None,
        };
        let place_type = target.type_id;
        let span = expr.span;
        let ref_idx = self.alloc_local(handle.type_id, "$deref_ref".to_string());
        let val_idx = self.alloc_local(place_type, "$deref_val".to_string());
        let ref_nir = self.convert_expr(&handle);
        let val_nir = self.assigned_value(value, place_type);
        // Translator-synthesized bindings — never a user-visible defensive
        // copy, and the value's copy decision is made above.
        let mut out = vec![
            self.synthesized_let(ref_idx, "$deref_ref", handle.type_id, ref_nir.into(), span),
            self.synthesized_let(val_idx, "$deref_val", place_type, val_nir, span),
        ];
        self.write_in_place(
            (ref_idx, handle.type_id),
            (val_idx, place_type),
            span,
            &mut out,
        );
        Some(out)
    }

    /// `value` as an assignment writing it in place takes it: copied unless it
    /// is fresh or moved, since the write-back would otherwise share its
    /// storage. The seed walker registers a helper for the type so the wrap
    /// resolves.
    fn assigned_value(&self, value: &TirExpr, place_type: tir::TypeId) -> Operand {
        let converted = self.convert_operand(value);
        if self.should_wrap_value_copy(value) {
            self.wrap_value_copy_operand(converted, place_type)
        } else {
            converted
        }
    }

    /// Expand `xs[i] = value`, a `List::index_assign` call the ownership walk
    /// marked, so an element in bounds is written field by field into its own
    /// storage, every field of an in-place type into that field's storage
    /// too. One out of bounds takes the call itself, which traps with its
    /// message. The list, index and value are evaluated first, as for the call.
    fn try_expand_in_place_element_assign(&self, call: &TirExpr) -> Option<Vec<StmtId>> {
        let TirExprKind::Call { args, .. } = &call.kind else {
            return None;
        };
        if !call.marks().in_place {
            return None;
        }
        let [list, index, value] = args.iter().map(|a| &a.expr).collect::<Vec<_>>()[..] else {
            unreachable!("`index_assign` takes a list, an index and a value")
        };
        let element = self
            .base
            .type_table
            .borrow()
            .as_list(list.type_id)
            .expect("`index_assign` is a `List` method");
        // An affine element is an object, which resource drop synthesis has
        // written the call to drop and fill (WEP: Affine Types).
        if self.base.type_table.borrow().holds_affine_resource(element) {
            return None;
        }
        let span = call.span;

        let list_idx = self.alloc_local(list.type_id, "$list".to_string());
        let index_idx = self.alloc_local(TypeTable::I32, "$index".to_string());
        let val_idx = self.alloc_local(element, "$deref_val".to_string());
        let local = |index: u32, stem: &str, ty: tir::TypeId| {
            TirExpr::new(
                TirExprKind::Local {
                    index,
                    name: format!("{stem}_{index}"),
                },
                ty,
                span,
            )
        };
        let val_nir = self.assigned_value(value, element);
        let mut out = vec![
            self.synthesized_let(
                list_idx,
                "$list",
                list.type_id,
                self.convert_expr(list).into(),
                span,
            ),
            self.synthesized_let(
                index_idx,
                "$index",
                TypeTable::I32,
                self.convert_operand(index),
                span,
            ),
            self.synthesized_let(val_idx, "$deref_val", element, val_nir, span),
        ];

        let list_field = |field: SeqField, ty: tir::TypeId| {
            TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(local(list_idx, "$list", list.type_id)),
                    field_index: field.index(),
                    field_name: field.field_name().to_string(),
                },
                ty,
                span,
            )
        };
        let compare = |left: TirExpr, op: TirBinaryOp, right: TirExpr| {
            TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                },
                TypeTable::BOOL,
                span,
            )
        };
        let zero = TirExpr::new(
            TirExprKind::IntLiteral {
                value: 0,
                repr: "0".to_string(),
            },
            TypeTable::I32,
            span,
        );
        let in_bounds = compare(
            compare(
                zero,
                TirBinaryOp::LtEq,
                local(index_idx, "$index", TypeTable::I32),
            ),
            TirBinaryOp::BitAnd,
            compare(
                local(index_idx, "$index", TypeTable::I32),
                TirBinaryOp::Lt,
                list_field(SeqField::Len, TypeTable::I32),
            ),
        );

        let (array_type, array_ref_type, element_ref_type) = {
            let mut tt = self.base.type_table.borrow_mut();
            let array = tt.make_builtin_array(element);
            (array, tt.make_mut_ref(array), tt.make_mut_ref(element))
        };
        let get = builtin_call(
            "array_get_ref_mut",
            vec![
                TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::MutRef,
                        expr: Box::new(list_field(SeqField::Backing, array_type)),
                    },
                    array_ref_type,
                    span,
                ),
                local(index_idx, "$index", TypeTable::I32),
            ],
            element_ref_type,
        );
        let ref_idx = self.alloc_local(element_ref_type, "$deref_ref".to_string());
        let mut write = vec![self.synthesized_let(
            ref_idx,
            "$deref_ref",
            element_ref_type,
            self.convert_expr(&get).into(),
            span,
        )];
        self.write_in_place(
            (ref_idx, element_ref_type),
            (val_idx, element),
            span,
            &mut write,
        );

        let mut trapping = call.clone();
        let TirExprKind::Call { args: locals, .. } = &mut trapping.kind else {
            unreachable!()
        };
        for (arg, replacement) in locals.iter_mut().zip([
            local(list_idx, "$list", list.type_id),
            local(index_idx, "$index", TypeTable::I32),
            local(val_idx, "$deref_val", element),
        ]) {
            arg.expr = replacement;
        }
        let trap = self.alloc_stmt(
            StmtKind::Expr(self.convert_expr_unexpanded(&trapping).into()),
            span,
        );

        let then_block = self.alloc_block(write, span);
        let else_block = self.alloc_block(vec![trap], span);
        out.push(self.alloc_stmt(
            StmtKind::If {
                condition: self.convert_expr(&in_bounds).into(),
                then_block,
                else_block: Some(else_block),
            },
            span,
        ));
        Some(out)
    }

    fn synthesized_let(
        &self,
        index: u32,
        stem: &str,
        type_id: tir::TypeId,
        value: Operand,
        span: Span,
    ) -> StmtId {
        self.alloc_stmt(
            StmtKind::Let {
                name: format!("{stem}_{index}"),
                local_index: index,
                is_mut: false,
                type_id,
                value,
                skip_value_copy: true,
            },
            span,
        )
    }

    /// Write the value in local `val` into the storage the `&mut` local
    /// `handle` names, leaf by leaf ([`Self::leaf_paths`]), so a reference
    /// into any part of that storage reads the new value. A value made of
    /// parts of the place itself (`x = X { f: x.g, g: x.f }` with the copies
    /// elided) is read in full before anything is written.
    fn write_in_place(
        &self,
        (handle, handle_type): (u32, tir::TypeId),
        (val, val_type): (u32, tir::TypeId),
        span: Span,
        out: &mut Vec<StmtId>,
    ) {
        let chain =
            |local: u32, stem: &str, ty: tir::TypeId, path: &[(String, u32, tir::TypeId)]| {
                let mut expr = self.alloc_expr(
                    ExprKind::Local {
                        index: local,
                        name: format!("{stem}_{local}"),
                    },
                    ty,
                    span,
                );
                for (field_name, field_index, field_type) in path {
                    expr = self.alloc_expr(
                        ExprKind::FieldAccess {
                            expr: expr.into(),
                            field_index: *field_index,
                            field_name: field_name.clone(),
                        },
                        *field_type,
                        span,
                    );
                }
                expr
            };
        let leaves = self.leaf_paths(val_type);
        let nested = leaves.iter().any(|path| path.len() > 1);
        let values: Vec<Operand> = leaves
            .iter()
            .map(|path| {
                let read = chain(val, "$deref_val", val_type, path);
                if !nested {
                    return read.into();
                }
                let leaf_type = path.last().expect("a leaf path names a field").2;
                let temp = self.alloc_local(leaf_type, "$leaf".to_string());
                out.push(self.synthesized_let(temp, "$leaf", leaf_type, read.into(), span));
                let local = self.alloc_expr(
                    ExprKind::Local {
                        index: temp,
                        name: format!("$leaf_{temp}"),
                    },
                    leaf_type,
                    span,
                );
                local.into()
            })
            .collect();
        for (path, value) in leaves.iter().zip(values) {
            let assign = self.alloc_expr(
                ExprKind::Assign {
                    target: chain(handle, "$deref_ref", handle_type, path),
                    value,
                },
                TypeTable::UNIT,
                span,
            );
            out.push(self.alloc_stmt(StmtKind::Expr(assign.into()), span));
        }
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

    /// A destination needing no copy passes through. Where one is due, a
    /// missing helper is a hole in the seed walk, and aliasing there is silent.
    fn wrap_value_copy(&self, value: ExprId, type_id: tir::TypeId) -> ExprId {
        let span = self.expr_span(value);
        let helper = {
            let tt = self.base.type_table.borrow();
            let Some(helper) = self.base.value_copy.helpers.get(type_id, &tt) else {
                assert!(
                    !value_copy::needs_value_copy(type_id, &tt),
                    "the seed walk registered no value-copy helper for {}",
                    tt.mangle_type_arg_for_generic(type_id)
                );
                return value;
            };
            helper.clone()
        };
        let func = nir::FunctionRef {
            module_source: helper.0,
            name: helper.1,
            monomorph_info: None,
            method_info: None,
        };
        let func_id = self.base.interner.borrow_mut().resolve(&func);
        self.alloc_expr(
            ExprKind::Call {
                func_id,
                type_args: vec![],
                args: CallArgs::free(vec![ArenaCallArg {
                    expr: value.into(),
                    is_mut: false,
                }]),
            },
            type_id,
            span,
        )
    }

    /// `value` lowered for a store into a place, a literal or an argument: a copy
    /// unless the value is fresh or moved, since the two are independent after.
    fn convert_stored_operand(&self, value: &TirExpr) -> Operand {
        let needs_wrap = self.should_wrap_value_copy(value);
        let value_op = self.convert_operand(value);
        if needs_wrap {
            self.wrap_value_copy_operand(value_op, value.type_id)
        } else {
            value_op
        }
    }

    /// [`Self::wrap_value_copy`] over an operand: a promoted scalar
    /// (`Operand::Value`) is never value-semantic, so only an aggregate is wrapped.
    fn wrap_value_copy_operand(&self, value: Operand, type_id: tir::TypeId) -> Operand {
        match value {
            Operand::Expr(e) => self.wrap_value_copy(e, type_id).into(),
            Operand::Value(_) => value,
        }
    }

    fn convert_block(&self, block: &TirBlock) -> BlockId {
        let mut stmts: Vec<StmtId> = Vec::with_capacity(block.stmts.len());
        for s in &block.stmts {
            match &s.kind {
                TirStmtKind::Let { local_index, .. }
                    if self.guarded_locals.contains(local_index) =>
                {
                    stmts.extend(self.guarded_let(s, *local_index));
                }
                TirStmtKind::Expr(expr) => match self.expand_assignment(expr) {
                    Some(expanded) => stmts.extend(expanded),
                    None => stmts.push(self.convert_stmt(s)),
                },
                _ => stmts.push(self.convert_stmt(s)),
            }
        }
        self.alloc_block(stmts, block.span)
    }

    /// A guarded local's declaration, followed by its flag starting `false`.
    fn guarded_let(&self, stmt: &TirStmt, local: u32) -> Vec<StmtId> {
        let span = stmt.span;
        let flag = self.alloc_local_with(TypeTable::BOOL, "$assigned".to_string(), true);
        self.assigned_flags.borrow_mut().insert(local, flag);
        let start = self.convert_operand(&TirExpr::new(
            TirExprKind::BoolLiteral(false),
            TypeTable::BOOL,
            span,
        ));
        let declare = self.alloc_stmt(
            StmtKind::Let {
                name: format!("$assigned_{flag}"),
                local_index: flag,
                is_mut: true,
                type_id: TypeTable::BOOL,
                value: start,
                skip_value_copy: true,
            },
            span,
        );
        vec![self.convert_stmt(stmt), declare]
    }

    /// A whole assignment to a guarded local, wherever it sits: it stores and
    /// raises the flag while the local holds no storage yet, and writes in
    /// place once it does.
    fn guarded_assign(&self, assign: &TirExpr, flag: u32) -> Vec<StmtId> {
        let span = assign.span;
        let flag_read = || {
            self.alloc_expr(
                ExprKind::Local {
                    index: flag,
                    name: format!("$assigned_{flag}"),
                },
                TypeTable::BOOL,
                span,
            )
        };
        let raised = self.convert_operand(&TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            span,
        ));
        let raise = self.alloc_expr(
            ExprKind::Assign {
                target: flag_read(),
                value: raised,
            },
            TypeTable::UNIT,
            span,
        );
        let TirExprKind::Assign { target, value } = &assign.kind else {
            unreachable!("a guarded assignment is an `Assign`");
        };
        let TirExprKind::Local { index, name } = &target.kind else {
            unreachable!("a guarded assignment writes a local");
        };
        let boxed = self.box_type_of_local(*index, target.type_id);
        // A boxed local's first assignment allocates its box; every later one
        // writes through it, where a reference taken since may read it.
        let first_write = match boxed {
            Some(box_type) => {
                let boxed_value = self.wrap_in_box(self.convert_operand(value), box_type, span);
                let local = self.alloc_expr(
                    ExprKind::Local {
                        index: *index,
                        name: name.clone(),
                    },
                    box_type,
                    span,
                );
                self.alloc_expr(
                    ExprKind::Assign {
                        target: local,
                        value: boxed_value.into(),
                    },
                    TypeTable::UNIT,
                    span,
                )
            }
            None => self.convert_expr_unexpanded(assign),
        };
        let store = vec![
            self.alloc_stmt(StmtKind::Expr(first_write.into()), span),
            self.alloc_stmt(StmtKind::Expr(raise.into()), span),
        ];
        let in_place = match boxed {
            Some(_) => vec![self.alloc_stmt(
                StmtKind::Expr(self.convert_expr_unexpanded(assign).into()),
                span,
            )],
            None => match self.try_expand_in_place_assign(assign) {
                Some(in_place) => in_place,
                None => return store,
            },
        };
        let then_block = self.alloc_block(in_place, span);
        let else_block = self.alloc_block(store, span);
        vec![self.alloc_stmt(
            StmtKind::If {
                condition: flag_read().into(),
                then_block,
                else_block: Some(else_block),
            },
            span,
        )]
    }

    /// The `Box` a local of type `ty` lives in, where its address is taken.
    fn box_type_of_local(&self, local: u32, ty: tir::TypeId) -> Option<tir::TypeId> {
        if !self.address_taken.contains(&local) {
            return None;
        }
        self.base.box_plan.box_struct_types.get(&ty).copied()
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
                type_id,
                value,
                storage,
            } => {
                // Copy first, then box: a boxed local changes only its
                // storage cell, not its value semantics. A local declared
                // without a value gets its box at its first assignment
                // ([`Self::guarded_assign`]).
                let box_type = self.box_type_of_local(*local_index, *type_id);
                let effective_type = box_type.unwrap_or(*type_id);
                let box_wrap_type = box_type.filter(|_| !self.guarded_locals.contains(local_index));
                let needs_value_copy_wrap = !storage.skips_copy()
                    && !self.share_eligible_locals.contains(local_index)
                    && (*is_mut || !self.source_shares_immutable_storage(*local_index, value))
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
                    type_id: effective_type,
                    value: value_op,
                    skip_value_copy: storage.skips_copy(),
                }
            }
            TirStmtKind::Expr(expr) => StmtKind::Expr(self.convert_operand(expr)),
            TirStmtKind::Return { value } => StmtKind::Return {
                value: value.as_ref().map(|v| self.convert_returned_operand(v)),
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
            TirStmtKind::LetDestructure { pattern, value } => {
                let value = self.convert_stored_operand(value);
                StmtKind::LetDestructure {
                    pattern: self.convert_pattern(pattern),
                    value,
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
        // function's value pool — they never exist as an `ExprKind` (pure
        // scalars live only in the graph). `alloc_unshared`
        // keeps each constant's source width (a type-erased `7` of `i32` vs
        // `i64` must not collide).
        let vk = match &expr.kind {
            // A wide int is a struct, and `u64` could not hold one anyway:
            // fall through to `convert_expr`'s constructor call.
            TirExprKind::IntLiteral { value, .. } => self
                .wide_int_of(expr.type_id)
                .is_none()
                .then_some(ValueKind::Int(*value, expr.type_id)),
            TirExprKind::FloatLiteral { value, .. } => {
                Some(ValueKind::Float(value.to_bits(), expr.type_id))
            }
            TirExprKind::BoolLiteral(b) => Some(ValueKind::Bool(*b)),
            TirExprKind::CharLiteral(c) => Some(ValueKind::Char(*c)),
            // Pure constants whose WIR depends only on type/bytes (read back
            // from the pool by the extractor): `Null` → `None`/`ref.null`,
            // string → `seq_literal`, unit → no runtime value.
            TirExprKind::Null => {
                // At a primitive slot a `ref.null` reaches codegen and fails
                // Wasm validation, naming an offset rather than this site.
                assert!(
                    !matches!(
                        self.base.type_table.borrow().get(expr.type_id),
                        ResolvedType::Primitive(_)
                    ),
                    "[lower] `null` typed as a primitive: a synthesized placeholder \
                     at a primitive slot needs a zero of that type, not a reference null"
                );
                Some(ValueKind::Null)
            }
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
            None => self
                .intern_constant_arith(expr)
                .unwrap_or_else(|| Operand::Expr(self.convert_expr(expr))),
        }
    }

    /// Intern all-literal arithmetic as one pool value rather than building
    /// skeleton nodes for it. Sound this early because such a value names
    /// nothing outside the pool, so no later pass can re-contextualize it. A
    /// value naming a local can be, which is why the early freeze refuses one.
    fn intern_constant_arith(&self, expr: &TirExpr) -> Option<Operand> {
        // Only a compound: a bare literal took the caller's path above.
        if !matches!(
            expr.kind,
            TirExprKind::Binary { .. } | TirExprKind::Unary { .. } | TirExprKind::Cast { .. }
        ) || !self.is_constant_arith(expr)
        {
            return None;
        }
        let type_table = self.base.type_table.borrow();
        let mut arena = self.arena.borrow_mut();
        let v = self.constant_arith_in(expr, &mut arena.values, &type_table)?;
        // Only a tree that folded all the way to a constant is taken. One that
        // did not is a computation, and a computation belongs in the skeleton
        // until a pass with the position in hand decides otherwise: `1 / 0`
        // traps, and an unread `let` holding it has to keep its node so the
        // elider can see the trap it would erase.
        arena
            .values
            .kind(v)
            .is_operand_constant()
            .then_some(Operand::Value(v))
    }

    /// Whether every leaf of `expr` is a literal this lowering can pool, so the
    /// tree denotes one value at every point.
    fn is_constant_arith(&self, expr: &TirExpr) -> bool {
        match &expr.kind {
            // A wide int lowers to a struct constructor, not a pool scalar.
            TirExprKind::IntLiteral { .. } => self.wide_int_of(expr.type_id).is_none(),
            TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_) => true,
            TirExprKind::Binary { left, right, .. } => {
                self.is_constant_arith(left) && self.is_constant_arith(right)
            }
            TirExprKind::Unary { op, expr } => {
                matches!(op, TirUnaryOp::Neg | TirUnaryOp::Not | TirUnaryOp::BitNot)
                    && self.is_constant_arith(expr)
            }
            TirExprKind::Cast { expr, .. } => self.is_constant_arith(expr),
            _ => false,
        }
    }

    /// The pool value for a tree [`Self::is_constant_arith`] admits, folded
    /// through the same helpers the optimizer uses.
    fn constant_arith_in(
        &self,
        expr: &TirExpr,
        pool: &mut ValuePool,
        type_table: &TypeTable,
    ) -> Option<ValueId> {
        let tt = Some(type_table);
        let v = match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => {
                pool.alloc_unshared(ValueKind::Int(*value, expr.type_id), expr.type_id)
            }
            TirExprKind::FloatLiteral { value, .. } => pool.alloc_unshared(
                ValueKind::Float(value.to_bits(), expr.type_id),
                expr.type_id,
            ),
            TirExprKind::BoolLiteral(b) => pool.alloc_unshared(ValueKind::Bool(*b), expr.type_id),
            TirExprKind::CharLiteral(c) => pool.alloc_unshared(ValueKind::Char(*c), expr.type_id),
            TirExprKind::Binary { left, op, right } => {
                let lhs = self.constant_arith_in(left, pool, type_table)?;
                let rhs = self.constant_arith_in(right, pool, type_table)?;
                pool.binary_folded(convert_binary_op(*op), lhs, rhs, expr.type_id, tt)
            }
            TirExprKind::Unary { op, expr: inner } => {
                let operand = self.constant_arith_in(inner, pool, type_table)?;
                pool.unary_folded(convert_unary_op(*op), operand, expr.type_id, tt)
            }
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                let operand = self.constant_arith_in(inner, pool, type_table)?;
                pool.cast_folded(operand, *target_type, tt)
            }
            _ => return None,
        };
        // A folded `Bool` / `Char` carries no width in its kind, so interning
        // records no type and extraction would have none to read.
        pool.set_type(v, expr.type_id);
        Some(v)
    }

    /// Which wide-integer struct `type_id` is; `None` for every other type.
    fn wide_int_of(&self, type_id: tir::TypeId) -> Option<CompilerItem> {
        self.base.type_table.borrow().wide_int_item(type_id)
    }

    fn convert_expr(&self, expr: &TirExpr) -> ExprId {
        if let Some(stmts) = self.expand_assignment(expr) {
            let block = self.alloc_block(stmts, expr.span);
            return self.alloc_expr(
                ExprKind::plain_block(block, expr.type_id, "assign"),
                expr.type_id,
                expr.span,
            );
        }
        self.convert_expr_unexpanded(expr)
    }

    /// [`Self::convert_expr`] for an assignment it has decided to lower as the
    /// one node it is.
    fn convert_expr_unexpanded(&self, expr: &TirExpr) -> ExprId {
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
            let scrut_idx = self.alloc_local(scrutinee.type_id, "$wide_scrut".to_string());
            let scrut_local = TirExpr::new(
                TirExprKind::Local {
                    index: scrut_idx,
                    name: "$wide_scrut".to_string(),
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
                    name: "$wide_scrut".to_string(),
                    local_index: scrut_idx,
                    is_mut: false,
                    type_id: scrutinee.type_id,
                    value: (**scrutinee).clone(),
                    // Translator-synthesized; invisible to
                    // `value_copy::analyze`'s seed walker, so any
                    // wrap would look up a helper that was never
                    // registered.
                    storage: LetStorage::Aliased,
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
        // A wide int (`i128` / `u128`) is a struct, so a literal of one is a
        // constructor call and a comparison an `Eq` / `Ord` call. Pattern
        // lowering emits the scalar forms instead: it holds the type table
        // immutably and cannot intern either call. Every pattern position
        // passes through here, whatever nests it.
        if let TirExprKind::IntLiteral { repr, .. } = &expr.kind
            && let Some(item) = self.wide_int_of(expr.type_id)
        {
            let literal = literal_from_repr(
                item,
                repr,
                expr.type_id,
                &self.base.type_table.borrow(),
                expr.span,
            );
            return self.convert_expr(&literal);
        }
        if let TirExprKind::Binary { left, op, right } = &expr.kind
            && let Some(item) = self.wide_int_of(left.type_id)
            && let Some(call) =
                wide_int_literal::compare(item, *op, left, right, &self.base.type_table, expr.span)
        {
            return self.convert_expr(&call);
        }
        // `Closure` → raw `StructLiteral` (specialisable) or
        // `ClosureToCanonical` wrap (otherwise). The body is never
        // recursed into; it lives in the synthesized `$call`
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
        if let TirExprKind::Local { index, name } = &expr.kind
            && let Some(spec) = self.specialized_for_local(*index)
        {
            return self.read_specialized_local(*index, name, spec, expr.type_id, expr.span);
        }
        // `IndirectCall` whose callee resolves to a specialized
        // fn-param `Local` is dispatched directly to the functor's
        // `$call` method.
        if let TirExprKind::IndirectCall { callee, args } = &expr.kind
            && let TirExprKind::Local { index, name } = &callee.kind
            && let Some(spec) = self.specialized_for_local(*index)
            && let Some(functor) = self
                .base
                .closure
                .functor_infos
                .get(spec.functor_id as usize)
        {
            let nir_receiver = self.specialized_local(*index, name, spec, callee.span);
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
            let receiver = ArenaCallArg {
                expr: nir_receiver.into(),
                is_mut: params_is_mut.first().copied().unwrap_or(false),
            };
            let nir_args = CallArgs::method(
                receiver,
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
            let func = nir::FunctionRef::closure_call(&functor.module_source, functor.id);
            let func_id = self.base.interner.borrow_mut().resolve(&func);
            return self.alloc_expr(
                ExprKind::Call {
                    func_id,
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
        let kind = self.convert_expr_kind(&expr.kind, expr.type_id);
        self.alloc_expr(kind, expr.type_id, expr.span)
    }

    /// A specialized fn-param `Local`, typed as the functor `&$Closure_N` it holds.
    fn specialized_local(
        &self,
        index: u32,
        name: &str,
        spec: &closure::SpecializedLocal,
        span: Span,
    ) -> ExprId {
        self.alloc_expr(
            ExprKind::Local {
                index,
                name: name.to_string(),
            },
            spec.functor_ref_type,
            span,
        )
    }

    /// A read of a specialized fn-param: the `fn(...)` value a `site_type` of
    /// that shape expects, or the functor itself where the site takes `&$Closure_N`.
    fn read_specialized_local(
        &self,
        index: u32,
        name: &str,
        spec: &closure::SpecializedLocal,
        site_type: tir::TypeId,
        span: Span,
    ) -> ExprId {
        let functor = self.specialized_local(index, name, spec, span);
        if !matches!(
            self.base.type_table.borrow().get(site_type),
            tir::ResolvedType::Function { .. }
        ) {
            return functor;
        }
        let info = &self.base.closure.functor_infos[spec.functor_id as usize];
        self.alloc_expr(
            ExprKind::ClosureToCanonical {
                functor: functor.into(),
                functor_id: spec.functor_id,
                target_fn_type: site_type,
                closure_module: info.module_source.clone(),
            },
            site_type,
            span,
        )
    }

    /// `result_type` is the converted expression's own type, which an unlabeled
    /// TIR block needs to become a labelled NIR one.
    fn convert_expr_kind(&self, kind: &TirExprKind, result_type: tir::TypeId) -> ExprKind {
        match kind {
            // Pure scalar literals are interned into the `ValuePool` and born as
            // `Operand::Value` by `convert_operand`; every literal-bearing
            // position routes through `convert_operand`, so `convert_expr` is
            // never entered on one.
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
                value: self.convert_stored_operand(value),
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
                let value = self.convert_stored_operand(value);
                ExprKind::Assign {
                    target: self.convert_expr(target),
                    value,
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
            } => self.convert_call(func, type_args, args),
            TirExprKind::CmRawCall { target, args } => ExprKind::CmRawCall {
                target: target.clone(),
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
            TirExprKind::Block(block) => {
                ExprKind::plain_block(self.convert_block(block), result_type, "block")
            }
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
                    .map(|e| self.convert_stored_operand(e))
                    .collect(),
            },
            TirExprKind::ArrayLiteral { elements } => ExprKind::ArrayLiteral {
                elements: elements
                    .iter()
                    .map(|e| self.convert_stored_operand(e))
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
            TirExprKind::Move { .. } => unreachable!("drop elaboration removes every move"),
            TirExprKind::VariadicTupleComprehension { .. } => unreachable!(
                "TirExprKind::VariadicTupleComprehension should be expanded by monomorphize before lower::translate runs"
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
                    .map(|a| self.convert_stored_operand(a))
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
                payload: payload.as_ref().map(|p| self.convert_stored_operand(p)),
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
            TirExprKind::WithHandler { .. } => unreachable!(
                "TirExprKind::WithHandler should be desugared by synthesis::effect_dispatch before lower::translate runs"
            ),
        }
    }

    /// Rewrites a `builtin::copy_value::<T>(x)` marker into the helper that
    /// copies `T` — one `synthesize_helpers` planted, or one the source wrote.
    fn convert_call(
        &self,
        func: &FunctionRef,
        type_args: &[tir::TypeId],
        args: &CallArgs<CallArg>,
    ) -> ExprKind {
        if args.len() == 1
            && let Some(type_id) = value_copy::copy_value_type_arg(func)
            && let Some((helper_module, helper_name)) = self
                .base
                .value_copy
                .helpers
                .get(type_id, &self.base.type_table.borrow())
                .cloned()
        {
            let func = nir::FunctionRef {
                module_source: helper_module,
                name: helper_name,
                monomorph_info: None,
                method_info: None,
            };
            let func_id = self.base.interner.borrow_mut().resolve(&func);
            // Bypass `convert_call_arg_at`: wrapping a copy helper's own
            // argument would emit copy(copy(x)).
            return ExprKind::Call {
                func_id,
                type_args: vec![],
                args: CallArgs::free(
                    args.iter()
                        .map(|a| ArenaCallArg {
                            expr: self.convert_operand(&a.expr),
                            is_mut: a.is_mut,
                        })
                        .collect(),
                ),
            };
        }
        if let Some(rewritten) = self.convert_case_bridge_call(func, type_args, args) {
            return rewritten;
        }
        let ordered = self.call_args_in_param_order(func, args);
        let nir_func = convert_function_ref(func);
        let func_id = self.base.interner.borrow_mut().resolve(&nir_func);
        let has_receiver = args.receiver().is_some();
        let list = ordered
            .iter()
            .enumerate()
            .map(|(i, (e, is_mut))| {
                // A receiver the call does not write through is an ordinary
                // argument: a by-value `self` is the callee's to return or
                // keep.
                if has_receiver && i == 0 && *is_mut {
                    self.convert_mut_receiver_arg(e)
                } else if value_copy::analyze::passes_through(
                    &self.base.value_copy.builtins,
                    func,
                    i,
                ) {
                    ArenaCallArg {
                        expr: self.convert_operand(e),
                        is_mut: *is_mut,
                    }
                } else {
                    self.convert_call_arg_at(e, *is_mut, Some(func), i)
                }
            })
            .collect();
        ExprKind::Call {
            func_id,
            type_args: type_args.to_vec(),
            args: args.rebuild(list),
        }
    }

    /// The `N` type args a bridge marker names: the call's own, else the
    /// method's, else the `impl`'s.
    fn marker_type_args<const N: usize>(
        type_args: &[tir::TypeId],
        mi: Option<&tir::MonomorphInfo>,
        marker: &str,
    ) -> [tir::TypeId; N] {
        let ta = if type_args.is_empty() {
            let mi =
                mi.unwrap_or_else(|| panic!("{marker} marker without type args or monomorph info"));
            if mi.method_type_args.is_empty() {
                &mi.impl_type_args
            } else {
                &mi.method_type_args
            }
        } else {
            type_args
        };
        *ta.first_chunk::<N>()
            .unwrap_or_else(|| panic!("{marker} marker expects {N} type args, got {ta:?}"))
    }

    /// A call of the bridge helper `name`, homed where `head_ty` is declared,
    /// with a marker's arguments.
    fn bridge_call(
        &self,
        head_ty: tir::TypeId,
        name: String,
        args: &[CallArg],
        marker: &str,
    ) -> ExprKind {
        let module_source = self
            .base
            .type_table
            .borrow()
            .nominal_head(head_ty)
            .map(|(_, m)| m)
            .unwrap_or_else(|| panic!("{marker} marker on a type with no declaration"));
        let nir_func = nir::FunctionRef {
            module_source,
            name,
            monomorph_info: None,
            method_info: None,
        };
        let func_id = self.base.interner.borrow_mut().resolve(&nir_func);
        ExprKind::Call {
            func_id,
            type_args: vec![],
            args: CallArgs::free(
                args.iter()
                    .map(|a| ArenaCallArg {
                        expr: self.convert_operand(&a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
            ),
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
        let mi = func.monomorph_info.as_ref();

        if func.is_builtin_named("variant_tag") && args.len() == 1 {
            // The `&V` receiver matches the marker's argument, so `discriminant`
            // is the single lowering for the tag read.
            let arg_ty = args[0].expr.type_id;
            let (helper_name, variant_ty) = {
                let tt = self.base.type_table.borrow();
                // The marker's `&T` argument may already be lowered to `Box<V>`.
                let peeled = tt.peel_refs(arg_ty);
                let unboxed = self
                    .base
                    .box_plan
                    .get_box_inner_type(peeled)
                    .or_else(|| match tt.get(peeled) {
                        tir::ResolvedType::Struct { def, type_args }
                            if tt.is_compiler_struct_instance(peeled, CompilerItem::Box) =>
                        {
                            self.base
                                .struct_fields_map
                                .get(&(*def, type_args.clone()))
                                .and_then(|fields| fields.first())
                                .map(|f| f.type_id)
                        }
                        _ => None,
                    })
                    .unwrap_or(peeled);
                let variant_ty = tt.peel_refs(unboxed);
                // A plain variant carries the tag read as a trait method on its
                // declaration. A generic one has no instantiated declaration,
                // so it uses the per-instance helper minted post-monomorphize.
                let items = tt.compiler_items();
                let name = match tt.get(variant_ty) {
                    tir::ResolvedType::GenericInstance { .. } => {
                        variant_tag_helper_name(&tt.mangle_type_arg_unboxed(variant_ty))
                    }
                    _ => MethodName::format_local(
                        &tt.fq_type_name(variant_ty),
                        Some(&items.trait_fq(CompilerItem::ReflectVariant)),
                        items.method_name(CompilerItem::ReflectVariantDiscriminant),
                    ),
                };
                (name, variant_ty)
            };
            return Some(self.bridge_call(variant_ty, helper_name, args, "variant_tag"));
        }

        for (builtin, helper_name) in [
            (
                "struct_field_get",
                field_get_helper_name as fn(&str, &str) -> String,
            ),
            ("struct_field_ref", field_ref_helper_name),
        ] {
            if func.is_builtin_named(builtin) {
                let ta = Self::marker_type_args::<2>(type_args, mi, builtin);
                let name = {
                    let tt = self.base.type_table.borrow();
                    helper_name(
                        &tt.mangle_type_arg_unboxed(ta[0]),
                        &tt.mangle_type_arg_unboxed(ta[1]),
                    )
                };
                return Some(self.bridge_call(ta[0], name, args, builtin));
            }
        }

        // `hole_get<T, V>` names the shape and the hole type; `hole_fmt<T>`
        // and `template_segment<T>` the shape only.
        if func.is_builtin_named("hole_get") {
            let ta = Self::marker_type_args::<2>(type_args, mi, "hole_get");
            let name = {
                let tt = self.base.type_table.borrow();
                hole_get_helper_name(
                    &tt.mangle_type_arg_unboxed(ta[0]),
                    &tt.mangle_type_arg_unboxed(ta[1]),
                )
            };
            return Some(self.bridge_call(ta[0], name, args, "hole_get"));
        }
        for (builtin, helper_name) in [
            ("hole_fmt", hole_fmt_helper_name as fn(&str) -> String),
            ("template_segment", template_segment_helper_name),
        ] {
            if func.is_builtin_named(builtin) {
                let ta = Self::marker_type_args::<1>(type_args, mi, builtin);
                let name =
                    helper_name(&self.base.type_table.borrow().mangle_type_arg_unboxed(ta[0]));
                return Some(self.bridge_call(ta[0], name, args, builtin));
            }
        }

        let helper_name_for: fn(&str, &str) -> String =
            if func.is_builtin_named("variant_case_extract") {
                case_extract_helper_name
            } else if func.is_builtin_named("variant_case_construct") {
                case_construct_helper_name
            } else {
                return None;
            };

        let ta = Self::marker_type_args::<2>(type_args, mi, "case bridge");
        let name = {
            let tt = self.base.type_table.borrow();
            helper_name_for(
                &tt.mangle_type_arg_unboxed(ta[0]),
                &tt.mangle_type_arg_unboxed(ta[1]),
            )
        };
        Some(self.bridge_call(ta[0], name, args, "case bridge"))
    }

    fn convert_pattern(&self, pattern: &TirPattern) -> PatId {
        let kind = match pattern {
            TirPattern::PerInstance { .. } => {
                unreachable!("instance_patterns lowers every per-instance pattern")
            }
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
                case_index,
                bindings,
                payload_type,
            } => PatKind::Variant {
                enum_type: *enum_type,
                variant_name: variant_name.clone(),
                case_index: *case_index,
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
            TirPattern::Narrow { .. } => {
                panic!("pattern lowering turns every narrowing into a binding and a guard")
            }
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
            value: self.convert_stored_operand(&field.value),
            field_index: field.field_index,
        }
    }

    /// Convert a `return` value. A variant construction on its spine hands the
    /// payload out uncopied where this function does that at all
    /// ([`value_copy::analyze::returned_value`]) — recognized by where the
    /// conversion arrived, since a span can be shared with a synthesized node
    /// that is not returned.
    fn convert_returned_operand(&self, value: &TirExpr) -> Operand {
        let TirExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload: Some(payload),
        } = &value.kind
        else {
            return self.convert_operand(value);
        };
        if !self.hands_out_payload {
            return self.convert_operand(value);
        }
        let kind = ExprKind::VariantConstruct {
            variant_type: *variant_type,
            case_index: *case_index,
            case_name: case_name.clone(),
            payload: Some(self.convert_returned_operand(payload)),
        };
        self.alloc_expr(kind, value.type_id, value.span).into()
    }

    /// The single `Array<u8>` type every `String` / `List<u8>` literal uses for
    /// its `repr` field. The two share one canonical backing type, read off the
    /// always-loaded `String` struct through the compiler-item registry rather
    /// than a `"String"` literal: a bytes-only program may never monomorphize
    /// `List<u8>` into `struct_fields_map`, but `String` is always present.
    fn seq_u8_repr_type(&self) -> tir::TypeId {
        let string_head = StructDef::Decl(
            self.base
                .type_table
                .borrow()
                .compiler_item_def(CompilerItem::String)
                .expect("the `String` compiler item is declared"),
        );
        self.base
            .struct_fields_map
            .get(&(string_head, Vec::new()))
            .and_then(|fields| {
                fields
                    .iter()
                    .find(|f| f.name == SeqField::Backing.field_name())
            })
            .map(|f| f.type_id)
            .expect("String struct (repr field) is always loaded")
    }

    /// The element of a sequence literal's type, and the array type holding
    /// it. A `String` and every byte sequence share one `Array<u8>`.
    fn packed_layout(&self, seq_type_id: tir::TypeId) -> (PrimitiveType, tir::TypeId) {
        let elem = self.base.type_table.borrow().packed_element(seq_type_id);
        match elem {
            Some(PrimitiveType::U8) | None => (PrimitiveType::U8, self.seq_u8_repr_type()),
            Some(prim) => {
                let array = self
                    .base
                    .type_table
                    .borrow_mut()
                    .make_builtin_array(TypeTable::primitive_type_id(prim));
                (prim, array)
            }
        }
    }

    fn seq_literal(&self, seq_type_id: tir::TypeId, bytes: Vec<u8>, span: Span) -> ExprId {
        let (elem, repr_ty) = self.packed_layout(seq_type_id);
        let data = PackedData::new(bytes, elem);
        let len = i32::try_from(data.len()).expect("seq literal length fits i32");
        let is_array = {
            let tt = self.base.type_table.borrow();
            matches!(
                tt.get(tt.representation_head(seq_type_id)),
                ResolvedType::BuiltinArray(_)
            )
        };
        if is_array {
            return self.alloc_expr(ExprKind::PackedArray(data), seq_type_id, span);
        }
        let packed = self.alloc_expr(ExprKind::PackedArray(data), repr_ty, span);
        let used_val = self.arena.borrow_mut().values.alloc_unshared(
            ValueKind::Int(i64::from(len) as u64, TypeTable::I32),
            TypeTable::I32,
        );
        let kind = ExprKind::StructLiteral {
            struct_type: seq_type_id,
            struct_name: self
                .base
                .type_table
                .borrow()
                .struct_type_rendered_name(seq_type_id),
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

    /// Read a local, through the box when [`crate::lower::plan::boxing`]
    /// promoted its slot: the box *is* the storage, so the value stands one
    /// field in.
    fn read_local(&self, index: u32, name: &str, type_id: tir::TypeId, span: Span) -> ExprId {
        let boxed = self
            .address_taken
            .contains(&index)
            .then(|| self.base.box_plan.box_struct_types.get(&type_id).copied())
            .flatten();
        let local = self.alloc_expr(
            ExprKind::Local {
                index,
                name: name.to_string(),
            },
            boxed.unwrap_or(type_id),
            span,
        );
        let Some(_) = boxed else { return local };
        self.alloc_expr(
            ExprKind::FieldAccess {
                expr: local.into(),
                field_index: 0,
                field_name: "value".to_string(),
            },
            type_id,
            span,
        )
    }

    /// Arena struct-field values for a closure's captures. Mirrors the TIR-side
    /// `build_capture_fields` the closure planner uses for specialized closures
    /// at `Let` bindings.
    fn build_arena_capture_fields(
        &self,
        captures: &[TirCapture],
        span: Span,
    ) -> Vec<ArenaStructField> {
        captures
            .iter()
            .enumerate()
            .map(|(i, cap)| {
                let value = match cap.source {
                    CaptureSource::Local(index) => match self.specialized_for_local(index) {
                        Some(spec) => {
                            self.read_specialized_local(index, &cap.name, spec, cap.type_id, span)
                        }
                        None => self.read_local(index, &cap.name, cap.type_id, span),
                    },
                    CaptureSource::Capture(slot) => self.read_enclosing_capture(cap, slot, span),
                };
                ArenaStructField {
                    name: closure_capture_field(i as u32),
                    value: value.into(),
                    field_index: i as u32,
                }
            })
            .collect()
    }

    /// `self.$capture_<slot>` of the `$call` method being translated, for a
    /// binding its own closure reached the same way.
    fn read_enclosing_capture(&self, cap: &TirCapture, slot: u32, span: Span) -> ExprId {
        let Some((self_index, self_type)) = self.enclosing_env else {
            unreachable!(
                "at {}: capture `{}` reads slot {slot} of an enclosing environment, \
                 but the translated function has none",
                span.location(),
                cap.name,
            );
        };
        let self_expr = self.alloc_expr(
            ExprKind::Local {
                index: self_index,
                name: "self".to_string(),
            },
            self_type,
            span,
        );
        self.alloc_expr(
            ExprKind::FieldAccess {
                expr: self_expr.into(),
                field_index: slot,
                field_name: closure_capture_field(slot),
            },
            cap.type_id,
            span,
        )
    }

    /// Convert the receiver of a call that writes through it. A place receiver
    /// takes no `$value_copy$T`: the copy would hand the callee a throwaway and
    /// discard the mutation the call exists to perform (a `String` builder's
    /// `push_str` would append to the copy). One that is not a place names no
    /// storage the caller can reach again, so the call must not write through it
    /// to whatever it was read out of — there it takes the copy every by-value
    /// argument takes.
    fn convert_mut_receiver_arg(&self, receiver: &TirExpr) -> ArenaCallArg {
        let value = receiver_value(receiver);
        let names_a_place =
            place::is_source_place(value, self.base.type_table.borrow().compiler_items());
        if names_a_place || !self.should_wrap_value_copy(value) {
            return ArenaCallArg {
                expr: self.convert_operand(receiver),
                is_mut: true,
            };
        }
        let copied = self.wrap_value_copy_operand(self.convert_operand(value), value.type_id);
        let expr = match &receiver.kind {
            // Re-take the elaborator's auto-reference over the copy, in the two
            // shapes `try_boxing_ref` builds: a `Box<T>` wrapper for a boxed
            // referent, a plain reference otherwise. Its `&local` collapse
            // cannot apply, a local being a place.
            TirExprKind::Unary {
                op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
                ..
            } => match self.base.box_plan.box_struct_types.get(&value.type_id) {
                Some(&box_type) => self.wrap_in_box(copied, box_type, value.span).into(),
                None => self
                    .alloc_expr(
                        ExprKind::Unary {
                            op: convert_unary_op(*op),
                            expr: copied,
                        },
                        receiver.type_id,
                        receiver.span,
                    )
                    .into(),
            },
            _ => copied,
        };
        ArenaCallArg { expr, is_mut: true }
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
    ) -> ArenaCallArg {
        let needs_value_copy =
            self.should_wrap_value_copy(arg) && !self.arg_confined(arg, callee, param_index);
        let value_type = arg.type_id;
        let converted = self.convert_operand(arg);
        let expr = if needs_value_copy {
            self.wrap_value_copy_operand(converted, value_type)
        } else {
            converted
        };
        ArenaCallArg { expr, is_mut }
    }

    /// Whether a by-value argument into a confined parameter can skip its copy:
    /// nothing writes its storage before the callee is done with it.
    fn arg_confined(
        &self,
        arg: &TirExpr,
        callee: Option<&FunctionRef>,
        param_index: usize,
    ) -> bool {
        callee.is_some_and(|c| {
            self.base
                .value_copy
                .confined_params
                .is_confined(c, param_index)
        }) && self.args_checked
            && !value_copy::last_use::strip_casts(arg).marks().written
    }

    /// A call's arguments paired with their declared `mut`-ness, in the callee's
    /// parameter order. `receiver`, when present, heads the list as parameter 0
    /// and takes its mutability from the callee's `self` parameter.
    fn call_args_in_param_order<'b>(
        &self,
        callee: &FunctionRef,
        args: &'b [CallArg],
    ) -> Vec<(&'b TirExpr, bool)> {
        // Either source saying the callee writes the caller's storage through
        // this slot settles it. The syntax alone misses a receiver, whose
        // `is_mut` TIR leaves unset, and a `&mut` boxing rehomed into a `Box<T>`
        // the callee still writes through. The declared modes alone miss a
        // builtin, which has no row in this table.
        let mut_ref_params = self
            .base
            .value_copy
            .mut_ref_params
            .get(&callee.module_source, &callee.name);
        args.iter()
            .enumerate()
            .map(|(i, a)| {
                let declared = mut_ref_params
                    .and_then(|v| v.get(i))
                    .copied()
                    .unwrap_or(false);
                (&a.expr, a.is_mut || declared)
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
            param_abi: ParamAbi::default(),
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

pub(super) fn convert_function_ref(func: &FunctionRef) -> nir::FunctionRef {
    nir::FunctionRef {
        module_source: func.module_source.clone(),
        name: func.name.clone(),
        monomorph_info: func.monomorph_info.as_ref().map(convert_monomorph_info),
        method_info: func.method_info.clone(),
    }
}

fn convert_function_kind(kind: &tir::FunctionKind) -> nir::FunctionKind {
    match kind {
        tir::FunctionKind::Regular | tir::FunctionKind::UncheckedTwin { .. } => {
            nir::FunctionKind::Regular
        }
        tir::FunctionKind::ValueCopy { type_id } => {
            nir::FunctionKind::ValueCopy { type_id: *type_id }
        }
        tir::FunctionKind::ContractReport => nir::FunctionKind::ContractReport,
        tir::FunctionKind::FnCanonicalDispatch { arity, return_type } => {
            nir::FunctionKind::FnCanonicalDispatch {
                arity: *arity,
                return_type: *return_type,
            }
        }
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
