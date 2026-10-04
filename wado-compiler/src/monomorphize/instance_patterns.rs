//! Literal and range patterns whose scrutinee was a type parameter
//! (`TirPattern::PerInstance`). Monomorphization settles each instance's
//! scrutinee type, so each is judged and lowered here, as a pattern on a
//! settled type is in the elaborator.

use crate::compiler_host::CompilerHost;
use crate::elaborator::reify::lower_instance_pattern;
use crate::elaborator::types::TypeError;
use crate::flat_package::FlatPackage;
use crate::logger::{Bail, Logger};
use crate::synthesis::common::alloc_named_local;
use crate::tir::{TirLocal, TirPattern, TypeTable};
use crate::tir_visitor::TirMutVisitor;

/// Lower every per-instance pattern, reporting each that names no value of
/// its instance.
pub fn lower_instance_patterns<H: CompilerHost>(
    flat: &FlatPackage,
    logger: &Logger<'_, H>,
) -> Result<(), Bail> {
    let mut type_table = flat.type_table.borrow_mut();
    let mut reported = false;
    for func in &flat.functions {
        let mut func = func.borrow_mut();
        let func = &mut *func;
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let mut lowering = Lowering {
            type_table: &mut type_table,
            local_count: &mut func.local_count,
            locals: &mut func.locals,
            errors: Vec::new(),
        };
        lowering.visit_block(body);
        reported |= !lowering.errors.is_empty();
        for error in lowering.errors {
            let _ = logger.error_in(&func.module_source, error);
        }
    }
    if reported { Err(Bail) } else { Ok(()) }
}

struct Lowering<'a> {
    type_table: &'a mut TypeTable,
    local_count: &'a mut u32,
    locals: &'a mut Vec<TirLocal>,
    errors: Vec<TypeError>,
}

impl TirMutVisitor for Lowering<'_> {
    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        let TirPattern::PerInstance {
            pattern: instance,
            scrutinee_type,
            span,
        } = pattern
        else {
            return self.walk_pattern(pattern);
        };
        let (scrutinee_type, span) = (*scrutinee_type, *span);
        let lowered = lower_instance_pattern(
            instance,
            scrutinee_type,
            self.type_table,
            |name, ty| alloc_named_local(self.local_count, self.locals, Some(name), ty, false),
            span,
        );
        match lowered {
            Ok(lowered) => *pattern = lowered,
            Err(errors) => {
                for error in errors {
                    self.errors
                        .push(error.at(scrutinee_type, span, self.type_table));
                }
            }
        }
    }
}
