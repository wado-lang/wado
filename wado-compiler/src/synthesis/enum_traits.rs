//! Enum trait synthesis phase.
//!
//! Generates auto-derived trait implementations (Eq, Ord) for enum types.
//! For each enum declaration, generates synthetic TIR functions:
//! - `EnumName^Eq::eq(&self, &Self) -> bool` - discriminant equality
//! - `EnumName^Ord::cmp(&self, &Self) -> Ordering` - discriminant ordering
//!
//! Pipeline position: runs as part of the synthesis phase, before monomorphize.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexSet;

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::project::Project;
use crate::tir::{
    TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirModule, TirParam, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};

use super::common::make_synthetic_method;

/// Run enum trait synthesis on the entire project.
///
/// For each module, generates Eq and Ord implementations for all enum types
/// that don't already have user-provided implementations.
pub fn synthesize_enum_traits(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        generate_enum_trait_impls(module);
    }
}

/// Generate auto-derived trait implementations (Eq, Ord) for enum types in a module.
fn generate_enum_trait_impls(module: &mut TirModule) {
    if module.enums.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();

    // Collect enum info
    let enum_infos: Vec<_> = module
        .enums
        .iter()
        .map(|e| (e.name.clone(), e.span))
        .collect();

    // Check which trait methods already have user-provided implementations.
    // If the user wrote `impl Eq for Color { ... }`, skip generating Eq::eq.
    let existing_trait_methods: IndexSet<String> = module
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            func.method_info.as_ref().and_then(|info| {
                info.trait_name.as_ref().map(|trait_name| {
                    format!(
                        "{}^{}::{}",
                        info.base_struct_name, trait_name, info.method_name
                    )
                })
            })
        })
        .collect();

    let mut generated_functions = Vec::new();

    for (enum_name, span) in &enum_infos {
        let mut type_table = module.type_table.borrow_mut();
        let enum_type = type_table.make_enum(enum_name.clone(), module_source.clone());
        let ref_enum_type = type_table.make_ref(enum_type);

        // Generate Eq::eq
        let eq_key = MethodName::format_local(enum_name, Some("Eq"), "eq");
        if !existing_trait_methods.contains(&eq_key) {
            let func = generate_enum_eq_fn(enum_name, enum_type, ref_enum_type, *span);
            generated_functions.push(Rc::new(RefCell::new(func)));
        }

        // Generate Ord::cmp
        let cmp_key = MethodName::format_local(enum_name, Some("Ord"), "cmp");
        if !existing_trait_methods.contains(&cmp_key) {
            let ordering_type = type_table.make_enum(
                "Ordering".to_string(),
                ModuleSource::core("prelude/traits.wado"),
            );
            let func =
                generate_enum_ord_fn(enum_name, enum_type, ref_enum_type, ordering_type, *span);
            generated_functions.push(Rc::new(RefCell::new(func)));
        }
    }

    module.functions.extend(generated_functions);
}

/// Generate `EnumName^Eq::eq(&self, &Self) -> bool`
///
/// Body: `return *self == *other;` (i32 comparison via enum discriminant)
fn generate_enum_eq_fn(
    enum_name: &str,
    enum_type: TypeId,
    ref_enum_type: TypeId,
    span: crate::token::Span,
) -> crate::tir::TirFunction {
    let method_info = LocalMethodName::new(
        enum_name.to_string(),
        Some("Eq".to_string()),
        "eq".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    // params: self: &EnumType (local 0), other: &EnumType (local 1)
    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_enum_type,
            local_index: 0,
            span,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_enum_type,
            local_index: 1,
            span,
        },
    ];

    // Body: return *self == *other
    let deref_self = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 0,
                    name: "self".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );
    let deref_other = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 1,
                    name: "other".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );
    let comparison = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(deref_self),
            op: TirBinaryOp::Eq,
            right: Box::new(deref_other),
        },
        TypeTable::BOOL,
        span,
    );
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(comparison),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        TypeTable::BOOL,
        body,
        vec![ref_enum_type, ref_enum_type],
    )
}

/// Generate `EnumName^Ord::cmp(&self, &Self) -> Ordering`
///
/// Body:
/// ```text
/// let a = *self;
/// let b = *other;
/// if a < b { return Ordering::Less; }
/// if a > b { return Ordering::Greater; }
/// return Ordering::Equal;
/// ```
fn generate_enum_ord_fn(
    enum_name: &str,
    enum_type: TypeId,
    ref_enum_type: TypeId,
    ordering_type: TypeId,
    span: crate::token::Span,
) -> crate::tir::TirFunction {
    let method_info = LocalMethodName::new(
        enum_name.to_string(),
        Some("Ord".to_string()),
        "cmp".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_enum_type,
            local_index: 0,
            span,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_enum_type,
            local_index: 1,
            span,
        },
    ];

    // Local 2: a = *self, Local 3: b = *other
    let deref_self = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 0,
                    name: "self".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );
    let deref_other = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 1,
                    name: "other".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );

    let local_a = |span: crate::token::Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 2,
                name: "a".to_string(),
            },
            enum_type,
            span,
        )
    };
    let local_b = |span: crate::token::Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 3,
                name: "b".to_string(),
            },
            enum_type,
            span,
        )
    };

    // Ordering enum constructors
    let ordering_less = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 0,
            case_name: "Less".to_string(),
        },
        ordering_type,
        span,
    );
    let ordering_greater = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 2,
            case_name: "Greater".to_string(),
        },
        ordering_type,
        span,
    );
    let ordering_equal = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 1,
            case_name: "Equal".to_string(),
        },
        ordering_type,
        span,
    );

    // if a < b { return Ordering::Less; }
    let cond_lt = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(local_a(span)),
            op: TirBinaryOp::Lt,
            right: Box::new(local_b(span)),
        },
        TypeTable::BOOL,
        span,
    );
    let if_lt = TirStmt::new(
        TirStmtKind::If {
            condition: cond_lt,
            then_block: TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Return {
                        value: Some(ordering_less),
                    },
                    span,
                )],
                span,
            ),
            else_block: None,
        },
        span,
    );

    // if a > b { return Ordering::Greater; }
    let cond_gt = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(local_a(span)),
            op: TirBinaryOp::Gt,
            right: Box::new(local_b(span)),
        },
        TypeTable::BOOL,
        span,
    );
    let if_gt = TirStmt::new(
        TirStmtKind::If {
            condition: cond_gt,
            then_block: TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Return {
                        value: Some(ordering_greater),
                    },
                    span,
                )],
                span,
            ),
            else_block: None,
        },
        span,
    );

    // return Ordering::Equal;
    let return_equal = TirStmt::new(
        TirStmtKind::Return {
            value: Some(ordering_equal),
        },
        span,
    );

    let body = TirBlock::new(
        vec![
            TirStmt::new(
                TirStmtKind::Let {
                    name: "a".to_string(),
                    local_index: 2,
                    is_mut: false,
                    is_reactive: false,
                    type_id: enum_type,
                    value: deref_self,
                },
                span,
            ),
            TirStmt::new(
                TirStmtKind::Let {
                    name: "b".to_string(),
                    local_index: 3,
                    is_mut: false,
                    is_reactive: false,
                    type_id: enum_type,
                    value: deref_other,
                },
                span,
            ),
            if_lt,
            if_gt,
            return_equal,
        ],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        ordering_type,
        body,
        vec![ref_enum_type, ref_enum_type, enum_type, enum_type],
    )
}
