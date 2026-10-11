# Effects Through Closures and Iterators

## Context

A closure's type carries the effects a call to it may perform
([Closure Types](./spec-functions.md#closure-types)), and an iterator may
perform effects as it advances: a `next` that reads the next line of a stream
does I/O. The compiler holds neither.

- A closure literal's type is built with no effects, at annotation
  (`elaborator/closure.rs`) and again at reify. Its body is checked against the
  effects its expected type declares, plus every effect of the function it is
  written in.
- So a closure passed where a `fn()` is expected performs what its writer holds,
  and nobody declares it (`closure_escapes_effect_todo.wado`, #1087).
- An effect parameter is inferred from the function-typed arguments' effects
  (`resolve_effect_params`). A closure's type carries none, so `E` in
  `wrapper(|| println("hi"))` resolves to nothing. The call only works because
  the body takes `Stdout` from the function around it.
- `Iterator` says `with ()` and the specification says every standard library
  trait does. The lazy adapters store their closure as `fn mut(I::Item) -> U`,
  with no effects. `map(|x| { println(x); … })` works by the same leak, and an
  iterator whose `next` does I/O cannot be written.

The standard library's other closure parameters (`for_each`, `fold`,
`sort_by`, `Option::map`, …) declare no effects either, and rely on the same
leak.

## Decision

- A closure literal's type carries the effects its body performs, inferred from
  the body, as the specification states. An expected function type bounds them:
  a closure performing an effect where a `fn()` without `with` is expected is an
  error, as for a function.
- An effect parameter at a call is the union of what its closure arguments
  carry.
- `Iterator` and `IntoIterator` have an open head, `with _`. Each impl brings
  its own effects, and a pure one demands nothing of its callers.
- A method of a trait whose head is not `with ()` may write `with ()` to be
  pure in every impl. This follows from a method's clause overriding the head:
  under an open head, writing no clause means `with _`, so `with ()` is the one
  way to say pure. The default methods that only build an adapter (`map`,
  `filter`, `zip`, …) say it, so building one demands nothing.
- A lazy adapter is generic over its closure's type (`IterMap<I, F>`, with
  `F: fn mut(I::Item) -> U with E`), so the adapter's type carries the
  closure's effects. Its `next` is `with _`, resolved from `I` and `F`.
- A function that calls the closure it takes before returning (`for_each`,
  `fold`, `sort_by`, `Option::map`) is generic over its effects,
  `<effect E>`, as `Benchmark::run` already is.

### Mechanism

The elaborator cannot work out what a closure body performs: that needs every
call in it resolved, which is the effect check's work, and the effect check runs
on the whole program after reify. So the elaborator leaves a variable where an
effect row is not yet known, and the effect check solves it.

- An effect variable is one more kind of `EffectRef`, minted by the type table.
  A call gives each of its callee's own effect parameters a fresh one in the
  return type it produces, in the callee's frame before the type arguments
  fill it. An effect parameter of the enclosing impl is the receiver's row at
  the place the impl's target writes it, as an impl's type parameter is the
  receiver's argument. A closure literal's type takes a fresh one for its row.
- The type check, impl target matching and the trait solver all let a row
  holding an effect parameter or a variable meet any row.
- The effect check solves the variables, walking a body until no solution
  changes: an outer call's arguments are read before the inner call that
  minted their variable. A call's variable is what its parameter resolved to
  there, read off by setting the callee's declared return type beside the
  call's type. A closure's is what its body performs.
- Effects carry no representation: a function value is one whatever its row.
  So a type's own arguments are spelled and interned without rows, and two
  instances that differ only there are one struct. A function type standing
  alone keeps its row, which selects an impl written for it, and so does a
  function instance's type argument. After the check, a type holding a
  variable is redirected to the type with it solved, or emptied where nothing
  solved it, as newtype erasure redirects a newtype to its base.

## Roadmap

1. [x] A type parameter bounded by a function type is that signature in an
   impl as in a function: callable wherever a value of it is, a field among
   them. The impl matches a target whose argument is a closure of that
   signature, solving the parameters only the signature names, and an
   associated type naming one of them is what the target binds at the
   receiver. That last held of no impl parameter away from its declaration
   position, a nested one included.
2. [x] An effect parameter is resolved from the effects a closure argument's
   body performs, inferred where the call is checked. The body is still checked
   against its enclosing function, so nothing that compiles today stops
   compiling.
3. [x] `Iterator` and `IntoIterator` are open. An iterator whose `next` performs
   I/O compiles, and a loop over it, directly or through an adapter, demands
   its effects of the caller.
4. [x] The eager functions taking a closure declare an effect parameter, and a
   function value declaring effects passes to one.
5. [x] An effect parameter a call resolves is substituted in the types the call
   produces, as a type parameter is. The lazy adapters are generic over their
   closure's type, so an adapter built from a function value carries its
   effects.
6. [ ] A closure literal's type carries the effects inferred from its body. The
   body is checked against them, and an expected type without `with` admits
   none. An effectful closure through `map` demands its effects of whoever
   advances the adapter. `closure_escapes_effect_todo.wado` is rejected as it
   states.
7. [ ] The specification states the rules above.

## Known gaps

None yet.
