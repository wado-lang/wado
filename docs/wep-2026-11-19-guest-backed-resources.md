# Every Resource Can Be Backed by a Guest Value

## Context

A handler answers a resource's operations in Wado. To do that, it has to make
handles of the resource and reach the state behind each one. Only an extern
handle could: `R::backed_by(value)` makes a handle holding a guest value, and
`handle.backing::<T>()` reaches it
([Extern Handles](./wep-2026-11-14-extern-handles.md#a-guest-value-backs-a-handle)).
The other two kinds of resource had no such way.

- An affine resource of the Component Model, such as `Descriptor` or
  `TcpSocket`, is a box holding the host's `i32` handle. A handler can answer
  `TcpSocket::create`, but it has no `TcpSocket` to return. A test double of a
  WASI resource cannot be written.
- A resource declared without `#[cm(...)]` has no host at all. No value of it
  can exist, so its instance operations can never be called. Calling one of its
  static operations crashed the compiler (#2533). The specification says only
  that such a resource is affine. Over 300 e2e fixtures declare one, to test
  move-only and effect rules without a WIT interface.

## Decision

Every resource handle is either the host's object or backed by a guest value.
`#[cm(...)]` says where the host implementation is, when there is one. A
resource declared without it is a resource with no host implementation, and
every handle of it is backed.

### Any resource can be backed

`R::backed_by(value)` and `handle.backing::<T>()` are offered on every resource,
affine or an extern handle. `Backing` is implemented for every `R: Resource`, so
no other marker names the resources that offer it.

An affine resource passes its handle to `backing` by reference, since it cannot
be copied.

### An operation no handler answers goes to the host, or traps

With a handler installed, the handler answers, whatever the handle is. With
none, an operation of a resource with a host implementation calls the host. An
operation of a resource without one traps, as an interface operation without a
default does.

### A backed handle never reaches the host

A backed handle is no object of the host's. Where one would be handed to a host
call, the guest traps at the call. An extern handle reaching a real host is
rejected by the host instead, as it is today.

### Drop reaches the handler first

A drop is an operation, so an installed handler answers it for any handle
([Resources as Effects](./spec-effects.md#resources-as-effects)). With no
handler, a host handle ends with the Component Model's `resource.drop`. A backed
handle does nothing more: the collector reclaims the value it holds.

### A default implementation needs a resource with no host

An operation may carry a body, its default implementation, only in a resource
without a host implementation. A default is what answers when no handler is
installed. Where there is a host, the host already answers then, so a body
there would be a second answer.

A default of `drop` takes the resource it releases, so its body does not drop
that resource again. Were it dropped there, the drop would dispatch to the
default once more and never end.

### A resource with no host is affine

A resource without `#[cm(...)]` is affine, as the specification already states.
It cannot be declared copyable, since `linearity` is a field of `#[cm(...)]`.
Being affine is what gives a drop to the handler. A copyable handle a guest
value backs is an extern handle, which already exists.

It takes no part in `extends` either. An upcast copies the handle, which an
affine one forbids, so `extends` already requires both sides to be extern
handles ([Resource Inheritance](./spec-components.md#resource-inheritance)).

### A resource with no host does not cross a component boundary

A resource without `#[cm(...)]` has no WIT name. An export whose signature
mentions one is a compile error.

### An affine resource is one box with two fields

An affine resource stays one box type, `$Resource`, which holds the host's
`i32` handle and a nullable reference to the backing value. A host handle holds
a null backing; a backed handle holds 0 and the value. A host call reads the
handle after testing the backing is null, which is where a backed handle traps.

Two subtypes, a host handle and a backed handle, were the first design. It
fails on a store through `&mut R`: the box keeps its identity, and a store
writes the new content into it, which may replace a host handle with a backed
one. A box's type cannot change, so both kinds share its fields.

A resource with no host implementation lowers to the same box, so where a
handle crosses from one place to another nothing tells the two apart. The
optimizer unboxes a handle to its `i32` only where no backed box reaches it.

## Roadmap

- [x] Offer `Backing` on every resource, bounded by `Resource`. Remove the
  `Backable` marker the prototype added.
- [x] Give `$Resource` a backing beside the handle. A host call traps on a
  backed handle. A resource with no host implementation lowers to the same box,
  which replaces the prototype's `$GuestResource`.
- [x] Route a drop of a backed handle to an installed handler, and do nothing
  without one.
- [x] Admit a default implementation on an operation of a resource with no host
  implementation, and keep rejecting it elsewhere.
- [x] Reject an export signature mentioning a resource with no host
  implementation.
- [x] State the rules in `spec-components.md` and `spec-effects.md`.
- [ ] Admit a default on `drop`. Its body ends without dropping the resource it
  takes, and with no handler installed the default runs on every drop, a
  scope exit's included. The rejection it meets today goes.
- [ ] Admit a default on an operation of a generic resource, made once per
  instance of the resource as the resource's operations are. The rejection it
  meets today goes.

## Related WEPs

- [Extern Handles](./wep-2026-11-14-extern-handles.md): `backed_by` and
  `backing` on an extern handle, which this extends to every resource.
- [Affine Types](./wep-2026-10-17-affine-types.md): the drop obligation a backed
  affine handle carries.
- [Effect Handler](./wep-2026-04-11-effect-handler.md): how a handler answers an
  operation.
