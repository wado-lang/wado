//! Who a call names, and what its operands bind to.
//!
//! Three walks ask this — the frame that runs a call, the trackability scan
//! that decides what a call reaches, and the region scan that decides whether a
//! block is self-contained. They agree here rather than each restating the
//! calling convention, of which there is exactly one fact worth restating: a
//! method's receiver is its first parameter.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::nir::NirFunction;
use crate::nir_arena::{ArenaCallArg, Body, ExprId, ExprKind, Operand};

/// Identity of a callee in the [`CalleeMap`].
pub type CalleeKey = crate::nir::FuncId;

/// The callees a compile-time frame may run.
///
/// Membership answers whether a frame may *run* the callee at all
/// ([`super::is_ctfe_runnable`]), decided once at construction. Whether a
/// call's value may be substituted for it is a separate question, answered per
/// call.
pub type CalleeMap = IndexMap<CalleeKey, Callee>;

/// A callee the engine may run, with the parameter facts the trackability
/// analysis needs answered without a borrow. Asking the function later answers
/// only when nobody holds `borrow_mut` on it, and a fold must not turn on
/// which function the visitor happens to be walking.
pub struct Callee {
    pub func: Rc<RefCell<NirFunction>>,
    arity: usize,
    mut_params: Vec<bool>,
    stored_params: Vec<bool>,
}

impl Callee {
    #[must_use]
    pub fn new(func: Rc<RefCell<NirFunction>>) -> Self {
        let (arity, mut_params, stored_params) = {
            let borrowed = func.borrow();
            (
                borrowed.params.len(),
                borrowed.params.iter().map(|p| p.is_mut_ref).collect(),
                borrowed
                    .params
                    .iter()
                    .map(|p| borrowed.stores.contains(&p.name))
                    .collect(),
            )
        };
        Self {
            func,
            arity,
            mut_params,
            stored_params,
        }
    }

    pub(super) fn arity(&self) -> usize {
        self.arity
    }

    /// A `&mut T` borrow is the only parameter kind that reaches the caller's
    /// storage. An index the signature does not have answers as one that does,
    /// so a call the map cannot account for is exempt from nothing.
    pub(super) fn writes_param(&self, index: usize) -> bool {
        self.mut_params.get(index).copied().unwrap_or(true)
    }

    /// A stored parameter outlives the call, so naming its referent is not the
    /// passing read the other arguments are.
    pub(super) fn reads_only(&self, index: usize) -> bool {
        self.stored_params.get(index).is_some_and(|stored| !stored)
    }
}

/// A call site: who it names, and the operands its parameters bind.
pub(super) struct CallSite<'a> {
    pub(super) func_id: CalleeKey,
    args: &'a [ArenaCallArg],
}

impl<'a> CallSite<'a> {
    /// The call `e` spells, or `None` for a node that is not one.
    pub(super) fn of(body: &'a Body, e: ExprId) -> Option<Self> {
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return None;
        };
        Some(Self {
            func_id: *func_id,
            args,
        })
    }

    /// How many parameters the site supplies. A method's receiver is `args[0]`,
    /// so it is counted like any other.
    pub(super) fn arity(&self) -> usize {
        self.args.len()
    }

    /// Each operand paired with the parameter index it binds.
    pub(super) fn operands(&self) -> impl Iterator<Item = (usize, Operand)> + '_ {
        self.args.iter().enumerate().map(|(i, a)| (i, a.expr))
    }

    /// [`Self::operands`], but only where the site matches `callee`'s
    /// signature. A call the map cannot account for is exempt from nothing, so
    /// an arity mismatch answers `None` rather than a partial pairing.
    pub(super) fn matching_operands(
        &self,
        callee: &Callee,
    ) -> Option<impl Iterator<Item = (usize, Operand)> + '_> {
        (self.arity() == callee.arity()).then(|| self.operands())
    }
}
