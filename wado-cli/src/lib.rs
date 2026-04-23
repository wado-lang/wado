// Allow certain pedantic lints that are common in CLI code:
// - ref_option: &Option<T> is fine when coming from struct fields
// - missing_panics_doc: Mutex unwrap panics are obvious
// - struct_excessive_bools: CLI option structs naturally have many bools
// - too_many_lines: Large functions are acceptable in CLI code
// - needless_pass_by_value: Ownership transfer is sometimes intentional
#![allow(
    clippy::ref_option,
    clippy::missing_panics_doc,
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

pub mod args;
pub mod compile;
pub mod compiler_host;
pub mod doc;
pub mod dump;
pub mod format;
pub mod http_hooks;
pub mod init;
pub mod kiln_driver;
pub mod kiln_provider;
pub mod kiln_runtime;
pub mod lsp;
pub mod manifest;
pub mod query;
pub mod query_adapter;
pub mod run;
pub mod runtime;
pub mod serve;
pub mod syntax;
pub mod test;

pub use compiler_host::FilesystemCompilerHost;
