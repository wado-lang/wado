# package-web TODO

This file tracks the work that brings `wado-lang:web` to practical use. The
design and the initial implementation are in
[WEP: The Web Interface for Wado](../docs/wep-2026-04-01-web.md).

## Where it stands

The pipeline works end to end: WebIDL snapshot, generated bindings and glue,
`SurfaceDom`, server-side rendering, callbacks, and a jsdom run in CI. The slice
covers ten interfaces: `EventTarget`, `Event`, `Node`, `CharacterData`, `Text`,
`Element`, `HTMLElement`, `HTMLInputElement`, `Document` and `Window`.

`wado-from-idl` reads every definition and member kind webidl2 writes. It names
each thing it does not bind on stderr, and counts them by cause.
`mise run update-package-web` prints the count for the slice.

`mise run report-webidl-coverage` prints it for the whole platform: every
interface a page can reach, and every namespace. That is the measure of what is
left before the slice can be everything:

| Cause                                           | Definitions | Members | Overloads | Arguments |
| ----------------------------------------------- | ----------- | ------- | --------- | --------- |
| a dictionary                                    | 776         | 265     | 6         | 411       |
| a constant                                      | 0           | 1261    | 0         | 0         |
| a callback in a result                          | 0           | 932     | 0         | 0         |
| a type outside the slice                        | 0           | 263     | 1         | 2         |
| a union                                         | 0           | 197     | 7         | 16        |
| `any`                                           | 0           | 128     | 0         | 32        |
| a buffer                                        | 0           | 129     | 20        | 4         |
| a namespace member                              | 0           | 133     | 0         | 0         |
| a special operation                             | 0           | 81      | 0         | 0         |
| `[HTMLConstructor]`                             | 0           | 76      | 0         | 0         |
| `object`                                        | 0           | 54      | 1         | 6         |
| a redeclared inherited method                   | 0           | 55      | 0         | 0         |
| overloads                                       | 0           | 46      | 0         | 0         |
| an iteration declaration                        | 0           | 41      | 0         | 0         |
| an enum                                         | 18          | 15      | 0         | 0         |
| `Promise<…>`                                    | 0           | 29      | 0         | 1         |
| a callback the glue cannot call                 | 0           | 19      | 0         | 9         |
| a static attribute                              | 0           | 7       | 0         | 0         |
| `record<…>`                                     | 0           | 6       | 0         | 0         |
| a list of a list, a nullable type or a callback | 0           | 3       | 0         | 1         |
| `ObservableArray<…>`                            | 0           | 4       | 0         | 0         |
| parameters passed through memory                | 0           | 1       | 0         | 2         |
| `bigint`                                        | 0           | 1       | 0         | 0         |
| `async_sequence<…>`                             | 0           | 1       | 0         | 0         |
| Total                                           | 794         | 3747    | 35        | 484       |

The command prints the number of functions that do bind above the table. How to
read the table:

- A cause is what would have to lower for the item to. A list of dictionaries
  counts under the dictionary.
- A member is counted once, by the first thing that stops it. Lowering that
  cause may move it to another row rather than bind it.
- An overload is one overload of a member another overload binds. An argument
  is a trailing optional argument a bound member leaves out, with each one
  after it.
- A callback in a result is nearly always an event handler attribute
  (`onclick`).
- A type outside the slice is a name no snapshot definition declares. Over the
  whole platform those are `CSSOMString`, `WindowProxy`, the legacy aliases
  `SVGPoint`, `SVGRect` and `SVGMatrix`, and an interface only a worker reaches.
- An enum is one with a value Wado cannot spell as a case: the empty string, as
  in `ReferrerPolicy`, or one starting with a digit, as in `"2d"`.
- A redeclared inherited method is not lost: the parent's method serves the
  child, since it has the same types or differs only in a result that is a
  resource extending the parent's. One redeclared with any other type binds
  under its interface's name, so it is not in the table: `SVGElement.className`,
  an `SVGAnimatedString` where `Element.className` is a string, is
  `svg_element_class_name`, and `class_name` on an `SvgElement` is an error
  naming it.

The report leaves out the interfaces only a worker or a worklet reaches.

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

### 5. Enums, dictionaries and constants

- [x] Lower an `enum` to a Wado `enum`, in the bindings and in the glue
- [x] `SurfaceDom` answers `ready_state` and `visibility_state`
- [ ] Name a case for the empty string and for a value starting with a digit
- [ ] Lower a `dictionary` to a `struct`
- [ ] Bind a constant

### 6. A wider slice

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

- A program compiles longer against the bindings of the whole platform than
  against the slice, though it calls the same few members: each declaration
  costs something whether a program uses it or not.
  `mise run report-webidl-coverage` writes those bindings to
  `scratchpad/webidl-all/dom.wado`.
- A union collapsed to its one typable constituent in an argument is not
  reported, though the other constituents cannot be passed.
- A program cannot recover from a DOM exception: it traps.
- A host answering an enum value the snapshot does not declare traps the
  program: jsdom's `document.visibilityState` is `"prerender"`.
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
