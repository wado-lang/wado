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
`PascalCase`, as it is today. Where the short names of two exports coincide,
the short name is an error at the `use` that names their full names. Only the
renamed import reaches either. A short name never answers with the first
export, nor with an unspecified one.

This is the rule a package's default interfaces already follow for their
functions
([WEP: A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)),
made general.

### A module generated from WIT takes full names too

The `wasi:*` modules keep their shape. Each interface and each world one of
them offers can also be imported by its full name:

```wado
use { "wasi:cli/stdout" as Stdout, "wasi:cli/command" as Command } from "wasi:cli";
```

A world has no name in a component binary: a component decodes to a world its
decoder names. So a `.wasm` offers no world by name, and its plain-named
exports are the world's own.

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

## Roadmap

- [ ] A string-named import parses, and reaches an interface of a `.wasm` or a
  WIT-generated module by its full name.
- [ ] A short name two exports share is an error naming their full names. The
  fixture from issue #2421 imports both `trip` interfaces by full name.
- [ ] Each interface of a component is a scope of its own, and an operation is
  bound to its interface's declaration.
  `cm_interface_name_shared_with_stdlib_todo.wado` passes.

## Open question: where an interface's types are named

Two interfaces of one component may each declare a type of one name. The ES
module integration exports a type inside its instance, so the type would be
named through its interface (`One::Point`). Wado names a type only at module
level today, and the `wasi:*` modules offer their types flat.

## Open question: a component's imports

A component's non-WASI import is an interface its consumer provides, through a
`with` handler. The consumer names it today as an item of the `.wasm` module.
Under the ES module integration an import is not an export of the module: it is
resolved as a module specifier.

## Open question: the version in a full name

A component spells an interface name with its version (`wasi:cli/stdout@0.3.0`).
Whether a full-name import must write the version, or may leave it out where
one version is offered, is not decided.

## References

- Issue #2421
- [Wasm CM Component Import (`use`-based)](./wep-2026-06-26-wasm-cm-component-import.md)
- [WEP: Declaration Identity](./wep-2026-08-12-declaration-identity.md)
- [A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)
- [WebAssembly ESM integration](https://github.com/WebAssembly/esm-integration/tree/main/proposals/esm-integration)
