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
  and nobody declares it (`closure_escapes_effect_todo.wado`).
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
- A lazy adapter is generic over its closure's type (`IterMap<I, F>`, with
  `F: fn mut(I::Item) -> U with E`), so the adapter's type carries the
  closure's effects. Its `next` is `with _`, resolved from `I` and `F`.
- A function that calls the closure it takes before returning (`for_each`,
  `fold`, `sort_by`, `Option::map`, `Benchmark::run`) is generic over its
  effects, `<effect E>`.

## Roadmap

1. A type parameter bounded by a function type is callable wherever a value of
   it is: a local, a parameter, a field. An impl whose parameter only a bound
   names matches the types it should.
2. A closure literal's type carries the effects inferred from its body, and an
   effect parameter is resolved from them. The body is still checked against
   its enclosing function, so nothing that compiles today stops compiling.
3. `Iterator` and `IntoIterator` are open, the lazy adapters are generic over
   their closure's type, and the eager functions taking a closure declare an
   effect parameter. An iterator whose `next` performs I/O, and an effectful
   closure through `map`, both compile and demand their effects of the caller.
4. A closure body is checked against the effects its type carries, and an
   expected type without `with` admits none. `closure_escapes_effect_todo.wado`
   is rejected as it states.
5. The specification states the rules above, and no longer says every standard
   library trait is `with ()`.

## Known gaps

None yet.
