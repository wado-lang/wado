//! What a borrow or lvalue chain names.
//!
//! One walk answers it. [`named_place`] resolves an operand to the local it
//! roots at and the steps between, and every question here is a predicate over
//! those steps — so a node kind the IR gains is taught to this walk alone,
//! rather than to each caller's own idea of what a chain may contain.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprId, ExprKind, Operand};

/// One layer between a local and the storage an operand names. A cast is not a
/// step: it converts no storage, so it names exactly its operand, which is how
/// a borrow reaches a builtin after monomorphization.
#[derive(Clone, Copy)]
enum Step {
    /// `&x` or `&mut x`.
    Borrow { is_mut: bool },
    /// `*x`.
    Deref,
    /// `x.f`.
    Field(u32),
    /// `x[i]`, whose index this walk does not evaluate.
    Element,
}

impl Step {
    /// Whether the step names the same storage as the expression under it. A
    /// projection names storage inside it instead.
    fn is_wrapper(self) -> bool {
        matches!(self, Self::Borrow { .. } | Self::Deref)
    }
}

/// Where an operand's chain ends and what it passes through on the way.
struct NamedPlace {
    /// The local the chain roots at, or `None` where it roots at something no
    /// place names: a global, a call result, a literal. A write lands in this
    /// local whatever the steps are; where inside it is [`Self::field_path`].
    root: Option<u32>,
    /// The expression naming the storage once the wrappers around it are
    /// peeled: `&mut x.repr` names `x.repr`, and `x.repr` names itself.
    named: ExprId,
    /// The steps from the operand inward to the root, outermost first, each
    /// with the operand it wraps.
    steps: Vec<(Step, Operand)>,
}

/// Resolve `op` to the place it names. `None` where a layer is a promoted value
/// rather than a skeleton node, which names no storage a walk can record.
fn named_place(body: &Body, op: Operand) -> Option<NamedPlace> {
    let mut steps: Vec<(Step, Operand)> = Vec::new();
    let mut named = None;
    let mut cursor = op;
    loop {
        let e = cursor.as_expr()?;
        let (step, inner) = match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => {
                return Some(NamedPlace {
                    root: Some(*index),
                    named: named.unwrap_or(e),
                    steps,
                });
            }
            ExprKind::Cast { expr, .. } => (None, *expr),
            ExprKind::Unary { op: unary, expr } => (Some(unary_step(*unary)?), *expr),
            ExprKind::FieldAccess {
                expr, field_index, ..
            } => (Some(Step::Field(*field_index)), *expr),
            ExprKind::Index { expr, .. } => (Some(Step::Element), *expr),
            _ => {
                return Some(NamedPlace {
                    root: None,
                    named: named.unwrap_or(e),
                    steps,
                });
            }
        };
        if let Some(step) = step {
            if !step.is_wrapper() {
                named.get_or_insert(e);
            }
            steps.push((step, inner));
        }
        cursor = inner;
    }
}

/// The step a unary operator spells, or `None` for one that computes a value.
fn unary_step(op: NirUnaryOp) -> Option<Step> {
    match op {
        NirUnaryOp::Ref => Some(Step::Borrow { is_mut: false }),
        NirUnaryOp::MutRef => Some(Step::Borrow { is_mut: true }),
        NirUnaryOp::Deref => Some(Step::Deref),
        _ => None,
    }
}

impl NamedPlace {
    /// The field path from the root, root-first. `None` where a step reaches
    /// storage no path spells — an element, whose index this walk leaves
    /// unevaluated.
    fn field_path(&self) -> Option<Vec<u32>> {
        let mut path = Vec::with_capacity(self.steps.len());
        for (step, _) in self.steps.iter().rev() {
            match step {
                Step::Field(index) => path.push(*index),
                Step::Borrow { .. } | Step::Deref => {}
                Step::Element => return None,
            }
        }
        Some(path)
    }
}

/// The expression `op` names once the wrappers around it are peeled.
pub(super) fn peel_wrappers(body: &Body, op: Operand) -> Option<ExprId> {
    Some(named_place(body, op)?.named)
}

/// The local `op` names as a value, no step reaching inside it or through a
/// handle.
pub(super) fn named_local(body: &Body, op: Operand) -> Option<u32> {
    let place = named_place(body, op)?;
    place.steps.is_empty().then_some(place.root)?
}

/// The local an lvalue chain roots at, or `None` for a borrow: a handle is a
/// value naming storage, not that storage.
pub(super) fn lvalue_root_local(body: &Body, op: Operand) -> Option<u32> {
    let place = named_place(body, op)?;
    place
        .steps
        .iter()
        .all(|(step, _)| !matches!(step, Step::Borrow { .. }))
        .then_some(place.root)?
}

/// The local a borrow or lvalue chain roots at, and the field path reaching
/// into its value. A write needs both: which local it touches, and where
/// inside it the write lands.
pub(super) fn place_of(body: &Body, op: Operand) -> Option<(u32, Vec<u32>)> {
    let place = named_place(body, op)?;
    Some((place.root?, place.field_path()?))
}

/// The local a write through `op` lands in.
pub(super) fn write_root_local(body: &Body, op: Operand) -> Option<u32> {
    named_place(body, op)?.root
}

/// The borrow `op` spells over a place: whether it is mutable, and the operand
/// borrowed. `None` unless a borrow is the outermost step over a place
/// [`place_of`] can spell — `&GLOBAL` and a borrow of an rvalue name no local
/// place.
pub(super) fn borrowed_place_operand(body: &Body, op: Operand) -> Option<(bool, Operand)> {
    let place = named_place(body, op)?;
    let &(Step::Borrow { is_mut }, inner) = place.steps.first()? else {
        return None;
    };
    place.root?;
    place.field_path()?;
    Some((is_mut, inner))
}

/// Whether some place in `places` besides `(root, path)` itself reaches the
/// storage it names. One place covers another when its path is a prefix: `c`
/// covers `c.repr`, and `c.repr` and `c.used` cover nothing of each other.
///
/// `places` holds the target's own place too, so a second match is what a
/// second handle looks like.
pub(super) fn place_aliased_by_another(
    places: &[(u32, Vec<u32>)],
    root: u32,
    path: &[u32],
) -> bool {
    places
        .iter()
        .filter(|(other_root, other_path)| {
            *other_root == root && (other_path.starts_with(path) || path.starts_with(other_path))
        })
        .nth(1)
        .is_some()
}
