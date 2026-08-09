//! Integration tests that build or inspect the compiler's IRs directly —
//! the NIR interpreter, the TIR/NIR dumps, and the optimizer passes over
//! them. Paired with `surface`, which drives whole programs through the
//! compiler instead.
//!
//! Split in two rather than merged into one target: one binary per file
//! cost 41 links, but a single binary made every test edit recompile all
//! 19.6k lines. Two balanced halves keep the link count low without that.

#![allow(unused_crate_dependencies)]

#[path = "../common.rs"]
mod common;

mod codegen_flags;
mod dedupe_const_globals;
mod dump_moved_spans;
mod dump_tir_resolved;
mod niri;
mod redundant_bce;
mod remarks;
mod wasm_module_optimize;
