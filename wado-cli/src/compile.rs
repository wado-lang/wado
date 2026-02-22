use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use wado_compiler::LogLevel;

use crate::args::{
    self, next_arg, reject_multiple_inputs, require_input, require_string, unexpected_arg,
};
use crate::compiler_host::FilesystemCompilerHost;

/// Optimization level
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    /// No optimizations. Used for debugging.
    O0,
    /// Development optimizations. All passes except DCE.
    /// Keeps dead code visible for debugging while improving runtime.
    /// Iterations: 2, Inline threshold: 10.
    O1,
    /// Production optimizations. Full passes including DCE (default).
    /// Iterations: 10, Inline threshold: 10.
    #[default]
    O2,
    /// Aggressive production optimizations. Full passes including DCE.
    /// Iterations: 100, Inline threshold: 20.
    O3,
    /// Size optimizations. O2 plus name section stripping.
    Os,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Wasm,
    Wat,
}

impl OutputFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "wasm" => Some(OutputFormat::Wasm),
            "wat" => Some(OutputFormat::Wat),
            _ => None,
        }
    }

    fn from_extension(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| match ext {
                "wasm" => Some(OutputFormat::Wasm),
                "wat" => Some(OutputFormat::Wat),
                _ => None,
            })
    }
}

pub struct CompileOptions {
    pub input: String,
    pub output: Option<String>,
    pub format: Option<OutputFormat>,
    pub opt_level: OptLevel,
    pub wat_to_stdout: bool,
    pub log_level: LogLevel,
    pub target_world: Option<String>,
    pub skip_validation: bool,
}

#[derive(Clone, Copy)]
#[allow(clippy::enum_variant_names)]
enum Opt {
    Output,
    Format,
    WatToStdout,
    World,
    OptLevel,
    LogLevel,
    NoValidate,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Output,
        Self::Format,
        Self::WatToStdout,
        Self::World,
        Self::OptLevel,
        Self::LogLevel,
        Self::NoValidate,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Output => args::OptSpec {
                long: None,
                short: Some('o'),
                value: Some("<file>"),
                desc: "Output file path (default: <input>.wasm)",
            },
            Self::Format => args::OptSpec {
                long: Some("format"),
                short: None,
                value: Some("<fmt>"),
                desc: "Output format: wasm, wat (default: guessed from -o extension)",
            },
            Self::WatToStdout => args::OptSpec {
                long: Some("wat-to-stdout"),
                short: None,
                value: None,
                desc: "Output WAT to stdout (shorthand for --format wat -o /dev/stdout)",
            },
            Self::World => args::OptSpec {
                long: Some("world"),
                short: None,
                value: Some("<name>"),
                desc: "Target world (default: wasi:cli/command)\nUse 'test' to export test functions only",
            },
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::NoValidate => args::OptSpec {
                long: Some("no-validate"),
                short: None,
                value: None,
                desc: "Skip Wasm validation (output raw bytes even if invalid)",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

pub fn print_usage() {
    eprintln!("Usage: wado compile [options] <file.wado>");
    eprintln!();
    eprintln!("Compile a Wado source file to WebAssembly.");
    eprintln!();
    eprintln!("Options:");
    args::print_opts_help(Opt::ALL, |o| o.spec());
}

pub fn parse_args(mut parser: lexopt::Parser) -> CompileOptions {
    let mut output: Option<String> = None;
    let mut format: Option<OutputFormat> = None;
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut wat_to_stdout = false;
    let mut log_level = LogLevel::default();
    let mut target_world: Option<String> = None;
    let mut skip_validation = false;
    while let Some(arg) = next_arg(&mut parser) {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Output => output = Some(require_string(&mut parser)),
                Opt::Format => {
                    let fmt_str = require_string(&mut parser);
                    if let Some(f) = OutputFormat::from_str(&fmt_str) {
                        format = Some(f);
                    } else {
                        eprintln!("Error: unknown format '{fmt_str}'. Use 'wasm' or 'wat'");
                        process::exit(1);
                    }
                }
                Opt::WatToStdout => wat_to_stdout = true,
                Opt::World => target_world = Some(require_string(&mut parser)),
                Opt::OptLevel => {
                    let val = parser.optional_value();
                    let level_str = val
                        .as_ref()
                        .map(|v| v.to_string_lossy())
                        .unwrap_or_default();
                    opt_level = match level_str.as_ref() {
                        "" | "0" | "g" => OptLevel::O0,
                        "1" => OptLevel::O1,
                        "2" => OptLevel::O2,
                        "3" => OptLevel::O3,
                        "s" => OptLevel::Os,
                        _ => {
                            eprintln!(
                                "Error: unknown optimization level '-O{level_str}'. Use -O0, -O1, -O2, -O3, -Os, or -Og"
                            );
                            process::exit(1);
                        }
                    };
                }
                Opt::LogLevel => log_level = args::parse_log_level_arg(&mut parser),
                Opt::NoValidate => skip_validation = true,
                Opt::Help => {
                    print_usage();
                    process::exit(0);
                }
            }
        } else if let Value(val) = arg {
            reject_multiple_inputs(&input);
            input = Some(val.to_string_lossy().into_owned());
        } else {
            unexpected_arg(arg, print_usage);
        }
    }

    CompileOptions {
        input: require_input(input, print_usage),
        output,
        format,
        opt_level,
        wat_to_stdout,
        log_level,
        target_world,
        skip_validation,
    }
}

/// Convert CLI `OptLevel` to compiler `OptLevel`
fn to_compiler_opt_level(level: OptLevel) -> wado_compiler::OptLevel {
    match level {
        OptLevel::O0 => wado_compiler::OptLevel::O0,
        OptLevel::O1 => wado_compiler::OptLevel::O1,
        OptLevel::O2 => wado_compiler::OptLevel::O2,
        OptLevel::O3 => wado_compiler::OptLevel::O3,
        OptLevel::Os => wado_compiler::OptLevel::Os,
    }
}

/// Compile a Wado source file with optimization options
pub async fn compile_with_opts(
    filename: &str,
    opt_level: OptLevel,
    log_level: LogLevel,
) -> Vec<u8> {
    compile_with_full_opts(filename, opt_level, log_level, None, false).await
}

/// Compile a Wado source file with full options including target world
pub async fn compile_with_full_opts(
    filename: &str,
    opt_level: OptLevel,
    log_level: LogLevel,
    target_world: Option<String>,
    skip_validation: bool,
) -> Vec<u8> {
    let path = Path::new(filename);

    // Read source file
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {e}", path.display());
            process::exit(1);
        }
    };

    // Get base path for relative imports
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::with_log_level(base_path, log_level);

    // Build compiler options
    let options = wado_compiler::CompilerOptions {
        opt_level: to_compiler_opt_level(opt_level),
        target_world,
        skip_validation,
    };

    // Compile using async API
    let result = wado_compiler::compile_with_options(&source, &host, Some(filename), options).await;

    match result {
        Ok(result) => result.wasm,
        Err(_bail) => {
            // Errors already printed by host via emit_diagnostic
            process::exit(1);
        }
    }
}

/// Convert Wasm binary to WAT text format (folded style)
fn wasm_to_wat(wasm: &[u8]) -> String {
    let mut config = wasmprinter::Config::new();
    config.fold_instructions(true);
    let mut wat = String::new();
    config
        .print(wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .unwrap_or_else(|e| {
            eprintln!("Error generating WAT: {e}");
            process::exit(1);
        });
    wat
}

pub async fn run(opts: CompileOptions) {
    let wasm = compile_with_full_opts(
        &opts.input,
        opts.opt_level,
        opts.log_level,
        opts.target_world,
        opts.skip_validation,
    )
    .await;

    // Handle --wat-to-stdout: output WAT to stdout and return
    if opts.wat_to_stdout {
        let wat = wasm_to_wat(&wasm);
        print!("{wat}");
        return;
    }

    // Determine format: explicit > guessed from -o extension > default (wasm)
    let format = opts
        .format
        .or_else(|| {
            opts.output
                .as_ref()
                .and_then(|p| OutputFormat::from_extension(Path::new(p)))
        })
        .unwrap_or(OutputFormat::Wasm);

    // Determine output path, using format to pick extension if no -o specified
    let output_path = if let Some(path) = &opts.output {
        Path::new(path).to_path_buf()
    } else {
        let ext = match format {
            OutputFormat::Wasm => "wasm",
            OutputFormat::Wat => "wat",
        };
        Path::new(&opts.input).with_extension(ext)
    };

    match format {
        OutputFormat::Wasm => match fs::write(&output_path, &wasm) {
            Ok(()) => {
                eprintln!("Generated: {}", output_path.display());
            }
            Err(e) => {
                eprintln!("Error writing output file: {e}");
                process::exit(1);
            }
        },
        OutputFormat::Wat => {
            let wat = wasm_to_wat(&wasm);
            match fs::write(&output_path, &wat) {
                Ok(()) => {
                    eprintln!("Generated: {}", output_path.display());
                }
                Err(e) => {
                    eprintln!("Error writing output file: {e}");
                    process::exit(1);
                }
            }
        }
    }
}
