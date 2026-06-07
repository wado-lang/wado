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
    FreeFunctionName, FunctionId, MethodName, mangle_generic_name, mangle_local_method,
    mangle_local_trait_method, mangle_method_generic,
};
use crate::nir::{NirFunction, NirImport};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, PatKind, StmtId, StmtKind, StmtNode,
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
    struct_name: String,
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
    // Single AST walk per function body: build the call graph and
    // collect per-function used-globals / used-types in one go.
    let mut graph = build_analysis_graph(project);

    let mut analysis = DceAnalysis::empty();
    analysis.functions = compute_function_reachability(project, &mut graph);
    analysis.globals = compute_global_reachability(&graph, &analysis.functions);
    populate_type_reachability(project, &graph, &mut analysis);
    analysis
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
    let inspectable = collect_inspectable_signatures_from_reachable(project, &reachable_v1);
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
    extend_reachable_for_optimizer_passes(project, &graph.call_graph, &mut reachable);

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
    call_graph: &CallGraph,
    reachable: &mut IndexSet<FunctionId>,
) {
    use crate::compiler_item::CompilerItem;

    let mut push_str_id: Option<FunctionId> = None;
    let mut push_char_id: Option<FunctionId> = None;
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        match func.compiler_item {
            Some(CompilerItem::StringPushStr) => {
                push_str_id = Some(function_id_for(&func));
            }
            Some(CompilerItem::StringPushChar) => {
                push_char_id = Some(function_id_for(&func));
            }
            _ => {}
        }
    }
    if let (Some(str_id), Some(char_id)) = (push_str_id, push_char_id)
        && reachable.contains(&str_id)
        && !reachable.contains(&char_id)
    {
        reachable.extend(compute_reachable(call_graph, &char_id));
    }

    // `$value_copy$T<id>` helpers synthesized by `lower::plan::value_copy`
    // can be reached via `array_clone::<T>(arr)` for value-typed `T`: that
    // lowers to `WirInstr::ArrayClone { element_copy_func: Some(...) }`
    // where the helper appears as a *string* at WIR codegen time, not as a
    // NIR call edge — so the regular call graph misses it. Walk every
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
    let helpers_by_type_id: IndexMap<crate::tir::TypeId, FunctionId> = project
        .functions
        .iter()
        .filter_map(|func_rc| {
            let func = func_rc.borrow();
            if let crate::nir::FunctionKind::ValueCopy { type_id } = func.kind {
                Some((type_id, function_id_for(&func)))
            } else {
                None
            }
        })
        .collect();
    // Iterate to a fixpoint: a helper newly marked reachable may itself
    // call `array_clone::<T'>` for some `T'` whose helper isn't reachable
    // yet, and `compute_reachable` only follows direct call-graph edges
    // (it doesn't replay the array_clone scan). Single-pass would drop
    // inner helpers for chains like `List<List<List<T>>>`, panicking
    // codegen with `WirInstr::ArrayClone references unknown helper ...`.
    loop {
        let mut added_this_round = false;
        for func_rc in &project.functions {
            let func = func_rc.borrow();
            let func_id = function_id_for(&func);
            if !reachable.contains(&func_id) {
                continue;
            }
            if let Some(body) = func.body.as_ref() {
                let mut needed: IndexSet<crate::tir::TypeId> = IndexSet::default();
                collect_array_clone_element_types(body, &mut needed);
                for type_id in needed {
                    if let Some(helper_id) = helpers_by_type_id.get(&type_id)
                        && !reachable.contains(helper_id)
                    {
                        reachable.extend(compute_reachable(call_graph, helper_id));
                        added_this_round = true;
                    }
                }
            }
        }
        if !added_this_round {
            break;
        }
    }
}

/// Walk `block`'s expression tree and collect every `T` such that
/// `builtin::array_clone::<T>(...)` appears as a NIR call. The
/// corresponding `$value_copy$T<id>` helper has to survive DCE because
/// codegen will reach it by *name* at WIR time.
fn collect_array_clone_element_types(body: &Body, out: &mut IndexSet<crate::tir::TypeId>) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let ExprKind::Call { func, .. } = &body.exprs[e].kind
            && func.module_source.is_core_builtin()
            && func.name == "array_clone"
            && let Some(mi) = func.monomorph_info.as_ref()
            && let Some(elem) = mi.impl_type_args.first().copied()
        {
            out.insert(elem);
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

    // Also mark WASI functions as used if indirect calls are present
    // (for ambient logging). The kiln generator world forbids every
    // WASI interface (WEP 2026-04-12 §"Design principles" #1), so the
    // `call_indirect_{stdout,stderr}_*` builtins are rewritten to
    // `unreachable` at the WIR level and never need the matching WASI
    // function registered as "used" — skip the usage registration so
    // the component doesn't transitively import `wasi:cli/stderr` or
    // `wasi:cli/stdout`. Gate on `import KilnHost` so the rule fires
    // for any kiln-generator-shaped world, not just the canonical
    // `core:kiln/generator`.
    if !project.world_imports_interface("KilnHost") {
        if reachable.iter().any(|func_id| {
            matches!(func_id, FunctionId::Free(f) if is_builtin_func(f) && {
                let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
                name.starts_with("call_indirect_stdout")
            })
        }) {
            used_wasi_functions.insert("Stdout::write_via_stream".to_string());
        }
        if reachable.iter().any(|func_id| {
            matches!(func_id, FunctionId::Free(f) if is_builtin_func(f) && {
                let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
                name.starts_with("call_indirect_stderr")
            })
        }) {
            used_wasi_functions.insert("Stderr::write_via_stream".to_string());
        }
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
        .map(|f| {
            let func = f.borrow();
            (func.module_source.clone(), func.name.clone())
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
/// tracking), bytes literals are stored inline as `ExprKind::BytesLiteral(Vec<u8>)`.
/// This function scans all surviving function bodies to collect referenced bytes,
/// then retains only matching entries in `project.bytes_literals`.
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
    // Collect every `BytesLiteral` reachable from `root`, excluding patterns
    // (the tree walk never descended into `LetDestructure` / match-arm
    // patterns, so a `BytesLiteral` inside a `ConstantValue` pattern is not
    // counted).
    let mut stack = vec![NodeRef::Block(root)];
    while let Some(node) = stack.pop() {
        if matches!(node, NodeRef::Pat(_)) {
            continue;
        }
        if let NodeRef::Expr(e) = node
            && let ExprKind::BytesLiteral(b) = &body.exprs[e].kind
        {
            used.insert(b.clone());
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}
/// Remove closure functors whose `__call` method was eliminated by function DCE.
pub fn remove_unreachable_closure_functors(project: &mut NirPackage) {
    // Build a set of surviving (module_source, func_name) pairs for O(1) lookup.
    // The __call method is named "{struct_name}::__call" (via MethodName::format_local).
    let surviving_funcs: IndexSet<(ModuleSource, String)> = project
        .functions
        .iter()
        .map(|f| {
            let func = f.borrow();
            (func.module_source.clone(), func.name.clone())
        })
        .collect();

    project.closure_functors.retain(|functor| {
        let call_method_name = format!("{}::__call", functor.struct_name);
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
fn build_analysis_graph(project: &NirPackage) -> AnalysisGraph {
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

        let mut walker = DceWalker::new(type_table, module_source);
        walker.analyze(&func);
        let analysis = walker.analysis;

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
            scan_inspect_signatures_block(body, type_table, &mut sigs);
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
            FunctionId::Method(MethodName::new(
                module_source.clone(),
                info.struct_name.clone(),
                info.trait_name.clone(),
                info.method_name.clone(),
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
    sigs: &mut InspectableSignatures,
) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let ExprKind::MethodCall { receiver, func, .. } = &body.exprs[e].kind
            && let Some(info) = &func.method_info
            && info.base_struct_name == "Fn"
            && let Some(trait_name) = info.base_trait_name.as_deref()
        {
            // Receiver is `&Fn(...)` (possibly wrapped in `Box<fn(...)>` by the
            // boxing pass); peel both to read the function's arity + return type.
            let recv_type = type_table.peel_refs_and_box(body.exprs[*receiver].type_id);
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
    analysis: FunctionAnalysis,
}

impl<'a> DceWalker<'a> {
    fn new(type_table: &'a TypeTable, current_module: &'a ModuleSource) -> Self {
        Self {
            type_table,
            current_module,
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

        if func.method_info.is_some() {
            // Static method call (e.g. `Box::get`, `String^Display::fmt`):
            // `func_name` is `"Struct::method"` or `"Struct^Trait::method"`.
            let callee_id = if func.is_monomorphized() {
                let base_name = func
                    .base_struct_name()
                    .map(|base| {
                        func_name
                            .find("::")
                            .map(|pos| format!("{}::{}", base, &func_name[pos + 2..]))
                            .unwrap_or_else(|| base)
                    })
                    .unwrap_or_else(|| func_name.clone());
                FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    func.module_source.clone(),
                    func_name.clone(),
                    base_name,
                ))
            } else {
                let callee_module = original_callee_module;
                if let Some(sep_pos) = func_name.find("::") {
                    let prefix = &func_name[..sep_pos];
                    let method_name = &func_name[sep_pos + 2..];
                    let (struct_name, trait_name): (&str, Option<&str>) =
                        if let Some(caret_pos) = prefix.find('^') {
                            (&prefix[..caret_pos], Some(&prefix[caret_pos + 1..]))
                        } else {
                            (prefix, None)
                        };
                    FunctionId::Method(MethodName::new(
                        callee_module,
                        struct_name.to_string(),
                        trait_name.map(String::from),
                        method_name.to_string(),
                    ))
                } else {
                    FunctionId::Free(FreeFunctionName::from_module_source(
                        &callee_module,
                        &func_name,
                    ))
                }
            };
            self.analysis.callees.insert(callee_id);

            // Resource method call on a WASI module — record as an effect.
            let module_path = func.module_path();
            if module_path.len() >= 2
                && module_path[0] == "wasi"
                && let Some(pos) = func_name.find("::")
            {
                let resource_name = &func_name[..pos];
                let method_name = &func_name[pos + 2..];
                self.analysis
                    .effect_calls
                    .insert((resource_name.to_string(), method_name.to_string()));
            }
        } else {
            // Free function call.
            debug_assert!(
                !func_name.contains("::") || func_name.starts_with("builtin::"),
                "ExprKind::Call should not have method-style names: {func_name}"
            );

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
                .map(|base| {
                    func_name
                        .find("::")
                        .map(|pos| format!("{}::{}", base, &func_name[pos + 2..]))
                        .unwrap_or_else(|| base)
                })
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
                info.struct_name.clone(),
                info.trait_name.clone(),
                info.method_name.clone(),
            ));
            self.analysis.callees.insert(resolved_id);
        }

        // If the receiver was a newtype (e.g., flags type), also mark
        // the newtype's own methods as reachable (e.g., Perms^Inspect::inspect).
        if let Some((newtype_name, newtype_module)) = newtype_info {
            let method_id = FunctionId::Method(MethodName::new(
                newtype_module,
                newtype_name,
                trait_name.clone(),
                method_name.clone(),
            ));
            self.analysis.callees.insert(method_id);
        }

        match base_receiver_type {
            ResolvedType::Struct {
                ref name,
                is_monomorphized: true,
                base_name: Some(ref base_struct),
                ..
            } => {
                // Monomorphized struct method (e.g. `Box<i32>::get`):
                // monomorphized functions live in the *using* module, so
                // route the callee id through `current_module`. The base
                // method name uses the original generic struct name so
                // the inlining-induced graph stays mergeable.
                let mangled_func_name =
                    MethodName::format_local(name, trait_name.as_deref(), &method_name);
                let base_method_name =
                    MethodName::format_local(base_struct, trait_name.as_deref(), &method_name);
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
                        info.struct_name.clone(),
                        info.trait_name.clone(),
                        info.method_name,
                    ));
                    self.analysis.callees.insert(original_method_id);
                }
            }
            ResolvedType::Struct {
                name,
                module_source,
                is_monomorphized: false,
                ..
            } => {
                // Non-monomorphized struct method.
                let method_id = FunctionId::Method(MethodName::new(
                    module_source.clone(),
                    name,
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
                        info.struct_name.clone(),
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
                    prim.as_str().to_string(),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::Unit => {
                // `()` methods: `().to_string()`, `().fmt(&f)`, etc.
                let method_id = FunctionId::Method(MethodName::new(
                    ModuleSource::primitive(),
                    TypeTable::UNIT_TYPE_NAME.to_string(),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source,
            } if TypeTable::is_tuple_type(&name, &module_source) => {
                // Tuple method call: synthesized as non-monomorphized
                // with struct_name `"Tuple<f64,f64>"`.
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_name(*t))
                    .collect();
                let mangled_struct =
                    mangle_generic_name(TypeTable::TUPLE_TYPE_NAME, &type_arg_names);
                let method_id = FunctionId::Method(MethodName::new(
                    self.current_module.clone(),
                    mangled_struct,
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
                    module_source,
                    name,
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
                    module_source,
                    name,
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
                let type_arg_names = vec![
                    params.len().to_string(),
                    self.type_table.mangle_type_name(return_type),
                ];
                let mangled_struct = mangle_generic_name("Fn", &type_arg_names);
                let method_id = FunctionId::Method(MethodName::new(
                    self.current_module.clone(),
                    mangled_struct,
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::GenericResource {
                name, type_args, ..
            } => {
                // Generic resource method, e.g. `Future<T>^Inspect::inspect`.
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_name(*t))
                    .collect();
                let mangled_struct = mangle_generic_name(name.as_str(), &type_arg_names);
                let method_id = FunctionId::Method(MethodName::new(
                    self.current_module.clone(),
                    mangled_struct,
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
        let struct_name = format!(
            "{prefix}{functor_id}",
            prefix = crate::name::CLOSURE_STRUCT_PREFIX,
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
                    ExprKind::Call { func, .. } => self.record_call(func),
                    ExprKind::MethodCall { receiver, func, .. } => {
                        self.record_method_call(body.exprs[*receiver].type_id, func);
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
                prim.as_str().to_string(),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Unit => {
            let method_id = FunctionId::Method(MethodName::new(
                ModuleSource::primitive(),
                TypeTable::UNIT_TYPE_NAME.to_string(),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Struct { name, .. } if name == "String" => {}
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

/// Retain only the functions whose original position is in
/// `reachable_positions` (computed by [`analyze_dce`]). Downstream
/// phases (`wir_build`, codegen) see every surviving function and
/// don't repeat reachability work.
pub fn remove_unreachable_functions(
    project: &mut NirPackage,
    reachable_positions: &IndexSet<usize>,
) {
    // Dense `Vec<bool>` indexed by original position avoids hashing each
    // index against `reachable_positions` once per retain step.
    let mut keep = vec![false; project.functions.len()];
    for &pos in reachable_positions {
        if pos < keep.len() {
            keep[pos] = true;
        }
    }
    let mut idx = 0;
    project.functions.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
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
                    name,
                    module_source,
                    is_monomorphized,
                    base_name,
                } => {
                    if *is_monomorphized {
                        self.struct_monomorph_names.insert(name.clone());
                        if let Some(base) = base_name {
                            self.struct_monomorph_bases.insert(base.clone());
                        }
                    } else {
                        self.struct_exact
                            .insert((name.clone(), module_source.clone()));
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
    graph: &AnalysisGraph,
    analysis: &mut DceAnalysis,
) {
    // Always include primitive types (TypeId 0-17)
    for i in 0..18 {
        analysis.types.insert(TypeId(i));
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
            let mut walker = DceWalker::new(&type_table, &global.module_source);
            let init_body = global.initializer.body();
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

        // A struct's fields are kept iff its Struct type, any
        // `GenericInstance` of its name, or any monomorphized variant
        // sharing its base name is reachable.
        for tir_struct in &project.structs {
            let struct_reachable = if tir_struct.monomorph_info.is_none() {
                analysis
                    .struct_exact
                    .contains(&(tir_struct.name.clone(), tir_struct.module_source.clone()))
                    || analysis
                        .generic_instance_names
                        .contains(tir_struct.name.as_str())
                    || analysis
                        .struct_monomorph_bases
                        .contains(tir_struct.name.as_str())
            } else {
                analysis
                    .struct_monomorph_names
                    .contains(tir_struct.name.as_str())
            };

            if struct_reachable {
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

        // Same predicate as above but for variants: the base type or
        // any `GenericInstance` of the variant's name keeps payloads
        // alive.
        for variant in &project.variants {
            let base_reachable = analysis
                .variant_exact
                .contains(&(variant.name.clone(), variant.module_source.clone()));
            let instance_reachable = analysis
                .generic_instance_names
                .contains(variant.name.as_str());

            if base_reachable || instance_reachable {
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
    // A struct is kept if:
    // 1. Its Struct type is reachable, OR
    // 2. Any GenericInstance with its base name is reachable (e.g., Box<i32> for Box)
    // 3. Any monomorphized Struct with its base name is reachable
    project.structs.retain(|s| {
        if s.monomorph_info.is_none() {
            analysis
                .struct_exact
                .contains(&(s.name.clone(), s.module_source.clone()))
                || analysis.generic_instance_names.contains(s.name.as_str())
                || analysis.struct_monomorph_bases.contains(s.name.as_str())
        } else {
            analysis.struct_monomorph_names.contains(s.name.as_str())
        }
    });
    project.variants.retain(|v| {
        analysis
            .variant_exact
            .contains(&(v.name.clone(), v.module_source.clone()))
            || analysis.generic_instance_names.contains(v.name.as_str())
    });
    project.enums.retain(|e| {
        analysis
            .enum_exact
            .contains(&(e.name.clone(), e.module_source.clone()))
    });

    // Remove unreachable entries from the shared TypeTable.
    // This ensures that subsequent phases (WIR type registration, codegen) do not
    // emit types that are no longer referenced by any surviving function.
    project.type_table.borrow_mut().retain(&analysis.types);
}

// ──────────────────────────────────────────────────────────────────────────────
// Global variable DCE
// ──────────────────────────────────────────────────────────────────────────────

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

    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            let root = body.root;
            remove_dead_global_sets_block(body, root, used_globals);
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
) {
    // Recurse into sub-statements first.
    for s in body.blocks[block].stmts.clone() {
        remove_dead_global_sets_stmt(body, s, used);
    }

    // Process GlobalVarSet statements for dead globals.
    let old = std::mem::take(&mut body.blocks[block].stmts);
    let mut new_stmts: Vec<StmtId> = Vec::with_capacity(old.len());
    for s in old {
        let dead = if let StmtKind::Expr(expr) = &body.stmts[s].kind
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
            // Dead global: keep the value expression only if it has side
            // effects (e.g. panic() / unreachable — detected via never type).
            // The discarded GlobalVarSet owned `value`, so reuse its id here.
            if expr_has_side_effects(body, value) {
                let new_s = body.stmts.push(StmtNode {
                    kind: StmtKind::Expr(value),
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

/// Check whether an expression tree contains observable side effects.
///
/// Only diverging expressions (type `never` — e.g. `panic()`, `unreachable()`) are
/// considered side effects. Pure function calls like array construction are not.
fn expr_has_side_effects(body: &Body, e: ExprId) -> bool {
    if body.exprs[e].type_id == TypeTable::NEVER {
        return true;
    }
    match &body.exprs[e].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            block_has_side_effects(body, *block)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_side_effects(body, *condition)
                || block_has_side_effects(body, *then_branch)
                || else_branch.is_some_and(|b| block_has_side_effects(body, b))
        }
        ExprKind::Match { expr, arms } => {
            expr_has_side_effects(body, *expr)
                || arms.iter().any(|a| {
                    a.guard.is_some_and(|g| expr_has_side_effects(body, g))
                        || expr_has_side_effects(body, a.body)
                })
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_side_effects(body, *scrutinee)
                || arms.iter().any(|a| block_has_side_effects(body, *a))
                || block_has_side_effects(body, *default)
        }
        _ => false,
    }
}

fn block_has_side_effects(body: &Body, block: BlockId) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .any(|s| match &body.stmts[*s].kind {
            StmtKind::Expr(e) | StmtKind::Let { value: e, .. } => expr_has_side_effects(body, *e),
            StmtKind::Return { value } => value.is_some_and(|v| expr_has_side_effects(body, v)),
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                expr_has_side_effects(body, *condition)
                    || block_has_side_effects(body, *then_block)
                    || else_block.is_some_and(|b| block_has_side_effects(body, b))
            }
            StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
                block_has_side_effects(body, *b)
            }
            StmtKind::Break { value, .. } => value.is_some_and(|v| expr_has_side_effects(body, v)),
            StmtKind::Continue => false,
            StmtKind::LetDestructure { value, .. } => expr_has_side_effects(body, *value),
        })
}

fn remove_dead_global_sets_stmt(body: &mut Body, s: StmtId, used: &IndexSet<(String, String)>) {
    enum W {
        Expr(ExprId),
        Blocks(BlockId, Option<BlockId>),
        None,
    }
    let w = match &body.stmts[s].kind {
        StmtKind::Expr(expr) | StmtKind::Let { value: expr, .. } => W::Expr(*expr),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => W::Blocks(*then_block, *else_block),
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => W::Blocks(*b, None),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => match value {
            Some(expr) => W::Expr(*expr),
            None => W::None,
        },
        StmtKind::Continue | StmtKind::LetDestructure { .. } => W::None,
    };
    match w {
        W::Expr(e) => remove_dead_global_sets_expr(body, e, used),
        W::Blocks(b0, b1) => {
            remove_dead_global_sets_block(body, b0, used);
            if let Some(b1) = b1 {
                remove_dead_global_sets_block(body, b1, used);
            }
        }
        W::None => {}
    }
}

/// Recursively remove dead `GlobalVarSet` from expressions that contain blocks.
fn remove_dead_global_sets_expr(body: &mut Body, e: ExprId, used: &IndexSet<(String, String)>) {
    enum W {
        Block(BlockId),
        If(ExprId, BlockId, Option<BlockId>),
        Match(ExprId, Vec<ExprId>),
        Switch(Vec<BlockId>, BlockId),
        None,
    }
    let w = match &body.exprs[e].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => W::Block(*block),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => W::If(*condition, *then_branch, *else_branch),
        ExprKind::Match { expr, arms } => W::Match(*expr, arms.iter().map(|a| a.body).collect()),
        ExprKind::Switch { arms, default, .. } => W::Switch(arms.clone(), *default),
        _ => W::None,
    };
    match w {
        W::Block(b) => remove_dead_global_sets_block(body, b, used),
        W::If(cond, then_b, else_b) => {
            remove_dead_global_sets_expr(body, cond, used);
            remove_dead_global_sets_block(body, then_b, used);
            if let Some(eb) = else_b {
                remove_dead_global_sets_block(body, eb, used);
            }
        }
        W::Match(scrutinee, bodies) => {
            remove_dead_global_sets_expr(body, scrutinee, used);
            for b in bodies {
                remove_dead_global_sets_expr(body, b, used);
            }
        }
        W::Switch(arms, default) => {
            for a in arms {
                remove_dead_global_sets_block(body, a, used);
            }
            remove_dead_global_sets_block(body, default, used);
        }
        W::None => {}
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
