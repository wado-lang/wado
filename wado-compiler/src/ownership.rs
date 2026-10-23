//! Whether each expression a body evaluates takes ownership of its value
//! (WEP: Affine Types). The move check and reify both read [`body_modes`], so
//! the static rule and the drop elaboration it feeds agree on every place
//! that moves.

use crate::ast::{
    AstId, Block, Condition, ConditionElement, Expr, IdentExpr, Literal, MatchArm, Stmt, UnaryOp,
};
use crate::hashmap::IndexMap;
use crate::symbol::{Symbol, SymbolKind};

/// Whether `symbol` is a variable, a local or a global: what
/// [`BodyFacts::names_variable`] asks of the symbol an identifier names.
pub(crate) fn is_variable(symbol: &Symbol) -> bool {
    matches!(symbol.kind, SymbolKind::Variable(_) | SymbolKind::Global(_))
}

/// What the modes of a body read from its types.
pub(crate) trait BodyFacts {
    /// Whether the method call `call` takes its receiver by value.
    fn consumes_receiver(&self, call: AstId) -> bool;
    /// Whether `base[i]` subscripts a tuple, whose `t[0]` is its field `t.0`.
    fn indexes_tuple(&self, base: &Expr) -> bool;
    /// Whether the value of `expr` is move-only.
    fn is_affine(&self, expr: AstId) -> bool;
    /// Whether `ident` names a variable, a local or a global, rather than a
    /// function or a case.
    fn names_variable(&self, ident: &IdentExpr) -> bool;
}

/// A place a body reads: a binding and the field path into it (`h.f.g`).
pub(crate) struct PlacePath<'e> {
    pub(crate) root: &'e IdentExpr,
    pub(crate) fields: Vec<&'e str>,
}

impl PlacePath<'_> {
    pub(crate) fn owned_fields(&self) -> Vec<String> {
        self.fields.iter().map(|f| (*f).to_string()).collect()
    }
}

/// The place `expr` names, if it names one.
pub(crate) fn place_path<'e>(expr: &'e Expr, facts: &impl BodyFacts) -> Option<PlacePath<'e>> {
    if let Expr::Ident(root) = expr {
        return facts.names_variable(root).then(|| PlacePath {
            root,
            fields: Vec::new(),
        });
    }
    let (base, field) = field_step(expr, facts)?;
    let mut path = place_path(base, facts)?;
    path.fields.push(field);
    Some(path)
}

/// The aggregate `expr` reads a field of, and the field: `h.f`, or a tuple's
/// `t[0]`, which is its field `t.0`.
pub(crate) fn field_step<'e>(
    expr: &'e Expr,
    facts: &impl BodyFacts,
) -> Option<(&'e Expr, &'e str)> {
    match expr {
        Expr::FieldAccess(fa) => Some((&fa.expr, &fa.field)),
        Expr::Index(ix) if facts.indexes_tuple(&ix.expr) => {
            let Expr::Literal(lit) = &ix.index else {
                return None;
            };
            let Literal::Number(digits) = &lit.value else {
                return None;
            };
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // `t[00]` is `t.0` too.
            let unpadded = digits.trim_start_matches('0');
            let field = if unpadded.is_empty() {
                &digits[digits.len() - 1..]
            } else {
                unpadded
            };
            Some((&ix.expr, field))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperandMode {
    /// The position takes ownership: a `let`, an argument, an aggregate's
    /// element.
    Consume,
    /// The position references the value: `&x`, an operator's operand, a
    /// template hole.
    Borrow,
}

/// What an operand's position is, before its context is known.
#[derive(Clone, Copy)]
enum Position {
    Is(OperandMode),
    /// What the enclosing position is: a cast and a spread convert for free,
    /// so they move where their result moves.
    Inherit,
    /// A field's base: taking an affine field takes the aggregate holding it,
    /// and reading any other field copies it out.
    Projection,
    /// What the call's dispatch decides: a by-value `self` consumes.
    Receiver,
}

/// The mode of every expression a body evaluates. A place is evaluated whole,
/// so what it projects from has no mode.
pub(crate) struct Modes {
    modes: IndexMap<AstId, OperandMode>,
}

impl Modes {
    /// The mode of `expr`, which the body these modes were taken of
    /// evaluates.
    pub(crate) fn of(&self, expr: &Expr) -> OperandMode {
        *self
            .modes
            .get(&expr.id())
            .expect("the body walk gives every expression a mode")
    }

    /// Whether `expr` is a place the body moves its affine value out of. The
    /// move check moves exactly these, and reify marks exactly these for drop
    /// elaboration to take. `expr` may be one the body does not evaluate on
    /// its own: a projection's base, or no expression of this body.
    pub(crate) fn moves(&self, expr: &Expr, facts: &impl BodyFacts) -> bool {
        place_path(expr, facts).is_some() && self.takes(expr, facts)
    }

    /// Whether the body takes ownership of the affine value of `expr`, which
    /// a place holds or not.
    pub(crate) fn takes(&self, expr: &Expr, facts: &impl BodyFacts) -> bool {
        self.modes.get(&expr.id()) == Some(&OperandMode::Consume) && facts.is_affine(expr.id())
    }
}

/// The modes of `body`, a function's or a test's, closures included. An item
/// declared inside it has a body of its own.
pub(crate) fn body_modes(body: &Block, facts: &impl BodyFacts) -> Modes {
    let mut walk = ModeWalk {
        facts,
        modes: IndexMap::default(),
    };
    walk.block(body);
    Modes { modes: walk.modes }
}

/// The modes of `expr`, evaluated where no body walk reaches it: a default, a
/// global's initializer, an associated constant.
pub(crate) fn expr_modes(expr: &Expr, facts: &impl BodyFacts) -> Modes {
    let mut walk = ModeWalk {
        facts,
        modes: IndexMap::default(),
    };
    walk.expr(expr, OperandMode::Consume);
    Modes { modes: walk.modes }
}

struct ModeWalk<'f, F> {
    facts: &'f F,
    modes: IndexMap<AstId, OperandMode>,
}

impl<F: BodyFacts> ModeWalk<'_, F> {
    fn block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        use OperandMode::{Borrow, Consume};
        match stmt {
            Stmt::Let(l) => {
                if let Some(value) = &l.value {
                    self.expr(value, Consume);
                }
                if let Some(eb) = &l.else_block {
                    self.block(eb);
                }
            }
            Stmt::Expr(e) => self.expr(&e.expr, Consume),
            Stmt::Return(r) => {
                if let Some(value) = &r.value {
                    self.expr(value, Consume);
                }
            }
            Stmt::TaskReturn(t) => self.expr(&t.value, Consume),
            Stmt::Break(b) => {
                if let Some(value) = &b.value {
                    self.expr(value, Consume);
                }
            }
            Stmt::If(s) => self.branches(&s.condition, &s.then_block, s.else_block.as_ref()),
            Stmt::While(s) => {
                self.condition(&s.condition);
                self.block(&s.body);
            }
            Stmt::Loop(s) => self.block(&s.body),
            Stmt::For(s) => {
                if let Some(init) = &s.init {
                    self.stmt(init);
                }
                if let Some(condition) = &s.condition {
                    self.condition(condition);
                }
                if let Some(update) = &s.update {
                    self.expr(update, Borrow);
                }
                self.block(&s.body);
            }
            Stmt::ForOf(s) => {
                self.expr(&s.iterable, Consume);
                self.block(&s.body);
            }
            Stmt::Match(m) => self.match_arms(&m.expr, &m.arms),
            Stmt::Assert(a) => {
                self.expr(&a.condition, Borrow);
                if let Some(message) = &a.message {
                    self.expr(message, Borrow);
                }
            }
            Stmt::LabeledBlock(lb) => self.block(&lb.block),
            Stmt::Continue(_) | Stmt::Item(_) | Stmt::Error(_) => {}
        }
    }

    /// A `let` in a condition consumes its scrutinee on the path it matches.
    fn condition(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.expr(e, OperandMode::Borrow),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    match element {
                        ConditionElement::Let { expr, .. } => self.expr(expr, OperandMode::Consume),
                        ConditionElement::Expr(e) => self.expr(e, OperandMode::Borrow),
                    }
                }
            }
        }
    }

    fn branches(&mut self, condition: &Condition, then_block: &Block, else_block: Option<&Block>) {
        self.condition(condition);
        self.block(then_block);
        if let Some(eb) = else_block {
            self.block(eb);
        }
    }

    /// A scrutinee is consumed as an arm is entered, and an arm's body is the
    /// match's value. A guard sees the arm's bindings by reference.
    fn match_arms(&mut self, scrutinee: &Expr, arms: &[MatchArm]) {
        self.expr(scrutinee, OperandMode::Consume);
        for arm in arms {
            if let Some(guard) = &arm.guard {
                self.expr(guard, OperandMode::Borrow);
            }
            self.expr(&arm.body, OperandMode::Consume);
        }
    }

    fn expr(&mut self, expr: &Expr, mode: OperandMode) {
        self.modes.insert(expr.id(), mode);
        if place_path(expr, self.facts).is_some() {
            return;
        }
        match expr {
            Expr::Block(block) => self.block(block),
            Expr::LabeledBlock(lb) => self.block(&lb.block),
            Expr::If(if_expr) => self.branches(
                &if_expr.condition,
                &if_expr.then_block,
                if_expr.else_block.as_ref(),
            ),
            Expr::Match(m) => self.match_arms(&m.expr, &m.arms),
            Expr::Closure(closure) => self.expr(&closure.body, OperandMode::Consume),
            Expr::WithHandler(w) => {
                self.operands(expr, mode);
                self.block(&w.body);
            }
            Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticMethodCall(_)
            | Expr::Unary(_)
            | Expr::Binary(_)
            | Expr::CompoundAssign(_)
            | Expr::Assign(_)
            | Expr::ComparisonChain(_)
            | Expr::Index(_)
            | Expr::FieldAccess(_)
            | Expr::Cast(_)
            | Expr::Spread(..)
            | Expr::TryOp(_)
            | Expr::StructLiteral(_)
            | Expr::TupleLiteral(_)
            | Expr::TupleComprehension(_)
            | Expr::TemplateString(_)
            | Expr::TaggedTemplate(_)
            | Expr::Range(_)
            | Expr::Matches(_)
            | Expr::Ident(_)
            | Expr::Literal(_)
            | Expr::Error(_) => self.operands(expr, mode),
        }
    }

    fn operands(&mut self, expr: &Expr, mode: OperandMode) {
        for (operand, position) in positions(expr) {
            let operand_mode = match position {
                Position::Is(operand_mode) => operand_mode,
                Position::Inherit => mode,
                Position::Projection
                    if mode == OperandMode::Consume && self.facts.is_affine(expr.id()) =>
                {
                    OperandMode::Consume
                }
                Position::Projection => OperandMode::Borrow,
                Position::Receiver if self.facts.consumes_receiver(expr.id()) => {
                    OperandMode::Consume
                }
                Position::Receiver => OperandMode::Borrow,
            };
            self.expr(operand, operand_mode);
        }
    }
}

/// The operands of `expr`, in evaluation order. A construct whose children are
/// bodies rather than operands (a block, a branch, a match arm, a closure)
/// has none: its walk is control flow.
pub(crate) fn operands(expr: &Expr) -> impl Iterator<Item = &Expr> {
    positions(expr).into_iter().map(|(operand, _)| operand)
}

fn positions(expr: &Expr) -> Vec<(&Expr, Position)> {
    use OperandMode::{Borrow, Consume};
    let consume = Position::Is(Consume);
    let borrow = Position::Is(Borrow);
    match expr {
        Expr::Call(call) => std::iter::once((&call.callee, borrow))
            .chain(call.args.iter().map(|arg| (arg, consume)))
            .collect(),
        Expr::MethodCall(mc) => std::iter::once((&mc.receiver, Position::Receiver))
            .chain(mc.args.iter().map(|arg| (arg, consume)))
            .collect(),
        Expr::StaticMethodCall(smc) => smc.args.iter().map(|arg| (arg, consume)).collect(),
        // `unchecked` marks the call it wraps, and is that call's position.
        Expr::Unary(u) if u.op == UnaryOp::Unchecked => vec![(&u.expr, Position::Inherit)],
        Expr::Unary(u) => vec![(&u.expr, borrow)],
        Expr::Binary(b) => vec![(&b.left, borrow), (&b.right, borrow)],
        Expr::CompoundAssign(a) => vec![(&a.target, borrow), (&a.value, borrow)],
        Expr::Assign(a) if a.destructuring_targets().is_some() => std::iter::once((&a.value, consume))
            .chain(a.places().iter().map(|place| (place, borrow)))
            .collect(),
        Expr::Assign(a) => vec![(&a.target, borrow), (&a.value, consume)],
        Expr::ComparisonChain(c) => std::iter::once((&c.first, borrow))
            .chain(c.comparisons.iter().map(|cmp| (&cmp.right, borrow)))
            .collect(),
        Expr::Index(ix) => vec![(&ix.expr, borrow), (&ix.index, borrow)],
        Expr::FieldAccess(fa) => vec![(&fa.expr, Position::Projection)],
        Expr::Cast(c) => vec![(&c.expr, Position::Inherit)],
        Expr::Spread(inner, _) => vec![(&**inner, Position::Inherit)],
        Expr::TryOp(t) => vec![(&t.expr, consume)],
        Expr::StructLiteral(s) => s
            .spreads
            .iter()
            .map(|spread| (&*spread.expr, consume))
            .chain(s.fields.iter().map(|field| (&field.value, consume)))
            .collect(),
        Expr::TupleLiteral(t) => t.elements.iter().map(|el| (el, consume)).collect(),
        Expr::TupleComprehension(c) => vec![(&c.iterable, consume), (&c.body, consume)],
        Expr::TemplateString(ts) => ts.interpolations().map(|hole| (hole, borrow)).collect(),
        Expr::TaggedTemplate(t) => t
            .template
            .interpolations()
            .map(|hole| (hole, borrow))
            .collect(),
        Expr::Range(r) => vec![(&r.start, borrow), (&r.end, borrow)],
        Expr::Matches(m) => std::iter::once(&m.expr)
            .chain(&m.guard)
            .map(|operand| (operand, borrow))
            .collect(),
        Expr::WithHandler(w) => w.handlers.iter().map(|h| (&h.handler, consume)).collect(),
        Expr::Ident(_)
        | Expr::Literal(_)
        | Expr::Block(_)
        | Expr::LabeledBlock(_)
        | Expr::If(_)
        | Expr::Match(_)
        | Expr::Closure(_)
        | Expr::Error(_) => Vec::new(),
    }
}
