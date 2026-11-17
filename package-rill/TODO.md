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
- [ ] Disposing of a scope leaves its listeners attached.
- [ ] A flush happens when an `on` listener returns or when the program asks.
  Nothing schedules one after the current task, as a microtask would.

## DOM

- [ ] The keyed list `each` puts its rows back in order by appending every one
  of them, since `SurfaceDom` does not answer `insert_before`.
- [ ] A text binding owns its element's whole content, since `SurfaceDom` has no
  `Text` node.

## Declarative UI

- [ ] No template syntax exists. A page is built by calls.
  [WEP: Markup Dialect](../docs/wep-2026-08-29-markup-dialect.md) holds
  what is decided about one.
