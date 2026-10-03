//! Cleanup and normalization pass for WIR.
//!
//! Removes dead locals, nops, redundant `ref.as_non_null`, and dead code after
//! `Unreachable`. Leaves unnarrowed what an instruction traps on when null, which
//! turns a non-null read of a nullable slot into a nullable one. Called multiple
//! times throughout the pipeline as an interpass utility rather than a
//! standalone optimization.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

use super::util::{for_each_null_trapping_operand, is_side_effect_free, own_traps};

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

/// Leave out the narrowing of each operand `instr` traps on when null by
/// itself: a `RefAsNonNull`, or the `ref.as_non_null` codegen adds after a
/// non-null read of a nullable global or array slot. That moves the trap to
/// `instr`, past the operands evaluated between them, so it is done only where
/// those have no effect. They may still trap: a narrowing wraps a value the
/// types already hold non-null, so its trap never fires and moving it reorders
/// nothing. Each operand is checked once, and only after one that is narrowed.
fn relax_null_trapping_objects(instr: &mut WirInstr) {
    let own = own_traps(instr);
    let mut narrowed = false;
    let mut last_effect = None;
    let mut position = 0;
    instr.for_each_child(&mut |operand| {
        if narrowed {
            if !is_side_effect_free(operand) {
                last_effect = Some(position);
            }
        } else if own.on_null().any(|p| p == position) && is_narrowed(operand) {
            narrowed = true;
        }
        position += 1;
    });
    if !narrowed {
        return;
    }
    for_each_null_trapping_operand(instr, |position, operand| {
        if last_effect.is_none_or(|e| e < position) {
            relax_nonnull(operand);
        }
    });
}

fn is_narrowed(object: &WirInstr) -> bool {
    match object {
        WirInstr::RefAsNonNull(_) => true,
        WirInstr::Seq(items) => items.last().is_some_and(is_narrowed),
        WirInstr::GlobalGet { result_ty, .. } | WirInstr::ArrayGet { result_ty, .. } => {
            result_ty.is_nonnull_ref()
        }
        _ => false,
    }
}

/// Undo what [`is_narrowed`] finds.
fn relax_nonnull(object: &mut WirInstr) {
    match object {
        WirInstr::RefAsNonNull(inner) => {
            *object = std::mem::replace(inner.as_mut(), WirInstr::Nop);
            relax_nonnull(object);
        }
        WirInstr::Seq(items) => {
            if let Some(value) = items.last_mut() {
                relax_nonnull(value);
            }
        }
        WirInstr::GlobalGet { result_ty, .. } | WirInstr::ArrayGet { result_ty, .. } => {
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

    fn effect() -> WirInstr {
        WirInstr::Call {
            func_id: WirFuncId::new(0, Rc::from("f")),
            args: Vec::new(),
        }
    }

    #[test]
    fn a_store_whose_value_has_an_effect_keeps_the_narrowing() {
        let mut body = store_into(global_read(), effect());
        clean_body(&mut body);
        assert!(stored_array(&body).is_nonnull_result(), "{body:?}");
    }

    #[test]
    fn a_call_ref_takes_its_function_unnarrowed_after_any_argument() {
        let mut body = vec![WirInstr::CallRef {
            type_id: array_ty(),
            func_ref: Box::new(WirInstr::RefAsNonNull(Box::new(global_read()))),
            args: vec![effect()],
        }];
        clean_body(&mut body);
        let [WirInstr::CallRef { func_ref, .. }] = body.as_slice() else {
            panic!("{body:?}");
        };
        assert!(!func_ref.is_nonnull_result(), "{body:?}");
    }
}
