# Wadopoet Development Guide

Wadopoet builds Wado source text for a code generator. Grog and Loam emit through
it. A package names it under `[dependencies]` and imports it from
`lib:wadopoet`.

## Building Source

`CodeWriter` holds the text and its indentation. `begin` opens a block, `end`
closes it, and `line` writes one line at the current depth. The specs
(`StructSpec`, `VariantSpec`, `EnumSpec`, `FnSpec`, `GlobalSpec`, `ImplSpec`)
build a declaration and emit it into a writer. A function body stays plain
lines.

```wado
use { CodeWriter, FnSpec, StructSpec, Vis } from "lib:wadopoet";

let mut w = CodeWriter::new();
let mut s = StructSpec::new("Point", Vis::Pub);
s.add_pub_field("x", "i32").add_pub_field("y", "i32");
s.emit(&mut w);
w.blank();

let mut f = FnSpec::new("sum");
f.add_param("p", "&Point");
f.set_return_type("i32");
f.emit_begin(&mut w);
w.line("return p.x + p.y;");
w.end();

let source = w.to_string();
```

A name the generator mints from its input must not be reserved. Check it with
`is_reserved`. An enum case or another qualified name need only avoid
`is_keyword`. `src/vocabulary.wado` is generated from the compiler, so
regenerate it with `mise run update-wadopoet-vocabulary`, never by hand.

## Escaping with the `wado` Tag

`wado` is a tagged template. Each hole is escaped for the literal it sits in,
so a value cannot break out of its string.

```wado
let text = "say \"hi\"\n";
let quote = '\'';
let hole = "${x}";
let name = "expr_list";

wado`let s = "${text}";`      // let s = "say \"hi\"\n";
wado`let c = '${quote}';`     // let c = '\'';
wado`print(\`${hole}\`);`     // print(`\${x}`);
wado`pos = ${name}(tokens);`  // pos = expr_list(tokens);
```

A hole in code is written as it renders, since there it is code itself.

The tag tracks what the template's own text has opened: code, a string, a char
literal, a template, or a comment. A hole's value never changes that state. A
format specifier applies as usual (`${bits:x}`). Printable non-ASCII stays
verbatim.

Two helpers cover literals a hole cannot express:

- `byte_string_literal(bytes)` writes a `b"..."` literal.
- `char_range_pat(range)` writes a char range pattern, or one literal when the
  range holds one char.

## Testing

A test of what Wadopoet emits has two kinds of check:

- A string match, for the layout: indentation, blank lines, where a block
  closes, how a signature reads. The exact text is the contract here.
- An `eval` from `core:eval`, for what the text means: a literal reads back as
  its value, and composed declarations compile and run. Match the text alone
  and a wrong escape passes, since it can match an equally wrong expectation.

`src/wadopoet_test.wado` has a `printed` helper that compiles a program with
`eval` and returns what it printed. A round trip reads like this:

```wado
test "a hole in a string literal reads back as its value" {
    let values: List<String> = ["", "a\"\\b", "\n\u{1}", "あ"];
    let mut run = String::new();
    let mut expected = String::new();
    for let v of values {
        run.push_str(wado`print("${v}");`);
        expected.push_str(&v);
    }
    assert printed(run) == expected;
}
```

Write the body of a generated program as a `"..."` literal when it holds a
template. In a template of your own, `${...}` would interpolate in the test.

An evaluated program compiles on the first run, and later runs read the
outcome from the cache. See [`core:eval`](../docs/stdlib-core-eval.md).
