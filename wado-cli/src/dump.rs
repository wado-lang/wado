use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};
use crate::compiler_host::FilesystemCompilerHost;
use crate::knobs::{CompileKnobs, KnobOpt};

pub struct DumpOptions {
    pub inputs: Vec<String>,
    pub show_tokens: bool,
    pub show_ast: bool,
    pub show_symbols: bool,
    pub show_modules: bool,
    pub show_types: bool,
    pub show_tir_resolved: bool,
    pub show_tir_monomorphized: bool,
    pub show_assert_plan: bool,
    pub show_nir_lowered: bool,
    pub show_nir: bool,
    pub show_wir: bool,
    pub knobs: CompileKnobs,
    /// `--world <name>`; `None` selects the default `wasi:cli/command`.
    pub target_world: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Tokens,
    Ast,
    Modules,
    Symbols,
    Types,
    TirResolved,
    TirMonomorphized,
    AssertPlan,
    NirLowered,
    Nir,
    Wir,
    World,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Tokens,
        Self::Ast,
        Self::Modules,
        Self::Symbols,
        Self::Types,
        Self::TirResolved,
        Self::TirMonomorphized,
        Self::AssertPlan,
        Self::NirLowered,
        Self::Nir,
        Self::Wir,
        Self::World,
        Self::Help,
    ];

    const KNOBS: &[KnobOpt] = &[
        KnobOpt::OptLevel,
        KnobOpt::InlineThreshold,
        KnobOpt::InlineGrowth,
        KnobOpt::OptIterations,
        KnobOpt::LogLevel,
        KnobOpt::Allocator,
        KnobOpt::NoCache,
        KnobOpt::Feature,
    ];

    /// Whether this names a pipeline stage, which help lists under `Phases:`.
    const fn is_phase(self) -> bool {
        match self {
            Self::Tokens
            | Self::Ast
            | Self::Modules
            | Self::Symbols
            | Self::Types
            | Self::TirResolved
            | Self::TirMonomorphized
            | Self::AssertPlan
            | Self::NirLowered
            | Self::Nir
            | Self::Wir => true,
            Self::World | Self::Help => false,
        }
    }

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
            Self::TirResolved => args::OptSpec {
                long: Some("tir-resolved"),
                short: None,
                value: None,
                desc: "Show TIR after type resolution (before lowering)",
            },
            Self::AssertPlan => args::OptSpec {
                long: Some("assert-plan"),
                short: None,
                value: None,
                desc: "Show which operands each `assert` captures",
            },
            Self::TirMonomorphized => args::OptSpec {
                long: Some("tir-monomorphized"),
                short: None,
                value: None,
                desc: "Show TIR after monomorphization",
            },
            Self::NirLowered => args::OptSpec {
                long: Some("nir-lowered"),
                short: None,
                value: None,
                desc: "Show NIR right after lowering (before optimization)",
            },
            Self::Nir => args::OptSpec {
                long: Some("nir"),
                short: None,
                value: None,
                desc: "Show final NIR (after optimization, affected by -Ox)",
            },
            Self::Wir => args::OptSpec {
                long: Some("wir"),
                short: None,
                value: None,
                desc: "Show final WIR (after optimization, affected by -Ox) [default]",
            },
            Self::World => args::WORLD_SPEC,
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
    let phases: Vec<Opt> = Opt::ALL.iter().copied().filter(|o| o.is_phase()).collect();
    write!(buf, "{}", args::format_opts_help(&phases, |o| o.spec())).unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Other:").unwrap();
    write!(
        buf,
        "{}",
        args::OptsHelp::default()
            .add(&[Opt::World, Opt::Help], |o| o.spec())
            .add(Opt::KNOBS, |o| o.spec())
            .add(args::ParamOpt::ALL, |o| o.spec())
            .render()
    )
    .unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<DumpOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut show_tokens = false;
    let mut show_ast = false;
    let mut show_symbols = false;
    let mut show_modules = false;
    let mut show_types = false;
    let mut show_assert_plan = false;
    let mut show_tir_resolved = false;
    let mut show_tir_monomorphized = false;
    let mut show_nir_lowered = false;
    let mut show_nir = false;
    let mut show_wir = false;
    let mut target_world: Option<String> = None;
    let mut any_phase = false;
    let mut knobs = CompileKnobs::default();

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(p) = args::match_opt(&arg, args::ParamOpt::ALL, |p| p.spec()) {
            knobs.params.apply(p, &mut parser)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Tokens => {
                    show_tokens = true;
                    any_phase = true;
                }
                Opt::Ast => {
                    show_ast = true;
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
                Opt::TirResolved => {
                    show_tir_resolved = true;
                    any_phase = true;
                }
                Opt::TirMonomorphized => {
                    show_tir_monomorphized = true;
                    any_phase = true;
                }
                Opt::AssertPlan => {
                    show_assert_plan = true;
                    any_phase = true;
                }
                Opt::NirLowered => {
                    show_nir_lowered = true;
                    any_phase = true;
                }
                Opt::Nir => {
                    show_nir = true;
                    any_phase = true;
                }
                Opt::Wir => {
                    show_wir = true;
                    any_phase = true;
                }
                Opt::World => target_world = Some(args::require_string(&mut parser)?),
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
        show_symbols,
        show_modules,
        show_types,
        show_tir_resolved,
        show_tir_monomorphized,
        show_assert_plan,
        show_nir_lowered,
        show_nir,
        show_wir,
        knobs,
        target_world,
    })
}

pub async fn run(opts: DumpOptions) -> Result<(), CliExit> {
    let multiple_files = opts.inputs.len() > 1;

    for input in &opts.inputs {
        if multiple_files {
            println!("// ========== {input} ==========");
        }

        run_single(&opts, input).await?;
    }
    Ok(())
}

async fn run_single(opts: &DumpOptions, input: &str) -> Result<(), CliExit> {
    let path = Path::new(input);

    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;

    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::with_log_level(base_path.clone(), opts.knobs.log_level);

    let manifest_pair = crate::compile::load_nearest_manifest(path);
    let host = crate::compile::attach_manifest_and_component_deps(
        host,
        manifest_pair.as_ref(),
        &base_path,
        &source,
        false,
    )
    .await
    .map_err(CliExit::error)?;

    // Run any Kiln generators the entry declares (`use x from "<schema>" with
    // { generator: ... }`), so the loader below redirects to their generated
    // output instead of falling through to the raw (non-Wado) schema file.
    let invocations =
        crate::compile::maybe_run_pipeline(path, &host, opts.knobs.no_cache, manifest_pair)
            .await
            .map_err(|e| CliExit::error(format!("wado dump: {e}")))?
            .invocations;

    let knobs = &opts.knobs;
    // Diagnostics are already emitted by the host; signal silently on failure.
    let result = wado_compiler::dump_with_host_and_world(
        &source,
        &host,
        Some(input),
        knobs.opt_level.to_compiler(),
        opts.target_world.as_deref(),
        knobs.allocator.as_deref(),
        knobs.opt,
        &knobs.codegen_flags,
        &knobs.params.overrides,
        knobs.params.policy,
        invocations,
    )
    .await
    .map_err(|_bail| CliExit::silent_failure(1))?;
    if opts.show_tokens {
        println!("=== Tokens ===");
        for (i, token) in result.tokens.iter().enumerate() {
            println!("  [{i}] {token:?}");
        }
        println!();
    }
    if opts.show_ast {
        println!("=== AST ===");
        let unparser = wado_compiler::unparse::Unparser::new().with_trivia(&result.trivia);
        let unparsed = unparser.unparse(&result.ast);
        println!("{unparsed}");
        println!();
    }
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
    if opts.show_symbols {
        println!("=== Symbol Table ===");
        for symbol in result.symbols.all_symbols() {
            let module_path = symbol.module_source().to_string();
            let kind_str = match &symbol.kind {
                wado_compiler::symbol::SymbolKind::Function(f) => {
                    // Same row shape the parser reads back: one effect bare,
                    // more than one parenthesized.
                    let effects = match f.effects.as_slice() {
                        [] => String::new(),
                        [only] => format!(" with {only}"),
                        many => format!(" with ({})", many.join(", ")),
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
                wado_compiler::symbol::SymbolKind::BuiltinType => "builtin type".to_string(),
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
                    let imports: Vec<_> =
                        w.imports.iter().map(|i| i.interface_name.clone()).collect();
                    let exports: Vec<_> = w
                        .exports
                        .iter()
                        .map(|e| match e {
                            wado_compiler::symbol::WorldExportSymbol::Interface {
                                interface_name,
                            } => interface_name.clone(),
                            wado_compiler::symbol::WorldExportSymbol::Function { name, .. } => {
                                name.clone()
                            }
                        })
                        .collect();
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
                "  [ast#{}] {} :: {} = {}",
                symbol.defined_at.local(),
                module_path,
                symbol.name,
                kind_str
            );
        }
        println!();
    }
    if opts.show_types {
        if let Some(ref tir_modules) = result.tir_modules {
            // Type table is shared across modules; grab from the first one
            if let Some((_, first_module)) = tir_modules.iter().next() {
                let type_table = first_module.type_table.borrow();
                println!("=== Types ===");
                for (id, ty) in type_table.all_types() {
                    let name = type_table.type_name(id);
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
                        wado_compiler::tir::ResolvedType::InferVar(_) => "infer_var",
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
    if opts.show_assert_plan {
        println!("=== Assert Plans ===");
        match result.assert_plan_text {
            Some(ref text) if !text.is_empty() => println!("{text}"),
            Some(_) => println!("(no assert in this program)"),
            None => println!("(elaboration failed or not available)"),
        }
    }
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
    if opts.show_nir_lowered {
        if let Some(ref text) = result.lowered_nir_text {
            println!("=== NIR Lowered ===");
            println!("{text}");
        } else {
            println!("=== NIR Lowered ===");
            println!("(NIR lowering failed or not available)");
            println!();
        }
    }
    if opts.show_nir {
        if let Some(ref project) = result.optimized_package {
            println!("=== NIR ===");
            let unparsed = wado_compiler::nir_unparse::unparse_nir_package(project);
            println!("{unparsed}");
            println!();
        } else {
            println!("=== NIR ===");
            println!("(Optimization failed or not available)");
            println!();
        }
    }
    if opts.show_wir {
        if let Some(ref wir_package) = result.wir_package {
            let unparsed = wado_compiler::wir_unparse::unparse_wir(wir_package);
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
    Ok(())
}
