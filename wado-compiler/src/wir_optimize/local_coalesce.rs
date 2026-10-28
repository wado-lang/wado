//! Local coalescing: locals of one Wasm type that never hold values at once
//! share one slot, as a register allocator shares a register. A `bool` and a
//! `u32` are both an `i32` there, so they may share. Two locals interfere when
//! one is written where the other is live ([`Flow`]), except that a copy
//! `x = y` leaves `x` and `y` free to share, holding one value. A local live
//! at the entry holds the zero Wasm starts it at, so it is written there, as
//! each parameter is with its argument. A parameter keeps its slot and can
//! absorb a local that never overlaps it.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirLocals, WirPackage, WirType};
use crate::wir_optimize::local_layout::WasmClass;
use crate::wir_optimize::local_nullability::relax_unset_nonnull_locals_in;
use crate::wir_visitor::WirMutVisitor;

use super::liveness::{Flow, Observer, Store};
use super::util::Bits;

/// Rename each local to the slot it shares, dropping the declarations and the
/// self-copies the renaming leaves.
pub(super) fn coalesce_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let func_type = module.types[func.type_id.index() as usize].expect_func();
        let params: Vec<(&str, &WirType)> = func
            .param_names
            .iter()
            .map(String::as_str)
            .zip(&func_type.params)
            .collect();
        coalesce_body(body, &params);
    }
}

/// A slot holds one Wasm type, so each local's nullability is settled before
/// any shares one.
fn coalesce_body(body: &mut Vec<WirInstr>, params: &[(&str, &WirType)]) {
    relax_unset_nonnull_locals_in(body);
    let renames = plan_slots(body, params);
    if !renames.is_empty() {
        Rename { renames: &renames }.visit_body(body);
    }
}

/// Each local merged into another slot, mapped to that slot's name.
fn plan_slots(body: &[WirInstr], params: &[(&str, &WirType)]) -> IndexMap<String, String> {
    let declared = WirLocals::scan(body);
    let param_names: IndexSet<&str> = params.iter().map(|&(name, _)| name).collect();
    assert!(
        param_names.iter().all(|name| declared.get(name).is_none()),
        "[WIR] a local is declared under a parameter's name"
    );
    let flow = Flow::of(body);
    let locals = &flow.locals;
    let class: Vec<WasmClass> = locals
        .iter()
        .map(|name| {
            let ty = match param_names.get_index_of(name.as_str()) {
                Some(i) => params[i].1,
                None => declared.get(name).unwrap_or_else(|| {
                    panic!("[WIR] `{name}` is accessed but neither declared nor a parameter")
                }),
            };
            WasmClass::of(ty)
        })
        .collect();
    let is_param = |local: usize| param_names.contains(locals[local].as_str());
    let interference = Interference::of(&flow, is_param);

    // Parameters first, as they own their slots; then the locals in the order
    // they are first accessed.
    let mut order: Vec<usize> = (0..locals.len()).filter(|&l| is_param(l)).collect();
    order.sort_by_key(|&l| param_names.get_index_of(locals[l].as_str()));
    order.extend((0..locals.len()).filter(|&l| !is_param(l)));

    let mut slots: Vec<Slot> = Vec::new();
    let mut slot_of: Vec<Option<usize>> = vec![None; locals.len()];
    let mut renames = IndexMap::default();
    for local in order {
        let fits = |slot: &Slot| slot.class == class[local] && !slot.interference.contains(local);
        // A slot holding a copy's other end first, so the copy goes.
        let shared = if is_param(local) {
            None
        } else {
            interference.copies[local]
                .iter()
                .filter_map(|&partner| slot_of[partner])
                .chain(0..slots.len())
                .find(|&s| fits(&slots[s]))
        };
        let slot = if let Some(s) = shared {
            renames.insert(locals[local].clone(), locals[slots[s].owner].clone());
            s
        } else {
            slots.push(Slot {
                class: class[local].clone(),
                owner: local,
                interference: Bits::new(locals.len()),
            });
            slots.len() - 1
        };
        slots[slot].interference.union(&interference.edges[local]);
        slot_of[local] = Some(slot);
    }
    renames
}

struct Slot {
    class: WasmClass,
    owner: usize,
    /// The locals any member interferes with.
    interference: Bits,
}

/// Which locals hold values at once, and which a copy joins.
struct Interference {
    edges: Vec<Bits>,
    /// The locals each local is copied to or from.
    copies: Vec<Vec<usize>>,
}

impl Interference {
    fn of(flow: &Flow, is_param: impl Fn(usize) -> bool) -> Self {
        let width = flow.locals.len();
        let mut found = Self {
            edges: vec![Bits::new(width); width],
            copies: vec![Vec::new(); width],
        };
        let entry = flow.run(&mut found);
        // Every parameter holds its argument at the entry, and every local live
        // there its zero.
        for local in (0..width).filter(|&l| is_param(l) || entry.contains(l)) {
            let mut others = entry.clone();
            others.remove(local);
            found.edges[local].union(&others);
        }
        let one_way = found.edges.clone();
        for (local, edges) in one_way.iter().enumerate() {
            for other in edges.iter() {
                found.edges[other].insert(local);
            }
        }
        found
    }
}

impl Observer for Interference {
    fn store(&mut self, store: &Store, live_after: &Bits) {
        match store {
            Store::One { local, copy_of, .. } => {
                let mut others = live_after.clone();
                others.remove(*local);
                if let Some(source) = *copy_of {
                    others.remove(source);
                    if source != *local {
                        self.copies[*local].push(source);
                        self.copies[source].push(*local);
                    }
                }
                self.edges[*local].union(&others);
            }
            Store::Bind(locals) => {
                let mut written = live_after.clone();
                for &local in locals {
                    written.insert(local);
                }
                for &local in locals {
                    let mut others = written.clone();
                    others.remove(local);
                    self.edges[local].union(&others);
                }
            }
        }
    }

    // A loop pass whose head then moves sees less live than the final pass
    // does, so what it records the final pass records too.
    fn mark(&self) -> usize {
        0
    }

    fn rollback(&mut self, _: usize) {}
}

struct Rename<'a> {
    renames: &'a IndexMap<String, String>,
}

impl WirMutVisitor for Rename<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        body.retain(|instr| !matches!(instr, WirInstr::Nop));
    }

    fn visit_instr(&mut self, instr: &mut WirInstr) {
        self.walk_instr(instr);
        let renames = self.renames;
        if let WirInstr::DeclareLocal { name, .. } = instr
            && renames.contains_key(name.as_str())
        {
            *instr = WirInstr::Nop;
            return;
        }
        instr.for_each_local_name_mut(|name| {
            if let Some(slot) = renames.get(name.as_str()) {
                name.clone_from(slot);
            }
        });
        let self_copy = matches!(
            instr,
            WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value }
                if matches!(&**value, WirInstr::LocalGet { name: source, .. } if source == name)
        );
        if self_copy {
            *instr = match instr {
                WirInstr::LocalTee { value, .. } => std::mem::replace(&mut **value, WirInstr::Nop),
                _ => WirInstr::Nop,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::{WirAbstractHeapType, WirTypeId};

    fn decl(name: &str, ty: WirType) -> WirInstr {
        WirInstr::DeclareLocal {
            name: name.to_string(),
            ty,
        }
    }

    fn set(name: &str, value: WirInstr) -> WirInstr {
        WirInstr::LocalSet {
            name: name.to_string(),
            value: Box::new(value),
        }
    }

    fn get(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }

    fn use_of(name: &str) -> WirInstr {
        WirInstr::Drop(Box::new(get(name)))
    }

    fn renames(body: &[WirInstr], params: &[(&str, &WirType)]) -> Vec<(String, String)> {
        plan_slots(body, params).into_iter().collect()
    }

    fn merged(from: &str, into: &str) -> (String, String) {
        (from.to_string(), into.to_string())
    }

    #[test]
    fn disjoint_locals_share_a_slot() {
        let body = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
            set("b", WirInstr::I32Const(2)),
            use_of("b"),
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }

    #[test]
    fn locals_of_one_wasm_type_share_a_slot() {
        let body = [
            decl("a", WirType::Bool),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
            set("b", WirInstr::I32Const(2)),
            use_of("b"),
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }

    #[test]
    fn overlapping_locals_or_types_keep_their_slots() {
        let overlapping = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            set("b", WirInstr::I32Const(2)),
            use_of("a"),
            use_of("b"),
        ];
        assert_eq!(renames(&overlapping, &[]), []);
        let typed = [
            decl("a", WirType::I32),
            decl("b", WirType::I64),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
            set("b", WirInstr::I64Const(2)),
            use_of("b"),
        ];
        assert_eq!(renames(&typed, &[]), []);
    }

    /// `x` is read at the top of each iteration, so it is live over the whole
    /// loop and `y` cannot take it.
    #[test]
    fn a_value_crossing_the_back_edge_holds_its_slot_over_the_loop() {
        let body = [
            decl("x", WirType::I32),
            decl("y", WirType::I32),
            set("x", WirInstr::I32Const(0)),
            WirInstr::Loop {
                label: None,
                body: vec![
                    use_of("x"),
                    set("y", WirInstr::I32Const(1)),
                    use_of("y"),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(WirInstr::I32Const(1)),
                    },
                ],
            },
        ];
        assert_eq!(renames(&body, &[]), []);
    }

    /// Both locals are written first, at the loop's top level, every
    /// iteration: neither value crosses the back edge.
    #[test]
    fn locals_written_fresh_each_iteration_share_a_slot() {
        let body = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            WirInstr::Loop {
                label: None,
                body: vec![
                    set("a", WirInstr::I32Const(1)),
                    use_of("a"),
                    set("b", WirInstr::I32Const(2)),
                    use_of("b"),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(WirInstr::I32Const(1)),
                    },
                ],
            },
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }

    fn branch_writing(name: &str, value: i32) -> Vec<WirInstr> {
        vec![set(name, WirInstr::I32Const(value))]
    }

    #[test]
    fn a_local_written_on_every_path_starts_at_its_write() {
        let body = [
            decl("z", WirType::I32),
            decl("a", WirType::I32),
            set("z", WirInstr::I32Const(5)),
            use_of("z"),
            WirInstr::If {
                condition: Box::new(WirInstr::I32Const(1)),
                then_body: branch_writing("a", 1),
                else_body: Some(branch_writing("a", 2)),
                result: None,
            },
            use_of("a"),
        ];
        assert_eq!(renames(&body, &[]), [merged("a", "z")]);
    }

    /// The read after the one-armed `if` may see the zero `a` starts at, which
    /// `z`'s slot no longer holds.
    #[test]
    fn a_local_read_before_any_write_keeps_the_entry_zero() {
        let body = [
            decl("z", WirType::I32),
            decl("a", WirType::I32),
            set("z", WirInstr::I32Const(5)),
            use_of("z"),
            WirInstr::If {
                condition: Box::new(WirInstr::I32Const(1)),
                then_body: branch_writing("a", 1),
                else_body: None,
                result: None,
            },
            use_of("a"),
        ];
        assert_eq!(renames(&body, &[]), []);
    }

    /// `a` is last read in the `then` arm before `b` is written there, and
    /// read in the `else` arm, where `b` is never live.
    #[test]
    fn a_local_dead_on_one_arm_shares_with_one_written_there() {
        let body = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            WirInstr::If {
                condition: Box::new(WirInstr::I32Const(1)),
                then_body: vec![use_of("a"), set("b", WirInstr::I32Const(2)), use_of("b")],
                else_body: Some(vec![use_of("a")]),
                result: None,
            },
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }

    /// A multi-value bind writes its targets at once: a dead first target
    /// still holds its slot while the second is written.
    #[test]
    fn targets_of_one_bind_never_share_a_slot() {
        let body = [
            decl("q", WirType::I32),
            decl("r", WirType::I32),
            WirInstr::MultiValueLocalBind {
                instr: Box::new(WirInstr::Unreachable),
                locals: vec![Some("q".to_string()), Some("r".to_string())],
            },
            use_of("r"),
        ];
        assert_eq!(renames(&body, &[]), []);
    }

    /// `b` is written in both arms and read after them, which Wasm's
    /// block-scoped rule rejects for a non-null local, so `b` is declared
    /// nullable. Sharing `a`'s slot would make `a` nullable too.
    #[test]
    fn a_local_read_unset_to_wasm_keeps_out_of_a_non_null_slot() {
        let type_id = WirTypeId::new(0, "T".into());
        let ty = WirType::non_null_ref(type_id.clone());
        let read = |name: &str| {
            WirInstr::Drop(Box::new(WirInstr::LocalGet {
                name: name.to_string(),
                result_ty: ty.clone(),
            }))
        };
        let new = || WirInstr::RefCast {
            type_id: type_id.clone(),
            nullable: false,
            expr: Box::new(WirInstr::RefNull {
                heap_type: WirAbstractHeapType::Any,
            }),
        };
        let mut body = vec![
            decl("a", ty.clone()),
            decl("b", ty.clone()),
            set("a", new()),
            read("a"),
            WirInstr::If {
                condition: Box::new(WirInstr::I32Const(1)),
                then_body: vec![set("b", new())],
                else_body: Some(vec![set("b", new())]),
                result: None,
            },
            read("b"),
        ];
        coalesce_body(&mut body, &[]);
        let declared = WirLocals::scan(&body);
        assert!(declared.is_nonnull_ref("a"));
        assert!(declared.is_nullable_ref("b"));
    }

    #[test]
    fn a_parameter_absorbs_a_local_after_its_last_read() {
        let body = [
            decl("a", WirType::I32),
            use_of("p"),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
        ];
        assert_eq!(renames(&body, &[("p", &WirType::I32)]), [merged("a", "p")]);
    }

    /// A local read before any write would see the parameter's argument in
    /// its slot, not its zero, though the argument itself is never read.
    #[test]
    fn a_local_live_at_the_entry_keeps_out_of_a_parameter() {
        let body = [
            decl("a", WirType::I32),
            use_of("a"),
            set("p", WirInstr::I32Const(1)),
            use_of("p"),
        ];
        assert_eq!(renames(&body, &[("p", &WirType::I32)]), []);
    }

    #[test]
    fn a_copy_between_merged_locals_disappears() {
        let mut body = vec![
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            set("b", get("a")),
            use_of("b"),
        ];
        let renames = plan_slots(&body, &[]);
        Rename { renames: &renames }.visit_body(&mut body);
        assert_eq!(body.len(), 3);
        assert!(
            matches!(&body[2], WirInstr::Drop(read) if matches!(&**read, WirInstr::LocalGet { name, .. } if name == "a"))
        );
    }

    /// A copy whose ends are both live after it still shares a slot: they
    /// hold one value until one is written again.
    #[test]
    fn a_copy_shares_a_slot_while_both_ends_are_live() {
        let body = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            set("b", get("a")),
            use_of("a"),
            use_of("b"),
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }
}
