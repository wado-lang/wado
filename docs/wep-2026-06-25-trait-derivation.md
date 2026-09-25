# WEP: Trait Derivation Policy — Bound-Driven Synthesis

## Context

Wado derives type-directed traits for a type `T`. This WEP decides _when_ a
derived impl exists for `T`.
[Library-Defined Derivation (`ReflectStruct`)](./wep-2026-06-13-reflect-derivation.md)
decides _how_ its body is written.

Two policies used to decide it, and they disagreed:

- `Inspect`, `Eq`, `Ord` and `Default` were synthesized for every eligible type,
  whether or not the program used them.
- `Serialize` and `Deserialize` existed only where the user wrote the empty
  marker `impl Serialize for T;`. A bare `T: Serialize` bound did not trigger
  synthesis.

The split had two costs:

- An anonymous struct has no name, so no marker can be written for it, and it
  could never be serialized. [`core:log`](./wep-2026-06-25-core-log.md) needs
  exactly that for its field path.
- `Eq` and `Ord` cost compile time and code size for every declared type,
  including the ones the program never compares.

## Decision

### Each trait has a policy

| Policy              | A `T: Trait` obligation holds when …                                     | A body is generated  | Traits                                             |
| ------------------- | ------------------------------------------------------------------------ | -------------------- | -------------------------------------------------- |
| `on_bound` (demand) | every member of `T` satisfies the trait                                  | where a use needs it | `Eq`, `Ord`, `Default`, `Serialize`, `Deserialize` |
| `on_bound` (total)  | always                                                                   | for every type       | `Inspect`                                          |
| `display`           | a written `impl Display` exists, `T` is a plain `enum`, or its base does | see below            | `Display`                                          |
| `explicit`          | a written impl exists                                                    | where written        | every user-defined trait                           |

### Demand traits

`Eq`, `Ord`, `Default`, `Serialize` and `Deserialize` are derived structurally
where something needs them. `T: Eq` holds when every field or case of `T`
satisfies `Eq`. `Default` asks instead that every field carries a default
expression. A failure reports a reason chain from the bound to the offending
member ([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)).

These traits are not total. A `fn`-typed field blocks `Eq`, `Ord` and serde,
and a field without a default blocks `Default`. So `T: Eq` is a real
obligation, and generating a body only where one is used saves real code.

A use records the need at the site that resolves it:

- `Eq` and `Ord`: an operator, a comparison method, or a bound.
- `Default`: a `T: Default` bound, or a `P::default()` call.
- serde: a `T: Serialize` or `T: Deserialize` bound.

A value is compared, defaulted or serialized only through a bound or a call the
resolver sees, so recording at those sites finds every use.

A fieldless struct satisfies `Default` vacuously, since it has exactly one
value. That is what lets a marker type stand as a type parameter's default:
`fn info<T: Serialize = NoFields>(msg: String, fields: T = T::default())`.

A plain `enum` and a `flags` type have no members, so both satisfy every
structural obligation outright. `Eq` and `Ord` compare the discriminant and the
bitmask. A struct or variant carrying one therefore keeps its own derivation.

A generic declaration derives once, as a template that monomorphization
instantiates per concrete type. A generic struct derives no `Default`, because a
default expression is elaborated against the declaration. The serde templates
are also generic over the serializer (see [Serde](./wep-2026-02-28-serde.md)).

### A written impl wins where it reaches

A written `impl Trait for T { … }` always wins over a derived one. That holds
for every kind, including the three that erase to a scalar: an `enum`, a
`flags` type and a newtype (`eq_ord_manual_impl_wins.wado`). A newtype needs it
most, since it exists to be a type distinct from its base. This is rank 1 of
the selection order in [Trait Resolution](./wep-2026-09-01-trait-resolution.md).

A written impl wins only for the instances it reaches. `impl<T> Eq for
Pair<T, i32>` answers for `Pair<String, i32>`, and `Pair<i32, i64>` still
derives. Every derived trait behaves this way, `Inspect` included
(`impl_reach_partial_eq_ord_derives_elsewhere.wado`,
`impl_reach_partial_serialize_derives_elsewhere.wado`,
`impl_reach_partial_deserialize_derives_elsewhere.wado`,
`impl_reach_partial_inspect_derives_elsewhere.wado`). `Default` is the
exception only because a generic struct derives none: `Pair<String, i64>` has no
`Default` there (`impl_reach_partial_default_error.wado`).

A written impl reaches every instance of its head only when its target names a
head and writes each argument as a distinct type parameter. A reference, a
tuple, `()` and a function type are shapes, so an impl on one reaches one shape
at a time. A concrete argument or a repeated parameter narrows the impl to some
instances.

### A marker asks for the derivation

An empty marker `impl Trait for T;` checks that `T` is eligible and records the
need, as a bound does. The difference is where an ineligible `T` fails. A marker
is a compile error at its own span. A bound is merely unsatisfied where it is
asked.

- An `Inspect` marker always passes, so it documents intent.
- A `Display` marker is rejected. `Display` cannot be derived for an arbitrary
  type, so a marker has no body to stand for.

### `Inspect` is total

Every type can be debug-formatted, so `T: Inspect` always holds, for a type
parameter or any concrete type, and an `Inspect` marker always passes.

An `Inspect` body is generated for every type kind. `${v:?}` over an unbounded
type parameter has no bound to record, and its concrete type appears only at
monomorphization. Generating every body up front is what makes that case work.
Gating it would need a discovery pass after monomorphization for little saving,
since universal debug output is the point of the trait.

### `Display` is written, with two exceptions

`Display` is a type's human-facing text, and the compiler cannot invent one for
a struct: there is no canonical layout, delimiter or order. So `Display` is
never derived for a struct, a variant or a generic container. `${x}` on such a
type is a compile error that points to `${x:?}`. `T: Display` is a real
obligation, so an API such as `String::push_display` accepts only a type with a
real text form.

Two kinds have a canonical text form, and the compiler supplies it:

- A plain `enum` displays its bare case name (`Red`). `Inspect` shows the
  qualified `Color::Red`, so the two differ.
- A newtype displays as its base does (`Meters = f64` renders `3.14`). The
  format call reads through the newtype to its base. `Inspect` is the one format
  trait a newtype overrides, to add its `as Name` tag.

`${x:#}` runs the same `Display` with `Formatter.alternate` set. It holds exactly
where `Display` does.

### User-defined traits are explicit

A user-defined trait is never derived. An impl exists only where one is written.

### The wire boundary

`Serialize` and `Deserialize` cross a wire or storage boundary. Deriving them on
demand means a type becomes serializable the moment some code asks, and a later
field addition silently extends its wire shape. Rust's `serde` and Swift's
`Codable` are opt-in for that reason. Wado accepts the trade: a whole program has
no downstream consumer to surprise. A written impl and a `#[secret]` field are
the controls, and no opt-out is added.

`Eq` and `Ord` cross no boundary. Deriving on demand changes only when their
body is generated, never what a comparison returns.

## Roadmap

None. Every open item is a known gap below.

## Known gaps

### A user-defined trait cannot opt into derivation

A user trait is always explicit. No declaration syntax lets it take the demand
policy, so a library trait must be implemented by hand for every type, even
where a structural derivation would be obvious.
