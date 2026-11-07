//! Expands [`TirExprKind::ListViewOf`], a call argument converted to the
//! `&ListView<T>` parameter it meets, into `&ListArg::<T>::lowered(&value)`.
//! It runs before drop elaboration, so the view is a temporary that drops where
//! its statement ends, freeing any block the conversion copied.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::elaborator::trait_env::TraitEnv;
use crate::module_source::ModuleSource;
use crate::name::FqTraitName;
use crate::synthesis::common::{deref_expr, ref_expr};
use crate::synthesis::template::written_impl_at;
use crate::synthesis::traits::trait_call_on_type;
use crate::tir::{TirExpr, TirExprKind, TirModule};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr};
use crate::type_table::TypeTable;

pub fn expand_list_views(
    module: &mut TirModule,
    tt: &Rc<RefCell<TypeTable>>,
    trait_env: &TraitEnv,
) {
    let mut expander = ListViewExpander {
        tt,
        trait_env,
        module_source: &module.module_source,
    };
    for func_rc in &module.functions {
        if let Some(body) = &mut func_rc.borrow_mut().body {
            expander.visit_block(body);
        }
    }
}

struct ListViewExpander<'a> {
    tt: &'a Rc<RefCell<TypeTable>>,
    trait_env: &'a TraitEnv,
    module_source: &'a ModuleSource,
}

impl TirOptVisitor for ListViewExpander<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        let changed = opt_walk_expr(self, expr);
        let kind = std::mem::replace(&mut expr.kind, TirExprKind::Unit);
        let TirExprKind::ListViewOf { value } = kind else {
            expr.kind = kind;
            return changed;
        };
        let mut tt = self.tt.borrow_mut();
        let element = tt
            .list_view_ref_element(expr.type_id)
            .expect("a conversion is typed as the `&ListView<T>` it meets");
        let view = tt.make_list_view(element);
        let list_arg = tt
            .compiler_trait_fq(CompilerItem::ListArg)
            .with_args(vec![tt.fq_type_name(element)]);
        let value = answering_layer(*value, &list_arg, self.trait_env, &tt);
        let value_type = value.type_id;
        let lowered = trait_call_on_type(
            value,
            value_type,
            &list_arg,
            "lowered",
            view,
            vec![],
            false,
            self.trait_env,
            self.module_source,
            &mut tt,
            expr.span,
        );
        *expr = ref_expr(lowered, expr.type_id, expr.span);
        true
    }
}

/// `value` dereferenced to the layer whose own impl of `list_arg` answers: a
/// reference holds the trait by its pointee's answer, unless an impl is written
/// for the reference itself (`&LinearArray<T>`).
fn answering_layer(
    mut value: TirExpr,
    list_arg: &FqTraitName,
    trait_env: &TraitEnv,
    tt: &TypeTable,
) -> TirExpr {
    while let Some(pointee) = tt.ref_pointee(value.type_id)
        && !written_impl_at(trait_env, list_arg, value.type_id, tt)
    {
        let span = value.span;
        value = deref_expr(value, pointee, span);
    }
    value
}
