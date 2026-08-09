//! Integration tests that compile a Wado program and observe the result:
//! diagnostics, the emitted component, its WIT, and its runtime behaviour.
//! Paired with `ir`, which pokes the compiler's IRs directly instead.
//!
//! Split in two rather than merged into one target: one binary per file
//! cost 41 links, but a single binary made every test edit recompile all
//! 19.6k lines. Two balanced halves keep the link count low without that.

#![allow(unused_crate_dependencies)]

#[path = "../common.rs"]
mod common;

mod cm_catalog;
mod cm_donut_canary;
mod cm_newtype_boundary;
mod cm_provider_compose;
mod cm_reexport_type;
mod cm_world_func_import;
mod compile_errors;
mod default_purity_sem;
mod digest_interop;
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
mod semantics;
mod serde_positional;
mod stores_check_sem;
mod stream_canonical_options;
mod string_templates;
mod test_name_filter;
mod trait_query;
mod unused_diagnostics;
mod wasm_import_dce;
mod wat;
mod wit;
mod wit_bundle;
mod wit_import_plan;
mod zlib_interop;
