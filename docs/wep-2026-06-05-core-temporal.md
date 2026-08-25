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

Eight types, all ISO 8601. Two carry an instant, five are zoneless readings, one
is a span.

| Type             | What it is                       | Temporal counterpart      |
| ---------------- | -------------------------------- | ------------------------- |
| `Instant`        | exact point on the timeline      | `Temporal.Instant`        |
| `ZonedDateTime`  | instant + the zone it is read in | `Temporal.ZonedDateTime`  |
| `PlainDate`      | calendar date, no time, no zone  | `Temporal.PlainDate`      |
| `PlainTime`      | wall clock, no date, no zone     | `Temporal.PlainTime`      |
| `PlainDateTime`  | both, still no zone              | `Temporal.PlainDateTime`  |
| `PlainYearMonth` | a month of a year, no day        | `Temporal.PlainYearMonth` |
| `PlainMonthDay`  | a day of a month, no year        | `Temporal.PlainMonthDay`  |
| `Duration`       | a signed span                    | `Temporal.Duration`       |

`Unit` and `RoundingMode` are the two enums the difference and rounding
operations are parameterized by.

```wado
#![stdlib("core:temporal")]

/// An exact point on the timeline, as the offset from the Unix epoch
/// (1970-01-01T00:00:00Z). Time-zone- and calendar-independent.
pub struct Instant {
    /// Whole seconds since the Unix epoch. Negative values are before it.
    pub seconds: i64,
    /// Sub-second component, always in `0..1_000_000_000`. Incrementing
    /// `nanoseconds` always moves forward in time, even when `seconds < 0`.
    pub nanoseconds: u32,
}

/// An exact instant together with the time zone it is interpreted in — the
/// only "complete" temporal value. The calendar is always ISO 8601 and
/// therefore not stored.
pub struct ZonedDateTime {
    pub instant: Instant,
    /// IANA time-zone identifier (e.g. `"America/New_York"`) or a fixed UTC
    /// offset (e.g. `"+09:00"`). Mirrors the Temporal time-zone slot, which is
    /// also a string after the removal of `Temporal.TimeZone`.
    pub time_zone: String,
}
```

### No BigInt; `i64` seconds is more than enough

Temporal stores `epochNanoseconds` as a BigInt and limits the range to ±10^8
days (≈ ±273,790 years). Wado has no arbitrary-precision integer, but it does not
need one: `i64` seconds spans ≈ ±292 billion years, dwarfing Temporal's range,
and `u32` nanoseconds gives full nanosecond resolution. This is exactly the
shape of `wasi:clocks` `instant`, so host conversion is a field-for-field copy.
`epoch_nanoseconds` returns `i128`, which covers that whole span without
truncating.

### ISO 8601 only

Non-ISO calendars are out of scope and will be reconsidered only if a concrete
need arises. Because the calendar is fixed, no type stores a calendar field —
one fewer string per value and no calendar-resolution machinery. `era` and
`era_year` are undefined for ISO 8601 and are not offered; `month_code` is,
since it is part of the Temporal field vocabulary.

### Civil fields are derived, not stored

Following Temporal, a `ZonedDateTime`'s broken-down wall-clock fields are a
_function of_ `instant` + `time_zone`, not stored state. Storing only the instant
avoids representing redundant, possibly-inconsistent state; the accessors compute
the local date with Howard Hinnant's `civil_from_days`.

### Fixed UTC offsets only

`time_zone` is typed as a string so it can hold an IANA identifier, but only
`"Z"`, `"UTC"`, and `±HH:MM` are interpretable today. Every operation that needs
an offset traps on an IANA name; see the gap below.

`ZonedDateTime::new` canonicalizes the three spellings of UTC to `"Z"`, so
`"UTC"`, `"z"`, and `"+00:00"` are one value rather than three under the derived
`Eq`.

### Ordering is one relation, where Temporal has two

`Ord` is auto-derived everywhere. On `Instant` and on each plain type the field
order is significance-descending, so the derived order is the chronological one.
On `ZonedDateTime` it compares `instant` then `time_zone` lexically, which keeps
it consistent with the derived `Eq` but is not Temporal's `compare` — that one
weighs only the instant, while `equals` weighs the zone as well. Wado answers
both questions with one relation and points at `to_instant()` for "the same
moment, wherever it is read".

### `Duration` is plain data, not a validated construction

Temporal's constructor rejects a duration whose components disagree in sign.
A Wado struct literal has no constructor to reject anything, so the components
are not forced to share a sign: `sign()` reports the largest non-zero one and
arithmetic behaves as the sum of the parts. The ISO 8601 form is the one place
the invariant matters, and it asserts rather than assuming.

Field defaults make the literal the ergonomic constructor —
`Duration { hours: 1, minutes: 30 }` — and the same trick makes Wado's literal
spread (`PlainDate { ..d, day: 1 }`) Temporal's `with`, so no `with` method is
needed. `constrain` covers the month-end clamp that `with` would apply.

### What each type can be measured in

A date component has no fixed length, so the type that carries a calendar
position is the one that can resolve it:

| Receiver         | `add` / `subtract` accepts | `until` / `since` measures in     |
| ---------------- | -------------------------- | --------------------------------- |
| `Instant`        | hours and below            | hours and below (default: second) |
| `ZonedDateTime`  | everything                 | everything (default: hour)        |
| `PlainDate`      | date components            | days and above (default: day)     |
| `PlainTime`      | time components, wrapping  | hours and below                   |
| `PlainDateTime`  | everything                 | everything (default: hour)        |
| `PlainYearMonth` | years and months           | years or months                   |

Anything outside its row traps with a message naming the type that can do it.
Rounding follows the same rule: an `Instant` counts multiples from the epoch, a
`ZonedDateTime` and a `PlainTime` from local midnight, so a day-aligned unit
lands on the civil boundary rather than on an epoch multiple.

### `now()` rides the effect row

`Instant::now()` and `ZonedDateTime::now(time_zone)` declare `with SystemClock`,
the way `core:benchmark` declares `MonotonicClock`. The effect row, not
dead-code elimination, is what keeps the clock off callers that never ask the
time, so the module can offer `now()` without every user of a date type
acquiring a WASI import.

### Text forms

Every type parses and renders its ISO 8601 spelling, and that spelling is the
serde wire form — `Instant` and `ZonedDateTime` under CBOR's date/time tag 0
(RFC 8949 §3.4.1) with a bare string in JSON, the rest as plain strings.
Deserialization of the two instant-bearing types also accepts an epoch-seconds
number (tag 1 / JSON number), read as UTC. `FromStr` reads the same spellings
and is where each plain type's parser lives, so nothing copies its input to
reach a substring.

`Instant` additionally carries RFC 7231 IMF-fixdate, the form an HTTP `Date`,
`Expires`, or `Last-Modified` header takes. It renders that form and reads all
three a recipient must accept: IMF-fixdate, the obsolete RFC 850 (whose
two-digit year pivots at 70, since a pure parser has no clock to compare
against), and asctime.

A timestamp's sub-second fraction uses the least of 0, 3, 6, or 9 digits that is
exact; a duration's trims trailing zeros instead, because that is what ISO 8601
and Temporal spell for one.

### Bridging `wasi:clocks`

`wasi:clocks` has its own `Instant` record. That type is a Component Model
binding — pinned to a WASI version, regenerated by `wado-from-idl` — whereas
`core:temporal`'s is a plain, version-independent Wado type that grows methods.
They share a name and field layout deliberately: the same concept, a different
type. `From` impls both ways bridge them with a field-for-field copy.

## Known gaps

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

Whichever is chosen, DST forces a second decision. Today
`PlainDateTime::to_zoned_date_time` and `PlainDate::to_zoned_date_time` resolve a
local reading against a fixed offset, where the answer is unique. Against a named
zone it is not: a local time can fall in a spring-forward gap or a fall-back
overlap, so both need Temporal's `disambiguation`
(`compatible`/`earlier`/`later`/`reject`) and `offset`
(`use`/`ignore`/`prefer`/`reject`) options. `hours_in_day` returning a constant
24 and `days_in_week` returning 7 are the same gap seen from the other side.

### RFC 9557 zone annotations

Temporal's canonical `ZonedDateTime.toString()` is
`2023-11-14T22:13:20+09:00[Asia/Tokyo]` — RFC 9557 (IXDTF), an offset plus a
bracketed zone identifier. `core:temporal` neither emits nor parses the bracket,
and `parse_rfc3339` collapses the zone to the offset string it saw, so zone
identity does not survive a round trip. Closing it takes the bracket in both
directions plus Temporal's rule for reconciling a stored offset that disagrees
with what the named zone says at that instant — which needs the zone database
above, so this gap is downstream of it.

### `relativeTo` for calendar-unit durations

`Duration::total` and `Duration::round` trap on a duration carrying years,
months, or weeks, because those have no length without a calendar position, and
`Duration` has no ordering at all for the same reason — `Temporal.Duration`'s
`compare` also demands an anchor. Temporal's answer is a `relativeTo` argument
that turns the calendar components into exact time before measuring. Closing it
takes that parameter on `total` and `round`, a `compare` that takes it too, and
a decision about what it accepts — a `PlainDate`, a `ZonedDateTime`, or either.

### The system time zone

`Temporal.Now.timeZoneId` has no counterpart: `ZonedDateTime::now` takes the
zone from the caller. Reading the host's zone needs the unstable `wasi:clocks`
`timezone` interface (`iana-id()`), and an IANA name is exactly what the module
cannot yet interpret — so this gap closes with, not before, the zone database.

### Formatting beyond ISO 8601

Locale-aware formatting (`toLocaleString`) belongs to
[`core:icu`](./wep-2026-08-09-core-icu.md) and is tracked by that WEP's open item
on where its date/time formatting meets this module. Lenient parsing —
[`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) for the human-written
date spellings — is named as future work by that WEP and is not implemented
here either.

### Test coverage

`temporal_test.wado` covers each type's construction, ordering, text forms,
accessors, arithmetic, rounding, and serde. There is no property-style
round-trip over a wide instant range, no fixture pinning the CBOR tag-0 byte
encoding beyond the tag itself, and no test at the extremes of the `i64` second
range where the renderer produces years far outside four digits.

## Consequences

- The serde timestamp mapping is carried here, so `core:cbor` and `core:json`
  need no date/time knowledge of their own, and `core:log` gets an ISO 8601
  timestamp from a `wasi:clocks` reading through one `From`.
- Avoiding BigInt keeps every type a plain Wasm-GC struct with no special
  numeric support.
- ISO-8601-only is a deliberate limitation, revisited on demand.
- Until the tz database gap closes, `time_zone` is a string that promises more
  than the module delivers: an IANA name is storable, serializable, and traps on
  first use. That is the sharpest edge in the module today.
- `PlainMonthDay` stores no reference year, where Temporal keeps an ISO one
  (1972) so two month-days compare. Wado's derived `Ord` over `(month, day)`
  gives the same order without the field, at the cost of not round-tripping
  Temporal's `--MM-DD` reference-year form.
