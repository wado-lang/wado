//! Kiln: schema-driven code generation (WEP 2026-04-12). This module holds the
//! pure-data pieces that stay wasm32-clean — the canonical invocation
//! representation, the DAG and its topological sort, cache-key composition, and
//! `#![generated]` header emission. The driver that runs generators, writes
//! files and maintains `wado.lock` lives in `wado-cli` and consumes these.

pub mod cache;
pub mod harvest;
pub mod header;
pub mod import_check;
pub mod inline;
pub mod invocation;
pub mod metadata;
pub mod options;
pub mod options_check;
pub mod plan;

pub use cache::{
    CacheKeyInputs, FileHash, compose_cache_key, content_hash, empty_options_canonical,
    encode_options_canonical, file_hash, gather_file_hashes, generator_identity,
    hash_options_canonical, hex_digest,
};
pub use harvest::{Harvest, harvest_module_graph, remap_decl_files};
pub use header::{GeneratedHeader, has_generated_marker, parse_header};
pub use inline::{InvocationIndex, collect_inline_invocations};
pub use invocation::{
    DeclSite, GENERATOR_WORLD_FQ, GeneratorModule, GeneratorSpec, Invocation, InvocationPath,
    SpecParts, parse_spec, spec_key,
};
pub use options::{
    CanonicalValue, OptionsDescriptor, OptionsField, OptionsType, extract_options_descriptor,
};
pub use options_check::{CanonicalOptions, OptionsAnchor, validate as validate_options};
pub use plan::{Plan, PlanError, build_plan, depends_on};
