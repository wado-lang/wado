# Rill TODO

Open work, stated as what is missing. An entry says the symptom and what was
already measured, not how to close it. Closed work belongs in commit history.

## Runtime

- [ ] A tracked read dispatches an effect operation, and each run of a
  computation installs a handler. What either costs is not measured.
- [ ] A memo owns nothing. Registering a cleanup inside one traps, and so does
  making an effect or another memo there.
- [ ] Disposing of a scope stops its bindings, takes its rows out of an `each`,
  the text nodes out of a text child and the subtree out of a `show` or a
  `switch`. It leaves every other node where it is, and its listeners
  attached.
- [ ] A flush happens when an `on` listener returns or when the program asks.
  Nothing schedules one after the current task, as a microtask would.

## Declarative UI

- [ ] No template syntax exists. A page is built with `View`.
  [WEP: Markup Dialect](../docs/wep-2026-08-29-markup-dialect.md) holds
  what is decided about one.
- [ ] `show` and `switch` render a subtree anew each time they show it. A
  subtree that is costly to build has no way to be kept while hidden.
- [ ] A closure child or attribute must perform `Track`. A closure that reads
  no signal matches no `IntoChild` or `IntoAttr` impl, since a function type's
  impl is matched on its effects exactly.

## Static generation

- [ ] No app is published. [`static-generation.md`](./docs/static-generation.md)
  holds the design and its roadmap.
