//! The command line, which mirrors the subset of `wado run` that applies here.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Result, bail};
use lexopt::Arg::{Long, Short, Value};
use lexopt::ValueExt as _;

const USAGE: &str = "\
Run a Wado program on a wasi:webgpu host.

Usage: wado run-webgpu [options] <file.wado|file.wasm> [args...]

Options:
  -O<level>        Optimization level: 0, 1, 2, 3 or s (default: 2)
      --dir <path> Preopen a directory (repeatable; default: the current one)
      --no-dir     Preopen nothing
  -h, --help       Show this help
  -V, --version    Show the version

Arguments after the input file go to the program.
";

pub struct Args {
    pub input: PathBuf,
    pub opt_level: String,
    /// Empty means preopen nothing, which `--no-dir` also asks for.
    pub preopens: Vec<PathBuf>,
    pub program_args: Vec<String>,
}

impl Args {
    /// `Ok(None)` when the run is already answered, as `--help` is.
    pub fn parse<I>(argv: I) -> Result<Option<Self>>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut parser = lexopt::Parser::from_args(argv);
        let mut input = None;
        let mut opt_level = "2".to_string();
        let mut preopens = Vec::new();
        let mut no_dir = false;
        let mut program_args = Vec::new();

        while let Some(arg) = parser.next()? {
            match arg {
                Short('h') | Long("help") => {
                    print!("{USAGE}");
                    return Ok(None);
                }
                Short('V') | Long("version") => {
                    println!("wado-run-webgpu {}", env!("CARGO_PKG_VERSION"));
                    return Ok(None);
                }
                Short('O') => {
                    opt_level = parser.value()?.string()?;
                    if !matches!(opt_level.as_str(), "0" | "1" | "2" | "3" | "s") {
                        bail!("unknown optimization level '-O{opt_level}'\n\n{USAGE}");
                    }
                }
                Long("dir") => preopens.push(PathBuf::from(parser.value()?)),
                Long("no-dir") => no_dir = true,
                Value(value) if input.is_none() => input = Some(PathBuf::from(value)),
                Value(value) => program_args.push(value.string()?),
                other => bail!("{}\n\n{USAGE}", other.unexpected()),
            }
        }

        let Some(input) = input else {
            bail!("no input file\n\n{USAGE}");
        };
        if no_dir {
            preopens.clear();
        } else if preopens.is_empty() {
            preopens.push(std::env::current_dir()?);
        }

        Ok(Some(Self {
            input,
            opt_level,
            preopens,
            program_args,
        }))
    }
}
