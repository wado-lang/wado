# package-web TODO

This file tracks the work that brings `wado-lang:web` to practical use. The
design and the initial implementation are in
[WEP: The Web Interface for Wado](../docs/wep-2026-04-01-web.md).

## Where it stands

The pipeline works end to end: WebIDL snapshot, generated bindings and glue,
`SurfaceDom`, server-side rendering, callbacks, and a jsdom run in CI. The slice
covers eight interfaces: `EventTarget`, `Event`, `Node`, `Element`,
`HTMLElement`, `HTMLInputElement`, `Document` and `Window`.

`wado-from-idl` skips every member it cannot lower and names it on stderr. On
the current snapshot it skips 570 members:

| Reason                             | Members | Examples                                               |
| ---------------------------------- | ------- | ------------------------------------------------------ |
| An event handler attribute         | 353     | `onclick`, `oninput`                                   |
| A type outside the slice           | 145     | `NodeList`, `HTMLCollection`, `DOMTokenList`, `any`    |
| `Promise<T>`                       | 35      | `scroll_to`, `request_fullscreen`                      |
| `sequence<T>` and `FrozenArray<T>` | 17      | `composed_path`, `get_attribute_names`                 |
| A union                            | 13      | `append`, `prepend`, `before`, the `inner_html` getter |
| More than one overload lowers      | 2       | `Window.alert`                                         |

A variadic argument lowers, but `append` and its siblings take
`(Node or DOMString)`, so they wait on the union.

## Order

The lowering comes first, then the slice. Every type the frontend learns to
lower is then available to each interface the slice adds.

### 1. Sequences and variadics

- [ ] Lower `sequence<T>` and `FrozenArray<T>` to `List<T>`, in the bindings and
  in the glue
- [x] Lower a variadic argument to a `List<T>`
- [ ] `SurfaceDom` answers the members this unlocks that the slice already has

### 2. Default arguments on a `#[cm]` operation

- [x] The compiler accepts a default argument on a `#[cm]` operation
- [x] `wado-from-idl` emits `= null` for an `optional` without a default, and
  the WebIDL default where one is given, so `create_element("div")` and
  `clone_node()` compile

### 3. Overloads and unions

- [ ] Merge overloads into one member where they differ by trailing optional
  arguments (`Window.alert()` and `alert(message)`)
- [ ] Lower a union of two or more typable constituents to a `variant`, named
  after its typedef where it has one (`(Node or DOMString)`)

### 4. Promises

- [ ] Lower `Promise<T>` to `Future<T>`, in the bindings and in the glue
- [ ] `fetch` and `Response` join the slice, with an effect of their own

### 5. A wider slice

- [ ] Tree: `Text`, `CharacterData`, `Comment`, `DocumentFragment`, `NodeList`,
  `HTMLCollection`
- [ ] Style and classes: `DOMTokenList`, `CSSStyleDeclaration`, `DOMRect`
- [ ] Elements: `HTMLButtonElement`, `HTMLAnchorElement`, `HTMLFormElement`,
  `HTMLSelectElement`, `HTMLTextAreaElement`, `HTMLCanvasElement`,
  `CanvasRenderingContext2D`
- [ ] Events: `UIEvent`, `MouseEvent`, `KeyboardEvent`, `InputEvent`,
  `FocusEvent`
- [ ] Window: `Location`, `Storage`, `setTimeout` and `setInterval`
- [ ] `SurfaceDom` and `lib.wado` follow each addition

## Known gaps

- A handle is never released. The glue's table keeps every object the program
  receives, and the callback registry keeps every closure it registers.
- A program cannot recover from a DOM exception: it traps.
- CI runs the glue on jsdom only, never in a browser.
- jco 1.30.0 fails an export that a listener reenters during
  `dispatch_event`.
- `SurfaceDom` neither removes a listener nor ignores one added twice.
- Every callback shape a declared `#[cm]` import takes gets its export, whether
  the program passes such a closure or not.
- A mixin's members are folded into each interface that includes it, rather
  than declared once as a trait.
- A handle has no `Inspect`, `Display` or `serde` rule.
- `wado-lang:web` publishes no WIT.
- An event handler attribute (`onclick`) is skipped, since its type is a
  callback in a result. `add_event_listener` covers the same events.
