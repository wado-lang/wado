//! Constant propagation optimization for Wado TIR
//!
//! Propagates compile-time-known global variable values into their use sites.
//! When a global variable is not mutable and initialized with a scalar constant,
//! all `GlobalVarGet` references are replaced with the constant value.

use crate::hashmap::IndexMap;
use crate::name::ModuleSource;
use crate::flat_package::FlatPackage;
use crate::tir::{TirExpr, TirExprKind};

use crate::tir_visitor::{TirOptVisitor, opt_walk_expr, visit_project_functions};

#[derive(Debug, Clone)]
enum ConstValue {
    Int { value: u64, repr: String },
    Float { value: f64, repr: String },
    Bool(bool),
    Char(char),
}

impl ConstValue {
    fn to_expr_kind(&self) -> TirExprKind {
        match self {
            Self::Int { value, repr } => TirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Float { value, repr } => TirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Bool(b) => TirExprKind::BoolLiteral(*b),
            Self::Char(c) => TirExprKind::CharLiteral(*c),
        }
    }
}

fn extract_const_value(expr: &TirExpr) -> Option<ConstValue> {
    match &expr.kind {
        TirExprKind::IntLiteral { value, repr } => Some(ConstValue::Int {
            value: *value,
            repr: repr.clone(),
        }),
        TirExprKind::FloatLiteral { value, repr } => Some(ConstValue::Float {
            value: *value,
            repr: repr.clone(),
        }),
        TirExprKind::BoolLiteral(b) => Some(ConstValue::Bool(*b)),
        TirExprKind::CharLiteral(c) => Some(ConstValue::Char(*c)),
        _ => None,
    }
}

type GlobalKey = (ModuleSource, String);

/// Apply constant propagation to all functions in the project.
pub fn propagate_constants(project: &mut FlatPackage) -> bool {
    let mut constants: IndexMap<GlobalKey, ConstValue> = IndexMap::default();
    for global in &project.globals {
        if global.mutable {
            continue;
        }
        if let Some(value) = extract_const_value(&global.initializer) {
            constants.insert((global.module_source.clone(), global.name.clone()), value);
        }
    }
    if constants.is_empty() {
        return false;
    }

    let mut visitor = ConstPropVisitor {
        constants: &constants,
    };
    visit_project_functions(project, &mut visitor)
}

struct ConstPropVisitor<'a> {
    constants: &'a IndexMap<GlobalKey, ConstValue>,
}

impl TirOptVisitor for ConstPropVisitor<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        if let TirExprKind::GlobalVarGet {
            module_source,
            name,
        } = &expr.kind
        {
            let key = (module_source.clone(), name.clone());
            if let Some(const_value) = self.constants.get(&key) {
                expr.kind = const_value.to_expr_kind();
                return true;
            }
        }
        opt_walk_expr(self, expr)
    }
}
