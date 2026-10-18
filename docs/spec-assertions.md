# Assertions

`assert` checks a condition while a program runs. It is a statement like any
other, so it works in ordinary code as well as in a `test` block.

## The `assert` Statement

`assert` checks that a condition is true. If it is false, the program traps.
First it writes a message to standard error, showing the condition's source and
the values of its operands as power-assert does, unless the build drops it (see
[Assertions Are Never Removed](#assertions-are-never-removed)).

<!-- {"fixture":"spec_testing_assert.wado"} -->

```wado
// Traps if x is not greater than 0.
assert x > 0;

// Also assert can take an optional message.
assert x > 0, "x must be checked elsewhere";
```

## Evaluation

`assert` evaluates its condition exactly as the surrounding code would, so a
guarded operand never runs when its guard fails. An operand the run did not
reach is reported as `<not evaluated>`.

<!-- {"fixture":"spec_testing_power_assert.wado"} -->

```wado
let list: List<i32> = [1, 2, 3];
let i = 99;
assert i < list.len() && list[i] == 1;
// condition: i < list.len() && list[i] == 1
// i: 99
// list.len(): 3
// i < list.len(): false
// i: <not evaluated>
// list[i]: <not evaluated>
// list[i] == 1: <not evaluated>
```

## The Failure Message

The receiver of a method call or a subscript is not captured; the call's result
is. `list` in `list.len()` or `list[i]` may be a whole container, and rendering
it would cost more than the check, and keep the optimizer from taking the
container apart for a message that only a failure prints. What computes the
receiver's place, such as `i` in `rows[i].len()`, is still captured.

Each captured operand is rendered with `Inspect` (`:?`), so a long `String` or
`List` operand is cut at `Inspect`'s default length and marked where it was cut
(see [Inspect Truncation](./spec-literals.md#inspect-truncation)). This keeps a
failure readable. The optional message is an ordinary expression, formatted by
whatever template specifiers it uses. `Display` is never cut, so formatting a
value into the message yourself shows all of it.

## Assertions Are Never Removed

No build removes an assertion. At every optimization level the condition is
evaluated, and a false one traps. An invariant an `assert` states therefore
holds in every build, so a program can rely on `assert` to protect its state.

Checking costs run time. The part that can be dropped is the power-assert
overhead: capturing the operands and building the failure message. Under
`-f bare-asserts` a failed assertion traps without printing anything, and
nothing that captures or builds the message is compiled in. `-Os` turns it on,
and `-f no-bare-asserts` turns it back off:

```sh
wado compile -Os app.wado                     # a failed assert traps silently
wado compile -Os -f no-bare-asserts app.wado  # the message is kept
wado compile -O2 -f bare-asserts app.wado     # silent at -O2 too
```

## Contract Checks

A function states the preconditions its caller owes with `contract` clauses,
written after the `with` clause and before the body. Each clause is a condition
and an optional message, as an `assert` is, and a function may have any number
of them. A condition is parsed as an `if` condition is, with no struct literal.
It is evaluated where the function is defined and performs no effect.
Everything it names is at least as visible as the function, so a caller can
evaluate the contract it owes. A clause naming a less visible item, such as a
private field in a `pub fn`'s clause, is a compile error.

> Not yet implemented: a clause may name a less visible item.

<!-- {"fixture":"contract_clause.wado", "assert": false} -->

```wado
fn half(n: i32) -> i32
    contract n % 2 == 0, "n must be even"
{
    return n / 2;
}
```

A clause writes only the values it creates. A build may skip the clause, so a
write reaching anything else would make what the call computes depend on the
build. Writing a parameter, a global, or anything through a reference is a
compile error. So is taking `&mut` of one, and calling a method that takes
`&mut self` on one. A value the clause binds is its own only if it holds no
`&mut`, resource, or function value, since a copy of one shares what that
points to.

A callee may be handed such a value only where it cannot write through it: a
`&mut T` bound to a `&T` parameter, where `T` holds none of them, or a closure
the clause writes, whose body this rule covers. An argument written `&mut x`,
where `x` is the clause's own, is handed on as well. Calling a function value
follows the same rule: a `fn`, like a `fn mut`, may write through a `&mut` it
captured, so only a closure the clause writes may be called.

> Not yet implemented: a function a clause calls may write a mutable global.

<!-- {"fixture":"error_contract_mutates_param.wado"} -->

```wado
fn doubled(mut n: i32) -> i32
    contract doubling: { n = n * 2; true }
{
    return n;
}
```

The clauses are checked in the order written, and the first that fails traps,
so a later clause may rely on the earlier ones.

<!-- {"fixture":"contract_clause_order.wado", "assert": false} -->

```wado
fn non_negative_at(xs: &List<i32>, i: i32) -> i32
    contract 0 <= i < xs.len(), "i must be in range"
    contract xs[i] >= 0
{
    return xs[i];
}
```

A call traps when it is outside the contract, as a failed assertion does,
message included. The report names the function and the call's position, since
the call is what broke the contract. A call through a function value reports
the clause's position instead, as the function cannot know who called it.

A violation may trap sooner than the call that breaks the contract, at an
earlier call on the same path with nothing observable between the two. The
report then names that earlier call and its arguments. A contract promises only
that a violation traps, so `xs[i]; xs[i + 1]; xs[i + 2]` may check
`i + 2 < xs.len()` at `xs[i]`.

<!-- {"fixture":"contract_call_site.wado", "assert": false} -->

```wado
fn pick_through<P: Pick>(p: &P, i: i32) -> i32 {
    return p.pick(i);   // a failure reports this line
}
```

A caller that guarantees the contract itself marks the call
with the prefix operator `unchecked`, and a marked call is checked only where
the build checks contracts. A call without the marker is checked in every
build. The operand of `unchecked` is a call to a function with a contract, or
an index `c[i]` whose index method has one (see
[Indexing Traits](./spec-standard-traits.md#indexing-traits)); anything else is
a compile error. A marked index is the place `c[i]` names, referenced or written
through only where `c[i]` could be, but an index assignment takes no marker.
Postfix operators bind tighter, so
`unchecked f(x)?` marks the `?`, an error, and `(unchecked f(x))?` marks the
call.

<!-- {"fixture":"contract_clause.wado"} -->

```wado
test "calls inside the contract" {
    assert half(4) == 2;
    assert unchecked half(6) == 3;
}
```

A build that does not check evaluates nothing, and the compiler does not assume
the contract holds: the violation keeps the class the function's documentation
gives it (see [Behavior Classes](./spec-overview.md#behavior-classes)). A call
from another component carries no marker, so a contract on an `export fn` is
always checked.

Whether a build checks a marked call is set by `-f contract-checks` and
`-f no-contract-checks`, and by nothing a program writes. Without either flag,
the test world checks at every optimization level, and any other world checks
at `-O0` and `-O1`. `wado run` and `wado serve` build at `-O1` unless told
otherwise, so they check by default:

```sh
wado test app.wado                              # checks
wado test -f no-contract-checks app.wado        # does not
wado run app.wado                               # checks (-O1)
wado run -O2 app.wado                           # does not
wado compile app.wado                           # does not (-O2)
wado compile -O2 -f contract-checks app.wado    # checks
```

A trait method's clauses are written on the trait's declaration, one without a
body included, and bind every impl of the method and the trait's default body.
An impl of a method whose trait states clauses writes none; one that does is a
compile error. A call through a bound checks the trait's clauses, and
`unchecked` vouches for them.

An impl of a trait method that declares no clause may state its own. A call
checks them however it reaches the impl, through a bound included. `unchecked`
vouches for them only where the impl is the callee the call names. Through a
bound the caller sees the trait's declaration alone, so `unchecked` there is a
compile error: it would vouch for clauses the caller never saw.

<!-- {"fixture":"contract_impl_clause.wado", "assert": false} -->

```wado
impl Get for Small {
    fn get(&self, i: i32) -> i32
        contract i < 10
    {
        return self.value + i;
    }
}
```

<!-- {"fixture":"contract_trait_method.wado", "assert": false} -->

```wado
trait Pick with () {
    fn size(&self) -> i32;

    fn pick(&self, i: i32) -> i32
        contract 0 <= i < self.size();

    fn first(&self) -> i32
        contract self.size() > 0
    {
        return self.pick(0);
    }
}
```
