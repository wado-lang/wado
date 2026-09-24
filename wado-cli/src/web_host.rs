//! The `web:*` imports on a Wado host. No Wado host is a browser, so an effect
//! handler answers each call or nothing does: every import traps when called.

use anyhow::{Result, bail};
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Linker};

/// The CM namespace of the web platform interfaces.
const WEB_NAMESPACE: &str = "web:";

/// Define each `web:*` function `component` imports as a trap.
///
/// # Errors
///
/// Returns an error if a `web:*` import is not an instance of functions.
pub fn define_web_imports_as_traps<T: 'static>(
    linker: &mut Linker<T>,
    component: &Component,
) -> Result<()> {
    let engine = component.engine();
    let component_type = component.component_type();
    for (interface, import) in component_type.imports(engine) {
        if !interface.starts_with(WEB_NAMESPACE) {
            continue;
        }
        let ComponentItem::ComponentInstance(instance) = import.ty else {
            bail!("`{interface}` is imported as something other than an instance");
        };
        let mut linker_instance = linker.instance(interface)?;
        for (function, export) in instance.exports(engine) {
            let ComponentItem::ComponentFunc(_) = export.ty else {
                bail!("`{interface}` exports `{function}`, which is not a function");
            };
            let name = format!("{interface}#{function}");
            linker_instance.func_new(function, move |_, _, _, _| {
                Err(wasmtime::Error::msg(format!(
                    "`{name}` was called with no effect handler installed for it"
                )))
            })?;
        }
    }
    Ok(())
}
