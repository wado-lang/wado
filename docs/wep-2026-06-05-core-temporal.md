# WEP: Temporal Standard Library (`core:temporal`)

## Context

Wado has no date/time types. The need surfaces from several directions at once:
serde formats that carry timestamps (CBOR tags 0/1, JSON RFC 3339 strings),
logging, HTTP date headers, and ordinary application code. The immediate driver
is [`core:cbor`](./wep-2026-06-05-core-cbor.md): its typed timestamp mapping
(CBOR tag 0/1) needs a concrete Wado type to deserialize into, so a minimal time
type must exist before the CBOR serde impls can be written.

### What WASI provides — and what it does not

`wasi:clocks@0.3.0` is deliberately minimal. It standardizes only:

- `system-clock.instant` — a record `{ seconds: s64, nanoseconds: u32 }`, the
  physical instant since the Unix epoch (1970-01-01T00:00:00Z). No calendar, no
  time zone.
- `monotonic-clock.mark` — `u64`, elapsed time for measurement, not wall time.
- `types.duration` — `u64` nanoseconds.
- `timezone` (unstable, `feature = clocks-timezone`) — only `iana-id() ->
  option<string>`, `utc-offset(instant) -> option<s64>`, and a debug string. No
  civil datetime, no calendar arithmetic.

Crucially, the WIT comment on `instant` names TC39 Temporal as the conceptual
reference for richer time representation rather than defining one:

> For more on various different ways to represent time, see
> <https://tc39.es/proposal-temporal/docs/timezone.html>

So WASI provides a physical instant plus a UTC-offset lookup, and leaves the
civil/calendar model to be designed on top. That design is `core:temporal`.

### Prior art

| System           | Exact-time type                      | Zoned/civil type                                  | Precision | Notes                                                               |
| ---------------- | ------------------------------------ | ------------------------------------------------- | --------- | ------------------------------------------------------------------- |
| TC39 Temporal    | `Instant` (epochNanoseconds, BigInt) | `ZonedDateTime` (instant + tz id + calendar id)   | ns        | `Calendar`/`TimeZone` objects were removed; they are now string ids |
| Rust `jiff`      | `Timestamp`                          | `Zoned` (timestamp + `TimeZone`)                  | ns        | Mirrors Temporal closely                                            |
| Rust `chrono`    | `DateTime<Utc>`                      | `DateTime<Tz>`                                    | ns        | Tz via generic parameter                                            |
| Go `time`        | `time.Time` (wall+monotonic+loc)     | same type carries `*Location`                     | ns        | One fused type                                                      |
| Java `java.time` | `Instant`                            | `ZonedDateTime` (instant + `ZoneId` + chronology) | ns        | The model Temporal is based on                                      |

The recurring split is **exact time** (anchored to the epoch, UTC) versus
**zoned/civil time** (an instant plus a time-zone interpretation). The "complete"
value everywhere is _instant + time zone_. `core:temporal` adopts that split.

### Scope: this WEP is an MVP

This WEP introduces **type definitions only** — `Instant` and `ZonedDateTime` —
to unblock `core:cbor`. Methods, arithmetic, parsing/formatting, `now()`, civil
field accessors, and `Duration` are explicitly deferred to follow-up work.

## Decision

### Module: `core:temporal`

Two structs, both ISO 8601 only.

```wado
#![stdlib("core:temporal")]

/// An exact point on the timeline, as the offset from the Unix epoch
/// (1970-01-01T00:00:00Z). Time-zone- and calendar-independent.
/// Corresponds to `Temporal.Instant` and `wasi:clocks` `instant`.
pub struct Instant {
    /// Whole seconds since the Unix epoch. Negative values are before it.
    pub seconds: i64,
    /// Sub-second component, always in `0..1_000_000_000`. Incrementing
    /// `nanoseconds` always moves forward in time, even when `seconds < 0`
    /// (e.g. one nanosecond before the epoch is `{ seconds: -1,
    /// nanoseconds: 999_999_999 }`).
    pub nanoseconds: u32,
}

/// An exact instant together with the time zone it is interpreted in.
/// Corresponds to `Temporal.ZonedDateTime` — the only "complete" temporal
/// value (an instant plus a wall-clock interpretation). The calendar is
/// always ISO 8601 and therefore not stored.
pub struct ZonedDateTime {
    /// The exact instant.
    pub instant: Instant,
    /// IANA time-zone identifier (e.g. `"America/New_York"`, `"UTC"`) or a
    /// fixed UTC offset (e.g. `"+09:00"`). Mirrors the Temporal time-zone
    /// slot, which is also a string after the removal of `Temporal.TimeZone`.
    pub time_zone: String,
}
```

### Design notes

#### No BigInt; `i64` seconds is more than enough

Temporal stores `epochNanoseconds` as a BigInt and limits the range to ±10^8
days (≈ ±273,790 years). Wado has no arbitrary-precision integer, but it does not
need one: `i64` seconds spans ≈ ±292 billion years, dwarfing Temporal's range,
and `u32` nanoseconds gives full nanosecond resolution. This is exactly the
shape of `wasi:clocks` `instant`, so host conversion is a field-for-field copy.

#### ISO 8601 only

Non-ISO calendars are out of scope and will be reconsidered only if a concrete
need arises. Because the calendar is fixed, `ZonedDateTime` stores no calendar
field — one fewer string per value and no calendar-resolution machinery.

#### Civil fields are derived, not stored

Following Temporal, the broken-down wall-clock fields (year, month, day, hour,
…) are a _function of_ `instant` + `time_zone`, not stored state. Accessors that
compute them are future work; storing only the instant keeps the MVP faithful
and avoids representing redundant, possibly-inconsistent state.

#### Auto-derived traits

Both structs auto-derive `Eq` and `Ord` (all fields are `Eq`/`Ord`) and
`Inspect`, so values compare and print in tests without extra code. `Ord` on
`Instant` orders chronologically. Note that `Ord` on `ZonedDateTime` orders by
`instant` then `time_zone` lexically — it is _not_ a meaningful chronological
order across zones beyond the instant; ordering distinct zones is the caller's
concern.

## Consequences

- Unblocks `core:cbor`'s typed timestamp mapping: tag 1 (epoch) ↔ `Instant`,
  tag 0 (RFC 3339) ↔ `ZonedDateTime`. Those serde impls live in `core:cbor`.
- The MVP has no methods, so the types are data-only until follow-ups land.
  This is intentional: it is the smallest thing that removes the `core:cbor`
  blocker.
- Avoiding BigInt keeps the types as plain Wasm-GC structs with no special
  numeric support.
- ISO-8601-only is a deliberate limitation, revisited on demand.

## TODO

- [x] Type definitions: `Instant`, `ZonedDateTime` (this WEP)
- [ ] Conversions to/from `wasi:clocks` `instant` (host bridge)
- [ ] `now()` via `wasi:clocks` system-clock
- [ ] RFC 3339 parse/format (`ZonedDateTime` ↔ string)
- [ ] Civil field accessors (year/month/day/hour/… from `instant` + `time_zone`)
- [ ] `Duration` type and instant arithmetic
- [ ] serde impls — owned by [`core:cbor`](./wep-2026-06-05-core-cbor.md)
      (tag 0/1) and `core:json` (RFC 3339 string)
