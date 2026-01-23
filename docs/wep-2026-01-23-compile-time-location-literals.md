# Compile-Time Location Literals

## Context

Debugging and logging often require source location information. Languages handle this differently:

- C/C++: `__FILE__`, `__LINE__` (preprocessor macros)
- Swift: `#file`, `#line`, `#function` (literal expressions)
- Rust: `file!()`, `line!()` (built-in macros)

Wado has no macros and no preprocessor, so we need a distinct syntax that clearly signals compile-time evaluation.

## Decision

Introduce three compile-time location literals with `#` prefix:

| Literal     | Type     | Value                           |
| ----------- | -------- | ------------------------------- |
| `#file`     | `String` | Current source file path        |
| `#line`     | `i32`    | Current line number (1-indexed) |
| `#function` | `String` | Fully specialized function name |

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
| Generic method | `Array<String>::len`         |
| Closure        | `parent_function::{closure}` |

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

- Simple, explicit syntax for source location
- `#` prefix is consistent with existing attributes (`#[...]`, `#![...]`)
- No conflict with potential future directives
- Useful for debugging, logging, and assertion messages
