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
- [Lenient String Parsing](./wep-2026-06-22-lenient-from-str.md) — scalar tokens
  are converted with `LenientFromStr`.

## Decision

`core:args` is a non-self-describing, parse-only `Deserializer` over argv, peer
to `core:json` and `core:json_nsd`. The synthesized `Deserialize` code is reused
verbatim; only the format is new.

### Implementation Status

Implemented (`lib/core/args.wado`, tested in `lib/core/args_test.wado`):

- [x] `parse` / `from_env` entry points.
- [x] `--name value`, `--name=value`, and `bool` flags; `-`/`_` folding; `--`
      end-of-options marker.
- [x] Required, optional (defaulted), and `Option<T> = null` options.
- [x] Positional arguments (required, optional, variadic) via serde's
      `FieldSchema::positional_at`.
- [x] Repeatable `List<T>` options (`--include a --include b`, interspersing
      allowed). A consumed-token mask lets `begin_seq` gather every occurrence of
      one option up front, so the on-demand value pull still drives flag-vs-value
      detection.
- [x] Subcommands (`variant` fields) via externally-tagged `begin_variant`,
      including nesting and `command [global-opts] subcommand [sub-opts]`. A
      subcommand field is marked `#[serde(positional)]` (see
      [Subcommands as Variants](#subcommands-as-variants)); the case tag matches
      the variant case name verbatim.
- [x] Lenient scalar conversion (`LenientFromStr`) and the `ArgsError` kinds.

Deferred:

- [ ] Schema validation (positionals contiguous, required-before-optional, ≤1
      variadic last; no variadic-positional + subcommand) — a CLI-specific
      well-formedness rule, not a data-model property, so it stays out of the
      format-agnostic compiler layer; `core:args` relies on correct declaration
      order. Currently mis-declaration is GIGO; only "too many positionals" is
      caught at runtime.
- [ ] `--help` / `--version`.

Lowercase / kebab subcommand tags are handled by serde's case rename, not by
args: `#[serde(rename_all = "kebab-case")]` on the subcommand `variant` (or
`#[serde(rename = "...")]` per case) makes the wire tag `add-remote` while the
cases stay idiomatic PascalCase. `core:args` matches the tag against that wire
name with no special support — the same rename that drives JSON/CBOR. (Case
_folding_ — accepting `add` for a `PascalCase`-wire `Add` — is intentionally not
done; the tag must equal the wire name, like every other format.)

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

| Wado field type    | argv meaning                               |
| ------------------ | ------------------------------------------ |
| `T` (no default)   | required `--name <value>`                  |
| `T = expr`         | optional `--name <value>`, absent → `expr` |
| `Option<T> = null` | optional `--name <value>`, absent → `null` |
| `bool = false`     | flag `--name`, absent → `false`            |
| `List<T> = []`     | repeatable, optional (≥0)                  |
| `List<T>`          | repeatable, required (≥1)                  |

```wado
struct Cli {
    input: String,                  // --input <v>   (required)
    jobs: i32 = 1,                  // --jobs <n>    (optional, default 1)
    output: Option<String> = null,  // --output <v>  (optional)
    verbose: bool = false,          // --verbose     (flag)
    include: List<String> = [],     // --include <v> (repeatable, optional)
}
impl Deserialize for Cli;
```

This is the language's single "has default → optional" rule extended to argv —
no type is special-cased, so an optional option is `Option<T> = null`.

Option names fold `-`/`_`, so `--dry-run` binds `dry_run` (a `core:args`
normalization before `FieldSchema::lookup`; wire names unchanged).

### Value Conversion

The `ArgvDeserializer`'s scalar `deserialize_*` methods convert the token with
[`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md), not strict `FromStr`:
`--jobs 0x10`, `--retries 1_000`, `--verbose=1` all parse, matching CLI
conventions. The boundary is clean — argv tokens become scalar leaves via
`LenientFromStr`, composite structure via serde. Tokens are already
shell-split, so no trimming; a failed conversion is `InvalidValue`. Parsing an
`enum` from a bare value (`--color red`) needs a lenient enum derive and is
deferred (see that WEP's future work).

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

The explicit hint enables optional (any field with a default, e.g.
`= "out.txt"` or `Option<T> = null`) and variadic (`List<T> = []`) positionals.
Schema validation requires: positionals contiguous in declaration order,
required before optional, at most one variadic (last).

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

A subcommand set is a `#[serde(positional)]` field whose type is a `variant`,
read via serde's externally-tagged representation: the leading non-option token
is the tag selecting the case, its payload parsed from the rest.

```wado
struct AddArgs { #[serde(positional)] path: String, all: bool = false }

variant Command {
    Add(AddArgs),
    Remote(RemoteCmd),     // nested: payload holds another variant
}
variant RemoteCmd { AddRemote(AddRemoteArgs), List }

struct Cli {
    verbose: bool = false,
    #[serde(positional)] command: Command,   // the subcommand tag is an ordinal slot
}
impl Deserialize for Cli;
```

The `#[serde(positional)]` marker is what lets `next_field` reach the variant
field at all: serde addresses a field only by name (`--option`) or by ordinal
(`positional_at`), and the subcommand tag is an ordinal token, not a `--name`.
This keeps the compiler at two format-agnostic addressing primitives — no
CLI-specific "which field is the subcommand" concept leaks into synthesis. The
externally-tagged tag-reading lives entirely in `core:args`'s `begin_variant`.

Because the payload continues from the same deserializer state,
`command [global-opts] subcommand [sub-opts]` works: global options (and
fixed-arity positionals) before the tag bind to the outer struct, then the case
payload — itself a struct — parses the rest, so its `--options` and positionals
fall out of the same machinery. Options after the tag bind to the subcommand
(standard Unix order, like `git --verbose commit` vs `git commit --verbose`).
Nesting needs nothing new; receivers dispatch by pattern matching.

The tag matches the variant case name **verbatim** (`Add`, not `add`); to get a
lowercase subcommand, name the case lowercase (`variant Command { add(..) }`).
A variadic positional alongside a subcommand is unsupported (the variadic would
swallow the tag) — keep them in separate structs.

### Help, Version, Errors

`core:args` uses two compile-time paths over the argument type: `Deserialize`
for parsing, and **static reflection** for help. Parsing alone cannot render
help — the synthesized `Deserialize` only fills values; it exposes no doc
comments or default values. `--help` walks the type's reflected metadata
(field name, `-`/`_`-folded option name, doc comment, required-ness, and the
default's display string) and the `variant` cases (for subcommand help).

- [ ] `--help` walks the reflected schema, drawing text from doc comments and
      rendering each field's default inline (`--port <n>  (default: 8080)`).
      Depends on static reflection exposing a per-field `default_display:
      Option<String>` — the default value rendered via the field type's
      `Display`. Defaults are pure and compile-time-known, so this is a compile
      -time constant (no runtime reflection); `has_default` alone
      ([reflect-derivation](./wep-2026-06-13-reflect-derivation.md)) gives
      presence but not the value. The whole help text can be a `const`.
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
- [ ] `enum`-valued options (`--color red`) once a lenient enum derive lands.
- [ ] Shell completion from the schema.
- [ ] `core:cli` helper wiring `from_env` + error printing + exit codes.
- [ ] Subcommand-aware error context. `ArgsError` carries only `kind` + `message`,
      so a caller wanting a `myprog gen: ...` prefix must re-derive the active
      subcommand from `argv[0]` itself (fine for single-level CLIs). A future
      `ArgsError` could record the subcommand path the parse failed under (the
      tags seen by `begin_variant`); the program name stays caller-supplied, and
      nested paths need an accumulated breadcrumb, so this waits until nested
      subcommand diagnostics are a real pain.

## References

- [Serde](./wep-2026-02-28-serde.md),
  [Lenient String Parsing](./wep-2026-06-22-lenient-from-str.md),
  [Variant Payload Design](./wep-2026-01-25-variant-payload-design.md),
  [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [serde_args](https://docs.rs/serde_args/), [bpaf](https://docs.rs/bpaf/),
  [lexopt](https://github.com/blyxxyz/lexopt)
