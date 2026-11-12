//! Overwritten store elimination: a `struct.set` that a later one to the same
//! field chain replaces, with nothing between able to read the field or leave
//! the sequence, is not made; its value is still computed. Once
//! `reuse_struct_loads` hands each read the value stored before it, a run of
//! `List::push` writes `used` once per push and reads none of them back.

use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::reuse_loads::Key;
use super::util::Footprint;

pub(super) fn elide_overwritten_stores(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            Eliminator.visit_body(body);
        }
    }
}

struct Eliminator;

impl WirMutVisitor for Eliminator {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        for i in 0..body.len() {
            let WirInstr::StructSet {
                type_id,
                field_name,
                expr,
                ..
            } = &body[i]
            else {
                continue;
            };
            let Some(key) = Key::of_store(expr, type_id, field_name) else {
                continue;
            };
            if !overwritten(&body[i + 1..], &key) {
                continue;
            }
            let WirInstr::StructSet { value, .. } = std::mem::replace(&mut body[i], WirInstr::Nop)
            else {
                unreachable!("matched a struct.set above");
            };
            body[i] = match *value {
                WirInstr::LocalTee { name, value } => WirInstr::LocalSet { name, value },
                value => WirInstr::Drop(Box::new(value)),
            };
        }
    }
}

/// Whether the next store to `key` in `rest` comes before anything that can
/// read it.
fn overwritten(rest: &[WirInstr], key: &Key) -> bool {
    for instr in rest {
        if let WirInstr::StructSet {
            type_id,
            field_name,
            expr,
            value,
        } = instr
            && Key::of_store(expr, type_id, field_name).as_ref() == Some(key)
        {
            return !observes(expr, key) && !observes(value, key);
        }
        if observes(instr, key) {
            return false;
        }
    }
    false
}

/// Whether running `instr` may read the field `key` names, make `key` name
/// another place, or leave the sequence. A trap between the stores leaves
/// nothing to read the first one.
fn observes(instr: &WirInstr, key: &Key) -> bool {
    Footprint::reads(instr)
        .fields()
        .is_none_or(|mut fields| fields.any(|f| f == key.field()))
        || key.moved_by(&Footprint::writes(instr))
        || instr.leaves(0)
}
