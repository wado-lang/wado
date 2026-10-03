//! The compiler backend: the NIR optimizer, WIR construction and optimization,
//! and Wasm code generation.

// See the frontend's crate root for why these are denied here.
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

// The backend reads the frontend's IR everywhere; this keeps `crate::nir` and
// its siblings naming the frontend's modules.
use wado_compiler_frontend::{
    ast, builtin_registry, call_args, canonical, codegen_flags, compiler_host, compiler_item,
    compiler_trace, component_model, component_plan, const_eval, coverage, defs, graph, hashmap,
    kiln, loader, lower, module_source, name, nir, nir_arena, nir_engine, nir_package, nir_unparse,
    nir_value_graph, nir_visitor, niri, primitive, synthesis, test_names, tir, token, trace, wir,
    wir_visitor, world_registry,
};

pub mod codegen;
pub mod optimize;
pub mod remarks;
pub mod wir_build;
pub mod wir_optimize;
pub mod wir_unparse;

pub use codegen::{InvalidArtifact, ProviderComponent};
pub use optimize::{OptOverrides, optimize};
pub use remarks::{
    Remark, collect_const_region_remarks, collect_param_gate_remarks, collect_value_copy_remarks,
};
