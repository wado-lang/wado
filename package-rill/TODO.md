# Rill TODO

Open work, stated as what is missing. An entry says the symptom and what was
already measured, not how to close it. Closed work belongs in commit history.

## Runtime

- [ ] A tracked read dispatches an effect operation, and each run of a
  computation installs a handler. What either costs is not measured.
- [ ] A memo does not compare its new value with the old one, so its dependents
  rerun even when the value came out the same.
- [ ] A memo owns nothing. Registering a cleanup inside one traps, and so does
  making an effect or another memo there.
- [ ] Disposing of a scope stops its bindings, takes its rows out of an `each`
  and the text nodes out of a `bind_text`. It leaves every other node where it
  is, and its listeners attached.
- [ ] A flush happens when an `on` listener returns or when the program asks.
  Nothing schedules one after the current task, as a microtask would.

## Declarative UI

- [ ] No template syntax exists. A page is built with `View`.
  [WEP: Markup Dialect](../docs/wep-2026-08-29-markup-dialect.md) holds
  what is decided about one.
- [ ] A child shown only on a condition has no form: `children` takes a list,
  and a list literal holds no `if`.
- [ ] No binding shows or swaps a subtree as a signal changes. `each` is the
  only binding that adds and removes children.
