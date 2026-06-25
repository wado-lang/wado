# Structured Logging and Tracing Standard Library (`core:log`)

Status: Draft

## Context

Wado has two output facilities today: best-effort ambient functions
(`log_stdout` / `log_stderr`, see
[Ambient Logging Functions](./wep-2026-01-12-ambient-logging.md)) and strict
effectful I/O (`println` / `eprintln`). Neither is a logging facility: there are
no levels, no structured fields, no spans, no pluggable destination, no source
location, no filtering. Logging is a core component of modern software — which is
exactly why independent ecosystems have converged on the same shape — so this WEP
designs the ideal, full-set logging and tracing library for Wado, with no
compromise on the interface.

### What modern logging/tracing converged on

Rust `tracing` and Go `slog` agree on the substance:

- Structured key–value fields, never values concatenated into the message.
- A facade (the call site) decoupled from a backend (`tracing` `Subscriber` /
  `slog` `Handler`) that decides formatting and destination.
- Spans: named, timed units of work forming a tree; events attach to the current
  span and inherit its context (`tracing`). `slog` approximates this with child
  loggers (`Logger.With`).
- Layered backends: filter + format + telemetry composed independently
  (`tracing`'s `Layer` stack).
- Two-axis filtering: compile-time (strip disabled call sites — `tracing`/`log`
  `max_level_*` features) and runtime (per-target/level directives —
  `RUST_LOG` / `EnvFilter`, `slog` min level).
- Zero work when disabled; lazy escape hatches for the expensive case.
- JSON for production, pretty text for development; timestamps at emit time.

Both leans on capabilities Wado lacks by design: Rust uses macros; Go uses
runtime dynamic dispatch and variadic `...any`. Wado reproduces every behavior
with its own tools.

### Mapping the model onto Wado

| Concern                   | `tracing` / `slog`             | Wado mechanism                                                         |
| ------------------------- | ------------------------------ | ---------------------------------------------------------------------- |
| Backend abstraction       | `Subscriber` / `Handler` (dyn) | the `Log` **ambient effect**; sinks are effect handlers (static)       |
| Layer composition         | `Layer` stack                  | **nested effect handlers** (`with Log => &layer do`), forward to outer |
| Spans / scoped context    | spans, `Logger.With`           | first-class `Span` values entered with `with span do`                  |
| Compile-time level filter | `max_level_*` features         | **`#[param]` compile-time global** + constant-fold + DCE               |
| Runtime filter            | `EnvFilter` / min level        | a filter layer reading directives, via `enabled(meta)`                 |
| Source location           | `file!()` / `#[track_caller]`  | default args + call-site `#file` / `#line` / `#function`               |
| Structured fields         | macros / `...any`              | `List<Field>` built with `field<T: Serialize>(…)`                      |
| Field/event encoding      | `slog` serializer / `Visit`    | **`core:serde`**                                                       |
| Timestamp                 | subscriber adds it             | sink config (default on), via `wasi:clocks` `SystemClock`              |

The thesis: **a subscriber is an effect, layers and spans are nested handlers,
the level threshold is a compile-time parameter, and the default sink is the
existing ambient `log_stderr`.** Everything else is library code over features
Wado already has, plus the ambient-effects language extension; span scoping
already works through a closure, with `with span do` as optional sugar.

## Decision

### Two layers: ambient default + rich subscriber

`core:log` is the rich, dedicated path. When no subscriber is installed, the
ambient `Log` effect falls back to a best-effort default that reuses the existing
ambient output:

```wado
// Outermost fallback for the ambient Log effect. No effect, no allocation
// beyond rendering. Logs go to stderr (stdout is left for program output).
fn default_handler_event(event: &Event) {
    core:cli::log_stderr(render_plain(event));   // existing ambient function
}
// span lifecycle ops default to no-ops.
```

This makes `core:log` usable anywhere with zero setup — `info(...)` just works —
while a real subscriber is opt-in.

### Levels

```wado
// Ordered by `level as i32`; declaration order is the ordering.
pub enum Level { Trace, Debug, Info, Warn, Error }
```

### Metadata, Event, Span

```wado
pub struct Metadata {
    pub level: Level,
    pub target: String,   // category / module path; default = caller #function
    pub name: String,     // span name; "" for events
    pub file: String,     // call-site #file
    pub line: i32,        // call-site #line
}

pub struct Field { pub key: String, pub value: Value }   // Value from core:value
pub fn field<T: Serialize>(key: String, value: T) -> Field {
    let v = match value::to_value(&value) { Ok(v) => v, Err(_) => Value::Null };
    return Field { key, value: v };
}

pub struct Event {
    pub meta: Metadata,
    pub message: String,
    pub fields: List<Field> = [],
    pub parent: Option<SpanId> = null,   // null = current span
}

pub type SpanId = u64;
pub struct SpanAttrs { pub meta: Metadata, pub fields: List<Field> }
```

`Field`/`Value` and `to_value` are the baseline field representation; the
Efficient field passing section below replaces them with a zero-boxing path once
profiling justifies the `dyn` extension. `core:value::to_value` is implemented
(a direct `Value`-building serializer that preserves the data model).

Each library type that flows through serde (`Level`, `Metadata`, `Field`,
`Event`) carries an empty compiler-synthesized derive — `impl Serialize for T;`
(Wado's derive form; there is no `#[derive(...)]`). With bound-driven derivation
(see [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)) these
become implicit and the explicit lines disappear.

### The `Log` effect (Subscriber-as-effect)

`Log` is an ambient effect (see Language Extensions). Its operations are the
full `tracing` `Subscriber` surface, simplified by GC (no manual span refcounting):

```wado
#[ambient(default = default_handler)]
pub interface Log {
    fn enabled(meta: &Metadata) -> bool;            // filter check
    fn current_span() -> Option<SpanId>;            // capture current
    fn new_span(attrs: &SpanAttrs) -> SpanId;       // register a span
    fn record_fields(span: SpanId, fields: List<Field>);  // add fields to an open span
    fn follows_from(span: SpanId, cause: SpanId);   // non-parent causal link
    fn enter(span: SpanId);                         // push current
    fn exit(span: SpanId);                          // pop current
    fn close(span: SpanId);                         // span finished
    fn event(event: &Event);                        // emit an event
}
```

All operations return `()` (or a value with no error channel): logging is
best-effort and must never fail the application. Errors inside a sink (write
failure, serialize failure) are swallowed by the sink.

### Events

The facade is plain free functions. Because `Log` is ambient they need no
`with Log` annotation and are callable anywhere. Location defaults are
call-site-resolved (`#file`/`#line`/`#function` already report the caller):

```wado
#[inline(always)]
pub fn event(level: Level, message: String, fields: List<Field> = [],
             target: String = #function, file: String = #file, line: i32 = #line) {
    // Compile-time gate: LOG_MAX_LEVEL is a constant; a statically disabled
    // level folds to `if true { return }` and DCE removes the rest, including
    // the pure argument expressions at the call site.
    if (level as i32) < LOG_MAX_LEVEL { return; }
    let meta = Metadata { level, target, name: "", file, line };
    if !Log::enabled(&meta) { return; }            // runtime filter
    Log::event(&Event { meta, message, fields, parent: null });
}

#[inline(always)]
pub fn info(message: String, fields: List<Field> = [],
            target: String = #function, file: String = #file, line: i32 = #line) {
    event(Level::Info, message, fields, target, file, line);
}
// trace / debug / warn / error are identical with their level.

// Guard for genuinely expensive or effectful field computation.
pub fn enabled(level: Level, target: String = #function) -> bool {
    if (level as i32) < LOG_MAX_LEVEL { return false; }
    return Log::enabled(&Metadata { level, target, name: "", file: #file, line: #line });
}
```

The message is an eager `String`: it is built at the call site before `event`
runs. This is deliberate — the optimizer eliminates it when the level is
statically disabled, and `enabled()` covers the runtime-disabled hot path.

```wado
use { info, warn, error, field, enabled } from "core:log";

info(`user logged in`, [field("user_id", id), field("ip", ip)]);
warn(`retry`, [field("attempt", n)]);

if enabled(Level::Debug) {
    debug(`state`, [field("snapshot", expensive_snapshot())]);
}
```

### Spans

A span is a first-class value, created once and entered for a lexical scope:

```wado
pub struct Span { id: SpanId }

pub fn span(level: Level, name: String, fields: List<Field> = [],
            target: String = #function, file: String = #file, line: i32 = #line) -> Span {
    let meta = Metadata { level, target, name, file, line };
    return Span { id: Log::new_span(&SpanAttrs { meta, fields }) };
}

impl Span {
    pub fn id(&self) -> SpanId { return self.id; }
    pub fn record(&self, fields: List<Field>) { Log::record_fields(self.id, fields); }
    pub fn follows_from(&self, cause: &Span) { Log::follows_from(self.id, cause.id); }
}

pub fn current() -> Option<Span> {
    return match Log::current_span() { Some(id) => Option::Some(Span { id }), None => null };
}
```

A span is entered for a scope. v1 uses the closure form `in_span` (see Span
scoping under Language Extensions — it needs no language change); the illustrative
native sugar reads:

```wado
let s = span(Level::Info, "request", [field("route", route)]);
with s do {
    info(`received`);     // parent = s, inherits its context
    handle();             // events anywhere in the call graph parent to s
}                         // s exits here, on every exit path
```

Either form emits `Log::enter(s.id)` on entry and `Log::exit(s.id)` on every exit
path. The installed subscriber maintains the current-span stack from `enter`/`exit`,
so `current()` and event parenting need no separate global.

Entering is lexical/scoped — there is no free-floating RAII guard. This avoids the
classic "guard held across an await" footgun by construction.

`close` fires when the `Span` value is no longer reachable (GC) or via an
explicit `span.close()` for sinks that aggregate per-span. Common fmt sinks rely
only on `enter`/`exit`/`event` and ignore `close`.

### Subscribers and layers

A sink/layer is a struct that `impl Log`. Layers compose by nesting: each handles
what it cares about and forwards the rest to the outer handler (the effect
forwarding semantics of [Effect Handler](./wep-2026-04-11-effect-handler.md)).

```wado
// Pretty text to stderr. Default for CLI development.
pub struct TextSink { pub timestamp: bool = true, pub seq: bool = true, pub location: bool = false }
impl Log for TextSink {
    fn enabled(&self, meta: &Metadata) -> bool { resume true }
    fn event(&self, event: &Event) with Stderr, SystemClock {
        core:cli::eprintln(render_text(event, self));
        resume ()
    }
    ..  // span ops: render enter/exit if desired, else no-op
}

// One JSON object per line (JSONL). Default for the HTTP service world.
pub struct JsonSink { pub timestamp: bool = true, pub seq: bool = true }
impl Log for JsonSink {
    fn enabled(&self, meta: &Metadata) -> bool { resume true }
    fn event(&self, event: &Event) with Stdout, SystemClock { /* json::to_string */ resume () }
    ..
}

pub struct NopSink;                              // drops everything; enabled() == false
pub struct CaptureSink { events: List<Event> }   // test sink; mutated via &mut self

// Field-context layer (slog `With`): adds fixed fields to every event, forwards.
pub struct Context { fields: List<Field> }
impl Log for Context {
    fn event(&self, event: &Event) {
        let mut merged = self.fields;            // value semantics: copy
        merged.extend(event.fields);             // phase 1: list concat (no serde flatten)
        Log::event(&Event { meta: event.meta, message: event.message,
                            fields: merged, parent: event.parent });
        resume ()
    }
    ..  // everything else forwards to outer
}

// Runtime filter layer (EnvFilter-style), see Filtering.
pub struct Filter { directives: List<Directive> }
impl Log for Filter {
    fn enabled(&self, meta: &Metadata) -> bool { resume self.allows(meta) && Log::enabled(meta) }
    fn event(&self, event: &Event) { if self.allows(&event.meta) { Log::event(event); } resume () }
    ..
}
```

Installation composes layers with `with`:

```wado
export fn run() with Stdout, Stderr, SystemClock {
    with Log => &Filter { directives: parse_env() },
         Log => &TextSink { location: true } do {
        app();
    }
}
```

### Timestamp

The timestamp is owned by the sink, not the `Event`, and is configurable
(`timestamp: bool`, default on). Rationale: container/collector timestamps record
ingestion time, not event time, and drift under buffering; not every target has a
collector (CLI to a file, browser via jco, test world). When the deployment's
container stamps lines, set `timestamp: false` — and the sink then needs no
`SystemClock` effect, lightening the install-site requirements. An optional
monotonic `seq` counter (default on) preserves intra-process ordering without a
clock.

### Filtering (two axes)

Compile-time maximum level — a constant, folded away:

```wado
#[param(name = "log.level", from_env = "WADO_LOG")]
global LOG_LEVEL: String = "info";
global LOG_MAX_LEVEL: i32 = level_from_str(&LOG_LEVEL);   // pure → constant-folded
```

Anything below `LOG_MAX_LEVEL` is stripped everywhere (see Zero-cost). Runtime
refinement is a `Filter` layer parsing `EnvFilter`-style directives —
`target=level`, `level` (global), `mod::path=debug`, and span/field predicates —
consulted via `enabled(meta)`. The two axes mirror `tracing`'s `max_level_*`
(compile) + `EnvFilter` (runtime).

### Error handling and reentrancy

Operations return `()`; sinks swallow their own errors. Logging never aborts the
program.

Reentrancy (a sink, or a `Serialize` impl it calls, logs again) does not loop:
handler bodies execute in the outer effect scope, so a re-entrant log forwards
outward one level at a time and terminates at the non-logging default handler.
No panic is used — a panic would contradict the never-fail policy and punish
legitimate composition (a filter layer that logs, a type whose serialization
logs). As a backstop against a pathological self-reinstalling handler, a depth
limit drops (does not panic) further re-entrant records.

### Zero-cost when disabled

`LOG_MAX_LEVEL` is a compile-time constant, so the level gate folds. With the
facade `#[inline(always)]`, a statically disabled call inlines to
`if true { return }`; the body and the pure argument expressions at the call site
(message template, `field(...)` list) become dead and are removed by DCE over the
pure-value IR ([The Live ValueGraph](./wep-2026-06-15-live-value-graph.md)). An
argument with a side effect is not eliminable — correctly — so the hot effectful
case uses `enabled()`.

### Async semantics

The current-span stack is tracked through the effect dispatch state, which is
process-global today. Within a single synchronous scope this is exact. Across
concurrent tasks (the HTTP service world runs requests as tasks) a single global
current-span is wrong, so:

- v1 automatic current-span propagation is defined for the single synchronous
  scope only.
- Crossing a task boundary is explicit: carry the `Span` value and re-enter it
  with `with span do` in the other task (the manual equivalent of `tracing`'s
  `Instrument`). Spans being first-class re-enterable values makes this
  expressible with the v1 interface.
- Automatic cross-task propagation (per-task dispatch state) is a future
  addition layered on top once WASI threads / Wasm stack switching stabilize. It
  adds no API surface — the interface above is unchanged when it lands.

### Validated by a PoC

`example/logger_poc.wado` exercises this design on the current compiler (a plain
`Log` effect stands in for the ambient one, which is not yet implemented). Its
`wado test` blocks confirm: caller-resolved `#file`/`#line`/`#function` through
default args; the span lifecycle (`new`/`enter`/`exit`/`close`) with a
current-span stack and event parenting, including nested spans and `exit`/`close`
running on an early return inside `in_span`; effect-handler nesting and forwarding
through a `Context` field layer and a standalone `Filter` layer; the `enabled`
gate; `record_fields` / `follows_from` / `current()`; generic `field<T: Serialize>`
over the real `core:value::to_value`; serde encoding of the whole `Event` through
a JSON sink; and effect-polymorphic `in_span`. The PoC also pinned down the real
syntax used above: struct fields take no `mut` qualifier (mutate via `&mut self`),
and serde derivation is `impl Serialize for T;`.

## Language Extensions

### Ambient effects (required)

If `Log` were an ordinary effect, every function that logs — and transitively
every caller up to `run` — would need `with Log`. That cross-cutting infection is
why [Ambient Logging](./wep-2026-01-12-ambient-logging.md) made `log`/`panic`
non-effectful. Ambient effects keep that ergonomics while retaining handler
override (layers, spans, testing):

```wado
#[ambient(default = default_handler)]
pub interface Log { /* … */ }
```

An ambient effect:

- is not added to a function's required-effect set — performing a `Log`
  operation imposes no `with Log` on the caller, so the facade is callable
  anywhere;
- is still installable/overridable with `with Log => h do`, so layers, spans,
  and test sinks work exactly as for normal effects;
- has a default handler: when the dispatch global is null (no subscriber), the
  operation calls the named `default` (here `log_stderr`-backed) instead of
  trapping or hitting a CM adapter. The default uses the ambient I/O bypass, so
  it needs no effects of its own.

This is a thin addition to the existing dispatch
([Effect Handler](./wep-2026-04-11-effect-handler.md) § Dispatch Mechanism): the
per-operation dispatch function already branches "global is null → default
path"; for world-imported effects that path is the CM adapter, for an ambient
effect it is the declared default handler. Effect-checking skips ambient effects
when accumulating required sets. Installed handlers' own effects (e.g.
`TextSink`'s `Stderr`/`SystemClock`) are still checked at the `with` install
site. Ambient effects are a general capability (debug/trace/metrics sinks,
feature-flag oracles), not a logging special case.

### Span scoping — no language change required (optional sugar)

The PoC confirmed the bundled handler form (`with &h do { … }`) and value form
already work end-to-end, with scoped install and early-exit restore. Layers,
contextual fields, filters, and sinks therefore need no new syntax — they are
ordinary `impl Log` values installed with `with`.

Span entry needs enter/exit emitted at scope boundaries, and the dispatch
desugar has no install/uninstall hook. This is covered today with a closure:

```wado
pub fn in_span<T, effect E>(s: &Span, body: fn() -> T with E) -> T with E, Log {
    Log::enter(s.id());
    let r = body();        // a closure cannot skip past in_span's exit
    Log::exit(s.id());     // runs even on `?`/`return` inside the closure
    return r;
}

let s = span(Level::Info, "request", [field("route", route)]);
let resp = in_span(&s, || { handle(req) })?;
```

`in_span` is validated in the PoC. A native `with span do { … }` block (desugaring
to `Log::enter(id); B; Log::exit(id)` with `exit` injected on every exit path, via
the effect-handler restore injector) is an optional later ergonomic upgrade — it
lets control flow escape directly to the enclosing function rather than through
the closure boundary. It is not required for v1.

### Deferred (performance-gated): efficient field passing

The baseline `List<Field>` boxes each value into a `core:value::Value`. For fixed
call-site fields this is avoidable by passing an anonymous struct bounded by
`Serialize`:

```wado
info(`user logged in`, { user_id: id, ip: ip });   // anonymous struct, no Value boxing
```

This needs further extensions and would retire `Field`/`Value` from the
logger:

- Anonymous (structural) struct types that auto-derive `Serialize`/`Inspect`.
  An anonymous struct has no name to write `impl Serialize for …`, so this path
  also depends on bound-driven derivation
  ([Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)).
- Erased serde: a `dyn Serialize` payload (reference + monomorphized serialize
  funcref) carried across the non-generic `Log` operation, bridged through a
  `dyn Serializer` (the `Serialize::serialize<S>` generic method instantiated at
  `S = dyn Serializer` — the `erased_serde` pattern). Context merging then uses a
  serde flatten combinator (splice one object's entries into the open map),
  which `core:serde` does not yet support.

Decision: ship the interface first with `List<Field>`, measure, and adopt erased
serde only if profiling shows the per-call boxing matters. Because the field
argument is the only part that changes, this is an internal swap, not an
interface break. (Call-site resolution of `#file`/`#line`/`#function` is already
provided and is treated as a given.)

## Consequences

### Benefits

- A full-set logger and tracer — events, spans, layered subscribers, two-axis
  filtering — built from existing features plus the ambient-effects extension.
- Layer composition, scoped context, and automatic context restore come from
  effect-handler nesting, with static dispatch and no dynamic-dispatch
  requirement.
- Zero setup: `info(...)` works anywhere via the ambient default (`log_stderr`);
  a real subscriber is opt-in.
- Compile-time stripping + runtime filtering; caller source location with no
  macros; testable via a capturing sink in a `with` block.

### Trade-offs

- Ambient effects punch a deliberate hole in effect tracking (a sink may do I/O
  without it appearing in signatures) — the trade-off the ambient-logging WEP
  already accepted, now generalized.
- The message is eager; the optimizer (not a macro) removes it when statically
  disabled. Runtime-disabled hot paths rely on `enabled()`.
- Baseline fields box through `core:value`; removed later by the deferred
  efficient-field-passing path.
- Automatic span propagation is single-scope in v1; cross-task is explicit until
  the async runtime story stabilizes.

### Prerequisites

- [ ] Language: ambient effects with a default handler.
- [x] `core:value`: `pub fn to_value<T: Serialize>(value: &T) -> Result<Value, SerializeError>`
      (serde `to_value` analog) — implemented (direct Value-building serializer).
- [x] Span scoping via `in_span` closure — works today (no language change).
- [ ] Optional: native `with <span> do { … }` scoping sugar (enter/exit desugar
      via the restore injector).
- [ ] `core:log`: `Level`, `Metadata`, `Field`, `Event`, `Span`, `SpanAttrs`,
      `field`, the `Log` effect, the event facade
      (`trace`/`debug`/`info`/`warn`/`error`/`event`/`enabled`), `span` /
      `current`, sinks (`TextSink`, `JsonSink`, `NopSink`, `CaptureSink`),
      layers (`Context`, `Filter`), the `#[param]` level globals, and
      `default_handler` over `log_stderr`.
- [ ] Optional (ergonomics): bound-driven serde derivation
      ([Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)) — drops the
      `impl Serialize for T;` lines; a prerequisite for the anonymous-struct path.
- [ ] Deferred (perf-gated): anonymous structs, erased serde (`dyn Serialize` /
      `dyn Serializer`), serde flatten — retiring `Field`/`Value`.
- [ ] Deferred (async): automatic cross-task current-span propagation.

## Alternatives Considered

### Ordinary (non-ambient) effect

Make `Log` a normal effect requiring `with Log` everywhere. Principled and
needs no new feature, but infectious across the whole call graph — rejected for
the reasons the ambient-logging WEP rejected effectful `panic`.

### Global mutable subscriber

Store the subscriber in a `global mut` and have non-effect free functions call
it. Works without ambient effects, but scoped context (spans, layers) needs a
hand-managed global stack with no automatic restore on early exit — error-prone.
Ambient effects get scoped, auto-restored context from the `with` block.

### Panic on reentrancy

Fail-fast on re-entrant logging. Rejected: contradicts the never-fail policy and
penalizes legitimate composition; the forwarding semantics already terminate
re-entrancy at the non-logging default. A depth-limited drop is the backstop.

### Field passing: `Value` tree vs token buffer vs erased serde

The performance ladder is `Value` (tagged-union tree, allocating) < flat token
buffer (one allocation, replayable) < erased serde (zero boxing, zero copy until
bytes). The baseline uses `Value` for simplicity; the deferred path jumps to
erased serde once profiling justifies the language extension. A generic `Log`
operation (`event<F: Serialize>`) was rejected as the erasure route because the
handler vtable is installed at the `with` site, which cannot enumerate the `F`
shapes used across the block's dynamic extent.

## References

- [Ambient Logging Functions](./wep-2026-01-12-ambient-logging.md)
- [Compile-Time Location Literals](./wep-2026-01-23-compile-time-location-literals.md)
- [Default Arguments](./wep-2026-04-11-default-arguments.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Effect Handler](./wep-2026-04-11-effect-handler.md)
- [Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
- [Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md)
- [The Live ValueGraph](./wep-2026-06-15-live-value-graph.md)
- Rust [`tracing`](https://docs.rs/tracing), [`log`](https://docs.rs/log)
- Go [`log/slog`](https://pkg.go.dev/log/slog)
  </content>
