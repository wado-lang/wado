//! Two Wado names binding one CM operation. The interface's instance type
//! describes the imported interface, so it exports each CM name once; the core
//! module imports one alias per used Wado name. Emitting the CM side per Wado
//! name made the component invalid, and emitting the Wado side from the
//! registry rather than the used set left dead aliases behind.

use crate::common::compile_source;

const BOTH_NAMES_USED: &str = r#"
use { println, Stdout } from "core:cli";
use { Random } from "wasi:random";

interface Entropy {
    #[cm("wasi:random/random@0.3.0#get-random-u64")]
    fn next() -> u64;
}

export fn run() with (Stdout, Entropy, Random) {
    let a = Entropy::next();
    let b = Random::get_random_u64();
    println(`${a == b}`);
}
"#;

const ONE_NAME_USED: &str = r#"
use { println, Stdout } from "core:cli";

interface Entropy {
    #[cm("wasi:random/random@0.3.0#get-random-u64")]
    fn next() -> u64;
}

export fn run() with (Stdout, Entropy) {
    println(`${Entropy::next() == 0}`);
}
"#;

fn lines_matching(wasm: &[u8], needle: &str) -> Vec<String> {
    let wat = wasmprinter::print_bytes(wasm).expect("disassemble the component to WAT");
    wat.lines()
        .map(str::trim)
        .filter(|line| line.contains(needle))
        .map(str::to_string)
        .collect()
}

#[test]
fn one_cm_operation_is_exported_once_however_many_names_bind_it() {
    let result = compile_source(BOTH_NAMES_USED)
        .unwrap_or_else(|e| panic!("two names binding one CM operation must compile: {e}"));

    let exports = lines_matching(&result.wasm, "(export \"get-random-u64\"");
    assert_eq!(
        exports.len(),
        1,
        "the instance type must export the CM name once, got {exports:#?}"
    );

    let aliases = lines_matching(&result.wasm, "(alias export $wasi:random/random");
    assert_eq!(
        aliases.len(),
        2,
        "each used Wado name needs its own alias, got {aliases:#?}"
    );
}

#[test]
fn an_unused_binding_of_a_shared_operation_is_not_aliased() {
    let result = compile_source(ONE_NAME_USED)
        .unwrap_or_else(|e| panic!("one name binding a CM operation must compile: {e}"));

    let aliases = lines_matching(&result.wasm, "(alias export $wasi:random/random");
    assert_eq!(
        aliases.len(),
        1,
        "only `Entropy::next` is called, so the stdlib's binding must not be \
         aliased or lowered; got {aliases:#?}"
    );
}
