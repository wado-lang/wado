//! The optimizer's value-copy reasoning subsystem: the predicates shared by
//! `value_copy_elide` (full `$value_copy$T` wrapper removal) and
//! `value_copy_demote` (deep → shallow spine copy).
//!
//! - [`identity`] — whether a value shares storage with its source on a
//!   shallow copy.
//! - [`mutation`] — the mutation-witness recognizer and its callee oracle.
//!
//! Storage-root resolution is `arena_query::storage_root`.

pub(in crate::optimize) mod identity;
pub(in crate::optimize) mod mutation;

pub(in crate::optimize) use identity::{carries_identity, needs_value_copy};
