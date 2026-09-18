//! A user-declared `#[cm]` record passed to an imported method. The binding
//! asks `cm_type_to_type_id` for the record's guest type, and its resolution
//! chain reached the entry module's own declarations by no step at all, so the
//! record it had just registered was reported as having no TypeId.

use crate::common::compile_source;

const RECORD_PARAM: &str = r#"
#[cm("wasi:demo/gfx@0.1.0#options")]
struct Options {
    #[cm("label")]
    label: Option<String>,
}

#[cm("wasi:demo/gfx@0.1.0#device")]
resource Device {
    #[cm("wasi:demo/gfx@0.1.0#[method]device.configure")]
    #[cm_params("self", "options")]
    fn configure(&self, options: Options) -> u32;
}

interface Gfx {
    #[cm("wasi:demo/gfx@0.1.0#get-device")]
    fn get_device() -> Device;
}

export fn run() with (Gfx, Device) {
    let device = Gfx::get_device();
    let _ = device.configure(Options { label: null });
}
"#;

const OPTIONAL_RECORD_PARAM: &str = r#"
#[cm("wasi:demo/gfx@0.1.0#options")]
struct Options {
    #[cm("label")]
    label: Option<String>,
}

#[cm("wasi:demo/gfx@0.1.0#device")]
resource Device {
    #[cm("wasi:demo/gfx@0.1.0#[method]device.configure")]
    #[cm_params("self", "options")]
    fn configure(&self, options: Option<Options>) -> u32;
}

interface Gfx {
    #[cm("wasi:demo/gfx@0.1.0#get-device")]
    fn get_device() -> Device;
}

export fn run() with (Gfx, Device) {
    let device = Gfx::get_device();
    let _ = device.configure(null);
}
"#;

#[test]
fn a_user_declared_record_reaches_an_imported_method() {
    compile_source(RECORD_PARAM)
        .unwrap_or_else(|e| panic!("a user-declared record parameter must compile: {e}"));
}

#[test]
fn an_optional_user_declared_record_reaches_an_imported_method() {
    compile_source(OPTIONAL_RECORD_PARAM)
        .unwrap_or_else(|e| panic!("an optional user-declared record parameter must compile: {e}"));
}
