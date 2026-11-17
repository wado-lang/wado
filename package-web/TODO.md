# package-web TODO

This file tracks the work that brings `wado-lang:web` to practical use. The
design and the initial implementation are in
[WEP: The Web Interface for Wado](../docs/wep-2026-04-01-web.md).

## Where it stands

The pipeline works end to end: WebIDL snapshot, generated bindings and glue,
`SurfaceDom`, server-side rendering, callbacks, and a jsdom run in CI. The slice
covers ten interfaces: `EventTarget`, `Event`, `Node`, `CharacterData`, `Text`,
`Element`, `HTMLElement`, `HTMLInputElement`, `Document` and `Window`.

`wado-from-idl` skips every member it cannot lower and names it on stderr. On
the current snapshot it skips 546 members (`mise run update-package-web` prints
each):

| Reason                        | Members | Examples                                               |
| ----------------------------- | ------- | ------------------------------------------------------ |
| An event handler attribute    | 353     | `onclick`, `oninput`                                   |
| A type outside the slice      | 164     | `NodeList`, `HTMLCollection`, `sequence<Range>`, `any` |
| A union                       | 16      | `append`, `prepend`, `before`, the `inner_html` getter |
| More than one overload lowers | 8       | `Window.alert`, `scroll_to`                            |
| `ObservableArray<T>`          | 1       | `adopted_style_sheets`                                 |
| `[HTMLConstructor]`, a getter | 4       | `HTMLElement.new`, `Document`'s named getter           |

A member returning `Promise<T>` lowers where `T` does, so one whose `T` is
outside the slice is counted there.

A variadic argument lowers, but `append` and its siblings take
`(Node or DOMString)`, so they wait on the union.

## Order

The lowering comes first, then the slice. Every type the frontend learns to
lower is then available to each interface the slice adds.

### 1. Sequences and variadics

- [x] Lower `sequence<T>` and `FrozenArray<T>` to `List<T>`, in the bindings and
  in the glue
- [x] Lower a variadic argument to a `List<T>`
- [x] `SurfaceDom` answers the members this unlocks that the slice already has:
  `composed_path`, `get_attribute_names`, the `aria_*_elements` attributes

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

- [x] Lower a member returning `Promise<T>` to an `AsyncCall`, as
  [WEP: Extern Handles](../docs/wep-2026-11-14-extern-handles.md#a-member-returning-a-promiset-returns-an-asynccall)
  decides, in the bindings and in the glue
- [x] `SurfaceDom` answers `scroll_into_view`, `request_fullscreen` and
  `exit_fullscreen`
- [ ] `fetch` and `Response` join the slice, with an effect of their own

### 5. A wider slice

- [x] Tree: `Text` and `CharacterData`; `SurfaceDom` answers `create_text_node`,
  `data` and `insert_before`
- [ ] Tree: `Comment`, `DocumentFragment`, `NodeList`, `HTMLCollection`
- [ ] Style and classes: `DOMTokenList`, `CSSStyleDeclaration`, `DOMRect`
- [ ] Elements: `HTMLButtonElement`, `HTMLAnchorElement`, `HTMLFormElement`,
  `HTMLSelectElement`, `HTMLTextAreaElement`, `HTMLCanvasElement`,
  `CanvasRenderingContext2D`
- [ ] Events: `UIEvent`, `MouseEvent`, `KeyboardEvent`, `InputEvent`,
  `FocusEvent`
- [ ] Window: `Location`, `Storage`, `setTimeout` and `setInterval`
- [ ] `SurfaceDom` and `lib.wado` follow each addition

## Known gaps

- A program cannot recover from a DOM exception: it traps.
- CI runs the glue on jsdom only, never in a browser.
- jco 1.30.0 fails an export that a listener reenters during
  `dispatch_event`.
- `SurfaceDom` neither removes a listener nor ignores one added twice.
- Every callback shape a declared `#[cm]` import takes gets its export, whether
  the program passes such a closure or not.
- A mixin's members are folded into each interface that includes it, rather
  than declared once as a trait.
- A handle has no `Display` or `serde` rule. `Inspect` writes its class's name.
- `wado-lang:web` publishes no WIT.
- An event handler attribute (`onclick`) is skipped, since its type is a
  callback in a result. `add_event_listener` covers the same events.
