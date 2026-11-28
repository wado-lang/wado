# package-web TODO

This file tracks the work that brings `wado-lang:web` to practical use. The
design and the initial implementation are in
[WEP: The Web Interface for Wado](../docs/wep-2026-04-01-web.md).

## Where it stands

The pipeline works end to end: WebIDL snapshot, generated bindings and glue,
`SurfaceDom`, server-side rendering, callbacks, and a jsdom run in CI. The slice
covers the interfaces `SLICE` in `tools/webidl/snapshot.mjs` lists: the tree,
its traversal and collections, style, and canvas.

`wado-from-idl` reads every definition and member kind webidl2 writes. It names
each thing it does not bind on stderr, and counts them by cause.
`mise run update-package-web` prints the count for the slice.

`mise run report-webidl-coverage` prints it for the whole platform: every
interface a page can reach, and every namespace. That is the measure of what is
left before the slice can be everything:

| Cause                                           | Definitions | Members | Arguments |
| ----------------------------------------------- | ----------- | ------- | --------- |
| a callback in a result                          | 5           | 954     | 0         |
| `any`                                           | 3           | 158     | 35        |
| a buffer                                        | 15          | 172     | 5         |
| a namespace member                              | 0           | 133     | 0         |
| a special operation                             | 0           | 81      | 0         |
| a dictionary                                    | 23          | 49      | 6         |
| `object`                                        | 4           | 67      | 7         |
| `[HTMLConstructor]`                             | 0           | 76      | 0         |
| a redeclared inherited method                   | 0           | 56      | 0         |
| an enum                                         | 19          | 24      | 0         |
| an iteration declaration                        | 0           | 41      | 0         |
| a type outside the slice                        | 0           | 36      | 1         |
| `Promise<…>`                                    | 0           | 32      | 1         |
| a callback the glue cannot call                 | 0           | 13      | 5         |
| `record<…>`                                     | 0           | 15      | 0         |
| a union                                         | 3           | 12      | 0         |
| a list of a list, a nullable type or a callback | 5           | 7       | 1         |
| a static attribute                              | 0           | 7       | 0         |
| `ObservableArray<…>`                            | 0           | 4       | 0         |
| `bigint`                                        | 0           | 2       | 0         |
| `async_sequence<…>`                             | 0           | 1       | 0         |
| Total                                           | 77          | 1940    | 61        |

The command prints the number of functions that do bind above the table. How to
read the table:

- A cause is what would have to lower for the item to. A list of dictionaries
  counts under the dictionary.
- A definition is a dictionary or an enum with no binding. A dictionary counts
  under what stops a required member of it, and under the dictionary itself
  where no member binds or it holds itself.
- A member is counted once, by the first thing that stops it. Lowering that
  cause may move it to another row rather than bind it. A dictionary's member
  left out of its struct is a member too.
- Each overload binds under its own name, so it counts as a member of its own.
  An argument is a trailing optional argument a bound member leaves out, with
  each one after it.
- A callback in a result is nearly always an event handler attribute
  (`onclick`).
- A type outside the slice is a name no snapshot definition declares. Over the
  whole platform those are `WindowProxy`, the legacy aliases
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

- [x] Bind each overload under a name of its own, from the arguments that tell
  it apart: `Window.alert()` and `alert_with_message(message)`
- [x] Lower a union of two or more typable constituents to a `variant` where
  the guest supplies it, named after its typedef where it has one
  (`(Node or DOMString)`)
- [x] Lower such a union in a result, the glue telling the host's value apart
  by the case whose test it passes
- [x] `SurfaceDom` answers `hidden`, a union either way

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
- [x] Lower a `dictionary` to a `struct`, in the bindings and in the glue
- [x] Read parameters that cross in memory, as more than sixteen core values do
- [x] Bind an interface's constant as an associated constant of its resource
- [x] Bind a callback interface as a newtype over its closure type, holding
  its constants: `NodeFilter::SHOW_ALL`
- [x] Let a callback return a scalar or a handle, as `NodeFilter`'s does

### 6. A wider slice

- [x] Tree: `Text` and `CharacterData`; `SurfaceDom` answers `create_text_node`,
  `data` and `insert_before`
- [x] Traversal: `NodeIterator` and `TreeWalker`; `SurfaceDom` answers
  `create_node_iterator`, `create_tree_walker` and their traversals
- [x] Tree: `Comment`, `DocumentFragment`, `NodeList`, `HTMLCollection`
- [x] Style and classes: `DOMTokenList`, `CSSStyleDeclaration` and
  `CSSStyleProperties`, `DOMRect`
- [x] Canvas: `HTMLCanvasElement`, `CanvasRenderingContext2D`,
  `CanvasGradient`, `CanvasPattern`, `Path2D`, `TextMetrics`, `ImageData`
- [ ] Trusted Types: `TrustedHTML`, once
  [WEP: Union Types](../docs/wep-2026-11-28-union-types.md) settles how a
  string reaches a `(TrustedHTML or DOMString)` argument
- [ ] Elements: `HTMLButtonElement`, `HTMLAnchorElement`, `HTMLFormElement`,
  `HTMLSelectElement`, `HTMLTextAreaElement`
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
  reported, though the other constituents cannot be passed. Nor is a variant
  that leaves a constituent out.
- A union's type changes as the slice grows: `set_attribute` takes a `String`
  until `TrustedHTML` joins the slice, and a variant from then on.
- A dictionary's member is typed as a result is, so a callback is left out of
  it even where the dictionary is only an argument.
- An attribute whose union is there for its setter returns that union from its
  getter too: once `TrustedHTML` joins the slice, `inner_html` returns a
  variant whose case is always `AsString`, since the getter only ever returns a
  string. The IDL does not tell such an attribute apart.
- An optional member of a nullable type cannot be passed as `null`: its `None`
  is the member left out, for which the host reads the WebIDL default.
- Two unions that differ take one name where a typedef's name is another
  union's generated one, or where two constituents take one word, as
  `sequence<Node>` and `NodeList` do. The generator stops on it.
- No member of the slice takes more than sixteen core values, so no test runs
  parameters read from memory against a host.
- `HTMLCanvasElement.getContext` is not bound: it returns a union of every
  rendering context, and the slice holds only `CanvasRenderingContext2D`. So
  nothing reaches a `CanvasRenderingContext2D`, though its members bind.
- `SurfaceDom` answers none of the members the canvas, style and collection
  interfaces brought.
- No test runs the glue telling apart a result union's interface, sequence,
  dictionary or enum case against a host. `CanvasRenderingContext2D`'s
  `fill_style` and `stroke_style` return an interface case, but nothing reaches
  a `CanvasRenderingContext2D` while `getContext` is unbound.
- `SurfaceDom` takes the options of what it does not model and ignores them:
  `passive`, since no event is cancelled, the `is` of `create_element`, and
  those of `get_html`, `scroll_into_view` and `request_fullscreen`.
- A program cannot recover from a DOM exception: it traps.
- A host answering an enum value the snapshot does not declare traps the
  program: jsdom's `document.visibilityState` is `"prerender"`.
- CI runs the glue on jsdom only, never in a browser.
- jco 1.30.0 fails an export that a listener reenters during
  `dispatch_event`.
- `SurfaceDom` neither removes a listener nor ignores one added twice.
- `SurfaceDom` keeps every `NodeIterator` it creates for the document's life,
  and each removal visits them all: nothing drops an iterator the program no
  longer reaches.
- A callback that returns a value runs to its end before the page goes on, so
  waiting on a promise inside it traps.
- A `NodeFilter` is required, though the page accepts `null` for one that
  accepts every node: `|_| NodeFilter::FILTER_ACCEPT` says the same.
- Every callback shape a declared `#[cm]` import takes gets its export, whether
  the program passes such a closure or not.
- A mixin's members are folded into each interface that includes it, rather
  than declared once as a trait.
- A handle has no `Display` or `serde` rule. `Inspect` writes its class's name.
- `wado-lang:web` publishes no WIT.
- An event handler attribute (`onclick`) is skipped, since its type is a
  callback in a result. `add_event_listener` covers the same events.
