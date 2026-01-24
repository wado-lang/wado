use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::{Long, Short, Value};
use wado_compiler::LogLevel;

use crate::args::{
    next_arg, reject_multiple_inputs, require_input, require_string, unexpected_arg,
};
use crate::compiler_host::FilesystemCompilerHost;

/// Optimization level
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    /// No optimizations. Used for debugging.
    O0,
    /// Baseline optimizations including DCE. Intended for development (default).
    #[default]
    O1,
    /// All optimizations. Intended for production (server-side).
    O2,
    /// Same as O2 (reserved for future use).
    O3,
    /// O2 plus name section stripping. Intended for frontend.
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
}

pub fn print_usage() {
    eprintln!("Usage: wado compile [options] <file.wado>");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -o <file>        Output file path (default: <input>.wasm)");
    eprintln!("  --format <fmt>   Output format: wasm, wat (default: guessed from -o extension)");
    eprintln!(
        "  --wat-to-stdout  Output WAT to stdout (shorthand for --format wat -o /dev/stdout)"
    );
    eprintln!("  -O<n>            Optimization level: -O0, -O1, -O2, -O3, -Os");
    eprintln!("  --log-level <l>  Log level: debug, info, warn, error, off (default: info)");
    eprintln!("  --help           Show this help message");
}

/// Parse log level from string
fn parse_log_level(s: &str) -> Option<LogLevel> {
    match s.to_lowercase().as_str() {
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        "off" | "none" => Some(LogLevel::Off),
        _ => None,
    }
}

pub fn parse_args(mut parser: lexopt::Parser) -> CompileOptions {
    let mut output: Option<String> = None;
    let mut format: Option<OutputFormat> = None;
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut wat_to_stdout = false;
    let mut log_level = LogLevel::default();

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("wat-to-stdout") => {
                wat_to_stdout = true;
            }
            Short('o') => {
                output = Some(require_string(&mut parser));
            }
            Short('O') => {
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
            Long("format") => {
                let fmt_str = require_string(&mut parser);
                if let Some(f) = OutputFormat::from_str(&fmt_str) {
                    format = Some(f);
                } else {
                    eprintln!("Error: unknown format '{fmt_str}'. Use 'wasm' or 'wat'");
                    process::exit(1);
                }
            }
            Long("log-level") => {
                let level_str = require_string(&mut parser);
                if let Some(level) = parse_log_level(&level_str) {
                    log_level = level;
                } else {
                    eprintln!(
                        "Error: unknown log level '{level_str}'. Use debug, info, warn, error, or off"
                    );
                    process::exit(1);
                }
            }
            Value(val) => {
                reject_multiple_inputs(&input);
                input = Some(val.to_string_lossy().into_owned());
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    CompileOptions {
        input: require_input(input, print_usage),
        output,
        format,
        opt_level,
        wat_to_stdout,
        log_level,
    }
}

/// Convert CLI `OptLevel` to compiler `OptLevel`
fn to_compiler_opt_level(level: OptLevel) -> wado_compiler::OptLevel {
    match level {
        OptLevel::O0 => wado_compiler::OptLevel::None,
        OptLevel::O1 => wado_compiler::OptLevel::Basic,
        OptLevel::O2 | OptLevel::O3 => wado_compiler::OptLevel::Full,
        OptLevel::Os => wado_compiler::OptLevel::Size,
    }
}

/// Compile a Wado source file and return the Wasm binary
pub async fn compile(filename: &str) -> Vec<u8> {
    compile_with_opts(filename, OptLevel::default(), LogLevel::default()).await
}

/// Compile a Wado source file with optimization options
pub async fn compile_with_opts(filename: &str, opt_level: OptLevel, log_level: LogLevel) -> Vec<u8> {
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

    // Compile using async API
    let compiler_opt_level = to_compiler_opt_level(opt_level);
    let result =
        wado_compiler::compile_with_host(&source, &host, Some(filename), compiler_opt_level).await;

    match result {
        Ok(result) => result.wasm,
        Err(e) => {
            eprintln!("{e}");
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
    let wasm = compile_with_opts(&opts.input, opts.opt_level, opts.log_level).await;

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
