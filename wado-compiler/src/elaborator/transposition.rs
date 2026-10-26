//! What the syntax of a destructuring assignment says of it (WEP: Destructuring
//! Assignment), and which may be a transposition within one container. Annotate
//! settles the rest: that the container is a local, that no reference it is
//! reached through can be rebound, that no index reads a mutable global, and
//! that the type implements `IndexSwap`.

use crate::ast::{
    AssignExpr, AssignTarget, AstId, AstVisitor, BinaryOp, Expr, FieldAccessExpr, Function,
    IdentExpr, Item, Literal, Module, Pattern, UnaryOp, walk_expr, walk_function, walk_pattern,
};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::token::Span;

pub(super) enum Reading<'a> {
    /// The right side is not the targets, reordered.
    NotAPermutation,
    Transposition(Transposition<'a>),
    /// Two subscripts exchanged, of containers not written as one path.
    OtherContainers,
    /// Two subscripts of one container exchanged, an index of which is not an
    /// operand [`Transposition`] admits.
    UnplainIndex,
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
        if !same(&first.expr, &second.expr) || path(&first.expr, Derefs::Admitted).is_none() {
            return Self::OtherContainers;
        }
        let transposition = Transposition {
            container: &first.expr,
            i: &first.index,
            j: &second.index,
            vouched: value
                .elements
                .iter()
                .all(|read| matches!(read, Expr::Index(read) if read.unchecked.is_some())),
        };
        // An index read out of the container could change with the first write.
        let root = transposition.container_root();
        if !is_index_operand(transposition.i)
            || !is_index_operand(transposition.j)
            || transposition.index_locals().contains(&root)
        {
            return Self::UnplainIndex;
        }
        Self::Transposition(transposition)
    }
}

/// The spans of the right side's elements that spell one of the targets.
pub(super) fn target_reads(assign: &AssignExpr) -> Vec<Span> {
    let (AssignTarget::Destructuring { places, .. }, Expr::TupleLiteral(value)) =
        (&assign.target, &assign.value)
    else {
        return Vec::new();
    };
    value
        .elements
        .iter()
        .filter(|element| places.iter().any(|place| same(place, element)))
        .map(Expr::span)
        .collect()
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
    /// Whether both reads are `unchecked`: `index_swap`'s contract is the
    /// conjunction of theirs.
    pub(super) vouched: bool,
}

impl<'a> Transposition<'a> {
    pub(super) fn of(assign: &'a AssignExpr) -> Option<Self> {
        match Reading::of(assign) {
            Reading::Transposition(transposition) => Some(transposition),
            Reading::NotAPermutation
            | Reading::OtherContainers
            | Reading::UnplainIndex
            | Reading::Permutation => None,
        }
    }

    pub(super) fn container_path(&self) -> Path<'a> {
        path(self.container, Derefs::Admitted).expect("`Reading::of` admits a container path")
    }

    /// The local the container is reached from.
    pub(super) fn container_root(&self) -> &'a str {
        &self.container_path().root.name
    }

    /// The names the indices read, each path by its root.
    pub(super) fn index_reads(&self) -> Vec<&'a IdentExpr> {
        let mut reads = Vec::new();
        for index in [self.i, self.j] {
            collect_idents(index, &mut reads);
        }
        reads
    }

    fn index_locals(&self) -> Vec<&'a str> {
        let reads = self.index_reads().into_iter();
        reads.map(|read| read.name.as_str()).collect()
    }
}

/// What [`transpositions`] knows of one transposition whose index locals no
/// reference or capture reaches.
#[derive(Clone, Copy)]
pub(super) struct Clean {
    /// Whether the local the container is reached from is referenced or
    /// captured. Where that local is a `mut` reference, rebinding it changes
    /// which container the second target names, which only its type tells.
    pub(super) container_root_referenced: bool,
}

/// The destructuring assignments that [`Transposition::of`] reads, where no
/// index local is referenced or captured anywhere in the function holding it:
/// in every item of a module whose bodies are walked, and in every trait,
/// since a default body is walked in each module that takes it.
pub(super) fn transpositions(
    modules: &IndexMap<ModuleSource, Module>,
    walks_bodies_of: impl Fn(&ModuleSource) -> bool,
) -> IndexMap<AstId, Clean> {
    let mut scan = Scan::default();
    for (source, module) in modules {
        let walked = walks_bodies_of(source);
        for item in &module.items {
            if walked || matches!(item, Item::Trait(_)) {
                scan.visit_item(item);
                scan.settle();
            }
        }
    }
    scan.found
}

#[derive(Default)]
struct Scan<'ast> {
    /// Names something takes a reference to, a method may take `&self` of, or
    /// a closure captures, in the function being walked. Keyed by name, so a
    /// shadowed binding counts too.
    referenced: IndexSet<&'ast str>,
    /// The names each closure being walked binds itself, its parameters and
    /// its patterns, innermost last.
    closure_bindings: Vec<IndexSet<&'ast str>>,
    /// Each transposition in the function being walked, with its index locals
    /// and its container root.
    candidates: Vec<(AstId, Vec<&'ast str>, &'ast str)>,
    found: IndexMap<AstId, Clean>,
}

impl Scan<'_> {
    /// Move the candidates the walk so far leaves clean into `found`.
    fn settle(&mut self) {
        for (id, index_locals, root) in std::mem::take(&mut self.candidates) {
            if index_locals.iter().all(|l| !self.referenced.contains(l)) {
                let container_root_referenced = self.referenced.contains(root);
                self.found.insert(
                    id,
                    Clean {
                        container_root_referenced,
                    },
                );
            }
        }
        self.referenced.clear();
    }
}

impl<'ast> AstVisitor<'ast> for Scan<'ast> {
    fn visit_function(&mut self, func: &'ast Function) {
        self.settle();
        walk_function(self, func);
        self.settle();
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Unary(unary) if matches!(unary.op, UnaryOp::Ref | UnaryOp::MutRef) => {
                self.referenced.extend(root_local(&unary.expr));
            }
            Expr::MethodCall(call) => self.referenced.extend(root_local(&call.receiver)),
            Expr::Ident(ident) => {
                if ident.segments.is_empty()
                    && self
                        .closure_bindings
                        .last()
                        .is_some_and(|bound| !bound.contains(ident.name.as_str()))
                {
                    self.referenced.insert(&ident.name);
                }
            }
            Expr::Closure(closure) => {
                self.closure_bindings
                    .push(closure.params.iter().map(|p| p.name.as_str()).collect());
                walk_expr(self, expr);
                self.closure_bindings.pop();
                return;
            }
            Expr::Assign(assign) => {
                if matches!(assign.target, AssignTarget::Destructuring { .. })
                    && let Some(transposition) = Transposition::of(assign)
                {
                    self.candidates.push((
                        assign.id,
                        transposition.index_locals(),
                        transposition.container_root(),
                    ));
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }

    fn visit_pattern(&mut self, pat: &'ast Pattern) {
        if let Some(bound) = self.closure_bindings.last_mut()
            && let Pattern::Ident { name, .. } | Pattern::MutIdent { name, .. } = pat
        {
            bound.insert(name);
        }
        walk_pattern(self, pat);
    }
}

/// Whether `a` and `b` spell one place or index: a name, a literal, arithmetic,
/// a field, a dereference or a subscript of them. Anything else spells none.
fn same(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Ident(a), Expr::Ident(b)) => {
            a.name == b.name && a.type_args.is_empty() && b.type_args.is_empty()
        }
        (Expr::Literal(a), Expr::Literal(b)) => same_literal(&a.value, &b.value),
        (Expr::Binary(a), Expr::Binary(b)) => {
            a.op == b.op && same(&a.left, &b.left) && same(&a.right, &b.right)
        }
        (Expr::Unary(a), Expr::Unary(b)) => a.op == b.op && same(&a.expr, &b.expr),
        (Expr::FieldAccess(a), Expr::FieldAccess(b)) => {
            a.field == b.field && same(&a.expr, &b.expr)
        }
        // `unchecked` vouches for a read; the place it reads is the same.
        (Expr::Index(a), Expr::Index(b)) => same(&a.expr, &b.expr) && same(&a.index, &b.index),
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

/// A local and the fields and dereferences that reach a place from it.
pub(super) struct Path<'a> {
    pub(super) root: &'a IdentExpr,
    /// Outward from the root.
    pub(super) steps: Vec<Step<'a>>,
}

pub(super) enum Step<'a> {
    Field(&'a FieldAccessExpr),
    Deref,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Derefs {
    Admitted,
    Refused,
}

/// `expr` as a [`Path`], or `None` where it is none: an index is a path of
/// fields alone, a container may pass through dereferences too.
fn path(expr: &Expr, derefs: Derefs) -> Option<Path<'_>> {
    match expr {
        Expr::Ident(root) if root.segments.is_empty() => Some(Path {
            root,
            steps: Vec::new(),
        }),
        Expr::FieldAccess(field) => {
            let mut path = path(&field.expr, derefs)?;
            path.steps.push(Step::Field(field));
            Some(path)
        }
        Expr::Unary(unary) if unary.op == UnaryOp::Deref && derefs == Derefs::Admitted => {
            let mut path = path(&unary.expr, derefs)?;
            path.steps.push(Step::Deref);
            Some(path)
        }
        _ => None,
    }
}

/// `expr` as a local and the fields that reach a place from it.
pub(super) fn field_path(expr: &Expr) -> Option<Path<'_>> {
    path(expr, Derefs::Refused)
}

/// A literal, a name, a path of fields from a local, or arithmetic over them.
fn is_index_operand(expr: &Expr) -> bool {
    match expr {
        Expr::FieldAccess(_) => field_path(expr).is_some(),
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

/// The names an index operand reads, each path by its root.
fn collect_idents<'a>(expr: &'a Expr, out: &mut Vec<&'a IdentExpr>) {
    match expr {
        Expr::Ident(_) | Expr::FieldAccess(_) => {
            out.extend(field_path(expr).map(|path| path.root));
        }
        Expr::Binary(binary) => {
            collect_idents(&binary.left, out);
            collect_idents(&binary.right, out);
        }
        Expr::Unary(unary) => collect_idents(&unary.expr, out),
        _ => {}
    }
}

/// The local a place expression starts from.
fn root_local(place: &Expr) -> Option<&str> {
    match place.place_root() {
        Expr::Ident(ident) if ident.segments.is_empty() => Some(&ident.name),
        _ => None,
    }
}
