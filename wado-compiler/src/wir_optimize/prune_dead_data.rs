//! Removes passive data segments with no surviving `array.new_data` reference.
//!
//! `register_literal_data` conservatively registers a segment for every
//! string/bytes literal whose payload exceeds `string_inline_max_bytes`,
//! before later per-occurrence decisions (a bounded force-eager override for
//! `const_object_globalization`-hoisted globals, dead-store elimination in
//! `promote_const_global_inits`) settle whether any occurrence of that
//! payload actually ends up reading the segment. A payload that ends up
//! wholly represented via `array.new_fixed` leaves its registered segment
//! referenced by nothing. This pass finds those and drops them, then
//! compacts and remaps the surviving `data_index`s.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage};

pub(super) fn prune_dead_data(module: &mut WirPackage) {
    if module.data.is_empty() {
        return;
    }

    let mut used: IndexSet<u32> = IndexSet::default();
    for func in &module.functions {
        if let Some(body) = &func.body {
            for instr in body {
                collect_data_refs(instr, &mut used);
            }
        }
    }
    for global in &module.globals {
        collect_data_refs(&global.init, &mut used);
    }

    if used.len() == module.data.len() {
        return;
    }

    let mut remap: IndexMap<u32, u32> = IndexMap::default();
    let mut new_data = Vec::with_capacity(used.len());
    for (old_idx, segment) in module.data.drain(..).enumerate() {
        let old_idx = u32::try_from(old_idx).unwrap();
        if used.contains(&old_idx) {
            remap.insert(old_idx, u32::try_from(new_data.len()).unwrap());
            new_data.push(segment);
        }
    }
    module.data = new_data;

    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body {
                remap_data_refs(instr, &remap);
            }
        }
    }
    for global in &mut module.globals {
        remap_data_refs(&mut global.init, &remap);
    }
}

fn collect_data_refs(instr: &WirInstr, out: &mut IndexSet<u32>) {
    if let WirInstr::ArrayNewData { data_index, .. } = instr {
        out.insert(*data_index);
    }
    instr.for_each_child(&mut |child| collect_data_refs(child, out));
}

fn remap_data_refs(instr: &mut WirInstr, remap: &IndexMap<u32, u32>) {
    if let WirInstr::ArrayNewData { data_index, .. } = instr
        && let Some(&new_idx) = remap.get(data_index)
    {
        *data_index = new_idx;
    }
    instr.for_each_boxed_child_mut(&mut |child| remap_data_refs(child, remap));
}
