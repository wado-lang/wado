# web-signals — Fine-Grained Reactivity as a Library

A to-do list whose page is built once. After that, a write to a signal reruns
only the bindings that read it: no virtual DOM, and no diffing of a tree.

It is the library
[WEP: Reactive Signals](../../docs/wep-2026-04-04-reactive-signals.md)
describes: dependencies are found as the code runs, which needs no language
feature.

```sh
wado run     # prints the page after a short session
wado test    # runs the session against SurfaceDom
```

## Layout

- `src/signals.wado` — `signal`, `memo`, `observe`, `scope` and `flush`.
- `src/view.wado` — bindings to the DOM: `bind_text`, `bind_attribute`, `on`
  and the keyed list `each`.
- `src/app.wado` — the to-do list.

## How it works

- A signal is a pair of closures over one cell, so a copy of a `Signal` is the
  same signal.
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

## What it leaves out

- A memo does not compare its new value with the old one, so its dependents
  rerun even when the value came out the same.
- A memo owns nothing: a cleanup registered inside one traps, and so does
  making an effect or another memo there.
- Disposing of a scope stops its bindings and takes its rows out of an `each`.
  It leaves every other node where it is, and its listeners attached.
- `each` puts the rows back in order by appending every one of them, since
  `SurfaceDom` does not answer `insert_before`.
- A text binding owns its element's whole content, since the slice has no
  `Text` node.
