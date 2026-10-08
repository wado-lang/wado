//! Reachability analysis and dead-code elimination for the NIR package.

use std::borrow::Cow;
use std::ops::ControlFlow;
use std::sync::Arc;

use cranelift_entity::EntityRef;

use super::arena_query::is_pure_nontrapping_operand_typed;
use super::body_memo::BodyMemo;
use super::mod_ref::{CallFacts, FnSummaries};
use crate::parallel::Executor;

use crate::canonical::CmCallTarget;
use crate::hashmap::IndexSet;

use crate::compiler_item::CompilerItem;
use crate::component_model::operation_key;
use crate::defs::DefId;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::{
    FqTypeName, FreeFunctionName, FunctionId, MODULE_INIT_FUNCTION, MethodName, UNIT_TYPE_NAME,
    is_fn_type_name, mangle_generic_name, mangle_local_trait_method, mangle_method_generic,
};
use crate::nir::{FuncId, FunctionRef, NirFunction, NirImport, NirStruct};
use crate::nir_arena::{
    BlockId, BlockNode, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
    StmtNode,
};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::nir_visitor::{NirRefVisitor, reachable_exprs};
use crate::optimize::arena_query::{
    expr_node_may_trap, operand_values_may_trap, promoted_local_reads,
};
use crate::tir::{ResolvedType, StructDef, TypeId, TypeTable, projection_arguments};
use crate::{hashmap, nir};

/// Call graph: function ID -> set of called function IDs
type CallGraph = IndexMap<FunctionId, IndexSet<FunctionId>>;

/// Effect usage: function ID -> set of (`interface_name`, `operation_name`) pairs
type EffectUsageMap = IndexMap<FunctionId, IndexSet<(String, String)>>;

/// A pending `$Closure_N` `inspect` edge collected during the call-graph
/// walk. The edge is only added to the graph once the inspectable signature set
/// (computed from the reachable-without-inspect-roots set) is known. Storing them
/// out-of-band lets us build the call graph in a single AST walk instead of twice.
#[derive(Debug, Clone, PartialEq)]
struct PendingInspectEdge {
    /// The functor's `^Inspect` impl.
    inspect: FunctionId,
    /// `(arity, return_type)` key into `InspectableSignatures`.
    key: (usize, TypeId),
}

/// Analysis results for a single function
#[derive(Debug, Clone, Default, PartialEq)]
struct FunctionAnalysis {
    /// Functions called by this function
    callees: IndexSet<FunctionId>,
    /// Effect calls: (`interface_name`, `op_name`)
    effect_calls: IndexSet<(String, String)>,
    /// Pending `$Closure_N^Inspect::inspect` edges, added to the graph by
    /// `apply_inspect_edges` once the inspectable-signature set is known.
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
    /// The functions whose names `callees` and `pending_inspects` hold, by
    /// store position: a rename of one stales them.
    named: IndexSet<FuncId>,
    /// The signatures this function's `fn(..)^Inspect` calls receive.
    inspect_signatures: InspectableSignatures,
    /// Each `T` this function calls `array_clone::<T>` on.
    array_clone_elems: IndexSet<TypeId>,
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
    /// Whether `s` survives the sweep — the one predicate, read both by the walk
    /// that pulls a struct's field types into the reachable set and by the retain
    /// that drops the rest. A struct's stored `name` predates newtype / flags
    /// erasure while the reachable set renders after it (`FlagsBit<Perms>`
    /// against `FlagsBit<u32>`), so both spellings count.
    fn keeps_struct(&self, s: &NirStruct, type_table: &TypeTable) -> bool {
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
        // The struct's own head, not a declaration found by `generic_name`:
        // that index answers with whichever declaration of a spelling came
        // first, so two functions' `struct Box<T>` rendered one name and one
        // of them was swept while its uses survived.
        let rendered = type_table.struct_rendered_name(s.def, &mono.impl_type_args);
        self.struct_monomorph_names.contains(rendered.as_str())
            || type_table
                .find_struct_by_name(&rendered, &s.module_source)
                .is_some_and(|id| self.types.contains(&id))
    }
}

/// Compute every DCE input from the unpruned `project` in dependency order:
/// functions, then globals, then types. Each downstream `remove_*` is then a
/// pure mutator over the matching field. The split also puts type reachability
/// before `remove_unreachable_globals` mutates function bodies — those mutations
/// expose no new types, but the ordering makes that invariant observable.
pub(super) fn analyze_dce(
    project: &mut NirPackage,
    cache: &mut DescriptorCache,
    exec: &Executor,
) -> DceAnalysis {
    // The callee descriptor for every `FuncId`. A call's callee is identified by
    // its stamped `func_id` (born resolved, authoritative — `wir_build` never
    // falls back to name resolution for a NIR call), and the record at that id
    // carries the identical identity (name / module / method_info /
    // monomorph_info) the call node's `FunctionRef` used to. Indexed by
    // `func_id.index()` (== store position, Phase 4a), so the reachability walk
    // reads identity by id without a self-borrowing `store[id]` deref.
    let descriptors = cache.descriptors(project);

    // Single AST walk per function body: build the call graph and
    // collect per-function used-globals / used-types in one go.
    let mut graph = build_analysis_graph(project, descriptors, exec);

    let mut analysis = DceAnalysis::empty();
    analysis.functions = compute_function_reachability(project, &mut graph);
    analysis.globals = compute_global_reachability(&graph, &analysis.functions);
    populate_type_reachability(project, descriptors, &graph, &mut analysis);
    analysis
}

/// Callers relevant to interprocedural facts, including cached rewrite targets.
pub(super) fn reachable_function_positions(
    project: &mut NirPackage,
    cache: &mut DescriptorCache,
    walks: &mut ReachabilityCache,
    exec: &Executor,
    cached: impl IntoIterator<Item = FuncId>,
) -> IndexSet<usize> {
    let descriptors = cache.descriptors(project);
    let functors = functor_methods(project);
    let analyses = walks.analyses(project, exec, descriptors, &functors);
    let mut graph = assemble_analysis_graph(project, Cow::Borrowed(analyses), functors);
    let mut reachable = compute_function_reachability(project, &mut graph);
    let roots = cached
        .into_iter()
        .map(|id| function_id_for(&project.functions[id.index()].borrow()));
    let callees = compute_reachable(&graph.call_graph, roots);
    reachable.extend(compute_reachable_positions(&callees, &graph.func_positions));
    reachable
}

/// `NirPackage::callee_descriptors_from` held across the whole of optimization,
/// appended to as functions are minted rather than rebuilt.
#[derive(Default)]
pub(super) struct DescriptorCache {
    refs: Vec<FunctionRef>,
    /// How much of `NirPackage::renamed` has been refreshed.
    renames_seen: usize,
    /// Where `assert_one_fresh` resumes its rotation.
    #[cfg(debug_assertions)]
    cursor: usize,
}

impl DescriptorCache {
    pub(super) fn descriptors(&mut self, project: &NirPackage) -> &[FunctionRef] {
        debug_assert!(
            self.refs.len() <= project.functions.len(),
            "descriptor cache outlived a function removal"
        );
        for &id in &project.renamed[self.renames_seen..] {
            if let Some(slot) = self.refs.get_mut(id.index()) {
                let func = project.functions[id.index()].borrow();
                *slot = FunctionRef::from_resolved(&func, func.module_source.clone());
            }
        }
        self.renames_seen = project.renamed.len();
        self.refs
            .extend(project.callee_descriptors_from(self.refs.len()));
        #[cfg(debug_assertions)]
        self.assert_one_fresh(project);
        &self.refs
    }

    /// A descriptor is keyed by store position, so a rename in place goes
    /// stale unless `NirPackage::renamed` logged it. One entry per
    /// read, rotating: a whole-table check costs its caller O(n²), since a
    /// pass may read once per function.
    #[cfg(debug_assertions)]
    fn assert_one_fresh(&mut self, project: &NirPackage) {
        if self.refs.is_empty() {
            return;
        }
        self.cursor %= self.refs.len();
        let descriptor = &self.refs[self.cursor];
        let f = project.functions[self.cursor].borrow();
        assert!(
            descriptor.name == f.name
                && descriptor.method_info.as_ref().map(|i| &i.method_name)
                    == f.method_info.as_ref().map(|i| &i.method_name),
            "a cached descriptor went stale: a function was renamed in place"
        );
        self.cursor += 1;
    }
}

/// Resolve a call node's stamped `func_id` to its callee descriptor. `func_id`
/// is total for every NIR call (born resolved): the field is a non-optional
/// [`FuncId`].
pub(super) fn callee_descriptor(descriptors: &[FunctionRef], func_id: FuncId) -> &FunctionRef {
    &descriptors[func_id.index()]
}

/// Function reachability via call-graph BFS, shared by DCE and argument analysis.
///
/// Consumes the call graph (and its pending inspect edges) built in the single
/// AST walk of [`build_analysis_graph`]; mutates the graph by adding the gated
/// per-functor `$Closure_N^Inspect` edges once the inspectable-signature set
/// is known.
fn compute_function_reachability(
    project: &mut NirPackage,
    graph: &mut AnalysisGraph,
) -> IndexSet<usize> {
    // Phase 2a: compute the provisional reachable set from the raw graph
    // (without per-functor `$Closure_N^Inspect` edges). This is what
    // determines whether a `:?` / `:#?` call site is actually live.
    let reachable_v1 = compute_reachable_from_entries(project, &graph.call_graph);

    // Phase 2b: derive the inspectable `(arity, ret)` set from the reachable
    // functions only, then add the gated inspect edges to the call graph. The
    // per-functor impls themselves don't issue any `Fn^Inspect` calls (they
    // just write per-literal strings), so the inspectable set is stable under
    // this expansion — no fixpoint iteration is needed.
    let inspectable = inspectable_signatures(graph, &reachable_v1);
    apply_inspect_edges(&mut graph.call_graph, &graph.pending_inspects, &inspectable);

    // Phase 2c: re-compute the reachable set from the augmented graph.
    let mut reachable = compute_reachable_from_entries(project, &graph.call_graph);

    // Phase 3: extend reachable set with optimizer-induced virtual edges.
    // A pass may *synthesize* calls during the optimization loop. Their targets
    // must survive the DCE that runs before it, or the rewrite cannot fire.
    extend_reachable_for_optimizer_passes(project, graph, &mut reachable);

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

/// Add functions the NIR optimizer's rewrites reach without a call edge:
/// `nir/string_push`'s append primitives and `array_clone::<T>`'s helper.
fn extend_reachable_for_optimizer_passes(
    project: &NirPackage,
    graph: &AnalysisGraph,
    reachable: &mut IndexSet<FunctionId>,
) {
    let call_graph = &graph.call_graph;
    use crate::compiler_item::CompilerItem;

    // `push_str` is generic over `AsStrSlice`, so the item marks one id per
    // monomorph; any of them reaching the fusion keeps the primitives alive.
    let mut append_ids: Vec<FunctionId> = Vec::new();
    let mut fused: Vec<FunctionId> = Vec::new();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        match func.compiler_item {
            Some(CompilerItem::StringPushStr | CompilerItem::StringPushChar) => {
                append_ids.push(function_id_for(&func));
            }
            Some(
                CompilerItem::StringLen
                | CompilerItem::StringReserveUninit
                | CompilerItem::StringSetByteUnchecked
                | CompilerItem::StringWriteStrAt,
            ) => {
                fused.push(function_id_for(&func));
            }
            _ => {}
        }
    }
    // `nir/string_push`'s append fusion writes a run of appends in terms of the
    // four `String` primitives above, so they must survive the pre-loop DCE
    // wherever an append is reachable at all. Ungated beyond that: the fusion
    // reads a run out of a block rather than a single recognisable call, and a
    // later DCE drops the four again when no run fused.
    //
    // `push_ascii_unchecked` needs no root: both rules only recognise it, and
    // `Ctx::resolve` reads the compiler item off an entry this pass leaves in
    // place.
    if append_ids.iter().any(|id| reachable.contains(id)) {
        reachable.extend(compute_reachable(call_graph, fused));
    }

    // An `array_clone::<T>` site reaches its helper through the element type
    // rather than a call edge, so seed one root per value-typed site.
    //
    // Iterate to a fixpoint: a helper newly marked reachable may itself
    // call `array_clone::<T'>` for some `T'` whose helper isn't reachable
    // yet, and `compute_reachable` only follows direct call-graph edges
    // (it doesn't replay the array_clone scan). Single-pass would drop
    // inner helpers for chains like `List<List<List<T>>>`, and WIR build
    // would find no function for the helper its clone loop calls. Only a
    // reachable body seeds anything, so each is scanned once, when reached.
    let type_table = project.type_table.borrow();
    let ids = &graph.func_positions;
    let mut scanned = vec![false; ids.len()];
    loop {
        let mut fresh: Vec<FunctionId> = Vec::new();
        for (index, (id, _)) in ids.iter().enumerate() {
            if scanned[index] || !reachable.contains(id) {
                continue;
            }
            scanned[index] = true;
            for &type_id in &graph.analyses[index].array_clone_elems {
                // A stale `array_clone::<T>` can name a type already
                // pruned from the table; it has no helper, so skip it
                // rather than resolve an absent id (the structural key
                // recurses through `TypeTable::get`, which panics on a
                // missing slot). The top-level id suffices: `retain`
                // keeps the closure over exactly those edges.
                if type_table.get_pruned(type_id).is_none() {
                    continue;
                }
                if let Some(helper) = project.value_copy_helpers.get(type_id, &type_table) {
                    let (helper_id, _) = ids
                        .get_index(helper.index())
                        .expect("a helper is in the store");
                    if !reachable.contains(helper_id) {
                        fresh.push(helper_id.clone());
                    }
                }
            }
        }
        if fresh.is_empty() {
            break;
        }
        reachable.extend(compute_reachable(call_graph, fresh));
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
    let roots = project.functions.iter().filter_map(|func_rc| {
        let func = func_rc.borrow();
        let is_root = func.is_cm_export
            || (func.is_export
                && project
                    .wasm_module_sources
                    .contains_key(&func.module_source));
        is_root.then(|| FunctionId::free(&func.module_source, &func.name))
    });
    compute_reachable(call_graph, roots)
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
                used_wasi_functions.insert(operation_key(interface_name, op_name));
            }
        }
    }

    let reaches_intrinsic = |prefix: &str| {
        reachable.iter().any(|func_id| {
            matches!(func_id, FunctionId::Free(f)
                if f.module_source.is_core_builtin() && f.name.starts_with(prefix))
        })
    };

    // A sink-less world (`--lib`, kiln) leaves the ambient builtin unimported,
    // and `calls.rs` lowers it to `unreachable` off the `func_map` this fills.
    for (interface, intrinsic) in [
        ("Stdout", "call_indirect_stdout"),
        ("Stderr", "call_indirect_stderr"),
    ] {
        if project.provides_ambient_stdio_sink(interface) && reaches_intrinsic(intrinsic) {
            used_wasi_functions.insert(operation_key(interface, "write_via_stream"));
        }
    }

    // Collect imports using registry lookup instead of hard-coded match
    let mut imports: IndexSet<NirImport> = IndexSet::default();

    let add_import = |imports: &mut IndexSet<NirImport>, source: &ModuleSource, name: &str| {
        if let Some(info) = project.builtin_registry.get(source, name)
            && let Some(canonical_name) = &info.canonical_name
        {
            imports.insert(NirImport {
                module_source: source.clone(),
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
            && f.module_source.is_builtin()
        {
            add_import(&mut imports, &f.module_source, &f.name);
        }
    }

    // realloc is always needed for memory management
    add_import(&mut imports, &ModuleSource::builtin(), "realloc");

    // No `task-return` here: WIR translation types each delivery's canon from
    // the flat args at its own call site.

    // Store imports in the project
    project.imports = imports.into_iter().collect();
    // Sort imports for deterministic output
    project
        .imports
        .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    project.reserve_asset_memory();

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

/// Retain only the bytes literals surviving functions reference. Unlike string
/// literals there is no per-function map, so this scans every surviving body for
/// `ExprKind::PackedArray` nodes. String `repr`s are `PackedArray` too, making
/// the scanned set a superset — harmless, since a shared payload dedups into the
/// same segment anyway.
pub fn filter_bytes_literals(project: &mut NirPackage) {
    let mut used_bytes: IndexSet<Vec<u8>> = IndexSet::default();

    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = func.body.as_ref() {
            collect_bytes_literals_block(body, body.root(), &mut used_bytes);
        }
    }

    project.bytes_literals.retain(|b| used_bytes.contains(b));
}

fn collect_bytes_literals_block(body: &Body, root: BlockId, used: &mut IndexSet<Vec<u8>>) {
    // Collect every `PackedArray` payload reachable from `root` (the `repr` of
    // any string / bytes literal), excluding patterns (the tree walk never
    // descended into `LetDestructure` / match-arm patterns, so a payload inside
    // a `ConstantValue` pattern is not counted).
    body.walk_nodes_under::<()>(NodeRef::Block(root), |node| {
        if matches!(node, NodeRef::Pat(_)) {
            return ControlFlow::Continue(false);
        }
        if let NodeRef::Expr(e) = node
            && let ExprKind::PackedArray(data) = &body.exprs[e].kind
        {
            used.insert(data.bytes.clone());
        }
        ControlFlow::Continue(true)
    });
}
/// Remove closure functors whose `$call` method was eliminated by function DCE.
pub fn remove_unreachable_closure_functors(project: &mut NirPackage) {
    // A dead `$call` lingers in `functions` (Phase 4 marks, never removes).
    project
        .closure_functors
        .retain(|functor| !functor.call_method.borrow().is_dead);
}

/// Per-caller pending inspect edges, keyed by the caller's `FunctionId`.
/// Each entry collects every `$Closure_N` observed in that caller's body
/// alongside its `(arity, return_type)` signature. After the
/// inspectable-signature set is computed, `apply_inspect_edges` walks this
/// map and adds the matching `inspect` edges to the call graph.
type PendingInspectsByCaller = IndexMap<FunctionId, Vec<PendingInspectEdge>>;

/// `FunctionId` → position in `project.functions`. `function_id_for` is
/// required to be injective over `project.functions`; `build_analysis_graph`
/// asserts this so a regression in the synthesis layer (e.g. duplicate
/// emission of a per-signature dispatch stub) trips immediately instead of
/// silently dropping a function during DCE retain.
type FuncPositions = IndexMap<FunctionId, usize>;

/// Result of the single call-graph build. The call graph is the raw
/// reachability graph *without* `$Closure_N^Inspect` edges; those
/// edges are gated by the inspectable-signature set and added after the
/// fact by `apply_inspect_edges`.
///
/// `func_pos` maps each NIR-function `FunctionId` back to its position in
/// `project.functions` so that `remove_unreachable_functions` can keep
/// surviving functions by index instead of rebuilding `FunctionId`s and
/// hashing them again.
struct AnalysisGraph<'a> {
    call_graph: CallGraph,
    effect_usage: EffectUsageMap,
    pending_inspects: PendingInspectsByCaller,
    func_positions: FuncPositions,
    /// Each function's facts, indexed by position in `project.functions`:
    /// [`compute_global_reachability`] and [`populate_type_reachability`]
    /// aggregate the reachable ones.
    analyses: Cow<'a, [FunctionAnalysis]>,
    functors: FunctorMethods,
}

/// Build the call graph **and** per-function used-globals / used-types
/// sets in a single AST walk per function body. The downstream
/// reachability passes (`analyze_global_reachability`,
/// `populate_type_reachability`) then union per-function facts for the
/// reachable subset instead of re-walking bodies — three independent
/// walks collapsed into one.
fn build_analysis_graph(
    project: &NirPackage,
    descriptors: &[FunctionRef],
    exec: &Executor,
) -> AnalysisGraph<'static> {
    let type_table = &*project.type_table.borrow();
    let functors = functor_methods(project);
    let analyses = exec.map(&project.functions, |f| {
        function_analysis(&f.borrow(), type_table, descriptors, &functors)
    });
    assemble_analysis_graph(project, Cow::Owned(analyses), functors)
}

/// One function's facts for the [`AnalysisGraph`], from its signature and body.
fn function_analysis(
    func: &NirFunction,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
    functors: &FunctorMethods,
) -> FunctionAnalysis {
    let mut walker = DceWalker::new(type_table, &func.module_source, descriptors, functors);
    walker.analyze(func);
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
    analysis
}

/// The [`DceWalker`] results [`reachable_function_positions`] reads, kept
/// across the fixed-point loop.
#[derive(Default)]
pub(super) struct ReachabilityCache {
    walks: BodyMemo<FunctionAnalysis>,
    renames_seen: usize,
}

impl ReachabilityCache {
    /// Every function's [`function_analysis`] as `project` stands.
    fn analyses(
        &mut self,
        project: &NirPackage,
        exec: &Executor,
        descriptors: &[FunctionRef],
        functors: &FunctorMethods,
    ) -> &[FunctionAnalysis] {
        let renamed: IndexSet<FuncId> = project.renamed[self.renames_seen..]
            .iter()
            .copied()
            .collect();
        self.renames_seen = project.renamed.len();
        if !renamed.is_empty() {
            self.walks
                .forget_where(|walk| !walk.named.is_disjoint(&renamed));
        }
        let type_table = &*project.type_table.borrow();
        self.walks.refresh(project, exec, |f| {
            function_analysis(f, type_table, descriptors, functors)
        })
    }
}

fn assemble_analysis_graph<'a>(
    project: &NirPackage,
    analyses: Cow<'a, [FunctionAnalysis]>,
    functors: FunctorMethods,
) -> AnalysisGraph<'a> {
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

    for ((pos, func_rc), analysis) in project.functions.iter().enumerate().zip(analyses.iter()) {
        let func = func_rc.borrow();
        let func_id = function_id_for(&func);

        if let Some(prior) = func_positions.insert(func_id.clone(), pos) {
            let prior = project.functions[prior].borrow();
            panic!(
                "function_id_for collision in project.functions: `{}` and `{}` \
                 map to the same FunctionId {func_id:?}. `function_id_for` \
                 must be injective; check the synthesis or monomorphize layer \
                 for duplicate emission.",
                prior.name, func.name,
            );
        }
        call_graph.insert(func_id.clone(), analysis.callees.clone());
        if !analysis.effect_calls.is_empty() {
            effect_usage.insert(func_id.clone(), analysis.effect_calls.clone());
        }
        if !analysis.pending_inspects.is_empty() {
            pending_inspects.insert(func_id, analysis.pending_inspects.clone());
        }
    }

    AnalysisGraph {
        call_graph,
        effect_usage,
        pending_inspects,
        func_positions,
        analyses,
        functors,
    }
}

/// Augment `call_graph` with the gated `$Closure_N^Inspect::inspect`
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
            if sigs.contains(&edge.key) {
                callees.insert(edge.inspect.clone());
            }
        }
    }
}

/// Every `(arity, return_type)` signature receiving a `fn(..)^Inspect` call.
/// Gates the per-functor root marking from `ClosureToCanonical`: with no real
/// caller those impls cannot be invoked indirectly.
type InspectableSignatures = IndexSet<(usize, TypeId)>;

/// The inspectable `(arity, return_type)` set of the *reachable* functions
/// only. Restricting it to live code keeps a dead `:?`/`:#?` call from forcing
/// per-functor inspect impls to stay alive for an unrelated reachable closure of
/// the same signature.
fn inspectable_signatures(
    graph: &AnalysisGraph,
    reachable: &IndexSet<FunctionId>,
) -> InspectableSignatures {
    reachable
        .iter()
        .filter_map(|id| graph.func_positions.get(id))
        .flat_map(|&pos| graph.analyses[pos].inspect_signatures.iter().copied())
        .collect()
}

/// Compute the `FunctionId` used by the call graph for a NIR function.
/// Mirrors the keying logic in `build_analysis_graph`; centralising
/// it here so other passes (notably the inspectable-signatures scan)
/// can compare against the call graph's reachable set.
fn function_id_for(func: &NirFunction) -> FunctionId {
    let module_source = &func.module_source;
    if let Some(ref info) = func.method_info {
        if func.monomorph_info.is_some() {
            // A monomorphized method keys as a free function: its mangled
            // name already carries the receiver, the trait and every type
            // argument, so nothing more identifies it.
            FunctionId::Free(FreeFunctionName::new(
                module_source.clone(),
                func.name.clone(),
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
    } else {
        FunctionId::free(module_source, &func.name)
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
    functors: &'a FunctorMethods,
    /// The `Inspect` trait, whose calls on a function value record a signature.
    inspect: Option<DefId>,
    analysis: FunctionAnalysis,
}

/// Each closure functor's `$call` and `^Inspect` impl, by `(module, functor id)`,
/// as their records name them: `dae` renames what it reshapes, so a name built
/// from the functor id would miss one.
type FunctorMethods = IndexMap<(ModuleSource, u32), (FunctorMethod, FunctorMethod)>;

/// One of a functor's methods: its name, and where the name lives.
struct FunctorMethod {
    name: FunctionId,
    id: FuncId,
}

impl FunctorMethod {
    fn of(func: &NirFunction) -> Self {
        Self {
            name: function_id_for(func),
            id: func.id.expect("func_id assigned at lower"),
        }
    }
}

fn functor_methods(project: &NirPackage) -> FunctorMethods {
    project
        .closure_functors
        .iter()
        .map(|f| {
            let methods = (
                FunctorMethod::of(&f.call_method.borrow()),
                FunctorMethod::of(&f.inspect_method.borrow()),
            );
            ((f.module_source.clone(), f.id), methods)
        })
        .collect()
}

impl<'a> DceWalker<'a> {
    fn new(
        type_table: &'a TypeTable,
        current_module: &'a ModuleSource,
        descriptors: &'a [FunctionRef],
        functors: &'a FunctorMethods,
    ) -> Self {
        Self {
            type_table,
            current_module,
            descriptors,
            functors,
            inspect: type_table.compiler_items().trait_def(CompilerItem::Inspect),
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
            self.walk_node(body, NodeRef::Block(body.root()));
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

    fn record_call(&mut self, func: &nir::FunctionRef) {
        let original_callee_module = func.module_source.clone();
        let func_name = func.name.clone();

        if let Some(call_info) = func.method_info.as_ref() {
            // Static method call (e.g. `Box::get`, `String^Display::fmt`):
            // `func_name` is `"Struct::method"` or `"Struct^Trait::method"`.
            let callee_id = if func.is_monomorphized() {
                FunctionId::Free(FreeFunctionName::new(
                    func.module_source.clone(),
                    func_name.clone(),
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
            let callee_id = FunctionId::free(&callee_module, &func_name);
            self.analysis.callees.insert(callee_id);

            if let Some(interface_name) = original_callee_module.interface_name() {
                self.analysis
                    .effect_calls
                    .insert((interface_name, func_name));
            }
        }
    }

    fn record_method_call(&mut self, receiver_type: TypeId, func: &nir::FunctionRef) {
        let func_name = func.name.clone();

        // Monomorphized methods (e.g. `List<i32>::len`) already have
        // their concrete name on `func`; non-monomorphized methods are
        // dispatched by `receiver`'s type below.
        if func.is_monomorphized() {
            // Use the func's actual module_source — monomorphized functions
            // are placed in the module that uses them.
            let callee_id =
                FunctionId::Free(FreeFunctionName::new(func.module_source.clone(), func_name));
            self.analysis.callees.insert(callee_id);
            return;
        }

        // Non-monomorphized method - determine target from receiver type.
        // Strip any reference wrappers and newtypes to get the base type.
        let mut current_type = self.type_table.get(receiver_type);
        let mut newtype_info: Option<DefId> = None;
        loop {
            match current_type {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    current_type = self.type_table.get(*inner);
                }
                ResolvedType::Newtype { def, base_type, .. } => {
                    // Remember the outermost newtype for its own trait impls
                    if newtype_info.is_none() {
                        newtype_info = Some(*def);
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
        if let Some(newtype) = newtype_info {
            let method_id = FunctionId::Method(MethodName::new(
                self.type_table.def_module(newtype).clone(),
                FqTypeName::declared(self.type_table.defs(), newtype),
                trait_name.clone(),
                method_name.clone(),
            ));
            self.analysis.callees.insert(method_id);
        }

        match base_receiver_type {
            ResolvedType::Struct {
                ref def,
                ref type_args,
            } if !type_args.is_empty() => {
                let module_source = self.type_table.struct_head_module(*def);
                let name = &self.type_table.struct_rendered_name(*def, type_args);
                // Monomorphized struct method (e.g. `Box<i32>::get`):
                // monomorphized functions live in the *using* module, so
                // route the callee id through `current_module`.
                let mangled_func_name = MethodName::format_local(
                    &FqTypeName::shape(module_source, name),
                    trait_name.as_ref(),
                    &method_name,
                );
                let callee_id = FunctionId::Free(FreeFunctionName::new(
                    self.current_module.clone(),
                    mangled_func_name,
                ));
                self.analysis.callees.insert(callee_id);

                // For internal Box<T> types (primitive boxing), the method is
                // actually defined on the inner type (e.g., i32^Ord::cmp, not
                // Box<i32>^Ord::cmp). Also mark the FunctionRef's original
                // method target as reachable.
                let boxed = self.type_table.is_compiler_struct(*def, CompilerItem::Box);
                if boxed && let Some(info) = func.method_info.clone() {
                    let original_method_id = FunctionId::Method(MethodName::new(
                        func.module_source.clone(),
                        info.fq_struct_name(),
                        info.trait_name.clone(),
                        info.method_name,
                    ));
                    self.analysis.callees.insert(original_method_id);
                }
            }
            ResolvedType::Struct { def, .. } => {
                let module_source = self.type_table.struct_head_module(def).clone();
                // Non-monomorphized struct method.
                let method_id = FunctionId::Method(MethodName::new(
                    module_source.clone(),
                    self.type_table.fq_struct_head(def),
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
                    ModuleSource::of_primitive(prim),
                    FqTypeName::builtin(prim.as_str()),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::Unit => {
                let method_id = FunctionId::Method(MethodName::new(
                    ModuleSource::primitive(),
                    FqTypeName::builtin(UNIT_TYPE_NAME),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::GenericInstance { def, type_args }
                if self.type_table.is_tuple_def(def) =>
            {
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
            ResolvedType::GenericInstance { def, type_args } => {
                let name = self.type_table.def_name(def).to_string();
                // Generic instance method (e.g. `Box<i32>::get`,
                // `TreeMap<String,i32>^Index::index`). Trait methods
                // need the trait name baked into the mangle so trait-
                // and inherent-name collisions stay separate.
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_name(*t))
                    .collect();
                let mangled_func_name = if let Some(ref trait_n) = trait_name {
                    let generic_name = mangle_generic_name(&name, &type_arg_names);
                    mangle_local_trait_method(&generic_name, &trait_n.to_mangled(), &method_name)
                } else {
                    mangle_method_generic(&name, &type_arg_names, &method_name)
                };
                let callee_id = FunctionId::Free(FreeFunctionName::new(
                    self.current_module.clone(),
                    mangled_func_name,
                ));
                self.analysis.callees.insert(callee_id);
            }
            ResolvedType::Enum { def } => {
                let module_source = self.type_table.def_module(def).clone();
                // Enum method (user-defined or auto-derived trait impl).
                let method_id = FunctionId::Method(MethodName::new(
                    module_source,
                    FqTypeName::declared(self.type_table.defs(), def),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::Resource { def } => {
                let name = self.type_table.def_name(def).to_string();
                // Resource instance method (e.g. `fields.has()`):
                // recorded as an effect so it lands in
                // `used_wasi_functions`.
                self.analysis.effect_calls.insert((name, method_name));
            }
            ResolvedType::Variant { def } => {
                let module_source = self.type_table.def_module(def).clone();
                // Variant method, e.g. `Shape^Inspect::inspect`.
                let method_id = FunctionId::Method(MethodName::new(
                    module_source,
                    FqTypeName::declared(self.type_table.defs(), def),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::Function { .. } => {
                // A `fn(..)` method hangs off the type's own name, the same
                // spelling `synthesis::traits` registers its stub under.
                let method_id = FunctionId::Method(MethodName::new(
                    self.current_module.clone(),
                    self.type_table.fn_receiver_name(&base_receiver_type),
                    trait_name,
                    method_name,
                ));
                self.analysis.callees.insert(method_id);
            }
            ResolvedType::GenericResource { def, type_args } => {
                let name = self.type_table.def_name(def).to_string();
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

    fn record_cm_raw_call(&mut self, target: &CmCallTarget) {
        // Only a WASI import is tracked here; a canonical built-in has no
        // interface to attribute it to.
        let CmCallTarget::WasiAlias(local_name) = target else {
            return;
        };
        // Parse the alias (e.g., "wasi:cli/Stdout::write_via_stream")
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
        // `$call` is always live: the canonical closure struct holds
        // a `ref.func` to it directly.
        let (call, inspect) = self
            .functors
            .get(&(closure_module.clone(), functor_id))
            .expect(
                "a functor whose closure a live body converts reaches its `$call`, so DCE keeps it",
            );
        self.analysis.callees.insert(call.name.clone());
        self.analysis.named.extend([call.id, inspect.id]);

        // A per-functor `$Closure_N^Inspect` impl only needs to stay alive
        // when its matching `fn(..)^Inspect` dispatch stub is reachable, so a
        // program that never prints a closure of that shape keeps neither it
        // nor its per-literal source-string constant. The gating set is derived
        // from the first reachable-set computation, so record a pending edge
        // for `apply_inspect_edges` to resolve later.
        if let ResolvedType::Function {
            params,
            return_type,
            ..
        } = self.type_table.get(target_fn_type)
        {
            self.analysis.pending_inspects.push(PendingInspectEdge {
                inspect: inspect.name.clone(),
                key: (params.len(), *return_type),
            });
        }
    }
}

impl DceWalker<'_> {
    /// An `Inspect` call on a function value: record its `(arity, return
    /// type)`. The receiver is `&Fn(...)`, possibly boxed by the boxing pass.
    fn record_inspect_signature(&mut self, recv_ty: TypeId, callee: &FunctionRef) {
        let Some(info) = &callee.method_info else {
            return;
        };
        if self.inspect.is_none()
            || info.trait_decl() != self.inspect
            || !is_fn_type_name(&info.base_struct_name())
        {
            return;
        }
        if let ResolvedType::Function {
            params,
            return_type,
            ..
        } = self
            .type_table
            .get(self.type_table.peel_refs_and_box(recv_ty))
        {
            self.analysis
                .inspect_signatures
                .insert((params.len(), *return_type));
        }
    }

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
                    ExprKind::Call {
                        func_id, type_args, ..
                    } => {
                        self.analysis.named.insert(*func_id);
                        let callee = callee_descriptor(self.descriptors, *func_id);
                        // `array_clone` reaches its helper by the element type
                        // the call node carries, not by a call edge.
                        if matches!(
                            callee.intrinsic(),
                            Some("array_clone" | "array_clone_prefix")
                        ) && let Some(&elem) = type_args.first()
                        {
                            self.analysis.array_clone_elems.insert(elem);
                        }
                        match body.exprs[e].kind.as_method_call() {
                            Some((receiver, _, _)) => {
                                let recv_ty = body.operand_type(receiver);
                                self.record_inspect_signature(recv_ty, callee);
                                self.record_method_call(recv_ty, callee);
                            }
                            None => self.record_call(callee),
                        }
                    }
                    ExprKind::CmRawCall { target, .. } => self.record_cm_raw_call(target),
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
                ModuleSource::of_primitive(*prim),
                FqTypeName::builtin(prim.as_str()),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Unit => {
            let method_id = FunctionId::Method(MethodName::new(
                ModuleSource::primitive(),
                FqTypeName::builtin(UNIT_TYPE_NAME),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        _ => {}
    }
}

/// Worklist BFS over `call_graph` from all of `entries` at once. A separate
/// walk per root would re-visit whatever the roots share.
fn compute_reachable(
    call_graph: &IndexMap<FunctionId, IndexSet<FunctionId>>,
    entries: impl IntoIterator<Item = FunctionId>,
) -> IndexSet<FunctionId> {
    let mut reachable = IndexSet::default();
    let mut worklist: Vec<FunctionId> = entries.into_iter().collect();

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
                ResolvedType::Struct { def, type_args } => {
                    let decl_name = type_table.struct_head_name(*def);
                    let module_source = type_table.struct_head_module(*def);
                    if type_args.is_empty() {
                        self.struct_exact.insert((decl_name, module_source.clone()));
                    } else {
                        self.struct_monomorph_names
                            .insert(type_table.struct_rendered_name(*def, type_args));
                        self.struct_monomorph_bases.insert(decl_name);
                    }
                }
                ResolvedType::Variant { def } => {
                    self.variant_exact.insert((
                        type_table.def_name(*def).to_string(),
                        type_table.def_module(*def).clone(),
                    ));
                }
                ResolvedType::Enum { def } => {
                    self.enum_exact.insert((
                        type_table.def_name(*def).to_string(),
                        type_table.def_module(*def).clone(),
                    ));
                }
                ResolvedType::GenericInstance { def, type_args } => {
                    self.generic_instance_names
                        .insert(type_table.def_name(*def).to_string());
                    // The spelling a live `Struct` with type args records, so
                    // `keeps_struct` recognises the monomorph either way: a dead
                    // local still declares its type, and `wir_build` declares a
                    // Wasm local for it.
                    self.struct_monomorph_names
                        .insert(type_table.struct_rendered_name(StructDef::Decl(*def), type_args));
                }
                _ => {}
            }
        }
    }
}

/// Variant declarations that outlive their uses, both for
/// `optimize::sroa_variant_return`: `Option`, whose slots the pass mints after
/// the early DCE, and any variant a function was scalarized *from* — scalarizing
/// every use away is what makes a declaration look unreachable, and the pass
/// re-derives its layout to recognise its own earlier work. A kept declaration
/// keeps its payload types too, or `register_mono_variants` panics.
fn variant_decls_kept_past_use(
    project: &NirPackage,
    type_table: &TypeTable,
) -> hashmap::IndexSet<(String, ModuleSource)> {
    let mut kept: hashmap::IndexSet<(String, ModuleSource)> = project
        .functions
        .iter()
        .filter_map(|f| f.borrow().scalarized_from)
        .filter_map(|t| match type_table.get(t) {
            ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => {
                type_table.nominal_head(t)
            }
            _ => None,
        })
        .collect();
    if let Some(ms) = type_table
        .compiler_items()
        .variant_module(CompilerItem::Option)
    {
        kept.insert(("Option".to_string(), ms.clone()));
    }
    kept
}

/// Populate `analysis.types` and the name-keyed type-index views. A type is
/// reachable from any reachable function's signature, locals or expressions, or
/// any reachable global's initializer, closed transitively over struct fields
/// and variant payloads. Reads `analysis.functions` and `analysis.globals`, so
/// both must be populated first.
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
        let functors = &graph.functors;

        // Sum per-function used-types for reachable functions only.
        for &pos in &analysis.functions {
            if let Some(per_func) = graph.analyses.get(pos) {
                for &id in &per_func.used_types {
                    analysis.types.insert(id);
                }
            }
        }

        // Reachable globals' declared type + initializer types. At NIR
        // level non-constant initializers have already been extracted
        // into `$initialize_module` (see `lower::plan::globals`), so
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
            let mut walker =
                DceWalker::new(&type_table, &global.module_source, descriptors, functors);
            let init_body = global.init.slot_expr().body();
            walker.walk_node(init_body, NodeRef::Block(init_body.root()));
            for id in walker.analysis.used_types {
                analysis.types.insert(id);
            }
        }

        // A reachable `$call` keeps its functor's struct / ref types live:
        // `register_closure_wrappers` reads `ref_type_id` for the wrapper's
        // `ref.cast`, and DAE can drop every other NIR-side mention by removing
        // the env `self`. Compare by pointer identity — `functor.call_method`
        // and the matching `project.functions[i]` are the same `Arc`.
        let surviving_ptrs: IndexSet<*const _> = project
            .functions
            .iter()
            .enumerate()
            .filter(|(pos, _)| analysis.functions.contains(pos))
            .map(|(_, rc)| Arc::as_ptr(rc))
            .collect();
        for functor in &project.closure_functors {
            let cm_ptr = Arc::as_ptr(&functor.call_method);
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
        | ResolvedType::MutRef(inner) => {
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
        ResolvedType::AssocTypeProjection {
            param_id,
            args,
            trait_args,
            ..
        } => {
            collect_type_transitive(*param_id, type_table, reachable);
            for arg in projection_arguments(args, trait_args) {
                collect_type_transitive(arg, type_table, reachable);
            }
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
        | ResolvedType::AssocParam { .. }
        | ResolvedType::TypePack { .. } => {}
        ResolvedType::InferVar(var) => panic!("{var} reached DCE"),

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
    impl NirRefVisitor for Collect {
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
    NirRefVisitor::visit_node(&mut collect, body, NodeRef::Block(body.root()));
    collect.0
}

/// Locals some reachable node mentions, skeleton and value pool alike. A
/// binding absent here is never read: an assignment target, a borrow and a
/// capture all mention their local, so the census over-approximates and only
/// ever keeps a statement alive.
fn mentioned_locals(body: &Body) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    for e in reachable_exprs(body) {
        if let ExprKind::Local { index, .. } = &body.exprs[e].kind {
            out.insert(*index);
        }
    }
    promoted_local_reads(body, &mut out);
    out
}

/// The `GlobalVarGet`s in `expr`'s subtree, and the ids that read them.
fn global_reads_in(body: &Body, expr: ExprId) -> Vec<(ExprId, (String, String))> {
    struct Collect(Vec<(ExprId, (String, String))>);
    impl NirRefVisitor for Collect {
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
    NirRefVisitor::visit_node(&mut collect, body, NodeRef::Expr(expr));
    collect.0
}

/// Whether the pass may delete `value` outright — no observable effect, and no
/// trap, since a trap is observable too.
///
/// The expression predicate answers first, with its typed refinement, for a
/// literal aggregate. Failing that the tree is walked: it refuses every call on
/// sight, while the initializer globalization hoists for a reflect member walk
/// *is* a call, so each one is answered by `calls`: its callee's summary, less a
/// trap a proof for that call rules out.
pub(super) fn deletable_value(
    body: &Body,
    value: Operand,
    types: &TypeTable,
    calls: CallFacts,
) -> bool {
    if is_pure_nontrapping_operand_typed(body, value, Some(types)) {
        return true;
    }
    let Some(root) = value.as_expr() else {
        return false;
    };
    body.find_in_live_node_under(NodeRef::Expr(root), |node| match node {
        _ if operand_values_may_trap(body, node) => Some(()),
        NodeRef::Expr(id) => match &body.exprs[id].kind {
            // A hint goes with the code containing it, but `cold_path();` on
            // its own is what it marks.
            ExprKind::Call { func_id, .. } => {
                let effect = calls.call(id, *func_id);
                let deletable = if id == root {
                    effect.is_deletable()
                } else {
                    effect.is_unobservable()
                };
                (!deletable).then_some(())
            }
            ExprKind::GlobalVarSet { .. }
            | ExprKind::Assign { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::CmRawCall { .. } => Some(()),
            _ => expr_node_may_trap(body, id).then_some(()),
        },
        // A block statement that is not a binding or a discarded value
        // leaves the region, and deleting it would take the exit with it.
        NodeRef::Stmt(s) => {
            (!matches!(body.stmts[s].kind, StmtKind::Let { .. } | StmtKind::Expr(_))).then_some(())
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => None,
    })
    .is_none()
}

fn lazy_guard_global(
    body: &Body,
    stmt: StmtId,
    descriptors: &[FunctionRef],
    types: &TypeTable,
    calls: CallFacts,
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
    if !callee.is_builtin_named("is_uninitialized") {
        return None;
    }
    let [arg] = &args[..] else {
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
    if !deletable_value(body, *value, types, calls) {
        return None;
    }
    Some((
        read,
        (module_source.to_path().join("::"), name.clone()),
        *value,
    ))
}

struct GlobalGuards<'a> {
    descriptors: &'a [FunctionRef],
    types: &'a TypeTable,
    inert_functions: IndexSet<FuncId>,
}

impl GlobalGuards<'_> {
    /// Whether `value` does nothing observable: a value this pass may delete,
    /// or a call to a function whose whole body is such work.
    fn inert_value(&self, body: &Body, value: Operand, calls: CallFacts) -> bool {
        if deletable_value(body, value, self.types, calls) {
            return true;
        }
        let Some(expr) = value.as_expr() else {
            return false;
        };
        let ExprKind::Call { func_id, args, .. } = &body.exprs[expr].kind else {
            return false;
        };
        (self.inert_functions.contains(func_id) || calls.call(expr, *func_id).is_unobservable())
            && args
                .iter()
                .all(|arg| deletable_value(body, arg.expr, self.types, calls))
    }

    /// Whether `stmt` binds a local nothing mentions to a value the pass may
    /// delete — a binding that computes something and drops it. A trap is an
    /// observable effect, so a trapping value keeps the binding alive even
    /// though nobody reads it.
    fn dead_binding(
        &self,
        body: &Body,
        stmt: StmtId,
        mentioned: &IndexSet<u32>,
        calls: CallFacts,
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
        let expr = value.as_expr()?;
        deletable_value(body, *value, self.types, calls).then_some(expr)
    }

    fn inert_body(&self, body: &Body, calls: CallFacts) -> bool {
        body.blocks[body.root()]
            .stmts
            .iter()
            .all(|s| match body.stmts[*s].kind {
                StmtKind::Expr(value) | StmtKind::Let { value, .. } => {
                    self.inert_value(body, value, calls)
                }
                StmtKind::Return { value: None } => true,
                StmtKind::Return { value: Some(value) } => self.inert_value(body, value, calls),
                _ => false,
            })
    }

    fn find(
        &self,
        body: &Body,
        stmt: StmtId,
        calls: CallFacts,
    ) -> Option<(ExprId, (String, String), Operand)> {
        lazy_guard_global(body, stmt, self.descriptors, self.types, calls)
            .or_else(|| self.once_guard(body, stmt, calls))
    }

    /// `L: { if flag { break L; } inert_work(); flag = value; }` observes
    /// nothing when no other read observes the flag. Calls must terminate too.
    fn once_guard(
        &self,
        body: &Body,
        stmt: StmtId,
        calls: CallFacts,
    ) -> Option<(ExprId, (String, String), Operand)> {
        let (label, block) = match &body.stmts[stmt].kind {
            StmtKind::LabeledBlock { label, block, .. } => (label, *block),
            StmtKind::Expr(Operand::Expr(e)) if body.exprs[*e].type_id == TypeTable::UNIT => {
                let ExprKind::LabeledBlock { label, block, .. } = &body.exprs[*e].kind else {
                    return None;
                };
                (label, *block)
            }
            _ => return None,
        };
        let (first, rest) = body.blocks[block].stmts.split_first()?;
        let StmtKind::If {
            condition,
            then_block,
            else_block: None,
        } = body.stmts[*first].kind
        else {
            return None;
        };
        let read = condition.as_expr()?;
        let ExprKind::GlobalVarGet {
            module_source,
            name,
        } = &body.exprs[read].kind
        else {
            return None;
        };
        let [exit] = body.blocks[then_block].stmts.as_slice() else {
            return None;
        };
        if !matches!(&body.stmts[*exit].kind,
            StmtKind::Break { label: Some(target), value: None } if target == label)
        {
            return None;
        }
        let (last, work) = rest.split_last()?;
        let StmtKind::Expr(Operand::Expr(set)) = body.stmts[*last].kind else {
            return None;
        };
        let ExprKind::GlobalVarSet {
            module_source: set_module,
            name: set_name,
            value,
        } = &body.exprs[set].kind
        else {
            return None;
        };
        if set_module != module_source || set_name != name || !self.inert_value(body, *value, calls)
        {
            return None;
        }
        if !work.iter().all(|s| match body.stmts[*s].kind {
            StmtKind::Expr(value) | StmtKind::Let { value, .. } => {
                self.inert_value(body, value, calls)
            }
            _ => false,
        }) {
            return None;
        }
        Some((
            read,
            (module_source.to_path().join("::"), name.clone()),
            *value,
        ))
    }
}

/// Un-hoist a constant globalization hoisted for nobody: the folds that run
/// after globalization can take every reader with them, leaving a global that
/// holds its whole initializer in the binary for no observer. The
/// `is_uninitialized` guard and a read bound to an unmentioned local do not
/// count as observing, provided the value is a `deletable_value`.
///
/// Answers whether any body changed, so a caller holding `summaries` knows
/// whether they still describe the IR.
pub(super) fn unhoist_unobserved_globals(
    project: &mut NirPackage,
    cache: &mut DescriptorCache,
    summaries: &FnSummaries,
) -> bool {
    let descriptors = cache.descriptors(project);
    let type_table = project.type_table.clone();
    let types = type_table.borrow();
    let mut guards = GlobalGuards {
        descriptors,
        types: &types,
        inert_functions: IndexSet::default(),
    };
    loop {
        let before = guards.inert_functions.len();
        for (i, func) in project.functions.iter().enumerate() {
            let func = func.borrow();
            let calls = summaries.of_body(i);
            if let (Some(id), Some(body)) = (func.id, &func.body)
                && !guards.inert_functions.contains(&id)
                && guards.inert_body(body, calls)
            {
                guards.inert_functions.insert(id);
            }
        }
        if guards.inert_functions.len() == before {
            break;
        }
    }
    let mut guarded: IndexSet<(String, String)> = IndexSet::default();
    let mut observed: IndexSet<(String, String)> = IndexSet::default();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        let calls = summaries.of_body(i);
        let Some(body) = func.body.as_ref() else {
            continue;
        };
        let mentioned = mentioned_locals(body);
        let mut unobserving: IndexSet<ExprId> = IndexSet::default();
        for stmt in reachable_stmt_ids(body) {
            if let Some((read, key, _)) = guards.find(body, stmt, calls) {
                guarded.insert(key);
                unobserving.insert(read);
            }
            if let Some(value) = guards.dead_binding(body, stmt, &mentioned, calls) {
                unobserving.extend(global_reads_in(body, value).into_iter().map(|(e, _)| e));
            }
        }
        for e in reachable_exprs(body) {
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
        return false;
    }
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let calls = summaries.of_body(i);
        if let Some(body) = func.body.as_mut() {
            let mentioned = mentioned_locals(body);
            for block in reachable_block_ids(body) {
                drop_unobserved_stmts(body, block, &unobserved, &mentioned, &guards, calls);
            }
            debug_assert!(
                !reads_any_global(body, &unobserved),
                "[NIR] unhoist_unobserved_globals: a global lost its store while a \
                 read of it survived"
            );
        }
    }
    true
}

/// Whether a reachable expression still reads one of `globals`. The pass drops
/// each one's guard and store, so a surviving read would see the uninitialized
/// slot.
fn reads_any_global(body: &Body, globals: &IndexSet<(String, String)>) -> bool {
    reachable_exprs(body)
        .into_iter()
        .any(|e| match &body.exprs[e].kind {
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => globals.contains(&(module_source.to_path().join("::"), name.clone())),
            _ => false,
        })
}

/// Every block reachable from the body root. The drop below must see the same
/// statements the census above classified — an expression-position block among
/// them, which is where inlining leaves a guard — or a read it counted as
/// non-observing outlives the store it was counted against.
fn reachable_block_ids(body: &Body) -> Vec<BlockId> {
    struct Collect(Vec<BlockId>);
    impl NirRefVisitor for Collect {
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
    NirRefVisitor::visit_node(&mut collect, body, NodeRef::Block(body.root()));
    collect.0
}

fn drop_unobserved_stmts(
    body: &mut Body,
    block: BlockId,
    unobserved: &IndexSet<(String, String)>,
    mentioned: &IndexSet<u32>,
    guards: &GlobalGuards,
    calls: CallFacts,
) {
    let old = std::mem::take(&mut body.blocks[block].stmts);
    let mut kept: Vec<StmtId> = Vec::with_capacity(old.len());
    for s in old {
        let is_guard = guards
            .find(body, s, calls)
            .is_some_and(|(_, key, _)| unobserved.contains(&key));
        let is_dead_read = guards
            .dead_binding(body, s, mentioned, calls)
            .is_some_and(|value| {
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
        if let Some(per_func) = graph.analyses.get(pos) {
            for entry in &per_func.used_globals {
                used_globals.insert(entry.clone());
            }
        }
    }
    used_globals
}

/// Retain only globals whose `(module_key, name)` is in
/// `used_globals` (computed by [`analyze_dce`]), then strip every
/// `GlobalVarSet` for a dead global from surviving function bodies
/// (covers both the original `$initialize_module` and any inlined
/// copies).
///
/// Whether a global's initializer runs is unspecified, except that it has run
/// before the global is read. So under `Initializers::DropUnread`, which only
/// the DCE ahead of every rewrite asks for, a dead global's initializer goes
/// whole, effects and traps included, and takes with it the functions and
/// imports only it reached. Later, a global is dead because a rewrite removed
/// the program's reads, and its initializer keeps its effect.
pub(super) fn remove_unreachable_globals(
    project: &mut NirPackage,
    used_globals: &IndexSet<(String, String)>,
    summaries: &FnSummaries,
    initializers: Initializers,
) {
    project.globals.retain(|global| {
        let global_module_key = global.module_source.to_path().join("::");
        used_globals.contains(&(global_module_key, global.name.clone()))
    });

    let type_table = project.type_table.borrow();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let calls = summaries.of_body(i);
        let is_module_init = func.name == MODULE_INIT_FUNCTION;
        if let Some(body) = func.body.as_mut() {
            if is_module_init && initializers == Initializers::DropUnread {
                drop_dead_initializers(body, used_globals);
            }
            let root = body.root();
            remove_dead_global_sets(body, NodeRef::Block(root), used_globals, &type_table, calls);
        }
    }
}

/// What becomes of a dead global's initializer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Initializers {
    /// No rewrite has run, so a dead global is one the program never reads,
    /// and its initializer goes whole.
    DropUnread,
    /// A rewrite may have removed a read the program performs, so the
    /// initializer keeps its effect.
    KeepEffects,
}

/// Drop each top-level store of `$initialize_module` to a dead global: before
/// inlining copies it elsewhere, such a store is an initializer, and the
/// program's assignments are elsewhere.
fn drop_dead_initializers(body: &mut Body, used: &IndexSet<(String, String)>) {
    let root = body.root();
    let stmts = std::mem::take(&mut body.blocks[root].stmts);
    body.blocks[root].stmts = stmts
        .into_iter()
        .filter(|&s| match body.stmts[s].kind {
            StmtKind::Expr(Operand::Expr(store)) => dead_store_value(body, store, used).is_none(),
            _ => true,
        })
        .collect();
}

/// Strip every store to a dead global under `node`, keeping a value that is not
/// [`deletable_value`] evaluated where the store was.
fn remove_dead_global_sets(
    body: &mut Body,
    node: NodeRef,
    used: &IndexSet<(String, String)>,
    type_table: &TypeTable,
    calls: CallFacts,
) {
    if let NodeRef::Block(block) = node {
        let old = std::mem::take(&mut body.blocks[block].stmts);
        let mut kept: Vec<StmtId> = Vec::with_capacity(old.len());
        for s in old {
            let StmtKind::Expr(Operand::Expr(store)) = body.stmts[s].kind else {
                kept.push(s);
                continue;
            };
            let Some(value) = dead_store_value(body, store, used) else {
                kept.push(s);
                continue;
            };
            if let Some(effect) = kept_effect(body, value, type_table, calls) {
                body.stmts[s].kind = StmtKind::Expr(effect.into());
                kept.push(s);
            }
        }
        body.blocks[block].stmts = kept;
    }

    let mut stores: Vec<ExprId> = Vec::new();
    body.for_each_operand(node, |op| {
        if let Some(e) = op.as_expr()
            && dead_store_value(body, e, used).is_some()
        {
            stores.push(e);
        }
    });
    for store in stores {
        let value = dead_store_value(body, store, used).expect("collected as a dead store");
        let unit = body.exprs[store].type_id;
        if let Some(effect) = kept_effect(body, value, type_table, calls) {
            let span = body.exprs[store].span;
            let stmt = body.stmts.push(StmtNode {
                kind: StmtKind::Expr(effect.into()),
                span,
            });
            let block = body.blocks.push(BlockNode {
                stmts: vec![stmt],
                span,
            });
            body.exprs[store].kind = ExprKind::plain_block(block, unit, "dead_global_store");
        } else {
            let unit = Operand::Value(body.values.alloc_unshared(ValueKind::Unit, unit));
            body.replace_operand_to(node, store, unit);
        }
    }

    let mut children: Vec<NodeRef> = Vec::new();
    body.for_each_child(node, |c| children.push(c));
    for child in children {
        remove_dead_global_sets(body, child, used, type_table, calls);
    }
}

/// The value `e` stores, when `e` is a store to a global `used` does not hold.
fn dead_store_value(body: &Body, e: ExprId, used: &IndexSet<(String, String)>) -> Option<Operand> {
    let ExprKind::GlobalVarSet {
        module_source,
        name,
        value,
    } = &body.exprs[e].kind
    else {
        return None;
    };
    let key = (module_source.to_path().join("::"), name.clone());
    (!used.contains(&key)).then_some(*value)
}

/// The part of a dead store's value that must still run: all of it, unless it
/// is provably pure and cannot trap.
fn kept_effect(
    body: &Body,
    value: Operand,
    type_table: &TypeTable,
    calls: CallFacts,
) -> Option<ExprId> {
    value
        .as_expr()
        .filter(|_| !deletable_value(body, value, type_table, calls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_source::ModuleSourceInterner;
    use crate::token::Span;

    fn free_fn(interner: &mut ModuleSourceInterner, name: &str) -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(interner, &["test"], name))
    }

    #[test]
    fn test_empty_reachable_set() {
        let mut interner = ModuleSourceInterner::new();
        let call_graph = IndexMap::default();
        let entry = free_fn(&mut interner, "run");
        let reachable = compute_reachable(&call_graph, [entry]);
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

        let reachable = compute_reachable(&call_graph, [free_fn(&mut interner, "run")]);
        assert!(reachable.contains(&free_fn(&mut interner, "run")));
        assert!(reachable.contains(&free_fn(&mut interner, "foo")));
        assert!(reachable.contains(&free_fn(&mut interner, "bar")));
        assert!(!reachable.contains(&free_fn(&mut interner, "unused")));
    }

    /// A statement whose value is a promoted read of `local_index`, plus the
    /// block holding it.
    fn body_reading_promoted_local(local_index: u32) -> (Body, StmtId) {
        let mut body = Body::empty();
        let value = body.values.canonical_local(local_index, TypeId(0));
        let stmt = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Value(value)),
            span: Span::default(),
        });
        body.push_root(BlockNode {
            stmts: vec![stmt],
            span: Span::default(),
        });
        (body, stmt)
    }

    #[test]
    fn mentioned_locals_sees_a_local_read_through_a_promoted_value() {
        let (body, _) = body_reading_promoted_local(3);
        assert!(
            mentioned_locals(&body).contains(&3),
            "a read living in the value pool is still a read"
        );
    }

    #[test]
    fn mentioned_locals_ignores_a_promoted_value_no_reachable_slot_carries() {
        let (mut body, live) = body_reading_promoted_local(3);
        let stale = body.values.canonical_local(4, TypeId(0));
        body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Value(stale)),
            span: Span::default(),
        });
        let root = body.root();
        body.blocks[root].stmts = vec![live];

        let mentioned = mentioned_locals(&body);
        assert!(mentioned.contains(&3));
        assert!(!mentioned.contains(&4));
    }
}
