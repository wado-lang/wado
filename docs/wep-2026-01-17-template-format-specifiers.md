# WEP: Template Format Specifiers

## Context

Template strings in Wado support interpolation with format specifiers: `` `value: {x:spec}` ``. Different languages provide different sets of format specifiers:

### Language Comparison

| Feature                   | Go         | Python     | Rust           | Wado    |
| ------------------------- | ---------- | ---------- | -------------- | ------- |
| Fixed-point float         | `%f`, `%F` | `:f`, `:F` | `{}` (default) | `{}`    |
| Exponential (lower)       | `%e`       | `:e`       | `{:e}`         | `{:e}`  |
| Exponential (upper)       | `%E`       | `:E`       | `{:E}`         | `{:E}`  |
| General format (adaptive) | `%g`, `%G` | `:g`, `:G` | ❌ None        | ❌ None |
| Binary                    | `%b`       | `:b`       | `{:b}`         | `{:b}`  |
| Octal                     | `%o`       | `:o`       | `{:o}`         | `{:o}`  |
| Hex (lower)               | `%x`       | `:x`       | `{:x}`         | `{:x}`  |
| Hex (upper)               | `%X`       | `:X`       | `{:X}`         | `{:X}`  |
| Pointer                   | `%p`       | N/A        | `{:p}`         | ❌ None |
| Debug                     | `%#v`      | `!r`       | `{:?}`         | `{:?}`  |

### The Problem with General Format (`g`/`G`)

C's `%g` format specifier automatically switches between fixed-point and exponential notation based on magnitude. While convenient, this has been identified as a "negative legacy" from C:

- **Unpredictable output**: Values unexpectedly switch to scientific notation
- **Production issues**: Common source of bugs when output format matters
- **User confusion**: Developers often encounter this multiple times

Rust's design decision was to **exclude** `g`/`G` format specifiers entirely. Instead:

1. **RFC #844 (2015)**: Community requested `%g` equivalent for prettier float output
2. **RFC #2729**: Proposed `{:g?}` debug formatter
3. **Final solution (Rust 1.58)**: Modified `{:?}` (Debug) to automatically use exponential notation for very large/small floats, without adding a separate format specifier

This avoided the unpredictability of `g` while still providing readable output for debugging.

## Decision

**Wado will use Rust-compatible format specifiers**, following Rust's philosophy of explicit, predictable formatting.

### Format Specifier Syntax

```wado
`{expr}`           // Default (Display trait)
`{expr:spec}`      // With format specifier
```

Where `spec` follows Rust's format specification mini-language.

### Supported Format Types

| Specifier | Trait      | Description           | Example              |
| --------- | ---------- | --------------------- | -------------------- |
| (none)    | Display    | Default display       | `{x}` → `"42"`       |
| `?`       | Inspect    | Debug representation  | `{x:?}` → `"42"`     |
| `#?`      | InspectAlt | Pretty-print debug    | `{x:#?}` (indented)  |
| `b`       | Binary     | Binary integers       | `{x:b}` → `"101010"` |
| `o`       | Octal      | Octal integers        | `{x:o}` → `"52"`     |
| `x`       | LowerHex   | Lowercase hex         | `{x:x}` → `"2a"`     |
| `X`       | UpperHex   | Uppercase hex         | `{x:X}` → `"2A"`     |
| `e`       | LowerExp   | Lowercase exponential | `{x:e}` → `"4.2e1"`  |
| `E`       | UpperExp   | Uppercase exponential | `{x:E}` → `"4.2E1"`  |

**Notes**:

- No `g`/`G` format specifiers. Users must explicitly choose between default `{}` and exponential `{:e}/{:E}`.
- No `p` (Pointer) format specifier. Wado does not have pointer types.

### Format Parameters

Format specifiers can include additional parameters in this order:

```
{expr:[[fill]align][sign][#][0][width][.precision]type}
```

#### Alignment and Width

| Syntax    | Description                | Example    | Output    |
| --------- | -------------------------- | ---------- | --------- |
| `{x:5}`   | Width 5, default align     | `{42:5}`   | `"   42"` |
| `{x:<5}`  | Left align                 | `{42:<5}`  | `"42   "` |
| `{x:^5}`  | Center align               | `{42:^5}`  | `" 42  "` |
| `{x:>5}`  | Right align                | `{42:>5}`  | `"   42"` |
| `{x:05}`  | Zero-padding               | `{42:05}`  | `"00042"` |
| `{x:_>5}` | Fill with `_`, right align | `{42:_>5}` | `"___42"` |

#### Precision

For floating-point numbers, precision specifies decimal places:

```wado
println(`{3.14159:.2}`);    // => "3.14"
println(`{100.0:.4}`);      // => "100.0000"
println(`{1.23e5:.1e}`);    // => "1.2e5"
```

For integers in exponential notation, precision specifies significant digits:

```wado
println(`{12345:.2e}`);     // => "1.23e4"
```

For strings, precision specifies the maximum length in characters, matching
Rust's `{:.N}`. `Display` truncates silently; `Inspect` (`:?`) truncates to the
same characters and appends a `...` marker (outside the quotes) when truncation
actually happened. The marker does not count toward the precision:

```wado
let s = "hello world";
println(`{s:.5}`);          // => "hello"
println(`{s:.5?}`);         // => "hello"...   (note: with surrounding quotes)
println(`{s:.20}`);         // => "hello world" (precision >= length: unchanged)
```

For arrays, precision specifies the maximum number of elements. `Display`
truncates silently; `Inspect` appends `, ...` when elements are dropped:

```wado
let a: List<i32> = [1, 2, 3, 4, 5];
println(`{a:.3?}`);         // => "[1, 2, 3, ...]"
println(`{a:.3}`);          // => "[1, 2, 3]"
```

The truncation point is uniform across collection-like types: precision caps the
number of rendered items (characters for `String`, elements for `List`). A
shared `Formatter` flows into nested element rendering, so an explicit precision
also bounds elements inside collections (e.g. long `String` elements of an
`List`). Tuples and structs are fixed-size, so their own arity is never capped,
but their `String` / `List` fields still honor the active precision.

##### Default Inspect cap and the precision sentinels

`Inspect` / `InspectAlt` (`:?` / `:#?`) of a `String` or `List` apply a default
length cap of `DEFAULT_SEQ_LIMIT` (256 items) even when no precision is given, so
debug output — including power-assert operand dumps — stays readable. `Display`
never applies the default cap: a plain `{s}` renders the full value.

To make this work, `Formatter.precision` uses two negative sentinels instead of a
single "unspecified" value:

- `PRECISION_DEFAULT` (-2): no precision in the spec. Sequence `Inspect` falls
  back to `DEFAULT_SEQ_LIMIT`; every other type (floats, ints, `Display`) treats
  it the same as unset (its `precision >= 0` check stays false).
- `PRECISION_INFINITE` (-1): render with no cap; sequence `Inspect` skips
  truncation. There is no surface syntax for it yet (the `.N` grammar cannot
  produce a negative value), so it is currently reachable only by constructing a
  `Formatter` directly — see the TODO in `format.wado`.

This keeps precision's meaning intact per type (decimal places for floats, max
length for sequences) while letting `:?` cap sequences by default without
affecting non-sequence operands.

#### Sign

| Syntax    | Description                 | Example   | Output  |
| --------- | --------------------------- | --------- | ------- |
| `{x:+}`   | Always show sign            | `{42:+}`  | `"+42"` |
| `{x:+}`   | Always show sign (negative) | `{-42:+}` | `"-42"` |
| (default) | Only negative sign          | `{42}`    | `"42"`  |

#### Alternate Form

The `#` flag provides alternate representations:

| Type     | Effect          | Example   | Output       |
| -------- | --------------- | --------- | ------------ |
| `x`, `X` | Add `0x` prefix | `{42:#x}` | `"0x2a"`     |
| `b`      | Add `0b` prefix | `{42:#b}` | `"0b101010"` |
| `o`      | Add `0o` prefix | `{42:#o}` | `"0o52"`     |

### Complete Examples

```wado
// Basic formatting
let x = 42;
println(`{x}`);          // => "42"
println(`{x:?}`);        // => "42"
println(`{x:b}`);        // => "101010"
println(`{x:o}`);        // => "52"
println(`{x:x}`);        // => "2a"
println(`{x:X}`);        // => "2A"
println(`{x:#x}`);       // => "0x2a"

// Width and alignment
println(`{x:5}`);        // => "   42"
println(`{x:<5}`);       // => "42   "
println(`{x:^5}`);       // => " 42  "
println(`{x:>5}`);       // => "   42"
println(`{x:05}`);       // => "00042"

// Floating-point
let pi = 3.14159;
println(`{pi}`);         // => "3.14159"
println(`{pi:.2}`);      // => "3.14"
println(`{pi:+.2}`);     // => "+3.14"
println(`{pi:8.2}`);     // => "    3.14" (width 8, precision 2)
println(`{pi:e}`);       // => "3.14159e0"
println(`{pi:.2e}`);     // => "3.14e0"
println(`{pi:E}`);       // => "3.14159E0"

// Structs
let p = Point { x: 10, y: 20 };
println(`{p}`);          // => "(10, 20)" (if Display implemented)
println(`{p:?}`);        // => "Point { x: 10, y: 20 }" (inspect)

// Arbitrary expressions (Wado extension)
println(`{x + 1}`);      // => "43"
println(`{x * 2:x}`);    // => "54" (84 in hex)
println(`{p.x + p.y}`);  // => "30"
println(`{arr.len()}`);  // => "5"
```

### Interaction with Type Stringification

This WEP extends [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md):

1. **`{expr}`**: Uses Display trait, falls back to inspect
2. **`{expr:?}`**: Always uses inspect (compiler intrinsic)
3. **`{expr:spec}`**: Uses appropriate trait for `spec` (e.g., Binary, LowerHex, LowerExp)

Resolution order for `{expr:spec}`:

1. If type does not support the format trait → **compile error**
2. Apply format parameters (width, precision, alignment)
3. Return formatted String

**Key differences from Rust**:

1. **Arbitrary expressions**: Wado allows any expression in `{expr}`, while Rust only allows simple value expressions (no method calls, operators, etc. without parentheses)
2. **Inspect fallback**: Wado provides inspect as a fallback for `{}` and `{:?}`, while Rust requires explicit trait implementations
3. **Other format specifiers**: Format specifiers like `{:x}` still require the type to support that trait (or be a primitive)
4. **No `:p` specifier**: Wado excludes `:p` since it has no pointer types

**Syntax disambiguation**: The parser distinguishes `::` (scope resolution/turbofish) from `:` (format specifier) by lookahead:

- `{foo::bar}` - `::` is part of the expression
- `{foo::<T>}` - `::<` is turbofish syntax
- `{foo:x}` - `:` starts format specifier

## Consequences

### Positive

1. **Rust familiarity**: Developers familiar with Rust can immediately use Wado's format specifiers (except `:p`)
2. **Predictable output**: No surprise switches to exponential notation
3. **Explicit intent**: Developers explicitly choose the format they want
4. **Type safety**: Format specifiers that don't apply to a type cause compile errors
5. **Extensible**: Supports `{:#?}` for pretty-print and other Rust-compatible specifiers
6. **Language coherence**: Excluding `:p` aligns with Wado's lack of pointer types

### Negative

1. **No convenience format**: Developers must choose between `{}` and `{:e}` for floats
   - **Mitigation**: Default `{}` works well for most cases; `{:e}` is explicit for scientific notation
2. **Learning curve**: Developers from Python/Go may need to learn Rust's format syntax
   - **Mitigation**: Rust's format syntax is well-documented and widely used
3. **Implementation complexity**: Requires implementing all Rust format traits
   - **Mitigation**: Can implement incrementally, starting with most common specifiers

### Implemented Extensions

1. **Pretty-print**: `{:#?}` for indented multi-line debug output via `InspectAlt` trait

### Future Extensions

Following Rust's ecosystem, we can add:

1. **Dynamic width and precision**: Allow runtime values using nested `{}` syntax
   - Syntax: `{value:{width}.{precision}}` (Python-style)
   - Rationale: Python's nested `{}` approach is more consistent with Wado's "arbitrary expressions" philosophy than Rust's `$` syntax, which requires argument lists. Variables in the current scope can be referenced directly.
   - Example: `let w = 8; let p = 2; println(\`{pi:{w}.{p}}\`);`→`" 3.14"`

### Migration from Other Languages

For developers coming from other languages:

| From                      | To Wado                      | Notes                |
| ------------------------- | ---------------------------- | -------------------- |
| Python `f"{x:.2f}"`       | `` `{x:.2}` ``               | No `f` suffix needed |
| Go `fmt.Printf("%d", x)`  | `` `{x}` ``                  | Type inferred        |
| Go `fmt.Printf("%x", x)`  | `` `{x:x}` ``                | Same specifier       |
| Python `f"{x:g}"`         | `` `{x}` `` or `` `{x:e}` `` | Choose explicitly    |
| Rust `format!("{:?}", x)` | `` `{x:?}` ``                | Identical            |

## References

- [Rust std::fmt documentation](https://doc.rust-lang.org/std/fmt/)
- [RFC #844: Extend fmt to add %g analogous to printf](https://github.com/rust-lang/rfcs/issues/844)
- [RFC #2729: General floating point formatting in Debug](https://github.com/rust-lang/rfcs/pull/2729)
- [Rust PR #86479: Automatic exponential formatting in Debug](https://github.com/rust-lang/rust/pull/86479)
- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [WEP: Format Traits](./wep-2026-02-01-format-traits.md)
- [Go fmt package](https://pkg.go.dev/fmt)
- [Python f-strings](https://peps.python.org/pep-0498/)
