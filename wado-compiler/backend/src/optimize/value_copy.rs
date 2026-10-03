//! The optimizer's value-copy reasoning subsystem.
//!
//! - [`mutation`] — the mutation-witness recognizer and its callee oracle,
//!   consumed by `value_copy_demote` and `copy_prop`.
//!
//! Storage-root resolution is `arena_query::storage_root`.

pub(in crate::optimize) mod mutation;
