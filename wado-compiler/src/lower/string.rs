use crate::hashmap::{IndexMap, IndexSet};

use crate::name::LocalMethodName;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirModule, TirStmt, TirStmtKind};

pub(super) struct StringCollector {
    strings: IndexSet<String>,
    bytes: IndexSet<Vec<u8>>,
    /// Map of function name → strings in that function (for DCE filtering)
    function_strings: IndexMap<String, IndexSet<String>>,
    /// Map of function name → method info (for DCE to avoid parsing)
    function_method_info: IndexMap<String, Option<LocalMethodName>>,
    /// Current function being collected (for tracking)
    current_function: Option<String>,
}

impl StringCollector {
    pub(super) fn new() -> Self {
        Self {
            strings: IndexSet::default(),
            bytes: IndexSet::default(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
            current_function: None,
        }
    }

    pub(super) fn into_results(
        self,
    ) -> (
        Vec<String>,
        Vec<Vec<u8>>,
        IndexMap<String, Vec<String>>,
        IndexMap<String, Option<LocalMethodName>>,
    ) {
        let strings = self.strings.into_iter().collect();
        let bytes = self.bytes.into_iter().collect();
        let function_strings = self
            .function_strings
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect();
        (strings, bytes, function_strings, self.function_method_info)
    }

    fn add_string(&mut self, s: String) {
        self.strings.insert(s.clone());
        // Also track which function this string belongs to
        if let Some(func_name) = &self.current_function {
            self.function_strings
                .entry(func_name.clone())
                .or_default()
                .insert(s);
        }
    }

    pub(super) fn collect_module(&mut self, module: &TirModule) {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.current_function = Some(func.name.clone());
                self.function_method_info
                    .insert(func.name.clone(), func.method_info.clone());
                self.collect_block(body);
                self.current_function = None;
            }
        }
        // Also collect from trait impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.current_function = Some(method.name.clone());
                    self.function_method_info
                        .insert(method.name.clone(), method.method_info.clone());
                    self.collect_block(body);
                    self.current_function = None;
                }
            }
        }
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
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_expr(scrutinee);
                self.collect_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_block(else_blk);
                }
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
            | TirExprKind::TupleSpread { expr: inner } => {
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
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_expr(functor);
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
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.collect_expr(expr);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_expr(scrutinee);
                for arm in arms {
                    self.collect_block(arm);
                }
                self.collect_block(default);
            }
            // Literals and simple expressions don't contain strings
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
        }
    }
}
