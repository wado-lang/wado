# WEP: String Template Desugaring

## Context

A string template (`` `Hello, ${name}!` ``) is a `String` expression. This WEP
fixes what the compiler lowers it to: one buffer, one `Formatter`, no
intermediate strings. A template with a tag denotes a call on a synthesized
template type instead, and is
[Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md)'s
business; what the two share is the per-specifier rendering below, which the
tagged form reaches through `Hole::fmt`.

## Decision

### Untagged Template Desugaring

```wado
`Hello, ${name}! You are ${age}.`
```

The compiler directly emits an efficient sequence using a mutable string and labeled block expression. Every interpolation goes through a `Formatter` (see [Format Traits](./wep-2026-02-01-format-traits.md)), specifier or not. `Formatter` wraps `&mut String` and writes into the output buffer with no intermediate allocation.

```wado
$tmpl: {
    let mut __r = "Hello, ";
    name.fmt(&mut Formatter::new(&mut __r));
    __r.push_str("! You are ");
    age.fmt(&mut Formatter::new(&mut __r));
    __r.push_str(".");
    __r
}
```

One `Formatter` local serves the whole block; the snippets spell out a fresh
one per interpolation for readability.

This lowering is what the prelude's `format` tag computes, written out by the
compiler rather than reached through `ReflectTemplate`; the fixtures hold the
two to the same output.

### Format Specifiers

A specifier selects the trait method to call (`fmt`, `fmt_lower_hex`, …) and
the `Formatter` to call it with. The compiler emits `Formatter::new` when the
specifier sets no field beyond the type — width, precision, fill, alignment,
`+`, `#` and `0` are what make it a field-by-field literal instead:

```wado
`Pi is ${pi:.2}`
```

Desugars to:

```wado
$tmpl: {
    let mut __r = "Pi is ";
    pi.fmt(&mut Formatter {
        fill: ' ', align: Alignment::Right, sign_plus: false, alternate: false,
        zero_pad: false, width: -1, precision: 2, indent: 0, buf: &mut __r,
    });
    __r
}
```

The literal writes every field, sentinels included, rather than deriving from
`Formatter::new`. See [Format Traits](./wep-2026-02-01-format-traits.md) for
the field list.

In a tagged template the same selection is closed inside the synthesized
`Hole::fmt`: the method the type selects, on the `Formatter` the rest of the
specifier describes, over the buffer the tag passes. The tag keeps the typed
value and decides whether to render it.

### Braces and `$` Escaping

Only `${` opens an interpolation, so `{` and `}` are literal text and JSON-like
content needs no escaping:

```wado
`JSON: {"key": ${value}}`
// segments: "JSON: {\"key\": " and "}"
```

A literal `$` is also plain text; escape it with `\$` only when it directly
precedes a `{` that should stay literal (`` `\${x}` `` renders `${x}`).

### Edge Cases

| Case                    | Input                    | Output                         |
| ----------------------- | ------------------------ | ------------------------------ |
| Empty template          | `` ` ` ``                | `""`                           |
| No interpolation        | `` `hello` ``            | `"hello"`                      |
| Only interpolation      | `` `${x}` ``             | `Display::fmt` of x            |
| Adjacent interpolations | `` `${a}${b}` ``         | segments `""`, `""`, `""`      |
| Literal braces          | `` `{x}` ``              | `"{x}"` (no interpolation)     |
| Literal `${`            | `` `\${x}` ``            | `"${x}"` (literal)             |
| Nested template         | `` `outer ${`inner`}` `` | Inner template evaluated first |
| Multiline               | Preserved                | Newlines in segments           |

## Consequences

- One buffer per template, no intermediate `String` per interpolation.
- The specifier is resolved at the site, for the untagged form in the emitted
  block and for the tagged form inside `Hole::fmt`, so no runtime dispatch on a
  specifier exists anywhere.

## Related WEPs

- [Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md): the tagged form
- [Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md): the specifier grammar
- [Format Traits](./wep-2026-02-01-format-traits.md): `Formatter` and the per-type traits

## References

- [MDN: Template literals](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Template_literals)
