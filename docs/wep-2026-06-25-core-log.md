# WEP: Structured Logging and Tracing Standard Library (`core:log`)

Status: Implemented, less the default sink install (see [Default sink and scoped overrides](#default-sink-and-scoped-overrides))

## Context

Wado has best-effort ambient output (`log_stdout` / `log_stderr`, see
[Ambient Logging](./wep-2026-01-12-ambient-logging.md)) and strict effectful I/O
(`println` / `eprintln`), but no logging facility: no levels, structured fields,
spans, pluggable sink, source location, or filtering. This WEP designs the
full-set logger and tracer.

Rust `tracing` and Go `slog` converged on structured key–value fields, a facade
decoupled from its backend, spans as timed scopes, composable layers, layered
filtering, and zero work when disabled. Both reach it through what Wado lacks by
design — macros, dynamic dispatch, variadic `...any` — so each maps onto a
different mechanism here.

| Concern                   | `tracing` / `slog`             | Wado mechanism                                                        |
| ------------------------- | ------------------------------ | --------------------------------------------------------------------- |
| Backend abstraction       | `Subscriber` / `Handler` (dyn) | the `Log` effect; sinks are effect handlers (static dispatch)         |
| Layer composition         | `Layer` stack                  | nested effect handlers (`with Log => &layer do`), forwarding to outer |
| Spans / scoped context    | spans, `Logger.With`           | first-class `Span` values entered for a scope                         |
| Compile-time level filter | `max_level_*`                  | `#[param]` compile-time global + constant-fold + DCE                  |
| Process-wide level filter | `log::max_level()`             | a `core:log`-owned mutable global, set by `set_log_level`             |
| Per-callsite filter       | `EnvFilter`                    | a filter layer via `Log::enabled`                                     |
| Source location           | `file!()` / `#[track_caller]`  | default args + call-site `#file` / `#line` / `#function`              |
| Structured fields         | macros / `...any`              | an anonymous struct bounded by `Serialize`                            |
| Field/event encoding      | `slog` serializer / `Visit`    | `core:serde`                                                          |
| Timestamp                 | subscriber adds it             | sink config (default on), via `wasi:clocks` `SystemClock`             |

Thesis: a subscriber is an effect, layers and spans are nested handlers, and
filtering is a three-tier gate whose two cheap tiers never reach the effect.

## Decision

### Levels

`Level` is a plain `enum` — `Trace`, `Debug`, `Info`, `Warn`, `Error` — ordered
by declaration, so `level >= threshold` needs no cast. A "threshold" is the
lowest level emitted: at `Info`, `Trace` and `Debug` are off. The lowercase case
name is the wire spelling, and the one a directive and `-D log.level=` accept.

### Filtering — three tiers

Every level wrapper applies one gate, `enabled(level)`, which is three checks,
cheapest first, each strictly narrowing what the one before admitted. A wrapper
passes its level as a literal, so the fold tier 1 depends on survives inlining;
`enabled` is public, so a caller spells the same gate around fields too expensive
to build:

```wado
if enabled(Level::Debug) { debug(`state`, { snapshot: expensive_snapshot() }); }
```

Tier 1 — compile time. A `#[param]` global carries the threshold, so the
comparison against the wrapper's literal level folds and the body reduces to an
early return. Interprocedural DCE then removes the call and its pure arguments,
message construction included. The parameter is a `String` so the threshold is
spelled by name (`-D log.level=info`), and `from_env = "WADO_LOG_LEVEL"` reads
the build environment, not the runtime one. Its default admits every level, so a
build says what it strips rather than what it keeps. `#[param]` accepts only
built-in types today, hence the conversion through `level_from_str` — see
[Language and optimizer requirements](#language-and-optimizer-requirements).

Tier 2 — process-wide runtime threshold: a global read and a comparison, no call
and no allocation. The threshold belongs to `core:log`, not to the installed
sink, and is set explicitly through `set_log_level` (read back with `log_level`)
— the bootstrap reads `WADO_LOG`, an application may call it at any time — so no
sink contract and no cache invalidation exist to get wrong. A layer narrows
further through tier 3.

Tier 3 — the subscriber. `Log::enabled` runs the installed layer stack, so a
`Filter` layer's per-target directives decide. It costs one indirect call, on
what the first two tiers admitted.

No branch hint is emitted: whether a deployment logs is genuinely open, and a
static hint cannot beat the CPU's predictor on a branch this consistent.

### Types

An `Event` is `Metadata` — level, target, span name, file, line — plus a message,
a field payload and an optional parent `SpanId` (`u64`); `SpanAttrs` pairs the
same metadata with a span's fields. `target` defaults to the caller's
`#function`, `file` and `line` to `#file` and `#line`, all resolved at the call
site by default arguments rather than by a macro.

Fields are passed as an anonymous struct bounded by `Serialize` and boxed into a
single `core:value::Value` at the facade, before the effect boundary, so the
`Log` effect stays non-generic:

```wado
info(`user logged in`, { user_id: id, ip: ip });
```

The field parameter defaults to an empty struct (`NoFields`, which serializes to
`{}`), so `info(msg)` logs with no fields; the default type parameter supplies
the type for the omitted argument. Anonymous structs derive `Serialize` through
[bound-driven derivation](./wep-2026-06-25-trait-derivation.md), so no `derive`
form appears at the call site.

### The `Log` effect (subscriber)

An effect mirroring `tracing`'s `Subscriber`, simplified by GC (no span
refcounting): `enabled`, `current_span`, `new_span`, `record_fields` (fields onto
an open span), `follows_from` (a non-parent causal link), `enter`, `exit`,
`close`, `event`. Operations are best-effort and never fail the program; sinks
swallow their own write and serialize errors.

`enabled` takes a `Level`, not a `&Metadata`: the gate must not force the caller
to build a `Metadata` for an event about to be dropped. A layer filtering on
`target` reads it from the `Event` in `event` instead.

### Events

Free functions; location defaults resolve at the caller. The level wrappers own
the filtering — each knows its level as a compile-time literal — and forward to
the raw `event` emitter, which stamps the metadata, boxes the fields, and parents
the event to the subscriber's current span. A direct caller of `event` (a dynamic
`level`, say) opts out of the static gate.

### Spans

A span is a first-class, re-enterable value carrying only its id, with `record`
and `follows_from` methods; `current()` returns the innermost entered one. Entry
uses the `in_span` closure, so `exit` and `close` run on every exit path
including an early `return` from the body. Its body parameter is `fn mut`
(`fn <: fn mut`), so a scope that mutates what it captures enters a span like any
other.

The subscriber tracks the current-span stack from `enter` / `exit`, so `current()`
and event parenting need no separate global. Entry is lexical, so there is no
"guard held across an await" footgun. `close` fires as `in_span` leaves the
scope, or from an explicit `span.close()` on a span opened without it; a GC that
runs no finalizers cannot fire it on unreachability, and fmt sinks ignore it
anyway.

### Subscribers and layers

A sink or layer is `impl Log`; layers nest, each handling what it cares about and
delegating the rest outward with `..forward` ([Effect Handler](./wep-2026-04-11-effect-handler.md)). A sink is a layer that forwards
nothing, so a forwarded operation terminates there instead of trapping. A test
sink that must never see an operation uses `..trap`.

Four terminal sinks ship: `TextSink` and `JsonSink` write one line per event to
stderr, `NopSink` drops everything (for a program whose libraries log but which
must not), and `CaptureSink` keeps events and spans in memory. Each carries the
bookkeeping every terminal sink needs — fresh span ids and the current-span stack
that parents events — through one shared internal helper. Two layers ship:
`Context` prepends fixed fields to every event passing through (slog's `With`),
and `Filter` applies per-target directives; each implements only what it changes
and `..forward`s the rest.

`Filter`'s `enabled` sees a level and no target, so its gate is the floor over
every directive — the lowest level any of them admits — not the unmatched-target
default. A narrower answer would drop events its `event` would go on to admit.

`CaptureSink` is the testing sink: its events and spans are the captured values,
and an `ops` list records the span lifecycle in order (`new:name#id`, `enter:id`,
`exit:id`, `close:id`, `record:id:key`, `follows:id<-cause`), so a test asserts on
a `List<String>` rather than on output. `TextSink` and `JsonSink` expose the line
they would write as `render`, which is both how they are tested and the hook for
sending the same format somewhere other than stderr.

Compose by nesting `with`, innermost last — a `Filter` installed inside a
`TextSink` narrows what that sink writes.

`Span` carries only its id, so a sink wanting per-span data keeps its own map
keyed by `SpanId` (`tracing`'s `Registry`, hand-rolled).

### Output formats

`TextSink` writes one line per event, fields appended as `key=value`, omitting
any part its configuration turns off:

```
2026-08-08T11:47:08.204Z INFO 42 handle_request: user logged in user_id=7 ip=127.0.0.1
```

`JsonSink` writes JSONL through `json::to_string`, one object per line:

```json
{"ts":"2026-08-08T11:47:08.204Z","seq":42,"level":"info","target":"handle_request","message":"user logged in","span":3,"fields":{"user_id":7,"ip":"127.0.0.1"}}
```

A parent span is written as `span=3` in text, `"span":3` in JSON; `location`
appends `at file:line` to a text line and `file` / `line` keys to a JSON object.
A key a switch turns off is absent rather than null.

Both write through the ambient `log_stderr`, so a sink needs no `Stderr` in its
signature and installs in any world — a world without stderr degrades to a no-op
instead of a trap, matching the never-fail rule. The timestamp is the one part
needing a capability, and reaches it the same way: `Log`'s operations declare no
effects, so a sink cannot declare the clock read on them either, and it goes
through an `#[ambient]` helper. `timestamp: false` never reaches that call, which
is what makes a sink installable where no clock is.

### Runtime filter directives

`Filter` parses an `EnvFilter`-style comma-separated list, from `WADO_LOG` or any
string an application supplies:

```
info,core:json=debug,app::db=trace
```

A bare level is the default for unmatched targets; `target=level` overrides it
for targets under that prefix, longest matching prefix winning. A malformed
directive is skipped — a typo in an environment variable must not stop a program
from starting.

### Timestamp and sequence

The timestamp is owned by the sink, not `Event`, and defaults on: container and
collector stamps record ingestion time, drift under buffering, and not every
target has a collector. A monotonic `seq` counter (default on) preserves
intra-process order without a clock, and stays useful when `timestamp: false`
hands stamping to the container.

### Default sink and scoped overrides

`Log` is an ordinary effect, so an inner `with Log => &sink do` overrides the
outermost handler for a scope. The facade functions are `#[ambient]`, so
performing `Log` adds no `with Log` to callers — logging is callable anywhere
without infecting signatures.

Something must own the default install, since an operation with no handler traps.
Not the library: an `interface` operation has no default body to fall back to, so
`core:log` alone cannot make an uninstalled `info()` degrade instead of trap.
Until the compiler side lands, a program that logs installs its sink itself —
one `with Log => &mut TextSink {} do` around the entry body, the same shape every
other effect takes.

The export shim the compiler already synthesises around an entry point is where
the default belongs: for a program whose module graph reaches `core:log`, the
shim would install the default sink and seed `set_log_level` from `WADO_LOG`
before calling the entry function.

TODO: decide the default sink per world — `TextSink` everywhere is the obvious
choice, but `wado test` wants output attributed to the failing test, which may
mean `CaptureSink` under the test world.

### Error handling and reentrancy

Operations return `()`; sinks swallow errors; logging never aborts. Reentrancy (a
sink, or a `Serialize` it calls, logs again) doesn't loop: handler bodies run in
the outer scope, so a re-entrant log forwards outward and terminates at the
non-logging default. A `core:log`-owned depth counter backstops a pathological
self-reinstalling handler, dropping the event past a small fixed depth —
deliberately, since the alternative is an unbounded chain.

### Module surface

`core:log` lives in `wado-compiler/lib/core/log.wado`, its tests alongside in
`log_test.wado`, exercised through `CaptureSink` so assertions read the events
rather than captured output. The generated reference is
[`core:log`](./stdlib-core-log.md).

Exported: `Level`, `SpanId`, `Metadata`, `Event`, `SpanAttrs`, `Span`,
`NoFields`; the facade `trace` / `debug` / `info` / `warn` / `error` / `event` /
`enabled`; `span` / `current` / `in_span`; `set_log_level` / `log_level` /
`level_from_str`; the `Log` effect; sinks `TextSink` / `JsonSink` / `NopSink` /
`CaptureSink`; layers `Context` / `Filter` with `Directive` and
`parse_directives`.

### Async semantics

The current-span stack rides the effect-dispatch state, process-global today —
exact within one synchronous scope, wrong across concurrent tasks. So automatic
propagation is single-scope only, and cross-task is explicit: carry the `Span`
value and re-enter it (`tracing`'s `Instrument`, by hand). Per-task dispatch state
waits on WASI threads / Wasm stack switching, and adds no API surface when it
lands.

## Language and optimizer requirements

The design reuses `#[ambient]`, effect handlers, default arguments with call-site
location literals, `#[param]`, anonymous structs, and `core:value::to_value`.
Two optimizer gaps remain, both tracked in
[optimizer.md](./optimizer.md#not-yet-implemented) and both serving more than
logging.

Forwarding a local bound to a global read. `LOG_STATIC_LEVEL` is derived from a
`#[param]` global, so tier 1 folds only if that global's value reaches the
comparison. Read directly it does; through a helper's parameter it does not,
because a `String` binding is a deep copy no pass removes. Until that lands a
`#[param]` routed through a helper decides its gate at run time — reported rather
than silent, by [optimizer remarks](./wep-2026-06-03-optimizer-remarks.md).

`core:log` spells the conversion as `impl LenientFromStr for Level`
([Lenient String Parsing](./wep-2026-06-22-lenient-from-str.md)) with
`level_from_str` as its infallible wrapper. When `#[param]` gains compile-time
evaluation of `LenientFromStr` over arbitrary types
([NIR interpreter](./wep-2026-04-27-nir-interpreter.md)), the declaration becomes
`#[param] global LOG_LEVEL: Level = Level::Trace` and the wrapper is deleted.

Sinking pure definitions into a branch. Tier 2 reduces to a global read and a
compare, but a caller builds the message template and field struct in front of
it, so the disabled path pays two allocations for a branch it does not take.
Side-effecting arguments are never eliminable — guard those with `enabled()`.

Optionally, native span sugar: `with s do { … }` desugaring to `enter; B; exit`
with `exit` on every exit path would replace the `in_span` closure. Ergonomic
only.

## Consequences

### Benefits

- A full-set logger and tracer (events, spans, layered subscribers, three-tier
  filtering) built from existing language features.
- Layer composition, scoped context, and automatic context restore from
  effect-handler nesting — static dispatch throughout.
- `#[ambient]` keeps logging out of signatures, so one sink installed at the entry
  serves every call under it. Caller source location without macros; testable
  with a capturing sink.
- The two cheap tiers keep the disabled path off the effect-dispatch path, so the
  common case is a fold or a global read.

### Trade-offs

- `#[ambient]` hides a sink's I/O from signatures (the existing ambient-logging
  trade-off), and an operation with no handler traps — so the entry point must
  install a sink, by hand until the shim does it.
- Eager message: the optimizer, not a macro, is what drops it. Tier 1 already
  does; tier 2 needs the sinking pass, and `enabled()` is the manual guard until
  then.
- Fields box through `core:value` at the facade. Erased serde would remove the
  boxing; it waits on measurement.
- `set_log_level` is process-wide. Per-target and per-span filtering live in a
  layer and pay tier 3.
- Automatic span propagation is single-scope; cross-task is explicit until the
  async story settles.

### Prerequisites

- [x] `core:value::to_value` (direct `Value`-building serializer).
- [x] Anonymous structs with bound-driven `Serialize` derivation.
- [x] `#[param]` with compile-time constant folding and DCE.
- [x] Span scoping via the `in_span` closure.
- [x] `..forward` effect forwarding.
- [x] A remark when a compile-time parameter still decides a branch.
- [x] `core:log` itself — see [Module surface](#module-surface).
- [ ] The default sink install in the entry shim — see
      [Default sink and scoped overrides](#default-sink-and-scoped-overrides).
- [ ] Forwarding a local bound to a constant global read.
- [ ] Sinking pure definitions into the branch that uses them.
- [ ] Optional: native `with <span> do { … }` sugar.
- [ ] Optional: erased serde for field passing (performance-gated).
- [ ] Automatic cross-task current-span propagation (async-gated).

## Alternatives Considered

### Effect without an `#[ambient]` facade

A plain `Log` effect forces `with Log` through every caller up to `run` — too
infectious for a cross-cutting concern. `#[ambient]` removes the infection while
keeping the effect's handlers, scoping and testability.

### Global mutable subscriber

Avoids the effect entirely, but scoped context then needs a hand-managed global
stack with no automatic restore on early exit.

### Deriving the runtime threshold from the installed sink

Making tier 2 exact rather than a floor would add a `min_level` operation, a
cache-invalidation contract on every `with Log => …` site, and a composition rule
for layers — all to save a tier-3 dispatch on events the sink would accept anyway.

### Per-callsite interest cache

`tracing` caches each callsite's `Interest`, invalidated by a generation counter.
Wado would need a `#callsite` literal and a compiler-managed side table: a new
language feature bought for per-target filtering, while tier 2 already gives
level filtering the same one-load cost.

### Level as a newtype over an integer

`type Level = i32` would let `#[param]` convert the threshold directly, but the
conversion would be the base type's, so the threshold could only be spelled
numerically (`-D log.level=2`), and the type would lose exhaustive matching while
admitting out-of-range values.

### Field passing: `Value` vs token buffer vs erased serde

The ladder is `Value` (tagged-union tree, allocating) < flat token buffer (one
allocation, replayable) < erased serde (zero boxing until bytes). The facade uses
`Value`; erased serde is adopted once profiling justifies it. A generic `Log`
operation (`event<F: Serialize>`) was rejected as the erasure route: the handler
vtable is installed at the `with` site, which cannot enumerate the `F` shapes used
across the block's dynamic extent.

## References

- [Ambient Logging Functions](./wep-2026-01-12-ambient-logging.md)
- [Compile-Time Location Literals](./wep-2026-01-23-compile-time-location-literals.md)
- [Default Arguments](./wep-2026-04-11-default-arguments.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Effect Handler](./wep-2026-04-11-effect-handler.md)
- [Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
- [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)
- [Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md)
- [Lenient String Parsing (`LenientFromStr`)](./wep-2026-06-22-lenient-from-str.md)
- [Optimizer Remarks for Missed Optimizations](./wep-2026-06-03-optimizer-remarks.md)
- [NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md)
- Rust [`tracing`](https://docs.rs/tracing), [`log`](https://docs.rs/log)
- Go [`log/slog`](https://pkg.go.dev/log/slog)
