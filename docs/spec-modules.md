# Module System

A module is one Wado file. This chapter covers how far a declaration reaches,
how a module names another, and how it imports and re-exports names. The import
syntax follows ES modules (`use { x } from "module"`), since JavaScript is a
primary host for Wado.

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

<!-- {"fixture":"spec_modules_visibility.wado"} -->

```wado
// Private to this file (default)
fn helper() -> i32 { return 1; }

// Package-internal - accessible from other files in this package
internal fn build_ast() -> List<String> { return ["doc"]; }

// Library API - accessible from other Wado packages (Wado-native)
pub fn map<T, U>(f: fn(T) -> U, xs: List<T>) -> List<U> { return xs.into_iter().map(f).collect(); }

// Library API + CM boundary export
export fn run() { }

test {
    assert helper() == 1 && build_ast().len() == 1;
    assert map(|x: i32| x * 2, [1, 2]) == [2, 4];
}
```

| Declaration         | Same file | Same package | Other Wado packages | CM boundary |
| ------------------- | --------- | ------------ | ------------------- | ----------- |
| `fn foo()`          | Yes       | No           | No                  | No          |
| `internal fn foo()` | Yes       | Yes          | No                  | No          |
| `pub fn foo()`      | Yes       | Yes          | Yes                 | No          |
| `export fn foo()`   | Yes       | Yes          | Yes                 | Yes         |

A `pub`-only item reaches a Wado consumer of the package's source. It does not
reach a consumer of a registry package, which is a prebuilt component, and
whether it will is undecided
([Registries](./spec-packages.md#registries)). A non-Wado CM consumer sees
`export` items only.

`pub` is absolute. A module has no privacy of its own beyond its file, so there
is no `pub(crate)` / `pub(super)` family, and no enclosing module can narrow a
`pub` item.

The ladder applies to top-level items, struct fields, and `impl` members
(methods, associated constants); reaching one beyond its rung is a compile
error. `export` on a member is an error, because a method has no CM boundary. Only an
_inherent_ member has a ladder; a trait impl's members reach as far as the
trait.

<!-- {"fixture":"spec_modules_visibility.wado"} -->

```wado
impl Config {
    fn parse_raw() -> i32 { return 1; }         // this file only
    internal fn reload() -> i32 { return 2; }   // other files in this package
    pub fn get() -> i32 { return 3; }           // other packages
}

test {
    assert Config::parse_raw() + Config::reload() + Config::get() == 6;
}
```

`internal` reaches the files of one package, as
[The Modules of a Package](./spec-packages.md#the-modules-of-a-package) lists
them.

### Signature Reach

An item's signature may not name a declaration that reaches less far than the
item itself. Naming one is a compile error at the reference. A caller that
reaches the item has to be able to write the types it names, and a `pub fn`
returning a file-private struct hands back a value whose type no caller can
write.

<!-- {"fixture":"spec_modules_signature_reach.wado"} -->

```wado
struct Hidden { n: i32 }

pub fn make() -> Hidden { return Hidden { n: 0 }; }   // ERROR: widen `Hidden`, or narrow `make`
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

### Re-export Visibility

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

| Source Type                                            | Syntax                        | Example                              |
| ------------------------------------------------------ | ----------------------------- | ------------------------------------ |
| WASI standard                                          | `"wasi:<package>"`            | `"wasi:cli"`, `"wasi:filesystem"`    |
| Core library                                           | `"core:<module>"`             | `"core:cli"`, `"core:json"`          |
| [CM coordinate](./spec-packages.md#package-specifiers) | `"<ns>:<pkg>[@<ver>]"`        | `"docs:regex"`, `"docs:regex@1.0.0"` |
| [Library alias](./spec-packages.md#package-specifiers) | `"lib:<nick>"`                | `"lib:router"`, `"lib:shared"`       |
| Local file                                             | `"./<path>"` or `"../<path>"` | `"./utils.wado"`, `"../config.wado"` |

A specifier names a package or a local file. It never carries an interface segment: interfaces and their members
are selected in the `use { ... }` list (`Iface`, `Iface::{op}`). `core:` and
`wasi:` are coordinates whose namespace is bundled with the compiler, not a
separate scheme. Nested namespaces (`a:b:pkg`) follow WIT.

## Module Path Validation

Relative paths in Wado follow the gitignore / shell convention: a path that refers to a file relative to the current file must begin with `./` (next to me) or `../` (up one). A bare path (`foo/bar`, `utils.wado`) never refers to a file next to the current one. It is read as a namespace or coordinate, or handed to the host, and it is rejected wherever only a relative file path is valid. This rule holds for every path literal: module imports (`use ... from`), `#include_str` / `#include_bytes`, and Kiln input paths (`from`, `generator.inputs`, `generator.output_dir`).

A module path resolves by its form:

1. A bundled namespace, `core:` or `wasi:`, resolves to the standard library the compiler carries.
2. An open coordinate `<ns>:<pkg>`, in any other namespace, resolves to a dependency ([Package Specifiers](./spec-packages.md#package-specifiers)).
3. A library alias `lib:<nick>` resolves to a dependency ([Package Specifiers](./spec-packages.md#package-specifiers)).
4. A local path, starting `./` or `../`, resolves relative to the importing module.
5. Any other path is an error: `invalid module path 'xxx'; use './' for local modules or 'namespace:' for library modules`. A bare name (`"router"`) is one, except a deprecated bare `[dependencies]` key ([Package Specifiers](./spec-packages.md#package-specifiers)).

The reserved namespaces are `core`, `wasi`, and `lib`. `core` and `wasi` are
bundled; `lib` is not. Every other namespace is open.

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

<!-- {"fixture":"spec_modules_import_syntax.wado"} -->

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
use {Helper} from "./sub/spec_modules_utils.wado";
use {Config} from "../fixtures/sub/spec_modules_config.wado";

// 4. CM coordinate (declared in wado.toml, or given an inline `with` source)
use {Regexp} from "docs:regex";

// 5. Library alias (rename / private / coordinate-less dependency)
use {Router} from "lib:router";

test {
    assert to_string(&Config { port: 80 }) == Result::Ok("{\"port\":80}");
    assert Helper { name: "h" }.name == "h";
    assert Regexp { pattern: "a+" }.pattern == "a+" && Router { routes: 2 }.routes == 2;
}
```

Implementing a trait requires naming it: `impl Trait for Type` and the bodiless
derive form `impl Trait for Type;` both need `Trait` in scope, whether declared
in the module, imported, or auto-imported from the prelude.

<!-- {"fixture":"spec_modules_impl_needs_trait.wado"} -->

```wado
use {Deserialize} from "core:serde";
use {from_string} from "core:json";

struct Config { port: i32 }

impl Deserialize for Config;          // OK

test {
    assert from_string::<Config>("{\"port\":80}").unwrap().port == 80;
}
```

<!-- {"fixture":"spec_modules_impl_trait_not_imported.wado"} -->

```wado
impl Deserialize for Config;          // error without the import
```

An import's local name must not collide with a declaration in the importing
module. The name would mean two declarations at once and nothing could say
which, so the program is rejected; an alias says which one was meant.

<!-- {"fixture":"spec_modules_import_collision.wado"} -->

```wado
use {Widget} from "./sub/spec_modules_other.wado";
pub struct Widget { mine: bool }               // error: collides with the import
```

<!-- {"fixture":"spec_modules_import_alias.wado"} -->

```wado
use {Widget as Theirs} from "./sub/spec_modules_other.wado";
pub struct Widget { mine: bool }               // OK

test {
    assert Widget { mine: true }.mine && Theirs { theirs: true }.theirs;
}
```

## Import Attributes (`with`)

A `with { ... }` clause after the specifier gives an import its attributes.
`type` says what kind of file the import reads:

<!-- {"fixture":"spec_modules_type_attribute.wado"} -->

```wado
// `type` alone reads the file as that type
use {add_one, twice} from "./sub/wasm_import_user.wasm" with { type: "wasm" };

test {
    assert add_one(1) == 2 && twice(2.0) == 4.0;
}
```

The other keys have sections of their own. The dependency source keys give a
dependency its source with no `wado.toml`
([Inline Sources](./spec-packages.md#inline-sources)). `generator` makes the
import a [generated import](./spec-kiln.md), and `provider` satisfies a
component's guest effect ([Wasm Module and Component Imports](#wasm-module-and-component-imports)).

Any other key is an error, and so is a value of the wrong kind. Every key but
`generator` takes a string:

<!-- {"fixture":"import_attr_unknown_key_error.wado"} -->

```wado
use { println, Stdout } from "core:cli" with { tpye: "wasm", provider: 1 };
```

### How an Import Is Read

`type` is `"wasm"` or `"wat"`. A `use` never looks at its path's extension. Its
attributes decide how the file is read:

1. With `generator`, a [Kiln generator](./spec-kiln.md) reads it. A `type`
   beside `generator` is passed to the generator.
2. With `type` alone, the file is read as that type: `"wasm"` as a Wasm binary,
   `"wat"` as Wasm text.
3. Otherwise it is read as Wado source.

> Not yet implemented: the loader still decides by extension, and a generator
> does not receive `type`. See
> [WEP: Kiln](./wep-2026-04-12-kiln.md#known-gaps).

## Wasm Module and Component Imports

A Wasm asset is imported directly with `with { type: "wasm" | "wat" }`. Whether the file is a core module or a Component Model component is detected from its content, not declared, and either may be written as `.wasm` or `.wat`. A single `use` may pull several names (functions from a core module, interfaces from a component). The path is a `./` or `../` path.

| Imported file | Exposes as                                     | Call style                            |
| ------------- | ---------------------------------------------- | ------------------------------------- |
| Core module   | One free `pub fn` per function export          | `helper(x)` — plain function          |
| CM component  | One Wado `interface` per exported CM interface | `Iface::method(x)` — called like WASI |

<!-- {"fixture":"spec_modules_wasm_imports.wado"} -->

```wado
// Core wasm / wat — exports become free functions.
use { add_one } from "./sub/wasm_import_user.wat" with { type: "wat" };
use { twice }   from "./sub/wasm_import_user.wasm" with { type: "wasm" };

// CM component — each exported interface becomes a Wado `interface`,
// and its functions are called like WASI methods.
use { CmCatalog } from "./sub/cm-catalog.wasm" with { type: "wasm" };

test {
    assert add_one(41) == 42 && twice(1.5) == 3.0;
    assert CmCatalog::id_string("round trip") == "round trip";
}
```

### Core Modules

Each function export becomes a free function of the same name. A call to it
calls that export, whatever the name: an export named like a `core:builtin`
intrinsic or like another asset's export is still its own asset's function. An
export spelled like a Wado keyword is imported under an alias
(`use { resume as seven } from ...`).

An asset is embedded in the output and shares the component's memory. The
memory's first pages, as many as the asset's own memory declares at minimum,
are the asset's: they hold its data and, for a module a toolchain such as
Rust's built, its stack. The allocator's heap starts past them. An asset is
rejected at the import when it:

- is not a valid module;
- imports anything but `env.memory`;
- has more than one memory, counting the imported one, or a memory that is
  64-bit, shared, has a custom page size, or has a minimum of 2 GiB or more;
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
on a component that imports no guest effect is an error. So is one that does not
export every operation of the interface with the signature the component
imports. An operation's default body does not fill a gap here, because the
interface the component imports carries none.

<!-- {"fixture":"spec_modules_provider.wado"} -->

```wado
use { Hlc } from "./sub/hlc.wasm" with { type: "wasm", provider: "./sub/hl_ext.wado" };

test {
    assert Hlc::wrap("x", "wado").text == "[wado:x]";
}
```

Rationale: [WEP: Wasm Module Import](./wep-2026-01-10-wasm-import.md),
[WEP: Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) and
[WEP: Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md).

## Namespace Import

`use name from "..."`, without braces, imports a whole module as a namespace:

<!-- {"fixture":"spec_modules_namespace_import.wado"} -->

```wado
// Import a module as a namespace
use utils from "./sub/spec_modules_utils.wado";

test {
    assert utils::helper_function() == 42;   // not utils["helper_function"], as it's analyzed at compile time
}
```

A namespace import binds one name, the namespace. The source module's pub symbols are reached through the `ns::` prefix and are not imported under their bare names, so `distance(p1, p2)` below is an unknown function:

<!-- {"fixture":"spec_modules_namespace_members.wado"} -->

```wado
use geo from "./sub/spec_modules_geo.wado";

trait Show {
    fn show(&self) -> String;
}

struct Local {}

test {
    let [p1, p2] = [geo::Point { x: 0.0, y: 0.0 }, geo::Point { x: 3.0, y: 4.0 }];

    // Functions
    assert geo::distance(p1, p2) == 5.0;

    // Types (structs, enums, variants)
    let p: geo::Point = geo::Point::origin();
    let c = geo::Color::Red;
    let s = geo::Shape::Circle(3.14);
    assert p.x == 0.0 && c == geo::Color::Red && s matches { Circle(_) };
}

// Traits and types in an `impl` header, on either side
impl geo::Show for Local { fn show(&self) -> String { return "local"; } }
impl Show for geo::Tag { fn show(&self) -> String { return self.name; } }

test {
    assert Local {}.show() == "local" && geo::Tag { name: "tag" }.show() == "tag";
}
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
- `Effect::{op1, op2}` imports an effect's operations ([Importing Effect Operations](./spec-effects.md#importing-effect-operations))

<!-- {"fixture":"spec_modules_import_rules.wado"} -->

```wado
// Valid patterns
use {println, eprintln} from "core:cli";        // Named import
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
use utils from "./sub/spec_modules_utils.wado";  // Namespace import

test {
    assert utils::helper_function() == 42;
}
```

A wildcard is prohibited, with or without braces:

<!-- {"fixture":"spec_modules_import_wildcard.wado"} -->

```wado
use * from "core:cli";           // Wildcard not allowed
```

<!-- {"fixture":"spec_modules_import_wildcard_braces.wado"} -->

```wado
use {*} from "core:cli";         // Wildcard not allowed
```

Without braces, `use println from "core:cli"` is a namespace import named `println`, so the function is `println::println`.

## Renaming Imports

`as` binds an imported name under a local one, for an item and an effect
operation alike:

<!-- {"fixture":"spec_modules_import_rename.wado"} -->

```wado
use {to_string as json_string} from "core:json";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

test {
    assert json_string(&42) == Result::Ok("42");
}
```

## Re-exports (`pub use`)

A re-export makes an imported name a member of the importing module. A facade
uses this to publish a package's API under its entry module's own names, so
consumers never name the files behind it:

<!-- {"fixture":"sub/spec_modules_trig.wado"} -->

```wado
// math/internal/trig.wado
pub fn sin(x: f64) -> f64 { return f64::sin(x); }
pub fn cos(x: f64) -> f64 { return f64::cos(x); }

test {
    assert sin(0.0) == 0.0 && cos(0.0) == 1.0;
}
```

<!-- {"fixture":"sub/spec_modules_math.wado"} -->

```wado
// math/mod.wado - re-export from internal modules
pub use {sin, cos} from "./spec_modules_trig.wado";
pub use {sin as sine} from "./spec_modules_trig.wado";  // with rename

test {
    assert sine(0.5) == sin(0.5);
}
```

<!-- {"fixture":"spec_modules_reexport.wado"} -->

```wado
// user code - import from the facade (a "math" dependency declared in wado.toml)
use {sin, cos, sine} from "lib:math";

test {
    assert sine(1.0) == sin(1.0) && cos(0.0) == 1.0;
}
```

Re-export rules:

- `pub use` and `internal use` re-export at their modifier's reach, never
  further than the symbol they name (see [Re-export visibility](#re-export-visibility))
- A re-exported name is the item it names, not a copy: a re-exported type is the
  same type, and a re-exported effect the same effect
- Re-export chains are resolved transparently (A re-exports from B, B re-exports from C)
- Circular re-exports are prohibited
- Only named items can be re-exported. A namespace (`pub use utils from "..."`) and a nameless import (`pub use _ from "..."`) are compile errors.
- A re-export stays at module level, so `export use` is a compile error

Rationale: [WEP: Re-export Syntax (`pub use`)](./wep-2026-01-25-pub-use-reexport.md).

## The Prelude

The prelude (`core:prelude`) is imported into every module automatically, so
its names need no `use`. It and the [`builtin` namespace](#the-builtin-namespace)
are the two exceptions to explicit imports.
[Prelude Types](./spec-types.md#prelude-types) lists the types it provides, and
[`#![no_prelude]`](./spec-attributes.md#no_prelude) turns the import off for one
module.

The prelude's names are what `core:prelude` exports: its own `pub`
declarations and its `pub use` re-exports. A name that one of its
implementation modules declares without `core:prelude` re-exporting it is not a
prelude name and needs an import.

A module may not declare a type with a prelude type's name (`struct Option` is
an error). The builtin type names (`i32`, `bool`, ...) stay reserved under
`#![no_prelude]` too.

## The `builtin` Namespace

`builtin::name` names the declaration `name` in `core:builtin`, the module of
the compiler's intrinsics, in every module and with no `use`:
`builtin::black_box` ([Optimization Barrier](./spec-control-flow.md#optimization-barrier))
and `builtin::cold_path` ([Branch Hints](./spec-control-flow.md#branch-hints))
among them. The
prefix reaches no more than an import would: a declaration of `core:builtin`
that is not visible to the calling module is an error to name.
