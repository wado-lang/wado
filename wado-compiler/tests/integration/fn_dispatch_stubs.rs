//! `Fn<arity, ret>` dispatch stubs are synthesized only for signatures whose
//! return type is fully determined.
//!
//! Regression: the collector filtered candidates with a predicate that looked
//! at the top-level constructor alone, so `fn(I::Item) -> U` was rejected only
//! because its return type happened to be a bare `TypeParam`. Once a use site
//! instantiated that slot into an inference variable, nothing rejected it and
//! the compiler emitted `Fn<1,?1>^Inspect::inspect` — a stub named after a
//! variable, which no monomorphized closure can ever dispatch through.

use std::collections::BTreeSet;

use crate::common::InMemoryHost;
use wado_compiler::{OptLevel, dump_with_host_and_world};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// `map` takes `f: fn(Self::Item) -> U`, so elaborating this call instantiates
/// both slots and interns the `fn` type the collector used to accept.
const SOURCE: &str = r#"
export fn run() {
    let xs: List<i32> = [1, 2, 3];
    let doubled: List<i32> = xs.iter_ref().map(|x| *x * 2).collect();
    assert doubled.len() == 3;
}
"#;

fn monomorphized_tir(source: &str) -> String {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        source,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
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
        .expect("monomorphized TIR snapshot present after dump")
}

#[test]
fn no_inference_variable_reaches_monomorphized_tir() {
    let text = monomorphized_tir(SOURCE);
    let leaked: BTreeSet<&str> = text
        .lines()
        .filter(|line| {
            line.match_indices('?')
                .any(|(i, _)| line[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "inference variables must not survive elaboration, found:\n{}",
        leaked.into_iter().collect::<Vec<_>>().join("\n")
    );
}

/// Every quoted `"Fn<…>^Trait::method"` declaration name in `text`. Only the
/// name is keyed on the return type; a stub's `&self` parameter is built from
/// whichever `Function` type represented the signature and may still spell a
/// type parameter, which codegen never reads.
fn fn_stub_names(text: &str) -> Vec<&str> {
    text.split("pub fn \"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"'))
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("Fn<"))
        .collect()
}

#[test]
fn fn_dispatch_stubs_name_only_determined_return_types() {
    let text = monomorphized_tir(SOURCE);
    let names = fn_stub_names(&text);
    assert!(!names.is_empty(), "expected some `Fn<…>` dispatch stubs");
    // Every module that reaches the stub re-declares it, so dedup before
    // reporting or one bad key prints hundreds of times.
    let undetermined: BTreeSet<&str> = names
        .into_iter()
        .filter(|name| name.contains("::Item"))
        .collect();
    assert!(
        undetermined.is_empty(),
        "a stub keyed on an unresolved projection is unreachable post-monomorphization, found:\n{undetermined:#?}"
    );
}
