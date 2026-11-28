//! Every wado-cli integration test, in one binary.
//!
//! Modules share a process, so cwd and environment are not isolated between
//! them. No module mutates either: each drives the CLI through
//! `common::wado*`, which sets an absolute working directory on each command,
//! or calls a subcommand with absolute paths.

#![allow(unused_crate_dependencies)]

mod common;

mod cli;
mod cli_parse;
mod coverage;
mod dependency_resolution;
mod dump_checks;
mod dump_kiln;
mod eval_dependency;
// Installs `wado-<name>` shell scripts on a temporary `PATH`.
#[cfg(unix)]
mod external_subcommand;
mod fuel;
mod gale_cli;
mod git_dependency;
mod kiln_build_dep;
mod kiln_check;
mod kiln_compile;
mod kiln_embed_wit;
mod kiln_multi_file;
mod kiln_nested;
mod kiln_options;
mod kiln_pipeline;
mod kiln_probe;
mod lsp;
mod manifest_integration;
mod pgo;
mod remote;
mod run_inprocess;
mod scaffold;
// Spawns the binary, drives a real TCP socket, and sends Unix signals.
#[cfg(unix)]
mod serve;
