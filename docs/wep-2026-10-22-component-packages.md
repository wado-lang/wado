# WEP: A Component's Packages Are Modules, an Interface's Types Are Its Members

## Context

A component names everything it exports or imports by a Component Model
interface name, `namespace:package/interface`. One `.wasm` file holds one
world, and that world can reach interfaces of several packages. A component
built by souther-wasm-compiler exports `souther:program/<module>` and
`souther:decode/<module>`, and imports `souther:reached/<module>`.

Importing a component turned that structure into one flat Wado module. Each
interface became a Wado `interface` named after the interface alone, and each of
its named types became an item of that module named after the type alone. Two
interfaces of one name in different packages, or two types of one name in
different interfaces, then collided:

```text
./dup.wasm:1:1: error: duplicate definition 'Trip' (first defined at 1:1)
```

The flat module also broke what the compiler relies on to identify a
declaration. [WEP: Declaration Identity](./wep-2026-08-12-declaration-identity.md)
§9 reaches a WIT name's declaration through its `(name, module)` pair, which
names one declaration only while a module declares each WIT name once. The
synthesized module declared the types of every interface, so the pair named
two declarations whenever two interfaces shared a type name.

The standard library has the same defect, hidden. A generated `wasi:*` package
module re-exports the types of its interfaces flat, and leaves out a name two of
them share. `wasi:sockets/ip-name-lookup` declares an `error-code` that
`wasi:sockets` therefore does not offer.

## Decision

Wado's surface follows the Component Model's structure: a package is a module,
an interface is an item of that module, and a type is a member of its
interface. This is the
[Wasm in plain sight](./design-philosophy.md#wasm-in-plain-sight) idea applied
to imports. No name is ever left out for colliding, because the structure
admits no collision: WIT keeps an interface's name unique in its package, and a
type's name unique in its interface.

### A component's package is a module

A `use` of a component imports one of its packages. The `package` key of the
`with` clause names which one:

```wado
use { Trip } from "./dup.wasm" with { type: "wasm", package: "a:x" };
use { Trip as YTrip } from "./dup.wasm" with { type: "wasm", package: "b:y" };
use y from "./dup.wasm" with { type: "wasm", package: "b:y" };
```

The packages a component offers are those of the interfaces it exports and of
the non-WASI interfaces it imports. A WASI import is the host's, and resolves to
the standard library. A non-WASI import is an interface the consumer provides,
through a `with` handler.

The key may be left out where the component offers exactly one package, which
is the common case. Where it offers more, leaving it out is an error at the
`use` that names the packages offered. A version may be left out the same way:
`package: "a:x"` names `a:x@1.0.0` where the component offers no other version
of `a:x`.

### An interface's types are its members

A WIT type belongs to its interface, and a Wado type taken from one is a member
of that interface. It is named through the interface, by path or by `use`:

```wado
use { Types::{Request, Response} } from "wasi:http";
use { IpNameLookup } from "wasi:sockets";

fn handle(r: Request) -> Response { ... }
fn report(e: IpNameLookup::ErrorCode) { ... }
```

`use { Types::{Request} }` is WIT's `use wasi:http/types.{request}` spelled in
Wado, and the same item form that imports an interface's operations.

This holds for the standard library's `wasi:*` packages as it does for an
imported component. A package module no longer re-exports the types of its
interfaces.

The default interface is the exception, by
[WEP: A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md).
Its members, types and functions alike, are members of the package module. So a
library's types import the same way from source and from a component. The
exception follows from the interface's name, not from whether anything
collides.

### An interface's body declares its types

Wado source declares a type inside its interface's body, as WIT does:

```wado
pub interface Types {
    variant Method { Get, Post }
    resource Request { ... }

    fn send(request: Request) -> Method;
}
```

The body is a scope. An operation names a member type bare, and a type of
another interface by path. A member takes the interface's visibility and
writes none of its own, since WIT gives a member none. A member is one of WIT's
type kinds: a `struct`, `variant`, `enum`, `flags`, `resource` or newtype.

### Each WIT scope is one Wado scope

The bindings synthesized for a component keep each interface in a scope of its
own. A reference between them is a reference site the resolver answers, as
WIT's own `use` is, so no synthesized type carries only a spelling. A module
declares each WIT name once, and Declaration Identity's §9 `(name, module)`
step answers with one declaration again.

An operation is bound to its interface's declaration, not to the spelling
`Interface::operation`. This closes the
[known gap](./wep-2026-08-12-declaration-identity.md#known-gap-a-cm-operation-is-bound-by-its-spelling)
that rejected two same-named interfaces declaring one operation.

### A shared-name rule disappears

A package declares at most one default interface, so no two default interfaces
of one package share an operation. The rule leaving such a name out of the
module, and its diagnostic, have nothing left to cover.

## Roadmap

- [ ] A component import selects a package. The `package` key, the rule for
  leaving it out, and its diagnostic are in place, and each package's
  bindings form one module. The fixture from issue #2421 imports both
  `Trip`s.
- [ ] An interface's types are its members. An `interface` body declares
  types, `Iface::Type` and `use { Iface::{Type} }` resolve, a component's
  bindings declare each type in its interface's body, and two interfaces
  declaring one type name import together.
- [ ] The `wasi:*` standard library follows the rule. `wado-from-idl` stops
  flattening a package's types, and the corpus imports them through their
  interfaces.
- [ ] An operation is bound to its interface's declaration.
  `cm_interface_name_shared_with_stdlib_todo.wado` passes.

## References

- Issue #2421
- [Wasm CM Component Import (`use`-based)](./wep-2026-06-26-wasm-cm-component-import.md)
- [WEP: Declaration Identity](./wep-2026-08-12-declaration-identity.md)
- [A Package's Default Interface Is Its Module](./wep-2026-10-07-default-interface-as-module.md)
