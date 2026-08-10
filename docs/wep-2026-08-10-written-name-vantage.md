# WEP 2026-08-10: Vantage — identity from a written type name

## Context

A type name in Wado source is module-relative. `Box_` written in `entry.wado`
and `Box_` written in `sub/sub.wado` are two declarations; which one a spelling
means is a fact about the module that wrote it — its `use` list, its aliases,
its own declarations, the prelude behind them. The module a name is read from
is its **vantage**, and without one a written head answers no identity question
at all.

The compiler nevertheless had a function that answered it anyway:

```rust
pub(super) fn get_type_name_static(ty: &ast::Type) -> String
```

Two properties made the whole defect class writable through it.

- **It needs no vantage.** A free function over the AST, callable from any
  layer, including layers holding another module's syntax with no record of
  where it came from.
- **It returns a `String`.** The result hashes, compares and keys any registry,
  so it slots into maps whose every other key is a `DeclKey` without a murmur.

It also existed twice — as a free function in `trait_env.rs` and as an
associated function on `Elaborator` in `module.rs`, byte-identical. Two
implementations of one question is the shape WEP 2026-07-29 named as how the
indexes came to disagree with their callers; here it was the same answer
duplicated rather than two answers, but it is the same absent authority.

The result is a defect that reads as a one-line oversight each time and recurs
because nothing prevents it. `tests/fixtures/cross_module_same_name_*` counts
21 fixtures, one per occurrence found. Issue #1769 is the same shape at the
coherence layer: the inherent-impl collision check bucketed methods by the
written head, so a user `struct Box_` with `impl Box_<i32>` collided with any
other module's `impl<T> Box_<T>` — including the stdlib's `TreeMap`, `TreeSet`,
`Router`, `RangeExclusive` and `ArrayIter`, which made a handful of ordinary
names unusable for a user type with no second user module in sight.

WEP 2026-07-28 and WEP 2026-07-29 closed exactly this hole on the **resolved**
side: `FqTypeName` carries structure, `DeclName` / `MangledName` /
`StructListKey` carry namespaces, and a name from one cannot reach a consumer
keyed by another. The **written** side kept its `String`, and every occurrence
of the class since has been there.

### The second harvest

`TraitEnv::build` is the one pass that walks every loaded module's items with
the module source in hand. It precomputes each module's import scope, and
resolves every `impl` target through `impl_target_key` into an `ImplTargetKey`.
The vantage exists there, and the answer is computed there.

Five other whole-program checks then walked the same ASTs again — the
inherent-impl collision check, the orphan rules, the variadic-overlap check,
the sealed-trait scan, the trait-method arity check — and none of them was
handed that answer. A second harvest has no vantage to resolve with, so it
re-derived an identity from a bare head. Two of them said so in comments ("would require build-time import
resolution that the current `TraitEnv::build` doesn't have plumbed through";
"resolving the impl's trait name through imports needs machinery this pre-pass
lacks"). The machinery was one pass away, unshared.

That is the shape to design against: not "someone used a string", but "an
identity was derived a second time, somewhere the inputs for deriving it
correctly were absent".

## Decision

Three rules. Each is independently valuable, and they are ordered so the first
closes the most.

### 1. One harvest — the digest carries identity, and every check consumes it

`ImplHeader` is the digest `TraitEnv::build` already makes of every `impl`
block. It gains the vantage and the resolved identities:

```rust
pub(super) struct ImplHeader {
    module: ModuleSource,             // the vantage: who wrote this header
    target: ImplTargetKey,            // resolved once, where the vantage exists
    trait_key: Option<ImplTargetKey>, // ditto; None for an inherent impl
    ...
}
```

and enough of each method's signature (`span`, `name_span`, `param_count`) that
a check reporting on one needs no second look at the AST.

Every whole-program check now reads the digest:

| check                    | keyed by                                       |
| ------------------------ | ---------------------------------------------- |
| inherent-impl collisions | `ImplTargetKey` of the target                  |
| orphan rules (RFC 2451)  | `DeclKey` sets of user-owned types and traits  |
| variadic-impl overlap    | `ImplTargetKey` of the trait                   |
| sealed-trait impls       | the stdlib declaration's `ImplTargetKey`       |
| trait-method arity       | the declaration the impl's own header resolved |

The rule this establishes: **a whole-program check does not walk
`loaded_modules`.** Walking is how a pass ends up holding syntax without a
vantage; the digest is how it holds the vantage by construction.

### 2. One resolver, taking the vantage as an argument

There is one declaring-side answer to "which declaration does this written name
mean?":

```rust
type ResolveWritten<'a> =
    &'a dyn Fn(&ModuleSource, &ast::Type, &[ast::GenericParam]) -> ImplTargetKey;
```

The vantage is the first parameter, so it cannot be forgotten. The third
argument is the surrounding item's own type parameters: a binder shadows any
declaration of the same name, so `impl<T> Trait for T` written where a
`struct T` exists stays a blanket rather than joining that struct's bucket.

Its by-bare-name fallback — for a prelude `internal trait` such as
`ReflectStruct`, which no module `use`s and no symbol exports — is scoped to the
declarations the _position_ admits. A spelling cannot say whether `Codec` means
a `trait Codec` or a `struct Codec`; an unscoped scan hands an `impl Codec { … }`
target the trait's key, which is the wrong bucket and a bogus coherence error.

`ImplTargetKey::TypeParam` answers both "a binder" and "reaches no declaration",
so a consumer that needs them apart must ask the binder question itself, with
`binder_of`, before resolving. The orphan rule is that consumer: a name
resolving to nothing is a **foreign** type, and reading it as an uncovered
parameter drops the coherence error `impl Undeclared { … }` deserves while
inventing an orphan violation for `impl From<Local> for Undeclared`.

Its call-site counterpart is `trait_query::canonical_decl_key_with`, over
`decl_identity_core`. The two are different **vantages**, not two
implementations — an `impl` header is read before any import scope of the
caller exists, and a use site resolves through the caller's imports. Neither is
allowed a third.

### 3. A written head cannot be minted without a vantage

`get_type_name_static` is replaced by a borrowed view that has to be given one:

```rust
/// A type name as written in one module. Names are module-relative, so this
/// answers no identity question on its own: no `PartialEq`, no `Hash`, no
/// `Ord`, no conversion to `String`. The ways out are `resolve_with`, which
/// hands a resolver the spelling and the vantage together, and `Display`, for
/// diagnostics.
pub(crate) struct WrittenHead<'a> {
    spelling: &'a str,
    ref_kind: Option<RefKind>,   // a reference declares nothing; its kind is its identity
    vantage: &'a ModuleSource,
}
```

Missing `Hash` is what stops it keying a registry; missing `PartialEq` is what
stops `head_of(a) != head_of(b)` from compiling. Every defect this WEP fixes was
one of those two expressions: the collision check hashed the head, the orphan
and sealed checks compared it.

`WrittenHead::resolve_with` hands a resolver the spelling and the vantage in one
call, so the two cannot be paired wrongly. Questions that are _not_ identity
questions keep syntax-only helpers, because they are answered inside one item
where the vantage is shared by construction:

- "does the target mention one of this impl's own type parameters?"
  (`target_mentions_impl_param`)
- "is this target a bare binder, and with which bounds?" (`binder_of`,
  `WrittenHead::binder_in`)

One escape remains, `WrittenHead::spelling_pending_migration`, for the dispatch
paths under "Remaining work" that still key a receiver by name. It is unsound
by construction and named so: it is a marker on each surviving hole, not a
sanctioned way to ask. Introducing `WrittenHead` is otherwise
behaviour-preserving — it changes what compiles and where the vantage is
written down, not what any check decides.

## Consequences

Issue #1769 falls out of rule 1: the collision check keys on `header.target`, so
two modules' `Box_` are two buckets and the stdlib's generic inherent impls stop
claiming every user type that shares a head. `impl_inherent_concrete_duplicate_method`
— the real same-owner collision — still fails, because the two impls there
resolve to one key.

Three further corrections fall out rather than being sought:

- The orphan rule projected user-owned declarations down to **bare names**, so
  a user `struct Widget` vouched for the stdlib `Widget` in
  `impl ForeignTrait for Widget`, and a user `trait Display` made every
  `impl Display for <foreign>` in the package look local. Ownership is now a
  `DeclKey` set.
- The sealed-trait scan compared a written head against a compiler item's name
  and exempted the whole check whenever any user module declared a trait of
  that name. The sealed trait is the _stdlib_ declaration of that name, so a
  user trait sharing it is simply a different key — the exemption is gone with
  the ambiguity that motivated it.
- The trait-method arity check matched an impl against "the declaration in the
  impl's own module, else the only one bearing the name", and gave up whenever
  two modules declared the name. It now matches the declaration the header
  resolved to, and stops giving up.

Costs and risks:

- Rules 1 and 2 change behaviour wherever the old bare-name key was wrong, so
  the e2e suite is their gate, not a proof obligation discharged by review.
- `ImplHeader` grows. It is the intended direction — the digest exists so
  consumers stop re-reading the AST — but each added field must be one the
  build phase can fill correctly for _every_ impl block, stdlib included.
- Rule 3 makes every surviving hole say so at its call site, but does not close
  it. Each `spelling_pending_migration` caller is still a place the class can
  produce a wrong answer — just not a place it can be written by accident.

## Remaining work

Each item is a `spelling_pending_migration` caller group that still decides an
identity by comparing spellings.

-
  1. [ ] `method_call::conversion_impl_survey` / `locate_static_method_impl` /
         `has_from_synthesis_request` compare an impl header's written head
         against a `struct_name: &str` taken from the call site. Both sides have
         an identity available — `header.target` and
         `TypeTable::impl_receiver_key(receiver_id)` — and the comparison should
         be between those.
-
  2. [ ] `method_lookup::candidate_matches_receiver` compares the impl's written
         head against `ImplTargetKey::receiver().decl_key()`, a bare declaration
         name on both sides. `names_to_check.contains(&header.target)` is the
         whole test, once the receiver chain and the header agree on the key
         type.
-
  3. [ ] `method_lookup::lookup_static_method_type_params` takes the receiver as
         a written prefix from `call.rs`. Resolve it at the call site, where the
         vantage is the current module.
-
  4. [ ] `TraitEnv::trait_impl_modules` is keyed `(type name, trait name)`, both
         bare. Its readers hold names rather than keys, so re-keying it is the
         same plumbing as 1.
-
  5. [ ] `orchestration.rs`'s associated-type scan is the last whole-program
         walk of `loaded_modules` that reads heads. It belongs on the digest,
         like the five checks that moved.
-
  7. [ ] `trait_query::find_trait_impl_for_type_with_args` compares
         `header.trait_name` — the impl's spelling in its own module — against
         the query's spelling in the caller's, so an aliased bound is
         unsatisfiable and a same-named foreign trait is silently satisfied
         (issue #1785). The impl-side identity is `header.trait_key` and the
         caller-side one is `trait_decl_key_in_frame`; what remains is making
         identity the currency of `type_implements_trait`, whose whole path —
         including the `(TypeId, String)` recursion guard — threads
         `trait_name: &str`.
-
  6. [ ] `item.rs`'s unknown-trait check and `reify.rs`'s default-method
         synthesis are sound already — each hands its spelling to a frame
         resolver whose vantage _is_ the head's module — but they say so only in
         prose. They read through `resolve_with` once the resolver stops needing
         `&mut self` while the head borrows it.
