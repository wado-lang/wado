//! Local name binding within bodies: duplicate and keyword-spelled
//! bindings, reads before initialization, and assignments to immutable locals.

use crate::hashmap::IndexSet;

use crate::hashmap::IndexMap;

use crate::ast::{
    AssertStmt, Block, ClosureExpr, Condition, ConditionElement, Expr, ExprStmt, ForOfStmt,
    ForStmt, Function, IfExpr, IfStmt, Item, LetStmt, LoopStmt, MatchArm, MatchExpr, Module,
    Pattern, ReturnStmt, Stmt, WhileStmt, for_each_pattern_name,
};
use crate::compiler_host::{CompilerHost, Diagnostic};
use crate::logger::{Bail, Logger};
use crate::module_source::ModuleSource;
use crate::syntax::{expression_keyword_name_message, is_expression_keyword};
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
    /// Assignment to an immutable variable
    AssignToImmutable { name: String, span: Span },

    /// Variable used before it was definitely initialized
    UseBeforeInit { name: String, span: Span },

    /// A binding spelled like a keyword that begins an expression
    KeywordName { name: String, span: Span },
}

/// How the names a pattern binds enter the current scope.
#[derive(Clone, Copy)]
enum BindingKind {
    /// A `let` with an initializer, a `for-of` binding, or a pattern that may
    /// not match: `if let`, `while let`, a `match` arm, `matches`.
    Initialized { is_mut: bool, is_reactive: bool },
    /// A `let x: T;`, which every read before an assignment reports.
    Uninitialized { is_mut: bool, is_reactive: bool },
}

impl From<BindError> for Diagnostic {
    fn from(e: BindError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let (code, message, span) = match &e {
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
            BindError::KeywordName { name, span } => (
                Code::InvalidSyntax,
                expression_keyword_name_message(name),
                *span,
            ),
        };
        Diagnostic {
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
pub(crate) fn expr_references_var(expr: &Expr, name: &str) -> bool {
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

        Expr::If(if_expr) => if_references_var(
            &if_expr.condition,
            &if_expr.then_block,
            if_expr.else_block.as_ref(),
            name,
        ),
        Expr::Match(m) => {
            expr_references_var(&m.expr, name) || match_arms_reference_var(&m.arms, name)
        }
        Expr::Matches(m) => {
            expr_references_var(&m.expr, name)
                || (!pattern_binds_name(&m.pattern, name)
                    && m.guard
                        .as_ref()
                        .is_some_and(|g| expr_references_var(g, name)))
        }

        Expr::Block(block) => block_references_var(block, name),
        Expr::LabeledBlock(lb) => block_references_var(&lb.block, name),

        Expr::TemplateString(ts) => ts
            .interpolations()
            .any(|expr| expr_references_var(expr, name)),
        Expr::TaggedTemplate(t) => {
            expr_references_var(&t.tag, name)
                || t.template
                    .interpolations()
                    .any(|expr| expr_references_var(expr, name))
        }
        Expr::TupleLiteral(t) => t.elements.iter().any(|e| expr_references_var(e, name)),
        Expr::TupleComprehension(c) => {
            expr_references_var(&c.iterable, name)
                || (!pattern_binds_name(&c.binding, name) && expr_references_var(&c.body, name))
        }
        Expr::StructLiteral(s) => {
            s.fields.iter().any(|f| expr_references_var(&f.value, name))
                || s.spreads
                    .iter()
                    .any(|sp| expr_references_var(&sp.expr, name))
        }
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
                || block_references_var(&w.body, name)
        }
        Expr::Resume(r) => expr_references_var(&r.value, name),

        Expr::Literal(_) | Expr::Error(_) => false,
    }
}

fn stmt_references_var(stmt: &Stmt, name: &str) -> bool {
    match stmt {
        Stmt::Let(let_stmt) => let_stmt
            .value
            .as_ref()
            .is_some_and(|v| expr_references_var(v, name)),
        Stmt::Expr(expr_stmt) => expr_references_var(&expr_stmt.expr, name),
        Stmt::Return(ret) => ret
            .value
            .as_ref()
            .is_some_and(|v| expr_references_var(v, name)),
        Stmt::TaskReturn(tr) => expr_references_var(&tr.value, name),
        Stmt::Assert(a) => {
            expr_references_var(&a.condition, name)
                || a.message
                    .as_ref()
                    .is_some_and(|m| expr_references_var(m, name))
        }
        Stmt::If(if_stmt) => if_references_var(
            &if_stmt.condition,
            &if_stmt.then_block,
            if_stmt.else_block.as_ref(),
            name,
        ),
        Stmt::While(w) => {
            condition_references_var(&w.condition, name)
                || (!condition_binds_name(&w.condition, name)
                    && block_references_var(&w.body, name))
        }
        Stmt::For(f) => for_references_var(f, name),
        Stmt::ForOf(fo) => {
            expr_references_var(&fo.iterable, name)
                || (!pattern_binds_name(&fo.binding, name) && block_references_var(&fo.body, name))
        }
        Stmt::Loop(l) => block_references_var(&l.body, name),
        Stmt::Match(m) => {
            expr_references_var(&m.expr, name) || match_arms_reference_var(&m.arms, name)
        }
        Stmt::LabeledBlock(lb) => block_references_var(&lb.block, name),
        // A local type/impl declaration's methods aren't closures — they
        // can't capture `name` from the enclosing function — so it never
        // references an outer variable.
        Stmt::Item(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Error(_) => false,
    }
}

fn block_references_var(block: &Block, name: &str) -> bool {
    block.stmts.iter().any(|s| stmt_references_var(s, name))
}

/// A condition's bindings reach the `then` block and stop there, so the `else`
/// is read outside them.
fn if_references_var(
    condition: &Condition,
    then_block: &Block,
    else_block: Option<&Block>,
    name: &str,
) -> bool {
    condition_references_var(condition, name)
        || (!condition_binds_name(condition, name) && block_references_var(then_block, name))
        || else_block.is_some_and(|b| block_references_var(b, name))
}

/// An arm's pattern scopes its guard and body, so an arm taking the name reads
/// its own binding rather than the one outside.
fn match_arms_reference_var(arms: &[MatchArm], name: &str) -> bool {
    arms.iter().any(|arm| {
        !pattern_binds_name(&arm.pattern, name)
            && (arm
                .guard
                .as_ref()
                .is_some_and(|g| expr_references_var(g, name))
                || expr_references_var(&arm.body, name))
    })
}

/// The initializer binds for the condition, the update and the body, so the
/// walk stops there when it takes the name.
fn for_references_var(f: &ForStmt, name: &str) -> bool {
    let init = f.init.as_deref();
    if init.is_some_and(|i| stmt_references_var(i, name)) {
        return true;
    }
    if init.is_some_and(|i| stmt_binds_name(i, name)) {
        return false;
    }
    f.condition
        .as_ref()
        .is_some_and(|c| condition_references_var(c, name))
        || f.update
            .as_ref()
            .is_some_and(|u| expr_references_var(u, name))
        || block_references_var(&f.body, name)
}

/// Whether a statement binds the name for what follows it, as a C-style `for`'s
/// initializer does for the rest of the loop.
fn stmt_binds_name(stmt: &Stmt, name: &str) -> bool {
    matches!(stmt, Stmt::Let(l) if pattern_binds_name(&l.pattern, name))
}

/// Whether a name a construct binds is the one being looked for, so the body it
/// scopes reads that binder rather than the binding outside.
fn pattern_binds_name(pattern: &Pattern, name: &str) -> bool {
    let mut binds = false;
    for_each_pattern_name(pattern, &mut |bound, _| binds |= bound == name);
    binds
}

/// A let-chain element binds for the elements after it, so the walk stops at the
/// first one taking the name.
fn condition_references_var(condition: &Condition, name: &str) -> bool {
    match condition {
        Condition::Expr(expr) => expr_references_var(expr, name),
        Condition::LetChain { elements, .. } => {
            for elem in elements {
                match elem {
                    ConditionElement::Let { pattern, expr, .. } => {
                        if expr_references_var(expr, name) {
                            return true;
                        }
                        if pattern_binds_name(pattern, name) {
                            return false;
                        }
                    }
                    ConditionElement::Expr(expr) => {
                        if expr_references_var(expr, name) {
                            return true;
                        }
                    }
                }
            }
            false
        }
    }
}

/// Whether a condition's bindings reach past it, into the block it guards.
fn condition_binds_name(condition: &Condition, name: &str) -> bool {
    let Condition::LetChain { elements, .. } = condition else {
        return false;
    };
    elements.iter().any(|elem| match elem {
        ConditionElement::Let { pattern, .. } => pattern_binds_name(pattern, name),
        ConditionElement::Expr(_) => false,
    })
}

/// The binder performs local name resolution
pub struct Binder<'a, H: CompilerHost> {
    scopes: Vec<Scope>,
    logger: &'a Logger<'a, H>,
    /// Module whose bodies are being bound; every diagnostic is attributed to
    /// its source file.
    module_source: &'a ModuleSource,
    current_depth: u32,
    /// Variables declared without an initializer that have not yet been
    /// definitely assigned on all paths reaching the current point.
    /// Key: (`scope_depth`, name) — `scope_depth` disambiguates shadowed vars.
    possibly_uninit: IndexSet<(u32, String)>,
}

impl<'a, H: CompilerHost> Binder<'a, H> {
    /// Create a new binder
    pub fn new(logger: &'a Logger<'a, H>, module_source: &'a ModuleSource) -> Self {
        Self {
            scopes: vec![Scope::new()], // Global scope
            logger,
            module_source,
            current_depth: 0,
            possibly_uninit: IndexSet::default(),
        }
    }

    /// Emit a bind error attributed to the module being bound.
    fn emit(&self, err: impl Into<Diagnostic>) -> Result<(), Bail> {
        self.logger.error_in(self.module_source, err)
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

    /// Bind every body an item carries: a function's, a method's (a trait's or
    /// an interface operation's default included), a test's, and a global's
    /// initializer.
    fn bind_item(&mut self, item: &Item) -> Result<(), Bail> {
        match item {
            Item::Function(func) => self.bind_function(func),
            Item::Impl(impl_block) => self.bind_functions(&impl_block.methods),
            Item::Trait(trait_decl) => self.bind_functions(&trait_decl.methods),
            Item::Interface(interface_decl) => self.bind_functions(&interface_decl.methods),
            Item::Resource(resource_decl) => self.bind_functions(&resource_decl.methods),
            Item::Test(test) => self.in_body(|s| s.bind_block_contents(&test.body)),
            Item::Global(global) => self.in_body(|s| s.bind_expr(&global.initializer)),
            Item::Struct(_)
            | Item::Enum(_)
            | Item::Variant(_)
            | Item::Flags(_)
            | Item::Newtype(_)
            | Item::TupleTypeDecl(_)
            | Item::BuiltinTypeDecl(_)
            | Item::World(_)
            | Item::Use(_)
            | Item::Error(_) => Ok(()),
        }
    }

    fn bind_functions(&mut self, functions: &[Function]) -> Result<(), Bail> {
        for func in functions {
            self.bind_function(func)?;
        }
        Ok(())
    }

    /// Bind a function's parameters and body; a signature has no body to bind.
    fn bind_function(&mut self, func: &Function) -> Result<(), Bail> {
        self.in_body(|s| {
            for param in &func.params {
                s.define(&param.name, param.is_mut, false, param.span)?;
            }
            match &func.body {
                Some(body) => s.bind_block_contents(body),
                None => Ok(()),
            }
        })
    }

    /// Bind one body in a scope of its own.
    fn in_body(&mut self, bind: impl FnOnce(&mut Self) -> Result<(), Bail>) -> Result<(), Bail> {
        self.possibly_uninit.clear();
        self.enter_scope();
        bind(self)?;
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
            // Local type/impl declaration: only its methods (impl/trait) have
            // local scopes to bind, same as a top-level item.
            Stmt::Item(item) => self.bind_item(item)?,
            Stmt::Error(_) => {} // Parser error-recovery placeholder: nothing to bind
        }
        Ok(())
    }

    /// Bind a let statement
    fn bind_let(&mut self, let_stmt: &LetStmt) -> Result<(), Bail> {
        if let Some(ref value) = let_stmt.value {
            // Initialized let: bind the initializer first (uses outer scope vars)
            self.bind_expr(value)?;

            // Bind the else block before the pattern bindings, so it cannot
            // see them — they escape to the enclosing scope, not the else block.
            if let Some(else_block) = &let_stmt.else_block {
                self.bind_block(else_block)?;
            }

            self.bind_pattern_as(
                &let_stmt.pattern,
                BindingKind::Initialized {
                    is_mut: let_stmt.is_mut,
                    is_reactive: let_stmt.is_reactive,
                },
                let_stmt.span,
            )
        } else {
            // `let x: T;`, which the parser guarantees is annotated.
            self.bind_pattern_as(
                &let_stmt.pattern,
                BindingKind::Uninitialized {
                    is_mut: let_stmt.is_mut,
                    is_reactive: let_stmt.is_reactive,
                },
                let_stmt.span,
            )
        }
    }

    /// Enter each name `pattern` binds, as `kind` defines them. One walk for
    /// every kind: a walk apiece once dropped `Variant` from two of them.
    fn bind_pattern_as(
        &mut self,
        pattern: &Pattern,
        kind: BindingKind,
        span: Span,
    ) -> Result<(), Bail> {
        match pattern {
            Pattern::Ident { name, .. } => self.define_as(name, kind, false, span)?,
            // `let Some(mut n) = …` and `if let Some(mut n) = …` carry the
            // `mut` on the binding; a `let mut` carries it on every binding.
            Pattern::MutIdent { name, .. } => self.define_as(name, kind, true, span)?,
            Pattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.bind_pattern_as(p, kind, span)?;
                }
            }
            Pattern::Variant {
                bindings,
                span: variant_span,
                ..
            } => {
                for p in bindings {
                    self.bind_pattern_as(p, kind, *variant_span)?;
                }
            }
            Pattern::Struct { fields, .. } => {
                for field in fields {
                    self.bind_pattern_as(&field.pattern, kind, span)?;
                }
            }
            // Every alternative binds the same names, so the first speaks for
            // all of them.
            Pattern::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    self.bind_pattern_as(first, kind, span)?;
                }
            }
            Pattern::Typed { pattern, .. } => self.bind_pattern_as(pattern, kind, span)?,
            Pattern::Literal(_) | Pattern::Wildcard | Pattern::Range { .. } | Pattern::Error(_) => {
            }
        }
        Ok(())
    }

    /// Enter one name from a pattern. `pattern_mut` is a `mut` written on the
    /// binding itself, which adds to whatever the statement declared.
    fn define_as(
        &mut self,
        name: &str,
        kind: BindingKind,
        pattern_mut: bool,
        span: Span,
    ) -> Result<(), Bail> {
        match kind {
            BindingKind::Initialized {
                is_mut,
                is_reactive,
            } => self.define(name, is_mut || pattern_mut, is_reactive, span),
            BindingKind::Uninitialized {
                is_mut,
                is_reactive,
            } => self.define_uninit(name, is_mut || pattern_mut, is_reactive, span),
        }
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

        self.bind_pattern_as(
            &for_of_stmt.binding,
            BindingKind::Initialized {
                is_mut: for_of_stmt.is_mut,
                is_reactive: false,
            },
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
                if self.is_possibly_uninit(&ident.name) {
                    self.emit(BindError::UseBeforeInit {
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
                    self.emit(BindError::AssignToImmutable {
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
                    self.emit(BindError::AssignToImmutable {
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
                for expr in template.interpolations() {
                    self.bind_expr(expr)?;
                }
            }

            Expr::TaggedTemplate(tagged) => {
                self.bind_expr(&tagged.tag)?;
                for expr in tagged.template.interpolations() {
                    self.bind_expr(expr)?;
                }
            }

            Expr::Cast(cast) => {
                self.bind_expr(&cast.expr)?;
            }

            Expr::StructLiteral(struct_lit) => {
                for field in &struct_lit.fields {
                    self.bind_expr(&field.value)?;
                }
                for spread in &struct_lit.spreads {
                    self.bind_expr(&spread.expr)?;
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

            Expr::TupleComprehension(c) => {
                self.bind_expr(&c.iterable)?;
                self.enter_scope();
                self.bind_pattern(&c.binding, c.span)?;
                self.bind_expr(&c.body)?;
                self.exit_scope();
            }

            Expr::LabeledBlock(lb) => {
                // Labeled block expression creates a new scope for its block
                self.enter_scope();
                self.bind_block(&lb.block)?;
                self.exit_scope();
            }

            Expr::Matches(matches_expr) => {
                self.bind_expr(&matches_expr.expr)?;
                self.enter_scope();
                self.bind_pattern(&matches_expr.pattern, matches_expr.span)?;
                if let Some(guard) = &matches_expr.guard {
                    self.bind_expr(guard)?;
                }
                self.exit_scope();
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

            self.enter_scope();
            self.bind_pattern(&arm.pattern, arm.span)?;
            if let Some(guard) = &arm.guard {
                self.bind_expr(guard)?;
            }
            self.bind_expr(&arm.body)?;
            self.exit_scope();

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

    /// Bind a refutable pattern's names.
    fn bind_pattern(&mut self, pattern: &Pattern, span: Span) -> Result<(), Bail> {
        self.bind_pattern_as(
            pattern,
            BindingKind::Initialized {
                is_mut: false,
                is_reactive: false,
            },
            span,
        )
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
        if is_expression_keyword(name) {
            return self.emit(BindError::KeywordName {
                name: name.to_string(),
                span,
            });
        }
        // A name the scope already holds is replaced; the resolver reports the
        // redeclarations, since only it knows which bare names bind.
        self.possibly_uninit
            .shift_remove(&(self.current_depth, name.to_string()));
        let scope = self.scopes.last_mut().unwrap();
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
pub fn bind_module<H: CompilerHost>(
    module: &Module,
    module_source: &ModuleSource,
    logger: &Logger<H>,
) -> Result<(), Bail> {
    let mut binder = Binder::new(logger, module_source);
    binder.bind_module(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::{Diagnostic, InMemoryCompilerHost, LogLevel};
    use crate::lexer::lex;
    use crate::module_source::ModuleSource;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let r = lex(source);
        assert!(r.errors.is_empty(), "lex error: {:?}", r.errors);
        let mut parser = Parser::new(r.tokens);
        parser.parse_strict().expect("parse error")
    }

    fn bind_and_check(module: &Module) -> (bool, Vec<Diagnostic>) {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Error);
        let result = bind_module(module, &ModuleSource::default(), &logger);
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
