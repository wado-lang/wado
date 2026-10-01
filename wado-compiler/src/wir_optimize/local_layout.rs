//! Local layout: the order the emitter numbers a function's locals in. Wasm
//! declares locals as runs of one type, and a local index under 128 encodes in
//! one byte, so the locals sit grouped by Wasm type, the most used group first
//! and the most used local first within it. A local nothing reads or writes is
//! not declared.

use crate::hashmap::IndexMap;
use crate::wir::{WirFunction, WirInstr, WirLocals, WirScalarKind, WirType};
use crate::wir_visitor::WirRefVisitor;

/// The function's declared locals in emission order, unreferenced ones left out.
pub(super) fn lay_out_locals(func: &WirFunction) -> WirLocals {
    let mut uses = UseCount::default();
    uses.walk_body(func.body.as_deref().unwrap_or(&[]));

    let mut groups: Vec<Group> = Vec::new();
    for (name, ty) in func.declared_locals().iter() {
        let Some(&count) = uses.0.get(name) else {
            continue;
        };
        let class = WasmClass::of(ty);
        let member = (name.to_string(), ty.clone(), count);
        match groups.iter_mut().find(|group| group.class == class) {
            Some(group) => {
                group.uses += count;
                group.members.push(member);
            }
            None => groups.push(Group {
                class,
                uses: count,
                members: vec![member],
            }),
        }
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.uses));
    groups
        .into_iter()
        .flat_map(|mut group| {
            group
                .members
                .sort_by_key(|(_, _, count)| std::cmp::Reverse(*count));
            group.members.into_iter().map(|(name, ty, _)| (name, ty))
        })
        .collect()
}

struct Group {
    class: WasmClass,
    uses: u32,
    members: Vec<(String, WirType, u32)>,
}

/// What a local's Wasm declaration says: the scalar a signedness or nominal
/// type erases to, or the reference type itself.
#[derive(PartialEq)]
pub(super) enum WasmClass {
    Scalar(WirScalarKind),
    Reference(WirType),
}

impl WasmClass {
    pub(super) fn of(ty: &WirType) -> Self {
        match ty.scalar_kind() {
            Some(kind) => Self::Scalar(kind),
            None => Self::Reference(ty.clone()),
        }
    }
}

/// How many times each local is read or written.
#[derive(Default)]
struct UseCount(IndexMap<String, u32>);

impl UseCount {
    fn count(&mut self, name: &str) {
        match self.0.get_mut(name) {
            Some(count) => *count += 1,
            None => {
                self.0.insert(name.to_string(), 1);
            }
        }
    }
}

impl WirRefVisitor for UseCount {
    fn visit_instr(&mut self, instr: &WirInstr) {
        for name in instr.local_read().into_iter().chain(instr.local_writes()) {
            self.count(name);
        }
        self.walk_instr(instr);
    }
}
