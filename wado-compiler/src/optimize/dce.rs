//! Dead-code elimination for the NIR package.
//!
//! [`analyze_dce`] computes every reachability set the package needs
//! up front (functions / globals / types, plus name-keyed views over
//! the reachable type set for the type-retain predicate). The
//! downstream [`remove_unreachable_functions`] / `_globals` / `_types`
//! and the literal/closure-functor filters then become pure mutators
//! that consume the precomputed sets; no per-pass re-analysis.
//!
//! See [`crate::optimize::run_dce`] for the orchestration: analyze
//! once, mutate in dependency order.

use crate::hashmap::IndexSet;

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::{
    FqTypeName, FreeFunctionName, FunctionId, MethodName, mangle_generic_name, mangle_local_method,
    mangle_local_trait_method, mangle_method_generic,
};
use crate::nir::{FuncId, FunctionRef, NirFunction, NirImport};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Call graph: function ID -> set of called function IDs
type CallGraph = IndexMap<FunctionId, IndexSet<FunctionId>>;

/// Effect usage: function ID -> set of (`interface_name`, `operation_name`) pairs
type EffectUsageMap = IndexMap<FunctionId, IndexSet<(String, String)>>;

/// A pending `__Closure_N` `inspect/inspect_alt` edge collected during the call-graph
/// walk. The edge is only added to the graph once the inspectable signature set
/// (computed from the reachable-without-inspect-roots set) is known. Storing them
/// out-of-band lets us build the call graph in a single AST walk instead of twice.
#[derive(Debug, Clone)]
struct PendingInspectEdge {
    closure_module: ModuleSource,
    /// `__Closure_{functor_id}` struct name.
    struct_name: crate::name::FqTypeName,
    /// `(arity, return_type)` key into `InspectableSignatures`.
    key: (usize, TypeId),
}

/// Analysis results for a single function
#[derive(Debug, Clone, Default)]
struct FunctionAnalysis {
    /// Functions called by this function
    callees: IndexSet<FunctionId>,
    /// Effect calls: (`interface_name`, `op_name`)
    effect_calls: IndexSet<(String, String)>,
    /// Pending `__Closure_N^Inspect[Alt]::inspect[_alt]` edges. Added to the
    /// graph by `apply_inspect_edges` after the inspectable-signature set is
    /// known.
    pending_inspects: Vec<PendingInspectEdge>,
    /// `(module-path-joined-by-::, name)` pairs that this function reads via
    /// `GlobalVarGet`. Globals only written to (via `GlobalVarSet`) are not
    /// recorded here — those are dead per `remove_unreachable_globals`.
    used_globals: IndexSet<(String, String)>,
    /// Types directly referenced by this function (signature, locals,
    /// expression `type_id`s, and explicit type-bearing fields like
    /// `Cast.target_type`, `StructLiteral.struct_type`, etc.).
    /// Transitive closure happens later in
    /// [`populate_type_reachability`]'s Phase 2.
    used_types: IndexSet<TypeId>,
}

/// Combined DCE analysis: which functions / globals / types are
/// reachable from the project's entry points, plus name-keyed views
/// over the reachable type set that the downstream
/// `remove_unreachable_*` retain predicates need. Computed once up
/// front by [`analyze_dce`], then consumed by pure mutators.
pub struct DceAnalysis {
    /// Indices into `project.functions` that are reachable.
    pub functions: IndexSet<usize>,
    /// `(module-path-joined-by-::, global-name)` pairs that are read
    /// (via `GlobalVarGet`) by some reachable function. Globals only
    /// written to are dead.
    pub globals: IndexSet<(String, String)>,
    /// Reachable type IDs (transitively closed over struct fields,
    /// variant payloads, and per-type dependencies).
    pub types: IndexSet<TypeId>,
    /// Non-monomorphized `Struct` types in `types`, keyed by (name, module).
    pub struct_exact: IndexSet<(String, ModuleSource)>,
    /// Monomorphized struct names in `types` (e.g. `"Box<i32>"`).
    pub struct_monomorph_names: IndexSet<String>,
    /// Base names of monomorphized structs in `types` (e.g. `"Box"`).
    pub struct_monomorph_bases: IndexSet<String>,
    /// `GenericInstance` names in `types`.
    pub generic_instance_names: IndexSet<String>,
    /// `Variant` types in `types`, keyed by (name, module).
    pub variant_exact: IndexSet<(String, ModuleSource)>,
    /// `Enum` types in `types`, keyed by (name, module).
    pub enum_exact: IndexSet<(String, ModuleSource)>,
}

impl DceAnalysis {
    /// Whether `s` survives the sweep.
    ///
    /// The one predicate: the closure that pulls a struct's field types into
    /// the reachable set reads it, and so does the retain that drops the rest.
    /// Two spellings would let a struct be kept whose fields were never
    /// walked, and it would outlive the ids its own fields name.
    ///
    /// A struct's stored `name` predates newtype / flags erasure while the
    /// reachable set renders after it, so one type spells two ways
    /// (`FlagsBit<Perms>` against `FlagsBit<u32>`). Both spellings count.
    fn keeps_struct(&self, s: &crate::nir::NirStruct, type_table: &TypeTable) -> bool {
        let Some(mono) = &s.monomorph_info else {
            return self
                .struct_exact
                .contains(&(s.name.clone(), s.module_source.clone()))
                || self.generic_instance_names.contains(s.name.as_str())
                || self.struct_monomorph_bases.contains(s.name.as_str());
        };
        if self.struct_monomorph_names.contains(s.name.as_str()) {
            return true;
        }
        let rendered = type_table.struct_rendered_name(&mono.generic_name, &mono.impl_type_args);
        self.struct_monomorph_names.contains(rendered.as_str())
            || type_table
                .find_struct_by_name(&rendered, &s.module_source)
                .is_some_and(|id| self.types.contains(&id))
    }
}

/// Compute every DCE input from the unpruned `project` in dependency
/// order: function reachability → global reachability → type
/// reachability. Each downstream step (`remove_unreachable_functions`,
/// `remove_unreachable_globals`, `remove_unreachable_types`) then
/// becomes a pure mutator that consumes the corresponding field.
///
/// Splitting analysis from mutation also means the type-reachability
/// pass can run before `remove_unreachable_globals` mutates function
/// bodies (dropping `GlobalVarSet`s) — those mutations don't expose
/// any new types, but the explicit ordering makes the invariant
/// observable.
pub fn analyze_dce(project: &mut NirPackage) -> DceAnalysis {
    // The callee descriptor for every `FuncId`, materialized once from the
    // function records (borrow-safe: a plain pass, no body walk). A call's
    // callee is identified by its stamped `func_id` (born resolved, authoritative
    // — `wir_build` never falls back to name resolution for a NIR call), and the
    // record at that id carries the identical identity (name / module /
    // method_info / monomorph_info) the call node's `FunctionRef` used to. Indexed
    // by `func_id.index()` (== store position, Phase 4a), so the reachability
    // walk reads identity by id without a self-borrowing `store[id]` deref.
    let descriptors = build_callee_descriptors(project);

    // Single AST walk per function body: build the call graph and
    // collect per-function used-globals / used-types in one go.
    let mut graph = build_analysis_graph(project, &descriptors);

    let mut analysis = DceAnalysis::empty();
    analysis.functions = compute_function_reachability(project, &descriptors, &mut graph);
    analysis.globals = compute_global_reachability(&graph, &analysis.functions);
    populate_type_reachability(project, &descriptors, &graph, &mut analysis);
    analysis
}

/// The callee [`FunctionRef`] descriptor for every function, indexed by
/// `func_id.index()` (== store position). Used so a call site's identity is read
/// by its stamped `func_id` rather than the call node's own `FunctionRef`.
pub(super) fn build_callee_descriptors(project: &NirPackage) -> Vec<FunctionRef> {
    project
        .functions
        .iter()
        .map(|f| {
            let f = f.borrow();
            FunctionRef::from_resolved(&f, f.module_source.clone())
        })
        .collect()
}

/// Resolve a call node's stamped `func_id` to its callee descriptor. `func_id`
/// is total for every NIR call (born resolved): the field is a non-optional
/// [`FuncId`].
pub(super) fn callee_descriptor(descriptors: &[FunctionRef], func_id: FuncId) -> &FunctionRef {
    use cranelift_entity::EntityRef;
    &descriptors[func_id.index()]
}

/// Function reachability via call-graph BFS. Implementation detail of
/// [`analyze_dce`]; not called directly anywhere else.
///
/// Consumes the call graph (and its pending inspect edges) built in
/// the single AST walk of [`build_analysis_graph`]; mutates the graph
/// by adding the gated per-functor `__Closure_N^Inspect[Alt]` edges
/// once the inspectable-signature set is known.
fn compute_function_reachability(
    project: &mut NirPackage,
    descriptors: &[FunctionRef],
    graph: &mut AnalysisGraph,
) -> IndexSet<usize> {
    // Phase 2a: compute the provisional reachable set from the raw graph
    // (without per-functor `__Closure_N^Inspect[Alt]` edges). This is
    // what determines whether a `:?`/`:#?` call site is actually live.
    let reachable_v1 = compute_reachable_from_entries(project, &graph.call_graph);

    // Phase 2b: derive the inspectable `(arity, ret)` set from the
    // reachable functions only, then add the gated inspect edges to the
    // call graph. The per-functor impls themselves don't issue any
    // `Fn^Inspect[Alt]` calls (they just write per-literal strings), so
    // the inspectable set is stable under this expansion — no fixpoint
    // iteration is needed.
    let inspectable =
        collect_inspectable_signatures_from_reachable(project, descriptors, &reachable_v1);
    apply_inspect_edges(&mut graph.call_graph, &graph.pending_inspects, &inspectable);

    // Phase 2c: re-compute the reachable set from the augmented graph.
    let mut reachable = compute_reachable_from_entries(project, &graph.call_graph);

    // Phase 3: extend reachable set with optimizer-induced virtual edges.
    // Optimizer passes (e.g. `nir/string_push`) may *synthesize* new calls
    // during the optimization loop. Functions those passes call must
    // survive the early DCE that runs before the loop, otherwise the
    // synthesis target is gone and the rewrite cannot fire. The virtual
    // edges are gated by compiler-item markers so each rule names its
    // canonical pair (`string_push_str` → `string_push_char`, etc.).
    extend_reachable_for_optimizer_passes(project, descriptors, &graph.call_graph, &mut reachable);

    // Phase 4: resolve imports and WASI features using reachable set.
    resolve_imports(project, &reachable, &graph.effect_usage);

    // Phase 5: project the reachable `FunctionId`s back to positions in
    // `project.functions`. This avoids reallocating `FunctionId`s per
    // function inside `remove_unreachable_functions` (the previous
    // implementation cloned 3-4 strings per function during retain).
    compute_reachable_positions(&reachable, &graph.func_positions)
}

/// Map the reachable-`FunctionId` set back to positions in `project.functions`.
///
/// `build_analysis_graph` asserts `function_id_for` is injective over
/// `project.functions`, so this projection is exhaustive.
fn compute_reachable_positions(
    reachable: &IndexSet<FunctionId>,
    func_positions: &FuncPositions,
) -> IndexSet<usize> {
    reachable
        .iter()
        .filter_map(|id| func_positions.get(id).copied())
        .collect()
}

/// Add functions that the NIR optimizer's rewrites may *synthesize* calls
/// to. For now this is a single pair: `nir/string_push` rewrites
/// `String::push_str("short")` calls into `String::push(c)` calls, so
/// `String::push` (the function flagged with `string_push_char`) must
/// survive early DCE whenever the function flagged with `string_push_str`
/// is reachable.
fn extend_reachable_for_optimizer_passes(
    project: &NirPackage,
    descriptors: &[FunctionRef],
    call_graph: &CallGraph,
    reachable: &mut IndexSet<FunctionId>,
) {
    use crate::compiler_item::CompilerItem;

    let mut push_str: Option<(FunctionId, crate::nir::FuncId)> = None;
    let mut push_char_id: Option<FunctionId> = None;
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        match func.compiler_item {
            Some(CompilerItem::StringPushStr) => {
                push_str = Some((
                    function_id_for(&func),
                    func.id.expect("func_id assigned at lower"),
                ));
            }
            Some(CompilerItem::StringPushChar) => {
                push_char_id = Some(function_id_for(&func));
            }
            _ => {}
        }
    }
    // Keep `String::push` (`string_push_char`) reachable only while a
    // `nir/string_push` rewrite could still fire: the rule turns a short
    // constant `buf.push_str("abc")` into per-byte `buf.push(c)` calls, so its
    // target must survive the DCE that runs *before* the optimization loop.
    // The rule consumes each eligible call, so once the loop has run to
    // fixpoint (the final DCE), no eligible candidate remains — and re-seeding
    // `push_char` there keeps it and its callees alive as pure output bloat.
    // Gating on a surviving candidate makes the virtual edge self-limiting:
    // present at the pre-loop DCE (and at -O0, where the rule never runs), gone
    // at the final DCE. The `$value_copy$` half below is *not* gated this way —
    // its helpers are referenced by name at WIR build (after the final DCE), so
    // both invocations must seed them.
    if let (Some((str_id, str_func_id)), Some(char_id)) = (push_str, push_char_id)
        && reachable.contains(&str_id)
        && !reachable.contains(&char_id)
        && has_short_push_str_candidate(project, str_func_id)
    {
        reachable.extend(compute_reachable(call_graph, &char_id));
    }

    // `$value_copy$` helpers synthesized by `lower::plan::value_copy`
    // can be reached via `array_clone::<T>(arr)` for value-typed `T`: that
    // lowers to `WirInstr::ArrayClone { element_copy_type: Some(T) }`
    // where the helper is referenced by the element *type* at WIR codegen
    // time, not as a NIR call edge — so the regular call graph misses it. Walk every
    // reachable function body and seed the corresponding helper as a
    // virtual root for each value-typed `array_clone::<T>` site.
    //
    // (Marking every `FunctionKind::ValueCopy` helper unconditionally — the
    // previous shape — is correct but bloats unused monomorphisations.)
    //
    // TODO(optimizer): have `lower::plan::value_copy` register the
    // synthesized helper as a real call-graph edge on the caller of
    // `array_clone::<T>`. That folds this fixpoint into the regular
    // reachability walk and removes the only multi-pass step in DCE.
    // Key helpers by the copied type's canonical mangle, not its raw
    // `TypeId`: the type table can intern the same structural type under more
    // than one id, and `lower::plan::value_copy` synthesizes a single helper
    // per mangle. An `array_clone::<T>` site may name a different id for that
    // same type, so a raw-id map would miss the shared helper and DCE would
    // drop it, panicking codegen with "references a value-copy helper ... that
    // was not synthesized".
    // Precompute — while the type table is borrowed — the helper map and,
    // per function, the canonical mangles of every `array_clone::<T>`
    // element it names. Both the helper mangles and the per-site mangles
    // are intern-order-stable, so each is canonicalized exactly once here
    // rather than re-derived on every fixpoint round; the borrow is then
    // released before the loop, so `compute_reachable` never runs under a
    // live `TypeTable` borrow.
    let (helpers_by_mangle, candidates): (
        IndexMap<String, FunctionId>,
        Vec<(FunctionId, Vec<String>)>,
    ) = {
        let type_table = project.type_table.borrow();
        let helpers = project
            .functions
            .iter()
            .filter_map(|func_rc| {
                let func = func_rc.borrow();
                if let crate::nir::FunctionKind::ValueCopy { type_id } = func.kind
                    && type_table.get_pruned(type_id).is_some()
                {
                    Some((
                        type_table.mangle_type_arg_for_generic(type_id),
                        function_id_for(&func),
                    ))
                } else {
                    None
                }
            })
            .collect();
        let candidates = project
            .functions
            .iter()
            .map(|func_rc| {
                let func = func_rc.borrow();
                let func_id = function_id_for(&func);
                let mut mangles = Vec::new();
                if let Some(body) = func.body.as_ref() {
                    let mut needed: IndexSet<crate::tir::TypeId> = IndexSet::default();
                    collect_array_clone_element_types(body, descriptors, &mut needed);
                    for type_id in needed {
                        // A stale `array_clone::<T>` can name a type already
                        // pruned from the table; it has no helper, so skip it
                        // rather than canonicalize an absent id (the mangle
                        // recursion resolves ids through `TypeTable::get`,
                        // which panics on a missing slot). Guarding only the
                        // top-level id is sufficient: DCE's `retain` keeps the
                        // transitive closure over exactly the edges the mangle
                        // recurses through, so a surviving top-level type never
                        // has a pruned mangle-reachable component.
                        if type_table.get_pruned(type_id).is_some() {
                            mangles.push(type_table.mangle_type_arg_for_generic(type_id));
                        }
                    }
                }
                (func_id, mangles)
            })
            .collect();
        (helpers, candidates)
    };
    // Iterate to a fixpoint: a helper newly marked reachable may itself
    // call `array_clone::<T'>` for some `T'` whose helper isn't reachable
    // yet, and `compute_reachable` only follows direct call-graph edges
    // (it doesn't replay the array_clone scan). Single-pass would drop
    // inner helpers for chains like `List<List<List<T>>>`, panicking
    // codegen with `WirInstr::ArrayClone references unknown helper ...`.
    loop {
        let mut added_this_round = false;
        for (func_id, mangles) in &candidates {
            if !reachable.contains(func_id) {
                continue;
            }
            for mangle in mangles {
                if let Some(helper_id) = helpers_by_mangle.get(mangle)
                    && !reachable.contains(helper_id)
                {
                    reachable.extend(compute_reachable(call_graph, helper_id));
                    added_this_round = true;
                }
            }
        }
        if !added_this_round {
            break;
        }
    }
}

/// Whether any function body still holds a `nir/string_push`-rewritable call:
/// `buf.push_str(&"…")` with a 1..=[`SHORT_PUSH_STR_MAX_LEN`]-byte ASCII
/// constant literal. This mirrors the match shape of
/// `optimize::string_push::try_split_stmt` (minus its receiver-duplicability
/// refinement, which only ever narrows the set — so this stays a sound
/// superset that never drops `push_char` while a rewrite could still fire).
/// The pre-loop DCE sees such candidates; the final DCE, after the loop has
/// consumed them, sees none — which is what gates the `String::push` virtual
/// root to the invocation that needs it.
fn has_short_push_str_candidate(project: &NirPackage, push_str_id: crate::nir::FuncId) -> bool {
    project.functions.iter().any(|func_rc| {
        let func = func_rc.borrow();
        func.body
            .as_ref()
            .is_some_and(|body| body_has_short_push_str(body, push_str_id))
    })
}

/// Byte-length ceiling for a `push_str` literal the `nir/string_push` rule
/// expands. Must stay in sync with `string_push::MAX_SHORT_PUSH_STR_LEN`; a
/// value at least as large keeps [`has_short_push_str_candidate`] a sound gate
/// (an over-estimate only risks a little residual bloat, never a dropped
/// rewrite target).
const SHORT_PUSH_STR_MAX_LEN: usize = 8;

fn body_has_short_push_str(body: &Body, push_str_id: crate::nir::FuncId) -> bool {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let Some((_, func_id, args)) = body.exprs[e].kind.as_method_call()
            && func_id == push_str_id
            && args.len() == 1
            && let Some(arg) = args[0].expr.as_expr()
            && let ExprKind::Unary {
                op: crate::nir::NirUnaryOp::Ref,
                expr: inner,
            } = &body.exprs[arg].kind
            && let Some(inner_e) = inner.as_expr()
            && let ExprKind::StructLiteral { fields, .. } = &body.exprs[inner_e].kind
            && let Some(repr) = fields
                .iter()
                .find(|f| f.name == crate::compiler_item::SeqField::Backing.field_name())
                .map(|f| f.value)
            && let Some(repr_e) = repr.as_expr()
            && let ExprKind::PackedArray(bytes) = &body.exprs[repr_e].kind
            && !bytes.is_empty()
            && bytes.len() <= SHORT_PUSH_STR_MAX_LEN
            && bytes.is_ascii()
        {
            return true;
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}

/// Walk `block`'s expression tree and collect every `T` such that
/// `builtin::array_clone::<T>(...)` appears as a NIR call. The
/// corresponding `$value_copy$` helper has to survive DCE because
/// codegen will reach it by *name* at WIR time.
fn collect_array_clone_element_types(
    body: &Body,
    descriptors: &[FunctionRef],
    out: &mut IndexSet<crate::tir::TypeId>,
) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let ExprKind::Call {
                func_id, type_args, ..
            } = &body.exprs[e].kind
        {
            // Identity is the callee (by `func_id`); the element type `T` is
            // call-site data carried on the node's `type_args` (a generic
            // builtin like `array_clone` has no per-`T` record, so the
            // descriptor's `monomorph_info` is generic — only the node knows `T`).
            let func = callee_descriptor(descriptors, *func_id);
            if func.module_source.is_core_builtin()
                && (crate::nir::matches_builtin(
                    &func.name,
                    func.monomorph_info.as_ref(),
                    "array_clone",
                ) || crate::nir::matches_builtin(
                    &func.name,
                    func.monomorph_info.as_ref(),
                    "array_clone_prefix",
                ))
                && let Some(elem) = type_args.first().copied()
            {
                out.insert(elem);
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// Compute reachable functions from all entry points via call graph traversal.
///
/// Entry points are:
/// - `is_cm_export`: synthesized CM export wrappers (world-specific, always correct)
/// - `is_export` in `wasm_module` sources: raw wasm exports with no CM wrapper
fn compute_reachable_from_entries(
    project: &NirPackage,
    call_graph: &CallGraph,
) -> IndexSet<FunctionId> {
    let mut reachable = IndexSet::default();

    for func_rc in &project.functions {
        let func = func_rc.borrow();

        let is_root = func.is_cm_export
            || (func.is_export
                && project
                    .wasm_module_sources
                    .contains_key(&func.module_source));

        if is_root {
            let func_id = FunctionId::Free(FreeFunctionName::from_module_source(
                &func.module_source,
                &func.name,
            ));
            reachable.extend(compute_reachable(call_graph, &func_id));
        }
    }

    reachable
}

/// Resolve WASI imports and populate `project.imports` and `project.used_wasi_functions`
/// from the set of reachable functions and their effect usage.
fn resolve_imports(
    project: &mut NirPackage,
    reachable: &IndexSet<FunctionId>,
    effect_usage: &EffectUsageMap,
) {
    // Collect used WASI functions from reachable functions
    let mut used_wasi_functions: IndexSet<String> = IndexSet::default();
    for func_id in reachable {
        if let Some(effects) = effect_usage.get(func_id) {
            for (interface_name, op_name) in effects {
                used_wasi_functions.insert(format!("{interface_name}::{op_name}"));
            }
        }
    }

    let is_builtin_func = |f: &FreeFunctionName| {
        f.module_source.is_core_builtin()
            || f.module_source.is_wasm_asset()
            || f.name.starts_with("builtin::")
    };

    // Mark an ambient stdio function used when its `log_*` (panic /
    // assert-diagnostic) builtin is reachable and the world provides that
    // stream's sink — each stream gated on its own interface. In a sink-less
    // world (`--lib`, kiln) the builtin lowers to `unreachable` in `calls.rs`,
    // which keys off the `func_map` this populates, so the two stay in
    // agreement per stream and a purely-computational component stays
    // import-free.
    if project.provides_ambient_stdio_sink("Stdout")
        && reachable.iter().any(|func_id| {
            matches!(func_id, FunctionId::Free(f) if is_builtin_func(f) && {
                let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
                name.starts_with("call_indirect_stdout")
            })
        })
    {
        used_wasi_functions.insert("Stdout::write_via_stream".to_string());
    }
    if project.provides_ambient_stdio_sink("Stderr")
        && reachable.iter().any(|func_id| {
            matches!(func_id, FunctionId::Free(f) if is_builtin_func(f) && {
                let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
                name.starts_with("call_indirect_stderr")
            })
        })
    {
        used_wasi_functions.insert("Stderr::write_via_stream".to_string());
    }

    // Collect imports using registry lookup instead of hard-coded match
    let mut imports: IndexSet<NirImport> = IndexSet::default();

    let add_import_by_name = |imports: &mut IndexSet<NirImport>, name: &str| {
        if let Some(info) = project.builtin_registry.get(name)
            && let Some(canonical_name) = &info.canonical_name
        {
            imports.insert(NirImport {
                namespace: info.namespace.clone(),
                canonical_name: canonical_name.clone(),
                func_name: name.to_string(),
                params: info.params.iter().map(|(_, ty)| *ty).collect(),
                return_type: info.return_type,
            });
        }
    };

    // Map reachable builtin function calls to imports via registry lookup
    for func_id in reachable {
        if let FunctionId::Free(f) = func_id
            && is_builtin_func(f)
        {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            add_import_by_name(&mut imports, name);
        }
    }

    // realloc is always needed for memory management
    add_import_by_name(&mut imports, "realloc");

    // Async exports require task-return and potentially other canonical intrinsics.
    // The test world always has async exports (each test is an async component export).
    let has_async_export = project.is_test_world()
        || project
            .world_registry
            .get(&project.target_world)
            .is_some_and(crate::world_registry::WorldInfo::has_async_export);
    if has_async_export {
        // TaskReturn is always needed for async exports.
        // For Result-returning exports (e.g., HTTP handler), synthesis::cm_binding computes
        // the correct flattened CM ABI params. Override the builtin registry's default
        // single-i32 signature with the correct flat params.
        if let Some(flat_params) = project.task_return_flat_params.clone() {
            if let Some(info) = project.builtin_registry.get("task_return")
                && let Some(canonical_name) = &info.canonical_name
            {
                imports.insert(NirImport {
                    namespace: info.namespace.clone(),
                    canonical_name: canonical_name.clone(),
                    func_name: "task_return".to_string(),
                    params: flat_params,
                    return_type: info.return_type,
                });
            }
        } else {
            add_import_by_name(&mut imports, "task_return");
        }
    }

    // Store imports in the project
    project.imports = imports.into_iter().collect();
    // Sort imports for deterministic output
    project
        .imports
        .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));

    project.used_wasi_functions = used_wasi_functions;
}

/// Filter string literals to those owned by surviving functions.
///
/// Called by `run_dce` once function DCE has stripped dead functions from
/// `project.functions`. The set of surviving `(module_source, name)` keys is
/// derived directly from `project.functions`, so this pass no longer needs
/// the reachable-`FunctionId` set and rebuilds no `FunctionId`s itself.
pub fn filter_string_literals(project: &mut NirPackage) {
    let surviving: IndexSet<(ModuleSource, String)> = project
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            // Dead functions linger in `functions` (Phase 4 marks, never removes);
            // their string literals must not be kept alive.
            if func.is_dead {
                return None;
            }
            Some((func.module_source.clone(), func.name.clone()))
        })
        .collect();

    let mut reachable_strings: IndexSet<String> = IndexSet::default();
    for ((module_source, func_name), strings) in &project.function_strings {
        if surviving.contains(&(module_source.clone(), func_name.clone())) {
            reachable_strings.extend(strings.iter().cloned());
        }
    }

    project.string_literals = reachable_strings.into_iter().collect();
}

/// Filter bytes literals to only include bytes referenced by surviving functions.
///
/// Unlike string literals (which have a `function_strings` map for per-function
/// tracking), bytes literals are tracked only by their inline
/// `ExprKind::PackedArray(Vec<u8>)` nodes. This scans every surviving function
/// body for those nodes, then retains only matching entries in
/// `project.bytes_literals`. Since string literal `repr`s are *also*
/// `PackedArray` now, the scanned set is a superset that may include string
/// payloads, but the `retain` only ever drops entries from `bytes_literals`, so
/// a string payload that coincidentally equals an unused bytes literal at worst
/// keeps that one extra entry (which dedups to the same shared segment anyway).
pub fn filter_bytes_literals(project: &mut NirPackage) {
    let mut used_bytes: IndexSet<Vec<u8>> = IndexSet::default();

    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = func.body.as_ref() {
            collect_bytes_literals_block(body, body.root, &mut used_bytes);
        }
    }

    project.bytes_literals.retain(|b| used_bytes.contains(b));
}

fn collect_bytes_literals_block(body: &Body, root: BlockId, used: &mut IndexSet<Vec<u8>>) {
    // Collect every `PackedArray` payload reachable from `root` (the `repr` of
    // any string / bytes literal), excluding patterns (the tree walk never
    // descended into `LetDestructure` / match-arm patterns, so a payload inside
    // a `ConstantValue` pattern is not counted).
    let mut stack = vec![NodeRef::Block(root)];
    while let Some(node) = stack.pop() {
        if matches!(node, NodeRef::Pat(_)) {
            continue;
        }
        if let NodeRef::Expr(e) = node
            && let ExprKind::PackedArray(b) = &body.exprs[e].kind
        {
            used.insert(b.clone());
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}
/// Remove closure functors whose `__call` method was eliminated by function DCE.
pub fn remove_unreachable_closure_functors(project: &mut NirPackage) {
    // Build a set of surviving (module_source, func_name) pairs for O(1) lookup.
    let surviving_funcs: IndexSet<(ModuleSource, String)> = project
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            // A dead `__call` lingers in `functions` (Phase 4 marks, never removes),
            // so filter by liveness rather than mere presence.
            if func.is_dead {
                return None;
            }
            Some((func.module_source.clone(), func.name.clone()))
        })
        .collect();

    project.closure_functors.retain(|functor| {
        let call_method_name = crate::name::MethodName::format_local(
            &crate::name::FqTypeName::declared(&functor.module_source, &functor.struct_name),
            None,
            crate::name::CLOSURE_CALL_METHOD,
        );
        surviving_funcs.contains(&(functor.module_source.clone(), call_method_name))
    });
}

/// Per-caller pending inspect edges, keyed by the caller's `FunctionId`.
/// Each entry collects every `__Closure_N` observed in that caller's body
/// alongside its `(arity, return_type)` signature. After the
/// inspectable-signature set is computed, `apply_inspect_edges` walks this
/// map and adds the matching `inspect/inspect_alt` edges to the call graph.
type PendingInspectsByCaller = IndexMap<FunctionId, Vec<PendingInspectEdge>>;

/// `FunctionId` → position in `project.functions`. `function_id_for` is
/// required to be injective over `project.functions`; `build_analysis_graph`
/// asserts this so a regression in the synthesis layer (e.g. duplicate
/// emission of a per-signature dispatch stub) trips immediately instead of
/// silently dropping a function during DCE retain.
type FuncPositions = IndexMap<FunctionId, usize>;

/// Result of the single call-graph build. The call graph is the raw
/// reachability graph *without* `__Closure_N^Inspect[Alt]` edges; those
/// edges are gated by the inspectable-signature set and added after the
/// fact by `apply_inspect_edges`.
///
/// `func_pos` maps each NIR-function `FunctionId` back to its position in
/// `project.functions` so that `remove_unreachable_functions` can keep
/// surviving functions by index instead of rebuilding `FunctionId`s and
/// hashing them again.
struct AnalysisGraph {
    call_graph: CallGraph,
    effect_usage: EffectUsageMap,
    pending_inspects: PendingInspectsByCaller,
    func_positions: FuncPositions,
    /// Per-function globals read, indexed by position in
    /// `project.functions`. Aggregated for reachable positions by
    /// [`compute_global_reachability`].
    per_func_globals: Vec<IndexSet<(String, String)>>,
    /// Per-function types directly used, indexed by position in
    /// `project.functions`. Seeded into [`DceAnalysis::types`] by
    /// [`populate_type_reachability`]'s Phase 1.
    per_func_types: Vec<IndexSet<TypeId>>,
}

/// Build the call graph **and** per-function used-globals / used-types
/// sets in a single AST walk per function body. The downstream
/// reachability passes (`analyze_global_reachability`,
/// `populate_type_reachability`) then union per-function facts for the
/// reachable subset instead of re-walking bodies — three independent
/// walks collapsed into one.
fn build_analysis_graph(project: &NirPackage, descriptors: &[FunctionRef]) -> AnalysisGraph {
    let n = project.functions.len();
    // `call_graph` and `func_positions` get exactly one entry per function,
    // so size them up front to avoid the incremental rehashing that an
    // empty map would do as the whole program is walked.
    let mut call_graph: CallGraph =
        IndexMap::with_capacity_and_hasher(n, rustc_hash::FxBuildHasher);
    let mut effect_usage: EffectUsageMap = IndexMap::default();
    let mut pending_inspects: PendingInspectsByCaller = IndexMap::default();
    let mut func_positions: FuncPositions =
        IndexMap::with_capacity_and_hasher(n, rustc_hash::FxBuildHasher);
    let mut per_func_globals: Vec<IndexSet<(String, String)>> = Vec::with_capacity(n);
    let mut per_func_types: Vec<IndexSet<TypeId>> = Vec::with_capacity(n);

    let type_table = &*project.type_table.borrow();

    for (pos, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        let module_source = &func.module_source;
        let func_id = function_id_for(&func);

        let mut walker = DceWalker::new(type_table, module_source, descriptors);
        walker.analyze(&func);
        let mut analysis = walker.analysis;
        // Promoted operands hold their source type in the body's value pool, not
        // in an `ExprNode`, so the walker misses them. Keep those types reachable
        // (a literal of an otherwise-unreachable newtype) — else its `TypeId`
        // dangles after `remove_unreachable_types`.
        if let Some(body) = &func.body {
            for ty in body.values.recorded_types() {
                analysis.used_types.insert(ty);
            }
        }

        let prior = func_positions.insert(func_id.clone(), pos);
        assert!(
            prior.is_none(),
            "function_id_for collision in project.functions: two distinct \
             functions map to the same FunctionId {func_id:?}. \
             `function_id_for` must be injective; check the synthesis or \
             monomorphize layer for duplicate emission."
        );
        call_graph.insert(func_id.clone(), analysis.callees);
        if !analysis.effect_calls.is_empty() {
            effect_usage.insert(func_id.clone(), analysis.effect_calls);
        }
        if !analysis.pending_inspects.is_empty() {
            pending_inspects.insert(func_id, analysis.pending_inspects);
        }
        per_func_globals.push(analysis.used_globals);
        per_func_types.push(analysis.used_types);
    }

    AnalysisGraph {
        call_graph,
        effect_usage,
        pending_inspects,
        func_positions,
        per_func_globals,
        per_func_types,
    }
}

/// Augment `call_graph` with the gated `__Closure_N^Inspect[Alt]::inspect[_alt]`
/// edges. Inserts exactly one edge per (caller, struct, trait) match against
/// the inspectable-signature set computed in Phase 1b.
fn apply_inspect_edges(
    call_graph: &mut CallGraph,
    pending: &PendingInspectsByCaller,
    sigs: &InspectableSignatures,
) {
    for (caller, edges) in pending {
        let Some(callees) = call_graph.get_mut(caller) else {
            continue;
        };
        for edge in edges {
            if sigs.inspect.contains(&edge.key) {
                callees.insert(FunctionId::Method(MethodName::new(
                    edge.closure_module.clone(),
                    edge.struct_name.clone(),
                    Some("Inspect".to_string()),
                    "inspect".to_string(),
                )));
            }
            if sigs.inspect_alt.contains(&edge.key) {
                callees.insert(FunctionId::Method(MethodName::new(
                    edge.closure_module.clone(),
                    edge.struct_name.clone(),
                    Some("InspectAlt".to_string()),
                    "inspect_alt".to_string(),
                )));
            }
        }
    }
}

/// Walk all NIR function bodies and collect every `(arity, return_type)`
/// signature that is the receiver type of a `Fn<arity, ret>^Inspect` or
/// `Fn<arity, ret>^InspectAlt` method call. Used by the DCE call-graph
/// builder to gate the per-functor `inspect` / `inspect_alt` root
/// marking emitted from `ClosureToCanonical` independently: without a
/// real `Fn^Inspect[Alt]` caller, those per-functor impls cannot be
/// invoked indirectly, so keeping them alive is purely waste.
///
/// The two trait methods are tracked separately so a program that only
/// formats closures with `:?` doesn't keep every `__Closure_N^InspectAlt`
/// impl (and its per-literal source-string constant) alive.
#[derive(Default)]
struct InspectableSignatures {
    inspect: IndexSet<(usize, TypeId)>,
    inspect_alt: IndexSet<(usize, TypeId)>,
}

/// Compute the inspectable `(arity, return_type)` set from the bodies
/// of *reachable* functions only. Restricting the scan to live code
/// keeps a dead `:?`/`:#?` call from forcing per-functor inspect impls
/// to stay alive for an unrelated reachable closure of the same
/// signature.
fn collect_inspectable_signatures_from_reachable(
    project: &NirPackage,
    descriptors: &[FunctionRef],
    reachable: &IndexSet<FunctionId>,
) -> InspectableSignatures {
    let mut sigs = InspectableSignatures::default();
    let type_table = &*project.type_table.borrow();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let func_id = function_id_for(&func);
        if !reachable.contains(&func_id) {
            continue;
        }
        if let Some(body) = func.body.as_ref() {
            scan_inspect_signatures_block(body, type_table, descriptors, &mut sigs);
        }
    }
    sigs
}

/// Compute the `FunctionId` used by the call graph for a NIR function.
/// Mirrors the keying logic in `build_analysis_graph`; centralising
/// it here so other passes (notably the inspectable-signatures scan)
/// can compare against the call graph's reachable set.
fn function_id_for(func: &NirFunction) -> FunctionId {
    let module_source = &func.module_source;
    if let Some(ref info) = func.method_info {
        if let Some(monomorph_info) = &func.monomorph_info {
            FunctionId::Free(FreeFunctionName::with_monomorph_info(
                module_source.clone(),
                func.name.clone(),
                monomorph_info.generic_name.clone(),
            ))
        } else {
            // Type arguments are part of a method's identity: `field<T>` and
            // `field<i32>` are two functions, and the bare name collapses them.
            FunctionId::Method(MethodName::new(
                module_source.clone(),
                info.fq_struct_name(),
                info.trait_name.clone(),
                info.full_method_name(),
            ))
        }
    } else if let Some(monomorph_info) = &func.monomorph_info {
        FunctionId::Free(FreeFunctionName::with_monomorph_info(
            module_source.clone(),
            func.name.clone(),
            monomorph_info.generic_name.clone(),
        ))
    } else {
        FunctionId::Free(FreeFunctionName::from_module_source(
            module_source,
            &func.name,
        ))
    }
}

fn scan_inspect_signatures_block(
    body: &Body,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
    sigs: &mut InspectableSignatures,
) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let Some((receiver, func_id, _)) = body.exprs[e].kind.as_method_call()
            && let Some(info) = &callee_descriptor(descriptors, func_id).method_info
            && info.base_struct_name() == "Fn"
            && let Some(trait_name) = info.base_trait_name.as_deref()
        {
            // Receiver is `&Fn(...)` (possibly wrapped in `Box<fn(...)>` by the
            // boxing pass); peel both to read the function's arity + return type.
            let recv_type = type_table.peel_refs_and_box(body.operand_type(receiver));
            if let ResolvedType::Function {
                params,
                return_type,
                ..
            } = type_table.get(recv_type)
            {
                let key = (params.len(), *return_type);
                match trait_name {
                    "Inspect" => {
                        sigs.inspect.insert(key);
                    }
                    "InspectAlt" => {
                        sigs.inspect_alt.insert(key);
                    }
                    _ => {}
                }
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// Single-walk DCE fact collector: a [`NirRefVisitor`] that collects
/// **all** per-function facts the DCE driver needs (callees, effect calls,
/// pending inspect edges, used globals, used types) in one traversal of a
/// function body. Replaces three hand-rolled walkers (`analyze_block`,
/// `collect_global_reads_block`, `collect_types_from_block`) that all
/// rediscovered the same NIR shape independently — new `StmtKind` /
/// `ExprKind` variants now only need to be considered in the visitor
/// trait, not in three places.
struct DceWalker<'a> {
    type_table: &'a TypeTable,
    current_module: &'a ModuleSource,
    descriptors: &'a [FunctionRef],
    analysis: FunctionAnalysis,
}

impl<'a> DceWalker<'a> {
    fn new(
        type_table: &'a TypeTable,
        current_module: &'a ModuleSource,
        descriptors: &'a [FunctionRef],
    ) -> Self {
        Self {
            type_table,
            current_module,
            descriptors,
            analysis: FunctionAnalysis::default(),
        }
    }

    /// Walk a function's signature, locals, monomorphisation type
    /// arguments, and body. The signature/local/monomorph pre-walk
    /// covers types not visible from the body (e.g. an unused
    /// generic parameter type that still needs to survive WIR name
    /// mangling).
    fn analyze(&mut self, func: &NirFunction) {
        for param in &func.params {
            self.analysis.used_types.insert(param.type_id);
        }
        self.analysis.used_types.insert(func.return_type);
        for local in &func.locals {
            self.analysis.used_types.insert(local.type_id);
        }
        if let Some(info) = &func.monomorph_info {
            for &ta in &info.impl_type_args {
                self.analysis.used_types.insert(ta);
            }
            for &ta in &info.method_type_args {
                self.analysis.used_types.insert(ta);
            }
        }
        if let Some(body) = func.body.as_ref() {
            self.walk_node(body, NodeRef::Block(body.root));
        }
    }

    /// Record a directly-referenced type. Transitive closure (struct
    /// fields, variant payloads, generic dependencies) happens once,
    /// later, in [`populate_type_reachability`]'s Phase 2 fixed-point
    /// loop — so the per-expression walker only needs to mark the
    /// node's own `TypeId` here.
    fn add_type(&mut self, type_id: TypeId) {
        self.analysis.used_types.insert(type_id);
    }

    fn record_call(&mut self, func: &crate::nir::FunctionRef) {
        let original_callee_module = func.module_source.clone();
        let func_name = func.name.clone();

        if let Some(call_info) = func.method_info.as_ref() {
            // Static method call (e.g. `Box::get`, `String^Display::fmt`):
            // `func_name` is `"Struct::method"` or `"Struct^Trait::method"`.
            let callee_id = if func.is_monomorphized() {
                let base_name = func
                    .base_struct_name()
                    .map(|base| crate::name::rebase_monomorph_method(&func_name, &base))
                    .unwrap_or_else(|| func_name.clone());
                FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    func.module_source.clone(),
                    func_name.clone(),
                    base_name,
                ))
            } else {
                // `full_method_name`, not `method_name`: `function_id_for` keys
                // on the type arguments too, so a call keyed without them names
                // no definition and DCE drops a live method.
                FunctionId::Method(MethodName::new(
                    original_callee_module,
                    call_info.fq_struct_name(),
                    call_info.trait_name.clone(),
                    call_info.full_method_name(),
                ))
            };
            self.analysis.callees.insert(callee_id);

            // Resource method call on a WASI module — record as an effect.
            let module_path = func.module_path();
            if module_path.len() >= 2
                && module_path[0] == "wasi"
                && let Some((resource_name, method_name)) = func_name.split_once("::")
            {
                self.analysis
                    .effect_calls
                    .insert((resource_name.to_string(), method_name.to_string()));
            }
        } else {
            // Free function call. `method_info` is the discriminator — a name
            // is not one: a synthesized helper embeds a type mangle, which
            // carries `::` for an associated-type projection
            // (`$value_copy$S::MapSerializer`).
            let callee_module = original_callee_module.clone();
            let callee_id = FunctionId::Free(FreeFunctionName::from_module_source(
                &callee_module,
                &func_name,
            ));
            self.analysis.callees.insert(callee_id);

            if let Some(interface_name) = original_callee_module.interface_name() {
                self.analysis
                    .effect_calls
                    .insert((interface_name, func_name));
            }
        }
    }

    fn record_method_call(&mut self, receiver_type: TypeId, func: &crate::nir::FunctionRef) {
        let func_name = func.name.clone();

        // Monomorphized methods (e.g. `List<i32>::len`) already have
        // their concrete name on `func`; non-monomorphized methods are
        // dispatched by `receiver`'s type below.
        if func.is_monomorphized() {
            let base_name = func
                .base_struct_name()
                .map(|base| crate::name::rebase_monomorph_method(&func_name, &base))
                .unwrap_or_else(|| func_name.clone());

            // Use the func's actual module_source — monomorphized functions
            // are placed in the module that uses them.
            let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                func.module_source.clone(),
                func_name,
                base_name,
            ));
            self.analysis.callees.insert(callee_id);
            return;
        }

        // Non-monomorphized method - determine target from receiver type.
        // Strip any reference wrappers and newtypes to get the base type.
        let mut current_type = self.type_table.get(receiver_type);
        let mut newtype_info: Option<(String, ModuleSource)> = None;
        loop {
            match current_type {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    current_type = self.type_table.get(*inner);
                }
                ResolvedType::Newtype {
                    name,
                    module_source,
                    base_type,
                } => {
                    // Remember the outermost newtype for its own trait impls
                    if newtype_info.is_none() {
                        newtype_info = Some((name.clone(), module_source.clone()));
                    }
                    current_type = self.type_table.get(*base_type);
                }
                _ => break,
            }
        }
        let base_receiver_type = current_type.clone();

        // Extract method name and trait name from method_info
        let (method_name, trait_name) = if let Some(info) = func.method_info.clone() {
            (info.method_name.clone(), info.trait_name)
        } else {
            (func_name, None)
        };

        // Mark the *resolved* method target reachable directly from
        // `func.method_info`. That info was captured before newtype erasure
        // (Phase 9b), so it carries the real struct name; the receiver-type
        // dispatch below cannot recover it once the type is erased — e.g. an
        // `f32x4` struct field erases to its `v128` base, so `self.v.max(..)`
        // would be recorded as `v128::max` and the real `f32x4::max` dropped
        // as unreachable, failing WIR build. This is additive: DCE only adds
        // callees here, so trusting the resolved target cannot remove a real
        // function — it only guarantees the actual call target is kept.
        if let Some(info) = func.method_info.as_ref() {
            let resolved_id = FunctionId::Method(MethodName::new(
                func.module_source.clone(),
                info.fq_struct_name(),
                info.trait_name.clone(),
                info.method_name.clone(),
            ));
            self.analysis.callees.insert(resolved_id);
        }

        // If the receiver was a newtype (e.g., flags type), also mark
        // the newtype's own methods as reachable (e.g., Perms^Inspect::inspect).
        if let Some((newtype_name, newtype_module)) = newtype_info {
            let method_id = FunctionId::Method(MethodName::new(
                newtype_module.clone(),
                FqTypeName::declared(&newtype_module, &newtype_name),
                trait_name.clone(),
                method_name.clone(),
            ));
            self.analysis.callees.insert(method_id);
        }

        match base_receiver_type {
            ResolvedType::Struct {
                ref decl_name,
                ref type_args,
                ref module_source,
            } if !type_args.is_empty() => {
                let base_struct = decl_name;
                let name = &self.type_table.struct_rendered_name(decl_name, type_args);
                // Monomorphized struct method (e.g. `Box<i32>::get`):
                // monomorphized functions live in the *using* module, so
                // route the callee id through `current_module`. The base
                // method name uses the original generic struct name so
                // the inlining-induced graph stays mergeable.
                let mangled_func_name = MethodName::format_local(
                    &FqTypeName::declared(module_source, name),
                    trait_name.as_deref(),
                    &method_name,
                );
                let base_method_name = MethodName::format_local(
                    &FqTypeName::declared(module_source, base_struct),
                    trait_name.as_deref(),
                    &method_name,
                );
                let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    self.current_module.clone(),
                    mangled_func_name,
                    base_method_name,
                ));
                self.analysis.callees.insert(callee_id);

                // For internal Box<T> types (primitive boxing), the method is
                // actually defined on the inner type (e.g., i32^Ord::cmp, not
                // Box<i32>^Ord::cmp). Also mark the FunctionRef's original
                // method target as reachable.
                if base_struct == "Box"
                    && let Some(info) = func.method_info.clone()
                {
                    let original_method_id = FunctionId::Method(MethodName::new(
                        func.module_source.clone(),
                        info.fq_struct_name(),
                        info.trait_name.clone(),
                        info.method_name,
                    ));
                    self.analysis.callees.insert(original_method_id);
                }
            }
            ResolvedType::Struct {
                decl_name: name,
                module_source,
                ..
            } => {
                // Non-monomorphized struct method.
                let method_id = FunctionId::Method(MethodName::new(
                    module_source.clone(),
                    FqTypeName::declared(&module_source, &name),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);

                // Also mark reachable using the FunctionRef's module source,
                // since trait impls may live in a different module than the type
                // (e.g., `impl Display for String` is in format.wado, not string.wado)
                let func_module = func.module_source.clone();
                if func_module != module_source
                    && let Some(info) = func.method_info.clone()
                {
                    let alt_method_id = FunctionId::Method(MethodName::new(
                        func_module,
                        info.fq_struct_name(),
                        info.trait_name.clone(),
                        info.method_name,
                    ));
                    self.analysis.callees.insert(alt_method_id);
                }
            }
            ResolvedType::Primitive(prim) => {
                // Trait/inherent methods on primitives (`i32^Ord::cmp`,
                // `char::is_ascii_space`, `42.to_string()`, …).
                if method_name == "to_string" {
                    add_to_string_callee(receiver_type, self.type_table, &mut self.analysis);
                }
                let method_id = FunctionId::Method(MethodName::new(
                    ModuleSource::primitive(),
                    FqTypeName::builtin(prim.as_str()),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::Unit => {
                // `()` methods: `().to_string()`, `().fmt(&f)`, etc.
                let method_id = FunctionId::Method(MethodName::new(
                    ModuleSource::primitive(),
                    FqTypeName::builtin(TypeTable::UNIT_TYPE_NAME),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if TypeTable::is_tuple_type(&name) => {
                // Tuple method call: synthesized with struct_name `"[]<f64,f64>"`.
                let elements: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.type_table.fq_type_name(*t))
                    .collect();
                let method_id = FunctionId::Method(MethodName::new(
                    self.current_module.clone(),
                    FqTypeName::tuple(elements),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source: _,
            } => {
                // Generic instance method (e.g. `Box<i32>::get`,
                // `TreeMap<String,i32>^Index::index`). Trait methods
                // need the trait name baked into the mangle so trait-
                // and inherent-name collisions stay separate.
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_name(*t))
                    .collect();
                let (mangled_func_name, base_name) = if let Some(ref trait_n) = trait_name {
                    let generic_name = mangle_generic_name(&name, &type_arg_names);
                    let mangled = mangle_local_trait_method(&generic_name, trait_n, &method_name);
                    let base = mangle_local_trait_method(&name, trait_n, &method_name);
                    (mangled, base)
                } else {
                    let mangled = mangle_method_generic(&name, &type_arg_names, &method_name);
                    let base = mangle_local_method(&name, &method_name);
                    (mangled, base)
                };
                let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    self.current_module.clone(),
                    mangled_func_name,
                    base_name,
                ));
                self.analysis.callees.insert(callee_id);
            }
            ResolvedType::Enum {
                name,
                module_source,
            } => {
                // Enum method (user-defined or auto-derived trait impl).
                let method_id = FunctionId::Method(MethodName::new(
                    module_source.clone(),
                    FqTypeName::declared(&module_source, &name),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::Resource { name, .. } => {
                // Resource instance method (e.g. `fields.has()`):
                // recorded as an effect so it lands in
                // `used_wasi_functions`.
                self.analysis.effect_calls.insert((name, method_name));
            }
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => {
                // Variant method, e.g. `Shape^Inspect::inspect`.
                let method_id = FunctionId::Method(MethodName::new(
                    module_source.clone(),
                    FqTypeName::declared(&module_source, &name),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                // `Fn<arity, ret>` method, e.g. `Fn<2,i32>^Inspect::inspect`.
                // The arity is a literal, not a type — a builtin head with no
                // module, same as the `Fn` shape itself.
                let shape_args = vec![
                    FqTypeName::builtin(&params.len().to_string()),
                    self.type_table.fq_type_name(return_type),
                ];
                let method_id = FunctionId::Method(MethodName::new(
                    self.current_module.clone(),
                    FqTypeName::builtin(crate::name::CLOSURE_FN_TRAIT).with_args(shape_args),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::GenericResource {
                name, type_args, ..
            } => {
                // Generic resource method, e.g. `Future<T>^Inspect::inspect`.
                let resource_args: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.type_table.fq_type_name(*t))
                    .collect();
                let method_id = FunctionId::Method(MethodName::new(
                    self.current_module.clone(),
                    FqTypeName::builtin(name.as_str()).with_args(resource_args),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            _ => {}
        }
    }

    fn record_cm_raw_call(&mut self, local_name: &str) {
        // CmRawCall references a lowered WASI import function.
        // Parse the local_name (e.g., "wasi:cli/Stdout::write_via_stream")
        // to extract the interface_name and op_name for WASI import tracking.
        if let Some((interface_name, op_name)) = local_name.split_once("::").map(|(prefix, op)| {
            // prefix is like "wasi:cli/Stdout" → extract "Stdout"
            let effect = prefix.rsplit('/').next().unwrap_or(prefix);
            (effect.to_string(), op.to_string())
        }) {
            self.analysis.effect_calls.insert((interface_name, op_name));
        }
    }

    fn record_closure_to_canonical(
        &mut self,
        functor_id: u32,
        target_fn_type: TypeId,
        closure_module: &ModuleSource,
    ) {
        // `__call` is always live: the canonical closure struct holds
        // a `ref.func` to it directly.
        let struct_name = crate::name::FqTypeName::declared(
            closure_module,
            &format!(
                "{prefix}{functor_id}",
                prefix = crate::name::CLOSURE_STRUCT_PREFIX,
            ),
        );
        self.analysis
            .callees
            .insert(FunctionId::Method(MethodName::new(
                closure_module.clone(),
                struct_name.clone(),
                None,
                crate::name::CLOSURE_CALL_METHOD.to_string(),
            )));

        // Per-functor `__Closure_N^Inspect` / `^InspectAlt` impls only
        // need to stay alive when their matching `Fn<arity, ret>^Inspect[Alt]`
        // dispatch stub is reachable — tracked independently per trait so a
        // program using only `:?` doesn't keep `^InspectAlt` (and its
        // per-literal source-string constant) alive. The gating set isn't
        // known yet (it's derived from the first reachable-set computation),
        // so record a pending edge here for `apply_inspect_edges` to
        // resolve later.
        if let ResolvedType::Function {
            params,
            return_type,
            ..
        } = self.type_table.get(target_fn_type)
        {
            self.analysis.pending_inspects.push(PendingInspectEdge {
                closure_module: closure_module.clone(),
                struct_name,
                key: (params.len(), *return_type),
            });
        }
    }
}

impl DceWalker<'_> {
    /// Record the per-node facts, then recurse into every id-bearing child
    /// (including patterns, matching the former `NirRefVisitor` full walk).
    fn walk_node(&mut self, body: &Body, node: NodeRef) {
        match node {
            NodeRef::Stmt(s) => {
                // The `Let` binding's declared type is not visible from its
                // `value` (the value's `type_id` is the RHS type before coercion).
                if let StmtKind::Let { type_id, .. } = &body.stmts[s].kind {
                    self.add_type(*type_id);
                }
            }
            NodeRef::Expr(e) => {
                // Every expression has a result type that needs to stay alive.
                self.add_type(body.exprs[e].type_id);
                match &body.exprs[e].kind {
                    ExprKind::Call { func_id, .. } => {
                        let d = self.descriptors;
                        match body.exprs[e].kind.as_method_call() {
                            Some((receiver, _, _)) => {
                                let recv_ty = body.operand_type(receiver);
                                self.record_method_call(recv_ty, callee_descriptor(d, *func_id));
                            }
                            None => self.record_call(callee_descriptor(d, *func_id)),
                        }
                    }
                    ExprKind::CmRawCall { local_name, .. } => self.record_cm_raw_call(local_name),
                    ExprKind::ClosureToCanonical {
                        functor_id,
                        target_fn_type,
                        closure_module,
                        ..
                    } => {
                        self.add_type(*target_fn_type);
                        self.record_closure_to_canonical(
                            *functor_id,
                            *target_fn_type,
                            closure_module,
                        );
                    }
                    ExprKind::GlobalVarGet {
                        module_source,
                        name,
                    } => {
                        self.analysis
                            .used_globals
                            .insert((module_source.to_path().join("::"), name.clone()));
                    }
                    ExprKind::Cast { target_type, .. } => self.add_type(*target_type),
                    ExprKind::StructLiteral { struct_type, .. } => self.add_type(*struct_type),
                    ExprKind::VariantConstruct { variant_type, .. } => self.add_type(*variant_type),
                    ExprKind::VariantPayload { payload_type, .. } => self.add_type(*payload_type),
                    _ => {}
                }
            }
            NodeRef::Pat(p) => match &body.pats[p].kind {
                PatKind::Binding { type_id, .. } => self.add_type(*type_id),
                PatKind::Variant {
                    enum_type,
                    payload_type,
                    ..
                } => {
                    self.add_type(*enum_type);
                    self.add_type(*payload_type);
                }
                PatKind::Enum { enum_type, .. } => self.add_type(*enum_type),
                PatKind::Struct { struct_type, .. } => self.add_type(*struct_type),
                _ => {}
            },
            NodeRef::Block(_) => {}
        }
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            self.walk_node(body, c);
        }
    }
}

/// Mark the `to_string` impl that lowering will dispatch
/// `receiver.to_string()` to. `impl i32`, `impl ()`, etc. live in
/// `core:prelude/primitive`; `String::to_string` is a no-op and needs
/// no call.
fn add_to_string_callee(type_id: TypeId, type_table: &TypeTable, analysis: &mut FunctionAnalysis) {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => {
            let method_id = FunctionId::Method(MethodName::new(
                ModuleSource::primitive(),
                FqTypeName::builtin(prim.as_str()),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Unit => {
            let method_id = FunctionId::Method(MethodName::new(
                ModuleSource::primitive(),
                FqTypeName::builtin(TypeTable::UNIT_TYPE_NAME),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Struct { decl_name, .. } if decl_name == "String" => {}
        _ => {}
    }
}

/// Worklist BFS over `call_graph` starting at `entry`.
fn compute_reachable(
    call_graph: &IndexMap<FunctionId, IndexSet<FunctionId>>,
    entry: &FunctionId,
) -> IndexSet<FunctionId> {
    let mut reachable = IndexSet::default();
    let mut worklist = vec![entry.clone()];

    while let Some(func) = worklist.pop() {
        if reachable.contains(&func) {
            continue;
        }
        reachable.insert(func.clone());

        // Add all callees to worklist
        if let Some(callees) = call_graph.get(&func) {
            for callee in callees {
                if !reachable.contains(callee) {
                    worklist.push(callee.clone());
                }
            }
        }
    }

    reachable
}

/// Mark every function whose original position is **not** in
/// `reachable_positions` (computed by [`analyze_dce`]) as dead by clearing
/// its body. The function record stays in `project.functions` at its
/// original position, so `FuncId == position` holds for the whole pipeline
/// (`dce` never renumbers). A dead function then lingers as an inert bodyless record,
/// indistinguishable from an extern declaration: every body-iterating pass
/// and codegen already skip `body.is_none()`, and the type / global / string
/// reachability is filtered by reachable *position* (not body presence), so
/// clearing the body is behavior-preserving versus the old `retain` removal.
pub fn remove_unreachable_functions(
    project: &mut NirPackage,
    reachable_positions: &IndexSet<usize>,
) {
    // Dense `Vec<bool>` indexed by original position avoids hashing each
    // index against `reachable_positions` once per step.
    let mut keep = vec![false; project.functions.len()];
    for &pos in reachable_positions {
        if pos < keep.len() {
            keep[pos] = true;
        }
    }
    for (i, func_rc) in project.functions.iter().enumerate() {
        if !keep[i] {
            let mut func = func_rc.borrow_mut();
            func.is_dead = true;
            func.body = None;
        }
    }
}

impl DceAnalysis {
    fn empty() -> Self {
        Self {
            functions: IndexSet::default(),
            globals: IndexSet::default(),
            types: IndexSet::default(),
            struct_exact: IndexSet::default(),
            struct_monomorph_names: IndexSet::default(),
            struct_monomorph_bases: IndexSet::default(),
            generic_instance_names: IndexSet::default(),
            variant_exact: IndexSet::default(),
            enum_exact: IndexSet::default(),
        }
    }

    /// Rebuild the name-keyed type-index views from the current
    /// `self.types`. Cheap relative to the alternative of `iter().any()`
    /// lookups — see [`populate_type_reachability`]'s Phase 2 comment.
    fn refresh_indexes(&mut self, type_table: &TypeTable) {
        self.struct_exact.clear();
        self.struct_monomorph_names.clear();
        self.struct_monomorph_bases.clear();
        self.generic_instance_names.clear();
        self.variant_exact.clear();
        self.enum_exact.clear();
        for &id in &self.types {
            match type_table.get(id) {
                ResolvedType::Struct {
                    decl_name,
                    module_source,
                    type_args,
                } => {
                    if type_args.is_empty() {
                        self.struct_exact
                            .insert((decl_name.clone(), module_source.clone()));
                    } else {
                        self.struct_monomorph_names
                            .insert(type_table.struct_rendered_name(decl_name, type_args));
                        self.struct_monomorph_bases.insert(decl_name.clone());
                    }
                }
                ResolvedType::Variant {
                    name,
                    module_source,
                } => {
                    self.variant_exact
                        .insert((name.clone(), module_source.clone()));
                }
                ResolvedType::Enum {
                    name,
                    module_source,
                } => {
                    self.enum_exact
                        .insert((name.clone(), module_source.clone()));
                }
                ResolvedType::GenericInstance { name, .. } => {
                    self.generic_instance_names.insert(name.clone());
                }
                _ => {}
            }
        }
    }
}

/// Variant declarations that outlive their uses, both for
/// `optimize::sroa_variant_return`:
///
/// - `Option`, because the pass mints `Option<T>` slots *after* the early DCE
///   run and `wir_build::register_mono_variants` registers the instance off the
///   declaration.
/// - Any variant a function was scalarized *from*. Scalarizing every use of a
///   variant away is exactly what makes its declaration look unreachable, and
///   the pass re-derives its layout from that declaration to recognise its own
///   earlier work in a later iteration.
///
/// A kept declaration keeps its case payload types too: `register_mono_variants`
/// substitutes the declaration's payloads against each instance's type args, so
/// a declaration whose payload `TypeId` was pruned panics in `wir_build`. That
/// is why the same set gates both the payload walk and the retain.
///
/// Keeping a declaration costs nothing: WIR registers instances, not
/// declarations.
fn variant_decls_kept_past_use(
    project: &NirPackage,
    type_table: &TypeTable,
) -> crate::hashmap::IndexSet<(String, ModuleSource)> {
    let mut kept: crate::hashmap::IndexSet<(String, ModuleSource)> = project
        .functions
        .iter()
        .filter_map(|f| f.borrow().scalarized_from)
        .filter_map(|t| match type_table.get(t) {
            ResolvedType::Variant {
                name,
                module_source,
            }
            | ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => Some((name.clone(), module_source.clone())),
            _ => None,
        })
        .collect();
    if let Some(ms) = type_table
        .compiler_items()
        .variant_module(crate::compiler_item::CompilerItem::Option)
    {
        kept.insert(("Option".to_string(), ms.clone()));
    }
    kept
}

/// Populate `analysis.types` and the name-keyed type-index views.
/// A type is reachable if it's used in any reachable function's
/// signature, locals, or expressions, or in any reachable global's
/// initializer (with transitive closure over struct fields, variant
/// payloads, and per-type dependencies).
///
/// Reads `analysis.functions` and `analysis.globals` to filter the
/// per-function / per-global walks — both must be populated first
/// (see [`analyze_dce`]). Running pre-pruning, so all DCE analysis
/// sits in `analyze_dce` and the downstream `remove_*` functions only
/// mutate.
fn populate_type_reachability(
    project: &NirPackage,
    descriptors: &[FunctionRef],
    graph: &AnalysisGraph,
    analysis: &mut DceAnalysis,
) {
    // Always include the pre-interned builtin scalar types (`I8` .. `UNKNOWN`).
    // Anchored on the `TypeTable` constants rather than a literal `0..18` so
    // adding or removing a primitive can never silently desync the range.
    for id in TypeTable::I8.0..=TypeTable::UNKNOWN.0 {
        analysis.types.insert(TypeId(id));
    }

    // Always include BuiltinArray(U8) as it's fundamental for String operations
    // and used by codegen for internal operations (assert statements, etc.)
    // Find the TypeId for BuiltinArray(U8) in the type table
    {
        let type_table = project.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::BuiltinArray(elem) = type_table.get(type_id)
                && *elem == TypeTable::U8
            {
                analysis.types.insert(type_id);
                break;
            }
        }
    }

    // Phase 1: Seed `analysis.types` from per-function facts (collected
    // by `DceWalker` during `build_analysis_graph`) and from reachable
    // globals' initializers + closure functor types. No function-body
    // re-walk — the per-function used-types set is already populated.
    {
        let type_table = project.type_table.borrow();

        // Sum per-function used-types for reachable functions only.
        for &pos in &analysis.functions {
            if let Some(per_func) = graph.per_func_types.get(pos) {
                for &id in per_func {
                    analysis.types.insert(id);
                }
            }
        }

        // Reachable globals' declared type + initializer types. At NIR
        // level non-constant initializers have already been extracted
        // into `__initialize_module` (see `lower::plan::globals`), so
        // each surviving `global.initializer` here is a constant
        // expression — DceWalker on it only walks the literal tree.
        for global in &project.globals {
            let global_key = (
                global.module_source.to_path().join("::"),
                global.name.clone(),
            );
            if !analysis.globals.contains(&global_key) {
                continue;
            }
            collect_type_transitive(global.ty, &type_table, &mut analysis.types);
            let mut walker = DceWalker::new(&type_table, &global.module_source, descriptors);
            let init_body = global.init.slot_expr().body();
            walker.walk_node(init_body, NodeRef::Block(init_body.root));
            for id in walker.analysis.used_types {
                analysis.types.insert(id);
            }
        }

        // When a closure functor's `__call` method is reachable, its
        // struct / ref types must stay live: `wir_build::register_closure_wrappers`
        // reads `ClosureFunctor::ref_type_id` to emit the wrapper's `ref.cast`,
        // and NIR DAE can drop every other NIR-side reference (it removes the
        // env `self` from `call_method.params[0]`), leaving the `ClosureFunctor`
        // record itself as the only live mention. Without this insertion the
        // type-table lookup panics with `TypeId not found`.
        //
        // `functor.call_method` and the matching `project.functions[i]` are the
        // same `Rc` — compare by pointer identity to avoid cloning a
        // `(ModuleSource, String)` key per functor.
        let surviving_ptrs: IndexSet<*const _> = project
            .functions
            .iter()
            .enumerate()
            .filter(|(pos, _)| analysis.functions.contains(pos))
            .map(|(_, rc)| std::rc::Rc::as_ptr(rc))
            .collect();
        for functor in &project.closure_functors {
            let cm_ptr = std::rc::Rc::as_ptr(&functor.call_method);
            if surviving_ptrs.contains(&cm_ptr) {
                analysis.types.insert(functor.struct_type_id);
                analysis.types.insert(functor.ref_type_id);
            }
        }
    }

    // Phase 2: Transitive closure - include struct fields, variant payloads, and type dependencies
    // Loop-invariant: it reads `project` and the type table, neither of which
    // the loop mutates. Recomputing it per round put a whole-program walk inside
    // the pass's hot spot. `remove_unreachable_types` hoists it the same way.
    let kept_past_use = variant_decls_kept_past_use(project, &project.type_table.borrow());

    let mut changed = true;
    while changed {
        changed = false;
        let before_len = analysis.types.len();

        let type_table = project.type_table.borrow();

        // Rebuild the name-keyed indexes so the struct/variant checks
        // below are O(1) hash probes instead of O(N) `iter().any()`
        // scans. Without this the loop is O(S × N) per iteration —
        // ~2M `type_table.get`s/iter on a 900-struct / 2200-type
        // Gale-generated parser, dominating the whole DCE pass.
        analysis.refresh_indexes(&type_table);

        // A struct that survives the sweep keeps its field types.
        for tir_struct in &project.structs {
            if analysis.keeps_struct(tir_struct, &type_table) {
                for field in &tir_struct.fields {
                    collect_type_transitive(field.type_id, &type_table, &mut analysis.types);
                }
                // Monomorphization type args are used by WIR for name mangling
                if let Some(info) = &tir_struct.monomorph_info {
                    for &ta in &info.impl_type_args {
                        collect_type_transitive(ta, &type_table, &mut analysis.types);
                    }
                    for &ta in &info.method_type_args {
                        collect_type_transitive(ta, &type_table, &mut analysis.types);
                    }
                }
            }
        }

        // Same predicate as above but for variants: the base type, any
        // `GenericInstance` of the variant's name, or a declaration kept past
        // its last use keeps payloads alive. The same predicate gates the
        // `project.variants` retain in `remove_unreachable_types`.
        for variant in &project.variants {
            let base_reachable = analysis
                .variant_exact
                .contains(&(variant.name.clone(), variant.module_source.clone()));
            let instance_reachable = analysis
                .generic_instance_names
                .contains(variant.name.as_str());

            if base_reachable
                || instance_reachable
                || kept_past_use.contains(&(variant.name.clone(), variant.module_source.clone()))
            {
                for case in &variant.cases {
                    collect_type_transitive(case.payload, &type_table, &mut analysis.types);
                }
            }
        }

        // Collect type dependencies (array elements, option inner, etc.)
        let current_types: Vec<TypeId> = analysis.types.iter().copied().collect();
        for type_id in current_types {
            collect_type_dependencies(type_id, &type_table, &mut analysis.types);
        }

        drop(type_table);

        if analysis.types.len() > before_len {
            changed = true;
        }
    }

    // Final index refresh so downstream consumers (e.g. the retain
    // calls in `remove_unreachable_types`) see indexes matching the
    // converged `analysis.types` rather than the second-to-last snapshot.
    {
        let type_table = project.type_table.borrow();
        analysis.refresh_indexes(&type_table);
    }
}

/// Add a type and its dependencies to the reachable set
fn collect_type_transitive(
    type_id: TypeId,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    if reachable.contains(&type_id) {
        return;
    }
    reachable.insert(type_id);
    collect_type_dependencies(type_id, type_table, reachable);
}

/// Collect direct type dependencies (struct fields, array elements, etc.)
fn collect_type_dependencies(
    type_id: TypeId,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    match type_table.get(type_id) {
        ResolvedType::BuiltinArray(inner)
        | ResolvedType::Ref(inner)
        | ResolvedType::MutRef(inner)
        | ResolvedType::Reactive(inner) => {
            collect_type_transitive(*inner, type_table, reachable);
        }
        ResolvedType::GenericResource { type_args, .. } => {
            for &arg in type_args {
                collect_type_transitive(arg, type_table, reachable);
            }
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            for param in params {
                collect_type_transitive(*param, type_table, reachable);
            }
            collect_type_transitive(*return_type, type_table, reachable);
        }
        ResolvedType::GenericInstance { type_args, .. } => {
            for arg in type_args {
                collect_type_transitive(*arg, type_table, reachable);
            }
        }
        // An associated-type projection (`I::Item`) depends on the
        // parameter it projects from. Without following `param_id`, a
        // surviving projection (e.g. a field type of a retained generic
        // template) would dangle when the parameter type is pruned,
        // crashing later name-mangling.
        ResolvedType::AssocTypeProjection { param_id, .. } => {
            collect_type_transitive(*param_id, type_table, reachable);
        }

        // Leaf types - no dependencies
        ResolvedType::Primitive(_)
        | ResolvedType::Unit
        | ResolvedType::Never
        | ResolvedType::Unknown
        | ResolvedType::Error
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. } => {}

        // Newtype: collect dependency on base type
        ResolvedType::Newtype { base_type, .. } => {
            collect_type_transitive(*base_type, type_table, reachable);
        }
        // Flags: depends on u32 (always reachable, no-op)
        ResolvedType::Flags { .. } => {}
    }
}

/// Remove unreachable types from the project's `TypeTable` and module definitions.
///
/// `analysis` is precomputed by [`analyze_dce`] — this function only
/// retains entries matching its precomputed indexes.
pub fn remove_unreachable_types(project: &mut NirPackage, analysis: &DceAnalysis) {
    {
        let type_table = project.type_table.borrow();
        project
            .structs
            .retain(|s| analysis.keeps_struct(s, &type_table));
    }
    // Loop-invariant, and the loop it would otherwise sit in is this pass's hot
    // spot: it reads `project` and the type table, neither of which the retains
    // below mutate.
    let kept_past_use = {
        let type_table = project.type_table.borrow();
        variant_decls_kept_past_use(project, &type_table)
    };
    project.variants.retain(|v| {
        analysis
            .variant_exact
            .contains(&(v.name.clone(), v.module_source.clone()))
            || analysis.generic_instance_names.contains(v.name.as_str())
            || kept_past_use.contains(&(v.name.clone(), v.module_source.clone()))
    });
    project.enums.retain(|e| {
        analysis
            .enum_exact
            .contains(&(e.name.clone(), e.module_source.clone()))
    });

    // Remove unreachable entries from the shared TypeTable.
    // This ensures that subsequent phases (WIR type registration, codegen) do not
    // emit types that are no longer referenced by any surviving function.
    // Plus the variant every scalarized return came from: dropping its `TypeId`
    // would make `optimize::sroa_variant_return` unable to resolve the layout it
    // recognises its own earlier work by.
    let mut keep = analysis.types.clone();
    for func_rc in &project.functions {
        if let Some(t) = func_rc.borrow().scalarized_from {
            keep.insert(t);
        }
    }
    project.type_table.borrow_mut().retain(&keep);
}

// ──────────────────────────────────────────────────────────────────────────────
// Global variable DCE
// ──────────────────────────────────────────────────────────────────────────────

/// Every statement id reachable from the body root. The arena keeps the nodes an
/// in-place rewrite displaced, and one nothing refers to never runs.
fn reachable_stmt_ids(body: &Body) -> Vec<StmtId> {
    struct Collect(Vec<StmtId>);
    impl crate::nir_visitor::NirRefVisitor for Collect {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Stmt(s) = node {
                self.0.push(s);
            }
            self.walk_node(body, node);
        }
    }
    if body.blocks.is_empty() {
        return Vec::new();
    }
    let mut collect = Collect(Vec::new());
    crate::nir_visitor::NirRefVisitor::visit_node(&mut collect, body, NodeRef::Block(body.root));
    collect.0
}

/// Locals some reachable expression mentions, plus those a promoted
/// `Opaque(Local)` reads from the value pool — that read is not in the skeleton,
/// and a binding taken for dead under it loses a value someone still extracts.
/// A binding absent here is never read: an assignment target, a borrow and a
/// capture all mention their local, so the census over-approximates and only
/// ever keeps a statement alive.
fn mentioned_locals(body: &Body) -> IndexSet<u32> {
    let mut out: IndexSet<u32> = body.values.opaque_local_sources().collect();
    for e in crate::nir_visitor::reachable_exprs(body) {
        if let ExprKind::Local { index, .. } = &body.exprs[e].kind {
            out.insert(*index);
        }
    }
    out
}

/// The `GlobalVarGet`s in `expr`'s subtree, and the ids that read them.
fn global_reads_in(body: &Body, expr: ExprId) -> Vec<(ExprId, (String, String))> {
    struct Collect(Vec<(ExprId, (String, String))>);
    impl crate::nir_visitor::NirRefVisitor for Collect {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node
                && let ExprKind::GlobalVarGet {
                    module_source,
                    name,
                } = &body.exprs[e].kind
            {
                self.0
                    .push((e, (module_source.to_path().join("::"), name.clone())));
            }
            self.walk_node(body, node);
        }
    }
    let mut collect = Collect(Vec::new());
    crate::nir_visitor::NirRefVisitor::visit_node(&mut collect, body, NodeRef::Expr(expr));
    collect.0
}

/// Whether the pass may delete `value` outright — no observable effect, and no
/// trap, since a trap is observable too.
///
/// The expression predicate answers first, with its typed refinement, for a
/// literal aggregate. Failing that the tree is walked: it refuses every call on
/// sight, while the initializer globalization hoists for a reflect member walk
/// *is* a call, so each one is answered by its whole-function summary instead.
fn deletable_value(
    body: &Body,
    value: Operand,
    types: &TypeTable,
    effects: &[super::mod_ref::FnEffect],
) -> bool {
    use cranelift_entity::EntityRef;

    if super::arena_query::is_pure_nontrapping_operand_typed(body, value, Some(types)) {
        return true;
    }
    let Some(root) = value.as_expr() else {
        return false;
    };
    let mut stack = vec![NodeRef::Expr(root)];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Expr(id) => match &body.exprs[id].kind {
                ExprKind::Call { func_id, .. } => {
                    let effect = effects
                        .get(func_id.index())
                        .copied()
                        .unwrap_or_else(super::mod_ref::FnEffect::opaque);
                    if !effect.is_pure() || effect.may_trap {
                        return false;
                    }
                }
                ExprKind::GlobalVarSet { .. }
                | ExprKind::Assign { .. }
                | ExprKind::IndirectCall { .. }
                | ExprKind::CmRawCall { .. } => return false,
                _ => {
                    if super::arena_query::expr_node_may_trap(body, id) {
                        return false;
                    }
                }
            },
            // A block statement that is not a binding or a discarded value
            // leaves the region, and deleting it would take the exit with it.
            NodeRef::Stmt(s) => {
                if !matches!(body.stmts[s].kind, StmtKind::Let { .. } | StmtKind::Expr(_)) {
                    return false;
                }
            }
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    true
}

fn lazy_guard_global(
    body: &Body,
    stmt: StmtId,
    descriptors: &[FunctionRef],
    types: &TypeTable,
    effects: &[super::mod_ref::FnEffect],
) -> Option<(ExprId, (String, String), Operand)> {
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    let ExprKind::Call { func_id, args, .. } = &body.exprs[condition.as_expr()?].kind else {
        return None;
    };
    let callee = callee_descriptor(descriptors, *func_id);
    if !(callee.module_source.is_core_builtin() && callee.name == "is_uninitialized") {
        return None;
    }
    let [arg] = args.as_slice() else {
        return None;
    };
    let read = arg.expr.as_expr()?;
    let ExprKind::GlobalVarGet {
        module_source,
        name,
    } = &body.exprs[read].kind
    else {
        return None;
    };
    let [only] = body.blocks[*then_block].stmts.as_slice() else {
        return None;
    };
    let StmtKind::Expr(Operand::Expr(set)) = &body.stmts[*only].kind else {
        return None;
    };
    let ExprKind::GlobalVarSet {
        module_source: set_module,
        name: set_name,
        value,
    } = &body.exprs[*set].kind
    else {
        return None;
    };
    if set_module != module_source || set_name != name {
        return None;
    }
    // Dropping the guard drops the value it stores, so a value whose trap the
    // program is entitled to is not a guard this pass may take.
    if !deletable_value(body, *value, types, effects) {
        return None;
    }
    Some((
        read,
        (module_source.to_path().join("::"), name.clone()),
        *value,
    ))
}

/// Whether `stmt` binds a local nothing mentions to a value the pass may
/// delete — a binding that computes something and drops it. A trap is an
/// observable effect, so a trapping value keeps the binding alive even though
/// nobody reads it.
fn dead_pure_binding(
    body: &Body,
    stmt: StmtId,
    mentioned: &IndexSet<u32>,
    types: &TypeTable,
) -> Option<ExprId> {
    let StmtKind::Let {
        local_index, value, ..
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    if mentioned.contains(local_index) {
        return None;
    }
    let value = value.as_expr()?;
    super::arena_query::is_pure_nontrapping_expr_typed(body, value, Some(types)).then_some(value)
}

/// Un-hoist a constant globalization hoisted for nobody.
///
/// Globalization moves a constant aggregate into a shared slot and guards the
/// store; the folds that run after it can take every reader with them. What is
/// left computes a value nothing observes, and holds whatever the initializer
/// builds — a reflect member walk and the strings it names — in the binary to
/// do it.
///
/// Two reads do not count as observing the value:
///
/// - the global's own `is_uninitialized` guard, which exists to decide the
///   store rather than to use what it holds;
/// - a read bound to a local nothing mentions, which is what folding a member's
///   facts out of the walk leaves behind.
///
/// Both are conditional on the value being one this pass may delete
/// ([`deletable_value`]) — a trap is observed like any other effect.
///
/// A global with no observation left loses its guard, its store, and those
/// bindings. It then has no reads at all, which the reachability census below
/// already answers, and its initializer goes with it.
pub fn unhoist_unobserved_globals(project: &mut NirPackage) {
    let descriptors = build_callee_descriptors(project);
    let effects = super::mod_ref::compute_fn_effects(&project.functions, &project.builtin_registry);
    let type_table = project.type_table.clone();
    let types = type_table.borrow();
    let mut guarded: IndexSet<(String, String)> = IndexSet::default();
    let mut observed: IndexSet<(String, String)> = IndexSet::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(body) = func.body.as_ref() else {
            continue;
        };
        let mentioned = mentioned_locals(body);
        let mut unobserving: IndexSet<ExprId> = IndexSet::default();
        for stmt in reachable_stmt_ids(body) {
            if let Some((read, key, _)) =
                lazy_guard_global(body, stmt, &descriptors, &types, &effects)
            {
                guarded.insert(key);
                unobserving.insert(read);
            }
            if let Some(value) = dead_pure_binding(body, stmt, &mentioned, &types) {
                unobserving.extend(global_reads_in(body, value).into_iter().map(|(e, _)| e));
            }
        }
        for e in crate::nir_visitor::reachable_exprs(body) {
            if let ExprKind::GlobalVarGet {
                module_source,
                name,
            } = &body.exprs[e].kind
                && !unobserving.contains(&e)
            {
                observed.insert((module_source.to_path().join("::"), name.clone()));
            }
        }
    }
    let unobserved: IndexSet<(String, String)> = guarded.difference(&observed).cloned().collect();
    if unobserved.is_empty() {
        return;
    }
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            let mentioned = mentioned_locals(body);
            for block in reachable_block_ids(body) {
                drop_unobserved_stmts(
                    body,
                    block,
                    &unobserved,
                    &mentioned,
                    &descriptors,
                    &types,
                    &effects,
                );
            }
        }
    }
}

/// Every block reachable from the body root. The drop below must see the same
/// statements the census above classified — an expression-position block among
/// them, which is where inlining leaves a guard — or a read it counted as
/// non-observing outlives the store it was counted against.
fn reachable_block_ids(body: &Body) -> Vec<BlockId> {
    struct Collect(Vec<BlockId>);
    impl crate::nir_visitor::NirRefVisitor for Collect {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Block(b) = node {
                self.0.push(b);
            }
            self.walk_node(body, node);
        }
    }
    if body.blocks.is_empty() {
        return Vec::new();
    }
    let mut collect = Collect(Vec::new());
    crate::nir_visitor::NirRefVisitor::visit_node(&mut collect, body, NodeRef::Block(body.root));
    collect.0
}

fn drop_unobserved_stmts(
    body: &mut Body,
    block: BlockId,
    unobserved: &IndexSet<(String, String)>,
    mentioned: &IndexSet<u32>,
    descriptors: &[FunctionRef],
    types: &TypeTable,
    effects: &[super::mod_ref::FnEffect],
) {
    let old = std::mem::take(&mut body.blocks[block].stmts);
    let mut kept: Vec<StmtId> = Vec::with_capacity(old.len());
    for s in old {
        let is_guard = lazy_guard_global(body, s, descriptors, types, effects)
            .is_some_and(|(_, key, _)| unobserved.contains(&key));
        let is_dead_read = dead_pure_binding(body, s, mentioned, types).is_some_and(|value| {
            global_reads_in(body, value)
                .iter()
                .any(|(_, key)| unobserved.contains(key))
        });
        if !is_guard && !is_dead_read {
            kept.push(s);
        }
    }
    body.blocks[block].stmts = kept;
}

/// Union every `(module_key, global_name)` pair read by some reachable
/// function. Reads come from the per-function index built once by
/// [`build_analysis_graph`]'s [`DceWalker`] walk; functions not in
/// `reachable_functions` are skipped since they'll be removed by
/// `remove_unreachable_functions`.
fn compute_global_reachability(
    graph: &AnalysisGraph,
    reachable_functions: &IndexSet<usize>,
) -> IndexSet<(String, String)> {
    let mut used_globals: IndexSet<(String, String)> = IndexSet::default();
    for &pos in reachable_functions {
        if let Some(per_func) = graph.per_func_globals.get(pos) {
            for entry in per_func {
                used_globals.insert(entry.clone());
            }
        }
    }
    used_globals
}

/// Retain only globals whose `(module_key, name)` is in
/// `used_globals` (computed by [`analyze_dce`]), then strip every
/// `GlobalVarSet` for a dead global from surviving function bodies
/// (covers both the original `__initialize_module` and any inlined
/// copies).
pub fn remove_unreachable_globals(
    project: &mut NirPackage,
    used_globals: &IndexSet<(String, String)>,
) {
    project.globals.retain(|global| {
        let global_module_key = global.module_source.to_path().join("::");
        used_globals.contains(&(global_module_key, global.name.clone()))
    });

    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            let root = body.root;
            remove_dead_global_sets_block(body, root, used_globals, &type_table);
        }
    }
}

/// Remove `GlobalVarSet` statements for dead globals from a block.
///
/// For dead globals whose initializer contains function calls (potential side
/// effects), the `GlobalVarSet` is replaced with the value expression to
/// preserve the side effects. For pure initializers (constants, struct/array
/// literals without calls), the entire statement is removed.
fn remove_dead_global_sets_block(
    body: &mut Body,
    block: BlockId,
    used: &IndexSet<(String, String)>,
    type_table: &TypeTable,
) {
    // Recurse into sub-statements first.
    for s in body.blocks[block].stmts.clone() {
        remove_dead_global_sets_stmt(body, s, used, type_table);
    }

    // Process GlobalVarSet statements for dead globals.
    let old = std::mem::take(&mut body.blocks[block].stmts);
    let mut new_stmts: Vec<StmtId> = Vec::with_capacity(old.len());
    for s in old {
        let dead = if let StmtKind::Expr(Operand::Expr(expr)) = &body.stmts[s].kind
            && let ExprKind::GlobalVarSet {
                module_source,
                name,
                value,
                ..
            } = &body.exprs[*expr].kind
        {
            let key = (module_source.to_path().join("::"), name.clone());
            if used.contains(&key) {
                None
            } else {
                Some((*value, body.stmts[s].span))
            }
        } else {
            None
        };
        if let Some((value, span)) = dead {
            // Dead global: keep the value expression when it is not provably
            // pure-and-nontrapping, so an initializer that calls a function
            // (writing another global, printing, asserting) or that can trap
            // keeps its effect/trap even though the global itself is gone.
            // The discarded GlobalVarSet owned `value`, so reuse its id here.
            if let Some(ve) = value.as_expr()
                && !super::arena_query::is_pure_nontrapping_expr_typed(body, ve, Some(type_table))
            {
                let new_s = body.stmts.push(StmtNode {
                    kind: StmtKind::Expr(ve.into()),
                    span,
                });
                new_stmts.push(new_s);
            }
            continue;
        }
        new_stmts.push(s);
    }
    body.blocks[block].stmts = new_stmts;
}

fn remove_dead_global_sets_stmt(
    body: &mut Body,
    s: StmtId,
    used: &IndexSet<(String, String)>,
    type_table: &TypeTable,
) {
    enum W {
        Expr(ExprId),
        Blocks(BlockId, Option<BlockId>),
        None,
    }
    let w = match &body.stmts[s].kind {
        StmtKind::Expr(expr) => expr.as_expr().map_or(W::None, W::Expr),
        StmtKind::Let { value, .. } => value.as_expr().map_or(W::None, W::Expr),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => W::Blocks(*then_block, *else_block),
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => W::Blocks(*b, None),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            value.and_then(Operand::as_expr).map_or(W::None, W::Expr)
        }
        StmtKind::Continue | StmtKind::LetDestructure { .. } => W::None,
    };
    match w {
        W::Expr(e) => remove_dead_global_sets_expr(body, e, used, type_table),
        W::Blocks(b0, b1) => {
            remove_dead_global_sets_block(body, b0, used, type_table);
            if let Some(b1) = b1 {
                remove_dead_global_sets_block(body, b1, used, type_table);
            }
        }
        W::None => {}
    }
}

/// Recursively remove dead `GlobalVarSet` from expressions that contain blocks.
/// Strip dead-global stores from `e`'s subtree.
///
/// Every child, not a hand-listed few. A dead store can sit under any
/// operand-carrying kind — globalization's inline-reference shape puts one
/// under a borrow, `&{ GLOBAL = v; GLOBAL }` — and a kind missing from such a
/// list keeps the store while the global itself goes, leaving an access to a
/// slot that no longer exists.
fn remove_dead_global_sets_expr(
    body: &mut Body,
    e: ExprId,
    used: &IndexSet<(String, String)>,
    type_table: &TypeTable,
) {
    let mut children: Vec<NodeRef> = Vec::new();
    body.for_each_child(NodeRef::Expr(e), |c| children.push(c));
    for child in children {
        match child {
            NodeRef::Block(b) => remove_dead_global_sets_block(body, b, used, type_table),
            NodeRef::Stmt(s) => remove_dead_global_sets_stmt(body, s, used, type_table),
            NodeRef::Expr(x) => remove_dead_global_sets_expr(body, x, used, type_table),
            NodeRef::Pat(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_source::ModuleSourceInterner;

    fn free_fn(interner: &mut ModuleSourceInterner, name: &str) -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(interner, &["test"], name))
    }

    #[test]
    fn test_empty_reachable_set() {
        let mut interner = ModuleSourceInterner::new();
        let call_graph = IndexMap::default();
        let entry = free_fn(&mut interner, "run");
        let reachable = compute_reachable(&call_graph, &entry);
        assert!(reachable.contains(&free_fn(&mut interner, "run")));
        assert_eq!(reachable.len(), 1);
    }

    #[test]
    fn test_transitive_reachability() {
        let mut interner = ModuleSourceInterner::new();
        let mut call_graph = IndexMap::default();
        call_graph.insert(
            free_fn(&mut interner, "run"),
            IndexSet::from_iter([free_fn(&mut interner, "foo")]),
        );
        call_graph.insert(
            free_fn(&mut interner, "foo"),
            IndexSet::from_iter([free_fn(&mut interner, "bar")]),
        );
        call_graph.insert(free_fn(&mut interner, "bar"), IndexSet::default());
        call_graph.insert(
            free_fn(&mut interner, "unused"),
            IndexSet::from_iter([free_fn(&mut interner, "bar")]),
        );

        let reachable = compute_reachable(&call_graph, &free_fn(&mut interner, "run"));
        assert!(reachable.contains(&free_fn(&mut interner, "run")));
        assert!(reachable.contains(&free_fn(&mut interner, "foo")));
        assert!(reachable.contains(&free_fn(&mut interner, "bar")));
        assert!(!reachable.contains(&free_fn(&mut interner, "unused")));
    }
}
