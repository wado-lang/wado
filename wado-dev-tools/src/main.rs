mod compiler_host;
mod data_section;
mod format_md;
mod pipeline;
mod template;

use wado_compiler::OptLevel;

enum Command {
    GoldenDump,
    Wasm2Wat,
    FormatMd,
}

fn main() {
    let mut parser = lexopt::Parser::from_env();
    let mut command: Option<Command> = None;
    let mut in_template: Option<String> = None;
    let mut out_template: Option<String> = None;
    let mut phase = pipeline::Phase::Wir;
    let mut opt_level = OptLevel::O2;
    let mut skip_empty = false;
    let mut check = false;
    let mut positional_args: Vec<String> = Vec::new();

    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            lexopt::Arg::Long("in") => {
                in_template = Some(parser.value().unwrap().to_string_lossy().into_owned());
            }
            lexopt::Arg::Long("out") => {
                out_template = Some(parser.value().unwrap().to_string_lossy().into_owned());
            }
            lexopt::Arg::Long("phase") => {
                let val = parser.value().unwrap().to_string_lossy().into_owned();
                phase = match val.as_str() {
                    "wir" => pipeline::Phase::Wir,
                    "tir" => pipeline::Phase::Tir,
                    "tir-lowered" => pipeline::Phase::TirLowered,
                    "wat" => pipeline::Phase::Wat,
                    _ => panic!("unknown phase: {val} (expected wir, tir, tir-lowered, or wat)"),
                };
            }
            lexopt::Arg::Short('O') => {
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
            lexopt::Arg::Long("skip-empty") => {
                skip_empty = true;
            }
            lexopt::Arg::Long("check") => {
                check = true;
            }
            lexopt::Arg::Value(val) if command.is_none() => {
                let cmd = val.to_string_lossy();
                command = Some(match cmd.as_ref() {
                    "golden-dump" => Command::GoldenDump,
                    "wasm2wat" => Command::Wasm2Wat,
                    "format-md" => Command::FormatMd,
                    _ => panic!("unknown command: {cmd}"),
                });
            }
            lexopt::Arg::Value(val) => {
                positional_args.push(val.to_string_lossy().into_owned());
            }
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }

    match command.expect("command is required (golden-dump, wasm2wat, format-md)") {
        Command::GoldenDump => {
            let in_template = in_template.expect("--in is required");
            let out_template = out_template.expect("--out is required");
            pipeline::run_pipeline(&in_template, &out_template, phase, opt_level, skip_empty);
        }
        Command::Wasm2Wat => {
            let input = positional_args
                .first()
                .expect("usage: wado-dev-tools wasm2wat <file.wasm>");
            let wasm = std::fs::read(input).expect("failed to read input file");
            let mut wat = String::new();
            let mut config = wasmprinter::Config::new();
            config.fold_instructions(true);
            config
                .print(&wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
                .expect("failed to print wasm");
            print!("{wat}");
        }
        Command::FormatMd => {
            let ok = format_md::run(&positional_args, check);
            if !ok {
                std::process::exit(1);
            }
        }
    }
}
