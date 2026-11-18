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

An effect guards operations, not handles. Holding a handle is the authority
over which values a function touches; `with` says which host operations it may
perform, and a handler intercepts exactly those. The checker and the dispatch
must therefore agree on whose operation a call is, and the dispatch has one
answer: the resource declaring the operation, whose handler the call reaches.

So an operation demands the resource declaring it, whatever resource the
receiver is. An inherited method demanding its receiver's resource let a
handler of `Node` discharge `n.tag()`, which `EventTarget` declares. The call
went past that handler to `EventTarget`'s handler or the host, from a function
that declared nothing.

How much one `with` grants is a separate choice, of granularity rather than
safety. A family under `extends` is one effect: `with Node` holds `EventTarget`,
every element and every sibling. A DOM is one capability in practice. A
listener typed `fn mut(Event) with Dom` calls `value()` on an input it narrowed
from a captured element, and a framework typing its callbacks `with Dom` cannot
list every element its users narrow to. The cost is that `with Element` no
longer says which kinds of node a function touches, and a family cannot split
authority between, say, a read-only handle and a writable one that extends it.

Two rules keep the coarser grant from reaching past a handler:

- A `with R => … do` block grants `R` alone, not what `R` implies.
- A handler holds the resources its effect's operations mention, transitively,
  but not their families. A handler of `Node` calling `this.tag()` declares
  `EventTarget`, or a function holding nothing could install it and reach the
  host through it.

What a function's signature no longer says, which handlers a test double needs,
the compiler reports where the double is installed: the
[`unhandled_operation`](./spec-diagnostics.md#the-unhandled_operation-lint)
lint warns at a handler scope whose body reaches a member of a handled family
that no handler in scope answers.
