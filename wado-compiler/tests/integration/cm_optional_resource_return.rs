//! A resource method returning `Option<Resource>` reaches the handle through
//! the shared type walk, which asks for own-handles by CM name. The
//! instance-type builder keys its map by Wado name, so the handle was looked
//! up under a name the map never held.

use crate::common::compile_source;

const OPTIONAL_RESOURCE_RETURN: &str = r#"
#[cm("wasi:demo/gfx@0.1.0#adapter")]
resource Adapter {
    #[cm("wasi:demo/gfx@0.1.0#[method]adapter.id")]
    #[cm_params("self")]
    fn id(&self) -> u32;
}

#[cm("wasi:demo/gfx@0.1.0#device")]
resource Device {
    #[cm("wasi:demo/gfx@0.1.0#[method]device.adapter")]
    #[cm_params("self")]
    fn adapter(&self) -> Option<Adapter>;
}

interface Gfx {
    #[cm("wasi:demo/gfx@0.1.0#get-device")]
    fn get_device() -> Device;
}

export fn run() with (Gfx, Device) {
    let device = Gfx::get_device();
    let _ = device.adapter();
}
"#;

#[test]
fn an_optional_resource_return_finds_its_own_handle() {
    compile_source(OPTIONAL_RESOURCE_RETURN)
        .unwrap_or_else(|e| panic!("an optional resource return must compile: {e}"));
}
