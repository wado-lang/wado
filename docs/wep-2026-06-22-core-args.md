# WEP: Command-Line Argument Parsing (`core:args`)

## Context

Wado programs read raw arguments via `core:cli::args() -> List<String>`. Every
CLI must hand-roll option parsing on top of that. The standard library should
provide structured, type-safe argument parsing.

Design goals:

- **POSIX-like, long options only.** `--name`, `--name=value`. No short
  options, no `-abc` bundling, no prefix abbreviation. Minimal surface.
- **Object mapping via `core:serde`.** No bespoke argument derive — a CLI's
  argument shape is a normal `struct`/`variant` with
  `impl Deserialize for T;`, parsed by an argv `Deserializer`.
- **Subcommands, including nested, fall out of `variant` deserialization.**

### Prior Art

Rust's typed parsers (clap-derive, argh, gumdrop, bpaf-derive) all model
subcommands as an `enum` of variants, and all rely on procedural macros Wado
does not have. The closest precedent is `serde_args`, which drives an entire
parser off a serde `Deserialize` impl with no argument-specific derive. Its
limitations (no optional/variadic positionals, all positionals required) stem
from _implicitly_ detecting field kind; threading an explicit, format-agnostic
positional hint through the derive lifts them. The minimalist engines
(`lexopt`, `pico-args`) fix the irreducible core: tokenize argv, pull option
values, honor `--`.

### Prerequisites

- [WEP: Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md) —
  `core:args` is a `Deserializer`, and depends on the per-field positional
  resolution added there.

## Decision

`core:args` is a non-self-describing `Deserializer` over argv (parse-only),
in the same family as `core:json` and `core:json_nsd`. The generated
`Deserialize` code is reused verbatim; only the format implementation is new.

### Entry Points

```wado
// Effect-free core: testable, argv injected.
pub fn parse<T: Deserialize>(argv: List<String>) -> Result<T, ArgsError>;

// Thin wrapper over core:cli::args().
pub fn from_env<T: Deserialize>() -> Result<T, ArgsError> with Environment;
```

Splitting `parse` from `from_env` mirrors bpaf's `run_inner(args)` vs `run()`:
E2E tests and subcommand dispatch feed a `List<String>` directly.

### Object Mapping

Struct fields become `--long` options. The field type selects arity, exactly as
in `core:json` — no argument-specific attributes for the common case:

| Wado field type              | argv meaning                         |
| ---------------------------- | ------------------------------------ |
| `T` (scalar)                 | required `--name <value>`            |
| `Option<T>`                  | optional `--name <value>`            |
| `bool` + `#[serde(default)]` | flag `--name` (absent → `false`)     |
| `List<T>`                    | repeatable `--name a --name b`       |
| any + `#[serde(default)]`    | uses the type's zero-value if absent |

```wado
struct Cli {
    jobs: i32,                 // --jobs <n>      (required)
    output: Option<String>,    // --output <v>    (optional)
    #[serde(default)]
    verbose: bool,             // --verbose       (flag)
    #[serde(default)]
    include: List<String>,     // --include <v>   (repeatable)
}
impl Deserialize for Cli;
```

Option names are matched with `-`/`_` folding, so `--dry-run` binds the field
`dry_run` without a per-field rename. This is a format-level normalization in
`core:args` (it normalizes the option token before calling
`FieldSchema::lookup`); the synthesized wire names are unchanged.

### Positional Arguments

Positionals use the serde-general `#[serde(positional)]` field hint (see the
serde WEP). A positional field is filled from the stream of non-option tokens
in declaration order, never matched by `--name`:

```wado
struct Cli {
    #[serde(positional)]
    input: String,             // 1st positional (required)
    #[serde(positional)]
    out: Option<String>,       // 2nd positional (optional)
    #[serde(positional)]
    rest: List<String>,        // remaining positionals (variadic)

    jobs: i32,                 // --jobs <n>
    #[serde(default)]
    verbose: bool,             // --verbose
}
impl Deserialize for Cli;
// myprog in.txt out.txt a b --jobs 4 --verbose
```

Because the hint is explicit, optional (`Option<T>`) and variadic (`List<T>`)
positionals are supported — unlike `serde_args`' implicit scheme. Validation
(checked when the parser walks the schema):

- Positionals form a contiguous group in declaration order.
- Required positionals precede optional ones.
- At most one variadic positional, and it must be last.

### Subcommands as Variants

A subcommand set is a `variant` field. argv parsing reuses serde's existing
externally-tagged variant representation: the leading non-option token is the
external tag selecting the case; its payload struct is parsed from the rest.

```wado
struct AddArgs    { #[serde(positional)] path: String, #[serde(default)] all: bool }
struct CommitArgs { message: String }

variant Command {
    Add(AddArgs),
    Commit(CommitArgs),
    Remote(RemoteCmd),     // nested
}

variant RemoteCmd {        // sub-subcommands under `remote`
    AddRemote(AddRemoteArgs),
    List,                  // payload-less case
}

struct Cli {
    #[serde(default)]
    verbose: bool,         // global flag
    command: Command,      // leading bare token picks the case
}
impl Deserialize for Cli;
```

Nesting needs nothing new: a case payload that contains another `variant` field
recurses. Receivers dispatch with ordinary exhaustive pattern matching.

Interaction with positionals: fixed-arity positionals in a struct are consumed
first, then the next bare token starts the subcommand. A variadic positional in
the same struct as a subcommand `variant` is rejected (ambiguous boundary).

### Help and Version

- [ ] `--help` is generated by walking the (statically known) schema, with text
      drawn from doc comments on the type and fields. This is the one feature beyond
      the minimal engine worth its size — it is what separates a usable tool from a
      hand-written help constant.
- [ ] `--version` prints the package version.

### Errors

```wado
enum ArgsErrorKind {
    UnknownOption,      // --foo not a field
    MissingValue,       // --jobs with no value
    MissingArgument,    // required option/positional absent
    UnknownSubcommand,  // leading token matches no variant case
    InvalidValue,       // value fails to parse into the field type
    TooManyPositionals, // extra positional with no variadic sink
}

struct ArgsError { kind: ArgsErrorKind, message: String }
```

A program maps a real error to exit code 2; `--help`/`--version` print to
stdout and exit 0, mirroring clap's Unix convention.

### Module Layout

```
core:args   — parse/from_env, ArgsError, the argv Deserializer
```

`core:cli` stays I/O-focused (`println`, `args`, `env`, `exit`); `core:args`
owns parsing.

## Consequences

### Positive

- No new derive machinery: `core:args` is one `Deserializer`, peer to
  `core:json`/`core:json_nsd`. Argument types reuse `impl Deserialize for T;`.
- Subcommands are `variant`s, matched exhaustively — no macro layer hides them,
  and nesting is free.
- Effect-free `parse` makes CLIs testable; `from_env` is the only effectful
  surface.
- Small footprint: a lexopt-class tokenizer plus the shared serde driver.

### Negative

- `core:serde`'s data model resists a few CLI idioms; these are out of scope for
  the first cut: count flags (`-vv`), flag-with-optional-value, mutually
  exclusive groups. A future combinator layer (bpaf-style) can cover them
  without changing the derive.
- One shared `Deserialize` impl means a type used for both JSON and CLI shares
  wire names; CLI-only kebab matching is handled by `core:args`' option-name
  folding rather than the derive.

### Future Extensions

- [ ] Combinator escape hatch for count flags, optional-value flags, and
      exclusive groups.
- [ ] Shell completion generation from the schema.
- [ ] `core:cli` convenience that wires `from_env` + error printing + exit codes.

## References

- [WEP: Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
- [WEP: Variant Payload Design](./wep-2026-01-25-variant-payload-design.md)
- [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [serde_args](https://docs.rs/serde_args/), [bpaf](https://docs.rs/bpaf/),
  [lexopt](https://github.com/blyxxyz/lexopt)
