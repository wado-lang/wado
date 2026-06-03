# Compile-Time Location Literals

## Context

Debugging and logging often require source location information. Languages handle this differently:

- C/C++: `__FILE__`, `__LINE__` (preprocessor macros)
- Swift: `#file`, `#line`, `#function` (literal expressions)
- Rust: `file!()`, `line!()` (built-in macros)

Wado has no macros and no preprocessor, so we need a distinct syntax that clearly signals compile-time evaluation.

## Decision

Introduce compile-time literals with `#` prefix:

| Literal     | Type     | Value                                              |
| ----------- | -------- | -------------------------------------------------- |
| `#file`     | `String` | Current source file path                           |
| `#line`     | `i32`    | Current line number (1-indexed)                    |
| `#function` | `String` | Fully specialized function name                    |
| `#data`     | `String` | `__DATA__` section content (compile error if none) |

### Syntax

```wado
fn example() {
    println(`Error at {#file}:{#line}`);
    println(`In function: {#function}`);
}
```

### `#function` Format

Returns the fully specialized name without signature:

| Context        | `#function` value            |
| -------------- | ---------------------------- |
| Free function  | `my_function`                |
| Method         | `Point::distance`            |
| Generic method | `List<String>::len`          |
| Closure        | `parent_function::{closure}` |

### `#data`

Returns the raw text content of the file's `__DATA__` section as a `String`. Using `#data` in a source file that has no `__DATA__` section is a compile error. This allows programs to embed and access static data inline without a separate data file.

```wado
export fn run() with Stdout {
    let config = #data;
    println(config);
}

__DATA__
{"key": "value"}
```

### Not Included

- `#column` - No compelling use case for user code
- `#module` - Equivalent to `#file` in Wado's module system

### Future: Line Directives

If line directives become necessary (e.g., for code generators), use inner attribute syntax to avoid conflict:

```wado
#![line = 123]
#![file = "./original_source.wado"]
```

## Consequences

- Simple, explicit syntax for source location and embedded data
- `#` prefix is consistent with existing attributes (`#[...]`, `#![...]`)
- No conflict with potential future directives
- Useful for debugging, logging, assertion messages, and inline data embedding
- `#data` reuses the already-existing `__DATA__` section mechanism with no additional syntax overhead
