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

|                             |                                                      |
| --------------------------- | ---------------------------------------------------- |
| `Found` with `params: Some` | one declaration, with lists read at the receiver     |
| `Found` with `params: None` | resolves, but no list this call can check against    |
| `Ambiguous`                 | several traits supply the name; no argument can pick |
| `NotStatic`                 | a variant case or a flags member owns the name       |

The lookups differed less on _where they looked_ than on what they did when they
did not fully find something, so each of these had to be named:

- A rung that cannot resolve falls through to the next. It is not a spelling
  that names nothing.
- A declaration whose slots the receiver has not filled resolves, and has a
  return type. Only its parameter list is unusable, so the optionality sits on
  the list — `params: Option<CalleeParams>` — and never hides the rest. An
  unfilled list is not offered in its place: it says the call takes a `T`, which
  no argument is.
- An overloaded name picks no declaration until an argument does, so
  `method_ref` is `Option` too. Its return type still answers where every
  candidate agrees: each `From` impl on a receiver returns it.
- An empty list is not "takes nothing".

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
- A trait impl's declaration is mangled with its trait. Taken as inherent it
  names a body nothing declares.
- An inherited default body has two modules — the block's, where the body is
  emitted and the call must point, and the trait's, where its defaults resolve.

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

- A variant case and an inherited trait static that share a name: the static
  claims the spelling before the variant arm is reached, so `V::A(5)` resolves
  to the trait's `A` and types as its return. The resolution is now the one
  place that decides this, but which should win is a language rule, not a
  refactor: a declaration on the type shadowing an inherited one would match
  what `impl_method_entries` already does for methods.
- `locate_static_method_impl` remains as the trait-selection rung, still
  matching a `From` impl's source type by rendered name. TypeId matching is its
  replacement ([Overload Resolution](./wep-2026-07-31-overload-resolution.md)
  phase 4).
