# WEP: A Component's Exports Are Named as the Component Names Them

## Context

A component names each of its imports and exports in one of two ways
([Explainer § Import and Export Definitions](../vendor/component-model/design/mvp/Explainer.md#import-and-export-definitions)).
A plain name is a label such as `twin`. An interface name is
`namespace:package/interface@version`, such as `a:x/trip`. The binary has no
package and no world of its own: a package is a prefix of interface names, and
the world is the component's type.

Component Model validation makes the labels in one scope strongly unique, so a
binding generator can turn each into an identifier without a collision. That
guarantee covers a label and a whole interface name. It does not cover the last
label of an interface name.

Importing a component gave each interface the identifier made from that last
label, and put every interface and every named type into one flat module. Two
interfaces of one name in different packages, or two types of one name in
different interfaces, then collided:

```text
./dup.wasm:1:1: error: duplicate definition 'Trip' (first defined at 1:1)
```

jco, which turns a component into an ES module, has the same defect. It
exports each interface under its full name and under a short alias, and the
short alias of `b:y/trip` is lost:

```js
export { trip, trip as 'a:x/trip', trip$1 as 'b:y/trip' }
```

The flat module also broke what the compiler relies on to identify a
declaration. [WEP: Declaration Identity](./wep-2026-08-12-declaration-identity.md)
§9 reaches a WIT name's declaration through its `(name, module)` pair, which
names one declaration only while a module declares each WIT name once.

## Decision

### A component file is an ES module

A directly imported `.wasm` component is one module, as the ES module
integration of WebAssembly reads one
([Explainer § ESM-integration](../vendor/component-model/design/mvp/Explainer.md#esm-integration)).
Its items are its exports, under the names the component gives them. A plain
name is an item of that name. An interface name is a namespace, named by the
interface name as a string, as an ES module names an export that is not an
identifier:

```wado
use { "a:x/trip" as Trip, "b:y/trip" as YTrip, twin } from "./dup.wasm" with { type: "wasm" };
```

A string is not an identifier, so a string-named import takes `as`.

### A name the component gives outranks one Wado derives

Each interface is also offered under its basename: the last label of its
interface name, without the version, in `PascalCase` (`a:x/trip@0.1.0` is
`Trip`), as it is today. These names share the module's one namespace with the
plain-named exports and the default interface's functions and types, in three
ranks:

1. A plain-named export. The component gave the name.
2. A function or a type of the default interface. A Wado library declares
   these at module level, so a `use` of the library reads the same from source
   and from a component.
3. An interface's basename. Wado derived the name.

Where two names of different ranks coincide, the higher rank keeps it, as a
world-level function already keeps its name over a default interface's
function. A library `acme:counter` that exports `struct Counter` keeps
`Counter` for the type, and its default interface is reached by its full name.
Where two names of one rank coincide, the name is an error at the `use` that
names what it could mean, and only a path through a full name reaches either.
A name never answers with the first item found, nor with an unspecified one.

The error is the rule a package's default interfaces already follow for their
functions
([WEP: A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)),
made general.

### A module generated from WIT takes full names too

Each interface a `wasi:*` module offers can also be imported by its full name:

```wado
use { "wasi:cli/stdout" as Stdout } from "wasi:cli";
```

The ranking does not reach them. A `wasi:*` module is Wado source, and the
interfaces it offers are the ones its `pub use` list chooses. It offers no type
by bare name, since each WASI type is declared inside its interface, as any
interface's type is (below). `wasi:sockets/types` and
`wasi:sockets/ip-name-lookup` each declare an `ErrorCode`, named
`Types::ErrorCode` and `IpNameLookup::ErrorCode`.

A world is not imported by its full name. A `.wasm` cannot offer one, since its
world's name is not observed (see the known gap below), and a world one module
could name and another could not would make the rule depend on where the
module came from.

### Each WIT scope is one Wado scope

The bindings synthesized for a component keep each interface in a scope of its
own, so a module declares each WIT name once, and the `(name, module)` step
answers with one declaration. A reference between interfaces is a reference
site the resolver answers, as WIT's own `use` is, so no synthesized type
carries only a spelling.

An operation is bound to its interface's declaration, not to the spelling
`Interface::operation`, so two same-named interfaces may each declare an
operation of one name. A program's own `Client::send` used to be refused
because `wasi:http` declares one too, though the program never imports it.

### An interface's types are named through it

A WIT type is declared inside its interface, so it is named through that
interface: `Geo::Point`. Two interfaces of one component may then each declare
a `Point`. Since each WIT scope is one Wado scope, a Wado `interface` declares
types as WIT's does:

```wado
#[cm("a:x/geo@0.1.0")]
pub interface Geo {
    struct Point { pub x: f64, pub y: f64 }
    enum Unit { Metric, Imperial }
    variant Shape { Circle(f64), Poly(List<Point>) }
    flags Axis { X, Y }
    type Meters = f64;
    resource Canvas {
        fn new() -> Canvas;
        fn draw(&self, shape: Shape);
    }

    fn distance(a: Point, b: Point, unit: Unit) -> Meters;
}
```

An interface body holds operations and the type declarations WIT has: `struct`,
`enum`, `variant`, `flags`, `resource`, and `type Name = T;`. It holds nothing
else, so no `impl`, `trait` or `global`. An interface has no associated types,
so `type Name = T;` in one declares a newtype. A type in an interface needs no
`#[cm]`: its CM name is the interface's name with the type's name in kebab case.

Several alternatives were weighed. Keeping every type at module level and
making a shared name an error would let a component break its consumers by
adding an interface. One module per interface, reached by a specifier such as
`"./geo.wasm#a:x/geo"`, contradicts reading a component file as one module.
Giving only a component's interfaces a type scope, with no way to write one in
source, would leave a WIT interface that Wado source cannot declare.

#### Inside and outside the interface

Inside the interface, its own types are named bare, in the signatures and the
default bodies of its operations alike. A name there is looked up in the
interface first, then in the module.

Outside, a path is the type's only name, in every position:

```wado
let p: Geo::Point = Geo::Point { x: 1.0, y: 2.0 };
let canvas = Geo::Canvas::new();
let r = match shape {
    Geo::Shape::Circle(r) => r,
    Geo::Shape::Poly(_) => 0.0,
};
let u: Geo::Unit = Metric;      // a case is bare where the expected type gives it
```

A namespace import adds its prefix, as `geo::Geo::Point`. A handler writes the
path too, since `Self` in it is the handler's own type:

```wado
impl Geo for Flat {
    fn distance(&self, a: Geo::Point, b: Geo::Point, unit: Geo::Unit) -> Geo::Meters { ... }
}
```

An `impl` of a type declared in an interface, such as
`impl Display for Geo::Point`, follows the same rules as an `impl` of any other
type. A type name and an operation name never coincide, since one is
`PascalCase` and the other `snake_case`, and WIT keeps a type and a function of
one interface apart too.

No form names the type `Point` outside the interface: not a `use` of the
interface's types, not a re-export, and not a type alias. Each of those was
weighed, as `pub use { Geo::{Point} };`, `pub Point = Geo::Point;` and
`pub Geo::Point as Point;`, and each adds language surface that a path already
covers. Source and component then read alike, since neither offers the short
name. The cost is the prefix every use outside the interface writes.

#### Visibility

A type in an interface takes no visibility modifier, and writing one is an
error. It reaches as far as its interface does. Its fields carry their own
visibility, as any struct's do. In an interface that carries `#[cm]`, every field
is `pub`, since a WIT record has no private field, and a field that is not `pub`
there is an error.

#### An exported interface declares its own types

A type an exported interface's signature names belongs to an interface: its
own, or one it reaches, such as the `Instant` of `wasi:clocks/system-clock`. A
signature that names a type no interface declares is an error, since nothing
says which WIT interface the type would belong to:

```wado
pub struct Point { x: f64, y: f64 }

#[cm("acme:geo/metrics@0.1.0")]
pub interface Metrics {
    fn distance(a: Point, b: Point) -> f64;   // error: `Point` is not declared in `Metrics`
}
```

Emitting such a type into the default interface, and reaching it from
`metrics` through a WIT `use`, was weighed. It would publish a type the author
never exported.

#### Where a WIT scope comes from

A component's bindings declare each non-default interface's types inside that
interface. A WIT `use` between two interfaces becomes a path, `Other::Point`,
that the resolver answers. A module generated from WIT is written the same way:
each `wasi:*` file holds one interface and declares its types inside it, so the
standard library names a WASI type as user code does, `SystemClock::Instant`.

A module-level type carries no `#[cm("…#name")]` that names it into an
interface. The `wasi:*` files use that form until the roadmap item below moves
their types into their interfaces. Keeping it for user code alone would leave
the standard library and user code writing one thing two ways. The `#[cm]` of an operation, a case or a flag is unchanged.

The module that declares an interface is then the declaring module of its
types, the one module per CM interface that
[§9 of Declaration Identity](./wep-2026-08-12-declaration-identity.md#one-declaring-module-per-cm-interface)
requires.

#### The default interface

The default interface is the exception, because it is its module
([WEP: A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)).
A Wado library declares its exported types at module level, so the default
interface's types are items of the component's module, as its functions are.

### The version may be left out

A full name may leave out its version where the module offers that interface at
one version: `"wasi:cli/stdout"` reaches `wasi:cli/stdout@0.3.0`. Where it
offers two, the version is required, and a name without one is an error naming
both.

### Each component's exports are its own

Two components may export one interface name, as two builds of one library do.
Each component's module declares its own interface for it, and a call through
either reaches the component it was imported from. The program imports each
under a name tagged with a digest of its component, as
`acme:geo-dependency{digest}/geo`, so the two never meet among the composed
component's imports, and composition wires each to its component's export. A
world function two components export is imported the same way, under a tagged
label.

### A component's imports are items of its module

A component's non-WASI import is an interface its consumer provides through a
`with` handler, so the consumer has to name it. The ES module integration
resolves an import as a module specifier and never offers it as an export.
Wado goes further: an imported interface is an item of the component's module,
named by the same rules as an export. Marl, built as a component, imports the
hook its consumer supplies and exports the renderer that calls it:

```wado
use { RenderHook, render_highlighted } from "./marl.wasm" with { type: "wasm" };

with RenderHook => &mut hook do {
    let html = render_highlighted(source);
}
```

Its full name is `"wado-lang:marl/render-hook" as RenderHook`.

An imported interface name has one declaration in the whole program, as
[§9 of Declaration Identity](./wep-2026-08-12-declaration-identity.md#one-declaring-module-per-cm-interface)
asks of every WIT name. Two components that import `x:y/log` name one Wado
interface, so one handler serves both. Their shapes of it must agree, and a
disagreement is an error naming both files. An exported interface is not shared
this way, since each component's export is its own declaration.

## Roadmap

- [x] A string-named import parses, and reaches an interface of a `.wasm` or a
  WIT-generated module by its full name, with or without its version.
- [x] A name items of different ranks share goes to the higher rank, and one
  two items of one rank share is an error naming what it could mean. The
  fixture from issue #2421 imports both `trip` interfaces by full name.
- [x] Each interface of a component is a scope of its own for its operations,
  and an operation is bound to its interface's declaration.
  `cm_interface_name_shared_with_stdlib.wado` passes.
- [x] Two components that export one interface name each declare their own,
  imported under a name tagged with the component, and composition wires each to
  its component's export.
  `cm_component_import_same_interface_two_components.wado` passes.
- [x] A component's interface types are named only through their interface,
  except the default interface's, which are items of the module. The bindings
  declare each as `Interface::Type`, a member of its interface, an imported
  interface's included, and a WIT `use` names the type of the interface it
  reaches. `cm_component_import_same_type_two_interfaces.wado`,
  `cm_component_import_interface_type_kinds.wado` and
  `cm_component_import_interface_type_namespace.wado` pass.
- [x] `core:icu` re-exports the interface `Properties` alone, and its callers
  write `Properties::BinaryProperty`.
- [ ] A Wado `interface` declares the WIT type kinds, named bare inside it and
  only through it outside. A visibility modifier on one, and a field that is not
  `pub` in a `#[cm]` interface, are errors.
- [ ] Each `wasi:*` type is declared inside its interface: `wado-from-idl` emits
  it there, the facades re-export only interfaces, and the standard library, the
  packages and the fixtures name each WASI type by its path. A module-level type
  carrying `#[cm("…#name")]` is then an error.
- [ ] An exported interface whose signature names a type no interface declares
  is an error.
- [ ] A component's imported interface is named as an export is, and two
  components importing one interface name one declaration, whose shapes must
  agree.

## Known gap: one interface imported and exported

A component may import an interface and export the same one, as a filter that
wraps whatever provides `x:y/filter` does. Its module would then offer one
interface name for two declarations: the import its consumer handles and the
export its consumer calls, so naming that interface in a `use` is an error.
What it admits is a component no Wado program can call through that interface.

Across two components the name is two declarations: one component exports
`x:y/log` as its own `Log`, and another imports `x:y/log`, which the program
declares once. Whether composition wires the importer to the exporter, or the
import waits for a handler the program installs, is not decided. What it admits
is a pair of components that meet only through a forwarding handler written by
hand.

## Known gap: when two imports of one interface agree

Two components that import `x:y/log` must agree on its shape, but agreement is
not defined. A component's imported instance type lists only the functions it
uses, so one may import `emit` alone and another `emit` and `flush`. Nor is it
decided whether `x:y/log@1.0.0` and `x:y/log@1.1.0` are one interface name or
two. What it admits is an ordinary pair of components the compiler may reject,
or accept as one interface when they are two.

## Known gap: where a shared import's types are declared

Two components that import `x:y/log` name one Wado interface, so its types,
such as a `record entry`, are declared once. Each component's module offers
that interface, and the program may declare `x:y/log` in source too. Which of
these is the one declaring module that
[§9 of Declaration Identity](./wep-2026-08-12-declaration-identity.md#one-declaring-module-per-cm-interface)
requires is not decided. What it admits is a handler for the shared interface
with no one path to name its types by: `first::Log::Entry` and
`second::Log::Entry` are one type under two spellings, or two types.

## Known gap: a component's world name is not observed

A component built from `world root` in package `a:x` decodes, through
`wit_component::decode`, to a world `root` in a package `root:component` the
decoder names. The name `a:x/root` is gone. What it admits is a world no
`.wasm` import can name, and a component whose plain-named exports cannot be
told apart by the world that declared them.

A component's type has no place for a world name. The only carrier is the
`component-type` custom section, so whether the name survives is up to the
producer. `wasm-tools component new` drops that section. Wado writes one, but
the world in it has lost its package (#2426). The loader does not read the
section either.

## References

- Issue #2421
- [Wasm CM Component Import (`use`-based)](./wep-2026-06-26-wasm-cm-component-import.md)
- [WEP: Declaration Identity](./wep-2026-08-12-declaration-identity.md)
- [A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)
- [WebAssembly ESM integration](https://github.com/WebAssembly/esm-integration/tree/main/proposals/esm-integration)
