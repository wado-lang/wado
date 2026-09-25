//! `package-web/src/dom.wado` and `package-web/glue/dom.js` are what the
//! vendored snapshot generates. Regenerate with `mise run update-package-web`.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn package_web_dom_is_generated_from_the_vendored_snapshot() {
    let root = root();
    let source = "package-web/idl/dom.webidl.json";
    let json = std::fs::read_to_string(root.join(source)).expect("the snapshot is vendored");
    let snapshot: wado_from_idl::webidl::Snapshot =
        serde_json::from_str(&json).expect("the snapshot parses");
    let generated =
        wado_from_idl::webidl::generate(&snapshot, source).expect("the slice transforms");
    for (path, code) in [
        ("package-web/src/dom.wado", &generated.wado),
        ("package-web/glue/dom.js", &generated.glue),
    ] {
        let committed = std::fs::read_to_string(root.join(path)).expect("the file is committed");
        assert!(
            *code == committed,
            "{path} is stale: run `mise run update-package-web`"
        );
    }
}

/// `package-web`'s hand-written facade re-exports every name `dom.wado` declares.
#[test]
fn package_web_lib_re_exports_every_generated_name() {
    let root = root();
    let generated = std::fs::read_to_string(root.join("package-web/src/dom.wado"))
        .expect("the module is committed");
    let facade =
        std::fs::read_to_string(root.join("package-web/src/lib.wado")).expect("the facade exists");
    let (list, _) = facade
        .split_once("} from \"./dom.wado\"")
        .expect("the facade re-exports from dom.wado");
    let (_, list) = list.rsplit_once('{').expect("a `pub use { … }` list");
    let re_exported: Vec<&str> = list.split(',').map(str::trim).collect();
    let missing: Vec<&str> = generated
        .lines()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("pub resource ")
                .or_else(|| line.strip_prefix("pub interface "))?;
            rest.split([' ', '{']).next()
        })
        .filter(|name| !re_exported.contains(name))
        .collect();
    assert!(
        missing.is_empty(),
        "package-web/src/lib.wado does not re-export {missing:?}"
    );
}
