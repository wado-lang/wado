# WEP: Temporal Standard Library (`core:temporal`)

## Context

Wado needs date/time types. The need surfaces from several directions at once:
serde formats that carry timestamps (CBOR tags 0/1, JSON RFC 3339 strings),
logging, HTTP date headers, and ordinary application code. The original driver
was [`core:cbor`](./wep-2026-06-05-core-cbor.md): its typed timestamp mapping
(CBOR tag 0/1) needs a concrete Wado type to deserialize into.

TC39 Temporal is the model. It is the most recently designed of the major
date/time APIs, it is the one WASI's own `wasi:clocks` points at, and its split
between exact time and zoned time is the split every serious library converged
on.

### What WASI provides — and what it does not

`wasi:clocks@0.3.0` is deliberately minimal. It standardizes only:

- `system-clock.instant` — a record `{ seconds: s64, nanoseconds: u32 }`, the
  physical instant since the Unix epoch (1970-01-01T00:00:00Z). No calendar, no
  time zone.
- `monotonic-clock.mark` — `u64`, elapsed time for measurement, not wall time.
- `types.duration` — `u64` nanoseconds.
- `timezone` (unstable, `feature = clocks-timezone`) — only `iana-id() ->
  option<string>`, `utc-offset(instant) -> option<s64>`, and a debug string. No
  civil datetime, no calendar arithmetic, and no transition list.

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

`Instant` carries normalizing and epoch-based constructors (`new`,
`from_epoch_seconds`, `from_epoch_milliseconds`, `from_epoch_nanoseconds`,
`from_unix_seconds`), the matching epoch accessors, and `to_rfc3339`.
`ZonedDateTime` carries `parse_rfc3339`, `to_rfc3339`, and the civil accessors
(`year` … `nanosecond`, `day_of_week`, `day_of_year`, `days_in_month`,
`days_in_year`, `in_leap_year`, `months_in_year`).

### No BigInt; `i64` seconds is more than enough

Temporal stores `epochNanoseconds` as a BigInt and limits the range to ±10^8
days (≈ ±273,790 years). Wado has no arbitrary-precision integer, but it does not
need one: `i64` seconds spans ≈ ±292 billion years, dwarfing Temporal's range,
and `u32` nanoseconds gives full nanosecond resolution. This is exactly the
shape of `wasi:clocks` `instant`, so host conversion is a field-for-field copy.

### ISO 8601 only

Non-ISO calendars are out of scope and will be reconsidered only if a concrete
need arises. Because the calendar is fixed, `ZonedDateTime` stores no calendar
field — one fewer string per value and no calendar-resolution machinery.

### Civil fields are derived, not stored

Following Temporal, the broken-down wall-clock fields (year, month, day, hour,
…) are a _function of_ `instant` + `time_zone`, not stored state. Storing only
the instant avoids representing redundant, possibly-inconsistent state; the
accessors compute the local date with Howard Hinnant's `civil_from_days`.

### Fixed UTC offsets only

`time_zone` is typed as a string so it can hold an IANA identifier, but only
`"Z"`, `"UTC"`, and `±HH:MM` are interpretable today. Every operation that needs
an offset traps on an IANA name; see the gap below.

### Auto-derived traits

Both structs auto-derive `Eq` and `Ord` (all fields are `Eq`/`Ord`) and
`Inspect`, so values compare and print in tests without extra code. `Ord` on
`Instant` orders chronologically.

### serde: an RFC 3339 string under CBOR tag 0

Both types serialize as an RFC 3339 string under CBOR's date/time tag 0
(RFC 8949 §3.4.1); JSON ignores the tag and emits the bare string.
Deserialization accepts either that string or an epoch-seconds number (tag 1 /
JSON number), read as UTC. The impls are format-agnostic and live here rather
than in `core:cbor` or `core:json`.

### Bridging `wasi:clocks`

`wasi:clocks` has its own `Instant` record. That type is a Component Model
binding — pinned to a WASI version, regenerated by `wado-from-idl` — whereas
`core:temporal`'s is a plain, version-independent Wado type that grows methods.
They share a name and field layout deliberately: the same concept, a different
type. `From` impls both ways bridge them with a field-for-field copy. Naming the
record is not a runtime use of the clock, so a component that never calls one
gains no WASI import.

## Known gaps

Each gap names what is missing and what closing it takes. Ordering matters: the
tz database and `Duration` are what most of the rest builds on.

### The IANA time-zone database

`parse_fixed_offset` traps on anything but `"Z"`, `"UTC"`, and `±HH:MM`, so a
`ZonedDateTime` in `"Asia/Tokyo"` cannot be formatted or read at all. Closing it
takes an offset-and-transition source. Two candidates, neither settled:

- `wasi:clocks` `timezone` (unstable, `feature = clocks-timezone`). Gives
  `utc-offset(instant)` — enough for the accessors, but it exposes no transition
  list, so `start_of_day`, `hours_in_day`, `get_time_zone_transition`, and DST
  gap/overlap disambiguation stay out of reach. It also makes every named-zone
  program import a clock interface.
- A bundled tzdb, sliced like [`core:icu`](./wep-2026-08-09-core-icu.md) slices
  CLDR. Self-contained and complete, at a size cost that has not been measured.
  `core:icu` already carries an open item for exactly this seam, and a
  `core:temporal` that depends on `core:icu` would tie the smallest date type to
  the largest data dependency in the stdlib.

Whichever is chosen, DST forces a second decision: an instant is unambiguous but
a local wall-clock time is not, so any `PlainDateTime → ZonedDateTime` path needs
Temporal's `disambiguation` (`compatible`/`earlier`/`later`/`reject`) and
`offset` (`use`/`ignore`/`prefer`/`reject`) options.

### `now()`

There is no `now()`. The effect system already gives it a shape: `fn now() ->
Instant with (SystemClock)`, the way `core:benchmark` declares
`MonotonicClock` — the effect row is what keeps the clock off callers that never
ask for the time, so adding it costs non-users nothing. What is open is the
surface. Temporal groups these under `Temporal.Now` (`instant`,
`zonedDateTimeISO`, `plainDateISO`, `plainTimeISO`, `timeZoneId`); Wado has no
namespacing convention for such a group inside one stdlib module, and
`timeZoneId` needs the unstable `wasi:clocks` `timezone` interface on top of the
system clock.

### `Duration` and arithmetic

Nothing arithmetic exists: no `add`, `subtract`, `until`, `since`, on either
type. Closing it takes `Temporal.Duration` first — a signed 10-field record
(years, months, weeks, days, hours, minutes, seconds, milliseconds,
microseconds, nanoseconds), with a single sign shared by all fields, balancing
rules, `total()`, `abs`/`negated`/`sign`/`blank`, and ISO 8601 duration strings
(`P1Y2M3DT4H5M6.5S`) to parse and format.

Two properties of Temporal's design carry over and are the hard part:

- Calendar units (years, months, weeks) are not fixed-length, so `Duration`
  arithmetic on them needs a `relativeTo` anchor. On `Instant`, Temporal rejects
  them outright — only hours and below are exact.
- `until`/`since` between two zoned values must walk the calendar, not divide
  seconds, and must respect DST — so this gap is blocked on the tz database for
  named zones.

The relation to `wasi:clocks` `Duration` (a `u64` nanosecond count) also needs
settling: a `From` bridge like the `Instant` one, or a deliberate separation
because the WASI type is unsigned and exact-only.

### Rounding and unit options

`round` is absent on both types, as is the `smallestUnit` / `roundingIncrement` /
`roundingMode` triple that also governs `until`/`since`/`total` and `toString`
precision. Closing it takes a `Unit` enum, a `RoundingMode` enum (Temporal has
nine modes), and a way to pass an options bag. Wado has
[default arguments](./wep-2026-04-11-default-arguments.md) and
[literal spread](./wep-2026-07-03-literal-spread.md) but no keyword arguments,
so the ergonomics of `zdt.round(smallestUnit: Unit::Hour)` are unresolved.
`toString` precision partly overlaps machinery that already exists: the
[format specifier](./wep-2026-01-17-template-format-specifiers.md) path already
selects fraction digits via `${instant:3}`.

### The plain (zoneless) types

`PlainDate`, `PlainTime`, `PlainDateTime`, `PlainYearMonth`, and `PlainMonthDay`
do not exist. They are where most of Temporal's ergonomics live — a birthday, a
business date, a recurring wall-clock time are all zoneless — and `ZonedDateTime`
cannot express any of them. Each needs its own parse/format, `with`, `add`/
`subtract`, `until`/`since`, comparison, and conversions among the family;
`toZonedDateTime` additionally needs the disambiguation options above.

### Missing surface on the two existing types

`Instant` lacks `parse_rfc3339` (only `ZonedDateTime` parses), `FromStr` and
[`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md), `epoch_microseconds` /
`from_epoch_microseconds`, and `to_zoned_date_time_iso(time_zone)`. Its
`epoch_nanoseconds` traps outside ≈ ±292 years of the epoch because it returns
`i64`; `i128` would remove the limit at the cost of a wider return type.

`ZonedDateTime` lacks `to_instant`, `epoch_milliseconds` / `epoch_nanoseconds`,
`offset` / `offset_nanoseconds`, `with` / `with_time_zone` / `with_plain_time`,
`start_of_day`, `hours_in_day`, `get_time_zone_transition`, and the ISO week
accessors `week_of_year` / `year_of_week` / `days_in_week`. `month_code` (`"M01"`
… `"M12"`, plus `"M05L"` in lunisolar calendars) is trivial under an ISO-only
calendar but is part of the Temporal field vocabulary; `era` / `era_year` are
undefined for ISO 8601 and have nothing to close.

### RFC 9557 zone annotations

Temporal's canonical `ZonedDateTime.toString()` is
`2023-11-14T22:13:20+09:00[Asia/Tokyo]` — RFC 9557 (IXDTF), an offset plus a
bracketed zone identifier. `core:temporal` neither emits nor parses the bracket,
and `parse_rfc3339` collapses the zone to the offset string it saw, so zone
identity does not survive a round trip even once the tz database lands. Closing
it takes the bracket in both directions, plus Temporal's rule for reconciling a
stored offset that disagrees with what the named zone says at that instant.

### Ordering and equality diverge from Temporal's

`Temporal.ZonedDateTime.compare` compares `epochNanoseconds` only, and `equals`
requires the time zone and calendar to match as well. Wado derives both from the
field order, giving one relation for two questions: `Ord` adds a lexical
`time_zone` tiebreak (so a sort is not chronological across zones), and `Eq`
makes `"Z"`, `"UTC"`, and `"+00:00"` three different zones for the same instant.
Closing it takes a choice between normalizing `time_zone` at construction and
replacing the derivations with hand-written impls that split "same moment" from
"same value".

### Parser divergences

`parse_rfc3339` differs from the ISO 8601 / RFC 3339 grammar in ways the
docstring does not admit:

- The year is documented as "four or more digits" but the code accepts one or
  more, so `"1-01-01T00:00:00Z"` parses. ISO 8601 wants exactly four digits, or
  six with a mandatory sign for expanded years.
- A leap second (`:60`) is rejected. RFC 3339 permits it and Temporal accepts it,
  constraining the value to 59.
- A fraction longer than nine digits is silently truncated; Temporal's grammar
  caps it at nine and rejects more.
- `-00:00`, which RFC 3339 defines as "offset unknown", is treated as `+00:00`.

Each is a small fix, but each changes what round-trips, so they belong with the
RFC 9557 work rather than as drive-by edits.

### Formatting beyond ISO 8601

The Context names HTTP date headers as a driver, but there is no RFC 7231
IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) formatter or parser, so
`wasi:http` `Date` / `Expires` / `Last-Modified` headers have no typed path.
Locale-aware formatting (`toLocaleString`) belongs to
[`core:icu`](./wep-2026-08-09-core-icu.md) and is tracked by that WEP's open item
on where its date/time formatting meets this module.

### Test coverage follows the gaps

`temporal_test.wado` covers what exists — construction, ordering, `Display`
precision, the accessors, parse round-trips, serde. There is no property-style
round-trip over a wide instant range, no fixture pinning the CBOR tag-0 byte
encoding, and no test at the extremes of the `i64` second range where
`render_iso8601` produces years far outside four digits.

## Consequences

- The serde timestamp mapping is carried here, so `core:cbor` and `core:json`
  need no date/time knowledge of their own, and `core:log` gets an ISO 8601
  timestamp from a `wasi:clocks` reading through one `From`.
- Avoiding BigInt keeps both types as plain Wasm-GC structs with no special
  numeric support, at the cost of an `epoch_nanoseconds` that traps outside
  ≈ ±292 years.
- ISO-8601-only is a deliberate limitation, revisited on demand.
- Until the tz database gap closes, `time_zone` is a string that promises more
  than the module delivers: an IANA name is storable, serializable, and traps on
  first use. That is the sharpest edge in the module today.
