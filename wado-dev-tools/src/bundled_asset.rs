//! Turn a relocatable core wasm module into the `.wat` asset the compiler
//! bundles, resolving its relocations into a `wado.dataref` map on the way.

use std::fmt::Write as _;

use lexopt::Arg::{Long, Value};
use wado_wasm_embed::dataref::{self, DataRefs};

/// Custom sections the asset has no use for: the relocations are spent once the
/// map is resolved, and nothing ever debugs the asset.
fn is_spent(name: &str) -> bool {
    name == "linking" || name.starts_with("reloc.") || name.starts_with(".debug_")
}

pub fn run(mut parser: lexopt::Parser) {
    let mut input: Option<String> = None;
    let mut component = false;
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("component") => component = true,
            Value(v) if input.is_none() => input = Some(v.to_string_lossy().into_owned()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let input = input.expect("usage: wado-dev-tools bundled-asset [--component] <file.wasm>");
    let wasm = std::fs::read(&input).expect("failed to read input file");

    let core = core_wat(&wasm);
    if component {
        print!("{}", component_wat(&core));
    } else {
        print!("{core}");
    }
}

fn core_wat(wasm: &[u8]) -> String {
    let refs = dataref::resolve(wasm).expect("failed to resolve the data-reference map");
    let mut wat = print(&strip(wasm));
    // `DataRefs::parse` refuses a map naming no function, so an asset with
    // nothing to say carries no section at all.
    if !refs.is_empty() {
        let close = wat.rfind(')').expect("printed wat must close its module");
        wat.insert_str(close, &block(&refs, data_bytes(wasm)));
    }
    wat
}

/// Wrap the module into a component, as `wasm-tools component new` does: its
/// `component-type` section says what the component exports. The text is
/// parsed rather than the stripped binary, since the `wado.dataref` section
/// exists only in the text.
fn component_wat(core_wat: &str) -> String {
    let core = wat::parse_str(core_wat).expect("the printed core module must parse");
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .expect("the core module must carry its `component-type` section")
        .encode()
        .expect("failed to encode the component");
    print(&component)
}

fn print(wasm: &[u8]) -> String {
    let mut wat = String::new();
    let mut config = wasmprinter::Config::new();
    config.fold_instructions(true);
    config
        .print(wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .expect("failed to print wasm");
    wat
}

/// Re-emit `wasm` without the sections [`is_spent`] names. Every other section
/// is copied verbatim, so nothing but the section list changes.
fn strip(wasm: &[u8]) -> Vec<u8> {
    let mut out = wasm_encoder::Module::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload.expect("input must be a core wasm module");
        let (id, range) = match &payload {
            wasmparser::Payload::CustomSection(reader) if is_spent(reader.name()) => continue,
            wasmparser::Payload::CustomSection(reader) => (0, reader.range()),
            other => match other.as_section() {
                Some(section) => section,
                None => continue,
            },
        };
        out.section(&wasm_encoder::RawSection {
            id,
            data: &wasm[range.start as usize..range.end as usize],
        });
    }
    out.finish()
}

fn data_bytes(wasm: &[u8]) -> usize {
    let mut total = 0;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::DataSection(reader)) = payload {
            for data in reader.into_iter().flatten() {
                total += data.data.len();
            }
        }
    }
    total
}

/// The map as a `.wat` annotation, one entry per line.
///
/// `wat` concatenates adjacent string literals, so a line each keeps the map
/// readable where one escaped blob would not, and the `;;` header is a comment
/// rather than payload.
fn block(refs: &DataRefs, data_bytes: usize) -> String {
    let mut block = String::from(
        "  ;; Which data bytes each function reads, resolved from the `linking` and\n\
         \x20 ;; `reloc.CODE` sections of the relocatable build. `wado-wasm-embed` keeps\n\
         \x20 ;; only the ranges the surviving functions claim and drops the rest, so an\n\
         \x20 ;; entry too narrow makes a program compute the wrong answer.\n\
         \x20 ;;\n\
         \x20 ;;   <function name> <segment>:<offset>+<size> ...      (sorted by offset)\n\
         \x20 ;;\n",
    );
    writeln!(
        block,
        "  ;; {} functions claim {} of {data_bytes} data bytes; the rest is padding.\n\
         \x20 ;; Regenerate with `mise run update-bundled` — never edit by hand.\n\
         \x20 (@custom \"{}\" (after data)",
        refs.len(),
        refs.claimed_bytes(),
        dataref::SECTION_NAME,
    )
    .expect("writing to a String cannot fail");
    for line in refs.to_text().lines() {
        writeln!(block, "    \"{line}\\n\"").expect("writing to a String cannot fail");
    }
    block.push_str("  )\n");
    block
}
