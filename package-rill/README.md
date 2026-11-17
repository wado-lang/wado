# Rill — Fine-Grained Reactivity for Wado

`wado-lang:rill` is signals and the DOM bound to them. A page is built once.
After that, a write to a signal reruns only the bindings that read it: no
virtual DOM, and no diffing of a tree.

[WEP: Reactive Signals](../docs/wep-2026-04-04-reactive-signals.md) holds the
initial design, and [`TODO.md`](./TODO.md) what the library leaves out.

```wado
use { bind_text, element, on, signal } from "wado-lang:rill";
use { Dom, Element, Event } from "wado-lang:web";

fn counter(root: Element) with Dom {
    let count = signal(0);
    let button = element("button");
    bind_text(button, || `clicked ${count.get()} times`);
    on(button, "click", |_event: Event| count.set(count.get() + 1));
    root.append_child(button);
}
```

## Layout

- `src/signals.wado` — `signal`, `memo`, `observe`, `scope` and `flush`.
- `src/view.wado` — bindings to the DOM: `bind_text`, `bind_attribute`, `on`
  and the keyed list `each`.
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
- `on` flushes when the listener returns. Every entry into the program from the
  browser is a listener, so no microtask is needed.
