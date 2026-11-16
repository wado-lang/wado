# WEP: Reactive Signals

## Context

UI frameworks have converged on fine-grained reactivity. A signal holds a
value, a computation that reads it becomes its dependent, and a write reruns
only the dependents. The page is built once, with no virtual DOM and no diffing
of a tree.

This WEP first proposed signals as a language feature: a `reactive` keyword on
bindings, `&reactive` references, and a compiler pass that found each
computation's dependencies statically. `example/web-signals` then built signals
as a library in the language as it is: `signal`, `memo`, `observe`, `scope` and
`flush`, bindings to the DOM, a keyed list, and a to-do list on top of them. It
needed no language feature. So the question this WEP answers changed from how
the compiler tracks dependencies to whether it should.

### Prior Art

Solid, Preact Signals, Vue and Svelte 5 find dependencies as the code runs. A
computation records itself as the one running, and a signal that is read
subscribes it. Dependencies follow control flow, and a signal is a value like
any other: it crosses function and module boundaries, and lives in a list or a
field.

Svelte 3 and 4 found dependencies at compile time. What their users complained
about is the useful half:

- Reactivity stopped at the boundary the compiler could see. Code moved out of
  a component into a function or another module was no longer reactive.
- State shared between components needed a second mechanism, stores, with its
  own rules.
- What made a statement reactive was a syntactic matter, and hard to predict.

Svelte 5 moved to signals tracked at run time. Solid's compiler never tracked
dependencies. It compiles templates, which is where its speed comes from.

## Decision

### Signals Are a Library

Signals are a library that finds dependencies as the code runs. The language
gains no `reactive` keyword, no `&reactive` reference, and no static dependency
analysis. `reactive` is an identifier like any other.

Static analysis sees a graph only where it is closed within one function. The
to-do list already leaves that: each item carries a signal of its own, created
as items arrive, kept in a list, and read by a row built in another function.
That part needs a graph at run time whatever the compiler does. A compile-time
graph would then be a second mechanism beside it, for the one case that is
cheap already. It would also subscribe a computation to every signal its text
mentions, on whichever branch.

The language stays smaller, and what the library needs from it is general:
closures, effect handlers, and resources.

### Model

- A signal is a value that tells its readers when it is written. It holds its
  state behind a reference, so a copy of a signal is the same signal, and
  reading or writing one calls no function value.
- A memo is a value computed from others. It recomputes only when one of them
  was written and someone then reads it.
- An effect, made by `observe`, runs once and again whenever something it read
  was written. `effect` is a keyword, which is why the function has another
  name.
- A computation owns what it creates while it runs: the effects, the memos and
  the cleanups it registered. They are disposed of before it reruns, and with
  it. `scope` runs a body that owns what it creates, and returns what disposes
  of it all.
- A computation subscribes to what it reads when it reads it, and drops its
  subscriptions when it runs again or is disposed of. So a dependency it no
  longer reads wakes nothing.

Reading reports to the running computation through an effect interface, whose
operations have default bodies. A computation installs a handler while it runs.
Outside one the default body runs, so a read is untracked, and the function
that reads needs no handler above it.

### Updates

A write only marks. A memo that is marked forgets its value and marks its own
dependents, and an effect that is marked joins a queue. `flush` then reruns each
queued effect once, until none is marked, and a memo recomputes when one of
them reads it.

So a write performs no effect, and several writes are one batch. An effect
never sees half of a batch, and never sees one memo updated beside another that
is stale, since a memo recomputes on the read that needs it.

A listener bound through the library flushes when it returns. Every entry into
a program from the browser is a listener, so the page is consistent again
before the browser gets control back. Code outside a listener calls `flush`
itself.

### Templates

Turning markup into the calls that build a page once and bind its holes is
where a compiler helps a signals library most. That belongs to the markup
dialect
([WEP: Markup Dialect](./wep-2026-08-29-markup-dialect.md)), a generator, and
not to the language.

## Known gaps

- A tracked read dispatches an effect operation, and each run of a computation
  installs a handler. What either costs is not measured.
- A leading `<` in expression position is still reserved for markup. This WEP
  reserved it for JSX and no longer uses it, and
  [Overload Resolution](./wep-2026-07-31-overload-resolution.md) rules out
  `<Type as Trait>::method` on that account.
- The library lives in `example/web-signals`. Nothing publishes it as a
  package.
- A memo does not compare its new value with the old one, so its dependents
  rerun even when the value came out the same.
- A memo owns nothing. Registering a cleanup inside one traps, and so does
  making an effect or another memo there.
- Disposing of a scope leaves its listeners attached.
- A flush happens when a listener returns or when the program asks. Nothing
  schedules one after the current task, as a microtask would.
- The keyed list puts its rows back in order by appending every one of them,
  since `SurfaceDom` does not answer `insert_before`.
- A text binding owns its element's whole content, since `SurfaceDom` has no
  `Text` node.
- No template syntax exists. A page is built by calls.
- A field holding a function value cannot be called in place. It is copied to
  a local first.
