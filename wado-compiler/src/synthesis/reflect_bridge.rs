//! Post-monomorphize reflect bridge synthesis.
//!
//! Every other synthesis phase runs before monomorphization. The reflect value
//! bridges cannot: lowering rewrites `builtin::struct_field_get::<S, F>` to a
//! helper named after the *concrete* struct and field mangles, and fields that
//! share an erased field type share one index-dispatched helper. For a generic
//! struct that grouping is only knowable per instantiation — `Pair<i32>` merges
//! `left: T` and `right: i32` into one helper where `Pair<String>` keeps them
//! apart — so the helpers are minted here, once each monomorphized struct
//! exists (WEP 2026-06-13).

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::tir::TypeId;

use super::traits::{REFLECT_MEMBERS_ASSOC, generate_field_bridge_helpers};

/// Mint the `$field_get$S$F` bridges of every monomorphized struct whose
/// generic base carries a synthesized `ReflectStruct` impl.
pub fn synthesize_monomorphized_reflect_bridges(flat: &mut FlatPackage) {
    let targets: Vec<(String, crate::module_source::ModuleSource, String)> = {
        let tt = flat.type_table.borrow();
        flat.structs
            .iter()
            .filter_map(|s| {
                let base = &s.monomorph_info.as_ref()?.generic_name;
                tt.has_generic_assoc_type_def(base, REFLECT_MEMBERS_ASSOC)
                    .then(|| (s.name.clone(), s.module_source.clone(), base.clone()))
            })
            .collect()
    };
    if targets.is_empty() {
        return;
    }

    let mut generated = Vec::new();
    for (name, module_source, base_name) in targets {
        let Some(decl) = flat
            .structs
            .iter()
            .find(|s| s.name == name && s.module_source == module_source)
        else {
            continue;
        };
        let fields: Vec<(String, TypeId, u32)> = decl
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.type_id, f.index))
            .collect();
        let span = decl.span;

        let (struct_type, ref_struct_type) = {
            let mut tt = flat.type_table.borrow_mut();
            let struct_type =
                tt.make_monomorphized_struct(name.clone(), module_source.clone(), base_name);
            let ref_struct_type = tt.make_ref(struct_type);
            (struct_type, ref_struct_type)
        };

        for mut helper in generate_field_bridge_helpers(
            &flat.type_table,
            &fields,
            struct_type,
            ref_struct_type,
            span,
        ) {
            // `link` — which assigns each synthesized function its home module —
            // has already run, so the module is set here instead.
            helper.module_source = module_source.clone();
            generated.push(Rc::new(RefCell::new(helper)));
        }
    }

    flat.functions.extend(generated);
}
