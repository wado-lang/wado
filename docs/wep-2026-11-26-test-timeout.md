# WEP: Test Timeout in Seconds

## Context

A test overrode the default timeout with `#[timeout_ms(N)]`. The attribute
carried its unit in its name, and the unit promised more than the runner keeps:
`wado test` interrupts a test on a clock that ticks once a second, so a test may
overrun its limit by up to a second. A value such as `300` read as a precise
bound and was not one.

Every other attribute that takes a named value writes it as `key = value`
(`#[param(from_env = "PORT")]`, `#[wire(name = "type")]`). `timeout_ms` was the
one attribute taking a bare number.

## Decision

### `#[timeout(seconds = N)]` replaces `#[timeout_ms(N)]`

The unit moves from the attribute's name into its key. `N` is a whole number
above zero. A test given no time cannot pass, so zero is an error rather than a
test that always fails.

`timeout_ms` is removed outright, with no period in which both are accepted. It
is now an unknown attribute.

### Seconds is the only unit

A `milliseconds` key would offer the precision the runner does not have, and a
second way to write the same limit. If the runner's clock becomes finer, a
finer unit can be added as another key without changing this one.
