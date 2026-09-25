# Module System

Wado uses an ESM-like import syntax with `use {...} from "module"`. This aligns with JavaScript/TypeScript conventions, as JavaScript is a primary host environment for Wado.

## Visibility

Visibility has two orthogonal axes: a Wado scope ladder (`internal` / `pub`)
and a CM-surface flag (`export`). See [WEP: Visibility — `internal` / `pub` /
`export`](./wep-2026-06-25-visibility-internal-pub-export.md).

| Keyword    | Axis    | Reach                                             |
| ---------- | ------- | ------------------------------------------------- |
| (none)     | scope   | The defining file (private)                       |
| `internal` | scope   | Other files in the same package                   |
| `pub`      | scope   | Other Wado packages — the library API             |
| `export`   | CM flag | Also lowered at the CM boundary; CM-representable |

`pub` is the library boundary (Wado-native, so generics, closures, and traits
may cross it). `export` is the Component Model boundary and is additive:
`export ⟹ pub`, and an `export`ed signature must be CM-representable, checked at
the definition site.

```wado
// Private to this file (default)
fn helper() { ... }

// Package-internal - accessible from other files in this package
internal fn build_ast() -> Doc { ... }

// Library API - accessible from other Wado packages (Wado-native)
pub fn map<T, U>(f: fn(T) -> U, xs: List<T>) -> List<U> { ... }

// Library API + CM boundary export
export fn run() { ... }
```

| Declaration         | Same file | Same package | Other Wado packages | CM boundary |
| ------------------- | --------- | ------------ | ------------------- | ----------- |
| `fn foo()`          | Yes       | No           | No                  | No          |
| `internal fn foo()` | Yes       | Yes          | No                  | No          |
| `pub fn foo()`      | Yes       | Yes          | Yes                 | No          |
| `export fn foo()`   | Yes       | Yes          | Yes                 | Yes         |

A `pub`-only item reaches Wado consumers only (source dependency or
provider-tagged `.wasm`); a non-Wado CM consumer sees `export` items only.

The ladder applies to top-level items, struct fields, and `impl` members
(methods, associated constants); reaching one beyond its rung is a compile
error. `export` on a member is an error — a method has no CM boundary. Only an
_inherent_ member has a ladder; a trait impl's members reach as far as the
trait.

```wado
impl Config {
    fn parse_raw() { }         // this file only
    internal fn reload() { }   // other files in this package
    pub fn get() { }           // other packages
}
```

### Signature reach

An item's signature may not name a declaration that reaches less far than the
item itself. Naming one is a compile error at the reference. A caller that
reaches the item has to be able to write the types it names, and a `pub fn`
returning a file-private struct hands back a value whose type no caller can
write.

```wado
struct Hidden { n: i32 }

pub fn make() -> Hidden { ... }   // ERROR: widen `Hidden`, or narrow `make`
```

The rule holds at every rung: an `internal` item may not name a file-private
type either. `export` counts as `pub` here. Whether the type crosses the CM
boundary is a separate question, answered at the definition site.

Where an item carries no modifier of its own, its reach comes from what encloses
it. A struct field reaches no further than its struct. An impl's member reaches
no further than the impl's head, and a trait impl's no further than the trait
either: a caller has to be able to write the head to name the member. So
`impl Add for Local` on a file-private `Local` gives its `add` that same reach,
and `add` may then name `Local` freely. An impl's own bounds count as part of
its head: a type that cannot name `T`'s bound cannot satisfy it, so
`impl<T: Local> Add for T` confines `add` the same way.

A declared reach is a claim, so a bound on one is checked instead of narrowing
it. `pub fn f<T: Local>()` and `pub trait F<T: Local>` are errors, because no
caller can supply the `T` they ask for.

A type parameter, `Self`, and an associated-type projection (`Self::Output`,
`I::Item`) are binders rather than declarations, so they carry no reach of their
own and are not checked.

The signature is everything the caller has to be able to write or read back, not
just the parameters and the return type. A type parameter's default is
instantiated at the call, an impl's associated type comes back through
`Self::Out`, and a resource's parent carries the methods it inherits, so all
three obey the rule.

A bound obeys the rule too, so naming a less visible trait in one is the same
error. Rust's sealed-trait pattern seals a trait by giving it a supertrait that
implementors cannot reach, and that is exactly what this forbids. Wado has no
equivalent. If sealing is wanted, it gets a keyword that says so.

### Re-export visibility

A `use` declaration carrying a visibility modifier re-exports the imported names
as members of the importing module, at the modifier's reach:

| Form                          | Re-exported reach                           |
| ----------------------------- | ------------------------------------------- |
| `pub use { x } from "M"`      | `x` joins this module's public API          |
| `internal use { x } from "M"` | `x` is re-exported package-internal         |
| `use { x } from "M"`          | file-private import; `x` is not re-exported |

A re-export cannot reach further than `x` itself, so `pub use { x }` requires
`x` to be `pub`; claiming more is a compile error at the re-export. Narrowing is
allowed, and the facade still names the API — a package's entry module publishes
its items under its own names, so consumers never name the files behind them:

```wado
// foo/impl.wado — the implementation
pub fn compute() -> i32 { ... }

// foo.wado — the package's entry module
pub use { compute } from "./impl.wado";   // reached as foo's `compute`
```

You may also only re-export a name you can see: `x` must be importable here
(`x` is `pub`, or `x` is `internal` and `M` is in this package). Re-exporting a
file-private name is a visibility error, like any other import. See [Re-export Syntax (`pub use`)](./wep-2026-01-25-pub-use-reexport.md).

## Module Source Types

| Source Type   | Syntax                        | Example                              |
| ------------- | ----------------------------- | ------------------------------------ |
| WASI standard | `"wasi:<package>"`            | `"wasi:cli"`, `"wasi:filesystem"`    |
| Core library  | `"core:<module>"`             | `"core:cli"`, `"core:json"`          |
| CM coordinate | `"<ns>:<pkg>[@<ver>]"`        | `"docs:regex"`, `"docs:regex@1.0.0"` |
| Library alias | `"lib:<nick>"`                | `"lib:router"`, `"lib:shared"`       |
| Local file    | `"./<path>"` or `"../<path>"` | `"./utils.wado"`, `"../config.wado"` |

A specifier names a package only — no interface segment; interfaces and members are selected in the `use { ... }` list. `core:`/`wasi:` are bundled coordinates, not a separate scheme. See [WEP: Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md).

## Module Path Validation

Relative paths in Wado follow the gitignore / shell convention: a path that refers to a file relative to the current file must begin with `./` (next to me) or `../` (up one). A bare path (`foo/bar`, `utils.wado`) is never relative-to-here — it is read as a namespace/coordinate or handed to the host, and is rejected wherever only a relative file path is valid. This rule is uniform across every path literal: module imports (`use ... from`), `#include_str` / `#include_bytes`, and Kiln input paths (`from`, `generator.inputs`, `generator.output_dir`).

Module paths are validated before loading to provide clear error messages:

Namespace Resolution (a namespace is reserved iff the compiler bundles it):

1. Bundled namespaces `core:` / `wasi:`: resolved from the embedded stdlib.

2. Open coordinates `<ns>:<pkg>` (any other namespace): resolved from a `[dependencies]` entry in `wado.toml` or an inline `with` source. An undeclared coordinate is an error.

3. Library aliases `lib:<nick>`: resolved via `wado.toml` or an inline `with`. An alias renames a dependency, shortens its name, tells two major versions apart, or names a dependency with no public coordinate.

4. Local modules (`./` or `../`): Resolved relative to importing module.

5. Invalid paths: Paths not matching any pattern are rejected.
   - Error: `invalid module path 'xxx'; use './' for local modules or 'namespace:' for library modules`

Bare names (`"router"`) are rejected. The one exception is a bare key in `[dependencies]`, which is deprecated and draws a warning. See [WEP: Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md) for resolution and version rules.

## Symbol Notation

A symbol is named `MODULE#SYMBOL` — the written form used by docs, `wado query`, and diagnostics. `MODULE` is the import specifier verbatim (quoted as in `use`; quotes may be dropped for a scheme or bare name with no whitespace). `SYMBOL` uses Wado's own operators, so its kind is visible from the separator: `::` for static scope, `.` for an instance method, `^` for a trait-impl member.

```
core:json#to_string                        # free function / global
core:collections#TreeMap::new              # associated const / static fn
core:collections#TreeMap.get               # instance method
core:collections#TreeMap<String, i32>.get  # generics use Wado angle brackets
core:url#Url^Display::fmt                  # trait-impl member
"./utils.wado"#Helper::new                 # relative path — must be quoted
```

See [WEP: Symbol Notation](./wep-2026-06-14-symbol-notation.md).

## Import Syntax

```wado
// ============================================
// WIT Package = Wado Module
// WIT Interface = Wado interface
// ============================================

// 1. WASI standard modules (wasi:*)
use {Stdout, Stderr} from "wasi:cli";
use {Stdout::{write_via_stream}} from "wasi:cli";

// Interface and its members together
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

// 2. Core library (core:*)
use {println, eprintln} from "core:cli";
use {to_string, from_string} from "core:json";

// 3. Local files (relative path, extension required)
use {Helper} from "./utils.wado";
use {Config} from "../config.wado";

// 4. CM coordinate (declared in wado.toml, or given an inline `with` source)
use {Regexp} from "docs:regex";

// 5. Library alias (rename / private / coordinate-less dependency)
use {Router} from "lib:router";
```

Implementing a trait requires naming it: `impl Trait for Type` and the bodiless
derive form `impl Trait for Type;` both need `Trait` in scope, whether declared
in the module, imported, or auto-imported from the prelude.

```wado
use {Deserialize} from "core:serde";
impl Deserialize for Config;          // OK

impl Deserialize for Config;          // error without the import
```

An import's local name must not collide with a declaration in the importing
module. The name would mean two declarations at once and nothing could say
which, so the program is rejected; an alias says which one was meant.

```wado
use {Widget} from "./other.wado";
pub struct Widget { … }               // error: collides with the import

use {Widget as Theirs} from "./other.wado";
pub struct Widget { … }               // OK
```

## Import Attributes (`with`)

Use `with { ... }` to specify import metadata:

```wado
// Inline dependency source (single-file scripts; no wado.toml needed).
// Same vocabulary as a [dependencies] value, with an exact version.
use {Regexp} from "docs:regex@1.0.0" with { registry: "oci://ghcr.io/acme" };  // exact pin via the specifier
use {Router} from "lib:router" with { git: "https://github.com/user/router.git", ref: "v1.0" };
use {Parse}  from "lib:rx"     with { registry: "oci://ghcr.io/acme", package: "docs:regex", version: "1.0.0" };

// Type attribute (REQUIRED for non-.wado imports)
use {sin, cos} from "./libm.wasm" with { type: "wasm" };
```

An inline `with` source and a `wado.toml` entry for the same specifier are mutually exclusive. Version ranges (`^`/`~`/`=`) are allowed only in `wado.toml`, where a lock file resolves them; the specifier `@ver` and a single-file `with` take an exact version — a range there is an error.

### Type Attribute Requirement

| Import Source      | `type` Attribute         | Notes                          |
| ------------------ | ------------------------ | ------------------------------ |
| `.wado` files      | Optional                 | Type inferred from Wado source |
| `.wasm` files      | Required                 | `type: "wasm"`                 |
| `.wat` files       | Required                 | `type: "wat"`                  |
| `core:*`, `wasi:*` | Not applicable           | Bundled namespace handling     |
| `https:` URLs      | Required for non-`.wado` | Must specify content type      |
| CM / `lib:` deps   | Optional                 | Type inferred from package     |

### Rationale

Explicit type annotations prevent ambiguity and make dependencies clear, aligning with Wado's design philosophy of explicit imports.

## Generated Imports (Kiln)

See [WEP: Kiln](./wep-2026-04-12-kiln.md) and [WEP: Gale](./wep-2026-03-02-gale.md).

A `use` clause whose source is neither a `.wado` module nor a Wasm asset (`.wasm` / `.wat`) is processed by Kiln — a code-generation pipeline that lowers the input to ordinary Wado source which the compiler then handles like any user-authored module. `.g4`, `.proto`, `.graphql`, `.wit`, and a Wado dialect's own extension all take this path. The `with { generator: { ... } }` clause specifies which generator to invoke:

```wado
// Gale generates a parser from an ANTLR4 grammar
use { Parser } from "./Calc.g4" with {
    generator: {
        module: "wado-lang:gale",
    },
};

// With supplementary input files (paths relative to the source file)
use { RustParser } from "./Rust.g4" with {
    generator: {
        module: "wado-lang:gale",
        inputs: ["./RustLexer.g4"],
    },
};
```

### `with { generator: { ... } }` fields

| Field        | Required | Meaning                                                                                                                                  |
| ------------ | -------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `module`     | yes      | Generator module — either a `<namespace>:<name>[@<version>]` reference resolved against `[build-dependencies]`, or a relative `./` path. |
| `options`    | no       | Record literal whose shape matches the generator's exported `pub struct Options`. Omit when every field has a default.                   |
| `inputs`     | no       | Supplementary input paths the generator cannot discover from the primary alone (e.g. a sibling lexer grammar).                           |
| `output_dir` | no       | Override for the per-invocation generated-source directory (default `build/kiln/<synthesized-id>/`).                                     |

### Manifest

Generators are declared in `[build-dependencies]` of `wado.toml` (a build-only graph that does not enter the consuming project's runtime dependency graph):

```toml
[build-dependencies]
"wado-lang:gale" = { version = "^0.0.9" }
```

A bare `use { ... } from "./schema.g4"` against such a file with no `with` clause is a hard error (`KILN_MISSING_WITH`). Two `use` clauses for the same `from` in the same file collapse to a single invocation if their `(module, inputs, options, output_dir)` match; mismatched clauses are a duplicate-generator error.

A file that is neither `.wado` nor a Wasm asset is only ever reached through a generator. When a `use` names one and no invocation produced a module for that schema, the import is a hard error (`KILN_NO_GENERATED_MODULE`); the compiler never falls back to parsing the schema as Wado.

### Authoring a generator

A generator is a normal Wado package whose `wado.toml` maps the `core:kiln/generator` world to a module under `[world]`:

```toml
[world]
"core:kiln/generator" = "src/generator.wado"
```

That module exports the world's `generate` function:

```wado
use { Request, Response, Error } from "core:kiln";

pub struct Options {
    namespace: String,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    // ... parse req.primary.content, emit Wado source ...
}
```

Every use site's `options` is type-checked against the generator's `Options`. Generators run in a deterministic sandbox (no clocks, randomness, network, environment, or filesystem): every input they see arrives by value, listed at the use site. Outputs are persisted under `build/kiln/<synthesized-id>/` and stamped with a `#![generated(by = "...", sources = [...])]` header. A compile reruns a generator only when its inputs have changed.

## Wasm Module and Component Imports

A `.wasm` / `.wat` asset is imported directly with `with { type: "wasm" | "wat" }`. The compiler detects from the binary header whether the file is a core module or a Component Model component — both `.wasm` shapes use `type: "wasm"`; the distinction is detected, not declared. A single `use` may pull several names (functions from a core module, interfaces from a component).

| Imported file             | Exposes as                                     | Call style                                |
| ------------------------- | ---------------------------------------------- | ----------------------------------------- |
| Core wasm module / `.wat` | One free `pub fn` per export                   | `helper(x)` — plain function              |
| CM component (`.wasm`)    | One Wado `interface` per exported CM interface | `Iface::method(x)` — effectful, like WASI |

```wado
// Core wasm / wat — exports become free functions.
use { sin, cos } from "./libm.wat" with { type: "wat" };
use { helper }   from "./mod.wasm" with { type: "wasm" };

// CM component — each exported interface becomes a Wado `interface`,
// and its functions are called like WASI methods (effectful).
use { Compress, Decompress } from "./brotli.wasm" with { type: "wasm" };

export fn run() with (Compress, Decompress) {
    let packed = Compress::compress(bytes);
    let back = Decompress::decompress(packed);  // Result<List<u8>, String>
}
```

Values lower/lift across the CM boundary per [Type Mapping at Component Boundaries](./spec-components.md#type-mapping-at-component-boundaries). The dependency component is statically composed into the output, so the result runs standalone. See [WEP: Wasm Module Import](./wep-2026-01-10-wasm-import.md) for the core-wasm path and [WEP: Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) for the component path.

## Namespace Import

Use `use name from "..."` (without curly braces) to import an entire module as a namespace:

```wado
// Import a module as a namespace
use utils from "./utils.wado";
utils::helper_function();      // not utils["helper_function"], as it's analyzed at compile time
```

A namespace import binds one name, the namespace. The source module's pub symbols are reached through the `ns::` prefix and are not imported under their bare names, so `distance(p1, p2)` below is an unknown function:

```wado
use geo from "./geo.wado";

// Functions
geo::distance(p1, p2);

// Types (structs, enums, variants)
let p: geo::Point = geo::Point::origin();
let c = geo::Color::Red;
let s = geo::Shape::Circle(3.14);

// Traits and types in an `impl` header, on either side
impl geo::Show for Local { ... }
impl Show for geo::Tag { ... }
```

A qualified head names the namespace's declaration even where the importing
module declares one of its own by that name.

The namespace belongs to the importing file alone. It names a module, not an
item, so `pub use utils from "..."` is a compile error; re-export the members by
name (see [Re-exports](#re-exports-pub-use)).

### Note

Wado does not support `use * as name` or default imports.

## Import Rules

- Named imports use curly braces: `use {x, y} from "..."`
- Namespace imports omit curly braces: `use name from "..."`
- Wildcards prohibited: `use {*} from "..."` is not allowed
- All imports must be explicit (except the prelude)
- Use `::` for effect operation access: `Effect::{op1, op2}`

```wado
// Valid patterns
use {println, eprintln} from "core:cli";        // Named import
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
use utils from "./utils.wado";                   // Namespace import

// Prohibited patterns
use * from "core:cli";           // Wildcard not allowed
use {*} from "core:cli";         // Wildcard not allowed
```

Without braces, `use println from "core:cli"` is a namespace import named `println`, so the function is `println::println`.

## Calling Effect Operations

Effect operations use `::` syntax:

```wado
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

fn example() with Stdout {
    // With import - direct call
    write_via_stream(stream);

    // Fully qualified - always works
    Stdout::write_via_stream(stream);
}
```

Notation distinction:

- `.` → struct fields and methods (`user.name`, `stream.read()`)
- `::` → effect operations and namespace access (`Stdout::write_via_stream()`)

## Renaming Imports

```wado
use {Stdout::{write_via_stream as stdout_write}} from "wasi:cli";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

fn log() with (Stdout, Stderr) {
    stdout_write(out_stream);
    stderr_write(err_stream);
}
```

## Re-exports (`pub use`)

Re-exports make imported symbols available to other modules that import from this module:

```wado
// math/internal/trig.wado
pub fn sin(x: f64) -> f64 { ... }
pub fn cos(x: f64) -> f64 { ... }

// math/mod.wado - re-export from internal modules
pub use {sin, cos} from "./internal/trig.wado";
pub use {sin as sine} from "./internal/trig.wado";  // with rename

// user code - import from the facade (a "math" dependency declared in wado.toml)
use {sin, cos, sine} from "lib:math";
```

Re-export rules:

- `pub use` combines `pub` visibility with import syntax
- A re-export reaches no further than the symbol it names (see [Re-export visibility](#re-export-visibility))
- Re-export chains are resolved transparently (A re-exports from B, B re-exports from C)
- Circular re-exports are prohibited
- Only named items can be re-exported. A namespace (`pub use utils from "..."`) and a wildcard (`pub use _ from "..."`) are compile errors.
- A re-export stays at module level, so `export use` is a compile error

## Exception: The Prelude

The prelude is automatically imported into every module, making `String`, `List`, `Option`, `Result`, `Stream`, `Future`, and the prelude traits available without explicit imports.

## Standard Library

```
core            # core: namespace for the core library
├── prelude     # Automatically imported (String, List, Option, Result, Stream, Future)
├── cli         # CLI helpers (println, eprintln, args, env, exit, ...)
├── serde       # Serialization traits (Serialize, Deserialize, Serializer, Deserializer)
├── json        # JSON format implementation (to_string, from_string)
├── collections # TreeMap, TreeSet
├── base64      # Base64 encoding/decoding
├── zlib        # Compression
├── ...
wasi            # wasi: namespace for system interfaces
├── cli
├── filesystem
├── ...
```

The [cheatsheet's Standard Library section](./cheatsheet.md#standard-library) links the API reference for every module.

## Global Functions defined in `core:prelude`

```wado
panic("error"); // traps with a message
unreachable(); // traps  with no message
```
