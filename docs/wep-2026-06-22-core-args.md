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

Struct fields become `--long` options. The field type and whether it carries a
declared default select arity — no argument-specific attributes:

| Wado field type  | argv meaning                               |
| ---------------- | ------------------------------------------ |
| `T` (no default) | required `--name <value>`                  |
| `T = expr`       | optional `--name <value>`, absent → `expr` |
| `Option<T>`      | optional `--name <value>`, absent → `null` |
| `bool = false`   | flag `--name`, absent → `false`            |
| `List<T>`        | repeatable, required (≥1 occurrence)       |
| `List<T> = []`   | repeatable, optional (≥0 occurrence)       |

```wado
struct Cli {
    input: String,             // --input <v>   (required)
    jobs: i32 = 1,             // --jobs <n>    (optional, default 1)
    output: Option<String>,    // --output <v>  (optional, absent → null)
    verbose: bool = false,     // --verbose     (flag)
    include: List<String> = [], // --include <v> (repeatable, optional)
}
impl Deserialize for Cli;
```

This is the language's "has default → optional, no default → required" rule
(see [Interaction with Default Field Values](#interaction-with-default-field-values)),
extended to argv. The common case carries no serde attributes at all.

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
    out: String = "out.txt",   // 2nd positional (optional, default)
    #[serde(positional)]
    rest: List<String> = [],   // remaining positionals (variadic)

    jobs: i32 = 1,             // --jobs <n>
    verbose: bool = false,     // --verbose
}
impl Deserialize for Cli;
// myprog in.txt out.txt a b --jobs 4 --verbose
```

Because the hint is explicit, optional positionals — via a declared default
(`out` above) or `Option<T>` — and variadic positionals (`List<T> = []`) are
supported, unlike `serde_args`' implicit scheme. Validation (checked when the
parser walks the schema):

- Positionals form a contiguous group in declaration order.
- Required positionals precede optional ones.
- At most one variadic positional, and it must be last.

### Interaction with Default Field Values

[Default field values](./wep-2026-04-11-default-arguments.md) are the backbone
of the arity model above. A declared field default makes the field optional on
deserialize (see the serde WEP's [Default Values for Missing Fields](./wep-2026-02-28-serde.md#default-values-for-missing-fields)),
so `core:args` needs no argument-specific default mechanism — it inherits the
language's uniform "has default → optional" rule:

- **Sensible defaults, not zero-values.** `port: i32 = 8080` makes `--port`
  optional defaulting to `8080`, the equivalent of clap's `default_value_t`,
  expressed in the language rather than an attribute. `#[serde(default)]` (which
  falls back to the type's zero-value) is rarely needed for CLIs.
- **`--help` shows defaults for free.** Default expressions are pure and
  compile-time-known, so help renders `--port <n>  (default: 8080)` without any
  per-field annotation — bpaf's `display_fallback` with no extra API.
- **Optional positionals.** A positional field with a default is an optional
  positional (`#[serde(positional)] dir: String = "."`).
- **Total empty-argv path.** A struct whose fields all have defaults
  auto-derives `Default`, so a no-argument invocation is exactly `Cli::default()`
  and parsing an empty argv cannot fail.

Defaults are restricted to pure expressions by the effect system, which CLI
defaults (literals, constants) always satisfy.

### Subcommands as Variants

A subcommand set is a `variant` field. argv parsing reuses serde's existing
externally-tagged variant representation: the leading non-option token is the
external tag selecting the case; its payload struct is parsed from the rest.

```wado
struct AddArgs    { #[serde(positional)] path: String, all: bool = false }
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
    verbose: bool = false, // global flag
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
      drawn from doc comments on the type and fields, and the default value of each
      defaulted field rendered inline (`--port <n>  (default: 8080)`). This is the
      one feature beyond the minimal engine worth its size — it is what separates a
      usable tool from a hand-written help constant.
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
