# Research: JSON Schema's Date and Time Formats

This records what the JSON Schema specifications, the Test Suite and the
implementations around them say about dates, times and durations, as of
2026-11-07. It holds facts only.

## Draft 2020-12

[Validation § 7.3.1](https://json-schema.org/draft/2020-12/json-schema-validation)
takes its date and time format names from RFC 3339 § 5.6.

| Format      | Valid when the string matches              |
| ----------- | ------------------------------------------ |
| `date-time` | RFC 3339 `date-time`                       |
| `date`      | RFC 3339 `full-date`                       |
| `time`      | RFC 3339 `full-time`                       |
| `duration`  | the `duration` rule of RFC 3339 Appendix A |

RFC 3339's `date-time` and `full-time` both end in a `time-offset`, so neither
admits a time without an offset.

The section also says:

- An implementation MAY support the other format names RFC 3339 defines, such
  as `partial-time`.
- If it implements `full-date` or `full-time`, it MUST implement `date` or
  `time` and make them behave identically.
- It SHOULD NOT define an extension attribute whose name matches an RFC 3339
  format, unless the attribute validates by that format's rule.

Draft 2020-12 defines no format for a date and time without an offset, or for a
time zone name.

## The next version

The `main` branch of
[json-schema-spec](https://github.com/json-schema-org/json-schema-spec/blob/main/specs/jsonschema-validation.md)
differs from 2020-12 in these ways:

- The format names come from RFC 9557 § 4.1, which extends RFC 3339 § 5.6.
- `date-time` matches RFC 9557's `date-time-ext`, which admits suffixes such as
  `[America/Los_Angeles]`. [#1572](https://github.com/json-schema-org/json-schema-spec/issues/1572)
  proposed this, and [#1609](https://github.com/json-schema-org/json-schema-spec/pull/1609)
  merged it in July 2025.
- `date-time` SHOULD allow and validate published leap seconds. `time` MUST NOT
  include leap seconds.
- An implementation SHOULD NOT define an extension attribute whose name matches
  an RFC 3339, RFC 9557 or ISO 8601 format.

Three changes were open on 2026-11-07:

- [#1795](https://github.com/json-schema-org/json-schema-spec/pull/1795)
  (October 2026) limits suffix validation to syntax. It forbids failing on an
  unknown IANA zone name, on an offset that disagrees with the zone, or on an
  unrecognized suffix key.
- [#1800](https://github.com/json-schema-org/json-schema-spec/pull/1800)
  (2026-11-02) returns `date-time` to RFC 3339 and removes the suffix tests. Its
  author later wrote that calling the decision a consensus was "a little
  strong".
- [#1603](https://github.com/json-schema-org/json-schema-spec/issues/1603) asks
  whether `duration` should admit fractional seconds (`PT0.5S`) and a sign
  (`-PT1S`). It is in the stable-release milestone, with no decision. A
  maintainer said the project reads formats strictly, and that any change
  belongs in the next version.

## The Test Suite

`vendor/JSON-Schema-Test-Suite/tests/draft2020-12/optional/format/` gives these
verdicts.

| Format      | Data                          | Valid |
| ----------- | ----------------------------- | ----- |
| `date-time` | `1963-06-19T08:30:06Z`        | yes   |
| `date-time` | `1963-06-19t08:30:06.283185z` | yes   |
| `date-time` | `1998-12-31T23:59:60Z`        | yes   |
| `date-time` | `1998-12-31T23:59:61Z`        | no    |
| `time`      | `08:30:06Z`                   | yes   |
| `time`      | `08:30:06z`                   | yes   |
| `time`      | `12:34:56-00:00`              | yes   |
| `time`      | `23:59:60Z`                   | yes   |
| `time`      | `12:00:00`                    | no    |
| `time`      | `12:00:00.52`                 | no    |
| `time`      | `08:30:06+0130`               | no    |
| `duration`  | `P1Y2M3DT4H5M6S`              | yes   |
| `duration`  | `P2W`                         | yes   |
| `duration`  | `PT0S`                        | yes   |
| `duration`  | `P1Y2D`                       | no    |
| `duration`  | `PT1H2S`                      | no    |
| `duration`  | `PT0.5S`                      | no    |
| `duration`  | `-P1D`                        | no    |
| `duration`  | `P1Y2W`                       | no    |

`P1Y2D` and `PT1H2S` fail because Appendix A's grammar lets a unit be followed
only by the next smaller one: `dur-year = 1*DIGIT "Y" [dur-month]`.

## Other Specifications

The [OpenAPI Format Registry](https://spec.openapis.org/registry/format/date-time-local)
lists `date-time-local`, "a RFC3339 date-time without timezone", and
[`time-local`](https://spec.openapis.org/registry/format/time-local), "a RFC3339
time without timezone".
[OAI/OpenAPI-Specification#4760](https://github.com/OAI/OpenAPI-Specification/pull/4760)
added both on 2025-07-10, to validate TOML's local date-times and local times.
The same request to JSON Schema,
[Discussion #876](https://github.com/orgs/json-schema-org/discussions/876),
received no reply from a maintainer.
[OAI/OpenAPI-Specification#1498](https://github.com/OAI/OpenAPI-Specification/issues/1498)
(2018) proposed a format that requires an offset, under names such as
`date-time-no-tz`.

## Implementations

- [ajv-formats](https://github.com/ajv-validator/ajv-formats) v3 requires the
  offset in `time` and `date-time`, to follow RFC 3339. It added `iso-time` and
  `iso-date-time`, whose offset is optional, for compatibility with its earlier
  behavior.
- [networknt/json-schema-validator#508](https://github.com/networknt/json-schema-validator/issues/508)
  and [Newtonsoft.Json.Schema#105](https://github.com/JamesNK/Newtonsoft.Json.Schema/issues/105)
  report `date-time` accepting a value without an offset.
- [NJsonSchema#436](https://github.com/RicoSuter/NJsonSchema/issues/436) and
  [kestra#5734](https://github.com/kestra-io/kestra/issues/5734) report `time`
  values without an offset, and the requests to accept them.
- Pydantic writes `format: "date-time"` for `datetime`, which also holds a value
  without an offset. [openapi-generator#19303](https://github.com/OpenAPITools/openapi-generator/issues/19303)
  reports the mismatch in generated clients.
  [pydantic Discussion #3477](https://github.com/pydantic/pydantic/discussions/3477)
  led to the `AwareDatetime` and `NaiveDatetime` types.
- [schemars](https://docs.fuchsia.dev/rust/src/schemars/json_schema_impls/chrono.rs.html),
  in the copy Fuchsia vendors, writes `partial-date-time` for chrono's
  `NaiveDateTime` and `NaiveTime`, and `date` for `NaiveDate`.
- A thread on the [OpenAI developer forum](https://community.openai.com/t/structured-response-datetime-format-not-supported-with-pydantic-schema/908403)
  reports structured outputs rejecting `format: "date-time"`, and users removing
  the keyword.

## What `core:temporal` Writes

`core:json` writes these strings. The verdict is the Test Suite's where it has
the case, and otherwise the RFC 3339 grammar's.

| Type             | Written                             | Format      | Valid |
| ---------------- | ----------------------------------- | ----------- | ----- |
| `Instant`        | `1970-01-01T00:00:01.5Z`            | `date-time` | yes   |
| `Instant`        | `-001199-02-15T14:13:20Z`           | `date-time` | no    |
| `PlainDate`      | `2024-02-29`                        | `date`      | yes   |
| `PlainDate`      | `+012345-01-01`                     | `date`      | no    |
| `PlainTime`      | `09:30:00`                          | `time`      | no    |
| `PlainDateTime`  | `2024-02-29T00:00:00`               | `date-time` | no    |
| `PlainYearMonth` | `2024-02`                           | none        |       |
| `PlainMonthDay`  | `02-29`                             | none        |       |
| `ZonedDateTime`  | `2023-11-14T22:13:20+09:00[+09:00]` | `date-time` | no    |
| `Duration`       | `PT0S`                              | `duration`  | yes   |
| `Duration`       | `P1Y2D`                             | `duration`  | no    |
| `Duration`       | `PT1H2S`                            | `duration`  | no    |
| `Duration`       | `PT0.5S`                            | `duration`  | no    |
| `Duration`       | `-P1D`                              | `duration`  | no    |
| `Duration`       | `P1Y2W`                             | `duration`  | no    |

RFC 3339's `date-fullyear` is four digits, which an expanded year such as
`+012345` does not match. A `ZonedDateTime` is valid under the `date-time` of
the `main` branch, which takes RFC 9557's suffix.
