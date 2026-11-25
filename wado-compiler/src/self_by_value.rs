//! The `self_by_value` lint: a by-value `self` on a type every value of which
//! is copyable, which every call copies. See
//! `docs/wep-2026-10-17-affine-types.md`.

use crate::Lints;
use crate::ast::{Item, SelfKind, attrs_allow, inner_attrs_allow, lint};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::semantics::Semantics;

/// Source-level `SelfByValue` warnings: every inherent method taking `self` by
/// value on a type no instance makes affine. An `impl` with an `affine`
/// parameter and a trait method are left alone, since an affine instance moves
/// through the same declaration.
pub fn self_by_value_diagnostics(sem: &Semantics, lints: Lints) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (src, module) in lints.modules(&sem.modules) {
        if inner_attrs_allow(&module.inner_attributes, lint::SELF_BY_VALUE) {
            continue;
        }
        for item in &module.items {
            let Item::Impl(block) = item else {
                continue;
            };
            if block.trait_type.is_some() || attrs_allow(&block.attrs, lint::SELF_BY_VALUE) {
                continue;
            }
            for method in &block.methods {
                if attrs_allow(&method.attrs, lint::SELF_BY_VALUE) {
                    continue;
                }
                for param in &method.params {
                    if param.self_kind != SelfKind::Value {
                        continue;
                    }
                    // A partial snapshot leaves a method its error stopped
                    // at with no recorded receiver.
                    let Some(receiver) = sem.local_type(param.id) else {
                        continue;
                    };
                    let admits_affine = block.type_params.iter().any(|p| p.is_affine);
                    if admits_affine || sem.is_affine(receiver, &[]) {
                        continue;
                    }
                    out.push(Diagnostic {
                        severity: Severity::Warning,
                        code: Code::SelfByValue,
                        message: format!(
                            "`self` by value copies `{}` at every call; take `&self`, or mark \
                             it `#[allow(self_by_value)]` if the copy is meant",
                            sem.types.name(receiver)
                        ),
                        span: Some(DiagnosticSpan::from_span(
                            &param.span,
                            Some(src.source_path().as_str()),
                        )),
                    });
                }
            }
        }
    }
    out
}
