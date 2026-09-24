//! `package-web/src/dom.wado` is what the vendored snapshot generates.
//! Regenerate with `mise run update-package-web`.

use std::path::Path;

#[test]
fn package_web_dom_is_generated_from_the_vendored_snapshot() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let source = "package-web/idl/dom.webidl.json";
    let json = std::fs::read_to_string(root.join(source)).expect("the snapshot is vendored");
    let snapshot: wado_from_idl::webidl::Snapshot =
        serde_json::from_str(&json).expect("the snapshot parses");
    let (generated, _skipped) =
        wado_from_idl::webidl::generate(&snapshot, source).expect("the slice transforms");
    let committed = std::fs::read_to_string(root.join("package-web/src/dom.wado"))
        .expect("the module is committed");
    assert!(
        generated == committed,
        "package-web/src/dom.wado is stale: run `mise run update-package-web`"
    );
}
