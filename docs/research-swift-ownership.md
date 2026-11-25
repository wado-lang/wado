# Research: Swift's Ownership Decisions, and What Its Users Say About Them

Wado's affine types follow Swift's noncopyable types. This survey collects what
Swift decided where copyable and noncopyable values meet, and what Swift's own
users and designers say went wrong. It feeds
[WEP: Affine Types](./wep-2026-10-17-affine-types.md). Everything here was read
from the sources listed at the end, as they stood on 2026-11-25.

## Swift's decisions

### A convention is not a kind

SE-0377 adds `borrowing` and `consuming` to parameters and to `self`. They say
how a function receives a value. Whether the value can be copied is a property
of its type. The two are independent.

- A call looks the same under either convention. A copyable argument passed to
  a `consuming` parameter is copied when the caller uses it afterwards, and
  moved when it does not. SE-0437 states it for `initialize(to:)`: "If a
  copyable value is passed as a `consuming` argument, Swift can pass an implicit
  copy of it as needed".
- A parameter with no modifier is borrowed. The parameters of an `init` and of
  a setter are consumed.
- A noncopyable parameter must name its convention, "since there isn't a clear
  default that is always safe to assume".
- Inside the function, a binding that names its convention is no longer
  implicitly copyable, `self` included. A copy is written `copy x`. The rule
  stops at the binding: passing it on, or assigning it to a `let`, restores
  implicit copies.
- `inout` parameters and `mutating self` keep their implicit copies, for source
  compatibility. SE-0377 records this as an inconsistency it leaves standing.

### Counting was not enough

Swift had counted classes before it had noncopyable types. SE-0390 gives the
reason for adding them: a class can represent a unique resource, but "classes
always demand _shared ownership_ of the resource", with a heap allocation and a
count, and "shared access often complicates or introduces unsafety or
additional overhead into an object's APIs".

### Generic code assumes copying

SE-0427 makes `Copyable` a requirement every type parameter, protocol and
associated type carries unless it writes `~Copyable`. Existing generic code is
untouched. SE-0503 (Swift 6.4) extends the suppression to associated types.

### The standard protocols were not widened

SE-0437 generalizes `Optional`, `Result` and the unsafe pointers, and says of
the rest: "All other public protocols in the Standard Library continue to
require their conforming types as well as all their associated types to be
copyable for now", naming `Equatable` and `Sequence` among them.

Higher-order functions were left out for a reason of their own. Of
`Optional.map` it asks "Is this function supposed to consume or borrow the
optional?" and answers "the answer seems to be both!". One name cannot serve
both, because the conventions take no part in overload resolution. The proposal
defers the choice between a prefix (`consumingMap`) and a view
(`consuming.map`).

### Iteration got a second protocol

`Sequence` stays copyable-only. SE-0516 (Swift 6.4) adds `Iterable` beside it.

- The reason is the shape of `next()`. It returns an `Element?`, so over
  noncopyable elements it "could only be implemented by consuming the elements
  of the iterated sequence".
- `Iterable`'s iterator lends a `Span` of elements instead of returning one.
- The iterator's associated type does not default to `Copyable`. The proposal
  calls this "a highly desirable property", because what a copied iterator does
  "is very implementation dependent", and concludes: "writing generic code that
  relies on being able to copy an iterator is never appropriate."
- The cost is stated. With noncopyable elements, `first(where:)` cannot be
  written, an element cannot leave the loop, and two elements from different
  spans cannot be compared: each element's lifetime ends at the iterator's next
  step. "_None_ of these restrictions apply when an `Iterable` type's element is
  `Copyable`".
- A `for` loop over a type that conforms to both keeps using `Sequence`, since
  switching would change the meaning of code that mutates an array while
  iterating it.
- Consuming, draining, mutating and generative iteration are each left to a
  later proposal.

The existing protocol already carried the hazard. `Sequence`'s documentation
says: "Copying an iterator is safe, but advancing one copy of an iterator by
calling its `next()` method may invalidate other copies of that iterator."
Its adapters consume the sequence: `makeIterator`, `filter`, `prefix` and
`dropFirst` are declared `__consuming`. `IteratorProtocol` has one requirement,
`mutating func next()`, and no adapters.

### A reference is a value that cannot escape

SE-0519 (Swift 6.4) adds first-class references as two types. `Ref<Value>` is
`Copyable & ~Escapable`. `MutableRef<Value>` is `~Copyable & ~Escapable`.
Neither outlives its target, which SE-0446's lifetime dependencies check, and
the target cannot be used while a `MutableRef` to it is alive.

## The voices

### The pieces add up to a lifetime system

Dave Abrahams, one of Swift's original designers and now on Hylo, opened a
thread in February 2026 arguing that the features were each added at a small
cost and together are a "march toward a full Rust-like lifetime system". His
claim is that yielding accessors with `borrow` and `inout` bindings cover
nearly every use, and that the rest is better left to unsafe code. He calls
first-class reference types "extremely worrying".

The usual defence is progressive disclosure: most developers never meet these
features. His reply: "It is a fallacy that complexity in the fundamental
language base is ever really hidden." It appears hidden only "because the
standard library hasn't (fully?) adopted it, at which point it is going to be
in everyone's faces." Another poster, from 25 years on a large application
team, adds that in a long-lived codebase somebody always decides an obscure
feature is essential, and then everyone who debugs that module has to learn it.

### Two hierarchies are forever

Reviewers of SE-0516 objected that it "introduces a parallel iteration
hierarchy" which "users and library authors will have to learn and live with
indefinitely", chosen to keep the compiler simpler.

The proposal's own author agrees about the default. Ben Cohen: borrowing "feels
like the right default behavior", as it is for parameters, and "The opt in is
for the rarer case of consuming." Swift cannot switch because the change breaks
source and `makeIterator` cannot become `consuming` under a stable ABI. He adds
that "eager implementations of e.g. `map` and `filter` were regrettable".

### `Sequence` was already a regret

Jordan Rose, formerly of the Swift team, lists `Sequence` among his Swift
regrets. Without move-only types, the language had to let `makeIterator()` be
called repeatedly, and nothing says whether a second call continues, restarts
or fails. An iterator is also not a sequence, so the adapters do not apply to
it. Of the SE-0516 pitch he writes: "It may ultimately make sense to continue
perpetuating the mistake of Sequence just for consistency".

### Copies nobody wrote

A Swift compiler engineer is prototyping an "explicit copies mode": a
function-level switch under which every copy the compiler would insert must be
written `copy x`. The first example is `for x in xs`, which copies `xs` because
`makeIterator` consumes it. Users report the same confusion from the other
side: a `consuming` parameter that rejects `let y = x` and accepts passing `x`
to a function and then returning it.

### One spelling, three readings

`~Copyable` on a type means the type cannot be copied. On a type parameter it
means the parameter might not be copyable. On a generic type with a conditional
conformance it means both at once. A forum thread titled "Is ~Copyable
confusing?" gets the answer "yes, it is confusing" from a poster who goes on to
defend the design. Another writes that the spelling "has different meanings in
your short code sample (even in a single line!)".

### Nobody agrees when a value is destroyed

A thread from October 2026 asks whether a noncopyable type should release a
system resource in its `deinit` at all. It was still open when this was
written.

- A value consumed on one branch only is destroyed where the branches rejoin,
  not at the end of its scope. John McCall says that is the rule. Alejandro
  Alonso, an author of SE-0519, calls it a bug. A third poster offered to move
  the destruction to the end of the scope.
- One poster: "I personally don't have a good enough mental model
  for when `~Copyable` types are actually destroyed even after having worked
  with them for over 2 years now."
- An implicit release loses the error a close can report, and an unrelated edit
  can move it.
- The alternative in use is a `consuming func close()` with a `deinit` that
  traps. It turns a forgotten close into a crash on the path that forgot it,
  which is usually the least tested one.
- Many posters want the compiler to require the consume. That is a linear type,
  which Swift does not have, and `defer` cannot consume a noncopyable value
  either.

## What separates Wado's position from Swift's

- Swift's two iteration protocols, and the `inout` inconsistency, are kept for
  source and ABI compatibility. Wado has neither constraint.
- Swift's designers and its critics agree on one point: an iterator should not
  be copied. Its collections and ranges stay copyable. Wado keeps its iterators
  copyable, and the WEP says what that choice weighed.
- Swift's references are safe because they cannot escape, and its mutable
  reference cannot be copied. Wado's `&mut T` can do both, and Wado has no
  borrow check. The WEP already departs from Swift here.
- Swift destroys a value consumed on one branch where the branches rejoin.
  Wado drops it where its scope ends, on the paths that did not move it, so an
  unrelated edit to one branch does not move the drop on another.
- Swift left open whether `map` on an optional consumes or borrows. Wado's
  `Option::map` takes `self` by value, as Rust's does: it moves an affine
  option and copies a copyable one. Borrowing is a pattern over `&opt`.
- Both languages trap on a value that had to be consumed and was dropped
  instead. Swift's `Continuation` does, and so does Wado's `FutureWritable`
  dropped before its write. Neither has a type the compiler requires to be
  consumed.

## Sources

Proposals, in [swiftlang/swift-evolution](https://github.com/swiftlang/swift-evolution/tree/main/proposals):

- [SE-0377: `borrowing` and `consuming` parameter ownership modifiers](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0377-parameter-ownership-modifiers.md)
- [SE-0390: Noncopyable structs and enums](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0390-noncopyable-structs-and-enums.md)
- [SE-0427: Noncopyable generics](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0427-noncopyable-generics.md)
- [SE-0437: Noncopyable standard library primitives](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0437-noncopyable-stdlib-primitives.md)
- [SE-0446: Nonescapable types](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0446-non-escapable.md)
- [SE-0503: Suppressed default conformances on associated types](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0503-suppressed-associated-types.md)
- [SE-0516: `Iterable`](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0516-borrowing-sequence.md)
- [SE-0519: `Ref` and `MutableRef` types](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0519-ref-mutableref-types.md)
- [`Sequence.swift` in the standard library](https://github.com/swiftlang/swift/blob/main/stdlib/public/core/Sequence.swift)

Discussion:

- [~Escapable, Span, Ownership Annotations, etc](https://forums.swift.org/t/escapable-span-ownership-annotations-etc/84566)
- [Pitch: Borrowing Sequence](https://forums.swift.org/t/pitch-borrowing-sequence/84332)
- [Second review of SE-0516: `Iterable`](https://forums.swift.org/t/second-review-se-0516-iterable/87106)
- [Swift Regret: Sequence](https://belkadan.com/blog/2021/08/Swift-Regret-Sequence/)
- [Explicit copies mode](https://forums.swift.org/t/explicit-copies-mode/81779)
- [Question on implicit copy in SE-0377](https://forums.swift.org/t/question-on-implicit-copy-in-se-0377/81368)
- [Is ~Copyable confusing?](https://forums.swift.org/t/is-copyable-confusing/84463)
- [Should noncopyable types release system resources in `deinit`?](https://forums.swift.org/t/should-noncopyable-types-release-system-resources-in-deinit/90120)
