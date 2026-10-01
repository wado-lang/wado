//! Local nullability: Wasm lets a non-defaultable local be read only after a
//! write earlier in the same block or an enclosing one, and forgets that write
//! at the end of each `block`, `loop` and `if` arm. A non-null reference local
//! every read of which such a write dominates keeps its type. Any other is
//! declared nullable, and each read of it that expects a non-null value gets an
//! explicit `RefAsNonNull`.

use crate::hashmap::IndexSet;
use crate::wir::{WirInstr, WirLocals, WirPackage};
use crate::wir_optimize::util::for_each_nullable_ref_operand;
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

/// Declare nullable each non-null reference local read before Wasm's
/// structural rule sees it written, narrowing its non-null reads explicitly.
pub(super) fn relax_unset_nonnull_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = func.body.as_mut() {
            relax_unset_nonnull_locals_in(body);
        }
    }
}

/// [`relax_unset_nonnull_locals`] over one function body.
pub(super) fn relax_unset_nonnull_locals_in(body: &mut Vec<WirInstr>) {
    let declared = WirLocals::scan(body);
    let candidates: IndexSet<String> = declared
        .iter()
        .filter(|(_, ty)| ty.is_nonnull_ref())
        .map(|(name, _)| name.to_string())
        .collect();
    if candidates.is_empty() {
        return;
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
    if demoted.is_empty() {
        return;
    }
    let locals: WirLocals = declared
        .iter()
        .map(|(name, ty)| {
            let ty = if demoted.contains(name) {
                ty.clone().as_nullable()
            } else {
                ty.clone()
            };
            (name.to_string(), ty)
        })
        .collect();
    Relax {
        demoted: &demoted,
        locals: &locals,
    }
    .walk_body(body);
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
            _ => {
                if let WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } =
                    instr
                {
                    assert!(
                        !(matches!(**value, WirInstr::RefNull { .. })
                            && self.candidates.contains(name.as_str())),
                        "[WIR] `ref.null` written to the non-null local `{name}`"
                    );
                }
                self.walk_instr(instr);
                if let Some(i) = instr
                    .local_read()
                    .and_then(|name| self.candidates.get_index_of(name))
                    && !self.initialized[i]
                {
                    self.demoted[i] = true;
                }
                for name in instr.local_writes() {
                    self.write(name);
                }
            }
        }
    }
}

/// Rewrites a body for its demoted locals, children first so a read's wrapper
/// meets the parent that may already narrow it or not need it.
struct Relax<'a> {
    demoted: &'a IndexSet<String>,
    /// The declared locals, the demoted ones already nullable.
    locals: &'a WirLocals,
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
        for_each_nullable_ref_operand(instr, self.locals, |operand| {
            if let WirInstr::RefAsNonNull(access) = operand
                && self.accesses_demoted(access)
            {
                *operand = std::mem::replace(&mut **access, WirInstr::Nop);
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
                *instr = std::mem::replace(&mut **inner, WirInstr::Nop);
            }
            _ => {}
        }
    }
}

fn narrow(instr: &mut WirInstr) {
    let read = std::mem::replace(instr, WirInstr::Nop);
    *instr = WirInstr::RefAsNonNull(Box::new(read));
}
