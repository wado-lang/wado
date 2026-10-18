//! The affine move check (WEP: Affine Types): a body reads an affine value at
//! most once, and a named declaration says whether it is affine. It runs on the
//! `Semantics` layer, over every module, and needs no instantiation: a generic
//! body is checked once, with its `affine` parameters affine.

use crate::ast::{
    self, AssignExpr, AstId, Block, ClosureExpr, Condition, ConditionElement, Expr, Function,
    GenericParam, IdentExpr, ImplBlock, Item, Pattern, Stmt, TraitDecl, for_each_pattern_binding,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::semantics::Semantics;
use crate::symbol::SymbolKind;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;

#[derive(Debug, Clone)]
pub struct ResourceMoveError {
    kind: MoveError,
    span: Span,
    module: String,
}

#[derive(Debug, Clone)]
enum MoveError {
    /// A binding read after a by-value use moved it.
    UseAfterMove { name: String, moved_at: Span },
    /// An affine value read by value through a reference, which leaves it with
    /// its owner: the read would be a second owner.
    MoveOutOfReference { type_name: String },
    /// A closure body moving a binding it captured, by reference.
    CapturedMove { name: String },
    /// A guard moving a binding of its arm, which it sees by reference.
    GuardMove { name: String },
    /// A global holding an affine value, read by value.
    GlobalByValue { name: String },
    /// A named struct or variant holding an affine value without `affine`.
    AffineUndeclared {
        keyword: &'static str,
        name: String,
        /// The field or case that holds it.
        member: String,
    },
    /// A named struct or variant declared `affine` that holds no affine value.
    AffineHoldsNothing { keyword: &'static str, name: String },
    /// An affine type implementing a trait not declared `affine`.
    UnmarkedTraitImpl {
        type_name: String,
        trait_name: String,
    },
}

impl From<ResourceMoveError> for Diagnostic {
    fn from(e: ResourceMoveError) -> Self {
        let (code, message) = match e.kind {
            MoveError::UseAfterMove { name, moved_at } => (
                Code::TypeMismatch,
                format!(
                    "affine value `{name}` used after it was moved (moved at {}:{})",
                    moved_at.line, moved_at.column,
                ),
            ),
            MoveError::MoveOutOfReference { type_name } => (
                Code::TypeMismatch,
                format!("cannot move affine type `{type_name}` out of a reference"),
            ),
            MoveError::CapturedMove { name } => (
                Code::TypeMismatch,
                format!("a closure captures `{name}` by reference, so its body cannot move it"),
            ),
            MoveError::GuardMove { name } => (
                Code::TypeMismatch,
                format!("a guard sees `{name}` by reference, so it cannot move it"),
            ),
            MoveError::GlobalByValue { name } => (
                Code::TypeMismatch,
                format!("cannot read global `{name}` by value: it holds an affine value"),
            ),
            MoveError::AffineUndeclared {
                keyword,
                name,
                member,
            } => (
                Code::AffineDeclaration,
                format!(
                    "`{name}` holds an affine value in `{member}`, so it must be declared `affine {keyword}`"
                ),
            ),
            MoveError::AffineHoldsNothing { keyword, name } => (
                Code::AffineDeclaration,
                format!(
                    "`{name}` holds no affine value, so it cannot be declared `affine {keyword}`"
                ),
            ),
            MoveError::UnmarkedTraitImpl {
                type_name,
                trait_name,
            } => (
                Code::AffineDeclaration,
                format!(
                    "`{type_name}` is affine, so it implements only an `affine trait`, and `{trait_name}` is not one"
                ),
            ),
        };
        Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

#[must_use]
pub fn check_resource_moves_semantic(sem: &Semantics) -> Vec<ResourceMoveError> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        let mut checker = Checker {
            sem,
            module: src,
            out: &mut out,
        };
        for item in &module.items {
            checker.check_item(item);
        }
    }
    out
}

/// The `affine` type parameters in scope: an item's, its enclosing impl's or
/// trait's, and `Self` in an `affine trait`.
fn affine_params<'a>(params: impl IntoIterator<Item = &'a GenericParam>) -> Vec<String> {
    params
        .into_iter()
        .filter(|p| p.is_affine)
        .map(|p| p.name.clone())
        .collect()
}

struct Checker<'a> {
    sem: &'a Semantics,
    module: &'a ModuleSource,
    out: &'a mut Vec<ResourceMoveError>,
}

impl Checker<'_> {
    fn push(&mut self, kind: MoveError, span: Span) {
        self.out.push(ResourceMoveError {
            kind,
            span,
            module: self.module.source_path(),
        });
    }

    fn check_item(&mut self, item: &Item) {
        self.check_affine_declaration(item);
        match item {
            Item::Function(func) => self.check_function(func, &[]),
            Item::Impl(b) => {
                let scope = affine_params(&b.type_params);
                self.check_impl_trait(b, &scope);
                for method in &b.methods {
                    self.check_function(method, &scope);
                }
            }
            Item::Trait(t) => {
                let scope = trait_scope(t);
                for method in &t.methods {
                    self.check_function(method, &scope);
                }
            }
            Item::Interface(i) => {
                for method in &i.methods {
                    self.check_function(method, &[]);
                }
            }
            Item::Test(test) => self.check_body(&test.body, Vec::new()),
            _ => {}
        }
    }

    fn check_function(&mut self, func: &Function, outer: &[String]) {
        let Some(body) = &func.body else {
            return;
        };
        let mut scope = outer.to_vec();
        scope.extend(affine_params(&func.type_params));
        self.check_body(body, scope);
    }

    fn check_body(&mut self, body: &Block, affine_params: Vec<String>) {
        let mut walker = MoveWalker {
            checker: self,
            affine_params,
            moved: IndexMap::default(),
            pinned: IndexMap::default(),
            closure_locals: None,
            suppress: 0,
        };
        walker.visit_block(body);
    }

    /// A named struct or variant is declared `affine` exactly when a field or a
    /// case holds an affine value in every instantiation.
    fn check_affine_declaration(&mut self, item: &Item) {
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
        match (self.sem.first_affine_member(id), is_affine) {
            (Some(index), false) => self.push(
                MoveError::AffineUndeclared {
                    keyword,
                    name: name.clone(),
                    member: members[index].to_string(),
                },
                name_span,
            ),
            (None, true) => self.push(
                MoveError::AffineHoldsNothing {
                    keyword,
                    name: name.clone(),
                },
                name_span,
            ),
            _ => {}
        }
    }

    /// An affine type implements only an `affine trait`: an unmarked trait's
    /// default bodies are checked with `Self` copyable.
    fn check_impl_trait(&mut self, b: &ImplBlock, scope: &[String]) {
        let Some(trait_type) = &b.trait_type else {
            return;
        };
        let Some(Item::Trait(t)) = self.sem.implemented(b) else {
            return;
        };
        if t.is_affine || !self.impl_self_is_affine(b, scope) {
            return;
        }
        self.push(
            MoveError::UnmarkedTraitImpl {
                type_name: self.sem.types.type_name(self.impl_self_type(b).unwrap()),
                trait_name: t.name.clone(),
            },
            trait_type.span(),
        );
    }

    fn impl_self_type(&self, b: &ImplBlock) -> Option<TypeId> {
        let decl = self.sem.declaration_of(&b.ty)?;
        self.sem.types.type_of_symbol(&decl.id())
    }

    /// Whether the implementing type is affine: affine in every instantiation,
    /// or written with an `affine` parameter of the impl as an argument.
    fn impl_self_is_affine(&self, b: &ImplBlock, scope: &[String]) -> bool {
        let mut mentioned = Vec::new();
        b.ty.mentioned_names(&mut mentioned);
        let names_affine_param = mentioned.iter().any(|name| scope.contains(name));
        names_affine_param
            || self
                .impl_self_type(b)
                .is_some_and(|ty| self.sem.is_affine(ty, &[]))
    }
}

/// An `affine trait`'s default bodies are checked with `Self` affine.
fn trait_scope(t: &TraitDecl) -> Vec<String> {
    let mut scope = affine_params(&t.type_params);
    if t.is_affine {
        scope.push("Self".to_string());
    }
    scope
}

/// What a loop evaluates at the start of each iteration.
enum LoopHead<'a> {
    None,
    /// `while` and C-style `for`: a `let` in it binds afresh each time.
    Condition(&'a Condition),
    /// `for ... of`: the binding takes the next element.
    Binding(&'a Pattern),
}

/// Why a binding may be read but not moved where it is pinned.
#[derive(Clone, Copy)]
enum Pinned {
    Guard,
}

/// A place a body reads: a binding and the field path into it (`h.f.g`).
struct PlacePath<'e> {
    root: &'e IdentExpr,
    fields: Vec<&'e str>,
}

struct MoveWalker<'a, 'c> {
    checker: &'c mut Checker<'a>,
    affine_params: Vec<String>,
    /// Each moved place: a binding and the field path moved out of it, empty
    /// for the binding as a whole.
    moved: IndexMap<(AstId, Vec<String>), Span>,
    pinned: IndexMap<AstId, Pinned>,
    /// Inside a closure body, the bindings it declares itself; every other
    /// local it names is a capture.
    closure_locals: Option<IndexSet<AstId>>,
    suppress: u32,
}

impl MoveWalker<'_, '_> {
    fn sem(&self) -> &Semantics {
        self.checker.sem
    }

    fn is_affine(&self, type_id: TypeId) -> bool {
        self.sem().is_affine(type_id, &self.affine_params)
    }

    fn expr_is_affine(&self, id: AstId) -> bool {
        self.sem()
            .expression_type(id)
            .is_some_and(|ty| self.is_affine(ty))
    }

    fn report(&mut self, kind: MoveError, span: Span) {
        if self.suppress == 0 {
            self.checker.push(kind, span);
        }
    }

    /// The moved place overlapping `def.path`, if any: the place itself, one
    /// inside it, or one holding it.
    fn moved_overlap(&self, def: AstId, path: &[&str]) -> Option<Span> {
        self.moved
            .iter()
            .find_map(|((moved_def, moved_path), span)| {
                let overlaps = *moved_def == def
                    && moved_path
                        .iter()
                        .zip(path)
                        .all(|(moved, read)| moved == read);
                overlaps.then_some(*span)
            })
    }

    fn place_path<'e>(expr: &'e Expr) -> Option<PlacePath<'e>> {
        match expr {
            Expr::Ident(root) => Some(PlacePath {
                root,
                fields: Vec::new(),
            }),
            Expr::FieldAccess(fa) => {
                let mut path = Self::place_path(&fa.expr)?;
                path.fields.push(&fa.field);
                Some(path)
            }
            _ => None,
        }
    }

    /// Whether `expr` is a reference, so a field read through it reads the
    /// referent.
    fn is_reference(&self, expr: &Expr) -> bool {
        self.sem().expression_type(expr.id()).is_some_and(|ty| {
            matches!(
                self.sem().types.get(ty),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        })
    }

    /// Whether a field path passes through a reference or a dereference.
    fn path_through_reference(&self, expr: &Expr) -> bool {
        match expr {
            Expr::FieldAccess(fa) => {
                matches!(&fa.expr, Expr::Unary(u) if u.op == ast::UnaryOp::Deref)
                    || self.is_reference(&fa.expr)
                    || self.path_through_reference(&fa.expr)
            }
            _ => false,
        }
    }

    fn read(&mut self, place: &PlacePath<'_>) {
        let Some(def) = self.sem().referenced_symbol(place.root.id) else {
            return;
        };
        if let Some(moved_at) = self.moved_overlap(def, &place.fields) {
            self.report(
                MoveError::UseAfterMove {
                    name: place.root.name.clone(),
                    moved_at,
                },
                place.root.span,
            );
        }
    }

    /// A by-value use of the place `expr` names, whose value is affine.
    fn consume(&mut self, expr: &Expr, place: &PlacePath<'_>) {
        let Some(def) = self.sem().referenced_symbol(place.root.id) else {
            return;
        };
        let name = place.root.name.clone();
        if matches!(
            self.sem().symbol_at(def).map(|s| &s.kind),
            Some(SymbolKind::Global(_))
        ) {
            self.report(MoveError::GlobalByValue { name }, place.root.span);
            return;
        }
        if self.path_through_reference(expr) {
            let type_name = self.type_name_of(expr);
            self.report(MoveError::MoveOutOfReference { type_name }, expr.span());
            return;
        }
        if let Some(Pinned::Guard) = self.pinned.get(&def) {
            self.report(MoveError::GuardMove { name }, place.root.span);
            return;
        }
        if self
            .closure_locals
            .as_ref()
            .is_some_and(|locals| !locals.contains(&def))
        {
            self.report(MoveError::CapturedMove { name }, place.root.span);
            return;
        }
        if let Some(moved_at) = self.moved_overlap(def, &place.fields) {
            self.report(MoveError::UseAfterMove { name, moved_at }, place.root.span);
            return;
        }
        let path = place.fields.iter().map(|f| (*f).to_string()).collect();
        self.moved.insert((def, path), place.root.span);
    }

    fn type_name_of(&self, expr: &Expr) -> String {
        let ty = self
            .sem()
            .expression_type(expr.id())
            .expect("an affine read has a type");
        self.sem().types.type_name(ty)
    }

    /// The bindings a place assignment gives new values: the place and every
    /// place inside it.
    fn clear_place(&mut self, place: &PlacePath<'_>) {
        let Some(def) = self.sem().referenced_symbol(place.root.id) else {
            return;
        };
        self.moved.retain(|(moved_def, moved_path), _| {
            *moved_def != def || !moved_path.starts_with_strs(&place.fields)
        });
    }

    fn visit_value(&mut self, expr: &Expr) {
        if let Some(place) = Self::place_path(expr) {
            if self.expr_is_affine(expr.id()) {
                self.consume(expr, &place);
            } else {
                self.read(&place);
            }
            return;
        }
        let out_of_reference = match expr {
            Expr::Unary(u) if u.op == ast::UnaryOp::Deref => Some(&u.expr),
            Expr::FieldAccess(fa) if self.path_through_reference(expr) => Some(&fa.expr),
            _ => None,
        };
        match out_of_reference {
            Some(base) if self.expr_is_affine(expr.id()) => {
                self.visit_expr(base);
                let type_name = self.type_name_of(expr);
                self.report(MoveError::MoveOutOfReference { type_name }, expr.span());
            }
            _ => self.visit_expr(expr),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Some(place) = Self::place_path(expr) {
            self.read(&place);
            return;
        }
        match expr {
            Expr::Ident(_) => unreachable!("a place path"),
            // A field of a temporary, which owns it.
            Expr::FieldAccess(fa) => self.visit_expr(&fa.expr),

            Expr::Call(call) => {
                self.visit_expr(&call.callee);
                for arg in &call.args {
                    self.visit_value(arg);
                }
            }
            Expr::MethodCall(mc) => {
                if self.sem().method_call_consumes_receiver(mc.id) {
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
            Expr::Index(ix) => {
                self.visit_expr(&ix.expr);
                self.visit_expr(&ix.index);
            }
            Expr::Cast(c) => self.visit_expr(&c.expr),
            Expr::TryOp(t) => self.visit_value(&t.expr),

            Expr::StructLiteral(s) => {
                for spread in &s.spreads {
                    self.visit_value(&spread.expr);
                }
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
                self.visit_if(
                    &if_expr.condition,
                    &if_expr.then_block,
                    if_expr.else_block.as_ref(),
                );
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
            Expr::Closure(closure) => self.visit_closure(closure),

            Expr::Literal(_) | Expr::Error(_) => {}
        }
    }

    /// A closure body is a body of its own, which captures by reference: it
    /// sees what the frame moved before it, and moves only its own locals.
    fn visit_closure(&mut self, closure: &ClosureExpr) {
        let locals: IndexSet<AstId> = closure.params.iter().map(|p| p.id).collect();
        let mut inner = MoveWalker {
            checker: &mut *self.checker,
            affine_params: self.affine_params.clone(),
            moved: self.moved.clone(),
            pinned: self.pinned.clone(),
            closure_locals: Some(locals),
            suppress: self.suppress,
        };
        inner.visit_value(&closure.body);
    }

    fn visit_assign(&mut self, a: &AssignExpr) {
        self.visit_value(&a.value);
        match Self::place_path(&a.target) {
            Some(place) => self.clear_place(&place),
            None => self.visit_expr(&a.target),
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
                let before = self.moved.clone();
                if let Some(value) = &l.value {
                    self.visit_value(value);
                }
                // A `let ... else` whose pattern fails consumes nothing; its
                // else block diverges, so the continuation keeps the state
                // the match left.
                if let Some(eb) = &l.else_block {
                    let matched = std::mem::replace(&mut self.moved, before);
                    self.visit_block(eb);
                    self.moved = matched;
                }
                self.bind_pattern(&l.pattern);
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
            Stmt::If(s) => self.visit_if(&s.condition, &s.then_block, s.else_block.as_ref()),
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
                self.checker.check_item(item);
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
            LoopHead::Binding(pattern) => self.bind_pattern(pattern),
        }
        self.visit_block(body);
        if let Some(update) = update {
            self.visit_expr(update);
        }
    }

    /// The bindings `pattern` makes hold new values, none of them moved.
    fn bind_pattern(&mut self, pattern: &Pattern) {
        for_each_pattern_binding(pattern, &mut |id| {
            self.moved.retain(|(def, _), _| *def != id);
            if let Some(locals) = &mut self.closure_locals {
                locals.insert(id);
            }
        });
    }

    /// An `if` whose `let`s consume their scrutinees on the matched path only:
    /// a refutable pattern that fails consumes nothing, so the `else` still
    /// holds them.
    fn visit_if(
        &mut self,
        condition: &Condition,
        then_block: &Block,
        else_block: Option<&Block>,
    ) -> bool {
        let before = self.moved.clone();
        self.suppress += 1;
        self.visit_condition_reading(condition);
        self.suppress -= 1;
        let unmatched = std::mem::replace(&mut self.moved, before);
        self.visit_condition(condition);
        let then_div = self.visit_block(then_block);
        let then_moved = std::mem::replace(&mut self.moved, unmatched);
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
        self.visit_value(scrutinee);
        let base = std::mem::take(&mut self.moved);
        let mut merged = base.clone();
        let mut all_diverge = true;
        for arm in arms {
            self.moved.clone_from(&base);
            self.bind_pattern(&arm.pattern);
            if let Some(guard) = &arm.guard {
                let saved = self.pinned.clone();
                for_each_pattern_binding(&arm.pattern, &mut |id| {
                    self.pinned.insert(id, Pinned::Guard);
                });
                self.visit_expr(guard);
                self.pinned = saved;
            }
            let diverged = self.visit_expr_diverges(&arm.body);
            if !diverged {
                all_diverge = false;
                let arm_moved = std::mem::take(&mut self.moved);
                for (place, span) in arm_moved {
                    merged.entry(place).or_insert(span);
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
                self.visit_value(other);
                false
            }
        }
    }

    /// A condition on the path where its `let`s matched: each consumes its
    /// scrutinee and binds its pattern afresh.
    fn visit_condition(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    match element {
                        ConditionElement::Let { pattern, expr, .. } => {
                            self.visit_value(expr);
                            self.bind_pattern(pattern);
                        }
                        ConditionElement::Expr(e) => self.visit_expr(e),
                    }
                }
            }
        }
    }

    /// A condition on the path where a `let` failed: it consumed nothing.
    fn visit_condition_reading(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    self.visit_expr(element.expr());
                }
            }
        }
    }

    fn union_into(&mut self, other: IndexMap<(AstId, Vec<String>), Span>) {
        for (place, span) in other {
            self.moved.entry(place).or_insert(span);
        }
    }
}

/// Whether a field path starts with `prefix`.
trait StartsWithStrs {
    fn starts_with_strs(&self, prefix: &[&str]) -> bool;
}

impl StartsWithStrs for Vec<String> {
    fn starts_with_strs(&self, prefix: &[&str]) -> bool {
        self.len() >= prefix.len() && self.iter().zip(prefix).all(|(a, b)| a == b)
    }
}
