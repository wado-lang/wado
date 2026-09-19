//! `wado dump` runs the same phases as `wado compile`, so it registers a user
//! module's `#[cm]` declarations too. Either entry point missing them reaches
//! WIR with the binding's call unresolved.

use crate::common::{InMemoryHost, block_on};
use wado_compiler::{OptLevel, dump_with_host_and_world};

const USER_CM_BINDING: &str = r#"
interface Entropy {
    #[cm("wasi:random/random@0.3.0#get-random-u64")]
    fn next() -> u64;
}

export fn run() with Entropy {
    let _ = Entropy::next();
}
"#;

#[test]
fn dump_registers_a_user_modules_cm_declarations() {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        USER_CM_BINDING,
        &host,
        Some("entry.wado"),
        OptLevel::O0,
        None,
        None,
        wado_compiler::OptOverrides::default(),
        &[],
        &wado_compiler::param_resolution::ParamInputs::default(),
        wado_compiler::kiln::InvocationIndex::default(),
    ))
    .expect("dumping a user CM binding succeeds");

    assert!(
        dump.lowered_nir_text.is_some(),
        "the dump must reach lowered NIR, not stop at the unresolved call"
    );
}
