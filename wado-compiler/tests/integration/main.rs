//! Every wado-compiler integration test except `e2e` and `format`, in one
//! binary. Those two stay separate because ci.yml and `mise run test-format`
//! select them by target name; `common.rs` stays beside them for `e2e`, so it
//! is reached from here by path.

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
