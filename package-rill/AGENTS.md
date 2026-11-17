# Rill Development Guide

Rill is fine-grained reactivity for Wado: signals, and the declarative UI bound
to them. The rules below hold for every change; the rest lives in companion
docs:

- [`README.md`](./README.md) — what Rill offers, and how it works.
- [`TODO.md`](./TODO.md) — open work.
- [WEP: Reactive Signals](../docs/wep-2026-04-04-reactive-signals.md) — the
  initial design and why signals are a library. It is complete and no longer
  edited: what Rill decides from here on is written in this directory.

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
