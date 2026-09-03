//! An exported interface whose signature names types owned by an imported
//! interface must survive a WIT decode: it exports only the types it defines,
//! so no type is registered under two owners.

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

const SERVICE_WORLD: &str = "wasi:http/service";

/// The smallest `wasi:http/service` program: `handle` names `Request`,
/// `Response` and `ErrorCode`, every one of them owned by `wasi:http/types`,
/// which the world imports.
const SERVICE_SOURCE: &str = r#"
use { Request, Response, ErrorCode, Fields, Headers, Trailers, StatusCode } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_future, _trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let [body_rx, body_tx] = Stream::<u8>::new();
    let [response, _tx_future] = Response::new(
        Headers::new(),
        Option::<Stream<u8>>::Some(body_rx),
        trailers_future,
    );
    response.set_status_code(200 as StatusCode);
    task return Result::<Response, ErrorCode>::Ok(response);
    body_tx.drop();
}
"#;

fn compile(world: &str, source: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cm_decode.wado");
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        target_world: Some(world.to_string()),
        ..Default::default()
    };
    crate::common::compile_source_with_compiler_options(Path::new(path), source, options)
        .expect("the service program compiles")
        .wasm
}

/// The world the decode reconstructs, as `(imports, exports)` of interface names.
fn decoded_world(wasm: &[u8]) -> (Vec<String>, Vec<String>) {
    let decoded = wit_component::decode(wasm).expect("the emitted component decodes to WIT");
    let wit_component::DecodedWasm::Component(resolve, world_id) = &decoded else {
        panic!("a compiled program decodes as a component, not a WIT package");
    };
    let world = &resolve.worlds[*world_id];
    let name = |key: &wit_parser::WorldKey| match key {
        wit_parser::WorldKey::Name(n) => n.clone(),
        wit_parser::WorldKey::Interface(id) => resolve
            .id_of(*id)
            .expect("an interface import/export is named"),
    };
    (
        world.imports.keys().map(name).collect(),
        world.exports.keys().map(name).collect(),
    )
}

#[test]
fn http_service_component_decodes_to_wit() {
    let wasm = compile(SERVICE_WORLD, SERVICE_SOURCE);
    // The failure this guards reports the imported type twice:
    // "the type `request` appears more than once".
    let (imports, exports) = decoded_world(&wasm);
    assert!(
        imports.iter().any(|i| i == "wasi:http/types@0.3.0"),
        "the types interface stays an import: {imports:?}"
    );
    assert_eq!(
        exports,
        vec!["wasi:http/handler@0.3.0".to_string()],
        "the handler is the one export, resolved to its own interface"
    );
}

#[test]
fn cli_command_component_decodes_to_wit() {
    let wasm = compile("wasi:cli/command", "export fn run() {\n}\n");
    let (_, exports) = decoded_world(&wasm);
    assert_eq!(exports, vec!["wasi:cli/run@0.3.0".to_string()]);
}
