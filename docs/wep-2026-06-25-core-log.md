# WEP: Structured Logging and Tracing Standard Library (`core:log`)

Status: Draft

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

```wado
pub enum Level { Trace, Debug, Info, Warn, Error }
```

`enum` values are ordered by declaration, so `level >= threshold` needs no cast.
A "threshold" is the lowest level emitted: at `Info`, `Trace` and `Debug` are off.

### Filtering — three tiers

Every level wrapper carries the same three gates, cheapest first, each strictly
narrowing what the one before admitted.

```wado
#[ambient]
pub fn debug<T: Serialize = NoFields>(
    message: String,
    fields: T = NoFields {},
    target: String = #function,
    file: String = #file,
    line: i32 = #line,
) {
    if Level::Debug < LOG_STATIC_LEVEL { return; }    // tier 1 — folds away
    if Level::Debug < runtime_level() { return; }     // tier 2 — one global read
    if !Log::enabled(Level::Debug) { return; }        // tier 3 — the subscriber
    event(Level::Debug, message, fields, target, file, line);
}
```

Tier 1 — compile time. A `#[param]` global carries the threshold, so the
comparison against the wrapper's literal level folds and the body reduces to an
early return. Interprocedural DCE then removes the call and its pure arguments,
message construction included.

```wado
#[param(name = "log.level", from_env = "WADO_LOG_LEVEL")]
global LOG_LEVEL: String = "trace";

global LOG_STATIC_LEVEL: Level = level_from_str(LOG_LEVEL);
```

The parameter is a `String` so the threshold is spelled by name
(`-D log.level=info`); `from_env` reads the build environment, not the runtime
one. The default admits every level, so a build says what it strips rather than
what it keeps. `#[param]` accepts only built-in types today, hence the conversion
function — see [Language and optimizer requirements](#language-and-optimizer-requirements).

Tier 2 — process-wide runtime threshold: a global read and a comparison, no call
and no allocation.

```wado
global mut RUNTIME_LEVEL: Level = Level::Trace;

pub fn set_log_level(level: Level) { RUNTIME_LEVEL = level; }
pub fn runtime_level() -> Level { return RUNTIME_LEVEL; }
```

The threshold belongs to `core:log`, not to the installed sink, and is set
explicitly — the bootstrap reads `WADO_LOG`, an application may call
`set_log_level` at any time — so no sink contract and no cache invalidation exist
to get wrong. A layer narrows further through tier 3.

Tier 3 — the subscriber. `Log::enabled` runs the installed layer stack, so a
`Filter` layer's per-target directives decide. It costs one indirect call, on
what the first two tiers admitted.

The same ladder is exposed for guarding expensive fields:

```wado
#[ambient]
pub fn enabled(level: Level) -> bool { ... }   // the three tiers, as a predicate

if enabled(Level::Debug) { debug(`state`, { snapshot: expensive_snapshot() }); }
```

No branch hint is emitted: whether a deployment logs is genuinely open, and a
static hint cannot beat the CPU's predictor on a branch this consistent.

### Types

```wado
pub type SpanId = u64;

pub struct Metadata {
    pub level: Level,
    pub target: String,   // category; default = caller #function
    pub name: String,     // span name; "" for events
    pub file: String,     // call-site #file
    pub line: i32,        // call-site #line
}

pub struct Event {
    pub meta: Metadata,
    pub message: String,
    pub fields: Value = Value::Null,       // Value from core:value
    pub parent: Option<SpanId> = null,
}

pub struct SpanAttrs { pub meta: Metadata, pub fields: Value }

pub struct NoFields {}                     // serializes to `{}`
```

Fields are passed as an anonymous struct bounded by `Serialize` and boxed into a
single `core:value::Value` at the facade, before the effect boundary, so the
`Log` effect stays non-generic:

```wado
info(`user logged in`, { user_id: id, ip: ip });
```

The field parameter defaults to an empty struct, so `info(msg)` logs with no
fields; the default type parameter supplies the type for the omitted argument.
Anonymous structs derive `Serialize` through
[bound-driven derivation](./wep-2026-06-25-trait-derivation.md), so no `derive`
form appears at the call site.

### The `Log` effect (subscriber)

An effect mirroring `tracing`'s `Subscriber`, simplified by GC (no span
refcounting). Operations are best-effort and never fail the program; sinks
swallow their own write and serialize errors.

```wado
pub interface Log {
    fn enabled(level: Level) -> bool;
    fn current_span() -> Option<SpanId>;
    fn new_span(attrs: SpanAttrs) -> SpanId;
    fn record_fields(span: SpanId, fields: Value);   // fields onto an open span
    fn follows_from(span: SpanId, cause: SpanId);    // non-parent causal link
    fn enter(span: SpanId);
    fn exit(span: SpanId);
    fn close(span: SpanId);
    fn event(event: Event);
}
```

`enabled` takes a `Level`, not a `&Metadata`: the gate must not force the caller
to build a `Metadata` for an event about to be dropped. A layer filtering on
`target` reads it from the `Event` in `event` instead.

### Events

Free functions; location defaults resolve at the caller. The level wrappers own
the filtering — each knows its level as a compile-time literal — and forward to
the raw `event` emitter. A direct caller of `event` (a dynamic `level`, say) opts
out of the static gate.

```wado
#[ambient]
pub fn event<T: Serialize = NoFields>(
    level: Level,
    message: String,
    fields: T = NoFields {},
    target: String = #function,
    file: String = #file,
    line: i32 = #line,
) {
    Log::event(Event {
        meta: Metadata { level, target, name: "", file, line },
        message,
        fields: box_fields(fields),
        parent: Log::current_span(),
    });
}
```

### Spans

A span is a first-class, re-enterable value entered for a scope:

```wado
pub struct Span { id: SpanId }

pub fn span(level: Level, name: String, fields: T = NoFields {}, ...) -> Span;
pub fn current() -> Option<Span>;

impl Span {
    pub fn id(&self) -> SpanId { ... }
    pub fn record<T: Serialize>(&self, fields: T) { Log::record_fields(self.id, box_fields(fields)); }
    pub fn follows_from(&self, cause: &Span) { Log::follows_from(self.id, cause.id); }
}
```

Entry uses the `in_span` closure, so `exit` and `close` run on every exit path
including an early `return` from the body:

```wado
#[ambient]
pub fn in_span<T, effect E>(s: &Span, body: fn() -> T with E) -> T with E {
    Log::enter(s.id());
    let r = body();
    Log::exit(s.id());
    Log::close(s.id());
    return r;
}
```

The subscriber tracks the current-span stack from `enter` / `exit`, so `current()`
and event parenting need no separate global. Entry is lexical, so there is no
"guard held across an await" footgun. `close` fires on GC unreachability or an
explicit `span.close()`; fmt sinks usually ignore it.

### Subscribers and layers

A sink or layer is `impl Log`; layers nest, each handling what it cares about and
delegating the rest outward with `..forward` ([Effect Handler](./wep-2026-04-11-effect-handler.md)). A sink is a layer that forwards
nothing, so a forwarded operation terminates there instead of trapping. A test
sink that must never see an operation uses `..trap`.

```wado
pub struct TextSink { pub timestamp: bool = true, pub seq: bool = true, pub location: bool = false }
pub struct JsonSink { pub timestamp: bool = true, pub seq: bool = true }
pub struct NopSink {}
pub struct CaptureSink { events: List<Event> = [], spans: List<SpanAttrs> = [] }

pub struct Context<T: Serialize> { fields: T }     // slog `With`: prepend fixed fields
pub struct Filter { directives: List<Directive> }  // EnvFilter-style, per target

impl<T: Serialize> Log for Context<T> {
    fn event(&self, event: Event) { Log::event(event.with_fields_under(&self.fields)); }
    ..forward
}

impl Log for Filter {
    fn enabled(&self, level: Level) -> bool { resume self.admits(level, "") && Log::enabled(level); }
    fn event(&self, event: Event) { if self.admits(event.meta.level, event.meta.target) { Log::event(event); } }
    ..forward
}
```

Compose by nesting `with`, innermost last:

```wado
with Log => &TextSink {} do {
    with Log => &Filter { directives: parse_directives(log_directives()) } do { app(); }
}
```

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

Both write through the ambient `log_stderr`, so a sink needs no `Stderr` in its
signature and installs in any world — a world without stderr degrades to a no-op
instead of a trap, matching the never-fail rule. Timestamps are the one part
needing a capability: `timestamp: true` reads `SystemClock`, `false` removes the
requirement.

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
The export shim the compiler already synthesises around an entry point is where
it goes: for a program whose module graph reaches `core:log`, the shim installs
the default sink and seeds `set_log_level` from `WADO_LOG` before calling the
entry function.

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
rather than captured output.

Exported: `Level`, `SpanId`, `Metadata`, `Event`, `SpanAttrs`, `Span`, `NoFields`;
the facade `trace` / `debug` / `info` / `warn` / `error` / `event` / `enabled`;
`span` / `current` / `in_span`; `set_log_level` / `log_level` / `level_from_str`;
the `Log` effect; sinks `TextSink` / `JsonSink` / `NopSink` / `CaptureSink`; layers
`Context` / `Filter` with `Directive` and `parse_directives`.

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
- `#[ambient]` keeps logging out of signatures; a default sink at the entry gives
  zero per-call setup. Caller source location without macros; testable with a
  capturing sink.
- The two cheap tiers keep the disabled path off the effect-dispatch path, so the
  common case is a fold or a global read.

### Trade-offs

- `#[ambient]` hides a sink's I/O from signatures (the existing ambient-logging
  trade-off), and an operation with no handler traps — so the entry point must
  install the default sink.
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
- [ ] Forwarding a local bound to a constant global read.
- [ ] Sinking pure definitions into the branch that uses them.
- [ ] `core:log` itself — see [Module surface](#module-surface).
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
- [The Live ValueGraph](./wep-2026-06-15-live-value-graph.md)
- Rust [`tracing`](https://docs.rs/tracing), [`log`](https://docs.rs/log)
- Go [`log/slog`](https://pkg.go.dev/log/slog)
