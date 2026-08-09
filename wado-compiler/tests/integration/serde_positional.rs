//! Synthesized `FieldSchema` positional resolution (WEP
//! `wep-2026-02-28-serde.md` §"Per-Field Positional Resolution").
//!
//! `#[wire(positional)]` marks a struct field as ordinal. For every
//! deserializable struct the synthesiser emits `FieldSchema::positional_at`
//! enumerating positional fields in declaration order, and `lookup` omits them
//! (a positional field is never matched by name). These functions are reachable
//! only through a positional format (`core:args`), so they are verified here
//! against the monomorphized TIR rather than at runtime.

use crate::common::InMemoryHost;
use wado_compiler::{OptLevel, dump_with_host_and_world};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Monomorphized TIR text for `source`.
fn monomorphized_tir(source: &str) -> String {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        source,
        &host,
        Some("entry.wado"),
        OptLevel::O0,
        None,
        None,
        None,
        None,
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
        wado_compiler::kiln::InvocationIndex::default(),
    ))
    .expect("dump succeeds");
    dump.monomorphized_tir_text
        .expect("monomorphized TIR present after dump")
}

/// The body of the unparsed `pub fn "<sig_prefix>…"` block, from its opening
/// `{` to the matching `}`.
fn function_body<'a>(tir: &'a str, sig_prefix: &str) -> &'a str {
    let start = tir
        .find(sig_prefix)
        .unwrap_or_else(|| panic!("function {sig_prefix:?} not found in TIR:\n{tir}"));
    let open = tir[start..]
        .find('{')
        .map(|i| start + i)
        .expect("function has a body");
    let mut depth = 0usize;
    for (i, c) in tir[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &tir[open..=open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces for {sig_prefix:?}");
}

const SOURCE: &str = r#"
use { Deserialize } from "core:serde";

struct Cli {
    #[wire(positional)] input: String,
    #[wire(positional)] out: String = "out.txt",
    jobs: i32 = 1,
    verbose: bool = false,
}

impl Deserialize for Cli;

export fn run() {}
"#;

#[test]
fn positional_at_maps_rank_to_declaration_order_index() {
    let tir = monomorphized_tir(SOURCE);
    let body = function_body(&tir, r#"fn "entry.wado/Cli^FieldSchema::positional_at""#);

    // rank 0 -> field 0 (input), rank 1 -> field 1 (out), else None.
    assert!(
        body.contains("__rank == 0") && body.contains("Some(0)"),
        "positional_at must map rank 0 to field index 0:\n{body}"
    );
    assert!(
        body.contains("__rank == 1") && body.contains("Some(1)"),
        "positional_at must map rank 1 to field index 1:\n{body}"
    );
    // Only the two positional fields are enumerated; rank 2 is absent.
    assert!(
        !body.contains("__rank == 2"),
        "positional_at must not enumerate non-positional fields:\n{body}"
    );
    assert!(
        body.contains("None"),
        "positional_at must fall through to None:\n{body}"
    );
}

#[test]
fn lookup_excludes_positional_fields() {
    let tir = monomorphized_tir(SOURCE);
    let body = function_body(&tir, r#"fn "entry.wado/Cli^FieldSchema::lookup""#);

    // Nominal fields jobs (index 2) and verbose (index 3) are matched by name.
    assert!(
        body.contains("Some(2)") && body.contains("Some(3)"),
        "lookup must match the nominal fields by name:\n{body}"
    );
    // Positional fields input (index 0) and out (index 1) are never matched by
    // name, so their indices never appear as a lookup result.
    assert!(
        !body.contains("Some(0)") && !body.contains("Some(1)"),
        "lookup must not match positional fields by name:\n{body}"
    );
}
