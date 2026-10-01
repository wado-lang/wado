//! Lower the wide-arithmetic builtins (`i64_mul_wide_u/s`, `i64_add128`,
//! `i64_sub128`) to their `core:rt` software forms under
//! `-f no-wide-arithmetic`, for V8, which lacks the proposal. Rewriting before
//! NIR hands the software forms to the optimizer like any other call.

use crate::compiler_item::CompilerItem;
use crate::flat_package::FlatPackage;
use crate::module_source::ModuleSource;
use crate::tir::{TirExpr, TirExprKind};
use crate::tir_visitor::TirMutVisitor;

/// Each wide-arithmetic builtin, with the compiler item that computes it
/// without the proposal.
const SOFT_FORMS: [(&str, CompilerItem); 4] = [
    ("i64_add128", CompilerItem::I64Add128Soft),
    ("i64_sub128", CompilerItem::I64Sub128Soft),
    ("i64_mul_wide_u", CompilerItem::I64MulWideUSoft),
    ("i64_mul_wide_s", CompilerItem::I64MulWideSSoft),
];

/// Rewrite every wide-arithmetic builtin call in `flat` to its software form,
/// unless the target has the proposal (`wide_arithmetic`).
pub fn lower(flat: &FlatPackage, wide_arithmetic: bool) {
    if wide_arithmetic {
        return;
    }
    let soft_forms = {
        let type_table = flat.type_table.borrow();
        SOFT_FORMS.map(|(builtin, item)| {
            let (module, name) = type_table.compiler_items().require_function(item);
            (builtin, module.clone(), name.to_string())
        })
    };
    flat.visit_bodies_mut(&mut WideArithLowering { soft_forms });
}

struct WideArithLowering {
    soft_forms: [(&'static str, ModuleSource, String); 4],
}

impl TirMutVisitor for WideArithLowering {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Call { func, .. } = &mut expr.kind
            && func.module_source == ModuleSource::builtin()
            && let Some((_, module, name)) = self
                .soft_forms
                .iter()
                .find(|(builtin, _, _)| func.name == *builtin)
        {
            func.module_source = module.clone();
            func.name.clone_from(name);
        }
        self.walk_expr(expr);
    }
}
