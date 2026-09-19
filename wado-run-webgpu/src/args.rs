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
      --no-dir     Drop that default
  -h, --help       Show this help
  -V, --version    Show the version

Arguments after the input file go to the program.
";

/// An optimization level `wado` accepts, so no other spelling reaches a caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Os,
}

impl OptLevel {
    fn parse(level: &str) -> Option<Self> {
        match level {
            "0" => Some(Self::O0),
            "1" => Some(Self::O1),
            "2" => Some(Self::O2),
            "3" => Some(Self::O3),
            "s" => Some(Self::Os),
            _ => None,
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            Self::O0 => "-O0",
            Self::O1 => "-O1",
            Self::O2 => "-O2",
            Self::O3 => "-O3",
            Self::Os => "-Os",
        }
    }
}

pub struct Args {
    pub input: PathBuf,
    pub opt_level: OptLevel,
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
        let mut opt_level = OptLevel::O2;
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
                // Attached and explicit, as `wado run` takes it: `-O2`.
                Short('O') => {
                    let Some(level) = parser.optional_value() else {
                        bail!("-O takes its level attached, as '-O2'\n\n{USAGE}");
                    };
                    let level = level.string()?;
                    let Some(parsed) = OptLevel::parse(&level) else {
                        bail!("unknown optimization level '-O{level}'\n\n{USAGE}");
                    };
                    opt_level = parsed;
                }
                Long("dir") => preopens.push(PathBuf::from(parser.value()?)),
                Long("no-dir") => no_dir = true,
                // Everything after the input file, flags included, is the
                // program's, exactly as `wado run` forwards it.
                Value(value) => {
                    input = Some(PathBuf::from(value));
                    if let Some(raw) = parser.try_raw_args() {
                        for raw_arg in raw {
                            program_args.push(raw_arg.string()?);
                        }
                    }
                    break;
                }
                other => bail!("{}\n\n{USAGE}", other.unexpected()),
            }
        }

        let Some(input) = input else {
            bail!("no input file\n\n{USAGE}");
        };
        if preopens.is_empty() && !no_dir {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn try_parse(argv: &[&str]) -> Result<Option<Args>> {
        Args::parse(argv.iter().map(OsString::from))
    }

    fn parse(argv: &[&str]) -> Args {
        try_parse(argv).expect("parsing").expect("a run to make")
    }

    #[test]
    fn the_optimization_level_comes_attached() {
        assert_eq!(parse(&["-O0", "app.wado"]).opt_level, OptLevel::O0);
        assert_eq!(parse(&["app.wado"]).opt_level, OptLevel::O2);
        assert!(try_parse(&["-O", "app.wado"]).is_err());
        assert!(try_parse(&["-O", "2", "app.wado"]).is_err());
        assert!(try_parse(&["-O9", "app.wado"]).is_err());
    }

    #[test]
    fn everything_after_the_input_file_belongs_to_the_program() {
        let args = parse(&["-O0", "app.wado", "--dir", "/data", "-x", "rest"]);

        assert_eq!(args.opt_level, OptLevel::O0);
        assert_eq!(args.program_args, ["--dir", "/data", "-x", "rest"]);
        assert_eq!(args.preopens, [std::env::current_dir().unwrap()]);
    }

    #[test]
    fn no_dir_drops_the_default_grant_and_keeps_every_explicit_one() {
        assert!(parse(&["--no-dir", "app.wado"]).preopens.is_empty());
        assert_eq!(
            parse(&["--no-dir", "--dir", "/data", "app.wado"]).preopens,
            [PathBuf::from("/data")]
        );
    }
}
