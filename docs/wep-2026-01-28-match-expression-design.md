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

    // Guard expressions
    Some(x) if x > 0 => ...,
    [a, b] if a == b => ...,
}
```

#### Expression vs Statement

**Proposal A: Unified Syntax (Rust-like)**

```wado
// Expression: no trailing semicolons inside, comma or nothing between arms
let x = match opt {
    Some(v) => v * 2,
    None => 0,
};

// Statement: same syntax, just not assigned
match opt {
    Some(v) => println(`{v}`),
    None => println("none"),
}

// Block bodies allowed
match opt {
    Some(v) => {
        let doubled = v * 2;
        println(`{doubled}`);
    },
    None => {
        println("none");
    },
}
```

Pros:
- Familiar to Rust users
- One syntax to learn
- Consistent with if expression

Cons:
- Block bodies need trailing comma which can be confusing
- Statement match still looks like expression

**Proposal B: Distinct Statement Form**

```wado
// Expression: uses =>
let x = match opt {
    Some(v) => v * 2,
    None => 0,
};

// Statement: uses -> and semicolons
match opt {
    Some(v) -> { println(`{v}`); };
    None -> { println("none"); };
}
```

Pros:
- Clear visual distinction
- Statement form is more natural for side effects

Cons:
- Two syntaxes to learn
- `->` conflicts with potential future arrow type syntax

**Proposal C: Semicolon Determines Form (Recommended)**

```wado
// Expression: arms end with comma (or nothing)
let x = match opt {
    Some(v) => v * 2,
    None => 0,
};

// Statement: arms end with semicolon
match command {
    Start => { engine.start(); };
    Stop => { engine.stop(); };
}
```

Pros:
- Natural extension of Wado's existing semicolon rules
- Clear when you want value vs side effect
- Single `=>` syntax
- Follows "trailing semicolon is optional" pattern like other blocks

Cons:
- Subtle difference (comma vs semicolon)

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

### Part 2: `matches` Functionality

Since Wado has no macros, `matches!` must be a language feature. Three proposals:

#### Proposal A: `matches` Keyword (Recommended)

```wado
// Basic usage - returns bool
let is_some = matches opt { Some(_) };
let is_circle = matches shape { Circle(_) };

// Multiple patterns with |
let is_primary = matches color { Red | Blue };

// With guard
let is_positive = matches opt { Some(x) if x > 0 };

// In conditions
if matches shape { Circle(r) if r > 5.0 } {
    println("large circle");
}

// Negation
if !matches opt { None } {
    println("has value");
}
```

Syntax: `matches <expr> { <pattern> [if <guard>] }`

Pros:
- Clean, dedicated syntax
- No ambiguity with other constructs
- Reads naturally: "matches opt with Some(_)"

Cons:
- New keyword to add
- Different from other languages

#### Proposal B: `is` Operator

```wado
// Returns bool
let is_some = opt is Some(_);
let is_circle = shape is Circle(_);

// Multiple patterns
let is_primary = color is (Red | Blue);

// With guard (requires parentheses)
let is_positive = opt is (Some(x) if x > 0);

// In conditions
if shape is Circle(r) && r > 5.0 {
    println("large circle");
}

// Negation
if opt is !None {  // or: !(opt is None)
    println("has value");
}
```

Syntax: `<expr> is <pattern>`

Pros:
- Very concise
- Familiar from TypeScript/Kotlin
- Reads naturally: "opt is Some(_)"

Cons:
- Guard syntax awkward: `opt is (Some(x) if x > 0)`
- Precedence issues with `&&` and `||`
- Pattern bindings scope unclear

#### Proposal C: `if let` Extension with `else` Check

```wado
// Current: if let Some(x) = opt { ... }

// Extension: if let pattern as bool expression
let is_some = (let Some(_) = opt);

// Multiple patterns
let is_primary = (let Red | Blue = color);

// With guard
let is_positive = (let Some(x) = opt if x > 0);
```

Pros:
- Reuses existing `let` pattern syntax
- No new keywords

Cons:
- Awkward syntax with parentheses
- Confusing that `let` returns bool in this context
- Inconsistent with normal `let` which doesn't return value

#### Proposal D: `match` as Expression with Single Arm

```wado
// match with single arm returns bool if pattern matches
let is_some = match opt { Some(_) };
let is_circle = match shape { Circle(_) };

// Multiple patterns
let is_primary = match color { Red | Blue };

// Full match still works
let x = match opt {
    Some(v) => v,
    None => 0,
};
```

Pros:
- Reuses `match` keyword
- No new syntax to learn
- Consistent with match expression

Cons:
- Overloading `match` semantics
- Single-arm match returning bool vs multi-arm returning value is confusing
- What does `match opt { Some(_) }` return for None? (false, but not obvious)

### Part 3: Comparison Summary

#### Match Expression/Statement

| Aspect | Proposal A (Rust-like) | Proposal B (Distinct) | Proposal C (Semicolon) |
|--------|------------------------|----------------------|------------------------|
| Learning curve | Low for Rust users | Medium | Low |
| Visual clarity | Medium | High | Medium-High |
| Consistency | High | Low | High |
| Implementation | Simple | Complex | Simple |

**Recommendation: Proposal C** - Semicolon determines form. Natural extension of Wado's design.

#### Matches Functionality

| Aspect | Proposal A (matches) | Proposal B (is) | Proposal C (let) | Proposal D (match) |
|--------|---------------------|-----------------|------------------|-------------------|
| Readability | High | Very High | Low | Medium |
| Conciseness | Medium | High | Low | Medium |
| Guard support | Good | Awkward | Awkward | N/A |
| New keyword | Yes | Yes | No | No |
| Precedence | Clear | Complex | Clear | Clear |

**Recommendation: Proposal A** (`matches` keyword) - Best balance of clarity and functionality.

### Part 4: Combined Design (Recommended)

```wado
// === Match Expression ===
let result = match shape {
    Circle(r) => 3.14159 * r * r,
    Rectangle([w, h]) => w * h,
    Point => 0.0,
};

// === Match Statement ===
match command {
    Start => { engine.start(); };
    Stop => { engine.stop(); };
    Pause => { engine.pause(); };
}

// === Matches Expression ===
let is_circle = matches shape { Circle(_) };
let is_large = matches shape { Circle(r) if r > 10.0 };

if matches opt { Some(_) } {
    println("has value");
}

// === Combined Examples ===

// Pattern with guard
let discount = match customer {
    Premium(years) if years > 5 => 0.3,
    Premium(_) => 0.2,
    Regular => 0.1,
    _ => 0.0,
};

// Or patterns
match key {
    "quit" | "exit" | "q" => { should_exit = true; };
    "help" | "h" | "?" => { show_help(); };
    _ => { println("unknown command"); };
}

// Nested patterns
match result {
    Ok([first, _]) => println(`first: {first}`),
    Ok([]) => println("empty"),
    Err(msg) => println(`error: {msg}`),
}
```

### Part 5: Grammar

```ebnf
match_expr ::= "match" expr "{" match_arm* "}"

match_arm ::= pattern ("if" expr)? "=>" arm_body

arm_body ::= expr ","?        (* expression arm *)
           | block ";"?       (* statement arm *)

matches_expr ::= "matches" expr "{" pattern ("if" expr)? "}"

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

// In matches: no bindings escape (like guard context)
let is_positive = matches opt { Some(x) if x > 0 };
// x NOT in scope here (pattern variables are internal)
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
    Some(v) => { println(`{v}`); };
    None => { return; };  // Different "return types" OK
}
```

## Consequences

### Positive

- Exhaustive matching catches missing cases at compile time
- `matches` provides concise boolean pattern checks
- Consistent with Wado's tuple `[T, U]` and variant syntax
- Clear distinction between expression and statement forms

### Negative

- New `matches` keyword adds to language surface
- Exhaustiveness checking requires careful implementation
- Or-patterns and guards add parser complexity

### Implementation Order

1. Basic `match` expression with single patterns
2. Exhaustiveness checking for variants
3. Or-patterns (`|`)
4. Guard expressions (`if`)
5. `matches` keyword
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

### `when` Keyword for Guards

```wado
match opt {
    Some(x) when x > 0 => "positive",
    Some(_) => "non-positive",
    None => "none",
}
```

Rejected: `if` is more familiar and Rust-compatible.

### `else` Arm Instead of `_`

```wado
match color {
    Red => "red",
    else => "other",
}
```

Rejected: `_` is more consistent with pattern syntax and more widely used.
