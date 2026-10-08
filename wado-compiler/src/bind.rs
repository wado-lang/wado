//! Local name binding within bodies: keyword-spelled bindings, reads before
//! initialization, and assignments to immutable locals.

use crate::hashmap::IndexSet;

use crate::hashmap::IndexMap;

use crate::ast::{
    AssertStmt, Block, ClosureExpr, Condition, ConditionElement, Expr, ExprStmt, ForOfStmt,
    ForStmt, Function, Item, LetStmt, LoopStmt, MatchArm, MatchExpr, Module, Pattern, ReturnStmt,
    Stmt, WhileStmt, for_each_pattern_name,
};
use crate::compiler_host::{CompilerHost, Diagnostic};
use crate::logger::{Bail, Logger};
use crate::module_source::ModuleSource;
use crate::syntax::{is_statement_keyword, statement_keyword_name_message};
use crate::token::Span;

/// Binding information for a local variable
#[derive(Debug, Clone)]
struct BindingInfo {
    is_mut: bool,
    /// Scope depth where the variable was defined
    scope_depth: u32,
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

    /// A second assignment to an immutable `let x: T;`
    AssignTwice { name: String, span: Span },

    /// Variable used before it was definitely initialized
    UseBeforeInit { name: String, span: Span },

    /// A closure naming a `let x: T;` that is not yet definitely initialized
    CaptureBeforeInit { name: String, span: Span },

    /// A binding spelled like a keyword that begins an expression
    KeywordName { name: String, span: Span },
}

/// A local, as the binder tells shadowed names apart.
type LocalKey = (u32, String);

/// What the binder knows, at one point of a body, about the locals declared
/// without an initializer.
#[derive(Clone, Default)]
struct Flow {
    /// False past a `return`, `break` or `continue`, where nothing runs.
    reachable: bool,
    /// Those that some path reaching here leaves unassigned.
    unassigned: IndexSet<LocalKey>,
    /// Those that some path reaching here has assigned.
    assigned: IndexSet<LocalKey>,
}

impl Flow {
    fn reachable() -> Self {
        Self {
            reachable: true,
            ..Self::default()
        }
    }

    /// The point two paths meet at.
    fn join(self, other: Flow) -> Flow {
        if !self.reachable {
            return other;
        }
        if !other.reachable {
            return self;
        }
        let mut joined = self;
        joined.unassigned.extend(other.unassigned);
        joined.assigned.extend(other.assigned);
        joined
    }

    fn forget(&mut self, key: &LocalKey) {
        self.unassigned.shift_remove(key);
        self.assigned.shift_remove(key);
    }
}

/// Where a `break` or `continue` goes, and the paths that went there.
enum JumpTarget {
    Loop {
        exits: Flow,
        repeats: Flow,
    },
    Label {
        label: String,
        exits: Flow,
    },
    /// A closure body, which no jump leaves.
    Closure {
        scope_depth: u32,
    },
}

/// How the names a pattern binds enter the current scope.
#[derive(Clone, Copy)]
enum BindingKind {
    /// A `let` with an initializer, a `for-of` binding, or a pattern that may
    /// not match: `if let`, `while let`, a `match` arm, `matches`.
    Initialized { is_mut: bool },
    /// A `let x: T;`, which every read before an assignment reports.
    Uninitialized { is_mut: bool },
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
            BindError::AssignTwice { name, span } => (
                Code::ImmutableAssignment,
                format!("cannot assign twice to immutable variable '{name}'"),
                *span,
            ),
            BindError::UseBeforeInit { name, span } => (
                Code::UninitializedVariable,
                format!("'{name}' is used before initialization"),
                *span,
            ),
            BindError::CaptureBeforeInit { name, span } => (
                Code::UninitializedVariable,
                format!("'{name}' is captured by a closure before initialization"),
                *span,
            ),
            BindError::KeywordName { name, span } => (
                Code::InvalidSyntax,
                statement_keyword_name_message(name),
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
    /// The locals in scope declared without an initializer.
    deferred: IndexSet<LocalKey>,
    flow: Flow,
    jump_targets: Vec<JumpTarget>,
    /// Above zero while a loop body is bound only to learn what reaches its
    /// next iteration; the pass that follows reports.
    quiet: u32,
}

impl<'a, H: CompilerHost> Binder<'a, H> {
    /// Create a new binder
    pub fn new(logger: &'a Logger<'a, H>, module_source: &'a ModuleSource) -> Self {
        Self {
            scopes: vec![Scope::new()], // Global scope
            logger,
            module_source,
            current_depth: 0,
            deferred: IndexSet::default(),
            flow: Flow::reachable(),
            jump_targets: Vec::new(),
            quiet: 0,
        }
    }

    /// Emit a bind error attributed to the module being bound.
    fn emit(&self, err: impl Into<Diagnostic>) -> Result<(), Bail> {
        if self.quiet > 0 {
            return Ok(());
        }
        self.logger.error_in(self.module_source, err)
    }

    /// The innermost binding of `name`, when it is declared without an
    /// initializer.
    fn deferred_key(&self, name: &str) -> Option<LocalKey> {
        let binding = self.lookup(name)?;
        let key = (binding.scope_depth, name.to_string());
        self.deferred.contains(&key).then_some(key)
    }

    /// Whether `key` is declared outside the closure being bound.
    fn captured(&self, key: &LocalKey) -> bool {
        self.jump_targets.iter().rev().any(
            |target| matches!(target, JumpTarget::Closure { scope_depth } if key.0 < *scope_depth),
        )
    }

    /// Read `name`, which a path leaving it unassigned makes an error.
    fn read(&mut self, name: &str, span: Span) -> Result<(), Bail> {
        let Some(key) = self.deferred_key(name) else {
            return Ok(());
        };
        if !self.flow.unassigned.contains(&key) {
            return Ok(());
        }
        let name = name.to_string();
        if self.captured(&key) {
            self.emit(BindError::CaptureBeforeInit { name, span })
        } else {
            self.emit(BindError::UseBeforeInit { name, span })
        }
    }

    /// Assign `name` with `=`. A local declared without an initializer is
    /// assigned once if immutable, and a closure never assigns it first.
    fn assign(&mut self, name: &str, span: Span) -> Result<(), Bail> {
        let Some(binding) = self.lookup(name) else {
            return Ok(());
        };
        let is_mut = binding.is_mut;
        let Some(key) = self.deferred_key(name) else {
            if !is_mut {
                self.emit(BindError::AssignToImmutable {
                    name: name.to_string(),
                    span,
                })?;
            }
            return Ok(());
        };
        if self.captured(&key) && self.flow.unassigned.contains(&key) {
            return self.emit(BindError::CaptureBeforeInit {
                name: name.to_string(),
                span,
            });
        }
        if !is_mut && self.flow.assigned.contains(&key) {
            self.emit(BindError::AssignTwice {
                name: name.to_string(),
                span,
            })?;
        }
        self.flow.unassigned.shift_remove(&key);
        self.flow.assigned.insert(key);
        Ok(())
    }

    /// Leave the current path for the innermost loop, or the labeled block
    /// named `label`, recording where it went.
    fn jump(&mut self, label: Option<&str>, repeat: bool) {
        let flow = std::mem::take(&mut self.flow);
        for target in self.jump_targets.iter_mut().rev() {
            match (target, label) {
                (JumpTarget::Closure { .. }, _) => return,
                (JumpTarget::Loop { exits, repeats }, None) => {
                    let into = if repeat { repeats } else { exits };
                    *into = std::mem::take(into).join(flow);
                    return;
                }
                (JumpTarget::Label { label: name, exits }, Some(label)) if name == label => {
                    *exits = std::mem::take(exits).join(flow);
                    return;
                }
                (JumpTarget::Loop { .. } | JumpTarget::Label { .. }, _) => {}
            }
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
                s.define(&param.name, param.is_mut, param.span)?;
            }
            match &func.body {
                Some(body) => s.bind_block_contents(body),
                None => Ok(()),
            }
        })
    }

    /// Bind one body in a scope of its own.
    /// A local item's body sees none of the enclosing body's flow.
    fn in_body(&mut self, bind: impl FnOnce(&mut Self) -> Result<(), Bail>) -> Result<(), Bail> {
        let flow = std::mem::replace(&mut self.flow, Flow::reachable());
        let jump_targets = std::mem::take(&mut self.jump_targets);
        self.enter_scope();
        bind(self)?;
        self.exit_scope();
        self.flow = flow;
        self.jump_targets = jump_targets;
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
            Stmt::If(if_stmt) => self.bind_if(
                &if_stmt.condition,
                &if_stmt.then_block,
                if_stmt.else_block.as_ref(),
            )?,
            Stmt::While(while_stmt) => self.bind_while(while_stmt)?,
            Stmt::For(for_stmt) => self.bind_for(for_stmt)?,
            Stmt::ForOf(for_of_stmt) => self.bind_for_of(for_of_stmt)?,
            Stmt::Loop(loop_stmt) => self.bind_loop(loop_stmt)?,
            Stmt::Match(match_expr) => self.bind_match_expr(match_expr)?,
            Stmt::Break(break_stmt) => {
                if let Some(value) = &break_stmt.value {
                    self.bind_expr(value)?;
                }
                self.jump(break_stmt.label.as_deref(), false);
            }
            Stmt::Continue(_) => self.jump(None, true),
            Stmt::Assert(assert_stmt) => self.bind_assert(assert_stmt)?,
            Stmt::LabeledBlock(labeled_block) => {
                self.bind_labeled_block(&labeled_block.label, &labeled_block.block)?
            }
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
            // It diverges, so only the matching path reaches what follows.
            if let Some(else_block) = &let_stmt.else_block {
                let matched = self.flow.clone();
                self.bind_block(else_block)?;
                self.flow = matched;
            }

            self.bind_pattern_as(
                &let_stmt.pattern,
                BindingKind::Initialized {
                    is_mut: let_stmt.is_mut,
                },
                let_stmt.span,
            )
        } else {
            // `let x: T;`, which the parser guarantees is annotated.
            self.bind_pattern_as(
                &let_stmt.pattern,
                BindingKind::Uninitialized {
                    is_mut: let_stmt.is_mut,
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
            BindingKind::Initialized { is_mut } => self.define(name, is_mut || pattern_mut, span),
            BindingKind::Uninitialized { is_mut } => {
                self.define_uninit(name, is_mut || pattern_mut, span)
            }
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
        self.flow = Flow::default();
        Ok(())
    }

    /// Bind an `if`, as a statement or an expression. Without an `else`, the
    /// path that skips the then block meets the one through it.
    fn bind_if(
        &mut self,
        condition: &Condition,
        then_block: &Block,
        else_block: Option<&Block>,
    ) -> Result<(), Bail> {
        let is_let_chain = matches!(condition, Condition::LetChain { .. });
        if is_let_chain {
            // Enter one scope for all chain elements and then_block.
            // Pattern bindings are visible in subsequent elements and then_block,
            // but NOT in else_block (scoped out before else).
            self.enter_scope();
        }

        self.bind_condition(condition)?;
        let skipped = self.flow.clone();
        self.bind_block(then_block)?;

        if is_let_chain {
            self.exit_scope();
        }

        let after_then = std::mem::replace(&mut self.flow, skipped);
        if let Some(else_block) = else_block {
            self.bind_block(else_block)?;
        }
        self.flow = std::mem::take(&mut self.flow).join(after_then);
        Ok(())
    }

    /// Bind a loop. `iteration` binds one pass from the loop's head and answers
    /// the path that leaves at the head without running the body; `update`,
    /// a C-style `for`'s, runs on every path that goes round again. A body that
    /// assigns may run again, so when locals declared without an initializer
    /// are in scope a quiet pass first learns what reaches the next iteration.
    /// Assigning only removes from `unassigned` and adds to `assigned`, so the
    /// second pass starts from the loop's fixed point.
    fn bind_loop_with(
        &mut self,
        update: Option<&Expr>,
        mut iteration: impl FnMut(&mut Self) -> Result<Flow, Bail>,
    ) -> Result<(), Bail> {
        if !self.deferred.is_empty() {
            let entry = self.flow.clone();
            self.quiet += 1;
            let (_, repeats) = self.bind_iteration(update, &mut iteration)?;
            self.quiet -= 1;
            self.flow = entry.join(repeats);
        }
        let (exits, _) = self.bind_iteration(update, &mut iteration)?;
        self.flow = exits;
        Ok(())
    }

    /// One pass of a loop body: the paths that leave the loop, and those that
    /// go round again.
    fn bind_iteration(
        &mut self,
        update: Option<&Expr>,
        iteration: &mut impl FnMut(&mut Self) -> Result<Flow, Bail>,
    ) -> Result<(Flow, Flow), Bail> {
        self.jump_targets.push(JumpTarget::Loop {
            exits: Flow::default(),
            repeats: Flow::default(),
        });
        let skipped = iteration(self)?;
        let Some(JumpTarget::Loop { exits, repeats }) = self.jump_targets.pop() else {
            panic!("a loop pops the jump target it pushed");
        };
        self.flow = std::mem::take(&mut self.flow).join(repeats);
        if let Some(update) = update {
            self.bind_expr(update)?;
        }
        let repeats = std::mem::take(&mut self.flow);
        Ok((skipped.join(exits), repeats))
    }

    /// Bind a labeled block, which a `break` to its label leaves.
    fn bind_labeled_block(&mut self, label: &str, block: &Block) -> Result<(), Bail> {
        self.jump_targets.push(JumpTarget::Label {
            label: label.to_string(),
            exits: Flow::default(),
        });
        self.bind_block(block)?;
        let Some(JumpTarget::Label { exits, .. }) = self.jump_targets.pop() else {
            panic!("a labeled block pops the jump target it pushed");
        };
        self.flow = std::mem::take(&mut self.flow).join(exits);
        Ok(())
    }

    /// Bind a while statement. It may run its body zero times, whatever its
    /// condition.
    fn bind_while(&mut self, while_stmt: &WhileStmt) -> Result<(), Bail> {
        self.bind_loop_with(None, |s| {
            let is_let_chain = matches!(while_stmt.condition, Condition::LetChain { .. });
            if is_let_chain {
                s.enter_scope();
            }
            s.bind_condition(&while_stmt.condition)?;
            let skipped = s.flow.clone();
            s.bind_block(&while_stmt.body)?;
            if is_let_chain {
                s.exit_scope();
            }
            Ok(skipped)
        })
    }

    /// Bind a for statement
    fn bind_for(&mut self, for_stmt: &ForStmt) -> Result<(), Bail> {
        self.enter_scope();

        if let Some(ref init) = for_stmt.init {
            self.bind_stmt(init)?;
        }

        self.bind_loop_with(for_stmt.update.as_ref(), |s| {
            if let Some(ref condition) = for_stmt.condition {
                s.bind_condition(condition)?;
            }
            let skipped = s.flow.clone();
            s.bind_block(&for_stmt.body)?;
            Ok(skipped)
        })?;

        self.exit_scope();
        Ok(())
    }

    /// Bind a for-of statement: `for let item of array { ... }`
    fn bind_for_of(&mut self, for_of_stmt: &ForOfStmt) -> Result<(), Bail> {
        // First bind the iterable expression (uses variables from outer scope)
        self.bind_expr(&for_of_stmt.iterable)?;

        self.bind_loop_with(None, |s| {
            let skipped = s.flow.clone();
            s.enter_scope();
            s.bind_pattern_as(
                &for_of_stmt.binding,
                BindingKind::Initialized {
                    is_mut: for_of_stmt.is_mut,
                },
                for_of_stmt.span,
            )?;
            s.bind_block(&for_of_stmt.body)?;
            s.exit_scope();
            Ok(skipped)
        })
    }

    /// Bind a `loop`, which only a `break` leaves.
    fn bind_loop(&mut self, loop_stmt: &LoopStmt) -> Result<(), Bail> {
        self.bind_loop_with(None, |s| {
            s.bind_block(&loop_stmt.body)?;
            Ok(Flow::default())
        })
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
            Expr::Ident(ident) => self.read(&ident.name, ident.span)?,

            Expr::Assign(assign) => {
                self.bind_expr(&assign.value)?;
                match &assign.target {
                    Expr::Ident(ident) => self.assign(&ident.name, assign.span)?,
                    target => self.bind_expr(target)?,
                }
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
                self.bind_if(
                    &if_expr.condition,
                    &if_expr.then_block,
                    if_expr.else_block.as_ref(),
                )?;
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

            Expr::LabeledBlock(lb) => self.bind_labeled_block(&lb.label, &lb.block)?,

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

            // Literals don't reference variables
            Expr::Literal(_) => {}

            // Parser error-recovery placeholder: nothing to bind.
            Expr::Error(_) => {}
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

        // A match with no arms is over an empty type, so nothing follows it.
        let scrutinized = std::mem::take(&mut self.flow);
        for arm in &match_expr.arms {
            let after_arms = std::mem::replace(&mut self.flow, scrutinized.clone());
            self.enter_scope();
            self.bind_pattern(&arm.pattern, arm.span)?;
            if let Some(guard) = &arm.guard {
                self.bind_expr(guard)?;
            }
            self.bind_expr(&arm.body)?;
            self.exit_scope();
            self.flow = std::mem::take(&mut self.flow).join(after_arms);
        }
        Ok(())
    }

    /// Bind a refutable pattern's names.
    fn bind_pattern(&mut self, pattern: &Pattern, span: Span) -> Result<(), Bail> {
        self.bind_pattern_as(pattern, BindingKind::Initialized { is_mut: false }, span)
    }

    /// Bind a closure. Its body runs whenever it is called, if ever, so what
    /// it assigns reaches nothing after it.
    fn bind_closure(&mut self, closure: &ClosureExpr) -> Result<(), Bail> {
        let created = self.flow.clone();
        self.enter_scope();
        self.jump_targets.push(JumpTarget::Closure {
            scope_depth: self.current_depth,
        });

        for param in &closure.params {
            self.define(&param.name, param.is_mut, closure.span)?;
        }
        self.bind_expr(&closure.body)?;

        self.jump_targets.pop();
        self.exit_scope();
        self.flow = created;
        Ok(())
    }

    /// Enter a new scope
    fn enter_scope(&mut self) {
        self.current_depth += 1;
        self.scopes.push(Scope::new());
    }

    /// Exit the current scope, forgetting its locals.
    fn exit_scope(&mut self) {
        if let Some(scope) = self.scopes.pop() {
            for (name, binding) in scope.bindings {
                let key = (binding.scope_depth, name);
                self.deferred.shift_remove(&key);
                self.flow.forget(&key);
            }
        }
        self.current_depth -= 1;
    }

    /// Define a variable in the current scope
    fn define(&mut self, name: &str, is_mut: bool, span: Span) -> Result<(), Bail> {
        if is_statement_keyword(name) {
            return self.emit(BindError::KeywordName {
                name: name.to_string(),
                span,
            });
        }
        // A name the scope already holds is replaced; the resolver reports the
        // redeclarations, since only it knows which bare names bind.
        let key = (self.current_depth, name.to_string());
        self.deferred.shift_remove(&key);
        self.flow.forget(&key);
        let scope = self.scopes.last_mut().unwrap();
        scope.bindings.insert(
            name.to_string(),
            BindingInfo {
                is_mut,
                scope_depth: self.current_depth,
            },
        );
        Ok(())
    }

    /// Like `define`, for a `let x: T;`, which starts unassigned.
    fn define_uninit(&mut self, name: &str, is_mut: bool, span: Span) -> Result<(), Bail> {
        self.define(name, is_mut, span)?;
        let key = (self.current_depth, name.to_string());
        self.deferred.insert(key.clone());
        if self.flow.reachable {
            self.flow.unassigned.insert(key);
        }
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
