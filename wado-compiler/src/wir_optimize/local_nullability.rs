//! Local nullability: Wasm lets a non-defaultable local be read only after a
//! write earlier in the same block or an enclosing one, and forgets that write
//! at the end of each `block`, `loop` and `if` arm. A non-null reference local
//! every read of which such a write dominates keeps its type. Any other is
//! declared nullable, and each read of it that expects a non-null value gets an
//! explicit `RefAsNonNull`.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirPackage};
use crate::wir_optimize::util::for_each_nullable_ref_operand;
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

/// Declare nullable each non-null reference local read before Wasm's
/// structural rule sees it written, narrowing its non-null reads explicitly.
pub(super) fn relax_unset_nonnull_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let candidates: IndexSet<String> = func
            .declared_locals()
            .iter()
            .filter(|(_, ty)| ty.is_nonnull_ref())
            .map(|(name, _)| name.to_string())
            .collect();
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        if candidates.is_empty() {
            continue;
        }
        let mut scan = InitScan {
            initialized: vec![false; candidates.len()],
            demoted: vec![false; candidates.len()],
            undo: Vec::new(),
            candidates: &candidates,
        };
        scan.walk_body(body);
        let demoted: IndexSet<String> = candidates
            .iter()
            .zip(&scan.demoted)
            .filter(|(_, demoted)| **demoted)
            .map(|(name, _)| name.clone())
            .collect();
        if !demoted.is_empty() {
            Relax { demoted: &demoted }.walk_body(body);
        }
    }
}

/// Walks a body in evaluation order, tracking which candidates Wasm considers
/// written at each point.
struct InitScan<'a> {
    candidates: &'a IndexSet<String>,
    initialized: Vec<bool>,
    demoted: Vec<bool>,
    /// Candidates written, in order, so leaving a scope forgets its writes.
    undo: Vec<usize>,
}

impl InitScan<'_> {
    fn write(&mut self, name: &str) {
        if let Some(i) = self.candidates.get_index_of(name)
            && !self.initialized[i]
        {
            self.initialized[i] = true;
            self.undo.push(i);
        }
    }

    fn scoped(&mut self, body: &[WirInstr]) {
        let mark = self.undo.len();
        self.walk_body(body);
        for i in self.undo.drain(mark..) {
            self.initialized[i] = false;
        }
    }
}

impl WirRefVisitor for InitScan<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::LocalGet { name, .. } => {
                if let Some(i) = self.candidates.get_index_of(name.as_str())
                    && !self.initialized[i]
                {
                    self.demoted[i] = true;
                }
            }
            WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } => {
                assert!(
                    !(matches!(**value, WirInstr::RefNull { .. })
                        && self.candidates.contains(name.as_str())),
                    "[WIR] `ref.null` written to the non-null local `{name}`"
                );
                self.visit_instr(value);
                self.write(name);
            }
            WirInstr::MultiValueLocalBind { instr, locals } => {
                self.visit_instr(instr);
                for name in locals.iter().flatten() {
                    self.write(name);
                }
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => self.scoped(body),
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit_instr(condition);
                self.scoped(then_body);
                if let Some(else_body) = else_body {
                    self.scoped(else_body);
                }
            }
            _ => self.walk_instr(instr),
        }
    }
}

/// Rewrites a body for its demoted locals, children first so a read's wrapper
/// meets the parent that may already narrow it or not need it.
struct Relax<'a> {
    demoted: &'a IndexSet<String>,
}

impl Relax<'_> {
    fn accesses_demoted(&self, instr: &WirInstr) -> bool {
        matches!(instr, WirInstr::LocalGet { name, .. } | WirInstr::LocalTee { name, .. }
            if self.demoted.contains(name.as_str()))
    }
}

impl WirMutVisitor for Relax<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        self.walk_instr(instr);
        // The narrowing is static only: a demoted local is never read unset at
        // run time, so an operand that accepts null needs none.
        for_each_nullable_ref_operand(instr, |operand| {
            if let WirInstr::RefAsNonNull(access) = operand
                && self.accesses_demoted(access)
            {
                let access = std::mem::replace(&mut **access, WirInstr::Nop);
                *operand = access;
            }
        });
        match instr {
            WirInstr::DeclareLocal { name, ty } if self.demoted.contains(name.as_str()) => {
                *ty = ty.clone().as_nullable();
            }
            WirInstr::LocalGet { name, result_ty }
                if self.demoted.contains(name.as_str()) && result_ty.is_nonnull_ref() =>
            {
                *result_ty = result_ty.clone().as_nullable();
                narrow(instr);
            }
            WirInstr::LocalTee { name, .. } if self.demoted.contains(name.as_str()) => {
                narrow(instr);
            }
            WirInstr::RefAsNonNull(inner) if matches!(**inner, WirInstr::RefAsNonNull(_)) => {
                let narrowed = std::mem::replace(&mut **inner, WirInstr::Nop);
                *instr = narrowed;
            }
            _ => {}
        }
    }
}

fn narrow(instr: &mut WirInstr) {
    let read = std::mem::replace(instr, WirInstr::Nop);
    *instr = WirInstr::RefAsNonNull(Box::new(read));
}
