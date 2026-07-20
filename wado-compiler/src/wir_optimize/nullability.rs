//! Nullability oracle shared by WIR optimization passes.
//!
//! Answers whether a WIR value is a statically non-null reference, combining
//! structural producer recognition ([`WirInstr::is_nonnull_result`] — a
//! `struct.new` / `array.new*` / `ref.as_non_null` / `ref.func` / non-null-typed
//! read) with each local's declared type ([`WirLocals`]). The latter matters
//! because a `local.get`'s own `result_ty` can read nullable for a non-null
//! local after inlining substitutes a nullable-typed argument, while the
//! `DeclareLocal` type stays authoritative.

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
