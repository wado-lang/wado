use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use wado_compiler::OptLevel;

use crate::args::{self, CliExit};
use crate::compiler_host::FilesystemCompilerHost;

pub struct DumpOptions {
    pub inputs: Vec<String>,
    pub show_tokens: bool,
    pub show_ast: bool,
    pub show_desugared: bool,
    pub show_symbols: bool,
    pub show_modules: bool,
    pub show_types: bool,
    pub show_tir: bool,
    pub show_tir_resolved: bool,
    pub show_tir_monomorphized: bool,
    pub show_tir_lowered: bool,
    pub show_wir: bool,
    pub opt_level: OptLevel,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
}

#[derive(Clone, Copy)]
enum Opt {
    Tokens,
    Ast,
    Desugared,
    Modules,
    Symbols,
    Types,
    Tir,
    TirResolved,
    TirMonomorphized,
    TirLowered,
    Wir,
    OptLevel,
    InlineThreshold,
    OptIterations,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Tokens,
        Self::Ast,
        Self::Desugared,
        Self::Modules,
        Self::Symbols,
        Self::Types,
        Self::Tir,
        Self::TirResolved,
        Self::TirMonomorphized,
        Self::TirLowered,
        Self::Wir,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Tokens => args::OptSpec {
                long: Some("tokens"),
                short: None,
                value: None,
                desc: "Show lexer tokens",
            },
            Self::Ast => args::OptSpec {
                long: Some("ast"),
                short: None,
                value: None,
                desc: "Show parsed AST",
            },
            Self::Desugared => args::OptSpec {
                long: Some("desugared"),
                short: None,
                value: None,
                desc: "Show desugared AST",
            },
            Self::Modules => args::OptSpec {
                long: Some("modules"),
                short: None,
                value: None,
                desc: "Show loaded modules",
            },
            Self::Symbols => args::OptSpec {
                long: Some("symbols"),
                short: None,
                value: None,
                desc: "Show symbol table",
            },
            Self::Types => args::OptSpec {
                long: Some("types"),
                short: None,
                value: None,
                desc: "Show type table (all resolved types)",
            },
            Self::Tir => args::OptSpec {
                long: Some("tir"),
                short: None,
                value: None,
                desc: "Show final TIR (after optimization, affected by -Ox)",
            },
            Self::TirResolved => args::OptSpec {
                long: Some("tir-resolved"),
                short: None,
                value: None,
                desc: "Show TIR after type resolution (before lowering)",
            },
            Self::TirMonomorphized => args::OptSpec {
                long: Some("tir-monomorphized"),
                short: None,
                value: None,
                desc: "Show TIR after monomorphization",
            },
            Self::TirLowered => args::OptSpec {
                long: Some("tir-lowered"),
                short: None,
                value: None,
                desc: "Show TIR after lowering (before optimization)",
            },
            Self::Wir => args::OptSpec {
                long: Some("wir"),
                short: None,
                value: None,
                desc: "Show final WIR (after optimization, affected by -Ox) [default]",
            },
            Self::OptLevel => args::OptSpec {
                long: None,
                short: Some('O'),
                value: Some("<n>"),
                desc: "Optimization level (affects --tir and --wir output)",
            },
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(
        buf,
        "Usage: wado dump [options] <file.wado> [file2.wado ...]"
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Dump compiler internal state for debugging.").unwrap();
    writeln!(
        buf,
        "Default: shows final WIR as unparsed source (equivalent to --wir)."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Phases:").unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(
            &[
                Opt::Tokens,
                Opt::Ast,
                Opt::Desugared,
                Opt::Modules,
                Opt::Symbols,
                Opt::Types,
                Opt::TirResolved,
                Opt::TirMonomorphized,
                Opt::TirLowered,
                Opt::Tir,
                Opt::Wir,
            ],
            |o| o.spec(),
        )
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Optimization Level:").unwrap();
    writeln!(buf, "  -O0          No optimizations").unwrap();
    writeln!(
        buf,
        "  -O1          Development optimizations (all passes except DCE)"
    )
    .unwrap();
    writeln!(buf, "  -O2          Production optimizations (default)").unwrap();
    writeln!(buf, "  -O3          Aggressive optimizations").unwrap();
    writeln!(buf, "  -Os          Size optimizations (O2 + strip names)").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Other:").unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(&[Opt::Help], |o| o.spec())
    )
    .unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse command-line arguments for the `dump` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<DumpOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut show_tokens = false;
    let mut show_ast = false;
    let mut show_desugared = false;
    let mut show_symbols = false;
    let mut show_modules = false;
    let mut show_types = false;
    let mut show_tir = false;
    let mut show_tir_resolved = false;
    let mut show_tir_monomorphized = false;
    let mut show_tir_lowered = false;
    let mut show_wir = false;
    let mut opt_level = OptLevel::O2;
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut any_phase = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Tokens => {
                    show_tokens = true;
                    any_phase = true;
                }
                Opt::Ast => {
                    show_ast = true;
                    any_phase = true;
                }
                Opt::Desugared => {
                    show_desugared = true;
                    any_phase = true;
                }
                Opt::Symbols => {
                    show_symbols = true;
                    any_phase = true;
                }
                Opt::Modules => {
                    show_modules = true;
                    any_phase = true;
                }
                Opt::Types => {
                    show_types = true;
                    any_phase = true;
                }
                Opt::Tir => {
                    show_tir = true;
                    any_phase = true;
                }
                Opt::TirResolved => {
                    show_tir_resolved = true;
                    any_phase = true;
                }
                Opt::TirMonomorphized => {
                    show_tir_monomorphized = true;
                    any_phase = true;
                }
                Opt::TirLowered => {
                    show_tir_lowered = true;
                    any_phase = true;
                }
                Opt::Wir => {
                    show_wir = true;
                    any_phase = true;
                }
                Opt::OptLevel => {
                    let level = args::require_value(&mut parser)
                        .map_err(|_| CliExit::error("-O requires a level (0, 1, 2, 3, or s)"))?;
                    let level_str = level.to_string_lossy();
                    opt_level = match level_str.as_ref() {
                        "0" => OptLevel::O0,
                        "1" => OptLevel::O1,
                        "2" => OptLevel::O2,
                        "3" => OptLevel::O3,
                        "s" => OptLevel::Os,
                        _ => {
                            return Err(CliExit::error(format!(
                                "Unknown optimization level: -O{level_str}\nValid levels: -O0, -O1, -O2, -O3, -Os"
                            )));
                        }
                    };
                }
                Opt::InlineThreshold => {
                    inline_threshold = Some(args::parse_inline_threshold_arg(
                        "--optimize-inline-threshold",
                        &mut parser,
                    )?);
                }
                Opt::OptIterations => {
                    opt_iterations = Some(args::parse_opt_iterations_arg(
                        "--optimize-iterations",
                        &mut parser,
                    )?);
                }
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let inputs = args::require_inputs(inputs, &usage)?;

    // Default: show WIR
    if !any_phase {
        show_wir = true;
    }

    Ok(DumpOptions {
        inputs,
        show_tokens,
        show_ast,
        show_desugared,
        show_symbols,
        show_modules,
        show_types,
        show_tir,
        show_tir_resolved,
        show_tir_monomorphized,
        show_tir_lowered,
        show_wir,
        opt_level,
        inline_threshold,
        opt_iterations,
    })
}

pub async fn run(opts: DumpOptions) {
    let multiple_files = opts.inputs.len() > 1;

    for input in &opts.inputs {
        if multiple_files {
            println!("// ========== {input} ==========");
        }

        run_single(&opts, input).await;
    }
}

async fn run_single(opts: &DumpOptions, input: &str) {
    let path = Path::new(input);

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
    let host = FilesystemCompilerHost::new(base_path);

    // Extract target world from __DATA__ section if present
    let target_world = extract_world_from_data_section(&source);

    // Dump using async API with target world
    let result = match wado_compiler::dump_with_host_and_world(
        &source,
        &host,
        Some(input),
        opts.opt_level,
        target_world.as_deref(),
        opts.inline_threshold,
        opts.opt_iterations,
    )
    .await
    {
        Ok(r) => r,
        Err(_bail) => {
            // Errors already printed by host via emit_diagnostic
            process::exit(1);
        }
    };

    // Tokens section (Lexer phase)
    if opts.show_tokens {
        println!("=== Tokens ===");
        for (i, token) in result.tokens.iter().enumerate() {
            println!("  [{i}] {token:?}");
        }
        println!();
    }

    // AST section (Parser phase)
    if opts.show_ast {
        println!("=== AST ===");
        let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
        let unparsed = unparser.unparse(&result.ast);
        println!("{unparsed}");
        println!();
    }

    // Desugared AST section
    if opts.show_desugared {
        println!("=== Desugared AST ===");
        let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
        let unparsed = unparser.unparse(&result.desugared_ast);
        println!("{unparsed}");
        println!();
    }

    // Modules section
    if opts.show_modules {
        println!("=== Loaded Modules ===");
        for module_source in &result.loaded_modules {
            let path_str = module_source.to_string();
            let is_implicit = result.implicit_modules.contains(module_source);
            if is_implicit {
                println!("  {path_str} (implicit)");
            } else {
                println!("  {path_str}");
            }
        }
        println!();

        println!("=== Implicit Modules ===");
        for module_source in &result.implicit_modules {
            let path_str = module_source.to_string();
            println!("  {path_str}");
        }
        println!();
    }

    // Symbols section
    if opts.show_symbols {
        println!("=== Symbol Table ===");
        for symbol in result.symbols.all_symbols() {
            let module_path = symbol.module_source.to_string();
            let kind_str = match &symbol.kind {
                wado_compiler::symbol::SymbolKind::Function(f) => {
                    let effects = if f.effects.is_empty() {
                        String::new()
                    } else {
                        format!(" with {}", f.effects.join(", "))
                    };
                    let ret = f
                        .return_type
                        .as_ref()
                        .map(|t| format!(" -> {t}"))
                        .unwrap_or_default();
                    let wasi = f
                        .cm_import
                        .as_ref()
                        .map(|w| format!(" [cm: {}]", w.interface_path()))
                        .unwrap_or_default();
                    format!("fn({}){ret}{effects}{wasi}", f.params.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Effect(e) => {
                    let wasi = e
                        .cm_import
                        .as_ref()
                        .map(|w| format!(" [cm: {}]", w.interface_path()))
                        .unwrap_or_default();
                    format!("effect{{ {} }}{wasi}", e.methods.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Struct(s) => {
                    format!("struct{{ {} }}", s.fields.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Enum(e) => {
                    format!("enum{{ {} }}", e.cases.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Flags(f) => {
                    format!("flags{{ {} }}", f.members.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Variant(v) => {
                    format!("variant{{ {} }}", v.cases.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Newtype(t) => {
                    format!("type = {}", t.aliased_type)
                }
                wado_compiler::symbol::SymbolKind::Variable(v) => {
                    let mut flags = Vec::new();
                    if v.is_mut {
                        flags.push("mut");
                    }
                    if v.is_reactive {
                        flags.push("reactive");
                    }
                    if flags.is_empty() {
                        "var".to_string()
                    } else {
                        format!("var({})", flags.join(", "))
                    }
                }
                wado_compiler::symbol::SymbolKind::Resource(r) => {
                    let wasi = r
                        .cm_import
                        .as_ref()
                        .map(|w| format!(" [cm: {}]", w.interface_path()))
                        .unwrap_or_default();
                    format!("resource{{ {} }}{wasi}", r.methods.join(", "))
                }
                wado_compiler::symbol::SymbolKind::World(w) => {
                    let imports: Vec<_> = w.imports.iter().map(|i| i.effect_name.clone()).collect();
                    let exports: Vec<_> = w.exports.iter().map(|e| e.name.clone()).collect();
                    format!(
                        "world{{ imports: [{}], exports: [{}] }}",
                        imports.join(", "),
                        exports.join(", ")
                    )
                }
                wado_compiler::symbol::SymbolKind::Trait(t) => {
                    format!("trait{{ {} }}", t.methods.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Global(g) => {
                    if g.is_mut {
                        "global mut".to_string()
                    } else {
                        "global".to_string()
                    }
                }
            };
            println!(
                "  [{}] {} :: {} = {}",
                symbol.id.0, module_path, symbol.name, kind_str
            );
        }
        println!();
    }

    // Types section (after type resolution)
    if opts.show_types {
        if let Some(ref tir_modules) = result.tir_modules {
            // Type table is shared across modules; grab from the first one
            if let Some((_, first_module)) = tir_modules.iter().next() {
                let type_table = first_module.type_table.borrow();
                println!("=== Types ===");
                for (id, ty) in type_table.all_types() {
                    let name = type_table.type_name(*id);
                    let kind = match ty {
                        wado_compiler::tir::ResolvedType::Primitive(_) => "primitive",
                        wado_compiler::tir::ResolvedType::Unit => "unit",
                        wado_compiler::tir::ResolvedType::Never => "never",
                        wado_compiler::tir::ResolvedType::Struct { .. } => "struct",
                        wado_compiler::tir::ResolvedType::Enum { .. } => "enum",
                        wado_compiler::tir::ResolvedType::Variant { .. } => "variant",
                        wado_compiler::tir::ResolvedType::Newtype { .. } => "newtype",
                        wado_compiler::tir::ResolvedType::Resource { .. }
                        | wado_compiler::tir::ResolvedType::GenericResource { .. } => "resource",
                        wado_compiler::tir::ResolvedType::Ref(_) => "ref",
                        wado_compiler::tir::ResolvedType::MutRef(_) => "mut_ref",
                        wado_compiler::tir::ResolvedType::Function { .. } => "fn",
                        wado_compiler::tir::ResolvedType::BuiltinArray(_) => "builtin_array",
                        wado_compiler::tir::ResolvedType::Reactive(_) => "reactive",
                        wado_compiler::tir::ResolvedType::TypeParam { .. } => "type_param",
                        wado_compiler::tir::ResolvedType::GenericInstance { .. } => {
                            "generic_instance"
                        }
                        wado_compiler::tir::ResolvedType::AssocTypeProjection { .. } => {
                            "assoc_type"
                        }
                        wado_compiler::tir::ResolvedType::Flags { .. } => "flags",
                        wado_compiler::tir::ResolvedType::TypePack { .. } => "type_pack",
                        wado_compiler::tir::ResolvedType::Unknown => "unknown",
                        wado_compiler::tir::ResolvedType::Error => "error",
                    };
                    println!("  [{id}] {kind}: {name}");
                }
            }
        } else {
            println!("=== Types ===");
            println!("(Type resolution failed or not available)");
        }
        println!();
    }

    // TIR resolved section (after type resolution, before lowering)
    if opts.show_tir_resolved {
        if let Some(ref tir_modules) = result.tir_modules {
            println!("=== TIR Resolved ===");
            for (module_source, module) in tir_modules {
                let path_str = module_source.to_string();
                println!("// --- Module: {path_str} ---");
                let unparsed = wado_compiler::unparse::unparse_tir(module);
                println!("{unparsed}");
            }
        } else {
            println!("=== TIR Resolved ===");
            println!("(TIR resolution failed or not available)");
            println!();
        }
    }

    // TIR monomorphized section
    if opts.show_tir_monomorphized {
        if let Some(ref text) = result.monomorphized_tir_text {
            println!("=== TIR Monomorphized ===");
            println!("{text}");
        } else {
            println!("=== TIR Monomorphized ===");
            println!("(Monomorphization failed or not available)");
            println!();
        }
    }

    // TIR lowered section (after lowering, before optimization)
    if opts.show_tir_lowered {
        if let Some(ref text) = result.lowered_tir_text {
            println!("=== TIR Lowered ===");
            println!("{text}");
        } else {
            println!("=== TIR Lowered ===");
            println!("(TIR lowering failed or not available)");
            println!();
        }
    }

    // Final TIR section (after optimization)
    if opts.show_tir {
        if let Some(ref project) = result.optimized_package {
            println!("=== TIR ===");
            let unparsed = wado_compiler::unparse::unparse_flat_package(project);
            println!("{unparsed}");
            println!();
        } else {
            println!("=== TIR ===");
            println!("(Optimization failed or not available)");
            println!();
        }
    }

    // Final WIR section (after optimization)
    if opts.show_wir {
        if let Some(ref wir_package) = result.wir_package {
            let unparsed = wado_compiler::wir_unparse::unparse_wir(wir_package, None);
            if unparsed.is_empty() {
                println!("(empty module)");
            } else {
                print!("{unparsed}");
            }
            println!();
        } else {
            println!("=== WIR ===");
            println!("(WIR generation failed or not available)");
            println!();
        }
    }
}

/// Extract the target world from the `__DATA__` JSON section of a source file.
///
/// Supports two formats:
/// - Old: `{"world": "wasi:http/service", ...}`
/// - New: `{"wasi:http/service": {...}}` (world as top-level key containing `:` or `"test"`)
fn extract_world_from_data_section(source: &str) -> Option<String> {
    let marker = "\n__DATA__\n";
    let data = if let Some(pos) = source.find(marker) {
        &source[pos + marker.len()..]
    } else { source.strip_prefix("__DATA__\n")? };
    let json: serde_json::Value = serde_json::from_str(data.trim()).ok()?;
    // Old format: explicit "world" key
    if let Some(world) = json.get("world").and_then(|v| v.as_str()) {
        return Some(world.to_string());
    }
    // New format: world name is a top-level key (wasi:* prefix or "test")
    if let Some(obj) = json.as_object() {
        for key in obj.keys() {
            if key.starts_with("wasi:") || key == "test" {
                return Some(key.clone());
            }
        }
    }
    None
}
