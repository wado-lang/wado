//! Local name binding for Wado
//!
//! The bind phase performs local name resolution within function bodies:
//! - Tracks variable scopes (blocks, if/while/for)
//! - Detects use-before-define errors
//! - Detects duplicate definitions within the same scope
//! - Records variable mutability and reactivity
//!
//! This phase does NOT:
//! - Load modules or resolve imports (that's the analyzer's job)
//! - Resolve cross-module references (that's the resolve phase)
//! - Perform type checking (that's the resolve phase)

use std::collections::{HashMap, HashSet};

use crate::ast::{
    AssertStmt, Block, ClosureExpr, Condition, Expr, ExprStmt, ForOfStmt, ForStmt, Function,
    IfExpr, IfStmt, Item, LetStmt, LoopStmt, MatchExpr, Module, ReturnStmt, Stmt, WhileStmt,
};
use crate::compiler_host::CompilerHost;
use crate::logger::{Bail, Logger};
use crate::token::Span;

/// Binding information for a local variable
#[derive(Debug, Clone)]
pub struct BindingInfo {
    /// Variable name
    pub name: String,
    /// Whether the variable is mutable
    pub is_mut: bool,
    /// Whether the variable is reactive
    pub is_reactive: bool,
    /// Where the variable was defined
    pub defined_at: Span,
    /// Scope depth where the variable was defined
    pub scope_depth: u32,
}

/// A scope containing local variable bindings
#[derive(Debug)]
struct Scope {
    bindings: HashMap<String, BindingInfo>,
}

impl Scope {
    fn new() -> Self {
        Self {
            bindings: HashMap::new(),
        }
    }
}

/// Errors from the bind phase
#[derive(Debug, Clone)]
pub enum BindError {
    /// Variable used before it was defined
    UseBeforeDefine { name: String, used_at: Span },

    /// Duplicate definition in the same scope
    DuplicateInScope {
        name: String,
        first: Span,
        second: Span,
    },

    /// Assignment to an immutable variable
    AssignToImmutable { name: String, span: Span },
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::UseBeforeDefine { name, used_at } => {
                write!(
                    f,
                    "{}:{}: error: '{}' is not in scope",
                    used_at.line, used_at.column, name
                )
            }
            BindError::DuplicateInScope {
                name,
                first,
                second,
            } => {
                write!(
                    f,
                    "{}:{}: error: cannot redeclare '{}' in the same scope (first defined at {}:{})",
                    second.line, second.column, name, first.line, first.column
                )
            }
            BindError::AssignToImmutable { name, span } => {
                write!(
                    f,
                    "{}:{}: error: cannot assign to immutable variable '{}'",
                    span.line, span.column, name
                )
            }
        }
    }
}

impl std::error::Error for BindError {}

impl From<BindError> for crate::compiler_host::Diagnostic {
    fn from(e: BindError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let (code, message, span) = match &e {
            BindError::UseBeforeDefine { name, used_at } => (
                Code::UndefinedVariable,
                format!("'{name}' is not in scope"),
                *used_at,
            ),
            BindError::DuplicateInScope {
                name,
                first,
                second,
            } => (
                Code::DuplicateDefinition,
                format!(
                    "cannot redeclare '{name}' in the same scope (first defined at {}:{})",
                    first.line, first.column
                ),
                *second,
            ),
            BindError::AssignToImmutable { name, span } => (
                Code::ImmutableAssignment,
                format!("cannot assign to immutable variable '{name}'"),
                *span,
            ),
        };
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&span, None)),
        }
    }
}

/// The binder performs local name resolution
pub struct Binder<'a, H: CompilerHost> {
    scopes: Vec<Scope>,
    logger: &'a Logger<'a, H>,
    current_depth: u32,
    /// All local variable names defined in the current function
    /// Used to distinguish "out of scope" errors from "global reference"
    local_names_in_function: HashSet<String>,
}

impl<'a, H: CompilerHost> Binder<'a, H> {
    /// Create a new binder
    pub fn new(logger: &'a Logger<'a, H>) -> Self {
        Self {
            scopes: vec![Scope::new()], // Global scope
            logger,
            current_depth: 0,
            local_names_in_function: HashSet::new(),
        }
    }

    /// Bind all local names in a module
    ///
    /// Errors are emitted to the logger. Returns `Err(Bail)` if any errors found.
    pub fn bind_module(&mut self, module: &Module) -> Result<(), Bail> {
        let bail = self.bind_module_inner(module).is_err();
        if bail || self.logger.has_errors() {
            return Err(Bail);
        }
        Ok(())
    }

    fn bind_module_inner(&mut self, module: &Module) -> Result<(), Bail> {
        for item in &module.items {
            self.bind_item(item)?;
        }
        Ok(())
    }

    /// Bind an item (only functions have local scopes)
    fn bind_item(&mut self, item: &Item) -> Result<(), Bail> {
        if let Item::Function(func) = item {
            self.bind_function(func)?;
        }
        // Impl blocks contain functions
        if let Item::Impl(impl_block) = item {
            for method in &impl_block.methods {
                self.bind_function(method)?;
            }
        }
        // Trait declarations contain method signatures (with optional bodies)
        if let Item::Trait(trait_decl) = item {
            for method in &trait_decl.methods {
                self.bind_function(method)?;
            }
        }
        Ok(())
    }

    /// Bind a function's local variables
    fn bind_function(&mut self, func: &Function) -> Result<(), Bail> {
        // Clear local names for this function
        self.local_names_in_function.clear();

        self.enter_scope();

        // Bind parameters as local variables
        for param in &func.params {
            self.define(&param.name, false, false, param.span)?;
        }

        // Bind body
        if let Some(ref body) = func.body {
            self.bind_block_contents(body)?;
        }

        self.exit_scope();
        Ok(())
    }

    /// Bind statements in a block (without creating a new scope)
    fn bind_block_contents(&mut self, block: &Block) -> Result<(), Bail> {
        for stmt in &block.stmts {
            self.bind_stmt(stmt)?;
        }
        Ok(())
    }

    /// Bind a block (creates a new scope)
    fn bind_block(&mut self, block: &Block) -> Result<(), Bail> {
        self.enter_scope();
        self.bind_block_contents(block)?;
        self.exit_scope();
        Ok(())
    }

    /// Bind a statement
    fn bind_stmt(&mut self, stmt: &Stmt) -> Result<(), Bail> {
        match stmt {
            Stmt::Let(let_stmt) => self.bind_let(let_stmt)?,
            Stmt::Expr(expr_stmt) => self.bind_expr_stmt(expr_stmt)?,
            Stmt::Return(ret_stmt) => self.bind_return(ret_stmt)?,
            Stmt::If(if_stmt) => self.bind_if_stmt(if_stmt)?,
            Stmt::While(while_stmt) => self.bind_while(while_stmt)?,
            Stmt::For(for_stmt) => self.bind_for(for_stmt)?,
            Stmt::ForOf(for_of_stmt) => self.bind_for_of(for_of_stmt)?,
            Stmt::Loop(loop_stmt) => self.bind_loop(loop_stmt)?,
            Stmt::Break(_) => {}    // No bindings for break
            Stmt::Continue(_) => {} // No bindings for continue
            Stmt::Assert(assert_stmt) => self.bind_assert(assert_stmt)?,
            Stmt::LabeledBlock(labeled_block) => self.bind_block(&labeled_block.block)?,
        }
        Ok(())
    }

    /// Bind a let statement
    fn bind_let(&mut self, let_stmt: &LetStmt) -> Result<(), Bail> {
        // First bind the value expression (uses variables from outer scope)
        self.bind_expr(&let_stmt.value)?;

        // Then define the variables from the pattern
        self.bind_let_pattern(
            &let_stmt.pattern,
            let_stmt.is_mut,
            let_stmt.is_reactive,
            let_stmt.span,
        )
    }

    /// Bind a let pattern with mutability and reactivity information
    fn bind_let_pattern(
        &mut self,
        pattern: &crate::ast::Pattern,
        is_mut: bool,
        is_reactive: bool,
        span: Span,
    ) -> Result<(), Bail> {
        match pattern {
            crate::ast::Pattern::Ident(name) => {
                self.define(name, is_mut, is_reactive, span)?;
            }
            crate::ast::Pattern::Tuple(patterns) => {
                for p in patterns {
                    self.bind_let_pattern(p, is_mut, is_reactive, span)?;
                }
            }
            crate::ast::Pattern::Wildcard => {
                // No variable introduced for wildcard
            }
            crate::ast::Pattern::Literal(_) | crate::ast::Pattern::Variant { .. } => {
                // Literal and variant patterns are not valid in let statements
                // This would be caught by the type checker
            }
        }
        Ok(())
    }

    /// Bind an expression statement
    fn bind_expr_stmt(&mut self, expr_stmt: &ExprStmt) -> Result<(), Bail> {
        self.bind_expr(&expr_stmt.expr)
    }

    /// Bind a return statement
    fn bind_return(&mut self, ret_stmt: &ReturnStmt) -> Result<(), Bail> {
        if let Some(ref value) = ret_stmt.value {
            self.bind_expr(value)?;
        }
        Ok(())
    }

    /// Bind an if statement
    fn bind_if_stmt(&mut self, if_stmt: &IfStmt) -> Result<(), Bail> {
        // Handle optional init binding (scoped to this if statement)
        if if_stmt.init.is_some() {
            self.enter_scope();
        }

        if let Some(init) = &if_stmt.init {
            self.bind_let(init)?;
        }

        // For pattern conditions, enter scope before the condition so pattern bindings
        // are only visible in then_block (not in else_block or outer scope)
        let is_pattern = matches!(if_stmt.condition, Condition::Pattern { .. });
        if is_pattern {
            self.enter_scope();
        }

        self.bind_condition(&if_stmt.condition)?;
        self.bind_block(&if_stmt.then_block)?;

        if is_pattern {
            self.exit_scope();
        }

        if let Some(ref else_block) = if_stmt.else_block {
            self.bind_block(else_block)?;
        }

        if if_stmt.init.is_some() {
            self.exit_scope();
        }
        Ok(())
    }

    /// Bind a while statement
    fn bind_while(&mut self, while_stmt: &WhileStmt) -> Result<(), Bail> {
        // For pattern conditions, enter scope before the condition so pattern bindings
        // are visible in the body
        let is_pattern = matches!(while_stmt.condition, Condition::Pattern { .. });
        if is_pattern {
            self.enter_scope();
        }

        self.bind_condition(&while_stmt.condition)?;
        self.bind_block(&while_stmt.body)?;

        if is_pattern {
            self.exit_scope();
        }
        Ok(())
    }

    /// Bind a for statement
    fn bind_for(&mut self, for_stmt: &ForStmt) -> Result<(), Bail> {
        self.enter_scope();

        // Bind init statement
        if let Some(ref init) = for_stmt.init {
            self.bind_stmt(init)?;
        }

        // Bind condition (may be pattern or expression)
        if let Some(ref condition) = for_stmt.condition {
            self.bind_condition(condition)?;
        }

        // Bind update
        if let Some(ref update) = for_stmt.update {
            self.bind_expr(update)?;
        }

        // Bind body
        self.bind_block(&for_stmt.body)?;

        self.exit_scope();
        Ok(())
    }

    /// Bind a for-of statement: `for let item of array { ... }`
    fn bind_for_of(&mut self, for_of_stmt: &ForOfStmt) -> Result<(), Bail> {
        // First bind the iterable expression (uses variables from outer scope)
        self.bind_expr(&for_of_stmt.iterable)?;

        // Enter a new scope for the loop binding and body
        self.enter_scope();

        // Define the loop variable
        self.define(
            &for_of_stmt.binding,
            for_of_stmt.is_mut,
            false, // not reactive
            for_of_stmt.span,
        )?;

        // Bind body
        self.bind_block(&for_of_stmt.body)?;

        self.exit_scope();
        Ok(())
    }

    /// Bind a loop statement
    fn bind_loop(&mut self, loop_stmt: &LoopStmt) -> Result<(), Bail> {
        self.bind_block(&loop_stmt.body)
    }

    /// Bind an assert statement
    fn bind_assert(&mut self, assert_stmt: &AssertStmt) -> Result<(), Bail> {
        self.bind_expr(&assert_stmt.condition)?;
        if let Some(ref message) = assert_stmt.message {
            self.bind_expr(message)?;
        }
        Ok(())
    }

    /// Bind an expression
    fn bind_expr(&mut self, expr: &Expr) -> Result<(), Bail> {
        match expr {
            Expr::Ident(ident) => {
                // Check if the identifier is defined in any scope
                if self.lookup(&ident.name).is_none() {
                    // Only report error if this name was defined as a local variable
                    // somewhere in this function (but is now out of scope).
                    // Unknown names might be global functions/constants - those are
                    // resolved later by the analyzer.
                    if self.local_names_in_function.contains(&ident.name) {
                        self.logger.error(BindError::UseBeforeDefine {
                            name: ident.name.clone(),
                            used_at: ident.span,
                        })?;
                    }
                }
            }

            Expr::Assign(assign) => {
                // Check mutability for simple variable assignments
                if let Expr::Ident(ident) = &assign.target
                    && let Some(binding) = self.lookup(&ident.name)
                    && !binding.is_mut
                {
                    self.logger.error(BindError::AssignToImmutable {
                        name: ident.name.clone(),
                        span: assign.span,
                    })?;
                }
                self.bind_expr(&assign.target)?;
                self.bind_expr(&assign.value)?;
            }

            Expr::CompoundAssign(compound) => {
                // Check mutability
                if let Expr::Ident(ident) = &compound.target
                    && let Some(binding) = self.lookup(&ident.name)
                    && !binding.is_mut
                {
                    self.logger.error(BindError::AssignToImmutable {
                        name: ident.name.clone(),
                        span: compound.span,
                    })?;
                }
                self.bind_expr(&compound.target)?;
                self.bind_expr(&compound.value)?;
            }

            Expr::Binary(binary) => {
                self.bind_expr(&binary.left)?;
                self.bind_expr(&binary.right)?;
            }

            Expr::Unary(unary) => {
                self.bind_expr(&unary.expr)?;
            }

            Expr::Call(call) => {
                self.bind_expr(&call.callee)?;
                for arg in &call.args {
                    self.bind_expr(arg)?;
                }
            }

            Expr::MethodCall(method_call) => {
                self.bind_expr(&method_call.receiver)?;
                for arg in &method_call.args {
                    self.bind_expr(arg)?;
                }
            }

            Expr::StaticMethodCall(static_call) => {
                for arg in &static_call.args {
                    self.bind_expr(arg)?;
                }
            }

            Expr::FieldAccess(field_access) => {
                self.bind_expr(&field_access.expr)?;
            }

            Expr::Index(index) => {
                self.bind_expr(&index.expr)?;
                self.bind_expr(&index.index)?;
            }

            Expr::Block(block) => {
                self.bind_block(block)?;
            }

            Expr::If(if_expr) => {
                self.bind_if_expr(if_expr)?;
            }

            Expr::Match(match_expr) => {
                self.bind_match_expr(match_expr)?;
            }

            Expr::Closure(closure) => {
                self.bind_closure(closure)?;
            }

            Expr::TemplateString(template) => {
                for part in &template.parts {
                    if let crate::ast::TemplatePart::Interpolation { expr, .. } = part {
                        self.bind_expr(expr)?;
                    }
                }
            }

            Expr::Cast(cast) => {
                self.bind_expr(&cast.expr)?;
            }

            Expr::StructLiteral(struct_lit) => {
                for field in &struct_lit.fields {
                    self.bind_expr(&field.value)?;
                }
            }

            Expr::ComparisonChain(chain) => {
                self.bind_expr(&chain.first)?;
                for comparison in &chain.comparisons {
                    self.bind_expr(&comparison.right)?;
                }
            }

            Expr::TupleLiteral(tuple_lit) => {
                for element in &tuple_lit.elements {
                    self.bind_expr(element)?;
                }
            }

            Expr::LabeledBlock(lb) => {
                // Labeled block expression creates a new scope for its block
                self.enter_scope();
                self.bind_block(&lb.block)?;
                self.exit_scope();
            }

            Expr::Matches(matches_expr) => {
                // Bind the scrutinee expression
                self.bind_expr(&matches_expr.expr)?;
                // Pattern bindings are scoped to the matches expression only
                // (specifically, they're only visible in the guard if present)
                // Always enter a scope and bind pattern to track variable names,
                // so we can detect "use after scope exit" errors
                self.enter_scope();
                self.bind_pattern(&matches_expr.pattern, matches_expr.span)?;
                if let Some(guard) = &matches_expr.guard {
                    self.bind_expr(guard)?;
                }
                self.exit_scope();
            }

            // Literals don't reference variables
            Expr::Literal(_) => {}
        }
        Ok(())
    }

    /// Bind an if expression
    fn bind_if_expr(&mut self, if_expr: &IfExpr) -> Result<(), Bail> {
        // Handle optional init binding (scoped to this if expression)
        if if_expr.init.is_some() {
            self.enter_scope();
        }

        if let Some(init) = &if_expr.init {
            self.bind_let(init)?;
        }

        // For pattern conditions, enter scope before the condition so pattern bindings
        // are only visible in then_block (not in else_block or outer scope)
        let is_pattern = matches!(if_expr.condition, Condition::Pattern { .. });
        if is_pattern {
            self.enter_scope();
        }

        self.bind_condition(&if_expr.condition)?;
        self.bind_block(&if_expr.then_block)?;

        if is_pattern {
            self.exit_scope();
        }

        if let Some(ref else_block) = if_expr.else_block {
            self.bind_block(else_block)?;
        }

        if if_expr.init.is_some() {
            self.exit_scope();
        }
        Ok(())
    }

    /// Bind an if condition (expression or pattern match)
    fn bind_condition(&mut self, condition: &Condition) -> Result<(), Bail> {
        match condition {
            Condition::Expr(expr) => {
                self.bind_expr(expr)?;
            }
            Condition::Pattern { pattern, expr, .. } => {
                // First bind the expression being matched
                self.bind_expr(expr)?;
                // Then bind the pattern (introduces variables)
                // Note: pattern variables are scoped to the then-block
                // This is handled by the caller entering/exiting scope
                self.bind_pattern(pattern, expr.span())?;
            }
        }
        Ok(())
    }

    /// Bind a match expression
    fn bind_match_expr(&mut self, match_expr: &MatchExpr) -> Result<(), Bail> {
        self.bind_expr(&match_expr.expr)?;
        for arm in &match_expr.arms {
            self.enter_scope();
            // Bind pattern (introduces variables)
            self.bind_pattern(&arm.pattern, arm.span)?;
            // Bind optional guard
            if let Some(guard) = &arm.guard {
                self.bind_expr(guard)?;
            }
            // Bind arm body
            self.bind_expr(&arm.body)?;
            self.exit_scope();
        }
        Ok(())
    }

    /// Bind a pattern (may introduce variables)
    fn bind_pattern(&mut self, pattern: &crate::ast::Pattern, span: Span) -> Result<(), Bail> {
        match pattern {
            crate::ast::Pattern::Ident(name) => {
                // Pattern bindings are immutable by default
                self.define(name, false, false, span)?;
            }
            crate::ast::Pattern::Tuple(patterns) => {
                for p in patterns {
                    self.bind_pattern(p, span)?;
                }
            }
            crate::ast::Pattern::Variant {
                bindings,
                span: variant_span,
                ..
            } => {
                // Bind nested patterns in variant
                for p in bindings {
                    self.bind_pattern(p, *variant_span)?;
                }
            }
            crate::ast::Pattern::Literal(_) | crate::ast::Pattern::Wildcard => {
                // No variables introduced
            }
        }
        Ok(())
    }

    /// Bind a closure
    fn bind_closure(&mut self, closure: &ClosureExpr) -> Result<(), Bail> {
        self.enter_scope();

        // Bind parameters
        for param in &closure.params {
            self.define(&param.name, false, false, closure.span)?;
        }

        // Bind body
        self.bind_expr(&closure.body)?;

        self.exit_scope();
        Ok(())
    }

    /// Enter a new scope
    fn enter_scope(&mut self) {
        self.current_depth += 1;
        self.scopes.push(Scope::new());
    }

    /// Exit the current scope
    fn exit_scope(&mut self) {
        self.scopes.pop();
        self.current_depth -= 1;
    }

    /// Define a variable in the current scope
    fn define(
        &mut self,
        name: &str,
        is_mut: bool,
        is_reactive: bool,
        span: Span,
    ) -> Result<(), Bail> {
        let scope = self.scopes.last_mut().unwrap();

        // Check for duplicate in the same scope
        if let Some(existing) = scope.bindings.get(name) {
            self.logger.error(BindError::DuplicateInScope {
                name: name.to_string(),
                first: existing.defined_at,
                second: span,
            })?;
            return Ok(());
        }

        // Track this name as a local variable in the current function
        self.local_names_in_function.insert(name.to_string());

        scope.bindings.insert(
            name.to_string(),
            BindingInfo {
                name: name.to_string(),
                is_mut,
                is_reactive,
                defined_at: span,
                scope_depth: self.current_depth,
            },
        );
        Ok(())
    }

    /// Look up a variable by name (searches all scopes)
    fn lookup(&self, name: &str) -> Option<&BindingInfo> {
        // Search from innermost scope outward
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.bindings.get(name) {
                return Some(binding);
            }
        }
        None
    }
}

/// Convenience function to bind a module
pub fn bind_module<H: CompilerHost>(module: &Module, logger: &Logger<H>) -> Result<(), Bail> {
    let mut binder = Binder::new(logger);
    binder.bind_module(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::{InMemoryCompilerHost, LogLevel, Severity};
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();
        let mut parser = Parser::new(tokens);
        parser.parse().unwrap()
    }

    fn bind_and_check(module: &Module) -> (bool, Vec<crate::compiler_host::Diagnostic>) {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Error);
        let result = bind_module(module, &logger);
        (result.is_ok(), host.diagnostics())
    }

    #[test]
    fn test_simple_binding() {
        let module = parse(
            r#"
            fn run() {
                let x = 1;
                let y = x;
            }
        "#,
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_out_of_scope() {
        let module = parse(
            r#"
            fn run() {
                if true {
                    let x = 1;
                }
                let y = x;
            }
        "#,
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(!ok);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.contains("is not in scope"));
    }

    #[test]
    fn test_duplicate_in_scope() {
        let module = parse(
            r#"
            fn run() {
                let x = 1;
                let x = 2;
            }
        "#,
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(!ok);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("cannot redeclare"));
    }

    #[test]
    fn test_shadowing_in_nested_scope() {
        let module = parse(
            r#"
            fn run() {
                let x = 1;
                if true {
                    let x = 2;
                }
            }
        "#,
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_assign_to_immutable() {
        let module = parse(
            r#"
            fn run() {
                let x = 1;
                x = 2;
            }
        "#,
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(!ok);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("cannot assign to immutable"));
    }

    #[test]
    fn test_assign_to_mutable() {
        let module = parse(
            r#"
            fn run() {
                let mut x = 1;
                x = 2;
            }
        "#,
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok);
        assert!(diags.is_empty());
    }
}
