#![allow(
    clippy::ref_option,
    clippy::missing_panics_doc,
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

pub mod args;
pub mod build;
pub mod build_dep;
pub mod cache;
pub mod check;
pub mod clean;
pub mod compile;
pub mod compiler_host;
pub mod coverage_host;
pub mod dep_component;
pub mod discover;
pub mod doc;
pub mod dump;
pub mod eval_host;
pub mod external;
pub mod fetch;
mod fingerprint;
pub mod format;
pub mod git;
pub mod http_hooks;
pub mod init;
mod input_cache;
pub mod kiln_provider;
pub mod kiln_wit;
pub mod knobs;
pub mod lsp;
pub mod manifest;
pub mod metadata_embed;
pub mod oci;
pub mod process_host;
pub mod publish;
pub mod query;
pub mod query_adapter;
pub mod registry;
mod rss;
pub mod run;
pub mod run_cache;
pub mod runtime;
pub mod serve;
pub mod syntax;
pub mod test;
mod test_cache;
mod test_report;
pub mod update;
pub mod wit;

pub use compiler_host::FilesystemCompilerHost;

pub use wado_compiler::parallel::STACK_SIZE as COMPILER_STACK_SIZE;
