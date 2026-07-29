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
            decl_name: name,
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
pub enum ResourceMoveError {
    /// A resource binding used after a by-value consumption moved it.
    UseAfterMove {
        name: String,
        use_span: Span,
        move_span: Span,
        module: String,
    },
    /// A resource moved out of a borrowed place (`self.f` on `&self`, a binding
    /// from `match *self`). The source keeps ownership, so the extracted handle
    /// would alias it; take `self` by value to consume, or return a fresh
    /// resource.
    MoveOutOfBorrow {
        /// The resource type being extracted.
        type_name: String,
        span: Span,
        module: String,
    },
}

impl std::fmt::Display for ResourceMoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourceMoveError::UseAfterMove {
                name,
                use_span,
                move_span,
                ..
            } => write!(
                f,
                "{}:{}: resource `{}` used after it was moved (moved at {}:{})",
                use_span.line, use_span.column, name, move_span.line, move_span.column,
            ),
            ResourceMoveError::MoveOutOfBorrow {
                type_name, span, ..
            } => write!(
                f,
                "{}:{}: cannot move resource `{type_name}` out of a borrow",
                span.line, span.column,
            ),
        }
    }
}

impl std::error::Error for ResourceMoveError {}

impl From<ResourceMoveError> for crate::compiler_host::Diagnostic {
    fn from(e: ResourceMoveError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let (message, span, module) = match e {
            ResourceMoveError::UseAfterMove {
                name,
                use_span,
                move_span,
                module,
            } => (
                format!(
                    "resource `{name}` used after it was moved (moved at {}:{})",
                    move_span.line, move_span.column,
                ),
                use_span,
                module,
            ),
            ResourceMoveError::MoveOutOfBorrow {
                type_name,
                span,
                module,
            } => (
                format!(
                    "cannot move resource `{type_name}` out of a borrow; take `self` by value to consume it, or return a freshly created resource"
                ),
                span,
                module,
            ),
        };
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message,
            span: Some(DiagnosticSpan::from_span(&span, Some(&module))),
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
        check_borrow_extraction(sem, module, func, out);
    }
}

/// Reject moving a resource out of a borrowed place: a `&self` / `&T`-param
/// method whose returned value is rooted in that borrow and carries a resource
/// (`fn peek(&self) -> Fields { return self.f; }`). The borrow keeps the source
/// owned, so the extracted handle would alias it — a double-drop. Extraction
/// must consume by value (`self`) or return a freshly produced resource.
fn check_borrow_extraction(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    out: &mut Vec<ResourceMoveError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    let mut borrowed: Vec<AstId> = Vec::new();
    for p in &func.params {
        let is_ref = matches!(p.self_kind, ast::SelfKind::Ref | ast::SelfKind::MutRef)
            || matches!(&p.ty, ast::Type::Reference(_) | ast::Type::MutReference(_));
        if is_ref {
            borrowed.push(p.id);
        }
    }
    if borrowed.is_empty() {
        return;
    }
    let mut checker = BorrowChecker {
        sem,
        module,
        borrowed,
        out,
    };
    checker.walk_block(body);
}

/// Tracks which bindings are rooted in a borrowed parameter and flags returns
/// that carry a resource out of one.
struct BorrowChecker<'a> {
    sem: &'a Semantics,
    module: &'a ModuleSource,
    /// Def `AstId`s rooted in a borrow: borrowed params plus `let` bindings and
    /// match-arm bindings projected from a borrowed place.
    borrowed: Vec<AstId>,
    out: &'a mut Vec<ResourceMoveError>,
}

impl BorrowChecker<'_> {
    /// Whether `expr`'s value is rooted in a borrowed place. `extra` holds
    /// match-arm bindings in scope for the current arm.
    fn roots_in_borrow(&self, expr: &Expr, extra: &[AstId]) -> bool {
        match expr {
            Expr::Ident(x) => self
                .sem
                .referenced_symbol(x.id)
                .is_some_and(|def| self.borrowed.contains(&def) || extra.contains(&def)),
            Expr::FieldAccess(fa) => self.roots_in_borrow(&fa.expr, extra),
            Expr::Unary(u) => self.roots_in_borrow(&u.expr, extra),
            Expr::Cast(c) => self.roots_in_borrow(&c.expr, extra),
            Expr::Index(ix) => self.roots_in_borrow(&ix.expr, extra),
            Expr::Block(b) => self
                .block_tail(b)
                .is_some_and(|t| self.roots_in_borrow(t, extra)),
            Expr::LabeledBlock(lb) => self
                .block_tail(&lb.block)
                .is_some_and(|t| self.roots_in_borrow(t, extra)),
            Expr::If(ife) => {
                self.block_tail(&ife.then_block)
                    .is_some_and(|t| self.roots_in_borrow(t, extra))
                    || ife
                        .else_block
                        .as_ref()
                        .and_then(|eb| self.block_tail(eb))
                        .is_some_and(|t| self.roots_in_borrow(t, extra))
            }
            Expr::Match(m) => {
                let scrut_borrow = self.roots_in_borrow(&m.expr, extra);
                m.arms.iter().any(|arm| {
                    let mut arm_extra = extra.to_vec();
                    if scrut_borrow {
                        collect_pattern_bindings(&arm.pattern, &mut arm_extra);
                    }
                    self.roots_in_borrow(&arm.body, &arm_extra)
                })
            }
            _ => false,
        }
    }

    /// The trailing expression a block evaluates to, if its last statement is a
    /// bare expression.
    fn block_tail<'b>(&self, block: &'b Block) -> Option<&'b Expr> {
        match block.stmts.last() {
            Some(Stmt::Expr(e)) => Some(&e.expr),
            _ => None,
        }
    }

    fn check_return(&mut self, value: &Expr) {
        if !self.roots_in_borrow(value, &[]) {
            return;
        }
        let Some(type_id) = self.sem.expression_type(value.id()) else {
            return;
        };
        if !type_carries_resource(self.sem, type_id, &mut Vec::new()) {
            return;
        }
        self.out.push(ResourceMoveError::MoveOutOfBorrow {
            type_name: self.sem.types.type_name(type_id),
            span: value.span(),
            module: self.module.source_path(),
        });
    }

    fn walk_block(&mut self, block: &Block) {
        let saved = self.borrowed.len();
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
        self.borrowed.truncate(saved);
    }

    fn walk_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(l) => {
                if let Some(value) = &l.value {
                    if self.roots_in_borrow(value, &[]) {
                        collect_pattern_bindings(&l.pattern, &mut self.borrowed);
                    }
                    self.walk_nested(value);
                }
                if let Some(eb) = &l.else_block {
                    self.walk_block(eb);
                }
            }
            Stmt::Return(r) => {
                if let Some(value) = &r.value {
                    self.check_return(value);
                    self.walk_nested(value);
                }
            }
            Stmt::TaskReturn(t) => {
                self.check_return(&t.value);
                self.walk_nested(&t.value);
            }
            Stmt::Break(b) => {
                if let Some(value) = &b.value {
                    self.walk_nested(value);
                }
            }
            Stmt::Expr(e) => self.walk_nested(&e.expr),
            Stmt::If(s) => {
                self.walk_block(&s.then_block);
                if let Some(eb) = &s.else_block {
                    self.walk_block(eb);
                }
            }
            Stmt::While(s) => self.walk_block(&s.body),
            Stmt::Loop(s) => self.walk_block(&s.body),
            Stmt::For(s) => {
                if let Some(init) = &s.init {
                    self.walk_stmt(init);
                }
                self.walk_block(&s.body);
            }
            Stmt::ForOf(s) => self.walk_block(&s.body),
            Stmt::Match(m) => self.walk_match(&m.expr, &m.arms),
            Stmt::LabeledBlock(lb) => self.walk_block(&lb.block),
            Stmt::Item(item) => check_item(self.sem, self.module, item, self.out),
            _ => {}
        }
    }

    /// Descend into a control-flow expression to catch `return`s nested in it.
    fn walk_nested(&mut self, expr: &Expr) {
        match expr {
            Expr::Block(b) => self.walk_block(b),
            Expr::LabeledBlock(lb) => self.walk_block(&lb.block),
            Expr::If(ife) => {
                self.walk_block(&ife.then_block);
                if let Some(eb) = &ife.else_block {
                    self.walk_block(eb);
                }
            }
            Expr::Match(m) => self.walk_match(&m.expr, &m.arms),
            Expr::WithHandler(w) => self.walk_block(&w.body),
            _ => {}
        }
    }

    fn walk_match(&mut self, scrutinee: &Expr, arms: &[ast::MatchArm]) {
        let scrut_borrow = self.roots_in_borrow(scrutinee, &[]);
        for arm in arms {
            let saved = self.borrowed.len();
            if scrut_borrow {
                collect_pattern_bindings(&arm.pattern, &mut self.borrowed);
            }
            self.walk_nested(&arm.body);
            self.borrowed.truncate(saved);
        }
    }
}

/// Collect the def `AstId`s a pattern binds, matching what `referenced_symbol`
/// resolves a later use to.
fn collect_pattern_bindings(pattern: &Pattern, out: &mut Vec<AstId>) {
    match pattern {
        Pattern::Ident { id, .. } | Pattern::MutIdent { id, .. } => out.push(*id),
        Pattern::Tuple(subs, _) => subs.iter().for_each(|s| collect_pattern_bindings(s, out)),
        Pattern::Variant { bindings, .. } => bindings
            .iter()
            .for_each(|s| collect_pattern_bindings(s, out)),
        Pattern::Struct { fields, .. } => fields
            .iter()
            .for_each(|f| collect_pattern_bindings(&f.pattern, out)),
        Pattern::Or(alts) => alts.iter().for_each(|a| collect_pattern_bindings(a, out)),
        _ => {}
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
        self.out.push(ResourceMoveError::UseAfterMove {
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
                // A `let ... else` else block diverges: visit it for move
                // violations, then discard its move-state so the matching
                // continuation keeps the pre-else state.
                if let Some(eb) = &l.else_block {
                    let base = self.moved.clone();
                    self.visit_block(eb);
                    self.moved = base;
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
