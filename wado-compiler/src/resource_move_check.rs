use crate::ast::{
    self, AssignExpr, AstId, Block, Condition, ConditionElement, Expr, Function, IdentExpr, Item,
    Pattern, Stmt,
};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::semantics::Semantics;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;

/// Whether `type_id` transitively owns an affine resource, making a binding of
/// that type move-only: a bare resource (`Resource` / `GenericResource`), or a
/// struct / tuple / `Result` that carries one. A reference stops the walk — a
/// borrowed place owns nothing. `visited` guards against recursive types.
///
/// The aggregate set is kept in step with `resource_cleanup::carries_resource`
/// (which synthesizes the compositional destructor): a type is treated as
/// move-only here only if the cleanup pass can also drop it, so the move check
/// never enforces move-only on an aggregate the runtime would then leak. User
/// `variant`s and generic containers (`Option` / `List` / …) are out of both
/// until their destructors land.
fn type_carries_resource(sem: &Semantics, type_id: TypeId, visited: &mut Vec<TypeId>) -> bool {
    let base = sem.types.get_ultimate_base_type(type_id);
    if visited.contains(&base) {
        return false;
    }
    visited.push(base);
    let children: Vec<TypeId> = match sem.types.get(base) {
        ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. } => return true,
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => return false,
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => {
            let (name, module_source) = (name.clone(), module_source.clone());
            sem.struct_field_type_ids(&name, &module_source)
                .unwrap_or_default()
        }
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == "Result" => type_args.clone(),
        _ => sem.types.as_tuple(base).unwrap_or_default(),
    };
    children
        .into_iter()
        .any(|t| type_carries_resource(sem, t, visited))
}

#[derive(Debug, Clone)]
pub struct ResourceMoveError {
    pub name: String,
    pub use_span: Span,
    pub move_span: Span,
    pub module: String,
}

impl std::fmt::Display for ResourceMoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: resource `{}` used after it was moved (moved at {}:{})",
            self.use_span.line,
            self.use_span.column,
            self.name,
            self.move_span.line,
            self.move_span.column,
        )
    }
}

impl std::error::Error for ResourceMoveError {}

impl From<ResourceMoveError> for crate::compiler_host::Diagnostic {
    fn from(e: ResourceMoveError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: format!(
                "resource `{}` used after it was moved (moved at {}:{})",
                e.name, e.move_span.line, e.move_span.column,
            ),
            span: Some(DiagnosticSpan::from_span(&e.use_span, Some(&e.module))),
        }
    }
}

#[must_use]
pub fn check_resource_moves_semantic(sem: &Semantics) -> Vec<ResourceMoveError> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        for item in &module.items {
            check_item(sem, src, item, &mut out);
        }
    }
    out
}

fn check_item(
    sem: &Semantics,
    module: &ModuleSource,
    item: &Item,
    out: &mut Vec<ResourceMoveError>,
) {
    match item {
        Item::Function(func) => check_function(sem, module, func, out),
        Item::Impl(impl_block) => {
            for method in &impl_block.methods {
                check_function(sem, module, method, out);
            }
        }
        Item::Trait(trait_decl) => {
            for method in &trait_decl.methods {
                check_function(sem, module, method, out);
            }
        }
        Item::Test(test) => check_block(sem, module, &test.body, out),
        _ => {}
    }
}

fn check_function(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    out: &mut Vec<ResourceMoveError>,
) {
    if let Some(body) = &func.body {
        check_block(sem, module, body, out);
    }
}

fn check_block(
    sem: &Semantics,
    module: &ModuleSource,
    body: &Block,
    out: &mut Vec<ResourceMoveError>,
) {
    let mut walker = MoveWalker {
        sem,
        module,
        moved: IndexMap::default(),
        suppress: 0,
        out,
    };
    walker.visit_block(body);
}

struct MoveWalker<'a> {
    sem: &'a Semantics,
    module: &'a ModuleSource,
    moved: IndexMap<AstId, Span>,
    suppress: u32,
    out: &'a mut Vec<ResourceMoveError>,
}

impl MoveWalker<'_> {
    fn resource_def(&self, use_id: AstId) -> Option<AstId> {
        let def = self.sem.referenced_symbol(use_id)?;
        let type_id = self.sem.expression_type(use_id)?;
        (type_carries_resource(self.sem, type_id, &mut Vec::new())).then_some(def)
    }

    fn read(&mut self, ident: &IdentExpr) {
        if let Some(def) = self.resource_def(ident.id)
            && let Some(&move_span) = self.moved.get(&def)
        {
            self.emit(ident, move_span);
        }
    }

    fn consume(&mut self, ident: &IdentExpr) {
        if let Some(def) = self.resource_def(ident.id) {
            if let Some(&move_span) = self.moved.get(&def) {
                self.emit(ident, move_span);
            } else {
                self.moved.insert(def, ident.span);
            }
        }
    }

    fn emit(&mut self, ident: &IdentExpr, move_span: Span) {
        if self.suppress > 0 {
            return;
        }
        self.out.push(ResourceMoveError {
            name: ident.name.clone(),
            use_span: ident.span,
            move_span,
            module: self.module.source_path(),
        });
    }

    fn visit_value(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(ident) => self.consume(ident),
            other => self.visit_expr(other),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(ident) => self.read(ident),

            Expr::Call(call) => {
                self.visit_expr(&call.callee);
                for arg in &call.args {
                    self.visit_value(arg);
                }
            }
            Expr::MethodCall(mc) => {
                if self.sem.method_call_consumes_receiver(mc.id) {
                    self.visit_value(&mc.receiver);
                } else {
                    self.visit_expr(&mc.receiver);
                }
                for arg in &mc.args {
                    self.visit_value(arg);
                }
            }
            Expr::StaticMethodCall(smc) => {
                for arg in &smc.args {
                    self.visit_value(arg);
                }
            }

            Expr::Unary(u) => self.visit_expr(&u.expr),
            Expr::Binary(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
            }
            Expr::Assign(a) => self.visit_assign(a),
            Expr::CompoundAssign(a) => {
                self.visit_expr(&a.target);
                self.visit_expr(&a.value);
            }
            Expr::FieldAccess(fa) => self.visit_expr(&fa.expr),
            Expr::Index(ix) => {
                self.visit_expr(&ix.expr);
                self.visit_expr(&ix.index);
            }
            Expr::Cast(c) => self.visit_expr(&c.expr),
            Expr::TryOp(t) => self.visit_value(&t.expr),

            Expr::StructLiteral(s) => {
                for field in &s.fields {
                    self.visit_value(&field.value);
                }
            }
            Expr::TupleLiteral(t) => {
                for el in &t.elements {
                    self.visit_value(el);
                }
            }
            Expr::TemplateString(ts) => {
                for part in &ts.parts {
                    if let ast::TemplatePart::Interpolation { expr, .. } = part {
                        self.visit_expr(expr);
                    }
                }
            }

            Expr::Block(block) => {
                self.visit_block(block);
            }
            Expr::LabeledBlock(lb) => {
                self.visit_block(&lb.block);
            }
            Expr::If(if_expr) => {
                self.visit_condition(&if_expr.condition);
                self.merge_if(&if_expr.then_block, if_expr.else_block.as_ref());
            }
            Expr::Match(m) => {
                self.visit_match(&m.expr, &m.arms);
            }

            Expr::WithHandler(w) => {
                for handler in &w.handlers {
                    self.visit_expr(&handler.handler);
                }
                self.visit_block(&w.body);
            }
            Expr::Resume(r) => self.visit_value(&r.value),
            Expr::Range(r) => {
                self.visit_expr(&r.start);
                self.visit_expr(&r.end);
            }
            Expr::Matches(m) => self.visit_expr(&m.expr),
            Expr::Spread(inner, _) => self.visit_expr(inner),
            Expr::ComparisonChain(c) => {
                self.visit_expr(&c.first);
                for cmp in &c.comparisons {
                    self.visit_expr(&cmp.right);
                }
            }

            Expr::Closure(_) | Expr::Literal(_) | Expr::Error(_) => {}
        }
    }

    fn visit_assign(&mut self, a: &AssignExpr) {
        self.visit_value(&a.value);
        match &a.target {
            Expr::Ident(ident) => {
                if let Some(def) = self.resource_def(ident.id) {
                    self.moved.swap_remove(&def);
                }
            }
            other => self.visit_expr(other),
        }
    }

    fn visit_block(&mut self, block: &Block) -> bool {
        for stmt in &block.stmts {
            if self.visit_stmt(stmt) {
                return true;
            }
        }
        false
    }

    fn visit_stmt(&mut self, stmt: &Stmt) -> bool {
        match stmt {
            Stmt::Let(l) => {
                if let Some(value) = &l.value {
                    self.visit_value(value);
                }
                self.clear_pattern(&l.pattern);
                false
            }
            Stmt::Expr(e) => {
                self.visit_value(&e.expr);
                false
            }
            Stmt::Return(r) => {
                if let Some(value) = &r.value {
                    self.visit_value(value);
                }
                true
            }
            Stmt::TaskReturn(t) => {
                self.visit_value(&t.value);
                false
            }
            Stmt::If(s) => {
                self.visit_condition(&s.condition);
                self.merge_if(&s.then_block, s.else_block.as_ref())
            }
            Stmt::While(s) => {
                self.visit_condition(&s.condition);
                self.visit_loop_body(&s.body, None);
                false
            }
            Stmt::Loop(s) => {
                self.visit_loop_body(&s.body, None);
                false
            }
            Stmt::For(s) => {
                if let Some(init) = &s.init {
                    self.visit_stmt(init);
                }
                if let Some(cond) = &s.condition {
                    self.visit_condition(cond);
                }
                if let Some(update) = &s.update {
                    self.visit_expr(update);
                }
                self.visit_loop_body(&s.body, None);
                false
            }
            Stmt::ForOf(s) => {
                self.visit_expr(&s.iterable);
                self.visit_loop_body(&s.body, Some(&s.binding));
                false
            }
            Stmt::Match(m) => self.visit_match(&m.expr, &m.arms),
            Stmt::Break(b) => {
                if let Some(value) = &b.value {
                    self.visit_value(value);
                }
                true
            }
            Stmt::Assert(a) => {
                self.visit_expr(&a.condition);
                if let Some(msg) = &a.message {
                    self.visit_expr(msg);
                }
                false
            }
            Stmt::LabeledBlock(lb) => self.visit_block(&lb.block),
            Stmt::Continue(_) => true,
            Stmt::Item(item) => {
                check_item(self.sem, self.module, item, self.out);
                false
            }
            Stmt::Error(_) => false,
        }
    }

    fn visit_loop_body(&mut self, body: &Block, rebind: Option<&Pattern>) {
        if let Some(pattern) = rebind {
            self.clear_pattern(pattern);
        }
        self.suppress += 1;
        self.visit_block(body);
        self.suppress -= 1;
        if let Some(pattern) = rebind {
            self.clear_pattern(pattern);
        }
        self.visit_block(body);
    }

    fn clear_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Ident { id, .. } | Pattern::MutIdent { id, .. } => {
                self.moved.swap_remove(id);
            }
            Pattern::Tuple(subs, _) => {
                for sub in subs {
                    self.clear_pattern(sub);
                }
            }
            Pattern::Variant { bindings, .. } => {
                for sub in bindings {
                    self.clear_pattern(sub);
                }
            }
            Pattern::Struct { fields, .. } => {
                for field in fields {
                    self.clear_pattern(&field.pattern);
                }
            }
            Pattern::Or(alts) => {
                for alt in alts {
                    self.clear_pattern(alt);
                }
            }
            _ => {}
        }
    }

    fn merge_if(&mut self, then_block: &Block, else_block: Option<&Block>) -> bool {
        let base = self.moved.clone();
        let then_div = self.visit_block(then_block);
        let then_moved = std::mem::replace(&mut self.moved, base);
        let else_div = match else_block {
            Some(eb) => self.visit_block(eb),
            None => false,
        };
        if !then_div {
            self.union_into(then_moved);
        }
        else_block.is_some() && then_div && else_div
    }

    fn visit_match(&mut self, scrutinee: &Expr, arms: &[ast::MatchArm]) -> bool {
        self.visit_expr(scrutinee);
        let base = std::mem::take(&mut self.moved);
        let mut merged = base.clone();
        let mut all_diverge = true;
        for arm in arms {
            self.moved.clone_from(&base);
            if let Some(guard) = &arm.guard {
                self.visit_expr(guard);
            }
            let diverged = self.visit_expr_diverges(&arm.body);
            if !diverged {
                all_diverge = false;
                let arm_moved = std::mem::take(&mut self.moved);
                for (def, span) in arm_moved {
                    merged.entry(def).or_insert(span);
                }
            }
        }
        self.moved = merged;
        all_diverge
    }

    fn visit_expr_diverges(&mut self, expr: &Expr) -> bool {
        match expr {
            Expr::Block(block) => self.visit_block(block),
            Expr::LabeledBlock(lb) => self.visit_block(&lb.block),
            other => {
                self.visit_expr(other);
                false
            }
        }
    }

    fn visit_condition(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    match element {
                        ConditionElement::Let { expr, .. } => self.visit_expr(expr),
                        ConditionElement::Expr(e) => self.visit_expr(e),
                    }
                }
            }
        }
    }

    fn union_into(&mut self, other: IndexMap<AstId, Span>) {
        for (def, span) in other {
            self.moved.entry(def).or_insert(span);
        }
    }
}
