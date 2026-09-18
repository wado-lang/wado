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

const OTHER_TYPES_MODULE: &str = r#"
#[cm("wasi:demo/gfx@0.1.0#adapter")]
pub resource Adapter {
    #[cm("wasi:demo/gfx@0.1.0#[method]adapter.id")]
    #[cm_params("self")]
    fn id(&self) -> u32;
}
"#;

const SPLIT_TYPES_ENTRY: &str = r#"
use { Options } from "./gfx.wado";
use { Adapter } from "./more.wado";

interface Gfx {
    #[cm("wasi:demo/gfx@0.1.0#get-adapter")]
    fn get_adapter() -> Adapter;
}

export fn run() with (Gfx, Adapter) {
    let a = Gfx::get_adapter();
    let _ = a.id();
}
"#;

/// A CM interface has one declaring module, so its types live in that module.
/// The `interface` naming them may sit elsewhere, as the test above relies on.
#[test]
fn one_interfaces_types_may_not_be_split_across_modules() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("gfx.wado"), TYPES_MODULE).expect("write gfx.wado");
    std::fs::write(dir.path().join("more.wado"), OTHER_TYPES_MODULE).expect("write more.wado");
    let entry = dir.path().join("main.wado");
    std::fs::write(&entry, SPLIT_TYPES_ENTRY).expect("write main.wado");

    let err = compile_file(Path::new(&entry))
        .err()
        .expect("a second declaring module for one interface must be a diagnostic");
    let message = err.to_string();
    assert!(
        message.contains("wasi:demo/gfx@0.1.0") && message.contains("already declares"),
        "expected the single-declaring-module error, got {message}"
    );
}
