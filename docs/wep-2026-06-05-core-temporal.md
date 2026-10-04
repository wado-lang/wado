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
- `timezone` (unstable, `feature = clocks-timezone`) — only `iana-id() -> option<string>`, `utc-offset(instant) -> option<s64>`, and a debug string. No
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
    /// The time zone identifier as Temporal canonicalizes it: `"UTC"` or a
    /// fixed offset such as `"+09:00"`. Mirrors the Temporal time-zone slot,
    /// which is also a string after the removal of `Temporal.TimeZone`.
    pub time_zone: String,
}
```

### Temporal is the specification

`core:temporal` is a port of Temporal as ECMA-262 specifies it. Where the
specification moves on from the TC39 proposal this module was ported from
(`wado-compiler/ref/tc39-temporal.md`), the module follows it.

The goal is that date and time code gives the same answers in JavaScript and in
Wado, so a program can do that work on either side. Three principles serve it:

- Within the range an ordinary application uses, the module behaves exactly as
  the specification says.
- Wado has no BigInt. A BigInt becomes an `i64` wherever one holds the value,
  and an `i128` only where nothing narrower does, since `i128` arithmetic is
  slow.
- A behaviour that differs from the specification is a bug, unless this WEP
  names it as a deliberate departure.

A float64 stays an `f64` where Temporal can produce a value an integer type
would hold differently. That is every `Duration` component, as the next section
says. Elsewhere every value Temporal can produce is an integer that an integer
type holds exactly, so the integer type is used.

The departures are these:

- A RangeError traps. A parser returns a `DeserializeError` instead, since its
  input is data.
- An options bag is a parameter list with defaults. The options it leaves out
  are known gaps.
- A struct literal skips every constructor check. So every operation checks its
  receiver and arguments first, and traps on a value Temporal could not hold: a
  date that does not exist, a mixed-sign duration, a zone that is not
  canonical.

### No BigInt; `i64` seconds is more than enough

Temporal stores `epochNanoseconds` as a BigInt and limits the range to ±10^8
days (≈ ±273,790 years). Wado has no arbitrary-precision integer, but it does not
need one: `i64` seconds spans ≈ ±292 billion years, dwarfing Temporal's range,
and `u32` nanoseconds gives full nanosecond resolution. This is exactly the
shape of `wasi:clocks` `instant`, so host conversion is a field-for-field copy.
`epoch_nanoseconds` returns `i128`, which covers that whole span without
truncating. Temporal's range still applies: an instant more than 10^8 days from
the epoch traps.

A duration component is an `f64` holding an integer, as Temporal's float64 is.
An `i64` would hold every component exactly, which Temporal does not: a
nanosecond count balanced from a span longer than about 104 days passes 2^53,
and Temporal rounds it to the nearest float. An `i64` would also trap where
Temporal goes on, past about 292 years of nanoseconds.

The arithmetic runs on Temporal's internal records instead, which are exact. A
time duration is a pair of `i64` seconds and nanoseconds rather than an `i128`,
since `i128` arithmetic is slow. An `i128` appears only where a value passes
2^63: reading a microsecond or nanosecond component that large, writing one
back as its nearest float, and dividing in `Duration::total`, whose float is
the one nearest the exact quotient.

### ISO 8601 only

Non-ISO calendars are not supported. Because the calendar is fixed, no type
stores a calendar field — one fewer string per value and no calendar-resolution
machinery. `era` and
`era_year` are undefined for ISO 8601 and are not offered; `month_code` is,
since it is part of the Temporal field vocabulary.

### Civil fields are derived, not stored

Following Temporal, a `ZonedDateTime`'s broken-down wall-clock fields are a
_function of_ `instant` + `time_zone`, not stored state. Storing only the instant
avoids representing redundant, possibly-inconsistent state; the accessors compute
the local date with Howard Hinnant's `civil_from_days`.

### UTC and fixed offsets only

No time zone database is bundled, so `"UTC"` is the only available named zone.
Every `±HH:MM` offset zone is available, as in Temporal. An IANA name is
storable in a struct literal, and every operation traps on it; see the gap
below.

A zone identifier is canonicalized where it enters, as Temporal does: `"utc"`
becomes `"UTC"`, `"-00:00"` becomes `"+00:00"`, and `"+0930"` becomes
`"+09:30"`. `"UTC"` and `"+00:00"` stay different zones, as `TimeZoneEquals`
says. `"Z"` is a designator rather than a zone, so `ZonedDateTime::new` refuses
it. `Instant::to_zoned_date_time` and `with_time_zone` also take an ISO string,
and read `Z` in one as `"UTC"`.

### Ordering is one relation, where Temporal has two

`Ord` is auto-derived everywhere. On `Instant` and on each plain type the field
order is significance-descending, so the derived order is the chronological one.
On `ZonedDateTime` it compares `instant` then `time_zone` lexically, which keeps
it consistent with the derived `Eq` but is not Temporal's `compare` — that one
weighs only the instant, while `equals` weighs the zone as well. Wado answers
both questions with one relation and points at `to_instant()` for "the same
moment, wherever it is read".

### `Duration` is plain data, and its arithmetic rebalances

Temporal's constructor rejects a duration that `IsValidDuration` refuses: mixed
signs, a year, month, or week count of 2^32 or more, or days and time totalling
2^53 seconds or more. It also rejects a component that is not a finite integer.
A Wado struct literal has no constructor, so such a literal is representable,
and every operation on it traps.

`add` and `subtract` rebalance rather than summing component-wise, as Temporal
does without a `relativeTo`: they sum the exact time, a day counting as 24
hours, and balance the result up to the larger of the two operands' largest
units. Component-wise, `1 hour - 30 minutes` would be
`{ hours: 1, minutes: -30 }`, which is not a valid duration. Years, months, and
weeks trap there for the same reason they trap in `total` and `round`.

Field defaults make the literal the ergonomic constructor —
`Duration { hours: 1, minutes: 30 }` — and the same trick makes Wado's literal
spread (`PlainDate { ..d, day: 1 }`) Temporal's `with`, so no `with` method is
needed. `constrain` covers the month-end clamp that `with` would apply.

### What each type can be measured in

A date component has no fixed length, so the type that carries a calendar
position is the one that can resolve it:

| Receiver         | `add` / `subtract` accepts             | `until` / `since` measures in     |
| ---------------- | -------------------------------------- | --------------------------------- |
| `Instant`        | hours and below                        | hours and below (default: second) |
| `ZonedDateTime`  | everything                             | everything (default: hour)        |
| `PlainDate`      | everything; time folds into whole days | days and above (default: day)     |
| `PlainTime`      | everything; days and above are ignored | hours and below (default: hour)   |
| `PlainDateTime`  | everything                             | everything (default: day)         |
| `PlainYearMonth` | years and months                       | years or months (default: year)   |

The two "everything" rows of the plain types are Temporal's: `PlainDate`
truncates its time to whole days, and `PlainTime` wraps. Anything outside its
row traps. `until` anchors at the receiver in both directions, so
`a.add(a.until(b))` is `b` even when `b` is earlier. `since` is `until` negated,
as in Temporal.

Rounding follows Temporal's operations. An `Instant` counts multiples from the
epoch, as if the count were positive, so `Trunc` rounds toward the past. A
`ZonedDateTime`, `PlainDateTime`, and `PlainTime` measure the quantity within
the next larger unit (`RoundTime`), so a half-even tie looks at the parity
within that unit. A `Duration` rounds with its sign.

### `now()` rides the effect row

`Instant::now()` and `ZonedDateTime::now(time_zone)` declare `with SystemClock`,
the way `core:benchmark` declares `MonotonicClock`. The effect row, not
dead-code elimination, is what keeps the clock off callers that never ask the
time, so the module can offer `now()` without every user of a date type
acquiring a WASI import.

### Text forms

Every type renders Temporal's `toString`, and `FromStr` reads Temporal's
RFC 9557 / ISO 8601 grammar for it: basic and extended formats, hour-only
times, `.` or `,` fractions of up to nine digits, sub-minute offsets, and
annotations. A `ZonedDateTime` renders as `2023-11-14T22:13:20+09:00[+09:00]`
and needs the zone annotation to parse. The plain types refuse a calendar other
than `iso8601`, and an `Instant` sets annotations aside.

The format specifier's precision is Temporal's `fractionalSecondDigits`: it
truncates to that many digits, and without one the fraction runs to its last
non-zero digit.

That string is the serde wire form. An `Instant`'s is RFC 3339 in the years
0000 to 9999, so there it goes under CBOR's date/time tag 0 (RFC 8949 §3.4.1).
An expanded year is not RFC 3339, so it goes untagged. A `ZonedDateTime`'s
carries an annotation tag 0 does not admit, so it goes untagged. JSON emits both bare, and
the rest are plain strings. Deserialization of the two instant-bearing types
also accepts an epoch-seconds number (tag 1 / JSON number), read as UTC.

`ZonedDateTime` also reads and writes RFC 3339 without the annotation, through
`parse_rfc3339` and `to_rfc3339`. `Z` reads as the `"UTC"` zone and an offset as
that offset zone.

`Instant` additionally carries RFC 7231 IMF-fixdate, the form an HTTP `Date`,
`Expires`, or `Last-Modified` header takes. It renders that form and reads all
three a recipient must accept: IMF-fixdate, the obsolete RFC 850 (whose
two-digit year pivots at 70, since a pure parser has no clock to compare
against), and asctime.

### Bridging `wasi:clocks`

`wasi:clocks` has its own `Instant` record. That type is a Component Model
binding — pinned to a WASI version, regenerated by `wado-from-idl` — whereas
`core:temporal`'s is a plain, version-independent Wado type that grows methods.
They share a name and field layout deliberately: the same concept, a different
type. `From` impls both ways bridge them with a field-for-field copy.

## Known gaps

### The IANA time-zone database

Every operation traps on a zone other than `"UTC"` and `±HH:MM`, so a
`ZonedDateTime` in `"Asia/Tokyo"` cannot be formatted or read at all. The data
comes from [`core:icu`](./wep-2026-08-09-core-icu.md), not from a tzdb of this
module's own and not from WASI. The reason is dedupe and altitude rather than
size: ICU4X carries zone data already, a second copy here would be one concept
with two implementations, and a stdlib that grows a bespoke data-bundling
mechanism per capability is the special case that should have been the shared
one. `core:icu` already carries an open item for this seam.

`wasi:clocks` `timezone` (unstable, `feature = clocks-timezone`) cannot serve
this even in principle: `utc-offset(when)` takes no zone, so it answers only for
the host's configured zone — a program cannot read an instant in `"Asia/Tokyo"`
on a host set to UTC. It also exposes no transition list, every function may
return `none`, and no host implements it (`timezone` appears in wasmtime's
`.wit` files and nowhere in its Rust). It stays useful for one thing only:
`Temporal.Now.timeZoneId`, once something implements it.

#### The size lever is the epoch, not the zone set

Measured with `zic` over tzdata's own `tzdata.zi`, whole database each time:

| build                           |        size |   gzip |
| ------------------------------- | ----------: | -----: |
| TZif fat, full history          |     682 KiB |        |
| TZif slim, full history         |     331 KiB | 84 KiB |
| TZif slim, from 1970            |     241 KiB |        |
| TZif slim, **from 2000**        | **126 KiB** | 32 KiB |
| TZif slim, from 2020            |      82 KiB |        |
| `tzdata.zi` (rules, not tables) |     114 KiB | 27 KiB |

Truncating history is worth more than switching to the rules form, and costs no
rule evaluator. Future timestamps are unaffected: the POSIX TZ footer survives
truncation, so a from-2000 build still resolves 2031 correctly.

Slicing by zone instead is rejected. It would make a program's zone set a
compile-time property, which a zone read from a config file or an HTTP header
cannot satisfy — a semantic restriction on the language bought with size. An
epoch cutoff restricts no expression, and choosing it at compile time is fine.

What the cutoff does cost is a wrong answer rather than a missing one: in a
from-2000 build `Asia/Tokyo` at 1950-06-15 reads +09:00, where the full database
has +10:00 for the 1948-51 JDT era. Whether a pre-cutoff instant should answer
that way, or trap, is open, as is which year.

#### ICU4X supplies the identifiers, not the offsets

Measured against `icu_time` 2.2, which carries four markers and no more:

| marker                              |  baked | serves                          |
| ----------------------------------- | -----: | ------------------------------- |
| `TimezoneIdentifiersIanaCoreV1`     | 9.5 KB | IANA ↔ BCP-47                   |
| `TimezoneIdentifiersIanaExtendedV1` | 9.7 KB | aliases, canonicalization       |
| `TimezoneIdentifiersWindowsV1`      | 8.6 KB | Windows names; nothing here     |
| `TimezonePeriodsV1`                 | 6.8 KB | a standard/daylight offset pair |

6.8 KB for every zone on earth is ~15 bytes each, against 283 bytes for
`America/New_York` alone in a from-2000 TZif. It is not a transition table and
cannot be one at that size. Querying it confirms the shape: `America/New_York`
answers `standard=-05:00 daylight=-04:00` at both 2005-06-15 and 2005-01-15 —
the pair a formatter needs to pick a display name, never which one is in effect.
`utc_offset(instant)` is not derivable from it. ICU4X says as much itself: the
API is `#[deprecated(since = "2.1.0", note = "this API is a bad approximation of a time zone database")]`. Its history is no better than a truncated tzdb either
— `Asia/Tokyo` at 1950 reads +09:00, and `Europe/London` reports BST as
_standard_ +01:00.

So the dedupe argument holds for identifiers and for the mechanism, and not for
the offsets: `core:icu` is where the IANA canonicalization comes from
(`Asia/Calcutta` → `Asia/Kolkata`, `US/Eastern` → `America/New_York`) and where a
data component would be hosted, but the transition data has to be tzdb's
whatever hosts it. Open: whether that rides the same blob as one more marker set
or arrives beside it.

#### DST disambiguation

Today `PlainDateTime::to_zoned_date_time` and `PlainDate::to_zoned_date_time`
resolve a local reading against a fixed offset, where the answer is unique.
Against a named zone it is not: a local time can fall in a spring-forward gap or
a fall-back overlap, so both need Temporal's `disambiguation`
(`compatible`/`earlier`/`later`/`reject`) and `offset`
(`use`/`ignore`/`prefer`/`reject`) options. `hours_in_day` returning a constant
24 and `days_in_week` returning 7 are the same gap seen from the other side.

### Options Temporal takes and this module does not

Each of these is a Temporal option with no parameter here, so the module always
behaves as Temporal's default would:

- the rounding options of `until` and `since` (`smallestUnit`,
  `roundingIncrement`, `roundingMode`);
- `overflow`, which is always `constrain`;
- `disambiguation` and `offset`, which a fixed offset never needs;
- the display options of `toString` beyond `fractionalSecondDigits`
  (`smallestUnit`, `roundingMode`, `calendarName`, `timeZoneName`, `offset`);
- `with`, `equals`, and `compare` as methods, beyond the derived `Eq` and `Ord`
  and the literal spread.

### `relativeTo` for calendar-unit durations

`Duration::total` and `Duration::round` trap on a duration carrying years,
months, or weeks, because those have no length without a calendar position, and
`Duration` has no ordering at all for the same reason — `Temporal.Duration`'s
`compare` also demands an anchor. Temporal's answer is a `relativeTo` argument
that turns the calendar components into exact time before measuring. Wado has no
counterpart, and what an anchor would accept is undecided: a `PlainDate`, a
`ZonedDateTime`, or either.

### The system time zone

`Temporal.Now.timeZoneId` has no counterpart: `ZonedDateTime::now` takes the
zone from the caller. It is the one question WASI's `timezone` could answer, and
it needs the zone database regardless — an IANA name is exactly what the module
cannot yet interpret.

### Formatting beyond ISO 8601

Locale-aware formatting (`toLocaleString`) belongs to
[`core:icu`](./wep-2026-08-09-core-icu.md) and is tracked by that WEP's open item
on where its date/time formatting meets this module. Lenient parsing —
[`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) for the human-written
date spellings — is named as future work by that WEP and is not implemented
here either.

### Test coverage

`temporal_test.wado` covers each type's construction, ordering, text forms,
accessors, arithmetic, rounding, and serde, the range limits, plus property
round-trips of `until` through `add` over random pairs and a sweep of the ISO
week anchors across -400..=400. There is still no property round-trip of the
_text_ forms over a wide instant range, no fixture pinning the CBOR tag-0 byte
encoding beyond the tag itself, and no run against Test262's Temporal tests.

## Consequences

- The serde timestamp mapping is carried here, so `core:cbor` and `core:json`
  need no date/time knowledge of their own, and `core:log` gets an ISO 8601
  timestamp from a `wasi:clocks` reading through one `From`.
- Avoiding BigInt keeps every type a plain Wasm-GC struct with no special
  numeric support.
- ISO-8601-only is a deliberate limitation, revisited on demand.
- Until the tz database gap closes, `time_zone` is a string that promises more
  than the module delivers: an IANA name is storable in a literal, and every
  operation traps on it, rendering and serializing included. That is the
  sharpest edge in the module today.
- `PlainMonthDay` stores no reference year, where Temporal keeps an ISO one
  (1972). Under ISO 8601 the reference year is always 1972, so the field would
  carry nothing, and the derived `Ord` over `(month, day)` gives Temporal's
  order.
