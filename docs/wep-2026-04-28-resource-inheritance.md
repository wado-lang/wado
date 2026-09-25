# WEP: Resource Inheritance and Narrowing (`resource extends`)

## Context

Wado's `resource` declares an opaque handle to a host-managed object. Today every `resource` is flat: it has methods, but no relation to any other resource type. This matches the Component Model (CM), whose canonical ABI defines resources as flat — there is no inheritance, no subtyping between resource types.

### What we want to solve

WebIDL bindings. Browser APIs are defined as deep single-inheritance hierarchies (`EventTarget → Node → Element → HTMLElement → HTMLInputElement`). A single JS object simultaneously satisfies every type in its prototype chain. Without language-level inheritance, the binding generator has to either (a) duplicate every parent method on every child, (b) force the user to cast at every level (the wasm-bindgen `dyn_into` pattern), or (c) lose type information entirely.

WIT does not have inheritance and is not a target for this feature. Pure-Wado user code that happens to fit the model gets it for free, but is not the design driver.

### Why this is hard

CM has no inheritance. Whatever Wado decides at the language level has to be lowered to a flat model at the CM boundary. The choice of linearity cascades into every other design decision — upcast cost, narrowing mechanism, method dispatch, ABI shape, host contract — so this WEP starts there.

A CM `resource` is reached through `own` and `borrow`, the Component Model's only handle types: `own` is "a unique, opaque address of a resource that will be destroyed when this value is dropped", `borrow` "must be dropped before the current export call returns", and a `borrow` may not appear in a result type at all. Those obligations are what makes a WIT-derived resource affine in Wado ([Resource Ownership](./wep-2026-05-21-resource-ownership.md)), and they are what a handle hierarchy cannot live with: an upcast would duplicate an owning handle, and a narrowing would either consume its subject or oblige the caller to drop a second one.

Wasm GC's `externref` is the representation that fits — freely copied, reclaimed by the collector, never dropped by hand. It is out of reach across a CM boundary. The Component Model has no reference value type, `own` / `borrow` are the only handle types, and a resource's representation is validated to `i32` or `i64`. CM-GC does not open that door either: it swaps how `own` and `borrow` are represented, not what they mean, and the uniqueness and dropping conditions stay enforced at runtime.

## Decision

### Guiding principle

Every design choice in this WEP picks the **strictest sound default** and leaves room to relax based on real-world usage. Loosening a rule later (accepting a previously rejected program) is a non-breaking change; tightening would break existing code, so the strict end is the only safe starting point. This applies uniformly to subtyping variance, method resolution, narrowing targets, trait/inherent collisions, and any other point of friction. Individual rules do not restate the principle.

### Linearity: two disciplines, pilot the unrestricted one through Tide

Wado will support **two resource linearities** for the foreseeable future:

| Linearity      | Discipline                                    | Used by                            |
| -------------- | --------------------------------------------- | ---------------------------------- |
| `affine`       | move-only, with a drop obligation             | WIT-derived resources (WASI, etc.) |
| `unrestricted` | copyable, owning nothing and dropping nothing | WebIDL-derived resources (Tide)    |

[Resource Ownership](./wep-2026-05-21-resource-ownership.md) already settles
which one a resource gets: the `dtor` decides the kind, not the representation.
This WEP gives that decision a surface spelling.

The representation then follows. An affine resource crosses as a CM `own` /
`borrow` handle. An unrestricted one owns nothing for the CM to track, so it
crosses as a number the host interprets, an integer-valued `f64`. It is the
_extern handle_ the Lowering section names throughout.

An operation of an unrestricted resource is therefore a plain CM function, and
its handle an ordinary parameter. The `[constructor]T` / `[method]T.m` /
`[static]T.m` spellings name operations of a CM `resource`, which an
unrestricted one does not declare, so a `#[cm(...)]` binding that writes one is
rejected.

Reasoning:

- Affinity is not Wado's invention for WIT-derived resources: `own` and `borrow` carry the CM's own drop obligation, and we cannot drop the discipline without breaking interop with `wasmtime` and the broader CM ecosystem.
- An unrestricted resource is not a CM `resource`. It is an ordinary CM value — one opaque `f64`, copied like any other — which is what buys the value semantics a handle hierarchy needs, at the cost of the CM knowing nothing about its lifetime.
- Browser bindings have a property no other binding has: **we own both sides of the boundary** — the Wado component and the JS host glue (`jco`-style transpilation, generated by us). There is no third-party host runtime to coordinate with for browser objects, so we are free to hand out an index the glue interprets.

This makes Tide the right pilot for unrestricted resources. The blast radius is contained: if the discipline proves wrong, WIT/WASI is untouched.

### `resource extends` is gated on `unrestricted` (v1)

In v1, `resource X extends Y { ... }` is permitted **only when `X` and `Y` are both unrestricted**.

The reason is mechanical, not philosophical: an upcast hands out a second name for one handle, which is a copy. An unrestricted handle may be copied, so `HTMLInputElement` and the same value typed as `Element` are the identical wasm value, differing only in Wado's static type witness; upcast is a no-op and narrowing reads the class the host tagged that one value with. An affine handle may not be copied at all — with CM `resource` handles the two are distinct entries in distinct tables, so every cast becomes a host call that mints a handle the caller then owes a drop on.

A future WEP can extend `extends` to affine resources if we find a lowering we are happy with (e.g., shared handle tables across an `extends` family). This WEP does not preclude that.

### How linearity is chosen for a given resource

Linearity is **declared on the resource** and **structurally verified** by the compiler — never inferred from the namespace. The field is optional and reads as `"affine"` when omitted; `extends` is what requires `"unrestricted"`, on both sides.

Concretely:

- `#[cm(...)]` on a `resource` takes a `linearity=...` field, `"affine"` or `"unrestricted"`. Making it mandatory means migrating every stdlib resource, which no consumer needs, so the affine majority writes nothing.
  ```wado
  #[cm("web:dom/element", linearity = "unrestricted")]
  pub resource Element { ... }

  #[cm("wasi:http/types@0.3.0#request")]
  pub resource Request { ... }
  ```
- A resource without `#[cm(...)]` is affine and **cannot use `extends`** in v1. Hierarchies require explicit CM identity.
- `resource X extends Y { ... }` is a compile error unless **both** `X` and `Y` declare `linearity = "unrestricted"`. A mismatch inside an `extends` family is a hard error, not a warning.
- A resource declared `"unrestricted"` without any `extends` relationship is allowed (an opt-in to value semantics on its own). This is the spelling the non-owning tokens of
  [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — `Waitable`, `core:icu`'s interned handles — have never had: their value semantics is inferred today from the absence of a `dtor`, and nothing in the declaration says so.

### Why the field names linearity, not representation

Linearity is what the compiler enforces. Whether a handle may be copied decides
the move check, the cleanup pass, and whether `extends` is possible at all.
`i32` versus `externref` decides none of those, and
[Resource Ownership](./wep-2026-05-21-resource-ownership.md) declares the two
axes orthogonal.

Naming the enforced axis also covers more ground. A representation can only be
stated for a resource that has one to name, but `Waitable` and `core:icu`'s
interned handles are copyable for the same reason Tide's handles are, with a
different backing. One field says so for all three.

### Why mandatory + structural over namespace inference

Considered alternatives:

1. **Infer from namespace** (`web:*` → unrestricted, `wasi:*` → affine).
   - Pros: zero boilerplate, generators always get the right form.
   - Cons: hidden rule baked into the compiler, hard to extend to new namespaces (`node:*`, vendor-specific), silent surprise if a user picks a `web:*` URL by accident.
2. **Mandatory explicit attribute, no validation.**
   - Pros: every declaration is grep-able; no magic.
   - Cons: a generator that forgets the attribute or picks the wrong value silently mismatches the host glue at runtime.
3. **Mandatory explicit attribute + structural validation (chosen).**
   - Pros: every declaration is self-describing; cross-declaration consistency is enforced by the compiler, so "broken state compiles" cannot happen — `extends` mismatches are rejected, and the declared linearity is the single source of truth.
   - Cons: more boilerplate per declaration. In practice, ~all `#[cm(...)]`-bearing resources are emitted by `wado-from-idl`, so the cost falls on one tool, not on humans.

### Syntax

```wado
pub resource Child extends Parent { ... }
```

The `extends Parent` clause slots between the resource name (and any generic parameter list) and the body. It is optional; resources without `extends` behave as today.

```wado
#[cm("web:dom/event-target", linearity = "unrestricted", classes = "0..=2")]
pub resource EventTarget { ... }

#[cm("web:dom/node", linearity = "unrestricted", classes = "1..=2")]
pub resource Node extends EventTarget { ... }

#[cm("web:dom/element", linearity = "unrestricted", classes = "2..=2")]
pub resource Element extends Node { ... }
```

`classes` numbers the tree for narrowing; §"Host runtime contract" says what the
numbers mean and which ones the compiler accepts.

Rules:

- `extends` is a keyword only between a resource name and its parent; elsewhere it stays an identifier.
- Only one parent type is permitted (single inheritance). The parent must be a `resource`, not a trait, struct, or enum.
- Neither side may be generic: a generic resource takes no part in `extends` yet. Not a rejection of the idea — no consumer asks for it (WebIDL has no generics), so it is unbuilt rather than out of scope. The check reads the declaration's arity, not the written shape, so `Base` and `Base<i32>` are one answer.
- `extends` is permitted only when both the declaring resource and its parent declare `linearity = "unrestricted"` in `#[cm(...)]`. A mismatch is a compile error (per the linearity section above).
- Visibility is independent: a `pub resource X extends Y` is permitted whether `Y` is `pub` or module-private. **Unenforced**: nothing checks that `Y` is visible where `X` is used, so a public child currently reaches a private parent's methods from another module. Same gap as rule (5) below, and the same fix — resource methods need a visibility of their own first.
- Cycles are rejected (`A extends B`, `B extends A`).

`extends` does not appear in any other position. There is no `extends` clause on `struct`, `trait`, `enum`, `variant`, `flags`, `effect`, or function declarations in this WEP. (Trait supertraits use a separate syntax, `:` — see [Super Traits](./wep-2026-07-27-super-traits.md).)

### No cross-linearity conversion in v1

An unrestricted resource and an affine one are **distinct types from Wado's perspective**, even if they happen to point to the same host concept. There is no implicit conversion, and no `as`-cast, between them in v1.

In practice this is not a constraint: WebIDL bindings live in `web:*` modules and do not appear in WIT signatures, and vice versa. The two linearities answer different questions — one is a CM-owned lifetime, the other a value the host interprets — so we expect the moment "convert an affine handle to an unrestricted one" becomes urgent to never quite arrive.

### Subtyping rules

Given `resource Child extends Parent`, the relation `Child <: Parent` is induced. The relation is **reflexive** (`T <: T`), **transitive** (`A <: B` and `B <: C` ⟹ `A <: C`), and **antisymmetric** (no cycles, enforced syntactically). Resources unrelated by `extends` are incomparable.

The variance rules below are picked for soundness — every position where a write can re-establish the underlying value at a different concrete type is **invariant**. This is strictly stronger than Rust's defaults and rules out Java-array-style breakage.

#### Reference types

Given `Child <: Parent`:

| Type         | Subtyping                          | Justification                                                                                                                                                                              |
| ------------ | ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `&Child`     | `&Child <: &Parent`                | Read-only view; every method available on `&Parent` is available on `&Child`.                                                                                                              |
| `&mut Child` | **invariant** in the resource type | A `&mut Parent` permits writing back any `Parent` (e.g., `*r = other_parent`); allowing `&mut Child <: &mut Parent` would let an arbitrary `Parent` be assigned where `Child` is required. |

Unrestricted resource handles have value semantics, so the common case is plain value passing — implicit upcast happens at the call site:

```wado
fn read_node(n: Node) -> u16 { return n.node_type(); }
let el: Element = ...;
read_node(el);               // OK: Element flows where Node is expected
```

The `&T` covariance rule applies when references are used, but `&` on resources is rare outside method receivers (`&self`), so the table entry is more formal than practical. `&mut T` invariance is the technical guard against `*r = parent_value` installing a non-`Child` value through a child reference; with `&mut self` not appearing on resources in this WEP's scope (see the Downcast sidebar), the rule rarely surfaces in resource code but remains in force for any `&mut T` slot.

#### Function types

**v1 is invariant in both**, and the rest of this section is the direction, not the state. Invariance only rejects programs subtyping would accept, and no consumer writes a resource-typed function parameter yet: Tide's callbacks reach the host through a key, not a Wado function type ([Tide § Callbacks](./wep-2026-04-01-tide.md#callbacks)).

The direction: `fn(A) -> B` (with or without `with` effects) is **contravariant in `A`** and **covariant in `B`**. Effects do not interact with subtyping for resources; they follow their own rules.

```wado
let f: fn(Element) -> i32 = ...;
let g: fn(Node) -> i32 = f;        // ERROR: contravariance — f might not accept arbitrary Nodes
let h: fn(Element) -> i32 = ...;   // OK
let k: fn(Element) -> Element = ...;
let m: fn(Element) -> Node = k;    // OK: Element <: Node in the result
```

The contravariant argument rule is what makes a generic event listener like `fn(Event)` accept callbacks that handle the most general event type, while still rejecting callbacks that demand a more specific input than the listener might receive.

#### Container and aggregate types

All composite types that store a resource value, struct field, or container slot are **invariant** in the contained resource type:

| Type                  | Variance in `T`                                 |
| --------------------- | ----------------------------------------------- |
| `List<T>`             | invariant                                       |
| `Option<T>`           | invariant                                       |
| `TreeMap<K, V>`       | invariant in both                               |
| `[T, U, ...]` (tuple) | invariant in each element                       |
| `struct { f: T }`     | invariant in `T` (field assignment writes back) |

The conservative invariance is forced by the existence of `&mut` access into the container — `arr[i] = other` and `m.f = other` must not be able to install an unrelated subtype.

#### Read-only views: covariant exception

A handful of types are "produce-only" with respect to their type parameter: there is no API that takes a `&mut Self` to write a `T` back. For those, covariance is sound:

| Type                | Variance in `T` | Reason                                                                                                                             |
| ------------------- | --------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `Future<T>`         | covariant       | The only consumer-side API is `read()` which yields `T`. There is no `set` on the read end; writes go through `FutureWritable<T>`. |
| `FutureWritable<T>` | contravariant   | Symmetric: writes into `T`, never reads.                                                                                           |
| `Stream<T>`         | covariant       | Same shape as `Future<T>`, repeated.                                                                                               |
| `StreamWritable<T>` | contravariant   | Same as `FutureWritable<T>`.                                                                                                       |

These exceptions are tied to specific stdlib types whose API surface the compiler knows. They do **not** generalize to user-defined generics. Every user-defined generic — `struct MyBox<T>`, `resource MyContainer<T>`, `variant MyEither<L, R>`, etc. — is **invariant** in each of its type parameters, regardless of how the parameter is used inside the body. There is no variance annotation, and no auto-variance inference.

Concretely, given `Element extends Node`:

```wado
struct Box<T> { value: T }

let el: Element = ...;
let el_box: Box<Element> = Box { value: el };
let node_box: Box<Node> = el_box;          // ERROR: Box<Element> ≮: Box<Node>
let node_box: Box<Node> = Box { value: el_box.value };  // OK: upcast at field site
```

In practice variance only constrains user code when `T` is itself a resource that participates in an `extends` hierarchy. For non-resource `T` (primitives, unrelated structs, etc.) there is no subtyping to propagate, so the rule has no observable effect.

#### Where coercion fires

Implicit upcast is inserted at:

- Function and method call argument positions
- Function return positions
- `let` and `let mut` bindings with an explicit type annotation
- Assignments to struct fields and container slots that are typed as a parent
- Branch arms of `if` / `match` / `loop` that need to unify to a common parent type

Implicit upcast does **not** fire at:

- Type parameter inference (`T` is solved to the most specific concrete type; the upcast happens later, at a use site)
- Inside aggregate types where the rule above forces invariance — e.g., constructing `List<Node>` from `[el1, el2]` where `el1: Element, el2: Element` requires an explicit annotation, because `List<T>` is invariant
- A constructor's payload, even against a container of the parent: `Option::Some(el)` against `Option<Node>` is an `Option<Element>`, so write `Option::Some(el as Node)`. The constructor's type parameter is solved first, as in the first item
- Across the affine / unrestricted boundary — by the rule from §"No cross-linearity conversion in v1"

#### Pattern matching and `match`

A `match` on a value of type `Parent` cannot reach a `Child` arm by structure alone — a case, a field or a tuple shape says nothing about which resource type a handle names. Narrowing to `Child` is a type pattern, which the host answers at runtime (§"Narrowing is a pattern").

### Method resolution

Method resolution is **fully static**. There is no virtual dispatch, no vtable, no late binding. The compiler walks the `extends` chain at compile time, picks one declaration, and emits a direct CM-level method call.

#### Core algorithm

For `recv.foo(args)`:

1. Walk the `extends` chain starting at `Recv`, collecting all `fn foo` declarations on each ancestor.
2. Walk the in-scope trait impls applicable to `Recv`, collecting all `fn foo` declarations.
3. Combine the two sets:
   - Empty → `no method 'foo'` error.
   - Exactly one declaration → resolve to it. Insert an implicit upcast on `recv` to that declaration's owning type.
   - Two or more declarations → **ambiguity error**. The user must disambiguate.
4. Emit the CM-level call to the resolved method on the resolved owning type, passing the upcast `recv`.

Implicit upcast in step 3 is a type-level operation only; on an unrestricted resource it lowers to a no-op at the wasm level.

#### Resolved corner cases

The five subtle cases below have explicit rules. All of them are validated at compile time; none of them rely on runtime checks.

##### (1) Override is forbidden

A child resource cannot redeclare a method with the same name as any method reachable through its `extends` chain. Doing so is a hard error.

```wado
pub resource Node    extends EventTarget { fn clone(&self) -> Node; }
pub resource Element extends Node        { fn clone(&self) -> Element; }  // ERROR: Element redeclares Node::clone
```

Reason: with no override, resolution stays purely static and `Self` semantics (see (4)) stay simple. WebIDL specifications avoid name collisions across an inheritance chain by convention, so this rule is rarely felt by the binding generator.

##### (2) Trait-vs-inherited collisions are an error

When the same method name is reachable through both the `extends` chain and a visible trait impl, the call is ambiguous and must be disambiguated explicitly:

```wado
pub resource Element extends Node { fn id(&self) -> String; }
trait Identified { fn id(&self) -> String; }
impl Identified for Element { ... }

el.id();              // ERROR: ambiguous — Element::id vs Identified::id
Element::id(&el);     // OK: invoke the inherent method
Identified::id(&el);  // OK: invoke the trait method
```

Reason: silently picking either side has a known failure mode. "Resource-first" silently shadows trait impls when a parent later grows a method; "trait-first" silently rebinds existing call sites when an `impl` is added. Hard error rejects both refactor hazards. Disambiguation only costs at the colliding call site, and the WebIDL/mixin pattern is curated to avoid such collisions in the first place.

##### (3) Static methods do not inherit

Resource-level static methods (no `&self` / `&mut self` parameter) belong to their declaring resource only. They are not reachable through subtypes.

```wado
pub resource Node    { fn create() -> Node; }
pub resource Element extends Node { ... }

Node::create();      // OK
Element::create();   // ERROR: no static method 'create' on Element
```

Reason: static methods correspond to a specific CM resource type's constructor / factory. Inheriting them would let a child name invoke the parent's factory, returning the parent type — confusing and not what the host implements.

##### (4) `Self` is fixed at the declaration site

Inside a method body, `Self` resolves to the resource that declares the method, not the dynamic / call-site type.

```wado
pub resource Node {
    fn next(&self) -> Self;   // Self == Node, always
}

let el: Element = ...;
let n: Node = el.next();      // return type is Node
```

Reason: with override forbidden (rule 1), there is no mechanism for a child to narrow `Self`. Treating `Self` as call-site-typed would require either a covariant override or a runtime cast — both rejected by other rules in this WEP. If a child needs a more specific result type, it declares a separate method with a different name.

##### (5) Visibility is judged at the declaring module

A method's visibility (`pub` or module-private) is evaluated against the **module that declares it**, not the module that declares the receiver's type.

```wado
// in dom.wado
pub resource Node {
    fn private_helper(&self);   // module-private to dom.wado
}
pub resource Element extends Node { ... }

// in user.wado
let el: Element = ...;
el.private_helper();            // ERROR: private_helper is visible only in dom.wado
```

Reason: `extends` is a type-level relation, not a name-space merge. Inheriting visibility from the child would let the child silently re-export private parent internals.

Not implementable as written today, and left unimplemented: a resource method has no visibility of its own — it takes the resource's, and the parser gives every one `Visibility::Private` for that reason — so a module-private method inside a `pub resource` cannot be spelled. The rule needs per-method visibility on resources first, which no consumer asks for: WebIDL members are all public.

### Narrowing is a pattern

Going from a parent handle to a child is a **type pattern**, not a method. A
pattern may carry a type ascription:

```
pattern : type
```

It matches when the subject is a value of that type, and the pattern to its left
binds it. There is no `downcast` method, no `Option` in between, and no name
synthesized into a resource's method namespace. A function that holds a parent
holds what it narrows to; see
[Signature-Resource Inference](./wep-2026-01-27-effect-system-design.md#signature-resource-inference).

```wado
let el: Element = ...;
if let input: HtmlInputElement = el {
    input.value();             // the full HtmlInputElement API
    input.set_attribute(...);  // inherited from Element
}
el.tag_name();                 // el is still valid: the handle was copied
```

#### Refutability is read off the subtype relation

One rule decides what an ascription means, from the subject's static type `S`
and the ascribed `T`:

| Relation          | Meaning                                                           |
| ----------------- | ----------------------------------------------------------------- |
| `S <: T`          | irrefutable — the implicit upcast, or an ordinary type annotation |
| `T <: S`, `T ≠ S` | refutable — a test of the handle's class, at runtime              |
| otherwise         | a type error, as a mismatched annotation is today                 |

The refutable case exists only where `extends` relates the two, so no other
relation becomes a runtime test: a newtype still needs its `as`, a literal still
coerces, and an unrelated annotation is still an error with the same message.

A refutable pattern needs a position that admits failure, and Wado already
decides which positions those are. So the annotation on a `let` and the narrowing
pattern become one construct under one rule. The grammar says this by moving a
single clause: the ascription leaves `letStatement` and joins `pattern`. Every
other pattern position — `if let`, `match`, `matches`, `let … else` — then picks
it up for free.

```text
letStatement : 'let' pattern ('=' expression ('else' block)?)?
patternPrimary : … | patternPrimary ':' typeRef
```

`let x: T = e` parses identically under both, so the spelling a reader already
knows keeps working:

```wado
let n: Node = el;                                   // irrefutable: today's upcast
let input: HtmlInputElement = el;                   // ERROR, as `let Some(x) = opt` is
let input: HtmlInputElement = el else { return; };  // the guard form, from `let ... else`
if let input: HtmlInputElement = el { ... }
if e matches { _: KeyboardEvent } { ... }           // the predicate form, bindings not escaping
```

An irrefutable ascription keeps its second job: it supplies type context, so
`let x: i64 = 42` coerces the literal exactly as before.

#### Why a pattern rather than a method

- Every real use site destructures the result immediately. A method returning
  `Option<T>` builds an aggregate only to take it apart on the next token, and
  [Tide](./wep-2026-04-01-tide.md)'s examples are all that shape.
- Wado narrows a variant only by pattern
  ([Variant-Independent Types](./wep-2026-02-09-variant-independent-types.md)).
  Giving resources a method-shaped route as well means two spellings of one idea.
- A synthesized `downcast` takes a name in the resource's method namespace,
  where the method-resolution rules above are otherwise strict: an override is
  forbidden, a trait collision is an error. WebIDL member names are not ours to
  control, so such a collision can really happen.
- One-of-N dispatch comes out flat rather than nested, which is the shape events
  need.
- The construct does not say how the binding is produced. Extending `extends` to
  affine resources later can bind by reference without changing the surface,
  which a method returning `Option<T>` by value could not.
- No `Option` is built, so the container invariance below never arises at a
  narrowing site.

The cost is that there is no expression form. A narrowing cannot be held as an
`Option<T>` and passed along without writing the `if let` out. No consumer wants
one yet, and adding it later would accept programs that are rejected now, which
this WEP's guiding principle permits.

#### Allowed targets

The static type checker enforces, at the pattern:

| Relation between `S` and `T`                                          | Status                                    |
| --------------------------------------------------------------------- | ----------------------------------------- |
| `T` is a strict subtype of `S`                                        | refutable — the runtime test              |
| `T == S`, or `S <: T`                                                 | irrefutable — the upcast, no host call    |
| `T` and `S` share an ancestor but neither extends the other (sibling) | compile error (statically cannot succeed) |
| `T` and `S` are unrelated                                             | compile error                             |

`T == S` is not an error, unlike the method form it replaces: under one rule it
degrades to a plain annotation, so a generator may emit the same shape whether or
not the narrowing turns out to be trivial. The sibling and unrelated rows stay
errors — neither can ever match.

Both `S` and `T` are unrestricted, because `extends` requires it on both sides.

#### Generic targets are forbidden in v1

`T` must be a concrete type at the pattern. A function like:

```wado
fn is_a<T>(el: Element) -> bool {
    return el matches { _: T };   // ERROR in v1
}
```

is rejected: nothing states that `T` is a subtype of `Element`, so the compiler
cannot tell a runtime test from an error. Accepting it needs a subtype-bound
syntax (`T <: Element`), which no consumer asks for — a known gap rather than a
refusal.

#### Open world: no exhaustiveness, and dead arms

A `match` over type patterns can never be exhaustive. The host may return an
object whose type the program does not name — WebIDL hierarchies are open, and
the slice a build compiles against is a cut of them — so a final `_` arm is
required:

```wado
match e {
    ke: KeyboardEvent => ke.key(),
    me: MouseEvent => `${me.client_x()}`,
    _ => "other",
}
```

[`match type`](./wep-2026-09-05-total-reflection.md) is the opposite case. It
narrows a type parameter at compile time, drops the arms it does not select, and
so is exhaustive and carries no `_`. A type pattern narrows a value at runtime
and cannot close its case set. The `type` keyword is what tells the two apart at
a glance.

Arms are tried in order, so an arm whose type is an ancestor of a later arm's
makes that later arm unreachable. Reachability checking therefore reads the
subtype lattice, not just syntactic equality, and reports the dead arm.

#### Failure mode

A failed narrowing takes the other branch. There is no panic, no effect, no
exception.

#### Host runtime contract

A handle is a number the host mints, `class * 2^37 + index`: an integer-valued
`f64` below 2^53, holding a 16-bit class and a 37-bit index into the host's
object table. It is never a NaN and never takes part in arithmetic; the guest
reads it only as those two fields.

The classes are numbered by the declarations. Each resource in an `extends` tree
declares `#[cm(..., classes = "lo..=hi")]`: its own class is `lo`, and the
resources extending it hold the rest of the range. A pre-order walk of the tree
assigns them, which is what `wado-from-idl` does in slice order. The compiler
accepts a numbering only where a range test is sound:

- a child's range lies inside its parent's, above the parent's own class;
- no two siblings share a class;
- a tree is numbered whole or not at all;
- a narrowing target declares `classes`.

A gap in a range is allowed. It stands for classes the slice leaves out.

A host written in Wado, such as `package-web`'s `SurfaceDom`, reads these
numbers rather than copying them: `wado-from-idl` emits each resource's own
class and the stride as `internal` globals (`NODE_CLASS`, `HANDLE_CLASS_STRIDE`).

The host tags each object with the class of the nearest ancestor of its runtime
type that the slice declares. An `HTMLDivElement` in a slice that stops at
`HTMLElement` carries `HtmlElement`'s class. The host answers this
`instanceof`-shaped question once, when it first hands the object out. A type the
slice does not name falls back to its nearest named ancestor, which is what an
open world needs.

A narrowing to `T` with classes `lo..=hi` lowers to
`lo * 2^37 <= h && h < (hi + 1) * 2^37` and a branch. The guest compares the
handle's bits as a `u64`, which orders a non-negative `f64` as its value does. A
negative `f64`, an infinity and a NaN fall outside every range. A fraction inside
a range passes, which only a handle forged with `as` can be: the host mints
integers. It is two integer compares with no boundary crossing, so a `match`
with `k` type-pattern arms costs no host call at all. The handle flows through
unchanged in the matching arm, because the two Wado types are one wasm value.

The guest holds the bits rather than the `f64` so that every comparison on a
handle is exact: `f64` equality would make a NaN unequal to itself and `-0.0`
equal to `0.0`, and `as` makes a handle of any `f64`.

The host hands out one handle per object, so the same object always crosses as
the same number. That interning is what makes `==` a plain compare (below).

`f64` rather than an integer: a `u32` has no room for a class beside an index,
and jco lifts a `u64` into a JavaScript `BigInt`, which the glue would pay for on
every call. An `f64` is a plain JavaScript number, and its 53 integer bits hold
both fields.

#### Sidebar: unrestricted resource handles are immutable

Every unrestricted `resource` method takes `&self`. The Wado-side handle (an
opaque index) carries no mutable state of its own; all observable mutation occurs
in the host object it names and is invoked through ordinary `&self` host calls.
There is no language-level rule preventing someone from writing
`fn foo(&mut self)` on a resource today, but in this WEP's scope the pattern does
not arise. A future WEP can decide whether to forbid `&mut self` on resources
outright.

Affine resources differ: in addition to `&self` methods they have by-value
consuming methods (e.g. `Request::consume_body`), which transfer ownership of the
receiver. Their ownership model is specified in
[WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md).

### Interaction with existing features

Four interactions need explicit rules. Effects have one more, stated in §"Narrowing is a pattern": a function holding a parent holds what it narrows to. Everything else (`Default`, `Ord`, `Drop`/RAII, variants holding resources, `fn` types, pattern matching) follows from the subtyping and method-resolution rules already established and needs no separate treatment.

#### `Eq` is auto-derived as reference equality

Every unrestricted resource auto-derives `Eq`, so a type holding one derives it as well. Two handles compare equal iff they reference the same host object — JavaScript's `===` semantics for the browser case.

`Eq` compares the bits of the two handles. The host interns handles, and an object's class never changes, so two handles are equal exactly when they name one object. No host call is made.

The comparison is on bits, not on `f64` values, so it agrees with itself on any `f64` that `as` makes a handle of: a NaN handle equals itself, and `0.0` and `-0.0` are two handles.

Cross-type comparison falls out of subtyping. `el == html_input` is well-typed when one operand is upcast to the other's static type, and the upcast leaves the number as it was.

`Ord` is **not** auto-derived. Resources have no natural ordering and the host has no obligation to define one.

#### `Inspect` shows the dynamic type, `Display` asks the host

Debug output is most useful when it shows what is really there. So `Inspect` (`${x:?}` / `${x:#?}`) renders the dynamic type, not the static one. The handle carries its class, and the class names one resource in the tree:

```wado
let n: Node = doc.create_element("div", null);
`${n:?}`   // "HtmlElement { type_id: 3, object_id: 1 }"
```

`type_id` is the class and `object_id` the index within it. The name is read from the class alone, so no host call is made. A class no resource in the tree owns keeps the static type's name. An `f64` no host mints, which `as` can make, renders as `Node { handle: 1.5 }`.

`Display` (`${x}`) has no such answer in the handle, so it calls a host-imported formatter:

```wit
display: func(r: extern-handle) -> string
```

A user `impl Inspect for Element { ... }` (or `Display`) shadows the auto-derived one by the normal trait-resolution rules, with the trait-vs-inherited collision rule (rule 2 of method resolution) keeping ambiguities loud.

#### `serde` is a compile error on resources

Wado derives `Serialize` / `Deserialize` where a use or bound asks for them (see [WEP: Serde](./wep-2026-02-28-serde.md)). For a resource type — or any struct or variant that transitively contains one — the derivation **fails at compile time**:

```wado
pub resource Element extends Node { ... }

impl Serialize for Element;            // ERROR: cannot synthesize Serialize for resource
struct Wrapper { el: Element }
impl Serialize for Wrapper;            // ERROR: Wrapper.el is a resource
```

There is no silent fallback, no runtime panic, no placeholder serialization. Resources are opaque host references; their identity is meaningful only inside the running component instance, so serializing one and reading it back has no defensible semantics. A user who wants a Serialize-shaped projection writes a hand-rolled `impl Serialize for Element { fn serialize(...) ... }` that picks specific fields off the host object.

#### `Option<T>` and `Result<T, E>` are invariant — a known sharp edge

Per the subtyping rules, every aggregate is invariant in its type parameters. `Option` and `Result` are aggregates, so:

```wado
let r: Option<HtmlInputElement> = ...;
let n: Option<Node> = r;                   // ERROR: Option<HtmlInputElement> ≮: Option<Node>
let n: Option<Node> = r.map(|el| el);      // OK: upcast applies inside the closure body
let s: Option<Node> = Option::Some(el);    // ERROR: the payload is not upcast
let s: Option<Node> = Option::Some(el as Node);  // OK
```

Coming from languages where `Option` is covariant (Scala, Kotlin's nullable types, etc.), this is the most likely surprise. The same rule applies to `Result<T, E>` in both type parameters and to every other generic container. The `.map(...)` workaround is short and does not allocate.

This is deliberate. Wado does not special-case a container or a constructor to make an upcast fit. The consistency comes from rules Wado already has: aggregates are invariant, type parameters are solved to the most specific type, and `as` converts a value explicitly.

### Lowering

How `extends` and the operations on it lower from Wado to WIT/CM, and from WIT/CM to wasm. The recurring pattern is **erasure at the CM layer**: extends-related Wado types collapse to a single CM resource type, with method namespacing carrying the only distinction the host needs.

#### Three layers

| Layer  | Identity                                                        |
| ------ | --------------------------------------------------------------- |
| Wado   | each type in the `extends` chain is distinct (`Element ≠ Node`) |
| WIT/CM | one type, `extern-handle`                                       |
| Wasm   | an `f64` bit pattern: a class and a host-table index            |

This is the same erasure pattern as [Newtype Semantics](./wep-2026-01-29-newtype-semantics.md): the Wado type system holds the structure, the wasm output knows nothing about it. extends differs from newtype only in that **method namespacing is preserved at the WIT layer** — methods are imported under per-Wado-type WIT interfaces, even though the receiver type is universal.

#### Universal receiver at the WIT layer

A single CM type:

```wit
type extern-handle = f64;   // v1; becomes the CM extern-handle type under CM-GC
```

v1 does not wait for CM-GC. The universal handle is the `f64` §"Host runtime contract" describes, naming an entry in a host-side table owned by the host glue, copyable and exempt from the affine analysis of [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — which is what gives unrestricted handles their value semantics. It is deliberately not a CM `resource`: CM resource handles are affine, and an affine handle cannot have the value semantics this WEP specifies. Every signature below is written against the `extern-handle` name, so the CM-GC switch changes the alias and the lowering, not the shapes.

Every extends-related Wado type is the same `extern-handle` once it crosses the boundary. There is no `event-target` resource, no `node` resource, no `element` resource at the CM level — only the methods are split.

The single type is what makes the erasure free: there is no per-Wado-type CM type for the boundary to convert between, so nothing has to accept "the same handle valid under multiple resource types".

#### Method imports

Methods are grouped into WIT interfaces named after their declaring Wado type. The receiver argument is `extern-handle`:

```wit
interface event-target {
    add-event-listener: func(self: extern-handle, type: string,
                             callback: option<extern-handle>,
                             options: option<extern-handle>);
}
interface node {
    append-child: func(self: extern-handle, child: extern-handle) -> extern-handle;
    text-content: func(self: extern-handle) -> option<string>;
}
interface element {
    get-attribute: func(self: extern-handle, name: string) -> option<string>;
}
interface mouse-event {
    button: func(self: extern-handle) -> s16;
}
interface html-button-element {
    name: func(self: extern-handle) -> string;
    set-name: func(self: extern-handle, value: string);
}
```

Same-named methods on unrelated Wado types (e.g., a hypothetical `mouse-event.button` vs `html-button-element.button`) do not collide because the WIT interface namespace separates them.

#### Built-in formatters

`Display` lowers to one flat CM import over `extern-handle`, whatever the size
of the hierarchy:

```wit
interface lang {
    display: func(r: extern-handle) -> string;
}
```

Narrowing, `Eq` and `Inspect` import nothing: the handle already carries what
they read.

#### Operation lowering at a glance

| Wado operation                           | Lowering                                                              |
| ---------------------------------------- | --------------------------------------------------------------------- |
| `let n: Node = el;` (implicit upcast)    | identity                                                              |
| `el.foo()` resolving to `Node::foo`      | call `node.foo(el, ...)`                                              |
| `input: HtmlInputElement` (type pattern) | compare `el` against the target's class range, branch; `el` unchanged |
| `a == b` for unrestricted `a`, `b`       | `i64.eq` on the bits of the two handles                               |
| `` `${x:?}` ``                           | name the resource owning `x`'s class, then write the class and index  |
| `` `${x}` ``                             | call `display(x)`                                                     |

Upcast and the receiver argument of inherited methods are wasm-level no-ops; the same handle value flows through unchanged.

`as` converts a handle to or from `f64`, keeping every bit, or upcasts it to a
resource it extends.
No other cast accepts one. A cast to an integer would lose the class, and a
downcast by `as` would skip the class test a type pattern makes.

#### Interaction with WIT bundling

`extends` is Wado-only metadata. The bundled WIT ([WIT Bundling](./wep-2026-03-21-wit-bundling.md)) emitted with the component contains only the flat per-Wado-type interfaces above; nothing about the inheritance relation appears. Other-language consumers of the bundled WIT see a flat method surface and do not need to understand `extends` to call any method.

#### Lifecycle

`extends` introduces no drop protocol: however many Wado static types name a handle, it is one value, copied like any number.

There is no release path. A handle the host hands out is never reclaimed, and each one costs a table slot for the lifetime of the instance. That counts every handle the program receives, not only the ones it keeps: a loop calling `query_selector` once a frame leaks one slot a frame. The CM knows nothing about the handle, so it cannot reclaim it, and Wasm GC offers no finalization to hang a release on. See the known gap below for the only representation that closes this.

## Consequences

### Implementation Status

This WEP is the full feature. The order it lands in is [Tide § Minimum Implementation Roadmap](./wep-2026-04-01-tide.md#minimum-implementation-roadmap), the one place milestones are sequenced.

#### Status

Implemented, with tests in `wado-compiler/tests/integration/unrestricted_resource.rs`:

- `#[cm(..., linearity = "affine" | "unrestricted", classes = "lo..=hi")]` — parsed, value and placement validated (`parser.rs`), `classes` only beside `"unrestricted"`; an unrestricted resource is copyable and exempt from the affine analysis and the cleanup pass (`resource_move_check.rs`, `synthesis/resource_cleanup.rs`, both reading `TypeTable::is_unrestricted_resource`).
- `extends` — keyword, AST, parser; the parent is resolved and validated in `elaborator/orchestration.rs::resolve_resource_extends` (parent is a resource, both sides unrestricted, no cycle, neither side generic), and the relation lives in `TypeTable::resource_parent` / `is_resource_subtype`.
- Subtyping and implicit upcast — one rule in `elaborator/typecheck.rs::check_at`, gated by `Position`: value, `return` and a `&T` referent admit a subtype; `&mut T`, containers and function types do not. Branch agreement is `elaborator/expr.rs::agreed_branch_type`, which `if`, `if let` and `match` all route through.
- Method resolution over the chain, and the corner cases: override forbidden, trait-vs-inherited ambiguity, statics do not inherit, `Self` fixed at the declaring resource.

- Class numbering — `resolve_resource_extends` checks each committed link against the rules in §"Host runtime contract", and `wado-from-idl` numbers `web:dom` in pre-order (`tests/fixtures/error_resource_classes.wado`).
- Lowering. An unrestricted resource is not a CM `resource`: it registers as an `f64` newtype (`component_model.rs`), so every `own` / `borrow` path passes it by, a `&self` receiver loses its reference, and the WIT renders the same `f64` in every position (`wit_emit.rs::extern_handle`). The guest holds the `f64`'s bits as a `u64` (`TypeTable::handle_scalar`), which the boundary reinterprets, and linear memory stores as the same eight bytes. Upcast and an inherited method's receiver are wasm-level no-ops — the call resolves to the declaring resource through `MethodOwner::Ancestor`.

- The registry reads that newtype two ways. `resolve_type` peels the handle to its `f64`, the view the boundary decides with: the flat ABI, the canonical options, the emitted WIT. `value_type` keeps the resource's own type, the view the guest holds. A binding's signature takes the second, because `Option<Element>` and `Option<f64>` are distinct GC types (`tests/integration/web_dom.rs`). `cm_type_to_type_id` finds the resource's `TypeId` under its package, a `web:` package being one flat file. A module outside the stdlib declares such a resource as the stdlib would: it is not a local newtype, so no instance exports it as a named type.

- Type patterns. `p: T` is a pattern wherever one stands, and a `let` annotation is that pattern. An ascription `T` strictly extending the subject's type narrows: a plain `let` rejects it as refutable, and a `match` over one needs a final `_` and reports an arm an earlier ancestor arm shadows. The test compares the handle against `T`'s class range, and a target without `classes` is rejected.
- `Eq`. `==` / `!=` on two handles one of whose types extends the other compares their bits (`tests/fixtures/unrestricted_handle_bits.wado`). The trait holds of every unrestricted resource, so `Option<Node>` or a struct holding one derives it too (`tests/fixtures/resource_eq_through_option.wado`).
- `as` between a handle and anything but `f64` or a handle type it upcasts to is rejected (`tests/fixtures/error_unrestricted_resource_cast.wado`).
- `Inspect` renders the dynamic type (`tests/fixtures/inspect_unrestricted_handle.wado`).

Not built:

- The `Display` host import.
- Rule (5), visibility judged at the declaring module, and the unenforced parent-visibility bullet above. Both need per-method visibility on resources, which nothing asks for yet.
- Declaring `linearity = "unrestricted"` on the non-owning tokens (`Waitable`, `core:icu`'s interned handles), which still take their value semantics from the absence of a `dtor`; and generic resources on either side of `extends`.
- A parent declared by a prebuilt module. The collect pass skips what the stdlib snapshot covers, so a snapshot parent reaches validation without its method names or its arity; rather than accept what it cannot check, the compiler rejects the clause. `web:dom` is a package, never snapshotted, so nothing reaches it yet.

### Known gap: the handle is an index, not a reference

An extern-handle would rather be a Wasm GC `externref`: the collector would reclaim it, and the host table would disappear along with the leak above. The WIR already carries the type (`WirAbstractHeapType::Extern`), so the guest side is not what blocks it.

The Component Model is. Its value types include no reference type; `own` and `borrow` are the only handle types and both carry the obligations §"Why this is hard" describes; and a resource's representation is validated to `i32` or `i64`. Nor is there a side door: `externtype` admits a `core module` import but no `core func`, and a component satisfies an imported module's own imports from its core index spaces, which bottom out at `canon lower` — so host code is reachable only through the canonical ABI. CM-GC changes the representation of `own` / `borrow`, not their semantics.

Nothing inside the component target reaches it. An `externref`-typed host import belongs to a core module, which is how `wasm-bindgen` reaches the same APIs. Targeting a core module would give up the CM machinery the web target rides on, including the jco transpile path [Tide](./wep-2026-04-01-tide.md) assumes.

## See Also

- [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — affine ownership and the resource-scoped borrow checker, and the `dtor`-decides-the-kind rule this WEP gives a surface spelling
- [GC in Components](./wep-2026-03-28-gc-in-components.md) — resource representation in CM-GC vs CM-LM
- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md) — how flat CM resources map to Wado today
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md) — where the CM lowering happens
- [WebIDL Binding Generator (Tide)](./wep-2026-04-01-tide.md) — the primary consumer of this feature
