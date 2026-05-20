//! In-process CLI argument parsing tests.
//!
//! Tests `parse_args` functions directly without spawning processes,
//! using `lexopt::Parser::from_args` to construct parsers from argument slices.

use lexopt::Parser;

use wado_cli::args::CliExit;
use wado_cli::compile::{self, OptLevel, OutputFormat};

/// Assert that a `Result<T, CliExit>` is an error with exit code 1
/// and the message contains the given substring.
fn assert_err<T>(result: Result<T, CliExit>, expected: &str) {
    match result {
        Ok(_) => panic!("expected CliExit error, but got Ok"),
        Err(err) => {
            assert_eq!(
                err.exit_code, 1,
                "expected exit_code 1, got {}",
                err.exit_code
            );
            assert!(
                err.message.contains(expected),
                "expected message to contain {expected:?}, got {:?}",
                err.message
            );
        }
    }
}

/// Assert that a `Result<T, CliExit>` is a help exit (exit code 0)
/// and the message contains the given substring.
fn assert_help<T>(result: Result<T, CliExit>, expected: &str) {
    match result {
        Ok(_) => panic!("expected CliExit help, but got Ok"),
        Err(err) => {
            assert_eq!(
                err.exit_code, 0,
                "expected exit_code 0, got {}",
                err.exit_code
            );
            assert!(
                err.message.contains(expected),
                "expected message to contain {expected:?}, got {:?}",
                err.message
            );
        }
    }
}

// ---- compile ----

#[test]
fn compile_basic() {
    let parser = Parser::from_args(&["input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert_eq!(opts.input, "input.wado");
    assert_eq!(opts.output, None);
    assert_eq!(opts.format, None);
    assert_eq!(opts.opt_level, OptLevel::O2); // default
    assert!(!opts.wat_to_stdout);
    assert!(!opts.skip_validation);
}

#[test]
fn compile_with_output() {
    let parser = Parser::from_args(&["-o", "out.wasm", "input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert_eq!(opts.input, "input.wado");
    assert_eq!(opts.output, Some("out.wasm".to_string()));
}

#[test]
fn compile_format_wat() {
    let parser = Parser::from_args(&["--format", "wat", "input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert_eq!(opts.format, Some(OutputFormat::Wat));
}

#[test]
fn compile_format_wasm() {
    let parser = Parser::from_args(&["--format", "wasm", "input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert_eq!(opts.format, Some(OutputFormat::Wasm));
}

#[test]
fn compile_wat_to_stdout() {
    let parser = Parser::from_args(&["--wat-to-stdout", "input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert!(opts.wat_to_stdout);
}

#[test]
fn compile_opt_levels() {
    for (arg, expected) in [
        ("-O0", OptLevel::O0),
        ("-O1", OptLevel::O1),
        ("-O2", OptLevel::O2),
        ("-O3", OptLevel::O3),
        ("-Os", OptLevel::Os),
    ] {
        let parser = Parser::from_args(&[arg, "input.wado"]);
        let opts = compile::parse_args(parser).unwrap();
        assert_eq!(opts.opt_level, expected, "failed for {arg}");
    }
}

#[test]
fn compile_no_validate() {
    let parser = Parser::from_args(&["--no-validate", "input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert!(opts.skip_validation);
}

#[test]
fn compile_world() {
    let parser = Parser::from_args(&["--world", "test", "input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert_eq!(opts.target_world, Some("test".to_string()));
}

#[test]
fn compile_log_level() {
    let parser = Parser::from_args(&["--log-level", "debug", "input.wado"]);
    let opts = compile::parse_args(parser).unwrap();
    assert_eq!(opts.log_level, wado_compiler::LogLevel::Debug);
}

#[test]
fn compile_help() {
    let parser = Parser::from_args(&["--help"]);
    assert_help(compile::parse_args(parser), "Usage: wado compile");
}

#[test]
fn compile_no_input() {
    // The repo root carries a wado.toml without `[package].command`, so
    // the resolver short-circuits with the more specific message.
    let parser = Parser::from_args::<&[&str]>(&[]);
    assert_err(
        compile::parse_args(parser),
        "wado.toml found but [package].command is not set",
    );
}

#[test]
fn compile_unknown_option() {
    let parser = Parser::from_args(&["--unknown", "input.wado"]);
    assert_err(compile::parse_args(parser), "invalid option");
}

#[test]
fn compile_invalid_format() {
    let parser = Parser::from_args(&["--format", "invalid", "input.wado"]);
    assert_err(compile::parse_args(parser), "unknown format");
}

#[test]
fn compile_invalid_opt_level() {
    let parser = Parser::from_args(&["-Ox", "input.wado"]);
    assert_err(compile::parse_args(parser), "unknown optimization level");
}

#[test]
fn compile_invalid_log_level() {
    let parser = Parser::from_args(&["--log-level", "invalid", "input.wado"]);
    assert_err(compile::parse_args(parser), "unknown log level");
}

#[test]
fn compile_multiple_inputs() {
    let parser = Parser::from_args(&["a.wado", "b.wado"]);
    assert_err(compile::parse_args(parser), "multiple input files");
}

#[test]
fn compile_all_options() {
    let parser = Parser::from_args(&[
        "-o",
        "out.wat",
        "--format",
        "wat",
        "-O3",
        "--log-level",
        "warn",
        "--world",
        "wasi:http/service",
        "--no-validate",
        "input.wado",
    ]);
    let opts = compile::parse_args(parser).unwrap();
    assert_eq!(opts.input, "input.wado");
    assert_eq!(opts.output, Some("out.wat".to_string()));
    assert_eq!(opts.format, Some(OutputFormat::Wat));
    assert_eq!(opts.opt_level, OptLevel::O3);
    assert_eq!(opts.log_level, wado_compiler::LogLevel::Warn);
    assert_eq!(opts.target_world, Some("wasi:http/service".to_string()));
    assert!(opts.skip_validation);
}

// ---- run ----

#[test]
fn run_basic() {
    let parser = Parser::from_args(&["input.wado"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    assert_eq!(opts.input, "input.wado");
    assert_eq!(opts.opt_level, OptLevel::O2);
    // Default: current dir is preopened
    assert_eq!(
        opts.preopened_dirs,
        vec![(".".to_string(), ".".to_string())]
    );
    assert!(opts.program_args.is_empty());
}

#[test]
fn run_with_dir() {
    let parser = Parser::from_args(&["--dir", "/tmp::/guest", "input.wado"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    // --dir overrides default
    assert_eq!(
        opts.preopened_dirs,
        vec![("/tmp".to_string(), "/guest".to_string())]
    );
}

#[test]
fn run_no_dir() {
    let parser = Parser::from_args(&["--no-dir", "input.wado"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    assert!(opts.preopened_dirs.is_empty());
}

#[test]
fn run_program_args() {
    let parser = Parser::from_args(&["input.wado", "arg1", "arg2"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    assert_eq!(opts.input, "input.wado");
    assert_eq!(opts.program_args, vec!["arg1", "arg2"]);
}

#[test]
fn run_help() {
    let parser = Parser::from_args(&["--help"]);
    assert_help(wado_cli::run::parse_args(parser), "Usage: wado run");
}

#[test]
fn run_no_input() {
    // Same situation as compile_no_input: repo-root wado.toml has no
    // `[package].command`, and the resolver surfaces that.
    let parser = Parser::from_args::<&[&str]>(&[]);
    assert_err(
        wado_cli::run::parse_args(parser),
        "wado.toml found but [package].command is not set",
    );
}

#[test]
fn run_unknown_option() {
    let parser = Parser::from_args(&["--unknown", "input.wado"]);
    assert_err(wado_cli::run::parse_args(parser), "invalid option");
}

#[test]
fn run_opt_level() {
    let parser = Parser::from_args(&["-O3", "input.wado"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    assert_eq!(opts.opt_level, OptLevel::O3);
}

#[test]
fn run_log_level() {
    let parser = Parser::from_args(&["--log-level", "error", "input.wado"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    assert_eq!(opts.log_level, wado_compiler::LogLevel::Error);
}

#[test]
fn run_profile_guest() {
    let parser = Parser::from_args(&["--profile", "guest", "input.wado"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    assert!(matches!(
        opts.profile,
        wado_cli::runtime::ProfileMode::Guest { .. }
    ));
}

#[test]
fn run_profile_invalid() {
    let parser = Parser::from_args(&["--profile", "unknown", "input.wado"]);
    assert_err(wado_cli::run::parse_args(parser), "unknown profile mode");
}

#[test]
fn run_allocator() {
    let parser = Parser::from_args(&["--allocator", "freelist", "input.wado"]);
    let opts = wado_cli::run::parse_args(parser).unwrap();
    assert_eq!(opts.allocator, Some("freelist".to_string()));
}

// ---- serve ----

#[test]
fn serve_basic() {
    let parser = Parser::from_args(&["input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert_eq!(opts.input, "input.wado");
    assert_eq!(opts.addr, "0.0.0.0:8080");
}

#[test]
fn serve_custom_addr() {
    let parser = Parser::from_args(&["--addr", "127.0.0.1:3000", "input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert_eq!(opts.addr, "127.0.0.1:3000");
}

#[test]
fn serve_help() {
    let parser = Parser::from_args(&["--help"]);
    assert_help(wado_cli::serve::parse_args(parser), "Usage: wado serve");
}

#[test]
fn serve_no_input() {
    // `wado serve` resolves the entry point against `[package].service`;
    // the repo-root wado.toml is missing that, so the resolver emits the
    // more specific error.
    let parser = Parser::from_args::<&[&str]>(&[]);
    assert_err(
        wado_cli::serve::parse_args(parser),
        "wado.toml found but [package].service is not set",
    );
}

#[test]
fn serve_opt_level() {
    let parser = Parser::from_args(&["-Os", "input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert_eq!(opts.opt_level, OptLevel::Os);
}

#[test]
fn serve_log_level() {
    let parser = Parser::from_args(&["--log-level", "warn", "input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert_eq!(opts.log_level, wado_compiler::LogLevel::Warn);
}

#[test]
fn serve_allocator() {
    let parser = Parser::from_args(&["--allocator", "debug", "input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert_eq!(opts.allocator, Some("debug".to_string()));
}

#[test]
fn serve_dir() {
    // Unlike `run`, `serve` defaults to NO preopens — explicit --dir is the
    // only way to grant filesystem access.
    let parser = Parser::from_args(&["--dir", "/tmp::/guest", "input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert_eq!(
        opts.preopened_dirs,
        vec![("/tmp".to_string(), "/guest".to_string())]
    );
}

#[test]
fn serve_no_dir_default() {
    let parser = Parser::from_args(&["input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert!(opts.preopened_dirs.is_empty());
}

#[test]
fn serve_workers_and_max_concurrency() {
    let parser = Parser::from_args(&["--workers", "4", "--max-concurrency", "64", "input.wado"]);
    let opts = wado_cli::serve::parse_args(parser).unwrap();
    assert_eq!(opts.workers, Some(4));
    assert_eq!(opts.max_concurrency, 64);
}

#[test]
fn serve_rejects_workers_above_max_concurrency() {
    let parser = Parser::from_args(&["--workers", "65", "--max-concurrency", "64", "input.wado"]);
    assert_err(
        wado_cli::serve::parse_args(parser),
        "--workers (65) must not exceed --max-concurrency (64)",
    );
}

#[test]
fn serve_rejects_workers_above_u32() {
    let parser = Parser::from_args(&["--workers", "4294967296", "input.wado"]);
    assert_err(
        wado_cli::serve::parse_args(parser),
        "--workers value is too large",
    );
}

#[test]
fn serve_rejects_max_concurrency_above_u32() {
    let parser = Parser::from_args(&["--max-concurrency", "4294967296", "input.wado"]);
    assert_err(
        wado_cli::serve::parse_args(parser),
        "--max-concurrency value is too large",
    );
}

#[test]
fn serve_rejects_no_dir() {
    // `serve` deliberately omits `--no-dir`: the default is already no
    // preopens, so the flag would be a misleading no-op. Confirm parsing
    // fails rather than silently accepting it.
    let parser = Parser::from_args(&["--no-dir", "input.wado"]);
    assert_err(wado_cli::serve::parse_args(parser), "invalid option");
}

// ---- test ----

#[test]
fn test_with_files() {
    // Explicit file arguments collapse into a single synthetic root
    // PackageRun; sub-package recursion only kicks in when no args are given.
    let parser = Parser::from_args(&["a.wado", "b.wado"]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.package_runs.len(), 1);
    assert_eq!(opts.package_runs[0].label, ".");
    assert_eq!(opts.package_runs[0].paths, vec!["a.wado", "b.wado"]);
}

#[test]
fn test_with_filter_keeps_matching_paths() {
    // --filter is a path-based wildcard applied during parse: `*.wado`
    // keeps `a.wado` and drops `b.txt`.
    let parser = Parser::from_args(&["-f", "*.wado", "a.wado", "b.txt"]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.package_runs.len(), 1);
    assert_eq!(opts.package_runs[0].paths, vec!["a.wado"]);
}

#[test]
fn test_with_filter_no_match_exits_cleanly() {
    // No discovered path matches the filter — the user gets a non-fatal exit
    // (code 0) with an explanatory message rather than running zero tests.
    let parser = Parser::from_args(&["-f", "no_match", "a.wado"]);
    assert_help(
        wado_cli::test::parse_args(parser),
        "No .wado files match --filter",
    );
}

#[test]
fn test_with_invalid_filter_pattern() {
    let parser = Parser::from_args(&["-f", "[unterminated", "a.wado"]);
    assert_err(
        wado_cli::test::parse_args(parser),
        "invalid --filter pattern",
    );
}

#[test]
fn test_with_exclude_keeps_non_matching_explicit_file() {
    // `--exclude` extends the manifest-level [test].exclude list and is
    // applied to explicit file arguments too: `package-gale/**` does not
    // match `a.wado`, so the file survives.
    let parser = Parser::from_args(&["--exclude", "package-gale/**", "a.wado"]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.package_runs[0].paths, vec!["a.wado"]);
}

#[test]
fn test_with_exclude_drops_matching_explicit_file() {
    // The matching counterpart: `drop/**` covers `drop/inner.wado` so
    // that file is filtered out, while `keep.wado` survives.
    let parser = Parser::from_args(&["--exclude", "drop/**", "drop/inner.wado", "keep.wado"]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.package_runs[0].paths, vec!["keep.wado"]);
}

#[test]
fn test_with_parallel() {
    let parser = Parser::from_args(&["-p", "4", "a.wado"]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.jobs, 4);
}

#[test]
fn test_parallel_invalid() {
    let parser = Parser::from_args(&["-p", "0", "a.wado"]);
    assert_err(
        wado_cli::test::parse_args(parser),
        "--parallel requires a positive integer",
    );
}

#[test]
fn test_parallel_non_numeric() {
    let parser = Parser::from_args(&["-p", "abc", "a.wado"]);
    assert_err(
        wado_cli::test::parse_args(parser),
        "--parallel requires a positive integer",
    );
}

#[test]
fn test_help() {
    let parser = Parser::from_args(&["--help"]);
    assert_help(wado_cli::test::parse_args(parser), "Usage: wado test");
}

#[test]
fn test_log_level() {
    let parser = Parser::from_args(&["--log-level", "debug", "a.wado"]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.log_level, wado_compiler::LogLevel::Debug);
}

#[test]
fn test_inline_threshold_and_iterations() {
    let parser = Parser::from_args(&[
        "--optimize-inline-threshold",
        "42",
        "--optimize-iterations",
        "7",
        "a.wado",
    ]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.inline_threshold, Some(42));
    assert_eq!(opts.opt_iterations, Some(7));
}

#[test]
fn test_allocator() {
    let parser = Parser::from_args(&["--allocator", "bump", "a.wado"]);
    let opts = wado_cli::test::parse_args(parser).unwrap();
    assert_eq!(opts.allocator, Some("bump".to_string()));
}

// ---- format ----

#[test]
fn format_basic() {
    let parser = Parser::from_args(&["input.wado"]);
    let opts = wado_cli::format::parse_args(parser).unwrap();
    assert_eq!(opts.inputs, vec!["input.wado"]);
    assert!(!opts.write_in_place);
    assert!(!opts.check);
}

#[test]
fn format_write() {
    let parser = Parser::from_args(&["-w", "input.wado"]);
    let opts = wado_cli::format::parse_args(parser).unwrap();
    assert!(opts.write_in_place);
}

#[test]
fn format_check() {
    let parser = Parser::from_args(&["--check", "input.wado"]);
    let opts = wado_cli::format::parse_args(parser).unwrap();
    assert!(opts.check);
}

#[test]
fn format_multiple_files_with_write() {
    let parser = Parser::from_args(&["-w", "a.wado", "b.wado"]);
    let opts = wado_cli::format::parse_args(parser).unwrap();
    assert_eq!(opts.inputs, vec!["a.wado", "b.wado"]);
}

#[test]
fn format_multiple_files_without_write() {
    let parser = Parser::from_args(&["a.wado", "b.wado"]);
    assert_err(
        wado_cli::format::parse_args(parser),
        "multiple files require -w or --check",
    );
}

#[test]
fn format_no_input() {
    let parser = Parser::from_args::<&[&str]>(&[]);
    assert_err(
        wado_cli::format::parse_args(parser),
        "no input file specified",
    );
}

#[test]
fn format_help() {
    let parser = Parser::from_args(&["--help"]);
    assert_help(wado_cli::format::parse_args(parser), "Usage: wado format");
}

// ---- dump ----

#[test]
fn dump_basic() {
    let parser = Parser::from_args(&["input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert_eq!(opts.inputs, vec!["input.wado"]);
    // Default: show WIR only
    assert!(!opts.show_tokens);
    assert!(!opts.show_ast);
    assert!(!opts.show_nir);
    assert!(opts.show_wir);
}

#[test]
fn dump_single_phase() {
    let parser = Parser::from_args(&["--ast", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_ast);
    assert!(!opts.show_tokens);
    assert!(!opts.show_nir);
}

#[test]
fn dump_opt_level() {
    let parser = Parser::from_args(&["--nir", "-O3", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_nir);
    assert_eq!(opts.opt_level, wado_compiler::OptLevel::O3);
}

#[test]
fn dump_tokens() {
    let parser = Parser::from_args(&["--tokens", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_tokens);
    assert!(!opts.show_ast);
    assert!(!opts.show_nir);
}

#[test]
fn dump_desugared() {
    let parser = Parser::from_args(&["--desugared", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_desugared);
    assert!(!opts.show_ast);
}

#[test]
fn dump_modules() {
    let parser = Parser::from_args(&["--modules", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_modules);
    assert!(!opts.show_symbols);
}

#[test]
fn dump_symbols() {
    let parser = Parser::from_args(&["--symbols", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_symbols);
    assert!(!opts.show_modules);
}

#[test]
fn dump_nir() {
    let parser = Parser::from_args(&["--nir", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_nir);
    assert!(!opts.show_wir);
}

#[test]
fn dump_tir_resolved() {
    let parser = Parser::from_args(&["--tir-resolved", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_tir_resolved);
    assert!(!opts.show_nir_lowered);
}

#[test]
fn dump_nir_lowered() {
    let parser = Parser::from_args(&["--nir-lowered", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_nir_lowered);
    assert!(!opts.show_tir_monomorphized);
}

#[test]
fn dump_wir() {
    let parser = Parser::from_args(&["--wir", "input.wado"]);
    let opts = wado_cli::dump::parse_args(parser).unwrap();
    assert!(opts.show_wir);
    assert!(!opts.show_nir);
}

#[test]
fn dump_no_input() {
    let parser = Parser::from_args::<&[&str]>(&[]);
    assert_err(
        wado_cli::dump::parse_args(parser),
        "no input file specified",
    );
}

#[test]
fn dump_help() {
    let parser = Parser::from_args(&["--help"]);
    assert_help(wado_cli::dump::parse_args(parser), "Usage: wado dump");
}

// ---- syntax ----

#[test]
fn syntax_default() {
    let parser = Parser::from_args::<&[&str]>(&[]);
    let opts = wado_cli::syntax::parse_args(parser).unwrap();
    assert_eq!(opts.format, wado_cli::syntax::SyntaxFormat::TmLanguage);
    assert_eq!(opts.output, None);
}

#[test]
fn syntax_with_format() {
    let parser = Parser::from_args(&["--format", "language-config"]);
    let opts = wado_cli::syntax::parse_args(parser).unwrap();
    assert_eq!(opts.format, wado_cli::syntax::SyntaxFormat::LanguageConfig);
}

#[test]
fn syntax_with_output() {
    let parser = Parser::from_args(&["-o", "out.json"]);
    let opts = wado_cli::syntax::parse_args(parser).unwrap();
    assert_eq!(opts.output, Some("out.json".to_string()));
}

#[test]
fn syntax_help() {
    let parser = Parser::from_args(&["--help"]);
    assert_help(wado_cli::syntax::parse_args(parser), "Usage: wado syntax");
}

#[test]
fn syntax_unexpected_positional() {
    let parser = Parser::from_args(&["foo"]);
    assert_err(wado_cli::syntax::parse_args(parser), "unexpected argument");
}

// ---- args helpers ----

#[test]
fn match_opt_long() {
    use wado_cli::args::{OptSpec, match_opt};

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TestOpt {
        Help,
        Verbose,
    }

    let specs: &[TestOpt] = &[TestOpt::Help, TestOpt::Verbose];
    let spec_fn = |o: &TestOpt| -> OptSpec {
        match o {
            TestOpt::Help => OptSpec {
                long: Some("help"),
                short: None,
                value: None,
                desc: "",
            },
            TestOpt::Verbose => OptSpec {
                long: Some("verbose"),
                short: Some('v'),
                value: None,
                desc: "",
            },
        }
    };

    let arg = lexopt::Arg::Long("help");
    assert_eq!(match_opt(&arg, specs, spec_fn), Some(TestOpt::Help));

    let arg = lexopt::Arg::Long("verbose");
    assert_eq!(match_opt(&arg, specs, spec_fn), Some(TestOpt::Verbose));

    let arg = lexopt::Arg::Long("unknown");
    assert_eq!(match_opt(&arg, specs, spec_fn), None);
}

#[test]
fn match_opt_short() {
    use wado_cli::args::{OptSpec, match_opt};

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TestOpt {
        Output,
    }

    let specs: &[TestOpt] = &[TestOpt::Output];
    let spec_fn = |_: &TestOpt| -> OptSpec {
        OptSpec {
            long: Some("output"),
            short: Some('o'),
            value: Some("<file>"),
            desc: "",
        }
    };

    let arg = lexopt::Arg::Short('o');
    assert_eq!(match_opt(&arg, specs, spec_fn), Some(TestOpt::Output));

    let arg = lexopt::Arg::Short('x');
    assert_eq!(match_opt(&arg, specs, spec_fn), None);
}

#[test]
fn format_opts_help_output() {
    use wado_cli::args::{OptSpec, format_opts_help};

    #[derive(Clone, Copy)]
    enum TestOpt {
        Help,
        Verbose,
    }

    let specs: &[TestOpt] = &[TestOpt::Help, TestOpt::Verbose];
    let spec_fn = |o: &TestOpt| -> OptSpec {
        match o {
            TestOpt::Help => OptSpec {
                long: Some("help"),
                short: None,
                value: None,
                desc: "Show help",
            },
            TestOpt::Verbose => OptSpec {
                long: Some("verbose"),
                short: Some('v'),
                value: None,
                desc: "Be verbose",
            },
        }
    };

    let output = format_opts_help(specs, spec_fn);
    assert!(output.contains("--help"), "should contain --help");
    assert!(
        output.contains("-v, --verbose"),
        "should contain -v, --verbose"
    );
    assert!(output.contains("Show help"), "should contain description");
}

#[test]
fn opt_level_maps_to_wasmtime_cranelift() {
    // Wado -O0 disables Cranelift codegen optimizations. -O1/-O2/-O3 all
    // collapse to wasmtime's `Speed` (wasmtime has no finer-grained level).
    // -Os hits `SpeedAndSize`, matching `wasmtime` CLI's own `-O s` mapping.
    use wasmtime::OptLevel as W;
    assert_eq!(OptLevel::O0.to_wasmtime(), W::None);
    assert_eq!(OptLevel::O1.to_wasmtime(), W::Speed);
    assert_eq!(OptLevel::O2.to_wasmtime(), W::Speed);
    assert_eq!(OptLevel::O3.to_wasmtime(), W::Speed);
    assert_eq!(OptLevel::Os.to_wasmtime(), W::SpeedAndSize);
}

#[test]
fn parse_log_level_valid() {
    use wado_cli::args::parse_log_level;
    assert_eq!(
        parse_log_level("debug"),
        Some(wado_compiler::LogLevel::Debug)
    );
    assert_eq!(parse_log_level("info"), Some(wado_compiler::LogLevel::Info));
    assert_eq!(parse_log_level("warn"), Some(wado_compiler::LogLevel::Warn));
    assert_eq!(
        parse_log_level("error"),
        Some(wado_compiler::LogLevel::Error)
    );
    assert_eq!(parse_log_level("off"), Some(wado_compiler::LogLevel::Off));
    assert_eq!(parse_log_level("invalid"), None);
}
