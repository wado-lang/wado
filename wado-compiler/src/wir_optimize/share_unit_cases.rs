//! Build each payload-less case of a boxed variant once. `Option<i64>::None`
//! is an object holding nothing but its discriminant, and every variant field
//! is immutable, so one instance per case serves every construction: each
//! `struct.new` of one becomes a `global.get` of a global holding it. A
//! `ref_eq` between two of them turns true, which the specification allows.
//!
//! It runs after the passes that scalarize a variant built where it is
//! consumed (`sroa_variant_return`), which key on the `struct.new` this
//! removes.

use crate::hashmap::IndexMap;
use crate::name::unit_case_global_name;
use crate::wir::{WirGlobal, WirInstr, WirMeta, WirName, WirPackage, WirType, WirTypeDef, WirTypeId};
use crate::wir_visitor::WirMutVisitor;

pub(super) fn share_unit_cases(module: &mut WirPackage) {
    let mut sharer = Sharer {
        types: &module.types,
        shared: IndexMap::default(),
    };
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            sharer.visit_body(body);
        }
    }
    let shared = sharer.shared;
    for ((type_id, case), name) in shared {
        module.globals.push(WirGlobal {
            name: WirName { fq: name },
            ty: WirType::non_null_ref(type_id.clone()),
            mutable: false,
            wado_mutable: false,
            init: WirInstr::StructNew {
                type_id,
                fields: vec![WirInstr::I32Const(case)],
            },
            meta: WirMeta::default(),
        });
    }
}

struct Sharer<'a> {
    types: &'a [WirTypeDef],
    /// The global minted for each (variant, discriminant) pair.
    shared: IndexMap<(WirTypeId, i32), String>,
}

impl Sharer<'_> {
    /// The variant and discriminant of a payload-less case built by `instr`.
    fn unit_case(&self, instr: &WirInstr) -> Option<(WirTypeId, i32)> {
        let WirInstr::StructNew { type_id, fields } = instr else {
            return None;
        };
        let WirTypeDef::Variant(_) = &self.types[type_id.index() as usize] else {
            return None;
        };
        let [WirInstr::I32Const(case)] = fields.as_slice() else {
            return None;
        };
        Some((type_id.clone(), *case))
    }
}

impl WirMutVisitor for Sharer<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        let Some(key) = self.unit_case(instr) else {
            self.walk_instr(instr);
            return;
        };
        let name = self
            .shared
            .entry(key.clone())
            .or_insert_with(|| unit_case_global_name(key.0.fq(), key.1))
            .clone();
        *instr = WirInstr::GlobalGet {
            name: WirName { fq: name },
            result_ty: WirType::non_null_ref(key.0),
        };
    }
}
