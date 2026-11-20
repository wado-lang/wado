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
            .bind_text(|| `clicked ${count.get()} times`)
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

## Layout

- `src/signals.wado` — `signal`, `memo`, `observe`, `scope` and `flush`.
- `src/view.wado` — `View`, an element as it is to be built, new from `element`
  or adopted with `View::of`. `text`, `attribute`, `child` and `children` set
  it once, `bind_text`, `bind_attribute`, the keyed list `each` and the
  conditional `show` and `switch` keep it current, `on` listens on it, and
  `with_node` hands it over. `build` builds a view; `hydrate` builds one on the
  served HTML, as [`static-generation.md`](./static-generation.md) describes.
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
- A binding writes the one place it owns, and takes it out when disposed of:
  `bind_text` a text node it adds, and `each` its rows. A reorder leaves the
  longest run of rows already in order where they are, and moves the rest.
- `on` flushes when the listener returns, so the page is consistent again
  before the browser gets control back. Any other code that writes, a listener
  added with `add_event_listener` directly included, calls `flush` itself.
