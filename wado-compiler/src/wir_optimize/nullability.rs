//! Nullability oracle shared by WIR optimization passes: whether a WIR value is
//! a statically non-null reference, from structural producers
//! ([`WirInstr::is_nonnull_result`]) plus each local's declared type
//! ([`WirLocals`], authoritative where inlining left a read site stale-nullable).

use crate::wir::{WirInstr, WirLocals};

pub(super) struct Nullability<'a> {
    locals: &'a WirLocals,
}

impl<'a> Nullability<'a> {
    pub(super) fn new(locals: &'a WirLocals) -> Self {
        Self { locals }
    }

    /// Whether `instr`'s result is a statically non-null reference.
    pub(super) fn is_nonnull(&self, instr: &WirInstr) -> bool {
        instr.is_nonnull_result()
            || matches!(instr, WirInstr::LocalGet { name, .. } if self.locals.is_nonnull_ref(name))
    }
}
