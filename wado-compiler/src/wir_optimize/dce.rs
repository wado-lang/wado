//! Dead Code Elimination (DCE) passes for WIR.
//!
//! - **`mark_unreachable_defined_functions`**: marks defined functions
//!   not reachable from exports/elements as dead via
//!   `dead_func_indices`.
//! - **`dce_unreachable_types`**: marks GC types not referenced by any
//!   live code as dead via `dead_type_indices`.
//! - **`compact_dead_items`**: removes all dead-marked items and remaps
//!   indices.
//!
//! All three operate on either the GC module or the memory module — the
//! base offset between absolute `WirFuncId` indices and 0-based
//! `module.functions` array indices is read from
//! [`WirPackage::defined_func_base`].

use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{
    WirExportDesc, WirFuncId, WirImportDesc, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId,
};

fn collect_func_refs_from_body(body: &[WirInstr], out: &mut IndexSet<u32>) {
    for instr in body {
        collect_func_refs_recursive(instr, out);
    }
}

/// Walk `body` and, for every `WirInstr::ArrayClone` carrying an
/// `element_copy_func` name suffix, resolve it through `resolve` and
/// insert the resulting array-index into `out`. Used by
/// [`mark_unreachable_defined_functions`] to root the per-element
/// `$value_copy$T<id>` helper that codegen reaches via name lookup.
fn collect_array_clone_helper_refs<F>(body: &[WirInstr], resolve: &F, out: &mut IndexSet<u32>)
where
    F: Fn(&str) -> Option<u32>,
{
    for instr in body {
        collect_array_clone_helper_refs_recursive(instr, resolve, out);
    }
}

fn collect_array_clone_helper_refs_recursive<F>(
    instr: &WirInstr,
    resolve: &F,
    out: &mut IndexSet<u32>,
) where
    F: Fn(&str) -> Option<u32>,
{
    if let WirInstr::ArrayClone {
        element_copy_func: Some(name),
        ..
    } = instr
        && let Some(idx) = resolve(name)
    {
        out.insert(idx);
    }
    instr.for_each_child(&mut |child| {
        collect_array_clone_helper_refs_recursive(child, resolve, out);
    });
}

fn collect_func_refs_recursive(instr: &WirInstr, out: &mut IndexSet<u32>) {
    match instr {
        WirInstr::Call { func_id, args } => {
            out.insert(func_id.index());
            for arg in args {
                collect_func_refs_recursive(arg, out);
            }
            return;
        }
        WirInstr::RefFunc { func_id } => {
            out.insert(func_id.index());
            return;
        }
        _ => {}
    }
    // For other variants, clone to use the mutable child traversal.
    // This is acceptable for the mem-module which has small function bodies.
    let mut clone = instr.clone();
    clone.for_each_boxed_child_mut(&mut |child| {
        collect_func_refs_recursive_mut(child, out);
    });
}

fn collect_func_refs_recursive_mut(instr: &mut WirInstr, out: &mut IndexSet<u32>) {
    match instr {
        WirInstr::Call { func_id, args } => {
            out.insert(func_id.index());
            for arg in args {
                collect_func_refs_recursive_mut(arg, out);
            }
            return;
        }
        WirInstr::RefFunc { func_id } => {
            out.insert(func_id.index());
            return;
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| {
        collect_func_refs_recursive_mut(child, out);
    });
}

fn remap_func_ids(instr: &mut WirInstr, remap: &IndexMap<u32, u32>) {
    match instr {
        WirInstr::Call { func_id, .. } => {
            if let Some(&new) = remap.get(&func_id.index()) {
                *func_id = WirFuncId::new(new, Rc::from(func_id.fq()));
            }
        }
        WirInstr::RefFunc { func_id } => {
            if let Some(&new) = remap.get(&func_id.index()) {
                *func_id = WirFuncId::new(new, Rc::from(func_id.fq()));
            }
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| {
        remap_func_ids(child, remap);
    });
}

/// Mark defined functions unreachable from exports/elements as dead
/// (`module.dead_func_indices`), so the subsequent `compact_dead_items`
/// removes them and remaps every `WirFuncId` reference.
///
/// Works on both the GC module (defined function indices start at
/// [`crate::wir_build::DEFINED_FUNC_BASE`]) and the memory module
/// (0-based indices) by reading the offset from
/// [`WirPackage::defined_func_base`].
///
/// Catches functions whose only call sites never materialized — most
/// notably the monomorphic `List<T>::push` (and its `List<T>::grow`
/// callee) for a single-element array literal `[v]`. NIR
/// `optimize::array_literal` rewrites `[v]` to `ExprKind::ArrayLiteral`,
/// which `wir_build` lowers to `array.new_fixed`, so the `push` chain is
/// never emitted and these instantiations have no callers.
///
/// This pass only sets dead flags; the actual removal + reindexing
/// happens in [`compact_dead_items`].
pub fn mark_unreachable_defined_functions(module: &mut WirPackage) {
    let num_funcs = u32::try_from(module.functions.len()).unwrap();
    if num_funcs == 0 {
        return;
    }

    // Translate an absolute Wasm function index (the value carried by
    // `WirFuncId`) into a 0-based index into `module.functions`. Returns
    // `None` for imported functions (whose index is below the base) and
    // for indices outside the defined-function range. Imports have no
    // body and cannot be DCE'd by this pass.
    let base = module.defined_func_base;
    let to_array_idx =
        |abs_idx: u32| -> Option<u32> { abs_idx.checked_sub(base).filter(|i| *i < num_funcs) };

    // `WirInstr::ArrayClone` references its per-element copy helper by
    // *name* (the `$value_copy$T<id>` synthesized by
    // `lower::plan::value_copy::synthesize`), not by `WirFuncId`, so the
    // generic body walker that follows `WirInstr::Call` / `RefFunc`
    // can't see the edge. Pre-build a `name-suffix → array index` map
    // so the per-instruction collector below can resolve those edges
    // and fold them into each function's callee set; without this the
    // helper is dropped by DCE, then the codegen call site can't
    // resolve it and panics.
    // Build the suffix → array-index map once. Helper names always have
    // the form `<module-prefix>/$value_copy$T<id>` and
    // `element_copy_func` carries the bare `$value_copy$T<id>` (the
    // last `/`-segment of `fq`), so an `O(1)` lookup is straightforward
    // — the previous `ends_with` linear scan was `O(#ArrayClone × #funcs)`.
    let helper_suffix_to_idx: IndexMap<String, u32> = module
        .functions
        .iter()
        .enumerate()
        .filter_map(|(i, func)| {
            let suffix = func
                .name
                .fq
                .rsplit('/')
                .next()
                .filter(|s| s.starts_with("$value_copy$"))?;
            Some((suffix.to_string(), u32::try_from(i).ok()?))
        })
        .collect();
    let resolve_helper_name =
        |name_suffix: &str| -> Option<u32> { helper_suffix_to_idx.get(name_suffix).copied() };

    let mut callees_of: Vec<IndexSet<u32>> = Vec::with_capacity(module.functions.len());
    for func in &module.functions {
        let mut callees: IndexSet<u32> = IndexSet::default();
        if let Some(body) = &func.body {
            let mut abs_callees: IndexSet<u32> = IndexSet::default();
            collect_func_refs_from_body(body, &mut abs_callees);
            for c in abs_callees {
                if let Some(arr_idx) = to_array_idx(c) {
                    callees.insert(arr_idx);
                }
            }
            collect_array_clone_helper_refs(body, &resolve_helper_name, &mut callees);
        }
        callees_of.push(callees);
    }

    // Roots: exports + element tables. Both reference functions via
    // `WirFuncId` (absolute index).
    let mut reachable: IndexSet<u32> = IndexSet::default();
    let mut queue: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
    let seed = |abs_idx: u32,
                reachable: &mut IndexSet<u32>,
                queue: &mut std::collections::VecDeque<u32>| {
        if let Some(arr_idx) = to_array_idx(abs_idx)
            && reachable.insert(arr_idx)
        {
            queue.push_back(arr_idx);
        }
    };
    for export in &module.exports {
        if let WirExportDesc::Func { func_id } = &export.desc {
            seed(func_id.index(), &mut reachable, &mut queue);
        }
    }
    for elem in &module.elements {
        for fid in &elem.func_ids {
            seed(fid.index(), &mut reachable, &mut queue);
        }
    }

    while let Some(idx) = queue.pop_front() {
        if let Some(callees) = callees_of.get(idx as usize) {
            for &c in callees {
                if reachable.insert(c) {
                    queue.push_back(c);
                }
            }
        }
    }

    for i in 0..num_funcs {
        if !reachable.contains(&i) {
            module.dead_func_indices.insert(i);
        }
    }
}

/// Mark types that are not referenced by any live function, import, or global
/// as dead by adding them to `module.dead_type_indices`.
///
/// After function DCE (at TIR level) and WIR optimization, some type definitions
/// may be registered but never actually used by any live code. This pass identifies
/// those orphan types and marks them so the emitter skips them.
///
/// Types are collected transitively: a struct field type, array element type,
/// or variant payload type that is only referenced from a dead type is also dead.
///
/// This is a standalone pass, separate from `optimize_wir`, so it can also run
/// at O0 if needed. Currently called at the end of `optimize_wir`.
pub fn dce_unreachable_types(module: &mut WirPackage) {
    let num_types = module.types.len();
    if num_types == 0 {
        return;
    }

    let mut reachable: IndexSet<u32> = IndexSet::default();

    // Seed: collect type indices referenced from live function signatures and bodies.
    // Skip functions that have been extracted into separate wasm_modules (dead_func_indices).
    for (i, func) in module.functions.iter().enumerate() {
        if module.dead_func_indices.contains(&(i as u32)) {
            continue;
        }
        reachable.insert(func.type_id.index());
        if let Some(body) = &func.body {
            for instr in body {
                collect_instr_type_refs(instr, &mut reachable);
            }
        }
    }

    // Seed: imports
    for import in &module.imports {
        match &import.desc {
            WirImportDesc::Func { type_id, .. } => {
                reachable.insert(type_id.index());
            }
            WirImportDesc::Global { ty, .. } | WirImportDesc::Table { ty, .. } => {
                collect_wir_type_ref(ty, &mut reachable);
            }
            WirImportDesc::Memory { .. } => {}
        }
    }

    // Seed: globals
    for global in &module.globals {
        collect_wir_type_ref(&global.ty, &mut reachable);
        collect_instr_type_refs(&global.init, &mut reachable);
    }

    // Transitive closure: for each reachable type, add the types it references.
    let mut queue: std::collections::VecDeque<u32> = reachable.iter().copied().collect();
    while let Some(idx) = queue.pop_front() {
        if (idx as usize) >= num_types {
            continue;
        }
        let mut refs: Vec<u32> = Vec::new();
        match &module.types[idx as usize] {
            WirTypeDef::Struct(s) => {
                for field in &s.fields {
                    collect_wir_type_ref_into(&field.ty, &mut refs);
                }
                // A struct that declares a supertype keeps the supertype
                // alive: Wasm GC requires the supertype to be defined for
                // the subtype declaration to validate.
                if let Some(super_id) = &s.supertype {
                    refs.push(super_id.index());
                }
            }
            WirTypeDef::Variant(v) => {
                // If a variant is reachable, its case struct types are also reachable.
                for (&case_wir_idx, &(variant_wir_idx, _)) in &module.variant_case_info {
                    if variant_wir_idx == idx {
                        refs.push(case_wir_idx);
                    }
                }
                for case in &v.cases {
                    for ty in &case.payload {
                        collect_wir_type_ref_into(ty, &mut refs);
                    }
                }
            }
            WirTypeDef::Array(a) => {
                collect_wir_type_ref_into(&a.element_type, &mut refs);
            }
            WirTypeDef::Func(ft) => {
                for ty in ft.params.iter().chain(ft.results.iter()) {
                    collect_wir_type_ref_into(ty, &mut refs);
                }
            }
            WirTypeDef::Enum(_) | WirTypeDef::Flags(_) => {}
        }
        for new_idx in refs {
            if reachable.insert(new_idx) {
                queue.push_back(new_idx);
            }
        }
    }

    // Mark unreachable types as dead.
    for i in 0..num_types as u32 {
        if !reachable.contains(&i) {
            module.dead_type_indices.insert(i);
        }
    }
}

/// Add the type index(es) referenced by a `WirType` to `out`.
fn collect_wir_type_ref(ty: &WirType, out: &mut IndexSet<u32>) {
    match ty {
        WirType::Ref { type_id, .. } | WirType::Enum { type_id } | WirType::Flags { type_id } => {
            out.insert(type_id.index());
        }
        _ => {}
    }
}

/// Like `collect_wir_type_ref` but collects into a `Vec` (for transitive closure work list).
fn collect_wir_type_ref_into(ty: &WirType, out: &mut Vec<u32>) {
    match ty {
        WirType::Ref { type_id, .. } | WirType::Enum { type_id } | WirType::Flags { type_id } => {
            out.push(type_id.index());
        }
        _ => {}
    }
}

/// Recursively collect all `WirTypeId` references from a `WirInstr` tree.
///
/// Handles the type-bearing instruction variants explicitly, then delegates
/// to `for_each_child` for recursive traversal of child instructions.
fn collect_instr_type_refs(instr: &WirInstr, out: &mut IndexSet<u32>) {
    match instr {
        WirInstr::StructNew { type_id, .. }
        | WirInstr::StructGet { type_id, .. }
        | WirInstr::StructSet { type_id, .. }
        | WirInstr::ArrayNew { type_id, .. }
        | WirInstr::ArrayNewDefault { type_id, .. }
        | WirInstr::ArrayNewData { type_id, .. }
        | WirInstr::ArrayNewFixed { type_id, .. }
        | WirInstr::ArrayGet { type_id, .. }
        | WirInstr::ArrayGetS { type_id, .. }
        | WirInstr::ArrayGetU { type_id, .. }
        | WirInstr::ArraySet { type_id, .. }
        | WirInstr::ArrayFill { type_id, .. }
        | WirInstr::RefCast { type_id, .. }
        | WirInstr::RefTest { type_id, .. }
        | WirInstr::CallIndirect { type_id, .. }
        | WirInstr::CallRef { type_id, .. } => {
            out.insert(type_id.index());
        }
        WirInstr::ArrayCopy {
            dest_type_id,
            src_type_id,
            ..
        } => {
            out.insert(dest_type_id.index());
            out.insert(src_type_id.index());
        }
        WirInstr::ArrayClone { type_id, .. } => {
            out.insert(type_id.index());
        }
        WirInstr::DeclareLocal { ty, .. } => {
            collect_wir_type_ref(ty, out);
        }
        WirInstr::Block { result, .. } | WirInstr::If { result, .. } => {
            if let Some(ty) = result {
                collect_wir_type_ref(ty, out);
            }
        }
        WirInstr::Select { ty, .. } => {
            if let Some(ty) = ty {
                collect_wir_type_ref(ty, out);
            }
        }
        _ => {}
    }
    // Recurse into child instructions.
    instr.for_each_child(&mut |child| {
        collect_instr_type_refs(child, out);
    });
}

/// Remove all dead-marked items from the module and remap their indices.
///
/// Consumes `dead_func_indices`, `dead_type_indices`, and `dead_global_indices`,
/// physically removing those items from the module's arrays and updating every
/// reference so the module is consistent and the emitter can emit it as-is.
///
/// - **Functions** (`dead_func_indices`): removed; `WirFuncId` values in all bodies,
///   exports, and element tables are remapped.
/// - **Types** (`dead_type_indices`): removed; `WirTypeId` values in all bodies,
///   function `type_ids`, import descriptors, globals, type definitions themselves,
///   and `variant_case_info` are remapped.
/// - **Globals** (`dead_global_indices`): removed from the globals array.
///   GlobalGet/GlobalSet use string names so no instruction patching is needed.
pub fn compact_dead_items(module: &mut WirPackage) {
    if module.dead_func_indices.is_empty()
        && module.dead_type_indices.is_empty()
        && module.dead_global_indices.is_empty()
    {
        return;
    }

    compact_funcs(module);
    compact_types(module);
    compact_globals(module);

    module.dead_func_indices.clear();
    module.dead_type_indices.clear();
    module.dead_global_indices.clear();
}

fn compact_funcs(module: &mut WirPackage) {
    if module.dead_func_indices.is_empty() {
        return;
    }

    // Build remap: old WirFuncId (base + i) → new WirFuncId (base + new_i).
    // The base is taken from `module.defined_func_base` so this works for
    // both the GC module (`DEFINED_FUNC_BASE`) and the memory module (`0`).
    let base = module.defined_func_base;
    let dead = &module.dead_func_indices;
    let mut remap: IndexMap<u32, u32> = IndexMap::default();
    let mut new_i = 0u32;
    for i in 0..u32::try_from(module.functions.len()).unwrap() {
        if !dead.contains(&i) {
            remap.insert(base + i, base + new_i);
            new_i += 1;
        }
    }

    // Filter functions
    let dead = module.dead_func_indices.clone();
    let old_functions: Vec<_> = module.functions.drain(..).enumerate().collect();
    module.functions = old_functions
        .into_iter()
        .filter(|(i, _)| !dead.contains(&u32::try_from(*i).unwrap()))
        .map(|(_, f)| f)
        .collect();

    // Remap WirFuncId in all function bodies
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body {
                remap_func_ids(instr, &remap);
            }
        }
    }

    // Remap exports
    for export in &mut module.exports {
        if let WirExportDesc::Func { func_id } = &mut export.desc
            && let Some(&new_id) = remap.get(&func_id.index())
        {
            *func_id = WirFuncId::new(new_id, Rc::from(func_id.fq()));
        }
    }

    // Remap elements
    for elem in &mut module.elements {
        for fid in &mut elem.func_ids {
            if let Some(&new_id) = remap.get(&fid.index()) {
                *fid = WirFuncId::new(new_id, Rc::from(fid.fq()));
            }
        }
    }
}

fn compact_types(module: &mut WirPackage) {
    if module.dead_type_indices.is_empty() {
        return;
    }

    // Build remap: old type index → new type index
    let dead = &module.dead_type_indices;
    let mut type_remap: IndexMap<u32, u32> = IndexMap::default();
    let mut new_idx = 0u32;
    for (i, _) in module.types.iter().enumerate() {
        let i = u32::try_from(i).unwrap();
        if !dead.contains(&i) {
            type_remap.insert(i, new_idx);
            new_idx += 1;
        }
    }

    // Filter types
    let dead = module.dead_type_indices.clone();
    let old_types: Vec<_> = module.types.drain(..).enumerate().collect();
    module.types = old_types
        .into_iter()
        .filter(|(i, _)| !dead.contains(&u32::try_from(*i).unwrap()))
        .map(|(_, t)| t)
        .collect();

    // Remap within type definitions themselves (struct fields, array element types, etc.)
    for typedef in &mut module.types {
        remap_typedef(typedef, &type_remap);
    }

    // Remap func.type_id in functions
    for func in &mut module.functions {
        remap_type_id(&mut func.type_id, &type_remap);
        if let Some(body) = &mut func.body {
            for instr in body {
                remap_type_ids_in_instr(instr, &type_remap);
            }
        }
    }

    // Remap import type references
    for import in &mut module.imports {
        match &mut import.desc {
            WirImportDesc::Func { type_id, .. } => remap_type_id(type_id, &type_remap),
            WirImportDesc::Global { ty, .. } | WirImportDesc::Table { ty, .. } => {
                remap_wir_type(ty, &type_remap);
            }
            WirImportDesc::Memory { .. } => {}
        }
    }

    // Remap global type references and init expressions
    for global in &mut module.globals {
        remap_wir_type(&mut global.ty, &type_remap);
        remap_type_ids_in_instr(&mut global.init, &type_remap);
    }

    // Remap variant_case_info: IndexMap<case_type_idx, (variant_type_idx, case_idx)>
    let old_vci: Vec<_> = module.variant_case_info.drain(..).collect();
    for (case_idx, (variant_idx, case_num)) in old_vci {
        if let (Some(&new_case), Some(&new_variant)) =
            (type_remap.get(&case_idx), type_remap.get(&variant_idx))
        {
            module
                .variant_case_info
                .insert(new_case, (new_variant, case_num));
        }
    }
}

fn compact_globals(module: &mut WirPackage) {
    if module.dead_global_indices.is_empty() {
        return;
    }
    let dead = module.dead_global_indices.clone();
    let old_globals: Vec<_> = module.globals.drain(..).enumerate().collect();
    module.globals = old_globals
        .into_iter()
        .filter(|(i, _)| !dead.contains(&u32::try_from(*i).unwrap()))
        .map(|(_, g)| g)
        .collect();
}

fn remap_type_id(type_id: &mut WirTypeId, remap: &IndexMap<u32, u32>) {
    if let Some(&new_idx) = remap.get(&type_id.index()) {
        *type_id = WirTypeId::new(new_idx, Rc::from(type_id.fq()));
    }
}

fn remap_wir_type(ty: &mut WirType, remap: &IndexMap<u32, u32>) {
    match ty {
        WirType::Enum { type_id } | WirType::Flags { type_id } | WirType::Ref { type_id, .. } => {
            remap_type_id(type_id, remap);
        }
        _ => {}
    }
}

fn remap_typedef(typedef: &mut WirTypeDef, remap: &IndexMap<u32, u32>) {
    match typedef {
        WirTypeDef::Struct(s) => {
            for field in &mut s.fields {
                remap_wir_type(&mut field.ty, remap);
            }
            if let Some(super_id) = &mut s.supertype {
                remap_type_id(super_id, remap);
            }
        }
        WirTypeDef::Variant(v) => {
            for case in &mut v.cases {
                for ty in &mut case.payload {
                    remap_wir_type(ty, remap);
                }
            }
        }
        WirTypeDef::Array(a) => {
            remap_wir_type(&mut a.element_type, remap);
        }
        WirTypeDef::Func(f) => {
            for ty in f.params.iter_mut().chain(f.results.iter_mut()) {
                remap_wir_type(ty, remap);
            }
        }
        WirTypeDef::Enum(_) | WirTypeDef::Flags(_) => {}
    }
}

fn remap_type_ids_in_instr(instr: &mut WirInstr, remap: &IndexMap<u32, u32>) {
    match instr {
        WirInstr::StructNew { type_id, .. }
        | WirInstr::StructGet { type_id, .. }
        | WirInstr::StructSet { type_id, .. }
        | WirInstr::ArrayNew { type_id, .. }
        | WirInstr::ArrayNewDefault { type_id, .. }
        | WirInstr::ArrayNewData { type_id, .. }
        | WirInstr::ArrayNewFixed { type_id, .. }
        | WirInstr::ArrayGet { type_id, .. }
        | WirInstr::ArrayGetS { type_id, .. }
        | WirInstr::ArrayGetU { type_id, .. }
        | WirInstr::ArraySet { type_id, .. }
        | WirInstr::ArrayFill { type_id, .. }
        | WirInstr::RefCast { type_id, .. }
        | WirInstr::RefTest { type_id, .. }
        | WirInstr::CallIndirect { type_id, .. }
        | WirInstr::CallRef { type_id, .. } => {
            remap_type_id(type_id, remap);
        }
        WirInstr::ArrayCopy {
            dest_type_id,
            src_type_id,
            ..
        } => {
            remap_type_id(dest_type_id, remap);
            remap_type_id(src_type_id, remap);
        }
        WirInstr::ArrayClone { type_id, .. } => {
            remap_type_id(type_id, remap);
        }
        WirInstr::DeclareLocal { ty, .. } => {
            remap_wir_type(ty, remap);
        }
        WirInstr::Block { result, .. } | WirInstr::If { result, .. } => {
            if let Some(ty) = result {
                remap_wir_type(ty, remap);
            }
        }
        WirInstr::Select { ty, .. } => {
            if let Some(ty) = ty {
                remap_wir_type(ty, remap);
            }
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| {
        remap_type_ids_in_instr(child, remap);
    });
}
