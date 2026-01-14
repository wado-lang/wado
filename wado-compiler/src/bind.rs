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

use std::collections::HashMap;

use crate::ast::{
    AssertStmt, Block, ClosureExpr, Expr, ExprStmt, ForStmt, Function, IfExpr, IfStmt, Item,
    LetStmt, MatchExpr, Module, ReturnStmt, Stmt, WhileStmt,
};
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
    depth: u32,
}

impl Scope {
    fn new(depth: u32) -> Self {
        Self {
            bindings: HashMap::new(),
            depth,
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
                    "{}:{}: error: use of undeclared variable '{}'",
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
                    "{}:{}: error: duplicate definition '{}' (first defined at {}:{})",
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

/// The binder performs local name resolution
pub struct Binder {
    scopes: Vec<Scope>,
    errors: Vec<BindError>,
    current_depth: u32,
}

impl Default for Binder {
    fn default() -> Self {
        Self::new()
    }
}

impl Binder {
    /// Create a new binder
    pub fn new() -> Self {
        Self {
            scopes: vec![Scope::new(0)], // Global scope
            errors: Vec::new(),
            current_depth: 0,
        }
    }

    /// Bind all local names in a module
    ///
    /// Returns Ok(()) if successful, or Err with all binding errors found.
    pub fn bind_module(&mut self, module: &Module) -> Result<(), Vec<BindError>> {
        for item in &module.items {
            self.bind_item(item);
        }

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(std::mem::take(&mut self.errors))
        }
    }

    /// Bind an item (only functions have local scopes)
    fn bind_item(&mut self, item: &Item) {
        if let Item::Function(func) = item {
            self.bind_function(func);
        }
        // Impl blocks contain functions
        if let Item::Impl(impl_block) = item {
            for method in &impl_block.methods {
                self.bind_function(method);
            }
        }
    }

    /// Bind a function's local variables
    fn bind_function(&mut self, func: &Function) {
        self.enter_scope();

        // Bind parameters as local variables
        for param in &func.params {
            self.define(&param.name, false, false, param.span);
        }

        // Bind body
        if let Some(ref body) = func.body {
            self.bind_block_contents(body);
        }

        self.exit_scope();
    }

    /// Bind statements in a block (without creating a new scope)
    fn bind_block_contents(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.bind_stmt(stmt);
        }
    }

    /// Bind a block (creates a new scope)
    fn bind_block(&mut self, block: &Block) {
        self.enter_scope();
        self.bind_block_contents(block);
        self.exit_scope();
    }

    /// Bind a statement
    fn bind_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(let_stmt) => self.bind_let(let_stmt),
            Stmt::Expr(expr_stmt) => self.bind_expr_stmt(expr_stmt),
            Stmt::Return(ret_stmt) => self.bind_return(ret_stmt),
            Stmt::If(if_stmt) => self.bind_if_stmt(if_stmt),
            Stmt::While(while_stmt) => self.bind_while(while_stmt),
            Stmt::For(for_stmt) => self.bind_for(for_stmt),
            Stmt::Assert(assert_stmt) => self.bind_assert(assert_stmt),
        }
    }

    /// Bind a let statement
    fn bind_let(&mut self, let_stmt: &LetStmt) {
        // First bind the value expression (uses variables from outer scope)
        self.bind_expr(&let_stmt.value);

        // Then define the variable
        self.define(
            &let_stmt.name,
            let_stmt.is_mut,
            let_stmt.is_reactive,
            let_stmt.span,
        );
    }

    /// Bind an expression statement
    fn bind_expr_stmt(&mut self, expr_stmt: &ExprStmt) {
        self.bind_expr(&expr_stmt.expr);
    }

    /// Bind a return statement
    fn bind_return(&mut self, ret_stmt: &ReturnStmt) {
        if let Some(ref value) = ret_stmt.value {
            self.bind_expr(value);
        }
    }

    /// Bind an if statement
    fn bind_if_stmt(&mut self, if_stmt: &IfStmt) {
        self.bind_expr(&if_stmt.condition);
        self.bind_block(&if_stmt.then_block);
        if let Some(ref else_block) = if_stmt.else_block {
            self.bind_block(else_block);
        }
    }

    /// Bind a while statement
    fn bind_while(&mut self, while_stmt: &WhileStmt) {
        self.bind_expr(&while_stmt.condition);
        self.bind_block(&while_stmt.body);
    }

    /// Bind a for statement
    fn bind_for(&mut self, for_stmt: &ForStmt) {
        self.enter_scope();

        // Bind init statement
        if let Some(ref init) = for_stmt.init {
            self.bind_stmt(init);
        }

        // Bind condition
        if let Some(ref condition) = for_stmt.condition {
            self.bind_expr(condition);
        }

        // Bind update
        if let Some(ref update) = for_stmt.update {
            self.bind_expr(update);
        }

        // Bind body
        self.bind_block(&for_stmt.body);

        self.exit_scope();
    }

    /// Bind an assert statement
    fn bind_assert(&mut self, assert_stmt: &AssertStmt) {
        self.bind_expr(&assert_stmt.condition);
        if let Some(ref message) = assert_stmt.message {
            self.bind_expr(message);
        }
    }

    /// Bind an expression
    fn bind_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(ident) => {
                // Check if the identifier is defined
                if self.lookup(&ident.name).is_none() {
                    // Only report error for local-looking names
                    // (not module paths like Stdout::write)
                    self.errors.push(BindError::UseBeforeDefine {
                        name: ident.name.clone(),
                        used_at: ident.span,
                    });
                }
            }

            Expr::Assign(assign) => {
                // Check mutability for simple variable assignments
                if let Expr::Ident(ident) = &assign.target
                    && let Some(binding) = self.lookup(&ident.name)
                    && !binding.is_mut
                {
                    self.errors.push(BindError::AssignToImmutable {
                        name: ident.name.clone(),
                        span: assign.span,
                    });
                }
                self.bind_expr(&assign.target);
                self.bind_expr(&assign.value);
            }

            Expr::CompoundAssign(compound) => {
                // Check mutability
                if let Expr::Ident(ident) = &compound.target
                    && let Some(binding) = self.lookup(&ident.name)
                    && !binding.is_mut
                {
                    self.errors.push(BindError::AssignToImmutable {
                        name: ident.name.clone(),
                        span: compound.span,
                    });
                }
                self.bind_expr(&compound.target);
                self.bind_expr(&compound.value);
            }

            Expr::Binary(binary) => {
                self.bind_expr(&binary.left);
                self.bind_expr(&binary.right);
            }

            Expr::Unary(unary) => {
                self.bind_expr(&unary.expr);
            }

            Expr::Call(call) => {
                self.bind_expr(&call.callee);
                for arg in &call.args {
                    self.bind_expr(arg);
                }
            }

            Expr::MethodCall(method_call) => {
                self.bind_expr(&method_call.receiver);
                for arg in &method_call.args {
                    self.bind_expr(arg);
                }
            }

            Expr::FieldAccess(field_access) => {
                self.bind_expr(&field_access.expr);
            }

            Expr::Index(index) => {
                self.bind_expr(&index.expr);
                self.bind_expr(&index.index);
            }

            Expr::Block(block) => {
                self.bind_block(block);
            }

            Expr::If(if_expr) => {
                self.bind_if_expr(if_expr);
            }

            Expr::Match(match_expr) => {
                self.bind_match_expr(match_expr);
            }

            Expr::Closure(closure) => {
                self.bind_closure(closure);
            }

            Expr::TemplateString(template) => {
                for part in &template.parts {
                    if let crate::ast::TemplatePart::Interpolation { expr, .. } = part {
                        self.bind_expr(expr);
                    }
                }
            }

            Expr::Cast(cast) => {
                self.bind_expr(&cast.expr);
            }

            Expr::StructLiteral(struct_lit) => {
                for field in &struct_lit.fields {
                    self.bind_expr(&field.value);
                }
            }

            Expr::ComparisonChain(chain) => {
                self.bind_expr(&chain.first);
                for comparison in &chain.comparisons {
                    self.bind_expr(&comparison.right);
                }
            }

            // Literals don't reference variables
            Expr::Literal(_) => {}
        }
    }

    /// Bind an if expression
    fn bind_if_expr(&mut self, if_expr: &IfExpr) {
        self.bind_expr(&if_expr.condition);
        self.bind_block(&if_expr.then_block);
        if let Some(ref else_block) = if_expr.else_block {
            self.bind_block(else_block);
        }
    }

    /// Bind a match expression
    fn bind_match_expr(&mut self, match_expr: &MatchExpr) {
        self.bind_expr(&match_expr.expr);
        for arm in &match_expr.arms {
            self.enter_scope();
            // Bind pattern (introduces variables)
            self.bind_pattern(&arm.pattern, arm.span);
            // Bind arm body
            self.bind_expr(&arm.body);
            self.exit_scope();
        }
    }

    /// Bind a pattern (may introduce variables)
    fn bind_pattern(&mut self, pattern: &crate::ast::Pattern, span: Span) {
        match pattern {
            crate::ast::Pattern::Ident(name) => {
                // Pattern bindings are immutable by default
                self.define(name, false, false, span);
            }
            crate::ast::Pattern::Tuple(patterns) => {
                for p in patterns {
                    self.bind_pattern(p, span);
                }
            }
            crate::ast::Pattern::Literal(_) | crate::ast::Pattern::Wildcard => {
                // No variables introduced
            }
        }
    }

    /// Bind a closure
    fn bind_closure(&mut self, closure: &ClosureExpr) {
        self.enter_scope();

        // Bind parameters
        for param in &closure.params {
            self.define(&param.name, false, false, closure.span);
        }

        // Bind body
        self.bind_expr(&closure.body);

        self.exit_scope();
    }

    /// Enter a new scope
    fn enter_scope(&mut self) {
        self.current_depth += 1;
        self.scopes.push(Scope::new(self.current_depth));
    }

    /// Exit the current scope
    fn exit_scope(&mut self) {
        self.scopes.pop();
        self.current_depth -= 1;
    }

    /// Define a variable in the current scope
    fn define(&mut self, name: &str, is_mut: bool, is_reactive: bool, span: Span) {
        let scope = self.scopes.last_mut().unwrap();

        // Check for duplicate in the same scope
        if let Some(existing) = scope.bindings.get(name) {
            self.errors.push(BindError::DuplicateInScope {
                name: name.to_string(),
                first: existing.defined_at,
                second: span,
            });
            return;
        }

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
pub fn bind_module(module: &Module) -> Result<(), Vec<BindError>> {
    let mut binder = Binder::new();
    binder.bind_module(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();
        let mut parser = Parser::new(tokens);
        parser.parse().unwrap()
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
        assert!(bind_module(&module).is_ok());
    }

    #[test]
    fn test_use_before_define() {
        let module = parse(
            r#"
            fn run() {
                let y = x;
                let x = 1;
            }
        "#,
        );
        let result = bind_module(&module);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(matches!(errors[0], BindError::UseBeforeDefine { .. }));
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
        let result = bind_module(&module);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(matches!(errors[0], BindError::DuplicateInScope { .. }));
    }

    #[test]
    fn test_shadowing_in_nested_scope() {
        // Shadowing in nested scope is allowed
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
        assert!(bind_module(&module).is_ok());
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
        let result = bind_module(&module);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(matches!(errors[0], BindError::AssignToImmutable { .. }));
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
        assert!(bind_module(&module).is_ok());
    }
}
