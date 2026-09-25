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

The imports are the `web:dom/*` Component Model interfaces. In a browser,
`glue/dom.js` provides them from the page's own DOM: the released-jco transpile
(`scripts/jco/transpile-released.mjs`) maps each `web:<package>/*` import to
its glue. `example/web-browser` runs one program that way.

`wado run`, `wado test` and `wado serve` define each import as a trap, so the
program instantiates anywhere, and `SurfaceDom` answers the calls instead:

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
layout, style or scripting. It holds a tree, text, attributes, an input's value
and the document's title. A call it does not answer traps, and so does a call
the DOM standard throws from, such as an insertion it rejects
(`HierarchyRequestError`) or an invalid name (`InvalidCharacterError`).

A server renders a page by serializing it. `dom.to_html()` gives the whole
document, doctype included, and `Element::get_html` an element's children. Both
follow the HTML standard's serialization. `example/web-ssr` prints a page with
`wado run` and serves it with `wado serve`.

## Layout

- `idl/dom.webidl.json` — the vendored WebIDL slice, as a webidl2 AST
  (`mise run update-webidl-snapshot`).
- `src/dom.wado` — the bindings generated from it
  (`mise run update-package-web`). Do not edit by hand.
- `glue/dom.js` — the browser glue generated beside them: one shim per
  member, over a table that hands out one handle per object, tagged with its
  nearest class in the slice. `mise run test-web-glue` runs it on Node against
  jsdom (`glue/dom.test.mjs`).
- `src/surface_dom.wado` — `SurfaceDom`, written by hand. It mints handles from
  the class numbers `dom.wado` generates.
- `src/lib.wado` — the facade `wado-lang:web` names. A name a wider slice
  generates must be added here; a test fails until it is.

See [WEP: WebIDL Binding Generator](../docs/wep-2026-04-01-tide.md).
