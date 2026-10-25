//! What the syntax of a destructuring assignment says of it (WEP: Destructuring
//! Assignment), and which may be a transposition within one container. Annotate
//! settles the rest: that the container is a local, that no index reads a
//! mutable global, and that the type implements `IndexSwap`.

use crate::ast::{
    AssignExpr, AssignTarget, AstId, AstVisitor, BinaryOp, Expr, IndexExpr, Literal, Module,
    UnaryOp, walk_expr,
};
use crate::hashmap::IndexSet;

pub(super) enum Reading<'a> {
    /// The right side is not the targets, reordered.
    NotAPermutation,
    Transposition(Transposition<'a>),
    /// Two subscripts exchanged, of containers not written as one path.
    OtherContainers,
    /// Any other reordering of the targets.
    Permutation,
}

impl<'a> Reading<'a> {
    pub(super) fn of(assign: &'a AssignExpr) -> Self {
        let AssignTarget::Destructuring { places, .. } = &assign.target else {
            unreachable!("only a destructuring assignment is read");
        };
        let Expr::TupleLiteral(value) = &assign.value else {
            return Self::NotAPermutation;
        };
        if !is_permutation(places, &value.elements) {
            return Self::NotAPermutation;
        }
        let ([Expr::Index(first), Expr::Index(second)], [read_second, _]) =
            (places.as_slice(), value.elements.as_slice())
        else {
            return Self::Permutation;
        };
        if !same(read_second, &places[1]) {
            return Self::Permutation;
        }
        if !same(&first.expr, &second.expr) || !is_container_path(&first.expr) {
            return Self::OtherContainers;
        }
        if !is_index_operand(&first.index) || !is_index_operand(&second.index) {
            return Self::Permutation;
        }
        Self::Transposition(Transposition {
            container: &first.expr,
            i: &first.index,
            j: &second.index,
        })
    }
}

/// Whether `values` is `places`, reordered.
fn is_permutation(places: &[Expr], values: &[Expr]) -> bool {
    let mut unmatched: Vec<&Expr> = places.iter().collect();
    values.len() == places.len()
        && values.iter().all(|value| {
            let found = unmatched.iter().position(|place| same(place, value));
            found.map(|at| unmatched.swap_remove(at)).is_some()
        })
}

/// `[c[i], c[j]] = [c[j], c[i]]`, read as `c.index_swap(i, j)`.
pub(super) struct Transposition<'a> {
    pub(super) container: &'a Expr,
    pub(super) i: &'a Expr,
    pub(super) j: &'a Expr,
}

impl<'a> Transposition<'a> {
    pub(super) fn of(assign: &'a AssignExpr) -> Option<Self> {
        match Reading::of(assign) {
            Reading::Transposition(transposition) => Some(transposition),
            Reading::NotAPermutation | Reading::OtherContainers | Reading::Permutation => None,
        }
    }

    /// The local the container is reached from.
    pub(super) fn container_root(&self) -> &'a str {
        root_local(self.container).expect("a container path starts from a local")
    }

    /// The locals a reference or a capture could rebind between the
    /// desugaring's two writes: each index's, and the one the container is
    /// reached through where a dereference reaches it.
    fn rebindable_locals(&self) -> Vec<&'a str> {
        let mut locals = Vec::new();
        for index in [self.i, self.j] {
            collect_idents(index, &mut locals);
        }
        if derefs(self.container) {
            locals.push(self.container_root());
        }
        locals
    }
}

/// The destructuring assignments in `module` that [`Transposition::of`]
/// reads, where no local in [`Transposition::rebindable_locals`] is referenced
/// or captured anywhere in the item holding it.
pub(super) fn transpositions(module: &Module) -> IndexSet<AstId> {
    let mut found = IndexSet::default();
    for item in &module.items {
        let mut scan = Scan::default();
        scan.visit_item(item);
        found.extend(
            scan.candidates
                .into_iter()
                .filter(|(_, locals)| locals.iter().all(|l| !scan.referenced.contains(l)))
                .map(|(id, _)| id),
        );
    }
    found
}

#[derive(Default)]
struct Scan<'ast> {
    /// Names something takes a reference to, a method may take `&self` of, or
    /// a closure captures. Keyed by name, so a shadowed binding counts too.
    referenced: IndexSet<&'ast str>,
    /// The parameters of each closure being walked, innermost last.
    closure_params: Vec<IndexSet<&'ast str>>,
    candidates: Vec<(AstId, Vec<&'ast str>)>,
}

impl<'ast> AstVisitor<'ast> for Scan<'ast> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Unary(unary) if matches!(unary.op, UnaryOp::Ref | UnaryOp::MutRef) => {
                self.referenced.extend(root_local(&unary.expr));
            }
            Expr::MethodCall(call) => self.referenced.extend(root_local(&call.receiver)),
            Expr::Ident(ident) => {
                if ident.segments.is_empty()
                    && self
                        .closure_params
                        .last()
                        .is_some_and(|params| !params.contains(ident.name.as_str()))
                {
                    self.referenced.insert(&ident.name);
                }
            }
            Expr::Closure(closure) => {
                self.closure_params
                    .push(closure.params.iter().map(|p| p.name.as_str()).collect());
                walk_expr(self, expr);
                self.closure_params.pop();
                return;
            }
            Expr::Assign(assign) => {
                if matches!(assign.target, AssignTarget::Destructuring { .. })
                    && let Some(transposition) = Transposition::of(assign)
                {
                    self.candidates
                        .push((assign.id, transposition.rebindable_locals()));
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

fn same_subscript(a: &IndexExpr, b: &IndexExpr) -> bool {
    a.unchecked.is_none()
        && b.unchecked.is_none()
        && same(&a.expr, &b.expr)
        && same(&a.index, &b.index)
}

/// Whether `a` and `b` spell one place or index: a name, a literal, arithmetic,
/// a field, a dereference or a subscript of them. Anything else spells none.
fn same(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Ident(a), Expr::Ident(b)) => a.name == b.name && a.type_args.is_empty() && b.type_args.is_empty(),
        (Expr::Literal(a), Expr::Literal(b)) => same_literal(&a.value, &b.value),
        (Expr::Binary(a), Expr::Binary(b)) => {
            a.op == b.op && same(&a.left, &b.left) && same(&a.right, &b.right)
        }
        (Expr::Unary(a), Expr::Unary(b)) => a.op == b.op && same(&a.expr, &b.expr),
        (Expr::FieldAccess(a), Expr::FieldAccess(b)) => {
            a.field == b.field && same(&a.expr, &b.expr)
        }
        (Expr::Index(a), Expr::Index(b)) => same_subscript(a, b),
        _ => false,
    }
}

fn same_literal(a: &Literal, b: &Literal) -> bool {
    match (a, b) {
        (Literal::Number(a), Literal::Number(b))
        | (Literal::String(a), Literal::String(b))
        | (Literal::Byte(a), Literal::Byte(b))
        | (Literal::Char(a), Literal::Char(b)) => a == b,
        (Literal::Bool(a), Literal::Bool(b)) => a == b,
        _ => false,
    }
}

/// A local, a field path from one, or a dereference of either.
fn is_container_path(expr: &Expr) -> bool {
    match expr {
        Expr::Ident(ident) => ident.segments.is_empty(),
        Expr::FieldAccess(field) => is_container_path(&field.expr),
        Expr::Unary(unary) => unary.op == UnaryOp::Deref && is_container_path(&unary.expr),
        _ => false,
    }
}

/// A literal, a name, or arithmetic over them.
fn is_index_operand(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(literal) => matches!(
            literal.value,
            Literal::Number(_)
                | Literal::String(_)
                | Literal::Byte(_)
                | Literal::Char(_)
                | Literal::Bool(_)
        ),
        Expr::Ident(ident) => ident.type_args.is_empty(),
        Expr::Binary(binary) => {
            matches!(
                binary.op,
                BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Mod
                    | BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor
                    | BinaryOp::Shl
                    | BinaryOp::Shr
            ) && is_index_operand(&binary.left)
                && is_index_operand(&binary.right)
        }
        Expr::Unary(unary) => {
            matches!(unary.op, UnaryOp::Neg | UnaryOp::BitNot) && is_index_operand(&unary.expr)
        }
        _ => false,
    }
}

fn collect_idents<'a>(expr: &'a Expr, out: &mut Vec<&'a str>) {
    match expr {
        Expr::Ident(ident) if ident.segments.is_empty() => out.push(&ident.name),
        Expr::Binary(binary) => {
            collect_idents(&binary.left, out);
            collect_idents(&binary.right, out);
        }
        Expr::Unary(unary) => collect_idents(&unary.expr, out),
        _ => {}
    }
}

fn derefs(path: &Expr) -> bool {
    match path {
        Expr::FieldAccess(field) => derefs(&field.expr),
        Expr::Unary(_) => true,
        _ => false,
    }
}

/// The local a place expression starts from.
fn root_local(place: &Expr) -> Option<&str> {
    match place {
        Expr::Ident(ident) if ident.segments.is_empty() => Some(&ident.name),
        Expr::FieldAccess(field) => root_local(&field.expr),
        Expr::Index(index) => root_local(&index.expr),
        Expr::Unary(unary) if unary.op == UnaryOp::Deref => root_local(&unary.expr),
        _ => None,
    }
}
