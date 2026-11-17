# Rill — Fine-Grained Reactivity for Wado

`wado-lang:rill` is signals and the DOM bound to them. A page is built once.
After that, a write to a signal reruns only the bindings that read it: no
virtual DOM, and no diffing of a tree.

[WEP: Reactive Signals](../docs/wep-2026-04-04-reactive-signals.md) holds the
initial design, and [`TODO.md`](./TODO.md) what the library leaves out.

The synopsis test in `src/lib.wado` holds this sample:

```wado
use { element, flush, scope, signal } from "wado-lang:rill";
use { Event, SurfaceDom } from "wado-lang:web";

let mut dom = SurfaceDom::new();
with &mut dom do {
    let count = signal(0);
    let button = element("button");
    let mut dispose = scope(|| {
        button
            .bind_text(|| `clicked ${count.get()} times`)
            .on("click", |_event: Event| count.set(count.get() + 1));
    });
    assert button.node.text_content() == Option::Some("clicked 0 times");

    count.set(2);
    flush();
    assert button.node.text_content() == Option::Some("clicked 2 times");

    dispose();
};
```

## Layout

- `src/signals.wado` — `signal`, `memo`, `observe`, `scope` and `flush`.
- `src/view.wado` — `View`, which builds an element and binds it: `text`,
  `attribute`, `child` and `children` set it once, while `bind_text`,
  `bind_attribute`, `on` and the keyed list `each` keep it current.
- `example/todo/` — a to-do list. `wado run` there prints the page after a
  short session, and `wado test` runs the session against `SurfaceDom`.

## How it works

- A signal holds its value behind a reference, so a copy of a `Signal` is the
  same signal, and reading or writing one calls no function value.
- `Track` is an effect. A computation installs a handler for it while it runs,
  and `Signal::get` reports the read to that handler. Outside a computation the
  operation's default body runs, and the read is untracked.
- The same handler collects what a computation owns: the effects it made and
  the cleanups it registered. They are disposed before it reruns, and with it.
- A write only marks its dependents. A memo marked this way forgets its value
  and marks its own dependents; an effect joins a queue. `flush` then reruns
  each queued effect once, and a memo recomputes when one of them reads it. So
  a write performs no effect, and an effect never sees half of a batch.
- A computation subscribes to a dependency when it reads it, and drops its
  subscriptions when it runs again or is disposed of. So a dependency it no
  longer reads wakes nothing. A memo is disposed of with the computation that
  made it.
- A binding writes the one place it owns, and takes it out when disposed of:
  `bind_text` a text node it appends, and `each` its rows. A reorder leaves the
  longest run of rows already in order where they are, and moves the rest.
- `on` flushes when the listener returns, so the page is consistent again
  before the browser gets control back. Any other code that writes, a listener
  added with `add_event_listener` directly included, calls `flush` itself.
