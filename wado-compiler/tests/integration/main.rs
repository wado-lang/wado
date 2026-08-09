//! Every wado-compiler integration test that is not named from CI, in one
//! binary.
//!
//! Cargo links one executable per file under `tests/`, and each of these
//! statically links the compiler and wasmtime. As separate targets the 41
//! files here cost 4.16 GB and 41 links; merged they cost one of each.
//!
//! `e2e` and `format` stay separate: ci.yml shards `e2e` across five
//! optimisation levels and `mise run test-format` drives `format`, both by
//! target name. `common.rs` stays beside them because `e2e` uses it too, so
//! it is reached from here by path.

#![allow(unused_crate_dependencies)]

#[path = "../common.rs"]
mod common;

mod cm_catalog;
mod cm_donut_canary;
mod cm_newtype_boundary;
mod cm_provider_compose;
mod cm_reexport_type;
mod cm_world_func_import;
mod codegen_flags;
mod compile_errors;
mod dedupe_const_globals;
mod default_purity_sem;
mod digest_interop;
mod dump_moved_spans;
mod dump_tir_resolved;
mod effect_check_sem;
mod guest_effect_import;
mod kiln_generator_world;
mod kiln_loader_redirect;
mod kiln_options;
mod lexer_recovery;
mod lib_async_task_return_free;
mod lib_sync_lift_post_return;
mod literals;
mod loader_canonical_identity;
mod niri;
mod redundant_bce;
mod remarks;
mod semantics;
mod serde_positional;
mod stores_check_sem;
mod stream_canonical_options;
mod string_templates;
mod test_name_filter;
mod trait_query;
mod unused_diagnostics;
mod wasm_import_dce;
mod wasm_module_optimize;
mod wat;
mod wit;
mod wit_bundle;
mod wit_import_plan;
mod zlib_interop;
