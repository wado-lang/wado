# web — Web Platform Bindings for Wado

`wado-lang:web` binds the browser's DOM to Wado. The bindings are generated
from WebIDL, so a DOM type is a Wado resource and a DOM method is a method on
it.

```wado
use { Dom } from "wado-lang:web";

export fn run() with Dom {
    let el = Dom::document().create_element("div", null);
    el.set_id("app");
}
```

The imports are the `web:dom/*` Component Model interfaces. A browser host
provides them. `wado run`, `wado test` and `wado serve` define each one as a
trap, so the program instantiates anywhere, and `SurfaceDom` answers the calls
instead:

```wado
use { Dom, SurfaceDom } from "wado-lang:web";

test "the app renders a heading" {
    let mut dom = SurfaceDom::new();
    with &mut dom do {
        app();
        let h1 = Dom::document().get_element_by_id("title").unwrap();
        assert h1.text_content() == Option::Some("Hello");
    };
}
```

`SurfaceDom` is the DOM's API surface with no browser engine behind it: no
layout, style or scripting. It holds a tree, text, attributes and an input's
value. A call it does not answer traps, and so does an insertion the DOM standard rejects with a
`HierarchyRequestError`.

## Layout

- `idl/dom.webidl.json` — the vendored WebIDL slice, as a webidl2 AST
  (`mise run update-webidl-snapshot`).
- `src/dom.wado` — the bindings generated from it
  (`mise run update-package-web`). Do not edit by hand.
- `src/surface_dom.wado` — `SurfaceDom`, written by hand. It mints handles from the
  class numbers `dom.wado` generates.
- `src/lib.wado` — the facade `wado-lang:web` names. A name a wider slice
  generates must be added here; a test fails until it is.

See [WEP: WebIDL Binding Generator](../docs/wep-2026-04-01-tide.md).
