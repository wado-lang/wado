//! A user module's `#[cm]` declarations are found by the interface they name,
//! not by their namespace or by their Wado name being unique across the
//! bundled stdlib. Codegen asked a global name index instead, so a namespace
//! other than `wasi:` was invisible and a Wado name the stdlib also uses
//! resolved to nothing — both reaching a panic rather than a component.

use crate::common::compile_source;

/// A resource-returning interface, spelled under `interface_fq` with the
/// resource named `resource` on the Wado side.
fn resource_program(interface_fq: &str, resource: &str) -> String {
    format!(
        r#"
#[cm("{interface_fq}#thing")]
resource {resource} {{
    #[cm("{interface_fq}#[method]thing.id")]
    #[cm_params("self")]
    fn id(&self) -> u32;
}}

interface Api {{
    #[cm("{interface_fq}#get-thing")]
    fn get_thing() -> {resource};
}}

export fn run() with (Api, {resource}) {{
    let t = Api::get_thing();
    let _ = t.id();
}}
"#
    )
}

/// A flags parameter, with the flags named `flags` on the Wado side.
fn flags_program(flags: &str) -> String {
    format!(
        r#"
#[cm("wasi:demo/gfx@0.1.0#opts")]
flags {flags} {{
    #[cm("first")]
    First,
}}

interface Api {{
    #[cm("wasi:demo/gfx@0.1.0#open")]
    fn open(opts: {flags}) -> u32;
}}

export fn run() with Api {{
    let _ = Api::open({flags}::First);
}}
"#
    )
}

#[test]
fn a_cm_interface_outside_the_wasi_namespace_is_imported() {
    let source = resource_program("acme:demo/gfx@0.1.0", "Thing");
    compile_source(&source)
        .unwrap_or_else(|e| panic!("a user CM interface in any namespace must compile: {e}"));
}

#[test]
fn a_user_resource_may_take_a_name_the_stdlib_also_uses() {
    let source = resource_program("wasi:demo/gfx@0.1.0", "Fields");
    compile_source(&source).unwrap_or_else(|e| {
        panic!(
            "`Fields` is this interface's own resource, whatever `wasi:http/types` calls its: {e}"
        )
    });
}

#[test]
fn a_user_flags_may_take_a_name_the_stdlib_also_uses() {
    let source = flags_program("PathFlags");
    compile_source(&source).unwrap_or_else(|e| {
        panic!(
            "`PathFlags` is this interface's own flags, whatever `wasi:filesystem` calls its: {e}"
        )
    });
}
