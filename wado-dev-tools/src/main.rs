mod compiler_host;
mod data_section;
mod pipeline;
mod template;

use wado_compiler::OptLevel;

fn main() {
    let mut parser = lexopt::Parser::from_env();
    let mut in_template: Option<String> = None;
    let mut out_template: Option<String> = None;
    let mut phase = pipeline::Phase::Wir;
    let mut opt_level = OptLevel::O2;

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
                    _ => panic!("unknown phase: {val} (expected wir or tir)"),
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
            lexopt::Arg::Value(cmd) => {
                let cmd = cmd.to_string_lossy();
                match cmd.as_ref() {
                    "golden-dump" => {}
                    _ => panic!("unknown command: {cmd}"),
                }
            }
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }

    let in_template = in_template.expect("--in is required");
    let out_template = out_template.expect("--out is required");

    pipeline::run_pipeline(&in_template, &out_template, phase, opt_level);
}
