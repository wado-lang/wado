//! The stream copy canonicals must carry the `async` option.
//!
//! Without it the canonical never hands BLOCKED back to the guest — it suspends
//! the calling thread instead — so the BLOCKED branch synthesis emits around
//! every stream read and write, and `core:rt`'s `cm_await_blocked` behind it,
//! become unreachable code shipped in every component. Runtime fixtures cannot
//! catch that regression: they behave identically either way, because the
//! runtime simply blocks somewhere else. The option is only observable in the
//! emitted component, so it is asserted here.
//!
//! See `docs/wep-2026-07-25-async-stream-canonical.md`.

#![allow(unused_crate_dependencies)]

mod common;

use wado_compiler::OptLevel;

/// Reads and writes a stream, so both copy canonicals survive DCE.
const SOURCE: &str = r#"
use { println, Stdout } from "core:cli";
use { Stdin } from "wasi:cli";

export fn run() with Stdout, Stdin {
    let [stdin_stream, _done] = Stdin::read_via_stream();
    let chunk = stdin_stream.read(16);
    stdin_stream.drop();
    println(`read ${chunk.len()} bytes`);
}
"#;

fn component_wat(opt_level: OptLevel) -> String {
    let wasm = common::compile_source_with_opts(
        std::path::Path::new("stream_canon.wado"),
        SOURCE,
        opt_level,
    )
    .expect("source failed to compile")
    .wasm;
    wasmprinter::print_bytes(&wasm).expect("failed to print component")
}

/// `(canon stream.read $t async (memory ...) ...)` — the option sits between the
/// stream type and the remaining options in the printed form.
fn assert_async_canon(wat: &str, canon: &str, opt_level: OptLevel) {
    let lines: Vec<&str> = wat
        .lines()
        .map(str::trim)
        .filter(|l| l.contains(&format!("canon {canon} ")))
        .collect();
    assert!(
        !lines.is_empty(),
        "[{opt_level:?}] no `canon {canon}` in the emitted component; \
         the fixture no longer exercises it"
    );
    for line in lines {
        assert!(
            line.contains(" async"),
            "[{opt_level:?}] `canon {canon}` is lowered without the `async` \
             option, so it can never return BLOCKED and the await path is dead \
             code: {line}"
        );
    }
}

fn run(opt_level: OptLevel) {
    let wat = component_wat(opt_level);
    assert_async_canon(&wat, "stream.read", opt_level);
    assert_async_canon(&wat, "stream.write", opt_level);
}

#[test]
fn stream_canonicals_are_async_o0() {
    run(OptLevel::O0);
}

#[test]
fn stream_canonicals_are_async_o2() {
    run(OptLevel::O2);
}
