# Rill Development Guide

Rill is fine-grained reactivity for Wado: signals, and the declarative UI bound
to them. The `wado-package-standard` skill says where each kind of file goes.
The rules below hold for every change; the rest lives in companion docs:

- [`README.md`](./README.md) — what Rill offers.
- [`docs/design.md`](./docs/design.md) — how it works.
- [`TODO.md`](./TODO.md) — open work.
- [`docs/static-generation.md`](./docs/static-generation.md) — publishing an
  app as static files that the browser program hydrates.

## Architecture

Signals, memos, effects and scopes form the reactive core; `Track` is the
effect a computation handles to learn what it read. The view layer builds DOM
nodes on `wado-lang:web` and binds each to the signals it reads, and `hydrate`
builds the same bindings on served HTML.

## Sources

No other framework's source is read, whatever its license. A rule comes from
what Rill's own documents decide, and another framework may be run as a
black-box oracle.

## Reentrancy

A callback Rill runs may call back into Rill: a cleanup may dispose of the scope
that owns it, and an effect's body may write, flush or dispose of itself. So a
list of callbacks is detached from its owner before any of them runs, and state
a callback can reach is read again after it returns. A change to `run`,
`release`, `flush` or a disposer comes with a test that reenters it.
