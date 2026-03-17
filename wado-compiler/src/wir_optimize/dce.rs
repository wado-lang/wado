//! Dead Code Elimination (DCE) passes for WIR.
//!
//! - **`dce_unreachable_functions`**: removes functions not reachable from exports.
//! - **`dce_unreachable_types`**: marks GC types not referenced by any live code as dead.

use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{
    WirExportDesc, WirFuncId, WirImportDesc, WirInstr, WirModule, WirType, WirTypeDef, WirTypeId,
};

/// Remove functions unreachable from exports via call-graph analysis.
///
/// Performs BFS from exported functions and element-referenced functions,
/// removing any function not transitively called. Remaps all `WirFuncId`
/// indices so the surviving functions are contiguously numbered.
///
/// This is a standalone pass that works on any `WirModule` (GC module or
/// memory module). It is separate from `optimize_wir` because it should
/// also run on modules that skip the main optimization pipeline (e.g.,
/// the linear-memory module built by `WasmModuleInfo::to_wir_module`).
pub fn dce_unreachable_functions(module: &mut WirModule) {
    let num_funcs = module.functions.len();
    if num_funcs == 0 {
        return;
    }

    // Collect root function indices: exported + element-referenced
    let mut roots: IndexSet<u32> = IndexSet::default();
    for export in &module.exports {
        if let WirExportDesc::Func { func_id } = &export.desc {
            roots.insert(func_id.index());
        }
    }
    for elem in &module.elements {
        for fid in &elem.func_ids {
            roots.insert(fid.index());
        }
    }

    // Build call graph: function index → set of callee indices
    let mut callees_of: Vec<IndexSet<u32>> = Vec::with_capacity(num_funcs);
    for func in &module.functions {
        let mut callees = IndexSet::default();
        if let Some(body) = &func.body {
            collect_func_refs_from_body(body, &mut callees);
        }
        callees_of.push(callees);
    }

    // BFS from roots
    let mut reachable: IndexSet<u32> = IndexSet::default();
    let mut queue = std::collections::VecDeque::new();
    for &root in &roots {
        if reachable.insert(root) {
            queue.push_back(root);
        }
    }
    while let Some(idx) = queue.pop_front() {
        if let Some(callees) = callees_of.get(idx as usize) {
            for &callee in callees {
                if reachable.insert(callee) {
                    queue.push_back(callee);
                }
            }
        }
    }

    if reachable.len() == num_funcs {
        return; // nothing to remove
    }

    // Build old→new index remap
    let mut remap: IndexMap<u32, u32> = IndexMap::default();
    let mut new_idx = 0u32;
    for old_idx in 0..num_funcs as u32 {
        if reachable.contains(&old_idx) {
            remap.insert(old_idx, new_idx);
            new_idx += 1;
        }
    }

    // Filter functions and types (1:1 correspondence)
    let mut new_functions = Vec::with_capacity(reachable.len());
    let mut new_types = Vec::with_capacity(reachable.len());
    for (i, (func, type_def)) in module
        .functions
        .drain(..)
        .zip(module.types.drain(..))
        .enumerate()
    {
        if reachable.contains(&(i as u32)) {
            new_functions.push(func);
            new_types.push(type_def);
        }
    }
    module.functions = new_functions;
    module.types = new_types;

    // Remap func IDs in function bodies
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body {
                remap_func_ids(instr, &remap);
            }
        }
        // Remap type_id (same indexing as functions for mem-module)
        if let Some(&new) = remap.get(&func.type_id.index()) {
            func.type_id = WirTypeId::new(new, Rc::from(func.type_id.fq()));
        }
    }

    // Remap exports
    for export in &mut module.exports {
        if let WirExportDesc::Func { func_id } = &mut export.desc
            && let Some(&new) = remap.get(&func_id.index())
        {
            *func_id = WirFuncId::new(new, Rc::from(func_id.fq()));
        }
    }

    // Remap elements
    for elem in &mut module.elements {
        for fid in &mut elem.func_ids {
            if let Some(&new) = remap.get(&fid.index()) {
                *fid = WirFuncId::new(new, Rc::from(fid.fq()));
            }
        }
    }

    // Remap name section
    module.names.function_names.retain_mut(|(idx, _)| {
        if let Some(&new) = remap.get(idx) {
            *idx = new;
            true
        } else {
            false
        }
    });
}

fn collect_func_refs_from_body(body: &[WirInstr], out: &mut IndexSet<u32>) {
    for instr in body {
        collect_func_refs_recursive(instr, out);
    }
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
pub fn dce_unreachable_types(module: &mut WirModule) {
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
        WirType::Ref { type_id, .. }
        | WirType::Enum { type_id }
        | WirType::Flags { type_id } => {
            out.insert(type_id.index());
        }
        _ => {}
    }
}

/// Like `collect_wir_type_ref` but collects into a `Vec` (for transitive closure work list).
fn collect_wir_type_ref_into(ty: &WirType, out: &mut Vec<u32>) {
    match ty {
        WirType::Ref { type_id, .. }
        | WirType::Enum { type_id }
        | WirType::Flags { type_id } => {
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
        | WirInstr::CallRef { type_id, .. }
        | WirInstr::ValueCopy { type_id, .. }
        | WirInstr::MultiValueStructNew { type_id, .. } => {
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
