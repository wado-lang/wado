# Static Call Resolution — One Walk, Four Answers

## Context

`Type::method(...)` is written five ways: bare, `ns::Type::method()`,
`Type::<A>::method()`, `Self::method()` and `T::method()` through a bound. Every
one of them has to answer the same questions — is this a static at all, which
declaration does it name, what are its parameters, what does it return, what
type parameters does it take.

Sixteen lookups answered those questions, each walking its own subset of one
ladder in its own order:

```
is_static_method                 is_static_method_at
locate_static_method_impl        has_inherent_static_method
declares_method_directly         find_static_method_trait
qualified_method_sig             qualified_method_sig_keyed
unique_qualified_method_sig      unique_qualified_method_sig_keyed
qualified_call_param_types       lookup_static_method_param_types_keyed
lookup_static_method_return_type lookup_static_method_type_params
agreed_qualified_method_return   find_blanket_static_method
```

The rungs they walked are the same ones: the receiver's own declarations, its
resource statics and their chain, a trait impl declaring the method, one
inheriting the trait's default body, the auto-derived `Default`, the newtype
base. What differed was where each stopped, what it did on a miss, and which key
it derived to start from.

So two of them could disagree about one call. The spelling resolved and its
signature did not, and the call reached codegen with no parameter list: an
unchecked arity, an unpadded default, a mangled name missing its trait segment.
Every defect the branch that produced this WEP chased was an instance of that,
and fixing them one at a time did not converge.

The rule this violates was already written down: `wado-compiler/AGENTS.md` says
to answer a question with one resolver rather than partial walkers, "which each
miss a different shape". [Declaration Identity](./wep-2026-08-12-declaration-identity.md)
says one identity, one scope, one answer.

## Decision

One resolution, in `elaborator/static_call.rs`. `resolve_static_callee` walks
the ladder once and every site reads its answer.

### The key is one vantage

The key the caller resolved at its own reference site, else the site's own, else
the name's. A key merely derived from a name never narrows the trait search: the
importing module does not name `Type`, and a primitive's `impl FromStr for f32`
is out of reach from a bare-name key.

### Four outcomes, each meaning one thing

|              |                                                        |
| ------------ | ------------------------------------------------------ |
| `Found`      | one declaration, with its lists read at the receiver   |
| `Overloaded` | one trait, several impls; the argument had none to say |
| `Ambiguous`  | several traits supply the name; no argument can pick   |
| `NotStatic`  | a variant case or a flags member owns the name         |

The lookups differed less on _where they looked_ than on what they did when they
did not fully find something, so each of these had to be named:

- A rung that cannot resolve falls through to the next. It is not a spelling
  that names nothing.
- A declaration whose slots the receiver has not filled resolves, and has a
  return type. Only the _types_ wait on the receiver: how many parameters there
  are, what they are called and which carry defaults are the declaration's own,
  so the arity is enforced and the defaults padded either way. The optionality
  belongs per parameter — a slot the call could not fill is skipped where the
  argument is checked — and never on the list, still less on the resolution.
- A name several declarations answer to picks none until an argument does, which
  is not the same as one picked that no trait names: the first has no identity
  to mangle, the second mangles without a trait segment. They are separate
  outcomes, or the second's spelling is built for the first.
- An overloaded name picks no declaration until an argument does, so it carries
  none to mangle. Its return type still answers where every candidate agrees:
  each `From` impl on a receiver returns it.
- An empty list is not "takes nothing", and neither is a partial answer taken
  for a whole one. A site that reads only `Found` and defaults the rest turns
  every other outcome into a name nothing declares.

### A rule kept in structure has to be restated as data

Choosing which lookup to call _was_ the rule. `has_inherent_static_method`
existing as its own function was the shadowing rule; `impl_method_entries`'
ordering was the qualifier that shadowing holds only within a kind;
`locate_static_method_impl` returning the impl's module while `TraitSig` holds
the trait's was how a call got the right one of the two. None of that survives
the merge on its own, so the resolution states each:

- An inherent declaration shadows an inherited one, and only of the same kind: a
  receiver-less declaration beside an instance one is no alternative, since
  different argument lists reach them.
- A `variant` case, an `enum` case and a `flags` member shadow an inherited
  static of the same name, by that same rule: they are written on the type,
  and the static is only borrowed. This matches Rust, where `V::A(5)` is the
  case and `<V as Tagged>::A(5)` reaches the trait's. It does not extend to an
  inherent static of the same name, which is a type declaring one name twice.
- A trait impl's declaration is mangled with its trait. Taken as inherent it
  names a body nothing declares.
- An inherited default body has two modules — the block's, where the body is
  emitted and the call must point, and the trait's, where its defaults resolve.

### The argument picks a declaration, not a trait

A static call is selected by the parameter the impl declares, compared against
the call's first argument. Arguments after the first do not narrow it further.

`From<T>` made that rule look like two narrower ones. Its source type is also
its trait argument, so the trait reference could stand in for the parameter, and
`from` takes exactly one argument, so one argument looked like the whole list.
Neither holds for any other trait. A trait implemented twice on one receiver
(`impl Conv<A> for M` beside `impl Conv<B> for M`) poses the same question at
any arity, and the declared parameter is what answers it.

Reading the parameter also removes the alias handling the trait-reference
spelling needed. The parameter is resolved in the impl's own frame, so both
sides of the comparison are already canonical.

The trait segment of a mangled name keeps the trait's arguments. Dropping them
collapses two declarations of one method name onto one body. What decides this
is how many times the receiver implements the trait, never which trait it is,
and it holds for a body the block inherits as much as one it writes: the segment
comes from the block's own trait reference. Minting one from the trait
declaration drops the arguments, because a declaration has none to give.

A parameter the block fills is a blanket, not a mismatch. Its unsubstituted
spelling must not be baked into a mangled name, so declining it sends the call
to the blanket resolver, which instantiates it. Only the block's own slots make
a parameter its to fill. A concrete impl whose method carries slots
(`fn build<T: Display>(v: T)`) is filled at the call instead, and once neither
resolves the two look alike, so the question goes to the block's written slots
rather than to the shape of the parameter's type. Two shapes settle the rest: a
slot reached only inside a constructor (`impl From<Array<T>> for List<T>`)
leaves a concrete head to mangle and is kept, and a reference to a slot
(`From<&T>`) is the slot, peeled before the question is asked.

### One call, one resolution

The literal preselect runs before the callee is resolved, and keys it. Resolving
the parameter lists without the argument and mangling the name with it gives two
answers for one call, which is the disagreement this WEP exists to remove.
Folding sixteen lookups into one resolver does not prevent it, because two
_calls_ to that resolver disagree just as well.

A spelling several traits answer is reported for the same reason, and reported
before the overload an argument settles. Whether each trait declares a body or
leaves its default to answer makes no difference, because neither separates the
traits. Every site that mangles a name has to consume that report. One built its
name from the `Found` answer alone and fell back to a trait-less name, which
names a body nothing declares.

The report names each trait once, in the order the blocks were written. It
dedupes by declaration rather than by rendered name, because a trait's two
blocks need not be adjacent.

### A checked argument is a source change

An inherited default-bodied static used to answer with no parameter list, so its
arguments went unchecked. They are checked now. That is visible to anyone
calling such a static: `Instant::from_str("…")`, against a declaration taking
`&String`, has to be written `&"…"`. `lib/core/temporal_test.wado` moved with
it, to the spelling `int128_test.wado` and `primitive_test.wado` already used.

The tightening is the point rather than a side effect. An argument no list
describes is an argument nothing can reject. It is the one change here a caller
outside the compiler sees.

### Resolving is not free

Two rungs mutate elaboration state: reading a trait-frame signature at the
receiver resolves a type name in an inherited-type-param scope, and the
auto-derived `Default` records a synthesis request. Each runs inside the rung
that needs it, never eagerly for the caller's convenience — resolving the
receiver up front made a lookup mutate state on paths that never used it, and
`List<T>::with_capacity`, called from inside its own `impl`, stopped resolving.

## Roadmap

- [x] `resolve_static_callee` and its outcomes.
- [x] The identity question: `is_static_method_at`, `is_static_method`.
- [x] The signature questions: `lookup_static_method_type_params`,
      `static_callee_params`, `qualified_call_param_types`,
      `lookup_static_method_return_type`.
- [x] The selection: the three sites that located an impl and then looked its
      return type up separately now make one call, so the two cannot name
      different declarations.
- [ ] The blanket path. `find_blanket_static_method` and the blanket arm of
      `lookup_static_method_param_types_keyed` key on the blanket's receiver
      _parameter_, which no name written at a call site reaches. Folding them in
      means the resolution answers for a receiver it cannot key on directly.

Thirteen of the sixteen lookups are gone. The three that remain ask a different
question — the declaration's own frame, for a caller that will instantiate it,
and the blanket keyed on its receiver parameter — and they agree with each
other. Unifying them further is symmetry, not this WEP's decision.

## Known gaps

- A trait's static has no trait-qualified spelling. `Tagged::<V>::tag(5)` — the
  counterpart of Rust's `<V as Tagged>::A(5)` — answers `unknown function`, and
  an instance method's `Tagged::describe(&v)` works only because the receiver
  argument pins `Self`. So where a case shadows an inherited static, a bound
  (`fn f<T: Tagged>() { T::tag(5) }`) is the only way left to reach it. The
  spelling is missing on its own, not just under shadowing.
- The selection compares the parameter's type _name_ with the argument's, so
  two distinct types printing the same name are one candidate to it.
  [Overload Resolution](./wep-2026-07-31-overload-resolution.md) phase 4
  replaces that with `TypeId` matching; threading the argument's `TypeId` from
  the four sites that already hold it is what closing it takes.
- Only the first argument selects. Two impls a call separates only by its
  _second_ argument have no selection, and the first candidate wins. The
  preselect that shapes a literal reads the first argument for the same reason.
  No fixture drives it: closing it is the same `TypeId` migration above, over
  the argument list rather than one name.
- A trait-frame signature is read at the receiver, and where no caller supplies
  one the rung resolves the receiver by bare name in the caller's frame — which
  cannot name a namespace-imported type. A `Self`-returning static reached as
  `lib::P::twice()` then binds `Self` to nothing. Closing it means threading the
  receiver's `TypeId` from the site that already resolved it.
