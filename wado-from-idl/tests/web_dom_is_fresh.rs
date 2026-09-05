//! `wado-compiler/lib/web/dom.wado` is what the vendored snapshot generates.
//! Regenerate with `mise run update-stdlib-web`.

use std::path::Path;

#[test]
fn the_bundled_web_dom_module_is_generated_from_the_vendored_snapshot() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let source = "wado-compiler/lib/web/dom.webidl.json";
    let json = std::fs::read_to_string(root.join(source)).expect("the snapshot is vendored");
    let snapshot: wado_from_idl::webidl::Snapshot =
        serde_json::from_str(&json).expect("the snapshot parses");
    let (generated, _skipped) =
        wado_from_idl::webidl::generate(&snapshot, source).expect("the slice transforms");
    let bundled = std::fs::read_to_string(root.join("wado-compiler/lib/web/dom.wado"))
        .expect("the module is bundled");
    assert!(
        generated == bundled,
        "wado-compiler/lib/web/dom.wado is stale: run `mise run update-stdlib-web`"
    );
}
