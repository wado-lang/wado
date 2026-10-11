# Lexical Structure

This chapter covers how source text splits into tokens: whitespace, comments,
the shebang and the data section, identifiers, and keywords. Literal tokens are
in [Literals](./spec-literals.md), and operators in
[Operators](./spec-expressions.md#operators).

## Whitespace

Whitespace separates tokens and is otherwise ignored. It is ASCII only: space,
tab, LF, form feed (U+000C) and CR. Any other character outside a string,
character literal or comment is an error, the no-break space (U+00A0) and the
ideographic space (U+3000) among them.

## Line Breaks

A line ends at LF or at CRLF, as in Rust, so a file means the same with either
line ending. Inside a literal, a comment or the data section, a CRLF reads as
one LF: a string spanning two lines holds `"\n"` between them, and `#data`
holds no CR. `#line` counts a CRLF as one line break.

A CR that no LF follows is not a line break. Between tokens it is whitespace.
Inside a string, template, byte string or character literal it is an error,
since most editors do not show it; write `\r` instead. In a comment it is part
of the text, and it does not end the line of a `__DATA__` marker.

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

A line holding only `__DATA__` separates the source code from the data section.
Everything after that line is raw text, not Wado code, and
[`#data`](./spec-literals.md#data) reads it. A module needs no data section.

<!-- {"fixture": "sub/location_submodule_helper.wado"} -->

```wado
pub fn show_data() with Stdout {
    println(#data);
}

__DATA__
SUBMODULE_DATA_MARKER
```

## Identifiers

An identifier is ASCII: `[a-zA-Z_][a-zA-Z0-9_]*`, except `_` alone.

<!-- {"fixture":"spec_lexical_identifiers.wado"} -->

```wado
let foo = 1;
let foo_bar = foo + 1;
let fooBar = foo_bar + 1;
let FooBar = fooBar + 1;
let FOO_BAR = FooBar + 1;
let _private = FOO_BAR + 1;
let name123 = _private + 1;
assert name123 == 7;
```

A non-ASCII character is an error, wherever it stands:

<!-- {"fixture":"spec_lexical_identifier_non_ascii_start.wado"} -->

```wado
let é = 1;    // Error: the first character must be ASCII
```

<!-- {"fixture":"spec_lexical_identifier_non_ascii_continue.wado"} -->

```wado
let café = 1;    // Error: `é` is not an ASCII letter, digit or `_`
```

Identifiers are case-sensitive.

## The Wildcard `_`

`_` alone is the wildcard, not an identifier. It leaves a value unbound: in a
pattern, as a parameter, and as a closure parameter. In a type it asks for
inference, `use _ from "…"` is a wildcard import, and `with _` is an effect
parameter. Since it binds nothing, there is nothing to read back:

<!-- {"fixture":"spec_lexical_underscore.wado"} -->

```wado
fn first(x: i32, _: i32) -> i32 {
    return x;
}

test {
    let _ = first(1, 2);
    let [a, _] = [3, 4];
    let pick = |_: i32| a;
    assert pick(5) == 3;
    assert match a { _ => true };
}
```

It cannot name anything: an item, a field, a variable to read, or a type:

<!-- {"fixture":"spec_lexical_underscore_not_a_name.wado"} -->

```wado
fn _() {}    // Error: `_` is not a name
```

A name that starts with `_` and goes on, such as `_private`, is an ordinary
identifier.

## Keywords

A keyword is not an identifier, so it cannot name a variable, parameter, item
or import:

```text
as        assert    async     break     const     continue  effect
else      enum      export    false     fn        for       global
if        impl      import    in        interface internal  let
loop      match     matches   mut       null      pub       reactive
resource  return    struct    trait     true      use       variant
while     with      world
```

`reactive` is reserved for reactive signals, which are not in the language yet,
so a program that writes it is rejected.

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
| `resume`  | `resume expr;` in an effect handler                 |
| `self`    | a method's receiver: `&self`, `self.field`          |
| `Self`    | the implementing or declared type: `Self::Item`     |

Elsewhere each is an ordinary identifier: a variable, field, parameter, or type
name.

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

`resume` is the exception. It begins a statement (`resume value;`) wherever a
statement can start, and is an error wherever an expression can, so a name
spelled `resume` could never be read. A
variable, parameter, item, case or import may not take it. Only a field or a
method, reached through `.`, may.
