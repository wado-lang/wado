# WEP: Type Stringification

## Context

When debugging or logging, developers need to convert arbitrary values to strings. Different languages handle this differently:

| Language   | Default Output               | Customization            | Escape Hatch            |
| ---------- | ---------------------------- | ------------------------ | ----------------------- |
| **Rust**   | None (trait required)        | `Display`/`Debug` traits | `{:?}` for Debug        |
| **Elixir** | All types inspectable        | `Inspect` protocol       | `structs: false` option |
| **Python** | `<Foo at 0x...>` (unhelpful) | `__str__`/`__repr__`     | `repr()`                |
| **Ruby**   | Detailed inspect             | `to_s`/`inspect`         | `p obj`                 |
| **Kotlin** | data class only              | `toString()` override    | None                    |

### Design Goals

1. **Always works**: Any type should be printable without explicit trait implementation
2. **Customizable**: Allow user-friendly display when needed
3. **Escape hatch**: Always able to see raw structure for debugging
4. **No macros**: Wado does not have macros, so derive-style auto-implementation is not available

## Decision

### 1. Built-in `inspect` Function

A compiler-generated intrinsic function that converts any value to its debug string representation:

```wado
// Available as builtin::inspect()
let p = Point { x: 10, y: 20 };
let s = builtin::inspect(p);  // => "Point { x: 10, y: 20 }"
```

The compiler automatically generates inspect logic for all types. This is not a trait method but a compiler intrinsic that has access to type information at compile time.

### 2. Template String Behavior

Template strings support two interpolation modes:

| Syntax     | Behavior                                  | Use Case       |
| ---------- | ----------------------------------------- | -------------- |
| `{expr}`   | Display if implemented, otherwise inspect | General output |
| `{expr:?}` | Always inspect (compiler-generated)       | Debugging      |

```wado
struct Point { x: i32, y: i32 }

let p = Point { x: 10, y: 20 };

// Without Display trait
println(`{p}`);      // => "Point { x: 10, y: 20 }" (fallback to inspect)
println(`{p:?}`);    // => "Point { x: 10, y: 20 }" (explicit inspect)

// With Display trait
impl Display for Point {
    fn display(&self) -> String {
        return `({self.x}, {self.y})`;
    }
}

println(`{p}`);      // => "(10, 20)" (Display)
println(`{p:?}`);    // => "Point { x: 10, y: 20 }" (still inspect)
```

### 3. Output Format (Wado Syntax)

The inspect output follows Wado literal syntax, making it familiar and potentially copy-pasteable:

```wado
// Primitives
builtin::inspect(42)           // => "42"
builtin::inspect(3.14)         // => "3.14"
builtin::inspect(true)         // => "true"
builtin::inspect('A')          // => "'A'"
builtin::inspect("hello")      // => "\"hello\""

// Structs
builtin::inspect(Point { x: 1, y: 2 })
// => "Point { x: 1, y: 2 }"

// Generic structs
builtin::inspect(Box { value: 42 })
// => "Box { value: 42 }"

// Arrays
builtin::inspect([1, 2, 3] as List<i32>)
// => "[1, 2, 3]"

// Tuples
builtin::inspect([1, "a", true])
// => "[1, \"a\", true]"

// Option
builtin::inspect(null)              // => "null"
builtin::inspect(Option::Some(42))  // => "Some(42)"

// Nested structures
builtin::inspect(Box { value: Point { x: 1, y: 2 } })
// => "Box { value: Point { x: 1, y: 2 } }"
```

### 4. Display Trait

The `Display` trait (defined in `wep-2026-01-13-struct-and-trait.md`) provides user-customizable string representation:

```wado
trait Display {
    fn display(&self) -> String;
}
```

Standard library types implement `Display`:

```wado
// Primitives have built-in Display (via to_string methods)
println(`{42}`);        // => "42"
println(`{3.14}`);      // => "3.14"
println(`{true}`);      // => "true"

// String displays without quotes
println(`{"hello"}`);   // => "hello" (not "\"hello\"")

// Arrays use inspect format (no Display by default)
let arr: List<i32> = [1, 2, 3];
println(`{arr}`);       // => "[1, 2, 3]" (fallback to inspect)
```

### 5. Resolution Order for `{expr}`

When evaluating `{expr}` in a template string:

1. If `expr` is `String` type → use directly
2. If `expr` has `Display` trait → call `display()`
3. Otherwise → call `builtin::inspect()`

This ensures template strings always produce output without requiring explicit trait implementations.

### 6. Comparison with Rust

| Aspect                 | Rust                            | Wado                                  |
| ---------------------- | ------------------------------- | ------------------------------------- |
| `{x}` without trait    | Compile error                   | Falls back to inspect                 |
| `{x:?}` without trait  | Compile error                   | Always works (compiler intrinsic)     |
| Debug implementation   | `#[derive(Debug)]` macro        | Compiler-generated inspect            |
| Display implementation | Manual `impl Display`           | Manual `impl Display`                 |
| Inspect function       | `dbg!` macro (prints file:line) | `builtin::inspect()` (returns String) |

## Consequences

### Positive

1. **Zero-friction debugging**: `println(\`{any_value}\`)` always works
2. **Progressive enhancement**: Add `Display` only when user-facing output matters
3. **Reliable escape hatch**: `{x:?}` always shows structure, even if `Display` is buggy
4. **No macro dependency**: Works without macro system
5. **Familiar to Elixir/Ruby developers**: Similar "always inspectable" philosophy

### Negative

1. **Code size**: Compiler must generate inspect code for all types
   - **Mitigation**: Tree-shaking can remove unused inspect code
2. **Implicit fallback may hide missing Display**: Developer might not notice they forgot to implement Display
   - **Mitigation**: Linter warning for public API types without Display

### Implemented Extensions

1. **Pretty-print specifier**: `{x:#?}` for indented multi-line output via `InspectAlt` trait

### Future Extensions

1. **Depth limit**: `{x:?3}` to limit nesting depth
2. **Width limit**: Integration with format specifiers like `{x:?.80}` for truncation

## References

- [WEP: Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Elixir Inspect Protocol](https://hexdocs.pm/elixir/Inspect.html)
- [Python **repr** vs **str**](https://realpython.com/python-repr-vs-str/)
- [Ruby inspect](https://thoughtbot.com/blog/ruby-inspect-tutorial)
- [Rust Debug and Display](https://doc.rust-lang.org/rust-by-example/hello/print/print_debug.html)
