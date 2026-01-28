# Match Expression Design

## Context

Wado needs `match` expressions/statements for exhaustive pattern matching on variants and other types. Additionally, since Wado has no macros, `matches!` functionality must be provided as a language feature.

### Current State

- `if let` pattern matching is implemented for Option and custom variants
- `while let` and `for let` patterns work
- Tuple destructuring with `let [a, b] = ...` is implemented
- AST structure for `MatchExpr` exists but parser doesn't handle it yet
- `Match` token is recognized by the lexer

### Key Differences from Rust

| Aspect | Rust | Wado |
|--------|------|------|
| Tuple type/literal | `(T, U)` | `[T, U]` |
| Multiple payload | `Foo(T, U)` | `Foo([T, U])` |
| Struct variant | `Foo { a: T }` | `Foo({ a: T })` |

## Decision

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

    // Guard expressions (uses `when` keyword, not `if`)
    Some(x) when x > 0 => ...,
    [a, b] when a == b => ...,
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

// With guard (uses `when`, not `if`)
let is_positive = opt matches { Some(x) when x > 0 };

// In conditions
if shape matches { Circle(r) when r > 5.0 } {
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

Syntax: `<expr> matches { <pattern> [when <guard>] }`

The `matches` operator:
- Returns `bool`
- Pattern bindings are scoped to the guard expression only (don't escape)
- Uses `when` for guards to avoid confusion with `if` expressions

#### Why `when` Instead of `if`

Guards use `when` keyword instead of `if` to:
- Avoid confusion with `if` expressions and statements
- Follow OCaml/F# precedent where `when` is the standard guard keyword
- Make the guard syntactically distinct from conditional expressions

#### Alternatives Considered

##### Alternative A: Prefix `matches` Keyword

```wado
let is_some = matches opt { Some(_) };
let is_positive = matches opt { Some(x) when x > 0 };
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
- Guard syntax awkward: `opt is (Some(x) when x > 0)`
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

| Language | Boolean Check Syntax | Guard Syntax | Bindings Escape? |
|----------|---------------------|--------------|------------------|
| **MoonBit** | `x is Pattern` | `if cond` in match | Yes, into `&&` |
| **Rust** | `matches!(x, P if g)` | `if` inside macro | No |
| **Swift** | `if case P = x` / `~=` | comma-separated | Yes, in scope |
| **Kotlin** | `x is Type` + smart cast | conditions in `when` | Yes, smart cast |
| **Scala** | `cond(x) { case P => }` | `if` after pattern | No |
| **OCaml** | match with bool return | **`when`** | No |
| **F#** | match with bool return | **`when`** | No |
| **Haskell** | pattern guards `\| pat <- expr` | `\|` chains | Yes, in chain |

Key insights:
- **OCaml/F#** use `when` for guards (not `if`)
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

// === Pattern with guard (uses `when`) ===
let discount = match customer {
    Premium(years) when years > 5 => 0.3,
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
let is_large = shape matches { Circle(r) when r > 10.0 };

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

match_arm ::= pattern ("when" expr)? "=>" arm_body ","?

arm_body ::= expr
           | block

matches_expr ::= expr "matches" "{" pattern ("when" expr)? "}"

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
- Guards use `when` keyword (not `if`) to distinguish from conditional expressions
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
let is_positive = opt matches { Some(x) when x > 0 };
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
- `when` for guards avoids confusion with `if` expressions
- `matches` infix operator reads naturally: `opt matches { Some(_) }`
- Clear scoping: pattern bindings don't escape `matches` expression

### Negative

- Exhaustiveness checking requires careful implementation
- Or-patterns and guards add parser complexity
- `matches` keyword adds to language surface
- `when` differs from Rust's `if` (minor learning curve for Rust users)

### Implementation Order

1. Basic `match` expression with single patterns
2. Exhaustiveness checking for variants
3. Or-patterns (`|`)
4. Guard expressions (`when`)
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

### `if` Keyword for Guards (Rust-style)

```wado
match opt {
    Some(x) if x > 0 => "positive",
    Some(_) => "non-positive",
    None => "none",
}
```

Rejected: `when` avoids confusion with `if` expressions/statements. OCaml/F# precedent.

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
