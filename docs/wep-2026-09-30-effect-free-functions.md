# WEP: A Function Without `with` Performs No Effects

## Context

The specification said a function without a `with` clause performs no effects.
The effect system did not hold that promise, in three ways.

- An operation of a user-defined interface demanded nothing of its caller. A
  function declaring nothing could call `Counter::next()`, and the handler
  answering it could perform any I/O, paid for where the handler was installed.
  Two calls of such a function returned different values.
- Installing a handler demanded nothing either. A function declaring nothing
  could install a handler whose methods declared `with Random` and reach the
  host through it.
- A handler method held the effect it handled without declaring it, so a handler
  that delegated to the host needed no capability anywhere.

The effect system also tracks no global: a function reads and writes a
`global mut` without declaring anything.

## Decision

A function without a `with` clause performs no effects. The effect system holds
this by the rules below.

The specification speaks of effects and nothing else. It does not call such a
function pure: the word promises more than Wado gives, such as never trapping
and never writing output.

### What an effect is not

A trap is not an effect. A `panic`, a failed `assert` or any other trap may
happen in a function that declares no effects.

The optimizer may remove a call to a function that declares no effects when its
result is unused. It does not remove a call that may trap, or one that may not
return. Removing either would change what the program does.

### Operations demand their interface

Calling an operation of any interface demands that interface.
`Counter::next()` needs `with Counter` in its caller, and
`with Counter => h do { … }` grants `Counter` to its body.

The dependency a handler injects is now visible in every signature it passes
through. That is the cost, and it is the point: a signature says everything its
function can reach.

### Installing a handler demands what the handler performs

A handler runs on behalf of the code that installs it. So `with E => h do`
demands every effect `h`'s methods declare, and `E` itself where the block ends
in `..forward`, which passes operations to the next handler out.

A handler method holds only what it declares, so one that delegates to the next
handler out declares `with E`.

### Waiting where the call starts

A function or closure that starts an async call also holds the effects
`AsyncCall`'s methods declare, so it waits on the call with nothing more to
declare. A closure is a body of its own, so a call it starts grants nothing to
the function around it.

### Globals

Reading or writing a `global mut` is an effect. The details, such as which
effect a global access demands, are not decided, so they are a known gap of
[WEP: Global Variables](./wep-2026-01-27-global-variables.md).

### `#[ambient]` stays

`#[ambient]` exempts a body from effect checking. The optimizer may remove a
call to an ambient function whose result is unused. Ambient output such as
`log_stderr` is therefore best-effort: it may not appear.

## Roadmap

1. Installing a handler demands what the handler performs, and a delegating
   handler method declares the effect it delegates. Done: a function declaring
   nothing no longer reaches a capability through a handler it installs.
2. An operation of a user-defined interface demands its interface. Done: the
   compiler rejects a call without it, and the standard library and the
   packages declare it where they call one. An operation's default body holds
   its own interface, and a binding on a `with` line holds what the bindings
   before it install.
3. The specification and the diagnostics speak of effects, not purity. Done:
   [Effect System](./spec-effects.md) states the rules above, and a position
   that admits no effects reports `EFFECT_NOT_ALLOWED`.
4. The standard library is effect-checked. Done: `core:*` compiles under the
   rules above.

## Known gaps

- A global access performs no effect yet. Which effect it performs is
  undecided (see [Globals](#globals)).
- The optimizer does not yet remove a call by its callee's declared effects.
