use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};
use crate::discover;

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
        "recursively for *.wado files, honoring the enclosing package's [format]"
    )
    .unwrap();
    writeln!(
        buf,
        "exclude/include globs in its wado.toml, .gitignore, and submodules."
    )
    .unwrap();
    writeln!(buf, "A file named directly is formatted regardless.").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Without -w, outputs formatted code to stdout (single file only)."
    )
    .unwrap();
    buf
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

/// Expand the raw input paths into a concrete list of `*.wado` files. An
/// explicit file is kept as-is: the golden-fixture scripts format excluded
/// fixtures by naming them, so only directory expansion is filtered.
fn resolve_inputs(inputs: &[String]) -> Result<Vec<PathBuf>, CliExit> {
    let mut files = Vec::new();
    for input in inputs {
        let path = Path::new(input);
        if path.is_dir() {
            files.extend(discover::files_in_dir(path, &format_filters)?);
        } else {
            files.push(path.to_path_buf());
        }
    }
    Ok(files)
}

/// Directories never formatted by default: machine-generated parser output and
/// build intermediates. A package can opt a path back in via `[format].include`.
const BUILTIN_FORMAT_EXCLUDES: &[&str] = &["**/generated/**", "**/build/**"];

fn format_filters(
    pkg_root: &Path,
) -> Result<(discover::ExcludeSet, discover::IncludeSet), CliExit> {
    discover::filters_at(
        pkg_root,
        |m| (&m.format.exclude, &m.format.include),
        BUILTIN_FORMAT_EXCLUDES,
    )
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
                // `format` takes a string, so its errors carry no path, and
                // under `format -w .` the message alone names no file.
                eprintln!("{}", e.with_filename(&input.to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::discover::fixture::{one, touch, write_manifest};

    fn resolved_names(dir: &Path) -> BTreeSet<String> {
        resolve_inputs(&[dir.to_string_lossy().into_owned()])
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    // `[format]` globs are package-root-relative, so a subdirectory argument
    // must still be walked under the enclosing manifest — otherwise
    // `wado format wado-compiler/tests` rewrites the very fixtures that
    // manifest excludes.
    #[test]
    fn subdir_invocation_honours_enclosing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_manifest(
            root,
            "[format]\nexclude = [\"sub/gen/**\"]\ninclude = [\"sub/**/keep.wado\"]\n",
        );
        touch(&root.join("sub/gen/drop.wado"));
        touch(&root.join("sub/gen/keep.wado"));

        let got = resolved_names(&root.join("sub"));
        assert!(got.contains("keep.wado"), "{got:?}");
        assert!(!got.contains("drop.wado"), "{got:?}");
    }

    // The built-in skips apply without a manifest saying so, and `include`
    // opts a path back in.
    #[test]
    fn generated_output_is_skipped_unless_included() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("generated/parser.wado"));
        touch(&root.join("build/tmp.wado"));
        touch(&root.join("src/main.wado"));
        assert_eq!(resolved_names(root), one("main.wado"));

        write_manifest(root, "[format]\ninclude = [\"generated/**\"]\n");
        assert_eq!(
            resolved_names(root),
            ["main.wado", "parser.wado"]
                .iter()
                .map(ToString::to_string)
                .collect()
        );
    }

    // The golden-fixture scripts format excluded fixture files by naming them,
    // so an explicit file argument bypasses the filters by design.
    #[test]
    fn an_explicit_file_bypasses_the_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_manifest(root, "[format]\nexclude = [\"tests/**\"]\n");
        let skipped = root.join("tests/skip.wado");
        touch(&skipped);

        let files = resolve_inputs(&[skipped.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(files, vec![skipped]);
    }
}
