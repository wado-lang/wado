//! Nullability-driven rewrites, over the shared [`Nullability`] oracle:
//!
//! - `ref.as_non_null(x)` where `x` is already non-null → `x` (the assertion is
//!   a proven no-op).
//! - `ref.is_null(x)` where `x` is non-null → `0` (false), when evaluating `x`
//!   is side-effect free and non-trapping so dropping it changes nothing.
//!
//! Both read a `LocalGet`'s nullability from the local's declared type, so they
//! fire even where inlining left the read site's `result_ty` stale-nullable.

use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::nullability::Nullability;
use super::util::{is_side_effect_free, may_trap_in};

pub(super) fn optimize_nullability(module: &mut WirPackage) {
    for func in &mut module.functions {
        let locals = func.declared_locals();
        if let Some(body) = &mut func.body {
            let null = Nullability::new(&locals);
            let mut rewriter = NullabilityRewrite { null: &null };
            for instr in body.iter_mut() {
                rewriter.visit_instr(instr);
            }
        }
    }
}

struct NullabilityRewrite<'a> {
    null: &'a Nullability<'a>,
}

impl WirMutVisitor for NullabilityRewrite<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        // Rewrite children first so an inner elision (e.g. a nested
        // `ref.as_non_null`) is visible when the outer node is examined.
        self.walk_instr(instr);
        match instr {
            WirInstr::RefAsNonNull(inner) if self.null.is_nonnull(inner) => {
                *instr = std::mem::replace(inner.as_mut(), WirInstr::Nop);
            }
            WirInstr::RefIsNull(inner)
                if self.null.is_nonnull(inner)
                    && is_side_effect_free(inner)
                    && !may_trap_in(inner, self.null) =>
            {
                *instr = WirInstr::I32Const(0);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::WirLocals;

    fn rewrite(mut instr: WirInstr) -> WirInstr {
        let locals = WirLocals::default();
        let null = Nullability::new(&locals);
        NullabilityRewrite { null: &null }.visit_instr(&mut instr);
        instr
    }

    /// `ref.i31(0)` is structurally non-null, so `ref.as_non_null` over it is a
    /// proven no-op and collapses to the inner value.
    #[test]
    fn elides_ref_as_non_null_over_nonnull() {
        let inner = WirInstr::RefI31(Box::new(WirInstr::I32Const(0)));
        assert!(matches!(
            rewrite(WirInstr::RefAsNonNull(Box::new(inner))),
            WirInstr::RefI31(_)
        ));
    }

    /// A nested redundant `ref.as_non_null` collapses too (children first).
    #[test]
    fn elides_nested_ref_as_non_null() {
        let inner = WirInstr::RefI31(Box::new(WirInstr::I32Const(0)));
        let doubled = WirInstr::RefAsNonNull(Box::new(WirInstr::RefAsNonNull(Box::new(inner))));
        assert!(matches!(rewrite(doubled), WirInstr::RefI31(_)));
    }

    /// `ref.is_null` of a non-null, side-effect-free value folds to `0`.
    #[test]
    fn folds_ref_is_null_over_nonnull() {
        let inner = WirInstr::RefI31(Box::new(WirInstr::I32Const(0)));
        assert!(matches!(
            rewrite(WirInstr::RefIsNull(Box::new(inner))),
            WirInstr::I32Const(0)
        ));
    }
}
