//! Every wado-compiler integration test except `e2e` and `format`, in one
//! binary. Those two stay separate because ci.yml and `mise run test-format`
//! select them by target name.

#![allow(unused_crate_dependencies)]

use wado_compiler_tests as common;

mod assert_capture_plan;
mod cm_async_value_import;
mod cm_callback_export;
mod cm_catalog;
mod cm_decls_across_modules;
mod cm_donut_canary;
mod cm_dump_user_decls;
mod cm_interface_export_decodes;
mod cm_newtype_boundary;
mod cm_provider_compose;
mod cm_reexport_type;
mod cm_world_func_import;
mod codegen_flags;
mod compile_errors;
mod digest_interop;
mod dump_moved_spans;
mod dump_tir_resolved;
mod effect_check_sem;
mod fn_dispatch_stubs;
mod guest_effect_import;
mod inline_decisions;
mod int128_literal_coercion;
mod kiln_generator_world;
mod kiln_loader_redirect;
mod kiln_options;
mod lexer_recovery;
mod lib_aliased_import_export;
mod lib_ambient_stderr;
mod lib_async_task_return_free;
mod lib_async_void_export;
mod lib_export_alias_user_struct_as_primitive;
mod lib_future_payload_name_collides_with_cli;
mod lib_snake_case_param;
mod lib_sync_lift_post_return;
mod lib_unrestricted_handle_export;
mod loader_canonical_identity;
mod niri;
mod opt_loop_convergence;
mod package_unused;
mod purity_sem;
mod semantics;
mod serde_positional;
mod stdlib_module_as_entry;
mod test_name_filter;
mod trait_query;
mod unrestricted_resource;
mod wasm_import_dce;
mod wasm_module_optimize;
mod wat;
mod web_dom;
mod web_dom_e2e;
mod wit;
mod wit_bundle;
mod wit_import_plan;
mod zlib_interop;
