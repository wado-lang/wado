# Rill Design

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
- `hydrate` builds a view on the served HTML, as
  [`static-generation.md`](./static-generation.md) describes.
