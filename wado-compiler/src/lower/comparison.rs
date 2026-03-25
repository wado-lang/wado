//! Lower comparison operators on non-primitive types to trait method calls.
//!
//! Converts `==`, `!=`, `<`, `>`, `<=`, `>=` on struct/variant/generic-instance types
//! to `Eq::eq` / `Ord::cmp` method calls. Primitives keep native comparison operators.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::name::{LocalMethodName, ModuleSource};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirModule, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::tir_visitor::TirMutVisitor;
use crate::token::Span;

/// Lower comparison operators in all functions of a module.
pub fn lower_comparisons(module: &mut TirModule) {
    let type_table_rc = module.type_table.clone();
    let trait_method_locations = std::mem::take(&mut module.trait_method_locations);
    let module_source = module.module_source.clone();

    let mut desugarer = ComparisonLowerer {
        trait_method_locations: &trait_method_locations,
        current_module_source: &module_source,
        type_table: &type_table_rc,
    };

    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(mut body) = func.body.take() {
            desugarer.visit_block(&mut body);
            func.body = Some(body);
        }
    }

    // Also lower comparisons in global variable initializers
    for global in &mut module.globals {
        desugarer.visit_expr(&mut global.initializer);
    }
}

struct ComparisonLowerer<'a> {
    trait_method_locations: &'a IndexMap<String, ModuleSource>,
    current_module_source: &'a ModuleSource,
    type_table: &'a Rc<RefCell<TypeTable>>,
}

impl TirMutVisitor for ComparisonLowerer<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.walk_expr(expr);

        if let TirExprKind::Binary { op, left, right } = &mut expr.kind
            && let Some(new_kind) = try_lower_comparison(
                self.trait_method_locations,
                self.current_module_source,
                expr.span,
                *op,
                left,
                right,
                &mut self.type_table.borrow_mut(),
            )
        {
            expr.kind = new_kind;
        }
    }
}

/// Try to lower a comparison operator to a trait method call.
///
/// Returns `Some(new_kind)` if the binary expression should be replaced,
/// or `None` if it should remain as is (for primitives).
fn try_lower_comparison(
    trait_method_locations: &IndexMap<String, ModuleSource>,
    current_module_source: &ModuleSource,
    span: Span,
    op: TirBinaryOp,
    left: &TirExpr,
    right: &TirExpr,
    type_table: &mut TypeTable,
) -> Option<TirExprKind> {
    let operand_type = type_table.get(left.type_id);
    let (base_struct_name, impl_type_args, type_module_source): (
        String,
        Vec<String>,
        Option<ModuleSource>,
    ) = match operand_type {
        ResolvedType::Struct {
            name,
            module_source,
            base_name,
            ..
        } => {
            let struct_name = base_name.as_deref().unwrap_or(name).to_string();
            (struct_name, vec![], Some(module_source.clone()))
        }
        ResolvedType::Variant {
            name,
            module_source,
            ..
        } => (name.clone(), vec![], Some(module_source.clone())),
        ResolvedType::GenericInstance {
            name,
            type_args,
            module_source,
            ..
        } => {
            let args: Vec<String> = type_args
                .iter()
                .map(|&t| type_table.mangle_type_name(t))
                .collect();
            (name.clone(), args, Some(module_source.clone()))
        }
        _ => return None,
    };

    if matches!(op, TirBinaryOp::Eq | TirBinaryOp::NotEq) {
        return lower_eq(
            trait_method_locations,
            current_module_source,
            span,
            op,
            left,
            right,
            type_table,
            &base_struct_name,
            &impl_type_args,
            type_module_source,
        );
    }

    if matches!(
        op,
        TirBinaryOp::Lt | TirBinaryOp::Gt | TirBinaryOp::LtEq | TirBinaryOp::GtEq
    ) {
        return lower_ord(
            trait_method_locations,
            current_module_source,
            span,
            op,
            left,
            right,
            type_table,
            &base_struct_name,
            &impl_type_args,
            type_module_source,
        );
    }

    None
}

fn resolve_trait_method_module(
    trait_method_locations: &IndexMap<String, ModuleSource>,
    current_module_source: &ModuleSource,
    mangled_name: &str,
    type_module_source: Option<ModuleSource>,
) -> ModuleSource {
    trait_method_locations
        .get(mangled_name)
        .cloned()
        .or(type_module_source)
        .unwrap_or_else(|| current_module_source.clone())
}

fn make_ref(expr: &TirExpr, type_table: &mut TypeTable, span: Span) -> (TirExpr, TypeId) {
    let ref_type = type_table.intern(ResolvedType::Ref(expr.type_id));
    let ref_expr = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(expr.clone()),
        },
        ref_type,
        span,
    );
    (ref_expr, ref_type)
}

#[allow(clippy::too_many_arguments)]
fn lower_eq(
    trait_method_locations: &IndexMap<String, ModuleSource>,
    current_module_source: &ModuleSource,
    span: Span,
    op: TirBinaryOp,
    left: &TirExpr,
    right: &TirExpr,
    type_table: &mut TypeTable,
    base_struct_name: &str,
    impl_type_args: &[String],
    type_module_source: Option<ModuleSource>,
) -> Option<TirExprKind> {
    let needs_negation = op == TirBinaryOp::NotEq;

    let (receiver, _) = make_ref(left, type_table, span);
    let (arg_ref, _) = make_ref(right, type_table, span);

    let method_info = LocalMethodName::new(
        base_struct_name.to_string(),
        Some("Eq".to_string()),
        "eq".to_string(),
    )
    .with_struct_type_args(impl_type_args);
    let mangled_name = method_info.to_mangled_name();

    let method_module_source = resolve_trait_method_module(
        trait_method_locations,
        current_module_source,
        &mangled_name,
        type_module_source,
    );

    let method_call = TirExprKind::MethodCall {
        receiver: Box::new(receiver),
        func: FunctionRef {
            module_source: method_module_source,
            name: mangled_name,
            monomorph_info: None,
            method_info: Some(method_info),
            is_cm_binding: false,
        },
        type_args: vec![],
        args: vec![CallArg::new(arg_ref, false)],
    };

    if needs_negation {
        let bool_type =
            type_table.intern(ResolvedType::Primitive(crate::tir::PrimitiveType::Bool));
        return Some(TirExprKind::Unary {
            op: TirUnaryOp::Not,
            expr: Box::new(TirExpr::new(method_call, bool_type, span)),
        });
    }
    Some(method_call)
}

#[allow(clippy::too_many_arguments)]
fn lower_ord(
    trait_method_locations: &IndexMap<String, ModuleSource>,
    current_module_source: &ModuleSource,
    span: Span,
    op: TirBinaryOp,
    left: &TirExpr,
    right: &TirExpr,
    type_table: &mut TypeTable,
    base_struct_name: &str,
    impl_type_args: &[String],
    type_module_source: Option<ModuleSource>,
) -> Option<TirExprKind> {
    let (receiver, _) = make_ref(left, type_table, span);
    let (arg_ref, _) = make_ref(right, type_table, span);

    let ordering_type_id = type_table.intern(ResolvedType::Enum {
        name: "Ordering".to_string(),
        module_source: ModuleSource::prelude(),
    });

    let method_info = LocalMethodName::new(
        base_struct_name.to_string(),
        Some("Ord".to_string()),
        "cmp".to_string(),
    )
    .with_struct_type_args(impl_type_args);
    let mangled_name = method_info.to_mangled_name();

    let ord_method_module_source = resolve_trait_method_module(
        trait_method_locations,
        current_module_source,
        &mangled_name,
        type_module_source,
    );

    let cmp_call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: FunctionRef {
                module_source: ord_method_module_source,
                name: mangled_name,
                monomorph_info: None,
                method_info: Some(method_info),
                is_cm_binding: false,
            },
            type_args: vec![],
            args: vec![CallArg::new(arg_ref, false)],
        },
        ordering_type_id,
        span,
    );

    let (compare_op, case_name, case_index): (TirBinaryOp, &str, u32) = match op {
        TirBinaryOp::Lt => (TirBinaryOp::Eq, "Less", 0),
        TirBinaryOp::Gt => (TirBinaryOp::Eq, "Greater", 2),
        TirBinaryOp::LtEq => (TirBinaryOp::NotEq, "Greater", 2),
        TirBinaryOp::GtEq => (TirBinaryOp::NotEq, "Less", 0),
        _ => unreachable!(),
    };

    let ordering_variant = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type_id,
            case_name: case_name.to_string(),
            case_index,
        },
        ordering_type_id,
        span,
    );

    Some(TirExprKind::Binary {
        op: compare_op,
        left: Box::new(cmp_call),
        right: Box::new(ordering_variant),
    })
}
