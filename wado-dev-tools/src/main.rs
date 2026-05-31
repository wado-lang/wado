mod bump_version;
mod compiler_host;
mod data_section;
mod format_md;
mod pipeline;
mod template;

use lexopt::Arg::{Long, Short, Value};
use wado_compiler::OptLevel;

fn main() {
    let mut parser = lexopt::Parser::from_env();
    let cmd = match parser.next().expect("failed to parse args") {
        Some(Value(v)) => v.to_string_lossy().into_owned(),
        Some(other) => panic!(
            "expected subcommand as first argument, got {other:?} \
             (commands: golden-dump, wasm2wat, format-md, bump-version)"
        ),
        None => panic!("command is required (golden-dump, wasm2wat, format-md, bump-version)"),
    };
    match cmd.as_str() {
        "golden-dump" => golden_dump(parser),
        "wasm2wat" => wasm2wat(parser),
        "format-md" => format_md::run(parser),
        "bump-version" => bump_version::run(parser),
        other => panic!("unknown command: {other}"),
    }
}

fn golden_dump(mut parser: lexopt::Parser) {
    let mut in_template: Option<String> = None;
    let mut emits: Vec<pipeline::Emit> = Vec::new();
    let mut opt_level = OptLevel::O2;
    let mut skip_empty = false;

    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("in") => {
                in_template = Some(parser.value().unwrap().to_string_lossy().into_owned());
            }
            // One output per `--emit <phase>:<out-template>`. A single invocation
            // can render every phase for each fixture, which keeps the golden set
            // consistent and lets the tool prune stale files itself.
            Long("emit") => {
                let val = parser.value().unwrap().to_string_lossy().into_owned();
                let (phase_str, out_template) = match val.split_once(':') {
                    Some(parts) => parts,
                    None => panic!("--emit expects <phase>:<out-template>, got {val:?}"),
                };
                emits.push(pipeline::Emit {
                    phase: parse_phase(phase_str),
                    out_template: out_template.to_string(),
                });
            }
            Short('O') => {
                let val = parser.value().unwrap().to_string_lossy().into_owned();
                opt_level = match val.as_str() {
                    "0" => OptLevel::O0,
                    "1" => OptLevel::O1,
                    "2" => OptLevel::O2,
                    "3" => OptLevel::O3,
                    "s" => OptLevel::Os,
                    _ => panic!("unknown optimization level: -O{val}"),
                };
            }
            Long("skip-empty") => {
                skip_empty = true;
            }
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }

    let in_template = in_template.expect("--in is required");
    assert!(
        !emits.is_empty(),
        "at least one --emit <phase>:<out-template> is required"
    );
    pipeline::run_pipeline(&in_template, &emits, opt_level, skip_empty);
}

fn parse_phase(val: &str) -> pipeline::Phase {
    match val {
        "wir" => pipeline::Phase::Wir,
        "nir" => pipeline::Phase::Nir,
        "nir-lowered" => pipeline::Phase::NirLowered,
        "wat" => pipeline::Phase::Wat,
        _ => panic!("unknown phase: {val} (expected wir, nir, nir-lowered, or wat)"),
    }
}

fn wasm2wat(mut parser: lexopt::Parser) {
    let mut input: Option<String> = None;
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Value(v) if input.is_none() => {
                input = Some(v.to_string_lossy().into_owned());
            }
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let input = input.expect("usage: wado-dev-tools wasm2wat <file.wasm>");
    let wasm = std::fs::read(&input).expect("failed to read input file");
    let mut wat = String::new();
    let mut config = wasmprinter::Config::new();
    config.fold_instructions(true);
    config
        .print(&wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .expect("failed to print wasm");
    print!("{wat}");
}
