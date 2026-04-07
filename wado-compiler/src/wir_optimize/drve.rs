//! Dead Return Value Elimination (DRVE) pass for WIR.
//!
//! Converts functions whose return value is always immediately dropped at every
//! call site to void return, eliminating the GC struct allocation in the return
//! and the `drop` at call sites.

use crate::hashmap::IndexSet;
use crate::wir::{WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId};
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

use super::util::{collect_pinned_func_ids, is_side_effect_free};

/// Dead Return Value Elimination (DRVE).
///
/// Finds non-pinned functions that return a GC struct ref where:
/// 1. Every return in the body is `Return { value: StructNew { pure_fields } }`.
/// 2. Every call site in the module is `Drop(Call(f, args))`.
///
/// Those functions are converted to void return, the `StructNew` is removed from
/// returns, and the Drop wrapper is removed at each call site.
pub(super) fn eliminate_dead_return_values(module: &mut WirPackage) {
    let pinned = collect_pinned_func_ids(module);

    let candidates = find_drve_candidates(module, &pinned);
    if candidates.is_empty() {
        return;
    }

    let confirmed = validate_drve_call_sites(module, &candidates);
    if confirmed.is_empty() {
        return;
    }

    apply_drve(module, &confirmed);
}

struct DrveCandidate {
    func_array_idx: usize,
}

fn find_drve_candidates(module: &WirPackage, pinned: &IndexSet<u32>) -> Vec<(u32, DrveCandidate)> {
    let mut candidates = Vec::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();

        if pinned.contains(&func_id_index) {
            continue;
        }
        if func.body.is_none() {
            continue;
        }

        let type_idx = func.type_id.index() as usize;
        let Some(WirTypeDef::Func(func_type)) = module.types.get(type_idx) else {
            continue;
        };

        // Must return exactly one Ref to a struct.
        if func_type.results.len() != 1 {
            continue;
        }
        let WirType::Ref {
            type_id: ret_type_id,
            ..
        } = &func_type.results[0]
        else {
            continue;
        };
        let ret_type_idx = ret_type_id.index() as usize;
        let Some(WirTypeDef::Struct(_)) = module.types.get(ret_type_idx) else {
            continue;
        };

        // All returns must be StructNew with all-pure field expressions.
        let body = func.body.as_ref().unwrap();
        let mut checker = AllReturnsPureStructNew { result: true };
        checker.visit_body(body);
        if !checker.result {
            continue;
        }

        candidates.push((func_id_index, DrveCandidate { func_array_idx: i }));
    }

    candidates
}

struct AllReturnsPureStructNew {
    result: bool,
}

impl WirRefVisitor for AllReturnsPureStructNew {
    fn visit_instr(&mut self, instr: &WirInstr) {
        if !self.result {
            return;
        }
        match instr {
            WirInstr::Return { value: Some(v) } => {
                let WirInstr::StructNew { fields, .. } = v.as_ref() else {
                    self.result = false;
                    return;
                };
                if !fields.iter().all(is_side_effect_free) {
                    self.result = false;
                }
            }
            WirInstr::Return { value: None } => {
                self.result = false;
            }
            _ => {}
        }
        self.walk_instr(instr);
    }
}

fn validate_drve_call_sites(
    module: &WirPackage,
    candidates: &[(u32, DrveCandidate)],
) -> Vec<(u32, DrveCandidate)> {
    let candidate_ids: IndexSet<u32> = candidates.iter().map(|(id, _)| *id).collect();
    let mut invalid: IndexSet<u32> = IndexSet::default();
    let mut has_drop_call: IndexSet<u32> = IndexSet::default();

    for func in &module.functions {
        if let Some(body) = &func.body {
            let mut checker = CheckDrveCallUses {
                candidate_ids: &candidate_ids,
                invalid: &mut invalid,
                has_drop_call: &mut has_drop_call,
            };
            checker.visit_body(body);
        }
    }

    candidates
        .iter()
        .filter(|(id, _)| has_drop_call.contains(id) && !invalid.contains(id))
        .map(|(id, c)| {
            (
                *id,
                DrveCandidate {
                    func_array_idx: c.func_array_idx,
                },
            )
        })
        .collect()
}

struct CheckDrveCallUses<'a> {
    candidate_ids: &'a IndexSet<u32>,
    invalid: &'a mut IndexSet<u32>,
    has_drop_call: &'a mut IndexSet<u32>,
}

impl WirRefVisitor for CheckDrveCallUses<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::Drop(inner) => {
                if let WirInstr::Call { func_id, args } = inner.as_ref()
                    && self.candidate_ids.contains(&func_id.index())
                {
                    self.has_drop_call.insert(func_id.index());
                    for arg in args {
                        find_drve_candidate_calls(arg, self.candidate_ids, self.invalid);
                    }
                    return;
                }
                find_drve_candidate_calls(inner, self.candidate_ids, self.invalid);
            }
            _ => {
                find_drve_candidate_calls(instr, self.candidate_ids, self.invalid);
            }
        }
        self.walk_instr(instr);
    }
}

fn find_drve_candidate_calls(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    if let WirInstr::Call { func_id, .. } = instr
        && candidate_ids.contains(&func_id.index())
    {
        invalid.insert(func_id.index());
        return;
    }
    instr.for_each_child(&mut |child| {
        find_drve_candidate_calls(child, candidate_ids, invalid);
    });
}

fn apply_drve(module: &mut WirPackage, confirmed: &[(u32, DrveCandidate)]) {
    let candidate_set: IndexSet<u32> = confirmed.iter().map(|(id, _)| *id).collect();

    // Step A: Change function return types to void and rewrite returns.
    for (_, candidate) in confirmed {
        let func = &mut module.functions[candidate.func_array_idx];

        let old_type_idx = func.type_id.index() as usize;
        let old_func_type = match &module.types[old_type_idx] {
            WirTypeDef::Func(ft) => ft,
            _ => unreachable!(),
        };
        let new_func_type = WirFuncType {
            name: old_func_type.name.clone(),
            params: old_func_type.params.clone(),
            results: vec![],
        };

        let new_type_idx = u32::try_from(module.types.len()).unwrap();
        module.types.push(WirTypeDef::Func(new_func_type));

        let new_type_id = WirTypeId::new(new_type_idx, func.type_id.fq().into());
        func.type_id = new_type_id;

        if let Some(body) = &mut func.body {
            let mut rewriter = RewriteDrveReturns;
            rewriter.visit_body(body);
        }
    }

    // Step B: Remove Drop wrappers at call sites.
    for i in 0..module.functions.len() {
        if module.functions[i].body.is_some() {
            let body = module.functions[i].body.as_mut().unwrap();
            let mut rewriter = RewriteDrveCallSites {
                candidate_set: &candidate_set,
            };
            rewriter.visit_body(body);
        }
    }
}

struct RewriteDrveReturns;

impl WirMutVisitor for RewriteDrveReturns {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        if let WirInstr::Return { value: Some(v) } = instr
            && matches!(v.as_ref(), WirInstr::StructNew { .. })
        {
            *instr = WirInstr::Return { value: None };
            return;
        }
        self.walk_instr(instr);
    }
}

struct RewriteDrveCallSites<'a> {
    candidate_set: &'a IndexSet<u32>,
}

impl WirMutVisitor for RewriteDrveCallSites<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        if let WirInstr::Drop(inner) = instr
            && matches!(inner.as_ref(), WirInstr::Call { func_id, .. }
                if self.candidate_set.contains(&func_id.index()))
        {
            let call = std::mem::replace(inner.as_mut(), WirInstr::Nop);
            *instr = call;
        }
        self.walk_instr(instr);
    }
}
