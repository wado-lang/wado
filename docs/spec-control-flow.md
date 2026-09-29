# Control Flow

This chapter covers what decides which code runs next: conditionals, loops,
`break` and `continue`, labeled blocks, `match` and `matches`, branch hints, and
error handling. The patterns these statements take apart are in
[Patterns](./spec-patterns.md).

## Conditional Statements

<!-- {"fixture":"spec_control_flow_conditionals.wado"} -->

```wado
let x = 7;
let mut sign = "";
if x < 0 {
    sign = "negative";
} else if x == 0 {
    sign = "zero";
} else {
    sign = "positive";
}
assert sign == "positive";
```

### If Expression

<!-- {"fixture":"spec_control_flow_conditionals.wado"} -->

```wado
let x = -3;
let abs = if x < 0 { -x } else { x };
assert abs == 3;

let score = 85;
let grade = if score >= 90 { "A" } else if score >= 80 { "B" } else { "C" };
assert grade == "B";
```

### If Let Pattern Matching

<!-- {"fixture":"spec_control_flow_conditionals.wado"} -->

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
let mut got = 0;
if let Some(x) = opt {
    got = x;
} else {
    got = -1;
}
assert got == 42;
```

### Let Else

`let PATTERN = EXPR else { ... };` binds a refutable pattern whose bindings
escape into the enclosing scope. On a match they cover the rest of the block;
otherwise the `else` block runs and must diverge (`return`, `break`,
`continue`, `panic`, …). The `else` block does not see the bindings.

<!-- {"fixture":"spec_control_flow_conditionals.wado"} -->

```wado
fn parse_port(s: String) -> i32 {
    let Ok(port) = i32::from_str(&s) else {
        return -1;
    };
    return port;                // `port` is in scope here
}

test {
    assert parse_port("8080") == 8080;
    assert parse_port("http") == -1;
}
```

An irrefutable pattern is an error, since its `else` could never run. Write a
plain `let` instead.

## While Loop

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let mut i = 0;
let mut sum = 0;
while i < 10 {
    sum += i;
    i = i + 1;
}
assert sum == 45;
```

### While Let Pattern Matching

`while let` allows iterating while a pattern matches:

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let items: List<i32> = [1, 2, 3];
let mut iter = items.into_iter();

let mut seen: List<i32> = [];
while let Some(x) = iter.next() {
    seen.push(x);
}
assert seen == [1, 2, 3];
```

The loop runs while the pattern matches, and ends the first time it does not.

## For Loop

C-style for loop with initialization, condition, and update. Parentheses are optional:

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let mut sum = 0;
for let mut i = 0; i < 10; i = i + 1 {
    sum += i;
}
assert sum == 45;

// With parentheses (also valid)
for (let mut i = 0; i < 10; i = i + 1) {
    sum += i;
}
assert sum == 90;

// All parts are optional
let mut n = 0;
for ;; {
    n += 1;
    if n == 3 { break; }
}
assert n == 3;
```

### Semantics

`for init; cond; update { body }` runs `init` once. Each iteration then checks
`cond`, runs `body` if it holds, and runs `update`. `continue` ends the body and
still runs `update`, as in C.

Each iteration has its own copy of the bindings `init` declares, as ECMA-262's
`for (let …)` does. Before `update` runs, the next iteration's bindings are
created holding the current values, and `update` and the next `cond` act on
them. So a closure or a reference taken in one iteration keeps that iteration's
binding, and later iterations do not change it:

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let mut fs: List<fn() -> i32> = [];
for let mut i = 0; i < 3; i += 1 {
    fs.push(|| i);
}
assert fs[0]() == 0 && fs[1]() == 1 && fs[2]() == 2;   // not 3, 3 and 3
```

### For with Pattern Condition

The condition part of a C-style for loop can use `let` pattern matching:

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let items: List<i32> = [10, 20, 30];
let mut iter = items.into_iter();

let mut sum = 0;
for ; let Some(x) = iter.next(); {
    sum += x;
}
assert sum == 60;

// With update expression
let mut again = items.into_iter();
let mut count = 0;
for ; let Some(x) = again.next(); count += 1 {
    assert x == items[count];
}
assert count == 3;
```

The loop runs while the pattern matches, as `while let` does, and the update
runs after each iteration.

## For-Of Loop

For iterating over any type that implements `IntoIterator`:

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let numbers: List<i32> = [1, 2, 3, 4, 5];
let mut sum = 0;
for let n of numbers {
    sum += n;
}
assert sum == 15;

// With mutable binding
for let mut item of numbers {
    item = item * 2;  // Can modify the local binding
    sum += item;
}
assert sum == 45 && numbers[0] == 1;

// Custom types that implement IntoIterator also work
let my_collection = Countdown { n: 3 };
let mut order: List<i32> = [];
for let x of my_collection {
    order.push(x);
}
assert order == [3, 2, 1];
```

### Semantics

`for let item of collection { body }` calls `collection.into_iter()` once,
before the first iteration. Each iteration then calls `next()` on the result,
binds the element to `item` and runs the body. The loop ends when `next()`
returns `None`.

The binding is a copy of each element (value semantics), so modifying it does
not affect the original collection. Each iteration binds it anew, so a closure
or a reference taken in one iteration keeps that iteration's element.

The binding must match every element, as a `let` pattern must (see
[Patterns That Cannot Fail](./spec-patterns.md#patterns-that-cannot-fail)). A pattern that can
fail (`for let Some(x) of xs`, a narrowing type pattern) is a compile error.
Match on the element in the body instead.

### Tuple for-of (compile-time expansion)

When the iterable is a tuple, the loop body is expanded once per element at compile time. Each expansion independently types the binding, enabling heterogeneous iteration with per-element trait dispatch.

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let t = [42, "hello", true];
let mut out = "";
for let v of t {
    out.push_str(`${v} `);  // expanded to three blocks, each with the correct type
}
assert out == "42 hello true ";
```

`break` and `continue` are not allowed inside a tuple for-of body because the loop is unrolled at compile time into sequential blocks and these have no natural target. `.enumerate()` is supported and provides a compile-time index.

### Tuple comprehension

Wrapping the same walk in `[...]` collects one result element per source element. The braces hold a single expression — the element's value — since every position of the result tuple has one.

<!-- {"fixture":"spec_control_flow_labeled_blocks.wado"} -->

```wado
impl<..T: Doubled> Doubled for [..T] {
    fn doubled(&self) -> [..T] {
        return [for let v of *self { v.doubled() }];
    }
}

test {
    assert [1, "ab"].doubled() == [2, "abab"];
}
```

The `.enumerate()` form binds the index alongside the value (`[for let [i, v] of t.enumerate() { ... }]`). The index is a compile-time constant, so it is also the one non-literal a tuple accepts as a subscript (`t[i]`), for reads and writes alike. A `mut` index can change, so it is not a subscript.

The source must be a variadic tuple (`[..T]`); a concrete tuple is not walkable this way.

## Infinite Loop

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
let mut n = 0;
loop {
    // runs forever until break
    n += 1;
    if n == 5 {
        break;
    }
}
assert n == 5;
```

## Break and Continue

`break` exits the innermost enclosing loop. `continue` skips to the next iteration.

<!-- {"fixture":"spec_control_flow_loops.wado"} -->

```wado
// break example
let mut i = 0;
while i < 100 {
    if i == 10 {
        break;  // exit the loop
    }
    i = i + 1;
}
assert i == 10;

// continue example
let mut kept: List<i32> = [];
for let mut j = 0; j < 10; j = j + 1 {
    if j == 5 {
        continue;  // skip 5
    }
    kept.push(j);
}
assert kept.len() == 9 && kept[5] == 6;
```

Both work with `while`, `for`, and `loop`, and are errors outside a loop. A loop
carries no label, and `continue` never takes one. `break LABEL` is a different
statement: it leaves an enclosing [labeled block](#labeled-blocks), and is how
to leave more than the innermost loop. A closure body starts its own loop scope,
so a loop around a closure binds nothing written inside it.

## Labeled Blocks

A labeled block is a named block that `break LABEL` leaves from anywhere inside
it. It is Wado's only non-local jump, and it replaces loop labels, labeled
`continue`, and `goto`. It is also the block form that carries a value.

### Syntax

`LABEL: { ... }`, in statement or expression position.

- The label is an identifier followed by a colon
- `break LABEL;` leaves the block; `break LABEL: expr;` leaves it with a value
- `()` is a value like any other, so `break LABEL: ()` says what `break LABEL`
  says, and `break ()` says what `break` says
- Nested blocks may reuse a label name; a `break` targets the innermost match
- The block opens a new scope, and a name declared inside may shadow an outer
  one

The label is mandatory to tell a block apart from a struct literal, since
`{ field: value }` on its own could be either. An unlabeled `{ ... }` block is
therefore a parse error.

### Escaping and Early Exit

`break LABEL` inside a loop nest leaves all of it at once, and the block's tail
is the path no `break` took. Inside a block,
`break LABEL` skips the rest, so a chain of guards stays flat instead of nesting
one inside the next. A `break` may also leave an effect handler's `do` block
(see [Handler Scope](./spec-effects.md#handler-scope)).

<!-- {"fixture":"spec_control_flow_labeled_blocks.wado"} -->

```wado
let grid: List<List<i32>> = [[1, 2], [3, 4]];
let needle = 3;
let mut hit = [0, 0];
search: {
    for let r of 0..<grid.len() {
        for let c of 0..<grid[r].len() {
            if grid[r][c] == needle {
                hit = [r, c];
                break search;   // leaves both loops
            }
        }
    }
    hit = [-1, -1];             // reached only when no break was taken
}
assert hit == [1, 0];
```

### As an Expression

A labeled block used as a value yields two ways: through `break LABEL: expr`,
and through its trailing statement on the path that reaches the end. Both are
branches, so every path must agree on one type. They unify against the type
expected where the block sits, so a literal coerces to that rather than to its
own default. Any expression position takes one, not only the right-hand side of
a `let`.

<!-- {"fixture":"spec_control_flow_labeled_blocks.wado"} -->

```wado
let items: List<Entry> = [{ key: "a", value: 1 }, { key: "b", value: 2 }];
let key = "b";
let found = search: {
    for let item of items {
        if item.key == key {
            break search: item.value;
        }
    }
    -1  // the value when no break is taken
};
assert found == 2;
```

A trailing statement that is not a value yields `()`. A block whose branches all
yield `()` has the type `()`, and one mixing `()` with a value is the same type
error as any other disagreement.

A path that cannot reach the end is no branch at all. When the trailing
statement is a loop that only `break LABEL` leaves, nothing reaches the tail, so
the breaks alone type the block. This is how a loop computes a value:

<!-- {"fixture":"spec_control_flow_labeled_blocks.wado"} -->

```wado
let mut n = 0;
let found = scan: {
    loop {
        n += 1;
        if n * n > 50 { break scan: n; }
    }
};
assert found == 8;
```

## Match Expression

A `match` tests its scrutinee against each arm's pattern in order and runs the
first arm that matches. Its arms must be
[exhaustive](./spec-patterns.md#exhaustiveness). A `match` produces a value, or
stands as a statement:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
// Match expression (produces a value)
let result = match opt {
    Some(x) => x * 2,
    None => 0,
};
assert result == 42;

// Match with custom variants
let area = match shape {
    Circle(r) => 3.14159 * r * r,
    Rectangle([w, h]) => w * h,
    Point => 0.0,
};
assert area == 6.0;

// Match statement (no value produced)
match command {
    Start => engine.start(),
    Stop => engine.stop(),
}
assert engine.running;
```

## Matches Operator

The infix `matches` operator answers whether a value matches a pattern, as a
`bool`:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
// Basic usage
let is_some = opt matches { Some(_) };
let is_circle = shape matches { Circle(_) };
assert is_some && is_circle;

// With guard
let is_large = shape matches { Circle(r) && r > 10.0 };
assert !is_large;

// In conditions
let mut has_value = false;
if opt matches { Some(_) } {
    has_value = true;
}
assert has_value;
```

### Scope

Pattern bindings are scoped to the guard only and do not escape, so `x` is not
in scope after the pattern:

<!-- {"fixture":"spec_control_flow_matches_scope.wado"} -->

```wado
if opt matches { Some(x) } && x > 0 { }
```

Use a guard inside the pattern instead:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
assert opt matches { Some(x) && x > 0 };
```

## Branch Hints

`builtin::cold_path()` marks the path that contains it as rarely run. It is a
statement with no effect on what the program computes. It hints that the other
side of the branch containing it is the likely one.

It is a statement rather than a condition wrapper, so it works anywhere a branch
body does. That includes an `if let` or `match` arm, where no boolean condition
is available:

<!-- {"fixture":"spec_control_flow_branch_hints.wado"} -->

```wado
impl Buffer {
    // Error/abort guard: the taken branch is cold.
    fn get(&self, i: i32) -> i32 {
        if i >= self.len {
            builtin::cold_path();
            panic("index out of bounds");
        }
        return self.data[i];
    }
}

// `match` arm with no boolean condition.
fn run(command: Command) -> i32 {
    return match command {
        Command::Run => execute(),
        Command::Crash => {
            builtin::cold_path();
            panic("crashed");
        },
    };
}

test {
    let b = Buffer { data: [1, 2], len: 2 };
    assert b.get(1) == 2 && run(Command::Run) == 1;
}
```

Placed on the fall-through after a guard whose taken branch diverges, it hints
that the guard is likely taken:

<!-- {"fixture":"spec_control_flow_branch_hints.wado"} -->

```wado
impl Cache {
    fn lookup(&self, key: String) -> i32 {
        if let Some(v) = self.fast_path(key) {
            return v;
        }
        builtin::cold_path(); // the slow path below is rarely reached
        return self.slow_path(key);
    }
}

test {
    let cache = Cache { hot: "a" };
    assert cache.lookup("a") == 1 && cache.lookup("bc") == 3;
}
```

## Optimization Barrier

`builtin::black_box(value)` returns `value` unchanged but never as a
compile-time constant, so the compiler does not fold away a computation reading
it. Use it to keep a test or benchmark measuring the
work it names:

<!-- {"fixture":"spec_control_flow_branch_hints.wado"} -->

```wado
test "sign extension folds the redundant mask" {
    assert to_i8(builtin::black_box(300)) == 44;
}
```

Without it, `to_i8(300)` folds to `44` and the test no longer reaches `to_i8`.

The barrier binds the Wado compiler only. The Wasm engine that runs the program
is still free to fold the value.

## Error Handling

### Unrecoverable Errors (Traps)

<!-- {"fixture":"spec_control_flow_errors.wado"} -->

```wado
#[expect_trap]
test "panic logs the message to stderr, then traps" {
    panic("Fatal error");
}

#[expect_trap]
test "unreachable traps" {
    unreachable();
}

#[expect_trap]
test "assert panics when the condition is false" {
    let condition = false;
    assert condition;
}
```

A trap cannot be caught in Wado. It ends the program.

### Recoverable Errors (Result Type)

A function that can fail returns a `Result`, and its caller decides what an
`Err` means:

<!-- {"fixture":"spec_control_flow_errors.wado"} -->

```wado
variant ConfigError {
    Io(FsError),
    Parse(ParseIntError),
}

impl From<ParseIntError> for ConfigError;

fn parse_config(text: String) -> Result<Config, ParseIntError> {
    return Ok(Config { port: i32::from_str(text.trim())? });
}

fn read_config(path: String) -> Result<Config, ConfigError> with Preopens {
    let content = fs::read_to_string(path)
        .map_err(|e| ConfigError::Io(e))?;
    let config = parse_config(content)?;
    return Ok(config);
}

test {
    // Handle with pattern matching
    let message = match read_config("missing.toml") {
        Ok(config) => `port ${config.port}`,
        Err(Io(e)) => `cannot read: ${e}`,
        Err(Parse(e)) => `bad port: ${e}`,
    };
    assert message.starts_with("cannot read");
}
```

### Error Propagation

The postfix `?` operator unwraps a `Result` or an `Option`. Where there is
nothing to unwrap, it returns from the enclosing function:

- On `Ok(v)`, `expr?` is `v`. On `Err(e)`, the function returns
  `Err(From::from(e))`, so the error converts to the function's own error type.
  Above, `parse_config(content)?` turns a `ParseIntError` into a `ConfigError`
  through the `From` impl.
- On `Some(v)`, `expr?` is `v`. On `None`, the function returns `None`.

`?` on a `Result` needs a function that returns a `Result`, and `?` on an
`Option` one that returns an `Option`. Any other pairing is an error:

<!-- {"fixture":"try_op_error_option_in_result_fn.wado"} -->

```wado
fn outer() -> Result<i32, String> {
    let x = inner()?;
    return Result::<i32, String>::Ok(x);
}
```

Inside a closure, `?` returns from the closure
([Closures](./spec-functions.md#closures)).

Rationale: [WEP: Conversion Traits](./wep-2026-03-16-conversion-traits.md).
