# Rill — Fine-Grained Reactivity for Wado

`wado-lang:rill` is signals and the DOM bound to them. A page is built once.
After that, a write to a signal reruns only the bindings that read it: no
virtual DOM, and no diffing of a tree.

[WEP: Reactive Signals](../docs/wep-2026-04-04-reactive-signals.md) holds the
initial design, and [`TODO.md`](./TODO.md) what the library leaves out.

The synopsis test in `src/lib.wado` holds this sample:

```wado
use { build, element, flush, scope, signal } from "wado-lang:rill";
use { Element, Event, SurfaceDom } from "wado-lang:web";

let mut dom = SurfaceDom::new();
with &mut dom do {
    let count = signal(0);
    let mut button: Option<Element> = null;
    let mut dispose = scope(|| {
        let view = element("button")
            .children([|| `clicked ${count.get()} times`])
            .on("click", |_event: Event| count.set(count.get() + 1));
        button = Option::Some(build(view));
    });
    assert button.unwrap().text_content() == Option::Some("clicked 0 times");

    count.set(2);
    flush();
    assert button.unwrap().text_content() == Option::Some("clicked 2 times");

    dispose();
};
```

## Building a page

A `View` is an element as it is to be built: `element("li")` makes a new one,
and `View::of(node)` adopts one already in the page. Its methods chain, and
each adds a step that `build` or `hydrate` runs later, in document order.

- `attr(name, value)` sets an attribute. A `String` sets it once. A closure
  keeps it current: one answering `String` sets it, and one answering
  `Option<String>` takes it out while it answers `None`.
- `children([...])` adds children, in order. Each is a `View`, a `String` for
  fixed text, a closure answering `String` for text kept current, or one of
  the control-flow children below.
- `on(type, handler)` listens for an event.
- `node_ref(r)` records the element in `r`, made with `node_ref()`. `r.get()`
  reads it once the view is built.

The control-flow children stand among the others, where they are written:

- `show(when, render)` holds the subtree `render` makes while `when` answers
  `true`.
- `switch(key, render)` holds the subtree `render` makes for the current key,
  and makes another when the key changes.
- `each(items, render)` holds a row per item, keyed by equality. A reorder
  leaves the longest run of rows already in order where they are, and moves
  the rest.

A control-flow child puts back what it holds only when it updates: `show` when
`when` changes its answer, `switch` when the key changes, and `each` each time
its items run. Until then, a subtree that other code removed stays removed, so
a run that changes nothing checks no placement.

```wado
use { Signal, each, element, show, signal } from "wado-lang:rill";

let items: Signal<List<String>> = signal(["a", "b"]);
let list = element("ul").children([
    element("li").children(["first"]),
    each(|| items.get(), |item: String| element("li").children([item])),
    show(|| items.get().is_empty(), || element("li").children(["Nothing yet."])),
]);
```

### Without `dyn`

Wado has no `dyn`, so `children` cannot take a list of mixed values. It takes a
pack instead: `children<..C: IntoChild>(kids: [..C])` sees each element at its
own type, and the loop over the pack unrolls at compile time. `IntoChild` turns
each into a `Child`, which holds one build step of a single closure type. The
steps go into a `List`, and from there every child is alike.

The closures are matched by their type, the effects they perform included:
`IntoChild` and `IntoAttr` have impls for `fn` and `fn mut` closures that
perform `Track`. A closure that reads no signal performs nothing and matches
no impl, so a value that never changes is passed as a `String`.

## Layout

- `src/signals.wado` — `signal`, `memo`, `observe`, `scope` and `flush`.
- `src/view.wado` — `View` and its children, `build`, and `hydrate`, which
  builds a view on the served HTML as
  [`static-generation.md`](./static-generation.md) describes.
- `example/todo/` — a to-do list, published as a static site. `site.wado`
  renders its initial page and bundles `src/main.wado`, the browser program
  that hydrates it; `mise run rill-todo-site` builds the site into
  `build/rill-todo`, and `mise run rill-todo-demo` serves it as well.
  `wado test` runs a session against `SurfaceDom`, built and hydrated.

## How it works

- A signal holds its value behind a reference, so a copy of a `Signal` is the
  same signal, and reading or writing one calls no function value.
- `Track` is an effect. A computation installs a handler for it while it runs,
  and `Signal::get` reports the read to that handler. Outside a computation the
  operation's default body runs, and the read is untracked.
- The same handler collects what a computation owns: the effects it made and
  the cleanups it registered. They are disposed before it reruns, and with it.
- A write only marks its dependents. A memo marked this way marks its own
  dependents in turn; an effect joins a queue. `flush` then reruns each queued
  effect once, and a memo recomputes when one of them reads it. So a write
  performs no effect, and an effect never sees half of a batch.
- A signal counts its writes, an equal value included, and a memo counts the
  times its value came out different. A computation keeps the counts it read,
  so a queued effect, or a marked memo, whose counts all still hold skips its
  run. A memo computes a value and does nothing else: a write or a flush while
  one computes, or while its value is compared with the last, traps.
- A computation subscribes to a dependency when it reads it, and drops its
  subscriptions when it runs again or is disposed of. So a dependency it no
  longer reads wakes nothing. A memo is disposed of with the computation that
  made it.
- A binding writes the one place it owns, and takes it out when disposed of: a
  text child the text node it adds, and `show`, `switch` and `each` what they
  hold. Each of the three keeps an empty text node after what it holds, to
  know where that ends among its siblings.
- `on` flushes when the listener returns, so the page is consistent again
  before the browser gets control back. Any other code that writes, a listener
  added with `add_event_listener` directly included, calls `flush` itself.
