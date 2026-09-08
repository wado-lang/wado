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

The same holds one rung down. A newtype reaches its base's impls, and looking the
base up by its bare name asks the caller's frame for a name a namespaced
`lib::Q::twice()` never imported. The rung reads the alias's declaration instead,
and the call is mangled under the base the resolution answered with — mangling
under the spelled `Q` names an impl WIR cannot find.

It holds for the inference too. `ns::Type::method()` infers the receiver's type
arguments from the key its site resolved, and reports the slots it could not
fill, exactly as `Type::method()` does. The namespaced spelling asked neither
question while it read only a written turbofish, so it mangled a name with no
arguments and said nothing where none could be inferred.

### A qualified spelling names a trait

`Tagged::<V>::tag(5)` asks for `Tagged`'s declaration, so only its impls are
candidates. The receiver's own declaration of the name is a different method, and
a case or flags member `V` declares builds a value rather than answering the
trait — the shadowing rule below applies to a bare `V::tag`, which is what the
turbofish rewrites to once it has supplied `Self`.

Only where the trait declares no parameters of its own. On one that does, the
turbofish is already that trait's argument list — `Take::<i64>::take(&f, 42)`
pins the list and the receiver argument supplies `Self`. A static has no such
argument, and the spec's answer for it is not a second turbofish: the receiver
type is written out and the call's arguments select the impl, `M::make(A {})`
against `impl Enc<A> for M` beside `impl Enc<B> for M` (`docs/spec.md`, "A
trait's associated function"). So `Take::<i64>::take()` names no receiver at
all, and is reported as that rather than as an unknown function.

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

### One list of candidates, one pass of rules

The rungs produce candidates; a single pass over them decides. A candidate is
one declaration a block on the receiver supplies for the name, carrying what the
rules read — the trait it comes through, the block, the declaration, whether it
takes a receiver, whether the block wrote the body or inherited it, and the
parameter the call's argument is checked against. It carries no decision.

Rungs written one at a time acquire the rules that were salient the day each was
written, and nothing makes a later one acquire the rest. Every defect this
section exists to prevent was that: the receiver-taking rung reported no
ambiguity where both other paths did, compared the receiver against the
parameter the impls differ on, and had no answer at all for a body its block
inherited. Each was a rule the other rungs already had. Adding a rung is adding
a candidate producer, which cannot miss a rule because it applies none.

The receiver's own declarations are candidates too — its inherent impl, the
statics a `resource` declares, and one it inherits along that chain — carrying
the origin `Own`. Their precedence was the order three early returns happened to
sit in; it is a rule the pass applies now.

The rules run in this order, and the order is the design:

1. The receiver's own declaration shadows the inherited candidates of its kind.
2. A receiver-less declaration answers before a receiver-taking one, so
   `Type::method(x)` is a static's call before it is a UFCS receiver.
3. Where several traits supply the name, the spelling names none of them, and
   that is reported.
4. The arguments pick among what is left.
5. The earlier origin outranks: the receiver's own, then a written body, then an
   inherited one — among what the arguments admit.

What remains is one declaration, or several of one trait that no argument
separated — the overload.

Steps 4 and 5 are in that order because a written body the argument rejects is
not an answer: `impl Conv<i32> for M {}` beside `impl Conv<String> for M { fn
make(…) }` answers `M::make(5)` from the inherited default, though a block wrote
a body for the other argument.

An own candidate is exempt from step 4. The arguments choose among _impls_, and
a declaration the receiver makes itself has none to be chosen against — reading
them there drops it on a mismatch, where the call site has an argument type
error to report against the one declaration the spelling names.

### A rule kept in structure has to be restated as data

Choosing which lookup to call _was_ the rule. Which of the sixteen a site called
was the shadowing rule; `impl_method_entries`' ordering was the qualifier that
shadowing holds only within a kind; `locate_static_method_impl` returning the
impl's module while `TraitSig` holds the trait's was how a call got the right one
of the two. None of that survives the merge on its own.

Shadowing is now `inherent_shadows`, which both the dot-syntax walk and the
resolution ask, so neither can hold a version of its own. The rest the
resolution states:

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

A static call is selected by the parameters the impl declares, each compared
against the argument written for it. The comparison is by `TypeId`, so two
distinct types that print one name are two candidates.

A parameter still holding slots is compared by shape: the slots are solved from
the argument and what that makes of the parameter is compared. Neither half of
this is a name. A rendered name spells a function type's own parameters, so
`fn(T) -> i32` never matches `fn(i32) -> i32`, and it drops a generic's
arguments, so `Holder<T, T>` matches a `Holder<i32, String>` that no `T` makes.

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
to the blanket resolver, which instantiates it.

Three things fill a slot, and only one of them is the argument. The receiver
fills a slot it mentions, so `impl<T> Make<T> for Wrap<T>` is no blanket to
`Wrap::<i32>::make`. The call fills a slot the _method_ declares, so a block's
own `fn build<T>(v: T)` is not one either — `declaring_slot_count` is where the
block's slots end and the method's begin. What is left is a block slot the
receiver never names, as in `impl<T: Display> From<T> for ByAny`, and only that
is a blanket. Two shapes settle the rest: a slot reached only inside a
constructor (`impl From<Array<T>> for List<T>`) leaves a concrete head to mangle
and is kept, and a reference to a slot (`From<&T>`) is the slot, peeled before
the question is asked.

### The spelling names both kinds

`Type::method(recv, …)` reaches a trait's instance method as well as its static,
passing the receiver first. Statics answer first, so a receiver-less declaration
of the name still wins.

An inherent method shadows a trait's for this spelling, as it does for dot
syntax: `Q::tag(&q)` names `impl Q`'s and the trait's is spelled `Tag::tag(&q)`.
Shadowing holds within a kind, so an inherent method leaves a trait's associated
function of the same name alone — the rule the receiver's own declarations
already follow above.

Where the selection starts reading follows from the kind rather than from the
call: a receiver-taking declaration has the receiver at argument zero, so its
own parameters begin at argument one. Reading from argument zero for both kinds
compares a receiver against the parameter two impls differ on, which separates
nothing and admits nothing.

### The receiver fills the declaring slots, once

A declaration's declaring slots — an `impl` block's, a `resource`'s, an
`interface`'s — are filled by the receiver's type _arguments_. A block aligns
them, since its head may reorder or fix some (`impl … for TreeMap<String, V>`
numbers only `V`); a resource numbers its own by position.

Never the receiver type itself. Only a trait's frame leads with `Self`, and that
frame is read where an inherited body is. Binding slot zero to the receiver
outside it made `Stream::<u8>::new()` return a `StreamWritable<Stream<u8>>`,
because `Stream`'s slot zero is its `T`.

The resolution fills them, so the call site does not. It substitutes the
method's own slots alone — the ones the resolution leaves open for the call to
solve. Filling the declaring slots a second time from the spelled turbofish is
two answers for one call, and it is why no rung could be given the receiver's
type until this was one answer.

The arguments are the fact, not the type that holds them. A receiver spelled as
a bare name has a list and no type to read it off: `Counter::make()` infers the
list at the call, and `Counter<T>` resolved from the name would answer with its
own parameters. So the query carries the arguments, and derives them from the
receiver type only where the call brings none.

### A concrete block is named where it is written

A block written for one instantiation hosts its own function, in its own module
and under the head it wrote. `impl Wrap<i32> for Cell<i32>` emits
`Cell<i32>::wrap` beside the block. A generic block has no such head:
monomorphization materialises each instance in the receiver's module, under the
receiver's own name.

So a call names the block it resolved to, not the receiver it spelled.
`Cell::wrap(7)` picks the concrete block by its argument, and then names
`Cell<i32>` in that block's module. Naming the receiver for both put the call and
the body in different modules, and the mangled name carried no arguments where
the body's did.

This is what the receiver's type arguments are inferred _for_. The turbofish is
not the only way to write them: the arguments select the block, and the block's
head says what the receiver is.

### Every parameter a block declares is a slot of that block

`bind_declared_target_params` numbers a block's slots by the position each takes
in the receiver's arguments. A parameter the receiver never mentions —
`impl<T: Display> From<T> for ByAny` — has no such position, and was left
unbound, so `fn from(v: T)` resolved to no type at all. It is numbered after the
ones the receiver does mention. Only an argument can fill it, which is what
makes the block a blanket, but a slot is what it is either way.

Two spellings decide where the binding goes and what it skips. It runs before
the trait's parameters are bound, since `From<T>`'s is also spelled `T`: binding
the trait's first claims that name for an argument nothing has resolved yet, and
the block's own slot never gets made. And a parameter position may hold a
concrete type — `impl<i32> IndexValue<i32> for Box` — which is not a parameter
and must not be bound, or the slot shadows the type it is named for and the
diagnostic reads `expected 'i32', found 'i32'`.

With the slot real, "only an argument can fill this parameter" is read off the
slot's index. Reading it off an _absent_ type meant every unresolved parameter
in a block with such a slot answered the same way.

### A blanket is a candidate, ranked last

A value blanket (`impl<T: Bound> Trait for T`) covers a receiver through its
bound rather than naming it, and its statics are indexed under the receiver
_parameter_, which no name at a call site reaches. It is a candidate all the
same: what a rung produces is facts, and which of several answers is the rules'
to decide.

It ranks after everything written for the receiver itself, so a concrete impl
wins by the rule rather than by which path ran first. Ranking happens before the
ambiguity report, or a blanket the receiver's own declaration outranks would be
named as an alternative to it.

Two traits blanketing one receiver supply the name and separate nothing, so that
is the same ambiguity two concrete impls raise. Running the blanket path as a
fallback — only where the resolution answered nothing — is what let it take the
first applicable blanket instead.

Selecting a blanket is the rules'; instantiating one is not. The template is
written against the receiver parameter, so the blanket resolver reads it and
mangles it, and the candidate declines to build a callee. That is what
`Selector::Blanket` already says for a blanket parameter.

### An inherited body is read in the trait's own frame

The trait's frame numbers `Self` as slot 0 and the trait's own parameters after
it, so a block reads a default body back by supplying its target and then its
trait arguments. Supplying the target alone leaves the rest open: every argument
reaches every block, `M::tag("x")` and `M::tag(5)` select whichever block came
first, and the mangled name then points at the other one's body.

So an inherited candidate's parameter is the one the block's trait arguments
produce, not the one the trait declared.

### One call, one resolution

The literal preselect runs before the callee is resolved, and keys it. Resolving
the parameter lists without the argument and mangling the name with it gives two
answers for one call, which is the disagreement this WEP exists to remove.
Folding sixteen lookups into one resolver does not prevent it, because two
_calls_ to that resolver disagree just as well.

So the resolution hands back everything a site needs from it: the declaration it
picked, what the call returns, and the lists to check and pad against. A site
that mangles from one and asks again for the other is the shape to look for, and
the second ask is the one that goes wrong — it is keyed by a bare name in the
caller's frame, which an alias or a namespace leaves without that name. The
facts that identify a receiver travel as one value for the same reason: a walk
given four of them separately is a walk somebody will call with three.

The preselect reads the whole argument list, each argument against the parameter
written for it, as the selection does. Reading argument zero alone shaped the
first literal from the impl it picked and left the rest to their own defaults, so
`M::make(0, 5)` typed `5` as `i32` and matched no `Conv<i64>` impl. An argument
whose type synthesis cannot produce admits every parameter, so one such argument
among others does not stop the rest from deciding.

A spelling several traits answer is reported for the same reason, and reported
before the overload an argument settles. Whether each trait declares a body or
leaves its default to answer makes no difference, because neither separates the
traits. Every site that mangles a name has to consume that report. One built its
name from the `Found` answer alone and fell back to a trait-less name, which
names a body nothing declares.

The report names each trait once, in the order the blocks were written. It
dedupes by declaration rather than by rendered name, because a trait's two
blocks need not be adjacent.

### A static's own slots are solved, not spelled

A method's own type parameters are inferred from the arguments, as instance
dispatch infers them. Where the block declares no slots of its own the method's
are numbered from zero and the receiver's substitution reaches them anyway;
where it declares some it does not, so they are solved at the call and the
signature re-instantiated with what came back.

The solve runs after the arguments are elaborated, because it reads them, and a
literal is re-coerced afterwards against the parameter the solve produced. A
slot the arguments do not pin is still reported, with the spelling
(`Box3::<i32>::build::<i32>(42)`) named — an unsolved slot reaching codegen is
what that diagnostic exists to prevent.

### A checked argument is a source change

An inherited default-bodied static checks its arguments, because it now answers
with the trait's parameter list. It previously answered with none, so those
arguments went unchecked, and callers written against that see a break:
`Instant::from_str("…")` against a declaration taking `&String` has to be
written `&"…"`. `lib/core/temporal_test.wado` moved with it, to the spelling
`int128_test.wado` and `primitive_test.wado` already used.

An argument no list describes is an argument nothing can reject, so the
tightening is the point rather than a side effect. It is the one change here a
caller outside the compiler sees.

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
- [x] The blanket path's selection: a value blanket is a candidate the rules
      rank, and every spelling that reaches one asks the resolution first.

Five of the sixteen names are gone outright. The rest no longer walk a ladder of
their own: each reads the resolution's answer, or asks one rung through the
shared `impl_method_entries` walk. `find_blanket_static_method` and
`lookup_static_method_param_types_keyed` still walk the blanket's own rungs, but
they instantiate what the rules picked rather than picking it.

## Known gaps

- The identity question resolves twice. `is_static_method_at` runs the whole
  walk and keeps only whether it answered, and the branch it guards then runs it
  again. Asking the resolution rather than a second index is this WEP's point,
  so the fix is to remember the answer, not to look it up another way. It has
  one caller and runs once per static call, never in a loop, and nothing here
  measures what the repeat costs.
