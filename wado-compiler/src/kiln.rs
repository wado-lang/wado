//! Kiln: schema-driven code generation for the Wado compiler.
//!
//! See WEP 2026-04-12 for the design. This module holds the pure-data
//! algorithmic pieces that stay wasm32-clean: the canonical invocation
//! representation, the DAG + topological sort, cache-key composition, and
//! `#![generated]` header emission.
//!
//! The pipeline driver — which actually calls
//! [`crate::CompilerHost::run_generator`], writes generated files, and
//! maintains `wado.lock` — lives in `wado-cli`. That driver takes the data
//! types exported here, schedules invocations against a [`plan::Plan`], and
//! checks each step against the lockfile via [`cache::compose_cache_key`].

pub mod cache;
pub mod header;
pub mod import_check;
pub mod inline;
pub mod invocation;
pub mod options;
pub mod options_check;
pub mod plan;

pub use cache::{
    CacheKeyInputs, FileHash, compose_cache_key, content_hash, encode_options_canonical, file_hash,
    gather_file_hashes, generator_identity, hash_options_canonical, hex_digest,
};
pub use header::{GeneratedHeader, has_generated_marker, parse_header};
pub use inline::{InvocationIndex, collect_inline_invocations, merge_manifest_and_inline};
pub use invocation::{DeclSite, GeneratorModule, Invocation, InvocationPath};
pub use options::{
    CanonicalValue, OptionsDescriptor, OptionsField, OptionsType, extract_options_descriptor,
};
pub use options_check::{CanonicalOptions, validate as validate_options};
pub use plan::{Plan, PlanError, build_plan, depends_on};
