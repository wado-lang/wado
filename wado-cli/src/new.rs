//! `wado new <dir>`: `mkdir -p <dir> && cd <dir> && wado init`.

use std::path::PathBuf;

use crate::args::CliExit;
use crate::init::{self, InitOptions};

#[derive(Debug)]
pub struct NewOptions {
    pub dir: PathBuf,
    pub init: InitOptions,
}

pub fn parse_args(parser: lexopt::Parser) -> Result<NewOptions, CliExit> {
    let usage = init::scaffold_usage(
        "new",
        "<dir> ",
        "Create <dir> and a package in it: `mkdir -p <dir> && cd <dir> && wado init`.",
    );
    let (init, dir) = init::parse_options(parser, &usage)?;
    let dir = dir.ok_or_else(|| CliExit::error_with_usage("missing <dir>", &usage))?;
    Ok(NewOptions { dir, init })
}

pub fn run(opts: NewOptions) -> Result<(), CliExit> {
    init::in_new_dir(&init::current_dir()?.join(opts.dir), |dir| {
        init::init_package(dir, &opts.init)
    })
}
