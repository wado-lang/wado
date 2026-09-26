# Module System

Wado uses an ESM-like import syntax with `use {...} from "module"`. This aligns with JavaScript/TypeScript conventions, as JavaScript is a primary host environment for Wado.

## Visibility

Visibility has two orthogonal axes: a Wado scope ladder (`internal` / `pub`)
and a CM-surface flag (`export`).

| Keyword    | Axis    | Reach                                             |
| ---------- | ------- | ------------------------------------------------- |
| (none)     | scope   | The defining file (private)                       |
| `internal` | scope   | Other files in the same package                   |
| `pub`      | scope   | Other Wado packages — the library API             |
| `export`   | CM flag | Also lowered at the CM boundary; CM-representable |

`pub` is the library boundary (Wado-native, so generics, closures, and traits
may cross it). `export` is the Component Model boundary and is additive:
`export ⟹ pub`, and an `export`ed signature must be CM-representable, checked at
the definition site. Every type it names needs a Component Model counterpart in
[Type Mapping at Component Boundaries](./spec-components.md#type-mapping-at-component-boundaries),
so a closure-typed parameter of an `export fn` is a compile error.

`internal` combines with neither `pub` nor `export`; writing both is a compile
error. `pub export` is accepted and means `export`.

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

`pub` is absolute. A module has no privacy of its own beyond its file, so there
is no `pub(crate)` / `pub(super)` family, and no enclosing module can narrow a
`pub` item.

The ladder applies to top-level items, struct fields, and `impl` members
(methods, associated constants); reaching one beyond its rung is a compile
error. `export` on a member is an error, because a method has no CM boundary. Only an
_inherent_ member has a ladder; a trait impl's members reach as far as the
trait.

```wado
impl Config {
    fn parse_raw() { }         // this file only
    internal fn reload() { }   // other files in this package
    pub fn get() { }           // other packages
}
```

### Packages

`internal` reaches the files of one package. The packages are:

- The entry module, every local module it reaches through `./` / `../`
  imports, and the Wasm assets those modules import.
- Each dependency. A relative import inside a dependency stays in that
  dependency's package.
- `core:*`, which is one package, and `wasi:*`, which is another.
- A [generated module](#generated-imports-kiln) belongs to the package of the
  module that imports it.

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
allowed.

A module can re-export only a name it can see: `x` must be importable there
(`x` is `pub`, or `x` is `internal` and `M` is in this package). Re-exporting a
file-private name is a visibility error, like any other import.

A name reached through a chain of re-exports reaches as far as the narrowest
hop in the chain.

Rationale: [WEP: Visibility — `internal` / `pub` / `export`](./wep-2026-06-25-visibility-internal-pub-export.md).

## Module Source Types

| Source Type   | Syntax                        | Example                              |
| ------------- | ----------------------------- | ------------------------------------ |
| WASI standard | `"wasi:<package>"`            | `"wasi:cli"`, `"wasi:filesystem"`    |
| Core library  | `"core:<module>"`             | `"core:cli"`, `"core:json"`          |
| CM coordinate | `"<ns>:<pkg>[@<ver>]"`        | `"docs:regex"`, `"docs:regex@1.0.0"` |
| Library alias | `"lib:<nick>"`                | `"lib:router"`, `"lib:shared"`       |
| Local file    | `"./<path>"` or `"../<path>"` | `"./utils.wado"`, `"../config.wado"` |

A specifier names a package or a local file. It never carries an interface segment: interfaces and their members
are selected in the `use { ... }` list (`Iface`, `Iface::{op}`). `core:` and
`wasi:` are coordinates whose namespace is bundled with the compiler, not a
separate scheme. Nested namespaces (`a:b:pkg`) follow WIT.

## Module Path Validation

Relative paths in Wado follow the gitignore / shell convention: a path that refers to a file relative to the current file must begin with `./` (next to me) or `../` (up one). A bare path (`foo/bar`, `utils.wado`) never refers to a file next to the current one. It is read as a namespace or coordinate, or handed to the host, and it is rejected wherever only a relative file path is valid. This rule holds for every path literal: module imports (`use ... from`), `#include_str` / `#include_bytes`, and Kiln input paths (`from`, `generator.inputs`, `generator.output_dir`).

A module path resolves by its form:

1. Bundled namespaces `core:` / `wasi:`: resolved from the embedded stdlib.

2. Open coordinates `<ns>:<pkg>` (any other namespace): resolved from a `[dependencies]` entry in `wado.toml` or an inline `with` source. An undeclared coordinate is an error.

3. Library aliases `lib:<nick>`: resolved via `wado.toml` or an inline `with`. An alias renames a dependency, shortens its name, tells two major versions apart, or names a dependency with no public coordinate.

4. Local modules (`./` or `../`): Resolved relative to importing module.

5. Invalid paths: Paths not matching any pattern are rejected.
   - Error: `invalid module path 'xxx'; use './' for local modules or 'namespace:' for library modules`

The reserved namespaces are `core`, `wasi`, and `lib`. `core` and `wasi` are
bundled; `lib` is not. Every other namespace is open.

`lib` is the one place an alias lives: a `[dependencies]` key under any other namespace is the
dependency's own coordinate, and a `lib:` key names the real coordinate it
stands for with its `package` field. A `[dependencies]` key is byte-identical to
the specifier that uses it.

Bare names (`"router"`) are rejected. The one exception is a bare key in `[dependencies]`, which is deprecated and draws a warning.

Rationale: [WEP: Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md).

## Symbol Notation

Docs, `wado query`, and diagnostics write a symbol as `MODULE#SYMBOL`. `MODULE` is the import specifier verbatim, so any module a `use` can import can be named. `SYMBOL` uses Wado's own operators, so its kind is visible from the separator:

| Symbol kind                                       | Written            |
| ------------------------------------------------- | ------------------ |
| Free function or global                           | `name`             |
| Associated constant, static function, nested item | `Type::name`       |
| Instance method                                   | `Type.name`        |
| Trait-impl member                                 | `Type^Trait::name` |

```
core:json#to_string                        # free function / global
core:collections#TreeMap::new              # associated const / static fn
core:collections#TreeMap.get               # instance method
core:collections#TreeMap<String, i32>.get  # generics use Wado angle brackets
core:url#Url^Display::fmt                  # trait-impl member
"./utils.wado"#Helper::new                 # relative path — must be quoted
```

`MODULE` is quoted as in `use`. The canonical form, which `wado query` and doc
anchors use, always quotes it. In prose the quotes may be dropped for a scheme or
a bare name with no whitespace. A relative path is always quoted,
because it can contain `#`, `/`, and `.`.

Rationale: [WEP: Symbol Notation](./wep-2026-06-14-symbol-notation.md).

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

An inline source takes the same keys as a `[dependencies]` value: `git`, `ref`,
`registry`, `package`, `path`, and an exact `version`. An inline `with` source and a `wado.toml` entry for the same specifier are mutually exclusive.

Version ranges (`^`/`~`/`=`) are allowed only in `wado.toml`, where a lock file resolves them. The specifier's `@ver` and an inline `with` take an exact version, and a range there is an error.

Two other keys have sections of their own: `generator` makes the import a
[generated import](#generated-imports-kiln), and `provider` satisfies a
component's guest effect ([Wasm Module and Component Imports](#wasm-module-and-component-imports)).

### Type Attribute Requirement

| Import Source      | `type` Attribute | Notes                          |
| ------------------ | ---------------- | ------------------------------ |
| `.wado` files      | Optional         | Type inferred from Wado source |
| `.wasm` files      | Required         | `type: "wasm"`                 |
| `.wat` files       | Required         | `type: "wat"`                  |
| `core:*`, `wasi:*` | Not applicable   | Bundled namespace handling     |
| CM / `lib:` deps   | Optional         | Type inferred from package     |

`"wasm"` and `"wat"` are the only values that make an import a Wasm asset. A
`.wasm` or `.wat` path without one is read as a schema, which needs a
generator.

Rationale: an explicit `type` keeps a Wasm import unambiguous and its
dependency visible, as Wado's imports are explicit elsewhere.

## Generated Imports (Kiln)

A `use` clause whose source is neither a `.wado` module nor a Wasm asset (`.wasm` / `.wat`) goes through Kiln, Wado's code generation step. A generator turns the input into ordinary Wado source, which is then compiled like any hand-written module. `.g4`, `.proto`, `.graphql`, `.wit`, and a Wado dialect's own extension all take this path. The `with { generator: { ... } }` clause names the generator.

The examples use Gale, a generator that builds a parser from an ANTLR4 grammar
([WEP: Gale](./wep-2026-03-02-gale.md)):

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

The literal after `from` is the primary input. It is a `./` or `../` path
resolved against the declaring file, like a local module import.

### `with { generator: { ... } }` fields

Each field has one type, and any other key or type is an error at the use site.

| Field        | Required | Meaning                                                                                                                                                                                           |
| ------------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `module`     | yes      | The generator: a `./` / `../` path to its source, or a `<namespace>:<name>[@<version>]` coordinate or `lib:<nick>` alias resolved against `[build-dependencies]`. A bare name is an error.        |
| `version`    | no       | Exact version of a coordinate `module`, for a file with no `wado.toml`. An error beside a path `module`, or when the manifest declares the generator.                                             |
| `registry`   | no       | Registry of a coordinate `module` (`oci://<host>[/<prefix>]`), under the same conditions as `version`.                                                                                            |
| `options`    | no       | Record literal whose shape matches the generator's exported `pub struct Options`. See [Options](#options).                                                                                        |
| `inputs`     | no       | Supplementary input paths (`./` / `../`) the generator cannot discover from the primary alone, such as a sibling lexer grammar. A schema that refers to other files lists every one of them here. |
| `output_dir` | no       | A `./` / `../` directory, resolved against the declaring file, that receives the generated files. Default `build/kiln/<synthesized-id>/` under the package root.                                  |

Every file a generator sees is named literally at the use site. There is no
glob and no directory listing, so the whole input set is known before any
generator runs.

### Binding the import

A generator emits exactly one entry module and zero or more supplementary
modules. The `use` binds against the entry module, under the ordinary
visibility rules; supplementary modules are ordinary Wado files that the entry
reaches with ordinary `use` statements. The entry module does not need to
exist before the first compile, because the generator runs before the import
is resolved.

A clause applies in the file that declares it. Another `use` of the same schema
in that file, with no `with` of its own, binds against the same entry. Another
file gets no binding from it.

Clauses are collected from every module the program reaches, so a module deep
in the graph can import a generated module of its own. A generator is an
ordinary Wado package, so its own source may import a generated module too. An
invocation whose generator is built from its own output, directly or through
other invocations, is a cycle, and the cycle is an error naming the invocations
in it.

Generated files are compiled exactly like hand-written source, so a generator
that emits invalid Wado fails with an ordinary error against the generated file
on disk. Only the diagnostics a generator reports itself point
into its input files.

### Errors

- A `use` of a file that is neither `.wado` nor a Wasm asset is
  `KILN_MISSING_WITH` when the importing file declares no `with { generator }`
  clause for it.
- A `use` whose generator produced no module for that schema is
  `KILN_NO_GENERATED_MODULE`. The compiler never falls back to parsing the
  schema as Wado.
- Two clauses that agree on `module`, the primary input, `inputs`, `options`,
  and `output_dir` are one invocation, wherever in the program they appear.
  Two clauses that share a primary input but disagree on any of those are an
  error naming both.

### Options

A generator declares its options as `pub struct Options`, and every use site's
`options` is checked against it before the generator runs, so a typo or type
mismatch is reported on the offending key.

- Omitting `options` means every field takes its default. A field with a
  default may be omitted on its own.
- A field of type `Option<T>`, `List<T>`, or `TreeMap<String, V>` may always
  be omitted. It is then `None`, the empty list, or the empty map.
- Any other field without a default must be supplied. An unknown key is an
  error.
- An `enum` option is written as its case name in a string.
- A `TreeMap<String, V>` option is written as an object whose keys are the
  author's own. The generator receives it sorted by key.
- A generator resolved as a prebuilt component carries no field defaults, so
  at its use sites every field is required unless its type is an `Option`, a
  `List`, or a `TreeMap`.

### Manifest

Generators are declared in `[build-dependencies]` of `wado.toml` (a build-only graph that does not enter the consuming project's runtime dependency graph):

```toml
[build-dependencies]
"wado-lang:gale" = { version = "^0.0.9" }
```

Generated code that calls a runtime library the generator's package ships
imports it like any other dependency, so the consumer also lists that package
under `[dependencies]`.

### Authoring a generator

A generator is a normal Wado package whose `wado.toml` maps the `core:kiln/generator` world to a module under `[world]`:

```toml
[world]
"core:kiln/generator" = "src/generator.wado"
```

That module exports the world's `generate` function:

```wado
use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

pub struct Options {
    namespace: String,
    emit_comments: bool = true,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let Ok(schema) = read_text(req.primary.content) else {
        return Result::Err(Error::InvalidSchema("not UTF-8"));
    };
    let source = emit(&schema, &req.options);   // the generator's own work
    return Result::Ok(Response {
        files: [OutputFile { path: `${req.options.namespace}.wado`, content: source, is_entry: true }],
    });
}
```

A generator with no configuration declares no `Options` and writes
`fn generate(req: Request)`.

What a generator receives and returns:

- `req.primary` and `req.inputs` are `InputFile`s: the path as the use site
  wrote it, and the content as a `Stream<u8>`. `read_all` and `read_text`
  collect a whole file.
- `req.module` is how the use site named the generator: the `module:`
  specifier as written, or for a relative path, that path from the project
  root. Output that imports a library the generator's package also ships names
  it through this, since only the use site knows what the consumer calls that
  package.
- `req.options` is the use site's options, with defaults filled in.
- It returns a `Response` whose `files` are `OutputFile`s: a path relative to
  the output directory, the Wado source, and whether this file is the entry
  module. Or it returns an `Error`: `InvalidSchema`, `Unsupported`, or
  `Other`, each with a message.
- It may report diagnostics through the `KilnHost` effect, each optionally
  spanning a byte range of one of the files it received. They surface as
  ordinary compile diagnostics.

An `Options` field is one of `bool`, a fixed-width integer, `f32`, `f64`,
`String`, a payload-less `enum`, a non-recursive `struct` of such fields,
`Option<T>`, `List<T>`, or `TreeMap<String, V>`. A field default is a literal.
Any other field fails the generator's own compile, since no use site could
supply it.

A generator runs in a deterministic sandbox: no clocks, randomness, network,
environment, or filesystem. Every input arrives by value, listed at the use site,
and every output is returned in the response. A generator that imports a
`wasi:*` interface, directly or through a `core:*` module, is a compile error
(`KILN_GENERATOR_FORBIDDEN_IMPORT`).

Outputs are written to the invocation's output directory, each stamped with a
`#![generated(by = "...", sources = [...])]` header naming the generator and
its inputs. A file in that directory that carries the header belongs to the
invocation, and a later run may overwrite or remove it. A file without it is
left alone. A compile reruns a generator only when its generator, the name it
was invoked under, its inputs, or its options have changed.

Rationale: [WEP: Kiln](./wep-2026-04-12-kiln.md).

## Wasm Module and Component Imports

A `.wasm` / `.wat` asset is imported directly with `with { type: "wasm" | "wat" }`. Whether the file is a core module or a Component Model component is detected from its content, not declared, and either may be written as `.wasm` or `.wat`. A single `use` may pull several names (functions from a core module, interfaces from a component). The path is a `./` or `../` path.

| Imported file | Exposes as                                     | Call style                            |
| ------------- | ---------------------------------------------- | ------------------------------------- |
| Core module   | One free `pub fn` per function export          | `helper(x)` — plain function          |
| CM component  | One Wado `interface` per exported CM interface | `Iface::method(x)` — called like WASI |

```wado
// Core wasm / wat — exports become free functions.
use { sin, cos } from "./libm.wat" with { type: "wat" };
use { helper }   from "./mod.wasm" with { type: "wasm" };

// CM component — each exported interface becomes a Wado `interface`,
// and its functions are called like WASI methods.
use { Compress, Decompress } from "./brotli.wasm" with { type: "wasm" };

export fn run() {
    let packed = Compress::compress(bytes);
    let back = Decompress::decompress(packed);  // Result<List<u8>, String>
}
```

### Core modules

Each function export becomes a free function of the same name. A call to it
calls that export, whatever the name: an export named like a `core:builtin`
intrinsic or like another asset's export is still its own asset's function. An
export spelled like a Wado keyword is imported under an alias
(`use { resume as seven } from ...`).

An asset is embedded in the output and shares the component's memory. It is
rejected at the import when it:

- is not a valid module;
- imports anything but `env.memory`;
- has more than one memory, counting the imported one, or a memory that is
  64-bit, shared, or has a custom page size;
- has a `start` section;
- exports a function that re-exports an imported one;
- exports a function whose parameters are not `i32`, `i64`, `f32`, `f64`, or
  `v128`, or that has more than one result, or a result of any other type.

`use _ from "./x.wasm" with { type: "wasm" }` embeds an asset without binding
any name.

### Components

A component is consumed through the type it carries. No Wado declaration file
or side-car `.wit` is involved.

- An exported interface becomes a Wado `interface`. Its named types become Wado
  items of the corresponding kind: a record a `struct`, a variant a `variant`,
  an enum an `enum`, flags a `flags`, and a type alias a newtype.
- A function the component's world exports directly becomes a free function,
  imported by bare name.
- An `async func` becomes an `async fn` returning `AsyncCall<T>`
  ([Async Imports](./spec-components.md#async-imports)).
- Values lower and lift per
  [Type Mapping at Component Boundaries](./spec-components.md#type-mapping-at-component-boundaries).
  A `stream<T>` or `future<T>` value is the readable end; the writable end stays
  with whoever created the pair.

The dependency is statically composed into the output, so the result is one
self-contained component that runs standalone.

An imported interface is not an effect by construction. Calling into a
component requires the effects its own host imports map to
(`wasi:clocks/monotonic-clock` requires `MonotonicClock`), and a component that
imports nothing from the host requires no effect. A component that imports an
interface no host provides (a guest effect) makes that interface an effect its
caller must handle. `with { provider: "./impl.wado" }` supplies it instead: the
named Wado file is compiled into a component that exports the interface, bound
by operation name, and composed in, so the caller needs no handler. A `provider`
on a component that imports no guest effect is an error.

```wado
use { Hlc } from "./hlc.wasm" with { type: "wasm", provider: "./highlight.wado" };
```

Rationale: [WEP: Wasm Module Import](./wep-2026-01-10-wasm-import.md),
[WEP: Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) and
[WEP: Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md).

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

Only the members visible at the import site are reachable through a namespace:
its `pub` items, and its `internal` items when it is in the same package.

A qualified head names the namespace's declaration even where the importing
module declares one of its own by that name.

The namespace belongs to the importing file alone. It names a module, not an
item, so it [cannot be re-exported](#re-exports-pub-use); re-export the members
by name instead.

## Import Rules

- Named imports use curly braces: `use {x, y} from "..."`
- Namespace imports omit curly braces: `use name from "..."`
- `use _ from "..."` loads a module and binds no name
- Wildcards prohibited: `use {*} from "..."` is not allowed
- No `use * as name` and no default imports
- All imports must be explicit (except the prelude)
- `Effect::{op1, op2}` imports an effect's operations

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

An effect operation is called as `Effect::op()`, or by its bare name once
imported. [Importing Effect Operations](./spec-effects.md#importing-effect-operations)
holds the rules.

- `.` reaches struct fields and methods (`user.name`, `stream.read()`).
- `::` reaches effect operations and namespace members
  (`Stdout::write_via_stream()`, `utils::helper()`).

## Renaming Imports

`as` binds an imported name under a local one, for an item and an effect
operation alike:

```wado
use {to_string as json_string} from "core:json";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";
```

## Re-exports (`pub use`)

A re-export makes an imported name a member of the importing module. A facade
uses this to publish a package's API under its entry module's own names, so
consumers never name the files behind it:

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

- `pub use` and `internal use` re-export at their modifier's reach, never
  further than the symbol they name (see [Re-export visibility](#re-export-visibility))
- A re-exported name is the item it names, not a copy: a re-exported type is the
  same type, and a re-exported effect the same effect
- Re-export chains are resolved transparently (A re-exports from B, B re-exports from C)
- Circular re-exports are prohibited
- Only named items can be re-exported. A namespace (`pub use utils from "..."`) and a wildcard (`pub use _ from "..."`) are compile errors.
- A re-export stays at module level, so `export use` is a compile error

Rationale: [WEP: Re-export Syntax (`pub use`)](./wep-2026-01-25-pub-use-reexport.md).

## Exception: The Prelude

The prelude is imported into every module automatically, so its names need no
`use`. [The Prelude](./spec-types.md#the-prelude) lists the types it provides.

The prelude's names are what `core:prelude` exports: its own `pub`
declarations and its `pub use` re-exports. A name that one of its
implementation modules declares without `core:prelude` re-exporting it is not a
prelude name and needs an import.

A module may not declare a type with a prelude type's name (`struct Option` is
an error). The builtin type names (`i32`, `bool`, ...) stay reserved under
`#![no_prelude]` too.

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
unreachable(); // traps with no message
```
