//! Literal and range patterns whose scrutinee was a type parameter
//! (`TirPattern::PerInstance`). Monomorphization settles each instance's
//! scrutinee type, so each is judged and lowered here, as a pattern on a
//! settled type is in the elaborator.

use crate::compiler_host::CompilerHost;
use crate::elaborator::types::TypeError;
use crate::elaborator::util::{pattern_literal_error, range_bound_errors};
use crate::flat_package::FlatPackage;
use crate::logger::{Bail, Logger};
use crate::tir::{InstancePattern, TirPattern, TypeTable};
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
        let module_source = func.module_source.clone();
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let mut lowering = Lowering {
            type_table: &mut type_table,
            errors: Vec::new(),
        };
        lowering.visit_block(body);
        reported |= !lowering.errors.is_empty();
        for error in lowering.errors {
            let _ = logger.error_in(&module_source, error);
        }
    }
    if reported { Err(Bail) } else { Ok(()) }
}

struct Lowering<'a> {
    type_table: &'a mut TypeTable,
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
        let scrutinee_type = *scrutinee_type;
        let errors = match instance {
            InstancePattern::Literal(value) => {
                pattern_literal_error(value, scrutinee_type, self.type_table)
                    .into_iter()
                    .collect()
            }
            InstancePattern::Range { start, end, .. } => {
                range_bound_errors(start, end, scrutinee_type, self.type_table)
            }
        };
        for error in errors {
            self.errors
                .push(error.at(scrutinee_type, *span, self.type_table));
        }
        *pattern = instance.lower(self.type_table.is_unsigned_int(scrutinee_type));
    }
}
