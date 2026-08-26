//! Turn a relocatable core wasm module into the `.wat` asset the compiler
//! bundles, resolving its relocations into a `wado.dataref` map on the way.

use std::fmt::Write as _;

use lexopt::Arg::Value;
use wado_wasm_embed::dataref::{self, DataRefs};

/// Custom sections the asset has no use for: the relocations are spent once the
/// map is resolved, and nothing ever debugs the asset.
fn is_spent(name: &str) -> bool {
    name == "linking" || name.starts_with("reloc.") || name.starts_with(".debug_")
}

pub fn run(mut parser: lexopt::Parser) {
    let mut input: Option<String> = None;
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Value(v) if input.is_none() => input = Some(v.to_string_lossy().into_owned()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let input = input.expect("usage: wado-dev-tools bundled-asset <file.wasm>");
    let wasm = std::fs::read(&input).expect("failed to read input file");

    let refs = dataref::resolve(&wasm).expect("failed to resolve the data-reference map");
    let stripped = strip(&wasm);

    let mut wat = String::new();
    let mut config = wasmprinter::Config::new();
    config.fold_instructions(true);
    config
        .print(&stripped, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .expect("failed to print wasm");

    // `DataRefs::parse` refuses a map naming no function, so an asset with
    // nothing to say carries no section at all.
    let block = if refs.is_empty() {
        String::new()
    } else {
        block(&refs, data_bytes(&wasm))
    };
    let close = wat.rfind(')').expect("printed wat must close its module");
    print!("{}{}{}", &wat[..close], block, &wat[close..]);
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
            data: &wasm[range.start..range.end],
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
