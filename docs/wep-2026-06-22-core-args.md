# WEP: Command-Line Argument Parsing (`core:args`)

## Context

Wado programs read raw arguments via `core:cli::args() -> List<String>` and
hand-roll their own parsing. `core:args` adds structured, type-safe parsing.

Goals:

- **Long options only.** `--name`, `--name=value`. No short options, bundling,
  or prefix abbreviation.
- **Object mapping via `core:serde`.** No bespoke derive — argument types are
  ordinary `struct`/`variant` with `impl Deserialize for T;`.
- **Subcommands, nested included, fall out of `variant` deserialization.**

### Prior Art

Rust's typed parsers (clap-derive, argh, gumdrop, bpaf) model subcommands as an
`enum` of variants but rely on proc macros Wado lacks. `serde_args` drives a
parser off a serde `Deserialize` impl with no argument derive; its lack of
optional/variadic positionals stems from _implicitly_ detecting field kind,
which an explicit positional hint fixes. `lexopt`/`pico-args` fix the
irreducible core: tokenize argv, pull values, honor `--`.

### Prerequisites

- [Serde](./wep-2026-02-28-serde.md) — `core:args` is a `Deserializer` and uses
  its per-field positional resolution.

## Decision

`core:args` is a non-self-describing, parse-only `Deserializer` over argv, peer
to `core:json` and `core:json_nsd`. The synthesized `Deserialize` code is reused
verbatim; only the format is new.

### Entry Points

```wado
pub fn parse<T: Deserialize>(argv: List<String>) -> Result<T, ArgsError>;   // effect-free, testable
pub fn from_env<T: Deserialize>() -> Result<T, ArgsError> with Environment; // wraps core:cli::args()
```

`parse` takes argv directly, so tests and subcommand dispatch inject a
`List<String>` (bpaf's `run_inner` vs `run`).

### Object Mapping

Struct fields become `--long` options; the field type and presence of a declared
default select arity:

| Wado field type  | argv meaning                               |
| ---------------- | ------------------------------------------ |
| `T` (no default) | required `--name <value>`                  |
| `T = expr`       | optional `--name <value>`, absent → `expr` |
| `Option<T>`      | optional `--name <value>`, absent → `null` |
| `bool = false`   | flag `--name`, absent → `false`            |
| `List<T>`        | repeatable, required (≥1)                  |
| `List<T> = []`   | repeatable, optional (≥0)                  |

```wado
struct Cli {
    input: String,              // --input <v>   (required)
    jobs: i32 = 1,              // --jobs <n>    (optional, default 1)
    output: Option<String>,     // --output <v>  (optional)
    verbose: bool = false,      // --verbose     (flag)
    include: List<String> = [], // --include <v> (repeatable, optional)
}
impl Deserialize for Cli;
```

This is the language's "has default → optional" rule extended to argv; the
common case needs no attributes. Option names fold `-`/`_`, so `--dry-run` binds
`dry_run` (a `core:args` normalization before `FieldSchema::lookup`; wire names
unchanged).

### Positional Arguments

`#[serde(positional)]` (serde-general) fills a field from non-option tokens in
declaration order, never by `--name`:

```wado
struct Cli {
    #[serde(positional)] input: String,            // required
    #[serde(positional)] out: String = "out.txt",  // optional (default)
    #[serde(positional)] rest: List<String> = [],  // variadic
    jobs: i32 = 1,                                  // --jobs <n>
}
impl Deserialize for Cli;
// myprog in.txt out.txt a b --jobs 4
```

The explicit hint enables optional (default or `Option<T>`) and variadic
(`List<T> = []`) positionals. Schema validation requires: positionals contiguous
in declaration order, required before optional, at most one variadic (last).

### Interaction with Default Field Values

[Default field values](./wep-2026-04-11-default-arguments.md) carry the whole
arity model: a declared default makes a field optional on deserialize (serde
WEP, [Default Values for Missing Fields](./wep-2026-02-28-serde.md#default-values-for-missing-fields)),
so `core:args` adds no default mechanism of its own.

- `jobs: i32 = 1` is clap's `default_value_t`, in the language not an attribute.
- `--help` renders defaults (`--jobs <n>  (default: 1)`), pure and
  compile-time-known, with no annotation.
- A defaulted positional is an optional positional.
- An all-defaulted struct auto-derives `Default`, so empty argv → `Cli::default()`
  cannot fail.

Defaults must be pure (effect system); CLI defaults always are.

### Subcommands as Variants

A subcommand set is a `variant` field, read via serde's externally-tagged
representation: the leading non-option token is the tag selecting the case, its
payload parsed from the rest.

```wado
struct AddArgs { #[serde(positional)] path: String, all: bool = false }

variant Command {
    Add(AddArgs),
    Remote(RemoteCmd),     // nested: payload holds another variant
}
variant RemoteCmd { AddRemote(AddRemoteArgs), List }

struct Cli {
    verbose: bool = false,
    command: Command,      // leading token picks the case
}
impl Deserialize for Cli;
```

Nesting needs nothing new; receivers dispatch by pattern matching. Fixed-arity
positionals are consumed before the subcommand token; a variadic positional
alongside a subcommand `variant` is rejected (ambiguous boundary).

### Help, Version, Errors

- [ ] `--help` walks the schema, drawing text from doc comments and rendering
      each field's default inline. Worth its size: the line between a usable tool
      and a hand-written help string.
- [ ] `--version` prints the package version.

```wado
enum ArgsErrorKind {
    UnknownOption, MissingValue, MissingArgument,
    UnknownSubcommand, InvalidValue, TooManyPositionals,
}
struct ArgsError { kind: ArgsErrorKind, message: String }
```

A real error exits 2; `--help`/`--version` print to stdout and exit 0 (clap's
Unix convention).

### Module Layout

`core:args` owns parsing (`parse`, `from_env`, `ArgsError`, the argv
`Deserializer`); `core:cli` stays I/O-focused (`println`, `args`, `env`, `exit`).

## Consequences

### Positive

- One `Deserializer`, no new derive: argument types reuse `impl Deserialize for T;`.
- Subcommands are exhaustively-matched `variant`s; nesting is free.
- Effect-free `parse` keeps CLIs testable.
- Small footprint: a lexopt-class tokenizer plus the shared serde driver.

### Negative

- `core:serde`'s data model omits a few CLI idioms (count flags `-vv`,
  flag-with-optional-value, exclusive groups) — out of scope; a future combinator
  layer can add them without touching the derive.
- A type shared by JSON and CLI shares wire names; CLI kebab matching lives in
  `core:args`, not the derive.

### Future Extensions

- [ ] Combinator layer for count flags, optional-value flags, exclusive groups.
- [ ] Shell completion from the schema.
- [ ] `core:cli` helper wiring `from_env` + error printing + exit codes.

## References

- [Serde](./wep-2026-02-28-serde.md),
  [Variant Payload Design](./wep-2026-01-25-variant-payload-design.md),
  [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [serde_args](https://docs.rs/serde_args/), [bpaf](https://docs.rs/bpaf/),
  [lexopt](https://github.com/blyxxyz/lexopt)
