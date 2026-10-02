//! Cleanup and normalization pass for WIR.
//!
//! Removes dead locals, nops, redundant `ref.as_non_null`, and dead code after
//! `Unreachable`. Called multiple times throughout the pipeline as an interpass
//! utility rather than a standalone optimization.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

use super::util::is_side_effect_free;

pub(super) fn cleanup(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            clean_body(body);
        }
    }
}

pub(super) fn clean_body(body: &mut Vec<WirInstr>) {
    // Remove DeclareLocal for locals that are never used (no LocalGet/LocalSet/LocalTee).
    eliminate_dead_locals(body);
    CleanupVisitor.visit_body(body);
}

/// Elide the redundant `RefAsNonNull` wrappers `struct_new` adds for
/// non-nullable fields from every global initializer, so a promoted initializer
/// reads in the same normal form as a function body. Emitted output is
/// unchanged: the wrapper is already transparent to `is_const_expressible`,
/// to `dedupe_const_globals`, and to the emitter.
pub(super) fn cleanup_global_inits(module: &mut WirPackage) {
    for global in &mut module.globals {
        CleanupVisitor.visit_instr(&mut global.init);
    }
}

/// Remove `DeclareLocal` instructions for locals that are never referenced
/// by any `LocalGet`, `LocalSet`, `LocalTee`, or `MultiValueLocalBind`.
fn eliminate_dead_locals(body: &mut [WirInstr]) {
    let mut used: IndexSet<String> = IndexSet::default();
    let mut collector = CollectLocalUses { used: &mut used };
    for instr in body.iter() {
        collector.visit_instr(instr);
    }
    let mut noper = NopUnusedDeclareLocals { used: &used };
    for instr in body.iter_mut() {
        noper.visit_instr(instr);
    }
}

struct CollectLocalUses<'a> {
    used: &'a mut IndexSet<String>,
}

impl WirRefVisitor for CollectLocalUses<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        for name in instr.local_read().into_iter().chain(instr.local_writes()) {
            self.used.insert(name.to_string());
        }
        self.walk_instr(instr);
    }
}

struct NopUnusedDeclareLocals<'a> {
    used: &'a IndexSet<String>,
}

impl WirMutVisitor for NopUnusedDeclareLocals<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        if let WirInstr::DeclareLocal { name, .. } = instr
            && !self.used.contains(name.as_str())
        {
            *instr = WirInstr::Nop;
            return;
        }
        self.walk_instr(instr);
    }
}

struct CleanupVisitor;

impl WirMutVisitor for CleanupVisitor {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        // Remove nops.
        body.retain(|i| !matches!(i, WirInstr::Nop));
        // Truncate after first unreachable (dead code elimination).
        if let Some(pos) = body.iter().position(|i| matches!(i, WirInstr::Unreachable)) {
            body.truncate(pos + 1);
        }
    }

    fn visit_instr(&mut self, instr: &mut WirInstr) {
        self.walk_instr(instr);
        if let WirInstr::RefAsNonNull(inner) = instr
            && inner.is_nonnull_result()
        {
            *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
        }
        relax_null_trapping_objects(instr);
    }
}

/// A GC access traps on a null object by itself, as a cast to a non-null type
/// does, so its object need not be narrowed first: neither by a `RefAsNonNull`,
/// nor by the `ref.as_non_null` codegen adds after a non-null read of a
/// nullable global or array slot.
fn relax_null_trapping_objects(instr: &mut WirInstr) {
    match instr {
        WirInstr::StructGet { expr: object, .. }
        | WirInstr::ArrayLen(object)
        | WirInstr::RefCast {
            nullable: false,
            expr: object,
            ..
        } => relax_nonnull(object, &[]),
        WirInstr::StructSet {
            expr: object,
            value,
            ..
        } => relax_nonnull(object, &[&**value]),
        WirInstr::ArrayGet {
            array: object,
            index,
            ..
        }
        | WirInstr::ArrayGetS {
            array: object,
            index,
            ..
        }
        | WirInstr::ArrayGetU {
            array: object,
            index,
            ..
        } => relax_nonnull(object, &[&**index]),
        WirInstr::ArraySet {
            array: object,
            index,
            value,
            ..
        } => relax_nonnull(object, &[&**index, &**value]),
        WirInstr::ArrayFill {
            array: object,
            offset,
            value,
            len,
            ..
        } => relax_nonnull(object, &[&**offset, &**value, &**len]),
        WirInstr::ArrayCopy {
            dest,
            dest_offset,
            src,
            src_offset,
            len,
            ..
        } => {
            relax_nonnull(src, &[&**src_offset, &**len]);
            relax_nonnull(dest, &[&**dest_offset, &**src, &**src_offset, &**len]);
        }
        _ => {}
    }
}

/// Leave out the narrowing of `object`. That moves its trap to the access, past
/// the `later` operands evaluated between them, so it is done only where those
/// have no effect, and they are checked only where there is a narrowing.
fn relax_nonnull(object: &mut WirInstr, later: &[&WirInstr]) {
    let trap_may_move = || later.iter().all(|operand| is_side_effect_free(operand));
    match object {
        WirInstr::RefAsNonNull(inner) if trap_may_move() => {
            *object = std::mem::replace(inner.as_mut(), WirInstr::Nop);
            // `later` has just passed the check.
            relax_nonnull(object, &[]);
        }
        WirInstr::Seq(items) => {
            if let Some(value) = items.last_mut() {
                relax_nonnull(value, later);
            }
        }
        WirInstr::GlobalGet { result_ty, .. } | WirInstr::ArrayGet { result_ty, .. }
            if result_ty.is_nonnull_ref() && trap_may_move() =>
        {
            result_ty.set_nullable();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::wir::{WirFuncId, WirName, WirType, WirTypeId};

    fn array_ty() -> WirTypeId {
        WirTypeId::new(0, Rc::from("a"))
    }

    /// A non-null read of a nullable global slot, which codegen narrows.
    fn global_read() -> WirInstr {
        WirInstr::GlobalGet {
            name: WirName { fq: "g".into() },
            result_ty: WirType::Ref {
                type_id: array_ty(),
                nullable: false,
            },
        }
    }

    fn store_into(array: WirInstr, value: WirInstr) -> Vec<WirInstr> {
        vec![WirInstr::ArraySet {
            type_id: array_ty(),
            array: Box::new(WirInstr::RefAsNonNull(Box::new(array))),
            index: Box::new(WirInstr::I32Const(0)),
            value: Box::new(value),
        }]
    }

    fn stored_array(body: &[WirInstr]) -> &WirInstr {
        let [WirInstr::ArraySet { array, .. }] = body else {
            panic!("{body:?}");
        };
        array
    }

    #[test]
    fn a_store_takes_its_array_unnarrowed() {
        let mut body = store_into(global_read(), WirInstr::I32Const(1));
        clean_body(&mut body);
        assert!(!stored_array(&body).is_nonnull_result(), "{body:?}");
    }

    #[test]
    fn a_store_whose_value_has_an_effect_keeps_the_narrowing() {
        let effect = WirInstr::Call {
            func_id: WirFuncId::new(0, Rc::from("f")),
            args: Vec::new(),
        };
        let mut body = store_into(global_read(), effect);
        clean_body(&mut body);
        assert!(stored_array(&body).is_nonnull_result(), "{body:?}");
    }
}
