//! The declared-type invariant, checked once between elaboration and lowering.
//!
//! `TypeTable::UNKNOWN` means "not decided yet" while inference is running, and
//! typechecks as compatible with everything so a deferred answer does not
//! cascade. Once inference is over, a *declared* type still holding it means
//! resolution never found one — an unimported name reads exactly like that.
//!
//! Left alone it reaches `wir_build`, which asserts and reports a compiler bug
//! at a location the user cannot act on. Checking it here turns the whole class
//! into an ordinary diagnostic, and keeps it that way as new annotation sites
//! appear: this is the single place that has to know the rule.

use crate::compiler_host::CompilerHost;
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::tir::TypeTable;
use crate::token::Span;

/// Report every declared type that resolution never answered. Returns the
/// number reported, so the caller can stop before lowering.
pub fn report_unresolved_declared_types<H: CompilerHost>(
    flat: &FlatPackage,
    logger: &Logger<'_, H>,
) -> usize {
    let mut reported = 0;

    for func in &flat.functions {
        let func = func.borrow();
        // A stub carries no body and no annotation of its own.
        if func.body.is_none() {
            continue;
        }
        for param in &func.params {
            if param.type_id == TypeTable::UNKNOWN {
                emit(
                    logger,
                    &func.module_source,
                    span_or(param.span, func.span),
                    &format!(
                        "parameter `{}` of `{}` has no resolved type",
                        param.name, func.name
                    ),
                );
                reported += 1;
            }
        }
        if func.return_type == TypeTable::UNKNOWN {
            emit(
                logger,
                &func.module_source,
                func.span,
                &format!("the return type of `{}` was never resolved", func.name),
            );
            reported += 1;
        }
        for local in &func.locals {
            if local.type_id == TypeTable::UNKNOWN {
                emit(
                    logger,
                    &func.module_source,
                    span_or(local.span, func.span),
                    &format!("`{}` in `{}` has no resolved type", local.name, func.name),
                );
                reported += 1;
            }
        }
    }

    for decl in &flat.structs {
        for field in &decl.fields {
            if field.type_id == TypeTable::UNKNOWN {
                emit(
                    logger,
                    &decl.module_source,
                    decl.span,
                    &format!(
                        "field `{}` of `{}` has no resolved type",
                        field.name, decl.name
                    ),
                );
                reported += 1;
            }
        }
    }

    reported
}

/// A synthesised slot carries the default span, which points at nothing; the
/// declaration that owns it is the next best anchor.
fn span_or(span: Span, fallback: Span) -> Span {
    if span == Span::default() {
        fallback
    } else {
        span
    }
}

fn emit<H: CompilerHost>(logger: &Logger<'_, H>, module: &ModuleSource, span: Span, message: &str) {
    let file = module.source_path();
    let _ = logger.error_at(
        &file,
        Diagnostic {
            severity: Severity::Error,
            code: Code::UnknownType,
            message: format!("{message}; a type it names was never imported here"),
            span: Some(DiagnosticSpan::from_span(&span, Some(&file))),
        },
    );
}
