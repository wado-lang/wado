//! Collect string and bytes literals referenced by function bodies.
//!
//! The translator copies these into the resulting [`crate::nir_package::NirPackage`]'s
//! data-section fields and per-function DCE maps — codegen looks them up
//! by `(module_source, function name)` to decide which literals to emit.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind};

pub struct StringPlan {
    pub string_literals: Vec<String>,
    pub bytes_literals: Vec<Vec<u8>>,
    /// Per-function literal usage; codegen DCE consults this to keep
    /// only the literals each kept function needs.
    pub function_strings: IndexMap<(ModuleSource, String), Vec<String>>,
    /// Per-function method-info cache; lets codegen DCE skip re-parsing
    /// `LocalMethodName` from the mangled function name.
    pub function_method_info: IndexMap<(ModuleSource, String), Option<LocalMethodName>>,
}

pub fn plan(flat: &FlatPackage) -> StringPlan {
    let mut collector = StringCollector::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        collector.collect_function(&func);
    }
    collector.into_plan()
}

struct StringCollector {
    strings: IndexSet<String>,
    bytes: IndexSet<Vec<u8>>,
    function_strings: IndexMap<(ModuleSource, String), IndexSet<String>>,
    function_method_info: IndexMap<(ModuleSource, String), Option<LocalMethodName>>,
    current_function: Option<(ModuleSource, String)>,
}

impl StringCollector {
    fn new() -> Self {
        Self {
            strings: IndexSet::default(),
            bytes: IndexSet::default(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
            current_function: None,
        }
    }

    fn into_plan(self) -> StringPlan {
        let function_strings = self
            .function_strings
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect();
        StringPlan {
            string_literals: self.strings.into_iter().collect(),
            bytes_literals: self.bytes.into_iter().collect(),
            function_strings,
            function_method_info: self.function_method_info,
        }
    }

    fn add_string(&mut self, s: String) {
        self.strings.insert(s.clone());
        if let Some(key) = &self.current_function {
            self.function_strings
                .entry(key.clone())
                .or_default()
                .insert(s);
        }
    }

    fn collect_function(&mut self, func: &TirFunction) {
        let Some(body) = &func.body else {
            return;
        };
        let key = (func.module_source.clone(), func.name.clone());
        self.current_function = Some(key.clone());
        self.function_method_info
            .insert(key, func.method_info.clone());
        self.collect_block(body);
        self.current_function = None;
    }

    fn collect_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.collect_stmt(stmt);
        }
    }

    fn collect_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.collect_expr(expr);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.collect_expr(expr);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_block(else_blk);
                }
            }
            TirStmtKind::Loop { body } => {
                self.collect_block(body);
            }
            TirStmtKind::Break { .. } | TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.collect_block(block);
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.collect_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    fn collect_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::StringLiteral(s) => {
                self.add_string(s.clone());
            }
            TirExprKind::BytesLiteral(b) => {
                self.bytes.insert(b.clone());
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_expr(left);
                self.collect_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.collect_expr(&arg.expr);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.collect_expr(receiver);
                for arg in args {
                    self.collect_expr(&arg.expr);
                }
            }
            TirExprKind::Block(block) => {
                self.collect_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.collect_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_expr(&field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_expr(elem);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_expr(target);
                self.collect_expr(value);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                self.collect_expr(inner);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_expr(array);
                self.collect_expr(index);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.collect_expr(guard);
                    }
                    self.collect_expr(&arm.body);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.collect_expr(body);
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_expr(callee);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_expr(payload_expr);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_expr(value);
            }
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.collect_expr(expr);
            }
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }
}
