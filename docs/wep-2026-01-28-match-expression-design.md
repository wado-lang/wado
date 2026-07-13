# WEP: Match Expression Design

## Context

Wado needs `match` expressions/statements for exhaustive pattern matching on variants and other types. Additionally, since Wado has no macros, `matches!` functionality must be provided as a language feature.

### Current State

- `if let` pattern matching is implemented for Option and custom variants
- `while let` and `for let` patterns work
- Tuple destructuring with `let [a, b] = ...` is implemented
- AST structure for `MatchExpr` exists but parser doesn't handle it yet
- `Match` token is recognized by the lexer

### Key Differences from Rust

| Aspect             | Rust           | Wado            |
| ------------------ | -------------- | --------------- |
| Tuple type/literal | `(T, U)`       | `[T, U]`        |
| Multiple payload   | `Foo(T, U)`    | `Foo([T, U])`   |
| Struct variant     | `Foo { a: T }` | `Foo({ a: T })` |

## Decision

### Part 0: Refutable and Irrefutable Patterns

Following Rust's terminology, Wado distinguishes between two kinds of patterns:

#### Irrefutable Patterns

Patterns that **always match** any possible value of the given type. Used where a match failure would be a compile-time error.

Examples:

- `x` (variable binding) - matches anything
- `_` (wildcard) - matches anything
- `[a, b]` (tuple pattern where components are irrefutable)

Contexts requiring irrefutable patterns:

- `let` statements: `let x = expr;`
- Function parameters: `fn foo(x: i32)`
- `for-of` bindings: `for let item of array`

```wado
let x = 42;           // OK: x is irrefutable
let [a, b] = tuple;   // OK: tuple pattern with irrefutable components
let Some(x) = opt;    // ERROR: Some(x) is refutable (opt could be None)
```

#### Refutable Patterns

Patterns that **may fail to match**. Used in contexts that handle match failure.

Examples:

- `Some(x)` (variant pattern) - may not match `None`
- `42` (literal pattern) - may not match other integers
- `"hello"` (string literal) - may not match other strings
- `[a, b] && a > b` (pattern with guard)

Contexts accepting refutable patterns:

- `if let`: `if let Some(x) = opt { ... }`
- `while let`: `while let Some(x) = iter.next() { ... }`
- `for` condition: `for ; let Some(x) = iter.next(); { ... }`
- `match` arms: `match opt { Some(x) => ..., None => ... }`
- `matches` operator: `opt matches { Some(_) }`

```wado
if let Some(x) = opt { ... }     // OK: refutable pattern in if let
while let Some(x) = iter.next() { ... }  // OK: refutable pattern in while let
match opt {
    Some(x) => x,                // OK: refutable pattern in match arm
    None => 0,
}
```

#### Pattern Contexts Summary

| Context            | Accepts          | Match Failure Handling |
| ------------------ | ---------------- | ---------------------- |
| `let` statement    | Irrefutable only | Compile error          |
| Function parameter | Irrefutable only | Compile error          |
| `for-of` binding   | Irrefutable only | Compile error          |
| `if let`           | Refutable        | Takes else branch      |
| `while let`        | Refutable        | Exits loop             |
| `for` condition    | Refutable        | Exits loop             |
| `match` arm        | Refutable        | Tries next arm         |
| `matches` operator | Refutable        | Returns `false`        |

#### Note on Irrefutable Patterns in Refutable Contexts

Irrefutable patterns are allowed in refutable contexts but may trigger warnings:

```wado
// Technically valid but likely a mistake - x always matches
if let x = get_value() {
    // This branch always executes
}

// Better: use regular let binding
let x = get_value();
```

### Part 1: Match Expression Syntax

#### Basic Syntax

```wado
// Match expression (produces a value)
let area = match shape {
    Circle(r) => 3.14159 * r * r,
    Rectangle([w, h]) => w * h,
    Point => 0.0,
};

// Match statement (no value, semicolon after each arm)
match command {
    Start => { engine.start(); };
    Stop => { engine.stop(); };
    Pause => { engine.pause(); };
}
```

#### Pattern Syntax

```wado
match value {
    // Variant patterns (case name only, no full path required)
    Some(x) => ...,
    None => ...,
    Circle(r) => ...,
    Rectangle([w, h]) => ...,        // Tuple payload destructuring
    Named({ width, height }) => ..., // Struct payload destructuring

    // Full path also allowed
    Shape::Circle(r) => ...,
    Option::<i32>::Some(x) => ...,

    // Literal patterns
    0 => ...,
    "hello" => ...,
    true => ...,

    // Tuple patterns
    [a, b, c] => ...,
    [first, _, last] => ...,

    // Wildcard
    _ => ...,

    // Or patterns (multiple patterns, one arm)
    Circle(r) | Rectangle([r, _]) => ...,

    // Guard expressions (uses `&&` for natural left-to-right reading)
    Some(x) && x > 0 => ...,
    [a, b] && a == b => ...,
}
```

#### Expression vs Statement

Match uses **unified Rust-like syntax**. The trailing comma after each arm is optional (like trailing commas elsewhere in Wado). The trailing semicolon inside block bodies also doesn't change semantics, following Wado's common block rules.

```wado
// Expression: produces a value
let x = match opt {
    Some(v) => v * 2,
    None => 0,
};

// Statement: same syntax, just not assigned
match opt {
    Some(v) => println(`{v}`),
    None => println("none"),
}

// Block bodies - trailing semicolon optional, doesn't change semantics
match opt {
    Some(v) => {
        let doubled = v * 2;
        println(`{doubled}`)   // no semicolon - OK
    },
    None => {
        println("none");       // with semicolon - also OK, same meaning
    },
}

// Trailing commas are optional
let y = match opt {
    Some(v) => v * 2,          // comma
    None => 0                   // no comma - OK
};
```

This is consistent with `if` expressions and other block constructs in Wado.

#### Constant Patterns

A bare identifier in pattern position that is neither a variant/enum case nor a
new binding resolves to a **constant value** when it names an immutable `global`
or an associated constant (`i32::MAX`, `Type::SOME_CONST`). The pattern matches
when the scrutinee equals that constant; it introduces no binding. This lets
named constants stand in for magic numbers in a `match`:

```wado
global MAJOR_TAG: u8 = 6;

match byte >> 5 {
    MAJOR_TAG => decode_tag(),   // matches when the value equals MAJOR_TAG (6)
    _ => decode_other(),
}
```

Only immutable globals qualify; a `global mut` is never a constant pattern.
Constant patterns combine with `|` (or-patterns) and `&&` (guards) like any
other refutable pattern, and a constant read here counts as a use of the global
(it is not reported as dead code).

#### Exhaustiveness

Match must be exhaustive:

```wado
variant Color { Red, Green, Blue }

// Error: non-exhaustive patterns
match color {
    Red => "red",
}

// OK: wildcard makes it exhaustive
match color {
    Red => "red",
    _ => "other",
}

// OK: all cases covered
match color {
    Red => "red",
    Green => "green",
    Blue => "blue",
}
```

For types with infinite values (integers, strings):

```wado
// Error: non-exhaustive
match num {
    0 => "zero",
    1 => "one",
}

// OK: wildcard required
match num {
    0 => "zero",
    1 => "one",
    _ => "other",
}
```

### Part 2: `matches` Infix Operator

Since Wado has no macros, `matches!` must be a language feature. After evaluating alternatives, we adopt the **`matches` infix operator**.

#### Syntax

```wado
// Basic usage - returns bool
let is_some = opt matches { Some(_) };
let is_circle = shape matches { Circle(_) };

// Multiple patterns with |
let is_primary = color matches { Red | Blue };

// With guard (uses `&&` for natural left-to-right reading)
let is_positive = opt matches { Some(x) && x > 0 };

// In conditions
if shape matches { Circle(r) && r > 5.0 } {
    println("large circle");
}

// Negation
if !(opt matches { None }) {
    println("has value");
}

// Chained conditions
if opt matches { Some(x) } && x > 0 {
    // Note: x is NOT in scope here (bindings don't escape)
    // Use match or if let for value extraction
}
```

Syntax: `<expr> matches { <pattern> [&& <guard>] }`

The `matches` operator:

- Returns `bool`
- Pattern bindings are scoped to the guard expression only (don't escape)
- Uses `&&` for guards to reflect natural left-to-right evaluation order

#### Why `&&` Instead of `if` or `when`

Guards use `&&` operator instead of `if` or `when` keywords:

- **Natural evaluation order**: `Some(x) && x > 0` reads as "Some(x) AND x > 0" - left to right matches actual evaluation
- **Avoids misleading syntax**: `if`/`when` suggest the condition is checked first, but pattern matching happens first
- **Familiar operator**: `&&` is already well-understood as logical AND
- **Consistent semantics**: Pattern match is like a boolean check, so `&&` with guard is natural

#### Alternatives Considered

##### Alternative A: Prefix `matches` Keyword

```wado
let is_some = matches opt { Some(_) };
let is_positive = matches opt { Some(x) && x > 0 };
```

Pros:

- Clean, dedicated syntax
- No precedence issues

Cons:

- Less natural English reading order
- Prefix style is unusual for binary operations

##### Alternative B: `is` Operator (MoonBit-style)

```wado
let is_some = opt is Some(_);
let is_circle = shape is Circle(_);
```

Pros:

- Very concise
- Familiar from MoonBit/TypeScript/Kotlin

Cons:

- Guard syntax awkward: `opt is (Some(x) && x > 0)`
- Precedence issues with `&&` and `||`
- MoonBit allows bindings to escape into `&&`, which has unclear scoping

##### Alternative C: `if let` Extension

```wado
let is_some = (let Some(_) = opt);
```

Pros:

- No new keywords

Cons:

- Awkward parentheses
- Confusing that `let` returns bool

##### Alternative D: Single-arm `match`

```wado
let is_some = match opt { Some(_) };
```

Pros:

- Reuses existing keyword

Cons:

- Overloads `match` semantics confusingly
- Unclear what non-matching case returns

### Part 3: Other Languages Comparison

| Language    | Boolean Check Syntax            | Guard Syntax         | Bindings Escape? |
| ----------- | ------------------------------- | -------------------- | ---------------- |
| **Wado**    | `x matches { Pattern }`         | **`&&`**             | No               |
| **MoonBit** | `x is Pattern`                  | `if cond` in match   | Yes, into `&&`   |
| **Rust**    | `matches!(x, P if g)`           | `if` inside macro    | No               |
| **Swift**   | `if case P = x` / `~=`          | comma-separated      | Yes, in scope    |
| **Kotlin**  | `x is Type` + smart cast        | conditions in `when` | Yes, smart cast  |
| **Scala**   | `cond(x) { case P => }`         | `if` after pattern   | No               |
| **OCaml**   | match with bool return          | `when`               | No               |
| **F#**      | match with bool return          | `when`               | No               |
| **Haskell** | pattern guards `\| pat <- expr` | `\|` chains          | Yes, in chain    |

Key insights:

- **Wado** uses `&&` for guards - reflects actual evaluation order (pattern first, then guard)
- **OCaml/F#** use `when`, **Rust** uses `if` - both suggest wrong evaluation order
- **MoonBit** `is` is concise but bindings escaping into `&&` has unclear scoping
- **Rust** `matches!` requires macro; Wado provides this as language feature

### Part 4: Examples

```wado
// === Match Expression ===
let result = match shape {
    Circle(r) => 3.14159 * r * r,
    Rectangle([w, h]) => w * h,
    Point => 0.0,
};

// === Match Statement ===
match command {
    Start => engine.start(),
    Stop => engine.stop(),
    Pause => engine.pause(),
}

// === Pattern with guard (uses `&&`) ===
let discount = match customer {
    Premium(years) && years > 5 => 0.3,
    Premium(_) => 0.2,
    Regular => 0.1,
    _ => 0.0,
};

// === Or patterns ===
match key {
    "quit" | "exit" | "q" => {
        should_exit = true;
    },
    "help" | "h" | "?" => {
        show_help();
    },
    _ => {
        println("unknown command");
    },
}

// === Nested patterns ===
match result {
    Ok([first, _]) => println(`first: {first}`),
    Ok([]) => println("empty"),
    Err(msg) => println(`error: {msg}`),
}

// === Matches Infix Operator ===
let is_circle = shape matches { Circle(_) };
let is_large = shape matches { Circle(r) && r > 10.0 };

if opt matches { Some(_) } {
    println("has value");
}

// Combined with other conditions
if shape matches { Circle(_) } && should_draw {
    draw_circle(shape);
}
```

### Part 5: Grammar

```ebnf
match_expr ::= "match" expr "{" match_arm* "}"

match_arm ::= pattern ("&&" expr)? "=>" arm_body ","?

arm_body ::= expr
           | block

matches_expr ::= expr "matches" "{" pattern ("&&" expr)? "}"

pattern ::= "_"
          | ident
          | literal
          | tuple_pattern
          | variant_pattern
          | pattern "|" pattern

tuple_pattern ::= "[" pattern ("," pattern)* ","? "]"

variant_pattern ::= ident ("(" pattern ")")?
                  | path "::" ident ("(" pattern ")")?
```

Note:

- Guards use `&&` operator for natural left-to-right evaluation order reading
- Trailing comma after each arm is optional
- Trailing semicolon inside block bodies follows Wado's common block rules (optional, doesn't change semantics)

### Part 6: Semantic Rules

#### Exhaustiveness Checking

1. For variant types: all cases must be covered, or `_` wildcard present
2. For primitive types: `_` wildcard required (or all possible values, impractical)
3. For tuple types: component patterns must be exhaustive

#### Pattern Binding Scope

```wado
// In match: bindings scoped to arm body
match opt {
    Some(x) => println(`{x}`),  // x in scope here
    None => println("none"),    // x NOT in scope here
}
// x NOT in scope here

// In matches: bindings scoped to guard only, don't escape
let is_positive = opt matches { Some(x) && x > 0 };
// x NOT in scope here (pattern variables are internal)

// This does NOT work (unlike MoonBit):
if opt matches { Some(x) } && x > 0 {  // ERROR: x not in scope
    // ...
}

// Use if let or match for value extraction:
if let Some(x) = opt {
    if x > 0 {
        // ...
    }
}
```

#### Type Inference

```wado
// Match arms must have compatible types for expression form
let x = match opt {
    Some(v) => v,      // i32
    None => 0,         // i32 - OK, compatible
};

// Different types = error
let x = match opt {
    Some(v) => v,      // i32
    None => "none",    // String - ERROR: incompatible types
};

// Statement form: no type unification needed
match opt {
    Some(v) => { println(`{v}`) },
    None => { return },  // Different "return types" OK
}
```

## Consequences

### Positive

- Exhaustive matching catches missing cases at compile time
- Rust-like syntax is familiar to many developers
- Consistent with Wado's tuple `[T, U]` and variant syntax
- Consistent with Wado's common block rules (trailing semicolons optional)
- `&&` for guards reflects natural left-to-right evaluation order
- `matches` infix operator reads naturally: `opt matches { Some(_) }`
- Clear scoping: pattern bindings don't escape `matches` expression

### Negative

- Exhaustiveness checking requires careful implementation
- Or-patterns and guards add parser complexity
- `matches` keyword adds to language surface
- `&&` for guards differs from Rust's `if` (minor learning curve for Rust users)

### Implementation Order

1. Basic `match` expression with single patterns
2. Exhaustiveness checking for variants
3. Or-patterns (`|`)
4. Guard expressions (`&&`)
5. `matches` infix operator
6. Optimizations (jump tables for dense patterns)

## Alternatives Considered

### `case` Instead of `match`

```wado
let x = case opt {
    Some(v) => v,
    None => 0,
};
```

Rejected: `match` is more widely recognized and avoids confusion with `switch/case`.

### `if` or `when` Keywords for Guards

```wado
// Rust-style
match opt {
    Some(x) if x > 0 => "positive",
    ...
}

// OCaml/F#-style
match opt {
    Some(x) when x > 0 => "positive",
    ...
}
```

Rejected: Both `if` and `when` suggest the condition is evaluated first, but pattern matching happens first. `&&` reflects the actual left-to-right evaluation order: pattern match AND guard condition.

### `else` Arm Instead of `_`

```wado
match color {
    Red => "red",
    else => "other",
}
```

Rejected: `_` is more consistent with pattern syntax and more widely used.

### `is` Operator for Matches

```wado
let is_some = opt is Some(_);
```

Rejected: Guard syntax becomes awkward, and MoonBit-style binding escape has unclear scoping.

### Prefix `matches` Keyword

```wado
let is_some = matches opt { Some(_) };
```

Rejected: Infix `opt matches { ... }` reads more naturally in English.
