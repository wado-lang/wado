# Static Call Resolution — One Walk, One Answer

## Context

`Type::method(...)` is written five ways: bare, `ns::Type::method()`,
`Type::<A>::method()`, `Self::method()` and `T::method()` through a bound. Every
one of them has to answer the same questions: is this a static at all, which
declaration does it name, what are its parameters, what does it return, what
type parameters does it take.

Many lookups answered those questions, each walking its own part of one ladder
in its own order. The rungs were the same: the receiver's own declarations, its
resource statics and their chain, a trait impl declaring the method, one
inheriting the trait's default body, a derived trait, the newtype base. What
differed was where each stopped, what it did on a miss, and which receiver it
started from.

So two of them could disagree about one call. The spelling resolved and its
signature did not, and the call was compiled with no parameter list: an
unchecked arity, an unpadded default, a call naming a body without its trait.
Fixing the instances one at a time did not converge.
[Declaration Identity](./wep-2026-08-12-declaration-identity.md) already says
one identity, one scope, one answer.

## Decision

One resolution answers every spelling, and every site that needs a fact about
the call reads it from that answer.

### The receiver is one vantage

The receiver is the declaration the call's own reference site resolved. A
receiver re-derived from the bare name in the caller's frame answers another
question: the importing module may not name `Type`, and a primitive's
`impl FromStr for f32` is out of reach from a name.

The same holds one link down. A newtype reaches its base through its own
declaration, never by the base's name, which a namespaced `lib::Q::twice()`
never imported. The call names the body of the link the resolution answered
with. Under the spelled `Q` it names a body that does not exist.

It holds for the inference too. `ns::Type::method()` infers the receiver's type
arguments, and reports the ones it cannot fill, exactly as `Type::method()`
does.

### A qualified spelling names a trait

`Tagged::<V>::tag(5)` asks for `Tagged`'s declaration, so only its impls are
candidates. The receiver's own declaration of the name is a different method,
and a case or flags member `V` declares builds a value rather than answering the
trait. The shadowing rule below applies to a bare `V::tag`, which is what the
turbofish rewrites to once it has supplied `Self`.

Only where the trait declares no parameters of its own. On one that does, the
turbofish is already that trait's argument list: `Take::<i64>::take(&f, 42)`
pins the list and the receiver argument supplies `Self`. A static has no such
argument, and the specification's answer for it is not a second turbofish. The
receiver type is written out and the call's arguments select the impl:
`M::make(A {})` against `impl Enc<A> for M` beside `impl Enc<B> for M`
(`docs/spec-traits.md`, "A trait's associated function"). So
`Take::<i64>::take()` names no receiver at all, and is reported as that rather
than as an unknown function.

A namespace qualifies the head, not the argument list.
`ns::Take::<i64>::take(…)` asks the same questions of the same declaration as
`Take::<i64>::take(…)`. The head is read where it was written, so a bare
spelling and a namespaced one reach the same answer.

### What the resolution answers

|                 |                                                                      |
| --------------- | -------------------------------------------------------------------- |
| one declaration | with its parameter lists read at the receiver                        |
| an overload     | one trait, several impls; the arguments had nothing to separate      |
| an ambiguity    | several traits supply the name; no argument can pick                 |
| not a static    | a variant case or a flags member owns the name                       |
| out of scope    | only traits the calling module has not imported supply the name      |
| tied            | impls of one trait that each leave a slot to the call reach it alike |

Each answer means one thing:

- A declaration whose slots the receiver has not filled resolves, and has a
  return type. Only the _types_ wait on the receiver. How many parameters there
  are, what they are called and which carry defaults are the declaration's own,
  so the arity is enforced and the defaults padded either way. A slot the call
  could not fill is skipped where its argument is checked; the list is never
  dropped.
- A name several declarations answer to picks none until an argument does.
  That is not one declaration that no trait names: the first has no body to
  name, the second names a body without a trait.
- An overload's return type still answers where every candidate agrees: each
  `From` impl on a receiver returns it.
- An empty list is not "takes nothing", and a partial answer is not a whole one.
  A site that reads only the first answer and defaults the rest turns every
  other into a name nothing declares.

### One list of candidates, one pass of rules

The rungs produce candidates, and a single pass over them decides. A candidate
is one declaration a block on the receiver supplies for the name. It carries
what the rules read: the trait it comes through, the block, the declaration,
whether it takes a receiver, whether the block wrote the body or inherited it,
and the parameter each argument is checked against. It carries no decision, so
a rung added later cannot miss a rule.

The receiver's own declarations are candidates too: its inherent impl, the
statics a `resource` declares, and one it inherits along that chain.

The rules run in this order, and the order is the design:

1. The receiver's own declaration shadows the inherited candidates of its kind.
2. A receiver-less declaration answers before a receiver-taking one, so
   `Type::method(x)` is a static's call before it is a UFCS receiver.
3. Where several traits supply the name, the spelling names none of them, and
   that is reported.
4. The arguments pick among what is left.
5. The earlier origin outranks: the receiver's own, then a written body, then an
   inherited one, among what the arguments admit.

What remains is one declaration, or several of one trait that no argument
separated: the overload.

Steps 4 and 5 are in that order because a written body the argument rejects is
not an answer. `impl Conv<i32> for M {}` beside
`impl Conv<String> for M { fn make(…) }` answers `M::make(5)` from the inherited
default, though a block wrote a body for the other argument.

An own candidate is exempt from step 4. The arguments choose among _impls_, and
a declaration the receiver makes itself has none to be chosen against. The call
site reports an argument mismatch against the one declaration the spelling
names.

### Rules about what the candidates are

- Shadowing holds within a kind, and dot syntax and the resolution ask one rule
  for it.
- A `variant` case, an `enum` case and a `flags` member shadow an inherited
  static of the same name: they are written on the type, and the static is only
  inherited. This matches Rust, where `V::A(5)` is the case and
  `<V as Tagged>::A(5)` reaches the trait's. It does not extend to an inherent
  static of the same name, which is a type declaring one name twice.
- A trait impl's declaration is named with its trait. Taken as inherent it
  names a body nothing declares.
- An inherited default body has two modules: the block's, where the body is
  emitted and the call points, and the trait's, where its defaults resolve.

### The argument picks a declaration, not a trait

A static call is selected by the parameters the impl declares, each compared
against the argument written for it. The comparison is by type identity, so two
distinct types that print one name are two candidates.

A parameter still holding slots is compared by shape: the slots are solved from
the argument, and what that makes of the parameter is compared. Neither half is
a name. A rendered name spells a function type's own parameters, so
`fn(T) -> i32` never matches `fn(i32) -> i32`, and it drops a generic's
arguments, so `Holder<T, T>` matches a `Holder<i32, String>` that no `T` makes.

`From<T>` is no special case. Its source type is also its trait argument and
`from` takes one argument, but a trait implemented twice on one receiver
(`impl Conv<A> for M` beside `impl Conv<B> for M`) poses the same question at
any arity, and the declared parameter is what answers it. The parameter is
resolved in the impl's own frame, so both sides of the comparison are
canonical.

The trait segment of the name a call is compiled under keeps the trait's
arguments. Dropping them collapses two declarations of one method name onto one
body. What decides this is how many times the receiver implements the trait,
never which trait it is. It holds for a body the block inherits as much as one
it writes: the segment comes from the block's own trait reference, since a
trait declaration has no arguments to give.

A parameter the block fills is a blanket, not a mismatch. Its unsubstituted
spelling must not be named, so the candidate declines it and the blanket is
instantiated instead.

Three things fill a slot, and only one of them is the argument. The receiver
fills a slot it mentions, so `impl<T> Make<T> for Wrap<T>` is no blanket to
`Wrap::<i32>::make`. The call fills a slot the _method_ declares, so a block's
own `fn build<T>(v: T)` is not one either. What is left is a block slot the
receiver never names, as in `impl<T: Display> From<T> for ByAny`, and only that
is a blanket. Two shapes settle the rest: a slot reached only inside a
constructor (`impl From<Array<T>> for List<T>`) leaves a concrete head to name
and is kept, and a reference to a slot (`From<&T>`) is the slot, peeled before
the question is asked.

### The spelling names both kinds

`Type::method(recv, …)` reaches a trait's instance method as well as its static,
passing the receiver first. Statics answer first, so a receiver-less declaration
of the name still wins.

An inherent method shadows a trait's for this spelling, as it does for dot
syntax: `Q::tag(&q)` names `impl Q`'s and the trait's is spelled `Tag::tag(&q)`.
Shadowing holds within a kind, so an inherent method leaves a trait's associated
function of the same name alone.

A receiver-taking declaration has the receiver at argument zero, so its own
parameters begin at argument one. Reading from argument zero for both kinds
compares a receiver against the parameter two impls differ on, which separates
nothing and admits nothing.

### The owning link is decided before the arguments

On a chain of newtypes, the call belongs to the first link, from the spelled
head down, that offers anything for the name: a declaration its impls reach, a
trait default its impl leaves to answer, a case it declares, or a derivation. A
newtype derives nothing itself, so a derivation is offered at the link it is
written for. The owner is found without the arguments, and the arguments then
select among that link's candidates only.

A link offers what is written for it, so two kinds of answer do not count. A
blanket reaches every link alike and so separates none: a link that only a
blanket answers passes the name down. An impl on another instance of a link
answers nothing for this one. The instance is the one the call spells, or the
one the chain declares where the head is spelled bare, never one the arguments
would infer.

The arguments cannot decide it, because they are typed against the owner. A
site that resolves the parameter list first, to give each argument its expected
type, needs the owner before any argument exists, so choosing the link by
whether its candidates admit the arguments is circular. Every site that needs
the owner, early or late, reads it from this one walk.

The rule rejects no call that compiled. Where a link's candidates reject the
arguments, the arguments were already typed against that link and the mismatch
reported, so the link below never answered.

### The receiver fills the declaring slots, once

A declaration's declaring slots, an `impl` block's, a `resource`'s or an
`interface`'s, are filled by the receiver's type _arguments_. A block aligns
them, since its head may reorder or fix some (`impl … for TreeMap<String, V>`
numbers only `V`); a resource numbers its own by position.

Never the receiver type itself. Only a trait's frame leads with `Self`, and that
frame is read where an inherited body is. Binding slot zero to the receiver
outside it made `Stream::<u8>::new()` return a `StreamWritable<Stream<u8>>`,
because `Stream`'s slot zero is its `T`.

The resolution fills them, so the call site does not. It substitutes the
method's own slots alone, the ones the resolution leaves open for the call to
solve. Filling the declaring slots a second time from the spelled turbofish is
two answers for one call.

The arguments are the fact, not the type that holds them. A receiver spelled as
a bare name has a list and no type to read it off: `Counter::make()` infers the
list at the call, and `Counter<T>` resolved from the name would answer with its
own parameters. So the resolution takes the arguments as a fact of their own,
and derives them from the receiver type only where the call brings none.

### A concrete block is named where it is written

A block written for one instantiation hosts its own function, in its own module
and under the head it wrote. `impl Wrap<i32> for Cell<i32>` emits
`Cell<i32>::wrap` beside the block. A generic block has no such head:
monomorphization materialises each instance in the receiver's module, under the
receiver's own name.

So a call names the block it resolved to, not the receiver it spelled.
`Cell::wrap(7)` picks the concrete block by its argument, and then names
`Cell<i32>` in that block's module. Naming the receiver for both puts the call
and the body in different modules, and the call's name carries no arguments
where the body's does.

This is what the receiver's type arguments are inferred _for_. The turbofish is
not the only way to write them: the arguments select the block, and the block's
head says what the receiver is.

### Every parameter a block declares is a slot of that block

A block's slots are numbered by the position each takes in the receiver's
arguments. A parameter the receiver never mentions, as in
`impl<T: Display> From<T> for ByAny`, is numbered after the ones it does. Only
an argument can fill it, which is what makes the block a blanket, but a slot is
what it is either way, so `fn from(v: T)` has a type. Whether only an argument
can fill a parameter is read off its slot.

The block's own parameters bind before the trait's, since `From<T>`'s is also
spelled `T`. Binding the trait's first claims that name for an argument nothing
has resolved yet, and the block's own slot is never made.

Every entry in `impl<...>` declares a parameter, as in Rust. A concrete
instantiation names its type in the trait or receiver instead:
`impl IndexValue<i32> for Box`.

### A blanket is a candidate, ranked last

A value blanket (`impl<T: Bound> Trait for T`) covers a receiver through its
bound rather than naming it, so no name at a call site reaches its statics. It
is a candidate all the same: what a rung produces is facts, and which of several
answers is the rules' to decide.

It ranks after everything written for the receiver itself, so a concrete impl
wins by the rule rather than by which path ran first. Ranking happens before the
ambiguity report, or a blanket the receiver's own declaration outranks would be
named as an alternative to it. Two traits blanketing one receiver supply the
name and separate nothing, which is the same ambiguity two concrete impls raise.
A blanket tried only where nothing else answered would take the first that
applies instead.

Selecting a blanket is the rules'; instantiating one is not. Its template is
written against the receiver parameter, so the blanket is instantiated where it
is named, and the candidate builds no callee.

### An inherited body is read in the trait's own frame

The trait's frame numbers `Self` as slot 0 and the trait's own parameters after
it, so a block reads a default body back by supplying its target and then its
trait arguments. Supplying the target alone leaves the rest open: every argument
reaches every block, `M::tag("x")` and `M::tag(5)` select whichever block came
first, and the call then names the other one's body.

So an inherited candidate's parameter is the one the block's trait arguments
produce, not the one the trait declared.

### One call, one resolution

The literal preselect runs before the callee is resolved, and keys it. Resolving
the parameter lists without the argument and naming the body with it gives two
answers for one call. One resolver does not prevent that by itself, because two
_calls_ to it disagree just as well.

So the resolution hands back everything a site needs from it: the declaration it
picked, what the call returns, and the lists to check and pad against. A site
that names the body from one answer and asks again for the other is the shape to
look for, and the second ask is the one that goes wrong: it is keyed by a bare
name in the caller's frame, which an alias or a namespace leaves without that
name. The facts that identify a receiver travel as one value for the same
reason.

The preselect reads the whole argument list, each argument against the parameter
written for it, as the selection does. Reading argument zero alone shapes the
first literal from the impl it picked and leaves the rest to their own defaults,
so `M::make(0, 5)` types `5` as `i32` and matches no `Conv<i64>` impl. An
argument whose type synthesis cannot produce admits every parameter, so one such
argument among others does not stop the rest from deciding.

A spelling several traits answer is reported for the same reason, and before the
overload an argument settles. Whether each trait declares a body or leaves its
default to answer makes no difference, because neither separates the traits.
Every site that names a body consumes that report, or it names a trait-less body
nothing declares.

The report names each trait once, in the order the blocks were written. It
dedupes by declaration rather than by rendered name, because a trait's two
blocks need not be adjacent.

### A static's own slots are solved, not spelled

A method's own type parameters are inferred from the arguments, as instance
dispatch infers them. Where the block declares no slots of its own, the method's
are numbered from zero and the receiver's substitution reaches them anyway.
Where it declares some, they are solved at the call and the signature
re-instantiated with what came back.

The solve runs after the arguments are elaborated, because it reads them, and a
literal is re-coerced afterwards against the parameter the solve produced. A
slot the arguments do not pin is reported, with the spelling
(`Box3::<i32>::build::<i32>(42)`) named: an unsolved slot must not reach
codegen.

### A checked argument is a source change

An inherited default-bodied static checks its arguments against the trait's
parameter list. Callers that relied on them going unchecked see a break:
`Instant::from_str("…")` against a declaration taking `&String` is written
`&"…"`.

An argument no list describes is an argument nothing can reject, so the
tightening is the point rather than a side effect. It is the one change here a
caller outside the compiler sees.

### Resolving runs only where its answer is used

Two rungs change compiler state: reading a trait-frame signature at the receiver
resolves a type name in an inherited-type-param scope, and the auto-derived
`Default` records a synthesis request. Each runs inside the rung that needs it,
never eagerly for a caller's convenience. Resolving the receiver up front
changes state on paths that never use it, and stops `List<T>::with_capacity`,
called from inside its own `impl`, from resolving.

A site that needs the owning link before the arguments walks the chain only on
a newtype receiver, and reads the rungs the call's own resolution then reads.

### Every name reads one answer

No lookup walks a ladder of its own. It reads the resolution's answer, or asks
one rung through the walk the resolution uses. Instantiating a blanket is the
exception: it walks the blanket's own rungs, but to instantiate what the rules
picked rather than to pick it.

## Known gaps

- A static call through an associated type is not one of the spellings the
  walk admits. With `T: Has` and `Has` declaring `type Out: Make`,
  `T::Out::make()` is rejected as `unknown function 'T::Out::make'`, in a
  generic function as in a default body, although the bound names the trait
  that declares `make`.

- Whether a spelling is a static at all is decided by a full resolution, which
  the call then runs again. It runs once per static call, never in a loop, and
  nothing here measures what the repeat costs.

- The owning link is walked again by each site that needs it on one call: the
  parameter list, the callee and the inherited retype each ask. Nothing here
  measures what the repeat costs.
