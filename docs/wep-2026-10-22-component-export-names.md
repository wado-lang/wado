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

### A short name collides into an error

Each interface is also offered under its short name, its last label in
`PascalCase`, as it is today. These names, the plain-named exports, and the
default interface's functions and types share the module's one namespace.
Where two of them coincide, the name is an error at the `use` that names what
it could mean. Only a path through a full name reaches either. A short name
never answers with the first item, nor with an unspecified one.

This is the rule a package's default interfaces already follow for their
functions
([WEP: A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)),
made general.

### A module generated from WIT takes full names too

The `wasi:*` modules keep their shape. Each interface one of them offers can
also be imported by its full name:

```wado
use { "wasi:cli/stdout" as Stdout } from "wasi:cli";
```

The short-name rule does not reach them. A `wasi:*` module is Wado source, and
its module-level names are the ones its `pub use` list chooses: `wasi:sockets`
offers the `ErrorCode` of `wasi:sockets/types`, though
`wasi:sockets/ip-name-lookup` declares one too.

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
`Interface::operation`. This closes the
[known gap](./wep-2026-08-12-declaration-identity.md#known-gap-a-cm-operation-is-bound-by-its-spelling)
that rejected two same-named interfaces declaring one operation.

### An interface's types are named through it

A type an interface declares is named through that interface, as the ES module
integration exports a type inside its instance: `Geo::Point`. Two interfaces of
one component may then each declare a `Point`. Since each WIT scope is one Wado
scope, a Wado `interface` declares types as WIT's does:

```wado
interface Geo {
    struct Point { x: f64, y: f64 }
    fn distance(a: Point, b: Point) -> f64;
}
```

`Geo::Point` is the type's only name. No form names it `Point`: not a `use`
of the interface's types, not a re-export, and not a type alias. Each of those
was weighed, as `pub use { Geo::{Point} };`, `pub Point = Geo::Point;` and
`pub Geo::Point as Point;`, and each adds language surface that a path already
covers. Source and component then read alike, since neither offers the short
name.

The default interface is the exception, because it is its module
([WEP: A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)).
A Wado library declares its exported types at module level, so the default
interface's types are items of the component's module, as its functions are.

A module that declares types in an interface makes that module their declaring
module, as
[§9 of Declaration Identity](./wep-2026-08-12-declaration-identity.md#one-declaring-module-per-cm-interface)
requires of every module declaring a `#[cm]` type.

### The version may be left out

A full name may leave out its version where the module offers that interface at
one version: `"wasi:cli/stdout"` reaches `wasi:cli/stdout@0.3.0`. Where it
offers two, the version is required, and a name without one is an error naming
both.

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

An interface name has one declaration in the whole program, as
[§9 of Declaration Identity](./wep-2026-08-12-declaration-identity.md#one-declaring-module-per-cm-interface)
asks of every WIT name. Two components that import `x:y/log` name one Wado
interface, so one handler serves both. Their shapes of it must agree, and a
disagreement is an error naming both files.

## Roadmap

- [ ] A string-named import parses, and reaches an interface of a `.wasm` or a
  WIT-generated module by its full name, with or without its version.
- [ ] A module-level name two items of a component share is an error naming
  what it could mean. The fixture from issue #2421 imports both `trip`
  interfaces by full name.
- [ ] Each interface of a component is a scope of its own, and an operation is
  bound to its interface's declaration.
  `cm_interface_name_shared_with_stdlib_todo.wado` passes.
- [ ] A Wado `interface` declares types, named only through it.
- [ ] A component's interface types are named only through their interface,
  except the default interface's, which are items of the module.
- [ ] A component's imported interface is named as an export is, and two
  components importing one interface name one declaration, whose shapes must
  agree.

## Known gap: one interface imported and exported

A component may import an interface and export the same one, as a filter that
wraps whatever provides `x:y/filter` does. Its module would then offer one
interface name for two declarations: the import its consumer handles and the
export its consumer calls. Naming that interface in a `use`, by either name, is
an error. What it admits is a component no Wado program can call through that
interface.

Across two components the same name meets the one-declaration rule: one
component exports `x:y/log` and another imports it. Whether `Log::emit()` calls
the first or dispatches to a handler for the second, and which module declares
the one `Log`, is not decided.

## Known gap: the visibility of an interface's types

Whether a type declared in an interface carries a visibility of its own, and
whether its fields must be `pub` as a WIT record's are, is not decided.

## Known gap: a newtype in an interface

An interface writes its members as a trait does, so `type Meters = f64;` inside
one reads as an associated type. A WIT type alias in a component's interface
is synthesized as a newtype named through it, but no Wado spelling declares
one.

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
