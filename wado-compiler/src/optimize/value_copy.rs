//! The optimizer's value-copy reasoning subsystem.
//!
//! Wado's value semantics insert `$value_copy$T` helpers at every assignment
//! and parameter pass of an identity-carrying value (`lower::plan::value_copy`).
//! The optimizer removes the ones whose clone is unobservable. The predicates
//! shared by those passes live here so each pass consults one definition rather
//! than re-deriving it:
//!
//! - [`identity`] — whether a value shares storage with its source on a
//!   shallow copy.
//! - [`mutation`] — the mutation-witness recognizer and its composite
//!   callee oracle (declared `&mut` bits ∧ receiver-writes fixpoint).
//!
//! Storage-root resolution is `arena_query::storage_root`, shared with the
//! escape / aliasing analyses. `value_copy_elide` (full wrapper removal) and
//! `value_copy_demote` (deep → shallow spine copy) are the consumers.

pub(in crate::optimize) mod identity;
pub(in crate::optimize) mod mutation;

pub(in crate::optimize) use identity::{carries_identity, needs_value_copy};
