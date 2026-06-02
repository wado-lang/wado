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

use crate::hashmap::IndexSet;

use crate::hashmap::IndexMap;

use crate::ast::{
    AssertStmt, Block, ClosureExpr, Condition, ConditionElement, Expr, ExprStmt, ForOfStmt,
    ForStmt, Function, IfExpr, IfStmt, Item, LetStmt, LoopStmt, MatchExpr, Module, ReturnStmt,
    Stmt, WhileStmt,
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
    bindings: IndexMap<String, BindingInfo>,
}

impl Scope {
    fn new() -> Self {
        Self {
            bindings: IndexMap::default(),
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

    /// Variable used before it was definitely initialized
    UseBeforeInit { name: String, span: Span },
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
            BindError::UseBeforeInit { name, span } => {
                write!(
                    f,
                    "{}:{}: error: '{}' is used before initialization",
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
                    "cannot redeclare '{name}' in the same scope (first defined at {}:{})\n  hint: shadowing is allowed when the new value is derived from the old one (e.g., `let {name} = {name} + 1`)",
                    first.line, first.column
                ),
                *second,
            ),
            BindError::AssignToImmutable { name, span } => (
                Code::ImmutableAssignment,
                format!("cannot assign to immutable variable '{name}'"),
                *span,
            ),
            BindError::UseBeforeInit { name, span } => (
                Code::UninitializedVariable,
                format!("'{name}' is used before initialization"),
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

/// Check whether an expression contains a variable reference to `name`.
///
/// This is used to decide whether `let x = <expr>` is a self-referential
/// shadowing (e.g., `let x = x + 1`). The walk skips closure bodies when
/// a parameter shadows `name`, since that `name` refers to the parameter
/// rather than the outer variable.
fn expr_references_var(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Ident(ident) => ident.name == name,

        // Closure: skip body if a parameter shadows the name
        Expr::Closure(closure) => {
            let param_shadows = closure.params.iter().any(|p| p.name == name);
            if param_shadows {
                false
            } else {
                expr_references_var(&closure.body, name)
            }
        }

        // Field access / method call: the *receiver* can reference `name`,
        // but the field/method name itself is not a variable reference.
        Expr::FieldAccess(fa) => expr_references_var(&fa.expr, name),
        Expr::MethodCall(mc) => {
            expr_references_var(&mc.receiver, name)
                || mc.args.iter().any(|a| expr_references_var(a, name))
        }

        // Recurse into sub-expressions
        Expr::Binary(b) => {
            expr_references_var(&b.left, name) || expr_references_var(&b.right, name)
        }
        Expr::Unary(u) => expr_references_var(&u.expr, name),
        Expr::Call(c) => {
            expr_references_var(&c.callee, name)
                || c.args.iter().any(|a| expr_references_var(a, name))
        }
        Expr::StaticMethodCall(sc) => sc.args.iter().any(|a| expr_references_var(a, name)),
        Expr::Index(idx) => {
            expr_references_var(&idx.expr, name) || expr_references_var(&idx.index, name)
        }
        Expr::Cast(c) => expr_references_var(&c.expr, name),
        Expr::TryOp(t) => expr_references_var(&t.expr, name),
        Expr::Spread(inner, _) => expr_references_var(inner, name),
        Expr::Range(range) => {
            expr_references_var(&range.start, name) || expr_references_var(&range.end, name)
        }

        Expr::If(if_expr) => {
            condition_references_var(&if_expr.condition, name)
                || if_expr
                    .then_block
                    .stmts
                    .iter()
                    .any(|s| stmt_references_var(s, name))
                || if_expr
                    .else_block
                    .as_ref()
                    .is_some_and(|b| b.stmts.iter().any(|s| stmt_references_var(s, name)))
        }
        Expr::Match(m) => {
            expr_references_var(&m.expr, name)
                || m.arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .is_some_and(|g| expr_references_var(g, name))
                        || expr_references_var(&arm.body, name)
                })
        }
        Expr::Matches(m) => {
            expr_references_var(&m.expr, name)
                || m.guard
                    .as_ref()
                    .is_some_and(|g| expr_references_var(g, name))
        }

        Expr::Block(block) => block.stmts.iter().any(|s| stmt_references_var(s, name)),
        Expr::LabeledBlock(lb) => lb.block.stmts.iter().any(|s| stmt_references_var(s, name)),

        Expr::TemplateString(ts) => ts.parts.iter().any(|part| {
            if let crate::ast::TemplatePart::Interpolation { expr, .. } = part {
                expr_references_var(expr, name)
            } else {
                false
            }
        }),
        Expr::TupleLiteral(t) => t.elements.iter().any(|e| expr_references_var(e, name)),
        Expr::StructLiteral(s) => s.fields.iter().any(|f| expr_references_var(&f.value, name)),
        Expr::ComparisonChain(cc) => {
            expr_references_var(&cc.first, name)
                || cc
                    .comparisons
                    .iter()
                    .any(|c| expr_references_var(&c.right, name))
        }
        Expr::Assign(a) => {
            expr_references_var(&a.target, name) || expr_references_var(&a.value, name)
        }
        Expr::CompoundAssign(ca) => {
            expr_references_var(&ca.target, name) || expr_references_var(&ca.value, name)
        }
        Expr::WithHandler(w) => {
            w.handlers
                .iter()
                .any(|b| expr_references_var(&b.handler, name))
                || w.body.stmts.iter().any(|s| stmt_references_var(s, name))
        }
        Expr::Resume(r) => expr_references_var(&r.value, name),

        Expr::Literal(_) | Expr::Error(_) => false,
    }
}

fn stmt_references_var(stmt: &crate::ast::Stmt, name: &str) -> bool {
    match stmt {
        crate::ast::Stmt::Let(let_stmt) => let_stmt
            .value
            .as_ref()
            .is_some_and(|v| expr_references_var(v, name)),
        crate::ast::Stmt::Expr(expr_stmt) => expr_references_var(&expr_stmt.expr, name),
        crate::ast::Stmt::Return(ret) => ret
            .value
            .as_ref()
            .is_some_and(|v| expr_references_var(v, name)),
        crate::ast::Stmt::TaskReturn(tr) => expr_references_var(&tr.value, name),
        crate::ast::Stmt::Assert(a) => {
            expr_references_var(&a.condition, name)
                || a.message
                    .as_ref()
                    .is_some_and(|m| expr_references_var(m, name))
        }
        crate::ast::Stmt::If(if_stmt) => {
            condition_references_var(&if_stmt.condition, name)
                || if_stmt
                    .then_block
                    .stmts
                    .iter()
                    .any(|s| stmt_references_var(s, name))
                || if_stmt
                    .else_block
                    .as_ref()
                    .is_some_and(|b| b.stmts.iter().any(|s| stmt_references_var(s, name)))
        }
        crate::ast::Stmt::While(w) => {
            condition_references_var(&w.condition, name)
                || w.body.stmts.iter().any(|s| stmt_references_var(s, name))
        }
        crate::ast::Stmt::For(f) => {
            f.init
                .as_ref()
                .is_some_and(|i| stmt_references_var(i, name))
                || f.condition
                    .as_ref()
                    .is_some_and(|c| condition_references_var(c, name))
                || f.update
                    .as_ref()
                    .is_some_and(|u| expr_references_var(u, name))
                || f.body.stmts.iter().any(|s| stmt_references_var(s, name))
        }
        crate::ast::Stmt::ForOf(fo) => {
            expr_references_var(&fo.iterable, name)
                || fo.body.stmts.iter().any(|s| stmt_references_var(s, name))
        }
        crate::ast::Stmt::Loop(l) => l.body.stmts.iter().any(|s| stmt_references_var(s, name)),
        crate::ast::Stmt::Match(m) => {
            expr_references_var(&m.expr, name)
                || m.arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .is_some_and(|g| expr_references_var(g, name))
                        || expr_references_var(&arm.body, name)
                })
        }
        crate::ast::Stmt::LabeledBlock(lb) => {
            lb.block.stmts.iter().any(|s| stmt_references_var(s, name))
        }
        crate::ast::Stmt::Break(_) | crate::ast::Stmt::Continue(_) => false,
    }
}

fn condition_references_var(condition: &crate::ast::Condition, name: &str) -> bool {
    match condition {
        crate::ast::Condition::Expr(expr) => expr_references_var(expr, name),
        crate::ast::Condition::LetChain { elements, .. } => {
            elements.iter().any(|elem| match elem {
                crate::ast::ConditionElement::Let { expr, .. } => expr_references_var(expr, name),
                crate::ast::ConditionElement::Expr(expr) => expr_references_var(expr, name),
            })
        }
    }
}

/// Collect bare `Pattern::Ident` names at the top level of a pattern,
/// including through or-alternatives. These are candidates for global
/// constant references that should not be tracked as local variables.
/// Nested idents inside variants/structs/tuples are real local bindings.
fn collect_top_level_bare_idents(pattern: &crate::ast::Pattern) -> Vec<String> {
    match pattern {
        crate::ast::Pattern::Ident { name, .. } => vec![name.clone()],
        crate::ast::Pattern::Or(alternatives) => alternatives
            .iter()
            .flat_map(collect_top_level_bare_idents)
            .collect(),
        _ => vec![],
    }
}

/// The binder performs local name resolution
pub struct Binder<'a, H: CompilerHost> {
    scopes: Vec<Scope>,
    logger: &'a Logger<'a, H>,
    current_depth: u32,
    /// All local variable names defined in the current function
    /// Used to distinguish "out of scope" errors from "global reference"
    local_names_in_function: IndexSet<String>,
    /// Variables declared without an initializer that have not yet been
    /// definitely assigned on all paths reaching the current point.
    /// Key: (`scope_depth`, name) — `scope_depth` disambiguates shadowed vars.
    possibly_uninit: IndexSet<(u32, String)>,
}

impl<'a, H: CompilerHost> Binder<'a, H> {
    /// Create a new binder
    pub fn new(logger: &'a Logger<'a, H>) -> Self {
        Self {
            scopes: vec![Scope::new()], // Global scope
            logger,
            current_depth: 0,
            local_names_in_function: IndexSet::default(),
            possibly_uninit: IndexSet::default(),
        }
    }

    /// Returns true if the innermost binding for `name` is possibly uninitialized.
    fn is_possibly_uninit(&self, name: &str) -> bool {
        if let Some(binding) = self.lookup(name) {
            self.possibly_uninit
                .contains(&(binding.scope_depth, name.to_string()))
        } else {
            false
        }
    }

    /// Mark the innermost binding for `name` as definitely initialized.
    fn mark_initialized(&mut self, name: &str) {
        if let Some(binding) = self.lookup(name) {
            self.possibly_uninit
                .shift_remove(&(binding.scope_depth, name.to_string()));
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
        // Clear per-function state
        self.local_names_in_function.clear();
        self.possibly_uninit.clear();

        self.enter_scope();

        // Bind parameters as local variables
        for param in &func.params {
            self.define(&param.name, param.is_mut, false, param.span)?;
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
            Stmt::TaskReturn(stmt) => self.bind_expr(&stmt.value)?,
            Stmt::If(if_stmt) => self.bind_if_stmt(if_stmt)?,
            Stmt::While(while_stmt) => self.bind_while(while_stmt)?,
            Stmt::For(for_stmt) => self.bind_for(for_stmt)?,
            Stmt::ForOf(for_of_stmt) => self.bind_for_of(for_of_stmt)?,
            Stmt::Loop(loop_stmt) => self.bind_loop(loop_stmt)?,
            Stmt::Match(match_expr) => self.bind_match_expr(match_expr)?,
            Stmt::Break(_) => {}    // No bindings for break
            Stmt::Continue(_) => {} // No bindings for continue
            Stmt::Assert(assert_stmt) => self.bind_assert(assert_stmt)?,
            Stmt::LabeledBlock(labeled_block) => self.bind_block(&labeled_block.block)?,
        }
        Ok(())
    }

    /// Bind a let statement
    fn bind_let(&mut self, let_stmt: &LetStmt) -> Result<(), Bail> {
        if let Some(ref value) = let_stmt.value {
            // Initialized let: bind the initializer first (uses outer scope vars)
            self.bind_expr(value)?;

            // Check if this is a same-scope shadowing with self-reference
            // (e.g., `let x = x + 1`). The old binding must remain in scope
            // during bind_expr above so the RHS can reference it. Now that
            // the RHS is bound, remove the old binding before define() so it
            // won't report a duplicate.
            if let crate::ast::Pattern::Ident { name, .. }
            | crate::ast::Pattern::MutIdent { name, .. } = &let_stmt.pattern
            {
                let is_duplicate_in_scope = self
                    .scopes
                    .last()
                    .is_some_and(|scope| scope.bindings.contains_key(name));
                if is_duplicate_in_scope && expr_references_var(value, name) {
                    self.scopes
                        .last_mut()
                        .unwrap()
                        .bindings
                        .shift_remove(name.as_str());
                }
            }

            // Define the variables from the pattern as initialized
            self.bind_let_pattern(
                &let_stmt.pattern,
                let_stmt.is_mut,
                let_stmt.is_reactive,
                let_stmt.span,
            )
        } else {
            // Uninitialized let (`let x: T;`): define as possibly uninitialized.
            // Type annotation is guaranteed by the parser.
            self.bind_let_pattern_uninit(
                &let_stmt.pattern,
                let_stmt.is_mut,
                let_stmt.is_reactive,
                let_stmt.span,
            )
        }
    }

    /// Like `bind_let_pattern` but registers variables as possibly uninitialized.
    fn bind_let_pattern_uninit(
        &mut self,
        pattern: &crate::ast::Pattern,
        is_mut: bool,
        is_reactive: bool,
        span: Span,
    ) -> Result<(), Bail> {
        match pattern {
            crate::ast::Pattern::Ident { name, .. }
            | crate::ast::Pattern::MutIdent { name, .. } => {
                self.define_uninit(name, is_mut, is_reactive, span)?;
            }
            crate::ast::Pattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.bind_let_pattern_uninit(p, is_mut, is_reactive, span)?;
                }
            }
            crate::ast::Pattern::Wildcard => {}
            crate::ast::Pattern::Struct { fields, .. } => {
                for field in fields {
                    self.bind_let_pattern_uninit(&field.pattern, is_mut, is_reactive, span)?;
                }
            }
            crate::ast::Pattern::Literal(_)
            | crate::ast::Pattern::Variant { .. }
            | crate::ast::Pattern::Range { .. } => {}
            crate::ast::Pattern::Or(alternatives) => {
                // Bind variables from the first alternative (all alternatives must bind the same names)
                if let Some(first) = alternatives.first() {
                    self.bind_let_pattern_uninit(first, is_mut, is_reactive, span)?;
                }
            }
        }
        Ok(())
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
            crate::ast::Pattern::Ident { name, .. }
            | crate::ast::Pattern::MutIdent { name, .. } => {
                self.define(name, is_mut, is_reactive, span)?;
            }
            crate::ast::Pattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.bind_let_pattern(p, is_mut, is_reactive, span)?;
                }
            }
            crate::ast::Pattern::Wildcard => {
                // No variable introduced for wildcard
            }
            crate::ast::Pattern::Struct { fields, .. } => {
                for field in fields {
                    self.bind_let_pattern(&field.pattern, is_mut, is_reactive, span)?;
                }
            }
            crate::ast::Pattern::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    self.bind_let_pattern(first, is_mut, is_reactive, span)?;
                }
            }
            crate::ast::Pattern::Literal(_)
            | crate::ast::Pattern::Variant { .. }
            | crate::ast::Pattern::Range { .. } => {
                // Literal, variant, and range patterns are not valid in let statements
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
        let is_let_chain = matches!(if_stmt.condition, Condition::LetChain { .. });
        if is_let_chain {
            // Enter one scope for all chain elements and then_block.
            // Pattern bindings are visible in subsequent elements and then_block,
            // but NOT in else_block (scoped out before else).
            self.enter_scope();
        }

        self.bind_condition(&if_stmt.condition)?;

        // Snapshot possibly_uninit before diverging branches.
        let uninit_before = self.possibly_uninit.clone();

        self.bind_block(&if_stmt.then_block)?;
        let uninit_after_then = self.possibly_uninit.clone();

        if is_let_chain {
            self.exit_scope();
        }

        if let Some(ref else_block) = if_stmt.else_block {
            // Process else with the pre-branch state
            self.possibly_uninit = uninit_before;
            self.bind_block(else_block)?;
            let uninit_after_else = self.possibly_uninit.clone();
            // After if-else: a var is possibly-uninit if uninit in either branch (union)
            self.possibly_uninit = uninit_after_then;
            for entry in uninit_after_else {
                self.possibly_uninit.insert(entry);
            }
        } else {
            // No else branch: restore to before-branch state (branch might not run)
            self.possibly_uninit = uninit_before;
        }

        Ok(())
    }

    /// Bind a while statement
    fn bind_while(&mut self, while_stmt: &WhileStmt) -> Result<(), Bail> {
        let is_let_chain = matches!(while_stmt.condition, Condition::LetChain { .. });
        if is_let_chain {
            self.enter_scope();
        }

        self.bind_condition(&while_stmt.condition)?;

        // Loop body does not guarantee initialization (may execute zero times).
        let uninit_before = self.possibly_uninit.clone();
        self.bind_block(&while_stmt.body)?;
        self.possibly_uninit = uninit_before;

        if is_let_chain {
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

        // Loop body does not guarantee initialization (may execute zero times).
        let uninit_before = self.possibly_uninit.clone();
        self.bind_block(&for_stmt.body)?;
        self.possibly_uninit = uninit_before;

        self.exit_scope();
        Ok(())
    }

    /// Bind a for-of statement: `for let item of array { ... }`
    fn bind_for_of(&mut self, for_of_stmt: &ForOfStmt) -> Result<(), Bail> {
        // First bind the iterable expression (uses variables from outer scope)
        self.bind_expr(&for_of_stmt.iterable)?;

        // Enter a new scope for the loop binding and body
        self.enter_scope();

        // Define the loop variable(s)
        self.bind_let_pattern(
            &for_of_stmt.binding,
            for_of_stmt.is_mut,
            false, // not reactive
            for_of_stmt.span,
        )?;

        // Loop body does not guarantee initialization (may execute zero times).
        let uninit_before = self.possibly_uninit.clone();
        self.bind_block(&for_of_stmt.body)?;
        self.possibly_uninit = uninit_before;

        self.exit_scope();
        Ok(())
    }

    /// Bind a loop statement
    fn bind_loop(&mut self, loop_stmt: &LoopStmt) -> Result<(), Bail> {
        // Loop body does not guarantee initialization (may execute zero times
        // from the perspective of the enclosing code).
        let uninit_before = self.possibly_uninit.clone();
        self.bind_block(&loop_stmt.body)?;
        self.possibly_uninit = uninit_before;
        Ok(())
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
                } else if self.is_possibly_uninit(&ident.name) {
                    self.logger.error(BindError::UseBeforeInit {
                        name: ident.name.clone(),
                        span: ident.span,
                    })?;
                }
            }

            Expr::Assign(assign) => {
                // If the target is an uninitialized variable, this is its first
                // initialization — allow it (skipping the immutability check) and
                // mark the variable as definitely initialized.
                if let Expr::Ident(ident) = &assign.target
                    && self.is_possibly_uninit(&ident.name)
                {
                    self.mark_initialized(&ident.name);
                    self.bind_expr(&assign.value)?;
                    return Ok(());
                }

                // Normal assignment: check mutability for simple variable assignments
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
                // so we can detect "use after scope exit" errors.
                //
                // Bare idents at the top level of the pattern (or or-alternatives)
                // might be global constants rather than new locals. We must remove
                // those from local_names_in_function after the scope exits, otherwise
                // they shadow the real globals when used as expressions later.
                let bare_idents = collect_top_level_bare_idents(&matches_expr.pattern);
                self.enter_scope();
                self.bind_pattern(&matches_expr.pattern, matches_expr.span)?;
                if let Some(guard) = &matches_expr.guard {
                    self.bind_expr(guard)?;
                }
                self.exit_scope();
                for name in &bare_idents {
                    self.local_names_in_function.shift_remove(name);
                }
            }

            Expr::TryOp(qm) => {
                self.bind_expr(&qm.expr)?;
            }

            Expr::Spread(inner, _) => {
                self.bind_expr(inner)?;
            }

            Expr::Range(range) => {
                self.bind_expr(&range.start)?;
                self.bind_expr(&range.end)?;
            }

            Expr::WithHandler(with_handler) => {
                for binding in &with_handler.handlers {
                    self.bind_expr(&binding.handler)?;
                }
                self.bind_block(&with_handler.body)?;
            }

            Expr::Resume(resume_expr) => {
                self.bind_expr(&resume_expr.value)?;
            }

            // Literals don't reference variables
            Expr::Literal(_) => {}

            // Parser error-recovery placeholder: nothing to bind.
            Expr::Error(_) => {}
        }
        Ok(())
    }

    /// Bind an if expression
    fn bind_if_expr(&mut self, if_expr: &IfExpr) -> Result<(), Bail> {
        let is_let_chain = matches!(if_expr.condition, Condition::LetChain { .. });
        if is_let_chain {
            self.enter_scope();
        }

        self.bind_condition(&if_expr.condition)?;

        // Snapshot possibly_uninit before diverging branches.
        let uninit_before = self.possibly_uninit.clone();

        self.bind_block(&if_expr.then_block)?;
        let uninit_after_then = self.possibly_uninit.clone();

        if is_let_chain {
            self.exit_scope();
        }

        if let Some(ref else_block) = if_expr.else_block {
            self.possibly_uninit = uninit_before;
            self.bind_block(else_block)?;
            let uninit_after_else = self.possibly_uninit.clone();
            self.possibly_uninit = uninit_after_then;
            for entry in uninit_after_else {
                self.possibly_uninit.insert(entry);
            }
        } else {
            self.possibly_uninit = uninit_before;
        }

        Ok(())
    }

    /// Bind an if condition (expression or let chain)
    fn bind_condition(&mut self, condition: &Condition) -> Result<(), Bail> {
        match condition {
            Condition::Expr(expr) => {
                self.bind_expr(expr)?;
            }
            Condition::LetChain { elements, .. } => {
                // Process each element in order. Let elements introduce bindings
                // visible in subsequent elements (caller must have entered a scope).
                for elem in elements {
                    match elem {
                        ConditionElement::Let { pattern, expr, .. } => {
                            self.bind_expr(expr)?;
                            self.bind_pattern(pattern, expr.span())?;
                        }
                        ConditionElement::Expr(expr) => {
                            self.bind_expr(expr)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Bind a match expression
    fn bind_match_expr(&mut self, match_expr: &MatchExpr) -> Result<(), Bail> {
        self.bind_expr(&match_expr.expr)?;

        let uninit_before = self.possibly_uninit.clone();
        let mut uninit_after_all_arms: Option<IndexSet<(u32, String)>> = None;

        for arm in &match_expr.arms {
            // Process each arm from the pre-match state
            self.possibly_uninit.clone_from(&uninit_before);

            let bare_idents = collect_top_level_bare_idents(&arm.pattern);
            self.enter_scope();
            self.bind_pattern(&arm.pattern, arm.span)?;
            if let Some(guard) = &arm.guard {
                self.bind_expr(guard)?;
            }
            self.bind_expr(&arm.body)?;
            self.exit_scope();
            for name in &bare_idents {
                self.local_names_in_function.shift_remove(name);
            }

            // Merge: a var is possibly-uninit after the match if possibly-uninit in any arm
            match uninit_after_all_arms.take() {
                None => uninit_after_all_arms = Some(self.possibly_uninit.clone()),
                Some(prev) => {
                    let mut merged = prev;
                    for entry in &self.possibly_uninit {
                        merged.insert(entry.clone());
                    }
                    uninit_after_all_arms = Some(merged);
                }
            }
        }

        self.possibly_uninit = uninit_after_all_arms.unwrap_or(uninit_before);
        Ok(())
    }

    /// Bind a pattern (may introduce variables).
    /// Bare identifiers are tentatively defined: if the name already exists in the
    /// current scope, it is silently skipped. This is necessary because the parser
    /// no longer uses case to distinguish variant case names from variable bindings,
    /// so duplicate bare names like `[Null, Null]` in a match pattern are valid
    /// (the elaborator disambiguates them using type information).
    fn bind_pattern(&mut self, pattern: &crate::ast::Pattern, span: Span) -> Result<(), Bail> {
        match pattern {
            crate::ast::Pattern::Ident { name, .. } => {
                let scope = self.scopes.last().unwrap();
                if !scope.bindings.contains_key(name) {
                    self.define(name, false, false, span)?;
                }
            }
            crate::ast::Pattern::MutIdent { name, .. } => {
                self.define(name, true, false, span)?;
            }
            crate::ast::Pattern::Tuple(patterns, _) => {
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
            crate::ast::Pattern::Struct { fields, .. } => {
                for field in fields {
                    self.bind_pattern(&field.pattern, span)?;
                }
            }
            crate::ast::Pattern::Literal(_)
            | crate::ast::Pattern::Wildcard
            | crate::ast::Pattern::Range { .. } => {
                // No variables introduced
            }
            crate::ast::Pattern::Or(alternatives) => {
                // Bind variables from the first alternative (all alternatives must bind the same names)
                if let Some(first) = alternatives.first() {
                    self.bind_pattern(first, span)?;
                }
            }
        }
        Ok(())
    }

    /// Bind a closure
    fn bind_closure(&mut self, closure: &ClosureExpr) -> Result<(), Bail> {
        self.enter_scope();

        // Bind parameters
        for param in &closure.params {
            self.define(&param.name, param.is_mut, false, closure.span)?;
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

    /// Exit the current scope, removing its variables from `possibly_uninit`.
    fn exit_scope(&mut self) {
        if let Some(scope) = self.scopes.pop() {
            for (name, binding) in &scope.bindings {
                self.possibly_uninit
                    .shift_remove(&(binding.scope_depth, name.clone()));
            }
        }
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

    /// Like `define`, but also marks the variable as possibly uninitialized.
    /// Used for `let x: T;` declarations without an initializer.
    fn define_uninit(
        &mut self,
        name: &str,
        is_mut: bool,
        is_reactive: bool,
        span: Span,
    ) -> Result<(), Bail> {
        self.define(name, is_mut, is_reactive, span)?;
        self.possibly_uninit
            .insert((self.current_depth, name.to_string()));
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
    use crate::lexer::lex;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let r = lex(source);
        assert!(r.errors.is_empty(), "lex error: {:?}", r.errors);
        let mut parser = Parser::new(r.tokens);
        parser.parse_strict().expect("parse error")
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
            r"
            fn run() {
                let x = 1;
                let y = x;
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_out_of_scope() {
        let module = parse(
            r"
            fn run() {
                if true {
                    let x = 1;
                }
                let y = x;
            }
        ",
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
            r"
            fn run() {
                let x = 1;
                let x = 2;
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(!ok);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("cannot redeclare"));
    }

    #[test]
    fn test_same_scope_shadow_with_self_ref() {
        let module = parse(
            r"
            fn run() {
                let x = 1;
                let x = x + 1;
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok, "shadowing with self-ref should be allowed: {diags:?}");
        assert!(diags.is_empty());
    }

    #[test]
    fn test_same_scope_shadow_with_self_ref_in_call() {
        let module = parse(
            r"
            fn transform(n: i32) -> i32 { return n; }
            fn run() {
                let x = 1;
                let x = transform(x);
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(
            ok,
            "shadowing with self-ref in call should be allowed: {diags:?}"
        );
        assert!(diags.is_empty());
    }

    #[test]
    fn test_same_scope_shadow_closure_param_not_self_ref() {
        // |x| x + 1 — the x inside refers to the closure param, not the outer variable
        let module = parse(
            r"
            fn run() {
                let x = 1;
                let x = |x: i32| x + 1;
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(!ok, "closure param shadowing should NOT count as self-ref");
        assert!(diags[0].message.contains("cannot redeclare"));
    }

    #[test]
    fn test_same_scope_shadow_closure_capture_is_self_ref() {
        // || x + 1 — captures the outer x, this IS a self-reference
        let module = parse(
            r"
            fn run() {
                let x = 1;
                let x = || x + 1;
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok, "closure capture should count as self-ref: {diags:?}");
        assert!(diags.is_empty());
    }

    #[test]
    fn test_shadowing_in_nested_scope() {
        let module = parse(
            r"
            fn run() {
                let x = 1;
                if true {
                    let x = 2;
                }
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_assign_to_immutable() {
        let module = parse(
            r"
            fn run() {
                let x = 1;
                x = 2;
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(!ok);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("cannot assign to immutable"));
    }

    #[test]
    fn test_assign_to_mutable() {
        let module = parse(
            r"
            fn run() {
                let mut x = 1;
                x = 2;
            }
        ",
        );
        let (ok, diags) = bind_and_check(&module);
        assert!(ok);
        assert!(diags.is_empty());
    }
}
