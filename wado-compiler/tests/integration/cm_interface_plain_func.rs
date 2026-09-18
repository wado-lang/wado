//! A CM interface that defines resources may also declare plain functions of
//! its own. Only `wasi:http/types`, whose every function belongs to a resource,
//! reached the resource-defining import path before, so an interface function
//! beside them was left out of the imported instance type and the core module's
//! import matched nothing.

use crate::common::compile_source;

const RESOURCE_AND_PLAIN_FUNC: &str = r#"
#[cm("wasi:demo/gfx@0.1.0#adapter")]
resource Adapter {
    #[cm("wasi:demo/gfx@0.1.0#[method]adapter.id")]
    #[cm_params("self")]
    fn id(&self) -> u32;
}

#[cm("wasi:demo/gfx@0.1.0#device")]
resource Device {
    // Uncalled, and there for the interface's shape alone: a method returning
    // `Option<Resource>` is what marks the interface resource-defining.
    #[cm("wasi:demo/gfx@0.1.0#[method]device.adapter")]
    #[cm_params("self")]
    fn adapter(&self) -> Option<Adapter>;

    #[cm("wasi:demo/gfx@0.1.0#[method]device.id")]
    #[cm_params("self")]
    fn id(&self) -> u32;
}

interface Gfx {
    #[cm("wasi:demo/gfx@0.1.0#get-device")]
    fn get_device() -> Device;
}

export fn run() with (Gfx, Device) {
    let device = Gfx::get_device();
    let _ = device.id();
}
"#;

#[test]
fn an_interface_function_beside_a_resource_is_imported() {
    compile_source(RESOURCE_AND_PLAIN_FUNC)
        .unwrap_or_else(|e| panic!("a plain function beside a resource must compile: {e}"));
}
