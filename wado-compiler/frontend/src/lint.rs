//! Source-level lints: what a program says that compiles but is likely not
//! meant, read off `Semantics` alone.

use crate::ast::{self, AstId, Item, attrs_allow, inner_attrs_allow, lint};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::elaborator::liveness::is_user_authored;
use crate::literal_cast::literal_cast_diagnostics;
use crate::resolve::Shadowed;
use crate::semantics::Semantics;

/// Every source-level lint. `unused` gates the unused lints alone, as
/// `--no-unused` names them alone; the rest are waived by `allow` instead.
pub fn lint_diagnostics(sem: &Semantics, unused: bool, is_test_world: bool) -> Vec<Diagnostic> {
    let mut lints = shadowing_diagnostics(sem);
    lints.extend(undecided_effect_diagnostics(sem));
    lints.extend(literal_cast_diagnostics(sem));
    if unused {
        lints.extend(unused_diagnostics(sem, is_test_world));
    }
    lints
}

/// Build unused-item warnings from the liveness classification: `DeadFunction`
/// / `DeadGlobal` (reached by neither production nor tests) and
/// `TestOnlyFunction` / `TestOnlyGlobal` (reached only by `test` blocks).
/// Pure over `Semantics`; `is_test_world` suppresses the `TestOnly*` warnings.
fn unused_diagnostics(sem: &Semantics, is_test_world: bool) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut collect = |ids: &[AstId], fn_code: Code, global_code: Code, reason: &str| {
        for id in ids {
            let Some(owning) = sem.module_of_id(*id) else {
                continue;
            };
            let Some(module) = sem.modules.get(owning) else {
                continue;
            };
            let filename = owning.source_path();
            for item in &module.items {
                let (code, message, span) = match item {
                    Item::Function(func) if func.id == *id => (
                        fn_code,
                        format!("function `{}` {reason}", func.name),
                        &func.name_span,
                    ),
                    Item::Global(global) if global.id == *id => (
                        global_code,
                        format!("global `{}` {reason}", global.name),
                        &global.name_span,
                    ),
                    _ => continue,
                };
                out.push(Diagnostic {
                    severity: Severity::Warning,
                    code,
                    message,
                    span: Some(DiagnosticSpan::from_span(span, Some(filename.as_str()))),
                });
            }
        }
    };

    collect(
        &sem.liveness.dead_items,
        Code::DeadFunction,
        Code::DeadGlobal,
        "is never used",
    );

    // A test-only item is production dead code, but flagging it during a
    // `wado test` run — where the `test` blocks that reach it are the whole
    // point — would be noise. Report it only in non-test builds (`wado
    // compile` / `wado check`).
    if !is_test_world {
        collect(
            &sem.liveness.test_only_items,
            Code::TestOnlyFunction,
            Code::TestOnlyGlobal,
            "is only used by tests",
        );
    }

    out
}

/// Source-level `ShadowedName` warnings: every binder the resolution pass found
/// taking a name that already reached a declaration or an enclosing binder.
/// Stdlib modules are left alone, as the unused lints leave them.
fn shadowing_diagnostics(sem: &Semantics) -> Vec<Diagnostic> {
    let Some(resolutions) = sem.resolutions() else {
        return Vec::new();
    };
    let defs = resolutions.defs();
    resolutions
        .shadowings()
        .iter()
        .filter(|s| is_user_authored(&s.module))
        .map(|shadowing| {
            let what = match shadowing.shadowed {
                Shadowed::Decl(def) => format!("the {} of the same name", defs.kind(def).label()),
                Shadowed::Binder => "a binding of the same name".to_string(),
            };
            Diagnostic {
                severity: Severity::Warning,
                code: Code::ShadowedName,
                message: format!(
                    "`{}` shadows {what}; rename it, or mark the binder \
                     `#[allow(shadowed_name)]` if that is deliberate",
                    shadowing.name
                ),
                span: Some(DiagnosticSpan::from_span(
                    &shadowing.span,
                    Some(shadowing.module.source_path().as_str()),
                )),
            }
        })
        .collect()
}

/// Source-level `UndecidedEffects` diagnostics: every trait head that says
/// nothing about the effects its impls may declare.
fn undecided_effect_diagnostics(sem: &Semantics) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !is_user_authored(src)
            || inner_attrs_allow(&module.inner_attributes, lint::UNDECIDED_EFFECTS)
        {
            continue;
        }
        for item in &module.items {
            let Item::Trait(trait_decl) = item else {
                continue;
            };
            if !matches!(trait_decl.head, ast::TraitHead::Undecided)
                || attrs_allow(&trait_decl.attrs, lint::UNDECIDED_EFFECTS)
            {
                continue;
            }
            let (severity, code) = if trait_decl.visibility.is_public() {
                (Severity::Warning, Code::UndecidedEffects)
            } else {
                (Severity::Info, Code::Remark)
            };
            out.push(Diagnostic {
                severity,
                code,
                message: format!(
                    "`{}` says nothing about the effects its impls may declare; \
                     write `with ()` to forbid them, `with _` to leave them to the impl, \
                     or `#[allow(undecided_effects)]` while deciding",
                    trait_decl.name
                ),
                span: Some(DiagnosticSpan::from_span(
                    &trait_decl.name_span,
                    Some(src.source_path().as_str()),
                )),
            });
        }
    }
    out
}
