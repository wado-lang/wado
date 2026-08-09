//! Every wado-cli integration test, in one binary.
//!
//! Cargo links one executable per file under `tests/`, and each of these
//! pulls in wado-compiler and wasmtime. As separate targets they cost 0.69 GB
//! and 15 links; merged they cost one of each.
//!
//! Tests here share a process, so anything process-wide — cwd, environment,
//! signal handlers — is no longer isolated per file. `run_inprocess` is the
//! only module that mutates such state, and it serializes on its own lock;
//! every other module drives the CLI through `common::wado*`, which sets an
//! absolute working directory on each command.

#![allow(unused_crate_dependencies)]

mod common;

mod cli;
mod cli_parse;
mod dependency_resolution;
mod dump_kiln;
mod gale_cli;
mod git_dependency;
mod kiln_build_dep;
mod kiln_compile;
mod kiln_embed_wit;
mod kiln_multi_file;
mod kiln_pipeline;
mod lsp;
mod manifest_integration;
mod run_inprocess;
// Spawns the binary, drives a real TCP socket, and sends Unix signals.
#[cfg(unix)]
mod serve;
