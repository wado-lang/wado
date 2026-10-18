use crate::ast::{
    self, AssignExpr, AstId, Block, Condition, ConditionElement, Expr, Function, IdentExpr, Item,
    Pattern, Stmt, for_each_pattern_binding,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::semantics::Semantics;
use crate::token::Span;

#[derive(Debug, Clone)]
pub enum ResourceMoveError {
    /// A resource binding used after a by-value consumption moved it.
    UseAfterMove {
        name: String,
        use_span: Span,
        move_span: Span,
        module: String,
    },
    /// A resource moved out of a referenced place (`self.f` on `&self`, a binding
    /// from `match *self`). The source keeps ownership, so the extracted handle
    /// would alias it; take `self` by value to consume, or return a fresh
    /// resource.
    MoveOutOfReference {
        /// The resource type being extracted.
        type_name: String,
        span: Span,
        module: String,
    },
    /// A named struct or variant holding an affine value without `affine`.
    AffineUndeclared {
        keyword: &'static str,
        name: String,
        /// The field or case that holds it.
        member: String,
        span: Span,
        module: String,
    },
    /// A named struct or variant declared `affine` that holds no affine value.
    AffineHoldsNothing {
        keyword: &'static str,
        name: String,
        span: Span,
        module: String,
    },
}

impl From<ResourceMoveError> for Diagnostic {
    fn from(e: ResourceMoveError) -> Self {
        let (code, message, span, module) = match e {
            ResourceMoveError::UseAfterMove {
                name,
                use_span,
                move_span,
                module,
            } => (
                Code::TypeMismatch,
                format!(
                    "resource `{name}` used after it was moved (moved at {}:{})",
                    move_span.line, move_span.column,
                ),
                use_span,
                module,
            ),
            ResourceMoveError::MoveOutOfReference {
                type_name,
                span,
                module,
            } => (
                Code::TypeMismatch,
                format!(
                    "cannot move resource `{type_name}` out of a reference; take `self` by value to consume it, or return a freshly created resource"
                ),
                span,
                module,
            ),
            ResourceMoveError::AffineUndeclared {
                keyword,
                name,
                member,
                span,
                module,
            } => (
                Code::AffineDeclaration,
                format!(
                    "`{name}` holds an affine value in `{member}`, so it must be declared `affine {keyword}`"
                ),
                span,
                module,
            ),
            ResourceMoveError::AffineHoldsNothing {
                keyword,
                name,
                span,
                module,
            } => (
                Code::AffineDeclaration,
                format!(
                    "`{name}` holds no affine value, so it cannot be declared `affine {keyword}`"
                ),
                span,
                module,
            ),
        };
        Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&span, Some(&module))),
        }
    }
}

#[must_use]
pub fn check_resource_moves_semantic(sem: &Semantics) -> Vec<ResourceMoveError> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        for item in &module.items {
            check_affine_declaration(sem, src, item, &mut out);
        }
        if !src.is_user_authored() {
            continue;
        }
        for item in &module.items {
            check_item(sem, src, item, &mut out);
        }
    }
    out
}

/// A named struct or variant is declared `affine` exactly when a field or a
/// case holds an affine value in every instantiation (WEP: Affine Types).
fn check_affine_declaration(
    sem: &Semantics,
    module: &ModuleSource,
    item: &Item,
    out: &mut Vec<ResourceMoveError>,
) {
    let (keyword, id, name, name_span, is_affine, members): (_, _, _, _, _, Vec<&str>) =
        match item {
            Item::Struct(s) => (
                "struct",
                s.id,
                &s.name,
                s.name_span,
                s.is_affine,
                s.fields.iter().map(|f| f.name.as_str()).collect(),
            ),
            Item::Variant(v) => (
                "variant",
                v.id,
                &v.name,
                v.name_span,
                v.is_affine,
                v.cases.iter().map(|c| c.name.as_str()).collect(),
            ),
            _ => return,
        };
    let module = module.source_path();
    match (sem.first_affine_member(id), is_affine) {
        (Some(index), false) => out.push(ResourceMoveError::AffineUndeclared {
            keyword,
            name: name.clone(),
            member: members[index].to_string(),
            span: name_span,
            module,
        }),
        (None, true) => out.push(ResourceMoveError::AffineHoldsNothing {
            keyword,
            name: name.clone(),
            span: name_span,
            module,
        }),
        _ => {}
    }
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
        Item::Interface(interface_decl) => {
            for method in &interface_decl.methods {
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
        check_reference_extraction(sem, module, func, out);
    }
}

/// Reject moving a resource out of a referenced place: a `&self` / `&T`-param
/// method whose returned value is rooted in that reference and carries a resource
/// (`fn peek(&self) -> Fields { return self.f; }`). The reference keeps the source
/// owned, so the extracted handle would alias it — a double-drop. Extraction
/// must consume by value (`self`) or return a freshly produced resource.
fn check_reference_extraction(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    out: &mut Vec<ResourceMoveError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    let mut referenced: Vec<AstId> = Vec::new();
    for p in &func.params {
        let is_ref = matches!(p.self_kind, ast::SelfKind::Ref | ast::SelfKind::MutRef)
            || matches!(&p.ty, ast::Type::Reference(_) | ast::Type::MutReference(_));
        if is_ref {
            referenced.push(p.id);
        }
    }
    if referenced.is_empty() {
        return;
    }
    let mut checker = ReferenceChecker {
        sem,
        module,
        referenced,
        out,
    };
    checker.walk_block(body);
}

/// Tracks which bindings are rooted in a reference parameter and flags returns
/// that carry a resource out of one.
struct ReferenceChecker<'a> {
    sem: &'a Semantics,
    module: &'a ModuleSource,
    /// Def `AstId`s rooted in a reference: reference params plus `let` bindings
    /// and match-arm bindings projected from a referenced place.
    referenced: Vec<AstId>,
    out: &'a mut Vec<ResourceMoveError>,
}

impl ReferenceChecker<'_> {
    /// Whether `expr`'s value is rooted in a referenced place. `extra` holds
    /// match-arm bindings in scope for the current arm.
    fn roots_in_reference(&self, expr: &Expr, extra: &[AstId]) -> bool {
        match expr {
            Expr::Ident(x) => self
                .sem
                .referenced_symbol(x.id)
                .is_some_and(|def| self.referenced.contains(&def) || extra.contains(&def)),
            Expr::FieldAccess(fa) => self.roots_in_reference(&fa.expr, extra),
            Expr::Unary(u) => self.roots_in_reference(&u.expr, extra),
            Expr::Cast(c) => self.roots_in_reference(&c.expr, extra),
            Expr::Index(ix) => self.roots_in_reference(&ix.expr, extra),
            Expr::Block(b) => self
                .block_tail(b)
                .is_some_and(|t| self.roots_in_reference(t, extra)),
            Expr::LabeledBlock(lb) => self
                .block_tail(&lb.block)
                .is_some_and(|t| self.roots_in_reference(t, extra)),
            Expr::If(ife) => {
                self.block_tail(&ife.then_block)
                    .is_some_and(|t| self.roots_in_reference(t, extra))
                    || ife
                        .else_block
                        .as_ref()
                        .and_then(|eb| self.block_tail(eb))
                        .is_some_and(|t| self.roots_in_reference(t, extra))
            }
            Expr::Match(m) => {
                let scrut_referenced = self.roots_in_reference(&m.expr, extra);
                m.arms.iter().any(|arm| {
                    let mut arm_extra = extra.to_vec();
                    if scrut_referenced {
                        collect_pattern_bindings(&arm.pattern, &mut arm_extra);
                    }
                    self.roots_in_reference(&arm.body, &arm_extra)
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
        if !self.roots_in_reference(value, &[]) {
            return;
        }
        let Some(type_id) = self.sem.expression_type(value.id()) else {
            return;
        };
        if !self.sem.carries_resource(type_id) {
            return;
        }
        self.out.push(ResourceMoveError::MoveOutOfReference {
            type_name: self.sem.types.type_name(type_id),
            span: value.span(),
            module: self.module.source_path(),
        });
    }

    fn walk_block(&mut self, block: &Block) {
        let saved = self.referenced.len();
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
        self.referenced.truncate(saved);
    }

    fn walk_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(l) => {
                if let Some(value) = &l.value {
                    if self.roots_in_reference(value, &[]) {
                        collect_pattern_bindings(&l.pattern, &mut self.referenced);
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
            Stmt::Item(item) => {
                check_affine_declaration(self.sem, self.module, item, self.out);
                check_item(self.sem, self.module, item, self.out);
            }
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
        let scrut_referenced = self.roots_in_reference(scrutinee, &[]);
        for arm in arms {
            let saved = self.referenced.len();
            if scrut_referenced {
                collect_pattern_bindings(&arm.pattern, &mut self.referenced);
            }
            self.walk_nested(&arm.body);
            self.referenced.truncate(saved);
        }
    }
}

/// The def `AstId`s a pattern binds, matching what `referenced_symbol` resolves
/// a later use to.
fn collect_pattern_bindings(pattern: &Pattern, out: &mut Vec<AstId>) {
    for_each_pattern_binding(pattern, &mut |id| out.push(id));
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

/// What a loop evaluates at the start of each iteration.
enum LoopHead<'a> {
    None,
    /// `while` and C-style `for`: a `let` in it binds afresh each time.
    Condition(&'a Condition),
    /// `for ... of`: the binding takes the next element.
    Binding(&'a Pattern),
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
        self.sem.carries_resource(type_id).then_some(def)
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
            Expr::TupleComprehension(c) => {
                self.visit_value(&c.iterable);
                self.visit_value(&c.body);
            }
            Expr::TemplateString(ts) => {
                for expr in ts.interpolations() {
                    self.visit_expr(expr);
                }
            }
            // A hole is read or referenced where it stands, never moved.
            Expr::TaggedTemplate(t) => {
                for expr in t.template.interpolations() {
                    self.visit_expr(expr);
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
                self.visit_loop(&LoopHead::Condition(&s.condition), &s.body, None);
                false
            }
            Stmt::Loop(s) => {
                self.visit_loop(&LoopHead::None, &s.body, None);
                false
            }
            Stmt::For(s) => {
                if let Some(init) = &s.init {
                    self.visit_stmt(init);
                }
                let head = match &s.condition {
                    Some(cond) => LoopHead::Condition(cond),
                    None => LoopHead::None,
                };
                self.visit_loop(&head, &s.body, s.update.as_ref());
                false
            }
            Stmt::ForOf(s) => {
                self.visit_expr(&s.iterable);
                self.visit_loop(&LoopHead::Binding(&s.binding), &s.body, None);
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

    /// A loop as two iterations: the first records what it moves, and the
    /// second reports what reads a value the first moved. Each iteration runs
    /// the head, the body, then `update`, as a C-style `for` does.
    fn visit_loop(&mut self, head: &LoopHead, body: &Block, update: Option<&Expr>) {
        self.suppress += 1;
        self.visit_iteration(head, body, update);
        self.suppress -= 1;
        self.visit_iteration(head, body, update);
    }

    fn visit_iteration(&mut self, head: &LoopHead, body: &Block, update: Option<&Expr>) {
        match head {
            LoopHead::None => {}
            LoopHead::Condition(condition) => self.visit_condition(condition),
            LoopHead::Binding(pattern) => self.clear_pattern(pattern),
        }
        self.visit_block(body);
        if let Some(update) = update {
            self.visit_expr(update);
        }
    }

    /// The bindings `pattern` makes hold new values, none of them moved.
    fn clear_pattern(&mut self, pattern: &Pattern) {
        for_each_pattern_binding(pattern, &mut |id| {
            self.moved.swap_remove(&id);
        });
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
            self.clear_pattern(&arm.pattern);
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

    /// A `let` in the chain binds its pattern afresh, so a value an earlier
    /// iteration moved out of that binding is not the one it now holds.
    fn visit_condition(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    self.visit_expr(element.expr());
                    if let ConditionElement::Let { pattern, .. } = element {
                        self.clear_pattern(pattern);
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
