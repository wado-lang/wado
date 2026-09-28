# Lexical Structure

## Whitespace

Whitespace separates tokens and is otherwise ignored. Any character with the
Unicode `White_Space` property is whitespace: space, tab, LF and CR, and also
characters such as the no-break space (U+00A0) and the ideographic space
(U+3000).

## Comments

<!-- {"fixture":"spec_lexical_comments.wado"} -->

```wado
//! Module doc comment

// Line comment (extends to end of line)

/* Block comment */

/*
 * Multi-line
 * block comment
 */

/// Doc comment
fn one() -> i32 {
    return /* a comment is whitespace */ 1;
}

test {
    assert one() == 1;
}
```

Block comments do not nest.

A doc comment is a line comment that documents code. Neither kind changes what
a program means.

- `///` documents the declaration that follows it: an item, or a field, case or
  method inside one. Consecutive `///` lines form one doc string. Attributes may
  stand between the doc comment and the declaration, but a blank line may not:
  it detaches the comment.
- `//!` documents the module. The `//!` lines ahead of the first item form the
  module's doc string.
- `////` and longer runs of `/` are ordinary line comments.

A doc string is Markdown. Each line loses its `///` or `//!` marker and one
space after it, so a line holding only `///` is an empty line.

<!-- {"fixture":"spec_lexical_doc_comments.wado"} -->

```wado
//! Geometry helpers.

/// A point on the plane.
///
/// Both coordinates are in pixels.
#[wire(name_policy = "camelCase")]
pub struct Point {
    /// Distance from the left edge.
    x: i32,
    y: i32,
}

test {
    assert Point { x: 1, y: 2 }.x == 1;   // a doc comment changes nothing
}
```

Rationale: [WEP: Documentation Generation](./wep-2026-02-28-doc-command.md).

## Shebang

<!-- {"fixture":"spec_lexical_shebang.wado"} -->

```wado
#!/usr/bin/env -S wado run
export fn run() {
    assert #line == 3;   // the shebang is line 1
}
```

`#!` at position 0 is a shebang and is ignored. `#![` is an inner attribute, not a shebang.

## Data Section

The `__DATA__` marker separates source code from embedded data. Everything after `__DATA__` on its own line is captured as raw text and is not parsed as Wado code. `#data` reads it.

<!-- {"fixture": "sub/location_submodule_helper.wado"} -->

```wado
pub fn show_data() with Stdout {
    println(#data);
}

__DATA__
SUBMODULE_DATA_MARKER
```

### Syntax Rules

- `__DATA__` must appear at the start of a line (after any preceding newline)
- The line must contain only `__DATA__` followed by a newline (no trailing content on the same line)
- Everything after the `__DATA__` line becomes the data section
- The data section is optional; most modules won't have one

### Accessing Data

Within Wado code, the content is available through the `#data` compile-time location literal. See [Compile-Time Location Literals](./spec-literals.md#compile-time-location-literals).

## Identifiers

An identifier starts with an ASCII letter or `_`. Each later character is `_`
or any Unicode letter or number (a character with the `Alphabetic` property or a
numeric general category):

<!-- {"fixture":"spec_lexical_identifiers.wado"} -->

```wado
let foo = 1;
let foo_bar = foo + 1;
let fooBar = foo_bar + 1;
let FooBar = fooBar + 1;
let FOO_BAR = FooBar + 1;
let _private = FOO_BAR + 1;
let name123 = _private + 1;
let café = name123 + 1;    // OK: `é` is not the first character
assert café == 8;
```

A non-ASCII first character is an error:

<!-- {"fixture":"spec_lexical_identifier_non_ascii_start.wado"} -->

```wado
let é = 1;    // Error: the first character must be ASCII
```

Identifiers are case-sensitive.

## Contextual Keywords

The following keywords are contextual. Each acts as a keyword only in the
position listed:

| Keyword   | Keyword context                                     |
| --------- | --------------------------------------------------- |
| `flags`   | `flags` declaration                                 |
| `type`    | `type` declaration                                  |
| `of`      | `for let <pattern> of <expr>`                       |
| `from`    | `use { ... } from "..."`                            |
| `test`    | `test "name" { ... }` block                         |
| `extends` | `resource Child extends Parent`                     |
| `do`      | `with Effect => handler do { ... }`                 |
| `task`    | `task return expr;`                                 |
| `trap`    | `..trap` rest clause of an effect handler `impl`    |
| `forward` | `..forward` rest clause of an effect handler `impl` |
| `resume`  | `resume expr` in an effect handler                  |

Elsewhere each is an ordinary identifier: a variable, field, parameter, or type
name. `resume` is the exception. It is a keyword in every expression position,
so it serves only as a field name.

<!-- {"fixture":"spec_lexical_contextual_keywords.wado"} -->

```wado
// 'of' as a variable name
let of = 42;
assert of == 42;

// 'of' as a struct field
struct Item { of: i32 }
let item = Item { of: 10 };
assert item.of == 10;

// 'of' as a for-of binding
let arr: List<i32> = [1, 2, 3];
let mut sum = 0;
for let of of arr {
    sum += of;
}
assert sum == 6;
```

A variable, parameter, item, case or import may not be named `resume`. Only a
field or a method, reached through `.`, may take the name. The reason is that
`resume` begins an expression (`resume value`), so such a name could never be
read.
