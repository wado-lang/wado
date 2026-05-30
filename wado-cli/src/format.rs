use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};
use crate::discover;
use crate::manifest as project_manifest;

pub struct FormatOptions {
    pub inputs: Vec<String>,
    pub write_in_place: bool,
    pub check: bool,
}

#[derive(Clone, Copy)]
enum Opt {
    Write,
    Check,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Write, Self::Check, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Write => args::OptSpec {
                long: Some("write"),
                short: Some('w'),
                value: None,
                desc: "Write formatted output back to file",
            },
            Self::Check => args::OptSpec {
                long: Some("check"),
                short: None,
                value: None,
                desc: "Check if file is formatted (exit 1 if not)",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado format [options] <path>...").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "A <path> may be a file or a directory. Directories are searched"
    )
    .unwrap();
    writeln!(
        buf,
        "recursively for *.wado files, honoring each package's [format]"
    )
    .unwrap();
    writeln!(
        buf,
        "exclude/include globs in its wado.toml, .gitignore, and submodules."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Without -w, outputs formatted code to stdout (single file only)."
    )
    .unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<FormatOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut write_in_place = false;
    let mut check = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Write => write_in_place = true,
                Opt::Check => check = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    if inputs.is_empty() {
        return Err(CliExit::error_with_usage("no input file specified", &usage));
    }

    // Multiple files require -w or --check
    if inputs.len() > 1 && !write_in_place && !check {
        return Err(CliExit::error_with_usage(
            "multiple files require -w or --check",
            &usage,
        ));
    }

    Ok(FormatOptions {
        inputs,
        write_in_place,
        check,
    })
}

/// Expand the raw input paths into a concrete list of `*.wado` files.
/// A file is kept as-is; a directory is searched recursively per package (see
/// [`collect_package_tree`]), honoring each package's `[format]` filters plus
/// `.gitignore` and git submodules.
fn resolve_inputs(inputs: &[String]) -> Result<Vec<PathBuf>, CliExit> {
    let mut files = Vec::new();
    for input in inputs {
        let path = Path::new(input);
        if path.is_dir() {
            collect_package_tree(path, &mut files)?;
        } else {
            files.push(path.to_path_buf());
        }
    }
    Ok(files)
}

/// Collect every formattable `*.wado` file at and below `root`, processing each
/// package (a directory with its own `wado.toml`) with its own `[format]`
/// filters — exactly how `wado test` discovers per-package. A nested package's
/// `exclude`/`include` is relative to that package's root, so e.g.
/// `package-gale/wado.toml` can exclude its own `tests/generated/**`.
fn collect_package_tree(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), CliExit> {
    let mut queue = vec![root.to_path_buf()];
    while let Some(pkg) = queue.pop() {
        let (excludes, includes) = format_filters_for(&pkg)?;
        let result =
            discover::discover_test_files(&pkg, &excludes, &includes).map_err(CliExit::error)?;
        files.extend(result.files);
        queue.extend(result.subpackages);
    }
    Ok(())
}

/// Directories never formatted by default: machine-generated parser output and
/// build intermediates. A package can opt a path back in via `[format].include`.
const BUILTIN_FORMAT_EXCLUDES: &[&str] = &["**/generated/**", "**/build/**"];

/// The effective exclude / include filters for the package rooted exactly at
/// `dir`: the built-in skips above plus the package's own `[format].exclude`,
/// and its `[format].include` (which overrides both). A manifest discovered
/// only in an ancestor directory does not apply (its globs are relative to a
/// different root), mirroring how `wado test` reads `[test]` filters.
fn format_filters_for(dir: &Path) -> Result<(discover::ExcludeSet, discover::IncludeSet), CliExit> {
    let (exclude, include) = match project_manifest::discover(dir) {
        Ok(Some(project)) if project.root == dir => {
            (project.manifest.format.exclude, project.manifest.format.include)
        }
        Ok(_) => (Vec::new(), Vec::new()),
        Err(e) => return Err(CliExit::error(e)),
    };
    let patterns: Vec<&str> = BUILTIN_FORMAT_EXCLUDES
        .iter()
        .copied()
        .chain(exclude.iter().map(String::as_str))
        .collect();
    let excludes = discover::ExcludeSet::compile(&patterns).map_err(CliExit::error)?;
    let includes = discover::IncludeSet::compile(&include).map_err(CliExit::error)?;
    Ok((excludes, includes))
}

pub fn run(opts: FormatOptions) -> Result<(), CliExit> {
    let files = resolve_inputs(&opts.inputs)?;

    if !opts.write_in_place && !opts.check && files.len() > 1 {
        return Err(CliExit::error(
            "multiple files require -w or --check (a directory expanded to several files)",
        ));
    }

    let mut any_would_reformat = false;
    let mut any_error = false;

    for path in &files {
        let input = path.display();

        let original = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading '{input}': {e}");
                any_error = true;
                continue;
            }
        };

        let start = Instant::now();
        let formatted = match wado_compiler::format(&original) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                any_error = true;
                continue;
            }
        };
        let elapsed_ms = start.elapsed().as_millis();

        if opts.check {
            if original != formatted {
                eprintln!("{input}: would reformat");
                any_would_reformat = true;
            }
        } else if opts.write_in_place {
            if original != formatted {
                match fs::write(path, &formatted) {
                    Ok(()) => {
                        eprintln!("Formatted: {input} ({elapsed_ms}ms)");
                    }
                    Err(e) => {
                        eprintln!("Error writing '{input}': {e}");
                        any_error = true;
                    }
                }
            }
        } else {
            print!("{formatted}");
        }
    }

    if any_error || (opts.check && any_would_reformat) {
        return Err(CliExit::silent_failure(1));
    }
    Ok(())
}
