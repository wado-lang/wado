# Resources Are Held Only by `with`

## Context

A function used to hold every resource its signature mentioned, without naming
it in `with`. The rule spared a function that handles a stream from writing
`with Stream` for a handle it was handed.

The rule looked inside struct fields, so it reached further than intended. A
method on any value holding a resource could call that resource's operations
with no `with`:

```wado
affine struct Wrap { f: Fields }

impl Wrap {
    fn touch(&self) {                    // no `with`
        let _ = self.f.set("x", [b"1"]);  // a host call that writes host state
    }
}
```

So a function without `with` could still call the host, and nothing said which
functions touch it (#2369). A contract clause had to reject any call on a
resource-holding value, even one reading a plain field. The `self_comparison`
lint had to treat such a call as possibly giving two answers.

## Decision

A function holds what its `with` clause declares and what that implies
([Implied Resources](./spec-effects.md#implied-resources)), and nothing else. A
closure holds nothing it captured. The rule is in
[Resources a Function Holds](./spec-effects.md#resources-a-function-holds).

Implied resources keep the cost low where it was meant to be low: `with Stdout`
still holds `Stream` and `StreamWritable`, so `println` needs nothing more. A
function handling a stream it was handed writes `with Stream` once.

Restricting the old rule to `&self`, or to resources named directly in the
signature, was rejected. A parameter `w: &Wrap` leaks the same way `&self`
does, and a resource parameter `f: &Fields` still calls the host with no
`with`.

Three bodies hold an effect they do not name:

- A method of a resource's own `impl R` holds `R`. It is one of `R`'s
  operations, and a call of it already demanded `R`.
- A handler method of `E` holds what `E` implies, since it builds what the
  operation hands back: a `Future` for `Stdout::write_via_stream`. It still
  declares `with E` where it delegates to the next handler out.
- A drop. A resource's drop, written or implicit, was already ambient.
  `Drop::drop` is declared `#[ambient(_)]`, and an impl inherits the attribute
  from its trait method. Generic code drops a `T` it cannot see into, so a drop
  declaring effects would reach every container. The attribute sits on the
  trait in the standard library, so an impl outside it raises no
  `undeclared_effects` warning.

`#[ambient(_)]` admits every effect, as `with _` stands for every row.

A resource implies every resource that extends it. A body used to hold a
narrowed resource only after a type pattern narrowed a held handle to it. A
closure holds nothing it captured, so a DOM event listener, typed
`fn mut(Event) with Dom`, could not call `value()` on an input element it
captured. The holder of an ancestor can narrow to any descendant, so the wider
grant adds no reach, and the narrowing rule is gone. In exchange, `with Element`
no longer says which kinds of element a function touches.

The grant runs downward only. A resource whose operations mention itself used
to imply its ancestors, so with the downward edge one child granted its whole
family, siblings included. An inherited method therefore demands the resource
of its receiver rather than of its declaration: `with Node` covers the methods
a `Node` inherits, and grants nothing on a sibling of `Node`.

## Known gaps

- An operation of an extern resource that carries no `#[cm(...)]` name has no
  host import, and a call to one crashes code generation. Whether such an
  operation is refused or gets an import name is undecided.
