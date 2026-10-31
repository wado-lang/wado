//! The `wasm-tools` subcommands the repository uses, so no task installs the
//! `wasm-tools` CLI. Each takes a subset of its `wasm-tools` namesake's
//! arguments, spelled the same, and validates a component before writing it.

use std::path::PathBuf;

use lexopt::Arg::{Long, Short, Value};

/// `component-new <core.wasm|.wat> -o <out.wasm>`
pub fn component_new(mut parser: lexopt::Parser) {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Short('o') => output = Some(parser.value().unwrap().into()),
            Value(v) if input.is_none() => input = Some(v.into()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let usage = "usage: wado-dev-tools component-new <core.wasm> -o <out.wasm>";
    let core = wat::parse_file(input.expect(usage)).expect("failed to read the core module");
    write(&output.expect(usage), &encode_component(&core));
}

/// `compose <component.wasm> -d <definition.wasm>... -o <out.wasm>`
pub fn compose(mut parser: lexopt::Parser) {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut config = wasm_compose::config::Config::default();
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Short('d') => config.definitions.push(parser.value().unwrap().into()),
            Short('o') => output = Some(parser.value().unwrap().into()),
            Value(v) if input.is_none() => input = Some(v.into()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let usage =
        "usage: wado-dev-tools compose <component.wasm> -d <definition.wasm>... -o <out.wasm>";
    let input = input.expect(usage);
    let composed = wasm_compose::composer::ComponentComposer::new(&input, &config)
        .compose()
        .unwrap_or_else(|e| panic!("failed to compose {}: {e:#}", input.display()));
    write(&output.expect(usage), &composed);
}

/// `print [-f] [--offsets] <file.wasm>`, the WAT on stdout.
pub fn print(mut parser: lexopt::Parser) {
    let mut input: Option<PathBuf> = None;
    let mut config = wasmprinter::Config::new();
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Short('f') => {
                config.fold_instructions(true);
            }
            Long("offsets") => {
                config.print_offsets(true);
            }
            Value(v) if input.is_none() => input = Some(v.into()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let input = input.expect("usage: wado-dev-tools print [-f] [--offsets] <file.wasm>");
    let wasm = std::fs::read(&input).expect("failed to read the input file");
    print!("{}", to_wat(&config, &wasm));
}

/// `objdump <file.wasm>`: each section's byte range, size, and item count, a
/// nested module or component indented under its parent.
pub fn objdump(mut parser: lexopt::Parser) {
    use wasmparser::Payload;

    let mut input: Option<PathBuf> = None;
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Value(v) if input.is_none() => input = Some(v.into()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let input = input.expect("usage: wado-dev-tools objdump <file.wasm>");
    let wasm = std::fs::read(&input).expect("failed to read the input file");

    // The outermost `End` closes the file itself, so it leaves the depth at -1.
    let mut depth: i32 = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
        let payload = payload.expect("the input must be wasm");
        let (name, count) = match &payload {
            Payload::Version { .. } | Payload::CodeSectionEntry(_) => continue,
            Payload::End(_) => {
                depth -= 1;
                continue;
            }
            Payload::TypeSection(r) => ("type".to_string(), Some(r.count())),
            Payload::ImportSection(r) => ("import".to_string(), Some(r.count())),
            Payload::FunctionSection(r) => ("function".to_string(), Some(r.count())),
            Payload::TableSection(r) => ("table".to_string(), Some(r.count())),
            Payload::MemorySection(r) => ("memory".to_string(), Some(r.count())),
            Payload::TagSection(r) => ("tag".to_string(), Some(r.count())),
            Payload::GlobalSection(r) => ("global".to_string(), Some(r.count())),
            Payload::ExportSection(r) => ("export".to_string(), Some(r.count())),
            Payload::StartSection { .. } => ("start".to_string(), None),
            Payload::ElementSection(r) => ("element".to_string(), Some(r.count())),
            Payload::DataCountSection { .. } => ("data count".to_string(), None),
            Payload::DataSection(r) => ("data".to_string(), Some(r.count())),
            Payload::CodeSectionStart { count, .. } => ("code".to_string(), Some(*count)),
            Payload::ModuleSection { .. } => ("module".to_string(), None),
            Payload::InstanceSection(r) => ("core instance".to_string(), Some(r.count())),
            Payload::CoreTypeSection(r) => ("core type".to_string(), Some(r.count())),
            Payload::ComponentSection { .. } => ("component".to_string(), None),
            Payload::ComponentInstanceSection(r) => ("instance".to_string(), Some(r.count())),
            Payload::ComponentAliasSection(r) => ("alias".to_string(), Some(r.count())),
            Payload::ComponentTypeSection(r) => ("component type".to_string(), Some(r.count())),
            Payload::ComponentCanonicalSection(r) => ("canonical".to_string(), Some(r.count())),
            Payload::ComponentStartSection { .. } => ("component start".to_string(), None),
            Payload::ComponentImportSection(r) => ("component import".to_string(), Some(r.count())),
            Payload::ComponentExportSection(r) => ("component export".to_string(), Some(r.count())),
            Payload::CustomSection(r) => (format!("custom {:?}", r.name()), None),
            Payload::UnknownSection { id, .. } => (format!("unknown {id}"), None),
            other => panic!("objdump does not know the payload {other:?}"),
        };
        let range = match &payload {
            Payload::ModuleSection {
                unchecked_range, ..
            }
            | Payload::ComponentSection {
                unchecked_range, ..
            } => unchecked_range.clone(),
            other => {
                other
                    .as_section()
                    .expect("every listed payload is a section")
                    .1
            }
        };
        let count = count.map_or(String::new(), |n| format!(" | {n} count"));
        let indent = usize::try_from(depth * 2).expect("a section is inside the file");
        println!(
            "{:indent$}{name:<width$} | {:#10x} - {:#10x} | {:>10} bytes{count}",
            "",
            range.start,
            range.end,
            range.end - range.start,
            width = 32 - indent,
        );
        if matches!(
            payload,
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. }
        ) {
            depth += 1;
        }
    }
}

/// Wrap a core module into a component, as `wasm-tools component new` does:
/// the module's `component-type` section says what the component exports.
pub fn encode_component(core: &[u8]) -> Vec<u8> {
    wit_component::ComponentEncoder::default()
        .validate(true)
        .module(core)
        .expect("the core module must carry its `component-type` section")
        .encode()
        .expect("failed to encode the component")
}

pub fn to_wat(config: &wasmprinter::Config, wasm: &[u8]) -> String {
    let mut wat = String::new();
    config
        .print(wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .expect("failed to print wasm");
    wat
}

fn write(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}
