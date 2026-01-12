use std::fs;
use std::path::Path;
use std::process;

use lexopt::prelude::*;

/// Optimization level (not yet implemented)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    #[default]
    O0,
    O1,
    O2,
    O3,
    Os,
    Og,
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
    #[allow(dead_code)] // Not yet implemented
    pub opt_level: OptLevel,
    pub wat_to_stdout: bool,
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
    eprintln!(
        "  -O<n>            Optimization level: -O0, -O1, -O2, -O3, -Os, -Og (not yet implemented)"
    );
    eprintln!("  --help           Show this help message");
}

pub fn parse_args(mut parser: lexopt::Parser) -> CompileOptions {
    let mut output: Option<String> = None;
    let mut format: Option<OutputFormat> = None;
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut wat_to_stdout = false;

    while let Some(arg) = parser.next().unwrap_or_else(|e| {
        eprintln!("Error: {e}");
        process::exit(1);
    }) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("wat-to-stdout") => {
                wat_to_stdout = true;
            }
            Short('o') => {
                let val = parser.value().unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    process::exit(1);
                });
                output = Some(val.to_string_lossy().into_owned());
            }
            Short('O') => {
                let val = parser.optional_value();
                let level_str = val
                    .as_ref()
                    .map(|v| v.to_string_lossy())
                    .unwrap_or_default();
                opt_level = match level_str.as_ref() {
                    "" | "0" => OptLevel::O0,
                    "1" => OptLevel::O1,
                    "2" => OptLevel::O2,
                    "3" => OptLevel::O3,
                    "s" => OptLevel::Os,
                    "g" => OptLevel::Og,
                    _ => {
                        eprintln!(
                            "Error: unknown optimization level '-O{level_str}'. Use -O0, -O1, -O2, -O3, -Os, or -Og"
                        );
                        process::exit(1);
                    }
                };
            }
            Long("format") => {
                let val = parser.value().unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    process::exit(1);
                });
                let fmt_str = val.to_string_lossy();
                match OutputFormat::from_str(&fmt_str) {
                    Some(f) => format = Some(f),
                    None => {
                        eprintln!("Error: unknown format '{fmt_str}'. Use 'wasm' or 'wat'");
                        process::exit(1);
                    }
                }
            }
            Value(val) => {
                if input.is_some() {
                    eprintln!("Error: multiple input files not supported");
                    process::exit(1);
                }
                input = Some(val.to_string_lossy().into_owned());
            }
            _ => {
                eprintln!("Error: {}", arg.unexpected());
                print_usage();
                process::exit(1);
            }
        }
    }

    let input = match input {
        Some(f) => f,
        None => {
            eprintln!("Error: no input file specified");
            print_usage();
            process::exit(1);
        }
    };

    CompileOptions {
        input,
        output,
        format,
        opt_level,
        wat_to_stdout,
    }
}

/// Compile a Wado source file and return the Wasm binary
pub fn compile(filename: &str) -> Vec<u8> {
    match wado_compiler::compile_file(Path::new(filename)) {
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

pub fn run(opts: CompileOptions) {
    let wasm = compile(&opts.input);

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
    let output_path = match &opts.output {
        Some(path) => Path::new(path).to_path_buf(),
        None => {
            let ext = match format {
                OutputFormat::Wasm => "wasm",
                OutputFormat::Wat => "wat",
            };
            Path::new(&opts.input).with_extension(ext)
        }
    };

    match format {
        OutputFormat::Wasm => match fs::write(&output_path, &wasm) {
            Ok(_) => {
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
                Ok(_) => {
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
