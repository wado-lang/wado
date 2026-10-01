//! Cleanup and normalization pass for WIR.
//!
//! Removes dead locals, nops, redundant `ref.as_non_null`, dead code after
//! `Unreachable`, and calls to a function left with an empty body. Called
//! multiple times throughout the pipeline as an interpass utility rather than a
//! standalone optimization.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

use super::nullability::Nullability;
use super::util::{is_side_effect_free, may_trap_in};

pub(super) fn cleanup(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            clean_body(body);
        }
    }
    elide_calls_to_empty_functions(module);
}

fn clean_body(body: &mut Vec<WirInstr>) {
    // Remove DeclareLocal for locals that are never used (no LocalGet/LocalSet/LocalTee).
    eliminate_dead_locals(body);
    CleanupVisitor.visit_body(body);
}

/// Remove every call to a defined function whose body is empty, keeping what
/// its arguments do. A module whose globals all became constants is left with
/// an empty `$initialize_module` that the program's initializer still calls.
/// Removing the calls can empty a caller in turn, so this runs to a fixpoint.
fn elide_calls_to_empty_functions(module: &mut WirPackage) {
    // Keyed by the `WirFuncId` index a call carries, which counts the imports.
    let mut empty: IndexSet<u32> = IndexSet::default();
    loop {
        let before = empty.len();
        let base = module.defined_func_base;
        empty.extend(
            module
                .functions
                .iter()
                .enumerate()
                .filter(|(_, func)| func.body.as_ref().is_some_and(Vec::is_empty))
                .map(|(index, _)| base + u32::try_from(index).expect("function index fits u32")),
        );
        if empty.len() == before {
            return;
        }
        for func in &mut module.functions {
            let locals = func.declared_locals();
            let Some(body) = &mut func.body else {
                continue;
            };
            let mut elider = EmptyCallElider {
                empty: &empty,
                null: &Nullability::new(&locals),
                elided: false,
            };
            elider.visit_body(body);
            if elider.elided {
                clean_body(body);
            }
        }
    }
}

struct EmptyCallElider<'a> {
    empty: &'a IndexSet<u32>,
    null: &'a Nullability<'a>,
    elided: bool,
}

impl WirMutVisitor for EmptyCallElider<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        if !body.iter().any(|instr| self.is_elided_call(instr)) {
            return;
        }
        self.elided = true;
        let mut kept = Vec::with_capacity(body.len());
        for instr in body.drain(..) {
            let WirInstr::Call { func_id, args } = instr else {
                kept.push(instr);
                continue;
            };
            if !self.empty.contains(&func_id.index()) {
                kept.push(WirInstr::Call { func_id, args });
                continue;
            }
            kept.extend(
                args.into_iter()
                    .filter(|arg| !is_side_effect_free(arg) || may_trap_in(arg, self.null))
                    .map(|arg| WirInstr::Drop(Box::new(arg))),
            );
        }
        *body = kept;
    }
}

impl EmptyCallElider<'_> {
    fn is_elided_call(&self, instr: &WirInstr) -> bool {
        matches!(instr, WirInstr::Call { func_id, .. } if self.empty.contains(&func_id.index()))
    }
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
        // Post-visit: elide redundant RefAsNonNull when the inner expression is already non-null.
        if let WirInstr::RefAsNonNull(inner) = instr
            && inner.is_nonnull_result()
        {
            *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
        }
    }
}
