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
provides them; `wado run` and `wado test` fill any it does not provide with a
trap, so a program that never touches the DOM still runs.

## Layout

- `idl/dom.webidl.json` — the vendored WebIDL slice, as a webidl2 AST
  (`mise run update-webidl-snapshot`).
- `src/dom.wado` — the bindings generated from it
  (`mise run update-package-web`). Do not edit by hand.

See [WEP: WebIDL Binding Generator](../docs/wep-2026-04-01-tide.md).
