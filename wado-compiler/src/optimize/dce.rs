//! Dead Code Elimination (DCE) for Wado TIR
//!
//! This module provides dead code elimination at two levels:
//!
//! 1. **Function-level DCE**: Reachability analysis starting from the entry point,
//!    removing functions that are never called.
//!
//! 2. **Constant branch pruning**: When an `if` condition is a compile-time boolean
//!    literal, the dead branch is eliminated and the taken branch is inlined in place.

use crate::hashmap::IndexSet;

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::{
    FreeFunctionName, FunctionId, MethodName, mangle_generic_name, mangle_local_method,
    mangle_local_trait_method, mangle_method_generic,
};
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirFunction, NirImport, NirStmt, NirStmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Call graph: function ID -> set of called function IDs
type CallGraph = IndexMap<FunctionId, IndexSet<FunctionId>>;

/// Effect usage: function ID -> set of (`interface_name`, `operation_name`) pairs
type EffectUsageMap = IndexMap<FunctionId, IndexSet<(String, String)>>;

/// Analysis results for a single function
#[derive(Debug, Clone, Default)]
struct FunctionAnalysis {
    /// Functions called by this function
    callees: IndexSet<FunctionId>,
    /// Effect calls: (`interface_name`, `op_name`)
    effect_calls: IndexSet<(String, String)>,
}

/// Analyze the project and populate its usage fields with DCE analysis results.
///
/// This performs dead code elimination analysis starting from the entry point
/// and populates the project's `used_wasi_functions` field and the `imports`
/// list. Returns the set of reachable functions for use by
/// `remove_unreachable_functions`.
pub fn analyze_project(project: &mut NirPackage) -> IndexSet<FunctionId> {
    // Phase 1a: build a provisional call graph that does NOT root the
    // per-functor `__Closure_N^Inspect[Alt]` impls from
    // `ClosureToCanonical`, then compute its reachable set. This
    // identifies which functions can actually run.
    let (call_graph_v1, _) = build_analysis_graph_with(project, &InspectableSignatures::default());
    let reachable_v1 = compute_reachable_from_entries(project, &call_graph_v1);

    // Phase 1b: derive the inspectable `(arity, ret)` set from the
    // reachable functions only. A dead `:?`/`:#?` call site in
    // unreachable code must NOT keep per-functor inspect impls alive
    // for a reachable canonicalised closure of the matching signature.
    let inspectable = collect_inspectable_signatures_from_reachable(project, &reachable_v1);

    // Phase 1c: rebuild the call graph with inspect roots gated by the
    // reachable-derived signatures, then compute the final reachable
    // set. The per-functor impls themselves don't issue any
    // `Fn^Inspect[Alt]` calls (they just write per-literal strings),
    // so the inspectable set is stable under this expansion — no
    // fixpoint iteration is needed.
    let (call_graph, effect_usage) = build_analysis_graph_with(project, &inspectable);
    let mut reachable = compute_reachable_from_entries(project, &call_graph);

    // Phase 1d: Extend reachable set with optimizer-induced virtual edges.
    // Optimizer passes (e.g. `tir/string_push`) may *synthesize* new calls
    // during the optimization loop. Functions those passes call must
    // survive the early DCE that runs before the loop, otherwise the
    // synthesis target is gone and the rewrite cannot fire. The virtual
    // edges are gated by compiler-item markers so each rule names its
    // canonical pair (`string_push_str` → `string_push_char`, etc.).
    extend_reachable_for_optimizer_passes(project, &call_graph, &mut reachable);

    // Phase 2: Resolve imports and WASI features using reachable set
    resolve_imports(project, &reachable, &effect_usage);

    // Phase 3: Filter literals to reachable functions
    filter_string_literals(project, &reachable);

    reachable
}

/// Add functions that the TIR optimizer's rewrites may *synthesize* calls
/// to. For now this is a single pair: `tir/string_push` rewrites
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

    // `$value_copy$T<id>` helpers synthesized by `lower::plan::value_copy` are
    // reached through two paths: (a) direct TIR-level
    // `copy_value::<T>(...)` callers, which the regular call graph
    // already covers; and (b) the per-element clone hidden inside
    // `array_clone::<T>(arr)` for value-typed `T`, which lowers to a
    // `WirInstr::ArrayClone { element_copy_func: Some("$value_copy$T<id>") }`
    // — the helper name appears as a *string* in the WIR instr at
    // codegen time, not as a TIR call edge, so DCE wouldn't otherwise
    // see it.
    //
    // Walk every reachable function body and, for each
    // `array_clone::<T>(...)` call where `T` is value-typed, mark the
    // corresponding `$value_copy$T<id>` helper as a virtual root.
    // Marking *every* `FunctionKind::ValueCopy` helper (the previous
    // shape) is correct but wastes code size on programs that have
    // many monomorphisations the array-clone path never visits.
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
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let func_id = function_id_for(&func);
        if !reachable.contains(&func_id) {
            continue;
        }
        if let Some(body) = &func.body {
            let mut needed: IndexSet<crate::tir::TypeId> = IndexSet::default();
            collect_array_clone_element_types(body, &mut needed);
            for type_id in needed {
                if let Some(helper_id) = helpers_by_type_id.get(&type_id)
                    && !reachable.contains(helper_id)
                {
                    reachable.extend(compute_reachable(call_graph, helper_id));
                }
            }
        }
    }
}

/// Walk `block`'s expression tree and collect every `T` such that
/// `builtin::array_clone::<T>(...)` appears as a TIR call. The
/// corresponding `$value_copy$T<id>` helper has to survive DCE because
/// codegen will reach it by *name* at WIR time.
fn collect_array_clone_element_types(
    block: &crate::nir::NirBlock,
    out: &mut IndexSet<crate::tir::TypeId>,
) {
    use crate::nir::{NirExpr, NirExprKind};
    use crate::nir_visitor::NirRefVisitor;

    struct Collector<'a> {
        out: &'a mut IndexSet<crate::tir::TypeId>,
    }
    impl NirRefVisitor for Collector<'_> {
        fn visit_expr(&mut self, expr: &NirExpr) {
            if let NirExprKind::Call { func, .. } = &expr.kind
                && func.module_source.is_core_builtin()
                && func.name == "array_clone"
                && let Some(mi) = func.monomorph_info.as_ref()
                && let Some(elem) = mi.impl_type_args.first().copied()
            {
                self.out.insert(elem);
            }
            self.walk_expr(expr);
        }
    }
    let mut collector = Collector { out };
    collector.visit_block(block);
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

/// Filter string literals to only include strings from reachable functions.
fn filter_string_literals(project: &mut NirPackage, reachable: &IndexSet<FunctionId>) {
    // Keys are (module_source, function_name) — no collision possible.
    let mut reachable_strings: IndexSet<String> = IndexSet::default();

    for ((module_source, func_name), strings) in &project.function_strings {
        let is_reachable = if let Some(Some(method_info)) = project
            .function_method_info
            .get(&(module_source.clone(), func_name.clone()))
        {
            let method_id = FunctionId::Method(MethodName::new(
                module_source.clone(),
                method_info.struct_name.clone(),
                method_info.trait_name.clone(),
                method_info.method_name.clone(),
            ));
            if reachable.contains(&method_id) {
                true
            } else {
                let free_id = FunctionId::Free(FreeFunctionName::from_module_source(
                    module_source,
                    func_name,
                ));
                reachable.contains(&free_id)
            }
        } else {
            let func_id = FunctionId::Free(FreeFunctionName::from_module_source(
                module_source,
                func_name,
            ));
            reachable.contains(&func_id)
        };

        if is_reachable {
            reachable_strings.extend(strings.iter().cloned());
        }
    }

    project.string_literals = reachable_strings.into_iter().collect();
}

/// Filter bytes literals to only include bytes referenced by surviving functions.
///
/// Unlike string literals (which have a `function_strings` map for per-function
/// tracking), bytes literals are stored inline as `NirExprKind::BytesLiteral(Vec<u8>)`.
/// This function scans all surviving function bodies to collect referenced bytes,
/// then retains only matching entries in `project.bytes_literals`.
pub fn filter_bytes_literals(project: &mut NirPackage) {
    let mut used_bytes: IndexSet<Vec<u8>> = IndexSet::default();

    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            collect_bytes_literals_block(body, &mut used_bytes);
        }
    }

    project.bytes_literals.retain(|b| used_bytes.contains(b));
}

fn collect_bytes_literals_block(block: &NirBlock, used: &mut IndexSet<Vec<u8>>) {
    for stmt in &block.stmts {
        collect_bytes_literals_stmt(stmt, used);
    }
}

fn collect_bytes_literals_stmt(stmt: &NirStmt, used: &mut IndexSet<Vec<u8>>) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. }
        | NirStmtKind::LetDestructure { value, .. }
        | NirStmtKind::Expr(value) => {
            collect_bytes_literals_expr(value, used);
        }
        NirStmtKind::Return { value } => {
            if let Some(expr) = value {
                collect_bytes_literals_expr(expr, used);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_bytes_literals_expr(condition, used);
            collect_bytes_literals_block(then_block, used);
            if let Some(else_blk) = else_block {
                collect_bytes_literals_block(else_blk, used);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            collect_bytes_literals_block(body, used);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_bytes_literals_expr(v, used);
            }
        }
        NirStmtKind::Continue => {}
    }
}

fn collect_bytes_literals_expr(expr: &NirExpr, used: &mut IndexSet<Vec<u8>>) {
    match &expr.kind {
        NirExprKind::BytesLiteral(b) => {
            used.insert(b.clone());
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_bytes_literals_expr(left, used);
            collect_bytes_literals_expr(right, used);
        }
        NirExprKind::Unary { expr, .. }
        | NirExprKind::Cast { expr, .. }
        | NirExprKind::FieldAccess { expr, .. }
        | NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. }
        | NirExprKind::GlobalVarSet { value: expr, .. }
        | NirExprKind::ClosureToCanonical { functor: expr, .. } => {
            collect_bytes_literals_expr(expr, used);
        }
        NirExprKind::Index { expr, index }
        | NirExprKind::Assign {
            target: expr,
            value: index,
        } => {
            collect_bytes_literals_expr(expr, used);
            collect_bytes_literals_expr(index, used);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_bytes_literals_expr(&arg.expr, used);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_bytes_literals_expr(receiver, used);
            for arg in args {
                collect_bytes_literals_expr(&arg.expr, used);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_bytes_literals_expr(arg, used);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            collect_bytes_literals_expr(callee, used);
            for arg in args {
                collect_bytes_literals_expr(arg, used);
            }
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_bytes_literals_expr(condition, used);
            collect_bytes_literals_block(then_branch, used);
            if let Some(else_blk) = else_branch {
                collect_bytes_literals_block(else_blk, used);
            }
        }
        NirExprKind::Match { expr, arms } => {
            collect_bytes_literals_expr(expr, used);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_bytes_literals_expr(guard, used);
                }
                collect_bytes_literals_expr(&arm.body, used);
            }
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            collect_bytes_literals_block(block, used);
        }
        NirExprKind::TupleLiteral { elements } => {
            for e in elements {
                collect_bytes_literals_expr(e, used);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                collect_bytes_literals_expr(&f.value, used);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_bytes_literals_expr(p, used);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_bytes_literals_expr(scrutinee, used);
            for arm in arms {
                collect_bytes_literals_block(arm, used);
            }
            collect_bytes_literals_block(default, used);
        }
        // Leaf nodes — no children to recurse into
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::Unit
        | NirExprKind::Null
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
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

/// Build call graph and effect usage from all TIR functions
fn build_analysis_graph_with(
    project: &NirPackage,
    inspectable_signatures: &InspectableSignatures,
) -> (CallGraph, EffectUsageMap) {
    let mut call_graph: CallGraph = IndexMap::default();
    let mut effect_usage: EffectUsageMap = IndexMap::default();

    let type_table = &*project.type_table.borrow();

    // Analyze functions (including methods stored as functions)
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let module_source = &func.module_source;
        let func_id = function_id_for(&func);
        let analysis = analyze_function(&func, module_source, type_table, inspectable_signatures);
        call_graph.insert(func_id.clone(), analysis.callees);
        if !analysis.effect_calls.is_empty() {
            effect_usage.insert(func_id.clone(), analysis.effect_calls);
        }
    }

    (call_graph, effect_usage)
}

/// Walk all TIR function bodies and collect every `(arity, return_type)`
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
        if let Some(body) = &func.body {
            scan_inspect_signatures_block(body, type_table, &mut sigs);
        }
    }
    sigs
}

/// Compute the `FunctionId` used by the call graph for a TIR function.
/// Mirrors the keying logic in `build_analysis_graph_with`; centralising
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
    block: &NirBlock,
    type_table: &TypeTable,
    sigs: &mut InspectableSignatures,
) {
    for stmt in &block.stmts {
        scan_inspect_signatures_stmt(stmt, type_table, sigs);
    }
}

fn scan_inspect_signatures_stmt(
    stmt: &NirStmt,
    type_table: &TypeTable,
    sigs: &mut InspectableSignatures,
) {
    use crate::nir_visitor::NirRefVisitor;
    struct Scanner<'a> {
        type_table: &'a TypeTable,
        sigs: &'a mut InspectableSignatures,
    }
    impl NirRefVisitor for Scanner<'_> {
        fn visit_expr(&mut self, expr: &NirExpr) {
            if let NirExprKind::MethodCall { receiver, func, .. } = &expr.kind
                && let Some(info) = &func.method_info
                && info.base_struct_name == "Fn"
                && let Some(trait_name) = info.base_trait_name.as_deref()
            {
                // Receiver type is `&Fn(...)` — peel the reference and read
                // the function's arity + return type out of the type table.
                let recv_type = self.type_table.peel_refs(receiver.type_id);
                if let ResolvedType::Function {
                    params,
                    return_type,
                    ..
                } = self.type_table.get(recv_type)
                {
                    let key = (params.len(), *return_type);
                    match trait_name {
                        "Inspect" => {
                            self.sigs.inspect.insert(key);
                        }
                        "InspectAlt" => {
                            self.sigs.inspect_alt.insert(key);
                        }
                        _ => {}
                    }
                }
            }
            self.walk_expr(expr);
        }
    }
    let mut s = Scanner { type_table, sigs };
    s.visit_stmt(stmt);
}

/// Analyze a TIR function for callees and effect usage
fn analyze_function(
    func: &NirFunction,
    current_module: &ModuleSource,
    type_table: &TypeTable,
    inspectable_signatures: &InspectableSignatures,
) -> FunctionAnalysis {
    let mut analysis = FunctionAnalysis::default();

    if let Some(body) = &func.body {
        analyze_block(
            body,
            current_module,
            type_table,
            inspectable_signatures,
            &mut analysis,
        );
    }
    analysis
}

fn analyze_block(
    block: &NirBlock,
    current_module: &ModuleSource,
    type_table: &TypeTable,
    inspectable_signatures: &InspectableSignatures,
    analysis: &mut FunctionAnalysis,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            NirStmtKind::Let { value, .. } => {
                analyze_expr(
                    value,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
            NirStmtKind::Expr(expr) => {
                analyze_expr(
                    expr,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
            NirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    analyze_expr(
                        expr,
                        current_module,
                        type_table,
                        inspectable_signatures,
                        analysis,
                    );
                }
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                analyze_expr(
                    condition,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
                analyze_block(
                    then_block,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
                if let Some(else_blk) = else_block {
                    analyze_block(
                        else_blk,
                        current_module,
                        type_table,
                        inspectable_signatures,
                        analysis,
                    );
                }
            }
            NirStmtKind::Loop { body } => {
                analyze_block(
                    body,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
            NirStmtKind::LabeledBlock { block, .. } => {
                analyze_block(
                    block,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
            NirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    analyze_expr(
                        v,
                        current_module,
                        type_table,
                        inspectable_signatures,
                        analysis,
                    );
                }
            }
            NirStmtKind::Continue => {}
            NirStmtKind::LetDestructure { value, .. } => {
                analyze_expr(
                    value,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
    }
}

fn analyze_expr(
    expr: &NirExpr,
    current_module: &ModuleSource,
    type_table: &TypeTable,
    inspectable_signatures: &InspectableSignatures,
    analysis: &mut FunctionAnalysis,
) {
    match &expr.kind {
        NirExprKind::Call { func, args, .. } => {
            let original_callee_module = func.module_source.clone();
            let func_name = func.name.clone();

            if func.method_info.is_some() {
                // Formerly StaticCall: static method call (e.g., Box::get, Uint128::from_u64).
                // func_name contains "::" (e.g., "StructName::method" or "StructName^Trait::method").
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
                analysis.callees.insert(callee_id);

                // Detect resource method calls from WASI modules
                let module_path = func.module_path();
                if module_path.len() >= 2
                    && module_path[0] == "wasi"
                    && let Some(pos) = func_name.find("::")
                {
                    let resource_name = &func_name[..pos];
                    let method_name = &func_name[pos + 2..];
                    analysis
                        .effect_calls
                        .insert((resource_name.to_string(), method_name.to_string()));
                }
            } else {
                // Free function call
                debug_assert!(
                    !func_name.contains("::") || func_name.starts_with("builtin::"),
                    "NirExprKind::Call should not have method-style names: {func_name}"
                );

                let callee_module = original_callee_module.clone();
                let callee_id = FunctionId::Free(FreeFunctionName::from_module_source(
                    &callee_module,
                    &func_name,
                ));
                analysis.callees.insert(callee_id);

                if let Some(interface_name) = original_callee_module.interface_name() {
                    analysis.effect_calls.insert((interface_name, func_name));
                }
            }

            for arg in args {
                analyze_expr(
                    &arg.expr,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            // Use the func reference directly - it already has the correct mangled name
            // and monomorph_info from lowering phase
            let func_name = func.name.clone();

            // Check if this is a monomorphized method using FunctionRef metadata
            if func.is_monomorphized() {
                // Monomorphized method (e.g., Array<i32>::len, Box<i32>::get)
                // Use the func reference's information directly
                let base_name = func
                    .base_struct_name()
                    .map(|base| {
                        // Extract method name from "Array<i32>::len" -> "len"
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
                analysis.callees.insert(callee_id);
            } else {
                // Non-monomorphized method - determine target from receiver type
                // First strip any reference wrappers and newtypes to get the base type
                let mut current_type = type_table.get(receiver.type_id);
                let mut newtype_info: Option<(String, ModuleSource)> = None;
                loop {
                    match current_type {
                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                            current_type = type_table.get(*inner);
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
                            current_type = type_table.get(*base_type);
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

                // If the receiver was a newtype (e.g., flags type), also mark
                // the newtype's own methods as reachable (e.g., Perms^Inspect::inspect).
                if let Some((newtype_name, newtype_module)) = newtype_info {
                    let method_id = FunctionId::Method(MethodName::new(
                        newtype_module,
                        newtype_name,
                        trait_name.clone(),
                        method_name.clone(),
                    ));
                    analysis.callees.insert(method_id);
                }

                match base_receiver_type {
                    ResolvedType::Struct {
                        ref name,
                        is_monomorphized: true,
                        base_name: Some(ref base_struct),
                        ..
                    } => {
                        // Monomorphized struct method call - use FunctionId::Free
                        let mangled_func_name =
                            MethodName::format_local(name, trait_name.as_deref(), &method_name);
                        // Build base method name using the original generic struct name
                        let base_method_name = MethodName::format_local(
                            base_struct,
                            trait_name.as_deref(),
                            &method_name,
                        );
                        // Use current module — monomorphized functions live in the
                        // module that uses them.
                        let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                            current_module.clone(),
                            mangled_func_name,
                            base_method_name,
                        ));
                        analysis.callees.insert(callee_id);

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
                            analysis.callees.insert(original_method_id);
                        }
                    }
                    ResolvedType::Struct {
                        name,
                        module_source,
                        is_monomorphized: false,
                        ..
                    } => {
                        // Regular struct method call - use FunctionId::Method
                        let method_id = FunctionId::Method(MethodName::new(
                            module_source.clone(),
                            name,
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);

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
                            analysis.callees.insert(alt_method_id);
                        }
                    }
                    ResolvedType::Primitive(prim) => {
                        // Primitive method call (e.g., i32.to_string())
                        if method_name == "to_string" {
                            add_to_string_callee(receiver.type_id, type_table, analysis);
                        }
                        // Trait and inherent methods on primitives
                        // (e.g., i32^Ord::cmp, char::is_ascii_space)
                        let prim_name = prim.as_str().to_string();
                        let method_id = FunctionId::Method(MethodName::new(
                            ModuleSource::primitive(),
                            prim_name,
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::Unit => {
                        // Unit type () method call (e.g., ().to_string(), ().fmt(&f))
                        let method_id = FunctionId::Method(MethodName::new(
                            ModuleSource::primitive(),
                            TypeTable::UNIT_TYPE_NAME.to_string(),
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_source,
                    } if TypeTable::is_tuple_type(&name, &module_source) => {
                        // Tuple method call (e.g., Tuple<f64,f64>^Inspect::inspect)
                        // Synthesized as non-monomorphized methods with struct_name "Tuple<f64,f64>"
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        let mangled_struct =
                            mangle_generic_name(TypeTable::TUPLE_TYPE_NAME, &type_arg_names);
                        let method_id = FunctionId::Method(MethodName::new(
                            current_module.clone(),
                            mangled_struct,
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_source: _,
                    } => {
                        // Generic instance method call (e.g., Box<i32>.get())
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        // Include trait name for trait methods (e.g., TreeMap<String,i32>^Index::index)
                        let (mangled_func_name, base_name) = if let Some(ref trait_n) = trait_name {
                            let generic_name = mangle_generic_name(&name, &type_arg_names);
                            let mangled =
                                mangle_local_trait_method(&generic_name, trait_n, &method_name);
                            let base = mangle_local_trait_method(&name, trait_n, &method_name);
                            (mangled, base)
                        } else {
                            let mangled =
                                mangle_method_generic(&name, &type_arg_names, &method_name);
                            let base = mangle_local_method(&name, &method_name);
                            (mangled, base)
                        };
                        let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                            current_module.clone(),
                            mangled_func_name,
                            base_name,
                        ));
                        analysis.callees.insert(callee_id);
                    }
                    ResolvedType::Enum {
                        name,
                        module_source,
                    } => {
                        // Enum method call (user-defined or auto-derived trait impls)
                        let method_id = FunctionId::Method(MethodName::new(
                            module_source,
                            name,
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::Resource { name, .. } => {
                        // Resource instance method call (e.g., fields.has(), fields.append())
                        // Record as effect call so it's tracked in used_wasi_functions
                        analysis.effect_calls.insert((name, method_name));
                    }
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    } => {
                        // Variant method call (e.g., Shape^Inspect::inspect)
                        let method_id = FunctionId::Method(MethodName::new(
                            module_source,
                            name,
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::Function {
                        params,
                        return_type,
                        ..
                    } => {
                        // Function type method call (e.g., Fn<2,i32>^Inspect::inspect)
                        let type_arg_names = vec![
                            params.len().to_string(),
                            type_table.mangle_type_name(return_type),
                        ];
                        let mangled_struct = mangle_generic_name("Fn", &type_arg_names);
                        let method_id = FunctionId::Method(MethodName::new(
                            current_module.clone(),
                            mangled_struct,
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::GenericResource {
                        name, type_args, ..
                    } => {
                        // Generic resource method call (e.g., Future<T>^Inspect::inspect)
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        let mangled_struct = mangle_generic_name(name.as_str(), &type_arg_names);
                        let method_id = FunctionId::Method(MethodName::new(
                            current_module.clone(),
                            mangled_struct,
                            trait_name,
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    _ => {}
                }
            }

            analyze_expr(
                receiver,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            for arg in args {
                analyze_expr(
                    &arg.expr,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::Binary { left, right, .. } => {
            analyze_expr(
                left,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            analyze_expr(
                right,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::Unary { expr, .. } => {
            analyze_expr(
                expr,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::Assign { target, value } => {
            analyze_expr(
                target,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            analyze_expr(
                value,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::Cast { expr, .. } => {
            analyze_expr(
                expr,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::CmRawCall { local_name, args } => {
            // CmRawCall references a lowered WASI import function.
            // Parse the local_name (e.g., "wasi:cli/Stdout::write_via_stream")
            // to extract the interface_name and op_name for WASI import tracking.
            if let Some((interface_name, op_name)) =
                local_name.split_once("::").map(|(prefix, op)| {
                    // prefix is like "wasi:cli/Stdout" → extract "Stdout"
                    let effect = prefix.rsplit('/').next().unwrap_or(prefix);
                    (effect.to_string(), op.to_string())
                })
            {
                analysis.effect_calls.insert((interface_name, op_name));
            }
            for arg in args {
                analyze_expr(
                    arg,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::FieldAccess { expr, .. } => {
            analyze_expr(
                expr,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::Index { expr, index } => {
            analyze_expr(
                expr,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            analyze_expr(
                index,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::Block(block) => {
            analyze_block(
                block,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_expr(
                condition,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            analyze_block(
                then_branch,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            if let Some(else_blk) = else_branch {
                analyze_block(
                    else_blk,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::Match { expr, arms } => {
            analyze_expr(
                expr,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_expr(
                        guard,
                        current_module,
                        type_table,
                        inspectable_signatures,
                        analysis,
                    );
                }
                analyze_expr(
                    &arm.body,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_expr(
                    &field.value,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                analyze_expr(
                    elem,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            analyze_expr(
                callee,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            for arg in args {
                analyze_expr(
                    arg,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::ClosureToCanonical {
            functor,
            functor_id,
            target_fn_type,
            closure_module,
        } => {
            analyze_expr(
                functor,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            // The `__call` method is always reached via `ref.func` baked
            // into the canonical closure struct's `func` slot.
            let struct_name = format!(
                "{prefix}{functor_id}",
                prefix = crate::name::CLOSURE_STRUCT_PREFIX,
            );
            analysis.callees.insert(FunctionId::Method(MethodName::new(
                closure_module.clone(),
                struct_name.clone(),
                None,
                crate::name::CLOSURE_CALL_METHOD.to_string(),
            )));

            // The per-functor `__Closure_N^Inspect` and `^InspectAlt`
            // impls (and their per-literal source-string constants) only
            // need to stay alive when their corresponding `Fn<arity,
            // ret>^Inspect[Alt]` dispatch stub is reachable — gated
            // independently per trait method. A program that only ever
            // uses `:?` keeps `__Closure_N^Inspect` but drops
            // `__Closure_N^InspectAlt` and its source-string literal.
            if let ResolvedType::Function {
                params,
                return_type,
                ..
            } = type_table.get(*target_fn_type)
            {
                let key = (params.len(), *return_type);
                if inspectable_signatures.inspect.contains(&key) {
                    analysis.callees.insert(FunctionId::Method(MethodName::new(
                        closure_module.clone(),
                        struct_name.clone(),
                        Some("Inspect".to_string()),
                        "inspect".to_string(),
                    )));
                }
                if inspectable_signatures.inspect_alt.contains(&key) {
                    analysis.callees.insert(FunctionId::Method(MethodName::new(
                        closure_module.clone(),
                        struct_name,
                        Some("InspectAlt".to_string()),
                        "inspect_alt".to_string(),
                    )));
                }
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                analyze_expr(
                    payload_expr,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
        }
        NirExprKind::LabeledBlock { block, .. } => {
            analyze_block(
                block,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            analyze_expr(
                value,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            analyze_expr(
                expr,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::VariantPayload { expr, .. } => {
            analyze_expr(
                expr,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_expr(
                scrutinee,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
            for arm in arms {
                analyze_block(
                    arm,
                    current_module,
                    type_table,
                    inspectable_signatures,
                    analysis,
                );
            }
            analyze_block(
                default,
                current_module,
                type_table,
                inspectable_signatures,
                analysis,
            );
        }
        // Leaf nodes - no calls
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Add the appropriate `to_string` function call for a type
fn add_to_string_callee(type_id: TypeId, type_table: &TypeTable, analysis: &mut FunctionAnalysis) {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => {
            // Primitive to_string methods are defined in core:prelude/primitive as impl blocks
            // e.g., impl i32 { fn to_string(&self) -> String { ... } }
            let prim_name = prim.as_str();
            // Method format: module_source/StructName::method_name
            let method_id = FunctionId::Method(MethodName::new(
                ModuleSource::primitive(),
                prim_name.to_string(),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Unit => {
            // Unit type () to_string is defined in core:prelude/primitive
            let method_id = FunctionId::Method(MethodName::new(
                ModuleSource::primitive(),
                TypeTable::UNIT_TYPE_NAME.to_string(),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Struct { name, .. } if name == "String" => {
            // String.to_string() is a no-op, no function call needed
        }
        _ => {}
    }
}

/// Mangle a type ID into a string suitable for struct/function names.
/// Compute the set of reachable functions from an entry point
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

/// Remove unreachable functions from the project's function list.
///
/// After this, all remaining functions are reachable — downstream phases
/// (`wir_build`, codegen) register every function without additional filtering.
pub fn remove_unreachable_functions(
    project: &mut NirPackage,
    reachable_functions: &IndexSet<FunctionId>,
) {
    // Pre-index the reachable set by monomorphization metadata so the
    // "keep a generic template if any of its instantiations is reachable"
    // check is O(1) per function instead of O(|reachable|).
    //
    // Every monomorphized `FreeFunctionName` carries `base_name` that is
    // exactly the generic template's `func.name` (e.g. "Array::with_capacity"
    // for "Array<i32>::with_capacity"), so we index by (module_source, base_name)
    // and compare it to (func.module_source, func.name) directly — no string
    // parsing required.
    let reachable_monomorph_bases: IndexSet<(ModuleSource, String)> = reachable_functions
        .iter()
        .filter_map(|id| match id {
            FunctionId::Free(name) if name.is_monomorphized => name
                .base_name
                .as_ref()
                .map(|base| (name.module_source.clone(), base.clone())),
            _ => None,
        })
        .collect();

    project.functions.retain(|func_rc| {
        let func = func_rc.borrow();
        let module_source = &func.module_source;

        // Use NirFunction's method_info to check if this is a method
        if let Some(ref info) = func.method_info {
            // Could be either:
            // - Instance method tracked as FunctionId::Method
            // - Static method tracked as FunctionId::Free with mangled name
            // Use method_info to build the method ID
            // Try as instance method (FunctionId::Method)
            let method_id = FunctionId::Method(MethodName::new(
                module_source.clone(),
                info.struct_name.clone(),
                info.trait_name.clone(),
                info.method_name.clone(),
            ));
            if reachable_functions.contains(&method_id) {
                return true;
            }

            // Try as static method (FunctionId::Free with mangled name)
            let free_id = FunctionId::Free(FreeFunctionName::from_module_source(
                module_source,
                &func.name,
            ));
            if reachable_functions.contains(&free_id) {
                return true;
            }

            // Generic template: keep it if any monomorphized instance is reachable.
            // The instance carries `base_name == func.name`, so this is a direct
            // metadata comparison — no string search.
            reachable_monomorph_bases.contains(&(module_source.clone(), func.name.clone()))
        } else {
            // Regular function
            let func_id = FunctionId::Free(FreeFunctionName::from_module_source(
                module_source,
                &func.name,
            ));
            reachable_functions.contains(&func_id)
        }
    });
}

/// Compute the set of reachable types from reachable functions.
/// A type is reachable if it's used in any reachable function's signature,
/// locals, or expressions.
fn compute_reachable_types(project: &NirPackage) -> IndexSet<TypeId> {
    let mut reachable_types: IndexSet<TypeId> = IndexSet::default();

    // Always include primitive types (TypeId 0-17)
    for i in 0..18 {
        reachable_types.insert(TypeId(i));
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
                reachable_types.insert(type_id);
                break;
            }
        }
    }

    // Phase 1: Collect types from all remaining functions
    // Note: We collect from ALL functions that exist after function DCE,
    // because function DCE has already removed unreachable functions.
    // This is more conservative but ensures we don't miss any types.
    {
        let type_table = project.type_table.borrow();

        for func_rc in &project.functions {
            let func = func_rc.borrow();
            collect_types_from_function(&func, &type_table, &mut reachable_types);
        }

        // Collect types from global variables
        for global in &project.globals {
            collect_types_from_expr(&global.initializer, &type_table, &mut reachable_types);
        }

        // Closure functor types are collected transitively from
        // ClosureToCanonical expressions in reachable functions. No need
        // to unconditionally mark all functor types as reachable — unused
        // closures should have their types DCE'd.
        //
        // BUT: if the functor's `__call` method is still reachable (kept
        // by function DCE), the functor's struct / ref types must stay
        // live too. `wir_build::register_closure_wrappers` reads
        // `ClosureFunctor::ref_type_id` to emit the wrapper's `ref.cast`,
        // and TIR DAE may have removed the struct ref from
        // `call_method.params[0]` (dropping the env `self`) — at which
        // point the only remaining TIR-side reference is the
        // `ClosureFunctor` record itself. Without this insertion, that
        // type-table lookup panics with `TypeId not found`.
        // The functor's `call_method` and `project.functions[i]` are the
        // same `Rc` when DCE has kept the function alive — comparing by
        // pointer identity avoids cloning `(ModuleSource, String)` per
        // function just to build a lookup set. Pre-compute a hash set of
        // raw pointers so the per-functor check is O(1) instead of an
        // O(|functions|) linear scan per functor.
        let surviving_ptrs: IndexSet<*const _> =
            project.functions.iter().map(std::rc::Rc::as_ptr).collect();
        for functor in &project.closure_functors {
            let cm_ptr = std::rc::Rc::as_ptr(&functor.call_method);
            if surviving_ptrs.contains(&cm_ptr) {
                reachable_types.insert(functor.struct_type_id);
                reachable_types.insert(functor.ref_type_id);
            }
        }
    }

    // Phase 2: Transitive closure - include struct fields, variant payloads, and type dependencies
    let mut changed = true;
    while changed {
        changed = false;
        let before_len = reachable_types.len();

        let type_table = project.type_table.borrow();

        // Collect struct field types for reachable structs
        // A struct's fields should be collected if:
        // 1. The Struct type itself is reachable, OR
        // 2. Any GenericInstance with this struct name is reachable, OR
        // 3. Any monomorphized version with this base name is reachable
        for tir_struct in &project.structs {
            let module_source = &tir_struct.module_source;
            let struct_reachable = if tir_struct.monomorph_info.is_none() {
                // Non-monomorphized struct
                let direct_reachable = type_table
                    .find_struct_type(&tir_struct.name, module_source)
                    .map(|id| reachable_types.contains(&id))
                    .unwrap_or(false);

                let instance_reachable = reachable_types.iter().any(|&id| {
                    matches!(
                        type_table.get(id),
                        ResolvedType::GenericInstance { name, .. } if name == &tir_struct.name
                    )
                });

                let monomorph_reachable = reachable_types.iter().any(|&id| {
                    matches!(
                        type_table.get(id),
                        ResolvedType::Struct { base_name: Some(base), is_monomorphized: true, .. } if base == &tir_struct.name
                    )
                });

                direct_reachable || instance_reachable || monomorph_reachable
            } else {
                // Monomorphized struct - check by exact name match
                reachable_types.iter().any(|&id| {
                    matches!(
                        type_table.get(id),
                        ResolvedType::Struct { name, is_monomorphized: true, .. } if name == &tir_struct.name
                    )
                })
            };

            if struct_reachable {
                for field in &tir_struct.fields {
                    collect_type_transitive(field.type_id, &type_table, &mut reachable_types);
                }
                // Monomorphization type args are used by WIR for name mangling
                if let Some(info) = &tir_struct.monomorph_info {
                    for &ta in &info.impl_type_args {
                        collect_type_transitive(ta, &type_table, &mut reachable_types);
                    }
                    for &ta in &info.method_type_args {
                        collect_type_transitive(ta, &type_table, &mut reachable_types);
                    }
                }
            }
        }

        // Collect variant payload types for reachable variants
        // A variant's payloads should be collected if:
        // 1. The base Variant type is reachable, OR
        // 2. Any GenericInstance with this variant name is reachable
        for variant in &project.variants {
            let base_reachable = type_table
                .iter_type_ids()
                .find(|&id| matches!(type_table.get(id), ResolvedType::Variant { name, .. } if name == &variant.name))
                .map(|id| reachable_types.contains(&id))
                .unwrap_or(false);

            let instance_reachable = reachable_types.iter().any(|&id| {
                matches!(
                    type_table.get(id),
                    ResolvedType::GenericInstance { name, .. } if name == &variant.name
                )
            });

            if base_reachable || instance_reachable {
                for case in &variant.cases {
                    collect_type_transitive(case.payload, &type_table, &mut reachable_types);
                }
            }
        }

        // Collect type dependencies (array elements, option inner, etc.)
        let current_types: Vec<TypeId> = reachable_types.iter().copied().collect();
        for type_id in current_types {
            collect_type_dependencies(type_id, &type_table, &mut reachable_types);
        }

        drop(type_table);

        if reachable_types.len() > before_len {
            changed = true;
        }
    }

    reachable_types
}

/// Collect all types used in a function
fn collect_types_from_function(
    func: &NirFunction,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    // Collect parameter types
    for param in &func.params {
        collect_type_transitive(param.type_id, type_table, reachable);
    }

    // Collect return type
    collect_type_transitive(func.return_type, type_table, reachable);

    // Collect local variable types (includes types from inlined functions)
    for local in &func.locals {
        collect_type_transitive(local.type_id, type_table, reachable);
    }

    // Collect monomorphization type args (used by WIR for name mangling)
    if let Some(info) = &func.monomorph_info {
        for &ta in &info.impl_type_args {
            collect_type_transitive(ta, type_table, reachable);
        }
        for &ta in &info.method_type_args {
            collect_type_transitive(ta, type_table, reachable);
        }
    }

    // Collect types from body
    if let Some(body) = &func.body {
        collect_types_from_block(body, type_table, reachable);
    }
}

/// Collect types from a block
fn collect_types_from_block(
    block: &NirBlock,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            NirStmtKind::Let { value, type_id, .. } => {
                collect_type_transitive(*type_id, type_table, reachable);
                collect_types_from_expr(value, type_table, reachable);
            }
            NirStmtKind::Expr(expr) => {
                collect_types_from_expr(expr, type_table, reachable);
            }
            NirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    collect_types_from_expr(expr, type_table, reachable);
                }
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_types_from_expr(condition, type_table, reachable);
                collect_types_from_block(then_block, type_table, reachable);
                if let Some(else_blk) = else_block {
                    collect_types_from_block(else_blk, type_table, reachable);
                }
            }
            NirStmtKind::Loop { body } => {
                collect_types_from_block(body, type_table, reachable);
            }
            NirStmtKind::LabeledBlock { block, .. } => {
                collect_types_from_block(block, type_table, reachable);
            }
            NirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    collect_types_from_expr(v, type_table, reachable);
                }
            }
            NirStmtKind::Continue => {}
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                collect_types_from_pattern(pattern, type_table, reachable);
                collect_types_from_expr(value, type_table, reachable);
            }
        }
    }
}

/// Collect types from an expression
fn collect_types_from_expr(
    expr: &NirExpr,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    // Always collect the expression's type
    collect_type_transitive(expr.type_id, type_table, reachable);

    match &expr.kind {
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_types_from_expr(&arg.expr, type_table, reachable);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_types_from_expr(receiver, type_table, reachable);
            for arg in args {
                collect_types_from_expr(&arg.expr, type_table, reachable);
            }
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_types_from_expr(left, type_table, reachable);
            collect_types_from_expr(right, type_table, reachable);
        }
        NirExprKind::Unary { expr, .. } => {
            collect_types_from_expr(expr, type_table, reachable);
        }
        NirExprKind::Assign { target, value } => {
            collect_types_from_expr(target, type_table, reachable);
            collect_types_from_expr(value, type_table, reachable);
        }
        NirExprKind::Cast { expr, target_type } => {
            collect_types_from_expr(expr, type_table, reachable);
            collect_type_transitive(*target_type, type_table, reachable);
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_types_from_expr(arg, type_table, reachable);
            }
        }
        NirExprKind::FieldAccess { expr, .. } => {
            collect_types_from_expr(expr, type_table, reachable);
        }
        NirExprKind::Index { expr, index } => {
            collect_types_from_expr(expr, type_table, reachable);
            collect_types_from_expr(index, type_table, reachable);
        }
        NirExprKind::Block(block) => {
            collect_types_from_block(block, type_table, reachable);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_types_from_expr(condition, type_table, reachable);
            collect_types_from_block(then_branch, type_table, reachable);
            if let Some(else_blk) = else_branch {
                collect_types_from_block(else_blk, type_table, reachable);
            }
        }
        NirExprKind::Match { expr, arms } => {
            collect_types_from_expr(expr, type_table, reachable);
            for arm in arms {
                collect_types_from_pattern(&arm.pattern, type_table, reachable);
                if let Some(guard) = &arm.guard {
                    collect_types_from_expr(guard, type_table, reachable);
                }
                collect_types_from_expr(&arm.body, type_table, reachable);
            }
        }
        NirExprKind::StructLiteral {
            struct_type,
            fields,
            ..
        } => {
            collect_type_transitive(*struct_type, type_table, reachable);
            for field in fields {
                collect_types_from_expr(&field.value, type_table, reachable);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_types_from_expr(elem, type_table, reachable);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            collect_types_from_expr(callee, type_table, reachable);
            for arg in args {
                collect_types_from_expr(arg, type_table, reachable);
            }
        }
        NirExprKind::ClosureToCanonical {
            functor,
            target_fn_type,
            ..
        } => {
            collect_types_from_expr(functor, type_table, reachable);
            collect_type_transitive(*target_fn_type, type_table, reachable);
        }
        NirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } => {
            collect_type_transitive(*variant_type, type_table, reachable);
            if let Some(payload_expr) = payload {
                collect_types_from_expr(payload_expr, type_table, reachable);
            }
        }
        NirExprKind::LabeledBlock { block, .. } => {
            collect_types_from_block(block, type_table, reachable);
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            collect_types_from_expr(value, type_table, reachable);
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            collect_types_from_expr(expr, type_table, reachable);
        }
        NirExprKind::VariantPayload {
            expr, payload_type, ..
        } => {
            collect_types_from_expr(expr, type_table, reachable);
            collect_type_transitive(*payload_type, type_table, reachable);
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_types_from_expr(scrutinee, type_table, reachable);
            for arm in arms {
                collect_types_from_block(arm, type_table, reachable);
            }
            collect_types_from_block(default, type_table, reachable);
        }
        // Leaf nodes
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Collect types from a pattern
fn collect_types_from_pattern(
    pattern: &crate::nir::NirPattern,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    use crate::nir::NirPattern;

    match pattern {
        NirPattern::Wildcard => {}
        NirPattern::Binding { type_id, .. } => {
            collect_type_transitive(*type_id, type_table, reachable);
        }
        NirPattern::Literal(_) | NirPattern::Range { .. } => {}
        NirPattern::Tuple(patterns, _) => {
            for p in patterns {
                collect_types_from_pattern(p, type_table, reachable);
            }
        }
        NirPattern::Variant {
            enum_type,
            bindings,
            payload_type,
            ..
        } => {
            collect_type_transitive(*enum_type, type_table, reachable);
            collect_type_transitive(*payload_type, type_table, reachable);
            for binding in bindings {
                collect_types_from_pattern(binding, type_table, reachable);
            }
        }
        NirPattern::Enum { enum_type, .. } => {
            collect_type_transitive(*enum_type, type_table, reachable);
        }
        NirPattern::Struct {
            struct_type,
            fields,
            ..
        } => {
            collect_type_transitive(*struct_type, type_table, reachable);
            for field in fields {
                collect_types_from_pattern(&field.pattern, type_table, reachable);
            }
        }
        NirPattern::Or(alternatives) => {
            for p in alternatives {
                collect_types_from_pattern(p, type_table, reachable);
            }
        }
        NirPattern::ConstantValue { expr } => {
            collect_type_transitive(expr.type_id, type_table, reachable);
        }
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
        | ResolvedType::TypePack { .. }
        | ResolvedType::AssocTypeProjection { .. } => {}

        // Newtype: collect dependency on base type
        ResolvedType::Newtype { base_type, .. } => {
            collect_type_transitive(*base_type, type_table, reachable);
        }
        // Flags: depends on u32 (always reachable, no-op)
        ResolvedType::Flags { .. } => {}
    }
}

/// Remove unreachable types from the project's `TypeTable` and module definitions.
/// This should be called after function DCE.
pub fn remove_unreachable_types(project: &mut NirPackage) {
    let reachable_types = compute_reachable_types(project);

    // Collect names of structs to keep
    // A struct is kept if:
    // 1. Its Struct type is reachable, OR
    // 2. Any GenericInstance with its base name is reachable (e.g., Box<i32> for Box)
    // 3. Any monomorphized Struct with its base name is reachable
    {
        let type_table = project.type_table.borrow();

        // Single pass over reachable_types to build lookup indices keyed by the
        // metadata already carried on each ResolvedType. This replaces the
        // earlier O(|structs| × |reachable_types|) repeated linear scans.
        let mut reachable_struct_exact: IndexSet<(String, ModuleSource)> = IndexSet::default();
        let mut reachable_struct_monomorph_names: IndexSet<String> = IndexSet::default();
        let mut reachable_struct_monomorph_bases: IndexSet<String> = IndexSet::default();
        let mut reachable_generic_instance_names: IndexSet<String> = IndexSet::default();
        let mut reachable_variant_exact: IndexSet<(String, ModuleSource)> = IndexSet::default();
        let mut reachable_enum_exact: IndexSet<(String, ModuleSource)> = IndexSet::default();

        for &id in &reachable_types {
            match type_table.get(id) {
                ResolvedType::Struct {
                    name,
                    module_source,
                    is_monomorphized,
                    base_name,
                } => {
                    if *is_monomorphized {
                        reachable_struct_monomorph_names.insert(name.clone());
                        if let Some(base) = base_name {
                            reachable_struct_monomorph_bases.insert(base.clone());
                        }
                    } else {
                        reachable_struct_exact.insert((name.clone(), module_source.clone()));
                    }
                }
                ResolvedType::GenericInstance { name, .. } => {
                    reachable_generic_instance_names.insert(name.clone());
                }
                ResolvedType::Variant {
                    name,
                    module_source,
                } => {
                    reachable_variant_exact.insert((name.clone(), module_source.clone()));
                }
                ResolvedType::Enum {
                    name,
                    module_source,
                } => {
                    reachable_enum_exact.insert((name.clone(), module_source.clone()));
                }
                _ => {}
            }
        }

        let keep_structs: IndexSet<(String, ModuleSource)> = project
            .structs
            .iter()
            .filter(|s| {
                if s.monomorph_info.is_none() {
                    reachable_struct_exact.contains(&(s.name.clone(), s.module_source.clone()))
                        || reachable_generic_instance_names.contains(&s.name)
                        || reachable_struct_monomorph_bases.contains(&s.name)
                } else {
                    reachable_struct_monomorph_names.contains(&s.name)
                }
            })
            .map(|s| (s.name.clone(), s.module_source.clone()))
            .collect();

        let keep_variants: IndexSet<(String, ModuleSource)> = project
            .variants
            .iter()
            .filter(|v| {
                reachable_variant_exact.contains(&(v.name.clone(), v.module_source.clone()))
                    || reachable_generic_instance_names.contains(&v.name)
            })
            .map(|v| (v.name.clone(), v.module_source.clone()))
            .collect();

        let keep_enums: IndexSet<(String, ModuleSource)> = project
            .enums
            .iter()
            .filter(|e| reachable_enum_exact.contains(&(e.name.clone(), e.module_source.clone())))
            .map(|e| (e.name.clone(), e.module_source.clone()))
            .collect();

        drop(type_table);

        // Remove unreachable definitions
        project
            .structs
            .retain(|s| keep_structs.contains(&(s.name.clone(), s.module_source.clone())));
        project
            .variants
            .retain(|v| keep_variants.contains(&(v.name.clone(), v.module_source.clone())));
        project
            .enums
            .retain(|e| keep_enums.contains(&(e.name.clone(), e.module_source.clone())));
    }

    // Remove unreachable entries from the shared TypeTable.
    // This ensures that subsequent phases (WIR type registration, codegen) do not
    // emit types that are no longer referenced by any surviving function.
    project.type_table.borrow_mut().retain(&reachable_types);
}

// ──────────────────────────────────────────────────────────────────────────────
// Global variable DCE
// ──────────────────────────────────────────────────────────────────────────────

/// Remove unreachable global variables from the project's TIR modules.
///
/// A global is considered "used" if any surviving function references it via
/// `GlobalVarGet`. Globals only referenced by `GlobalVarSet` (e.g., their
/// lazy initializer in `__initialize_module`) are dead.
///
/// When a global is removed:
/// 1. Its declaration is removed from `module.globals`
/// 2. Any `GlobalVarSet` statements for it are removed from function bodies
///    (this covers both the original `__initialize_module` and inlined copies)
pub fn remove_unreachable_globals(project: &mut NirPackage) {
    // Phase 1: Collect all GlobalVarGet references from surviving functions.
    // Key: (module_source path as string, global name)
    let mut used_globals: IndexSet<(String, String)> = IndexSet::default();

    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            collect_global_reads_block(body, &mut used_globals);
        }
    }

    // Phase 2: Remove unused globals
    project.globals.retain(|global| {
        let global_module_key = global.module_source.to_path().join("::");
        used_globals.contains(&(global_module_key, global.name.clone()))
    });

    // Phase 3: Remove GlobalVarSet statements for dead globals from function bodies
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = &mut func.body {
            remove_dead_global_sets_block(body, &used_globals);
        }
    }
}

/// Collect all `GlobalVarGet` references from a block.
fn collect_global_reads_block(block: &NirBlock, used: &mut IndexSet<(String, String)>) {
    for stmt in &block.stmts {
        collect_global_reads_stmt(stmt, used);
    }
}

fn collect_global_reads_stmt(stmt: &NirStmt, used: &mut IndexSet<(String, String)>) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. }
        | NirStmtKind::LetDestructure { value, .. }
        | NirStmtKind::Expr(value) => {
            collect_global_reads_expr(value, used);
        }
        NirStmtKind::Return { value } => {
            if let Some(expr) = value {
                collect_global_reads_expr(expr, used);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_global_reads_expr(condition, used);
            collect_global_reads_block(then_block, used);
            if let Some(else_blk) = else_block {
                collect_global_reads_block(else_blk, used);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            collect_global_reads_block(body, used);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_global_reads_expr(v, used);
            }
        }
        NirStmtKind::Continue => {}
    }
}

fn collect_global_reads_expr(expr: &NirExpr, used: &mut IndexSet<(String, String)>) {
    match &expr.kind {
        NirExprKind::GlobalVarGet {
            module_source,
            name,
        } => {
            used.insert((module_source.to_path().join("::"), name.clone()));
        }
        // Recurse into sub-expressions — mirrors analyze_expr structure
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_global_reads_expr(&arg.expr, used);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_global_reads_expr(arg, used);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_global_reads_expr(receiver, used);
            for arg in args {
                collect_global_reads_expr(&arg.expr, used);
            }
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_global_reads_expr(left, used);
            collect_global_reads_expr(right, used);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => {
            collect_global_reads_expr(inner, used);
        }
        NirExprKind::Assign { target, value } => {
            collect_global_reads_expr(target, used);
            collect_global_reads_expr(value, used);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_global_reads_expr(condition, used);
            collect_global_reads_block(then_branch, used);
            if let Some(else_blk) = else_branch {
                collect_global_reads_block(else_blk, used);
            }
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            collect_global_reads_block(block, used);
        }
        NirExprKind::Index { expr, index } => {
            collect_global_reads_expr(expr, used);
            collect_global_reads_expr(index, used);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_global_reads_expr(&field.value, used);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_global_reads_expr(elem, used);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            collect_global_reads_expr(callee, used);
            for arg in args {
                collect_global_reads_expr(arg, used);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_global_reads_expr(functor, used);
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_global_reads_expr(payload_expr, used);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            collect_global_reads_expr(value, used);
        }
        NirExprKind::Match { expr, arms } => {
            collect_global_reads_expr(expr, used);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_global_reads_expr(guard, used);
                }
                collect_global_reads_expr(&arm.body, used);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_global_reads_expr(scrutinee, used);
            for arm in arms {
                collect_global_reads_block(arm, used);
            }
            collect_global_reads_block(default, used);
        }
        // Leaf nodes — no GlobalVarGet possible
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Remove `GlobalVarSet` statements for dead globals from a block.
///
/// For dead globals whose initializer contains function calls (potential side
/// effects), the `GlobalVarSet` is replaced with the value expression to
/// preserve the side effects. For pure initializers (constants, struct/array
/// literals without calls), the entire statement is removed.
fn remove_dead_global_sets_block(block: &mut NirBlock, used: &IndexSet<(String, String)>) {
    // Recurse into sub-statements first
    for stmt in &mut block.stmts {
        remove_dead_global_sets_stmt(stmt, used);
    }

    // Process GlobalVarSet statements for dead globals
    let mut new_stmts: Vec<NirStmt> = Vec::with_capacity(block.stmts.len());
    for stmt in std::mem::take(&mut block.stmts) {
        if let NirStmtKind::Expr(ref expr) = stmt.kind
            && let NirExprKind::GlobalVarSet {
                ref module_source,
                ref name,
                ref value,
                ..
            } = expr.kind
        {
            let key = (module_source.to_path().join("::"), name.clone());
            if !used.contains(&key) {
                // Dead global: keep the value expression only if it has side effects
                // (e.g., panic() / unreachable — detected via never type)
                if expr_has_side_effects(value) {
                    new_stmts.push(NirStmt::new(NirStmtKind::Expr(*value.clone()), stmt.span));
                }
                continue;
            }
        }
        new_stmts.push(stmt);
    }
    block.stmts = new_stmts;
}

/// Check whether an expression tree contains observable side effects.
///
/// Only diverging expressions (type `never` — e.g. `panic()`, `unreachable()`) are
/// considered side effects. Pure function calls like array construction are not.
fn expr_has_side_effects(expr: &NirExpr) -> bool {
    if expr.type_id == TypeTable::NEVER {
        return true;
    }
    match &expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            block_has_side_effects(block)
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_side_effects(condition)
                || block_has_side_effects(then_branch)
                || else_branch.as_ref().is_some_and(block_has_side_effects)
        }
        NirExprKind::Match { expr, arms } => {
            expr_has_side_effects(expr)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(expr_has_side_effects)
                        || expr_has_side_effects(&a.body)
                })
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_side_effects(scrutinee)
                || arms.iter().any(block_has_side_effects)
                || block_has_side_effects(default)
        }
        _ => false,
    }
}

fn block_has_side_effects(block: &NirBlock) -> bool {
    block.stmts.iter().any(|stmt| match &stmt.kind {
        NirStmtKind::Expr(e) | NirStmtKind::Let { value: e, .. } => expr_has_side_effects(e),
        NirStmtKind::Return { value } => value.as_ref().is_some_and(expr_has_side_effects),
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_has_side_effects(condition)
                || block_has_side_effects(then_block)
                || else_block.as_ref().is_some_and(block_has_side_effects)
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            block_has_side_effects(body)
        }
        NirStmtKind::Break { value, .. } => value.as_ref().is_some_and(expr_has_side_effects),
        NirStmtKind::Continue => false,
        NirStmtKind::LetDestructure { value, .. } => expr_has_side_effects(value),
    })
}

fn remove_dead_global_sets_stmt(stmt: &mut NirStmt, used: &IndexSet<(String, String)>) {
    match &mut stmt.kind {
        NirStmtKind::Expr(expr) | NirStmtKind::Let { value: expr, .. } => {
            remove_dead_global_sets_expr(expr, used);
        }
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            remove_dead_global_sets_block(then_block, used);
            if let Some(else_blk) = else_block {
                remove_dead_global_sets_block(else_blk, used);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            remove_dead_global_sets_block(body, used);
        }
        NirStmtKind::Return { value } => {
            if let Some(expr) = value {
                remove_dead_global_sets_expr(expr, used);
            }
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(expr) = value {
                remove_dead_global_sets_expr(expr, used);
            }
        }
        NirStmtKind::Continue | NirStmtKind::LetDestructure { .. } => {}
    }
}

/// Recursively remove dead `GlobalVarSet` from expressions that contain blocks.
fn remove_dead_global_sets_expr(expr: &mut NirExpr, used: &IndexSet<(String, String)>) {
    match &mut expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            remove_dead_global_sets_block(block, used);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remove_dead_global_sets_expr(condition, used);
            remove_dead_global_sets_block(then_branch, used);
            if let Some(else_blk) = else_branch {
                remove_dead_global_sets_block(else_blk, used);
            }
        }
        NirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            remove_dead_global_sets_expr(scrutinee, used);
            for arm in arms {
                remove_dead_global_sets_expr(&mut arm.body, used);
            }
        }
        NirExprKind::Switch { arms, default, .. } => {
            for arm in arms {
                remove_dead_global_sets_block(arm, used);
            }
            remove_dead_global_sets_block(default, used);
        }
        _ => {}
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
