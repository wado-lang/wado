//! A user module's `#[cm]` declarations need not all sit in one file. What
//! decides whether a module is scanned read only `interface` items, so a module
//! holding just the resources and records was skipped and the interface using
//! them reached WIR unresolved.

use std::path::Path;

use crate::common::compile_file;

const TYPES_MODULE: &str = r#"
#[cm("wasi:demo/gfx@0.1.0#options")]
pub struct Options {
    #[cm("label")]
    pub label: Option<String>,
}

#[cm("wasi:demo/gfx@0.1.0#device")]
pub resource Device {
    #[cm("wasi:demo/gfx@0.1.0#[method]device.configure")]
    #[cm_params("self", "options")]
    fn configure(&self, options: Options) -> u32;
}
"#;

const ENTRY_MODULE: &str = r#"
use { Options, Device } from "./gfx.wado";

interface Gfx {
    #[cm("wasi:demo/gfx@0.1.0#get-device")]
    fn get_device() -> Device;
}

export fn run() with (Gfx, Device) {
    let device = Gfx::get_device();
    let _ = device.configure(Options { label: null });
}
"#;

#[test]
fn cm_declarations_may_live_in_a_module_of_their_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("gfx.wado"), TYPES_MODULE).expect("write gfx.wado");
    let entry = dir.path().join("main.wado");
    std::fs::write(&entry, ENTRY_MODULE).expect("write main.wado");

    compile_file(Path::new(&entry))
        .unwrap_or_else(|e| panic!("CM declarations split across modules must compile: {e}"));
}
