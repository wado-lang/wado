# Wadopoet Development Guide

Wadopoet builds Wado source text for a code generator. Grog and Loam emit through
it. A package names it under `[dependencies]` and imports it from
`lib:wadopoet`.

## Building Source

`CodeWriter` holds the text and its indentation (`begin`, `end`, `line`). The
specs (`StructSpec`, `VariantSpec`, `EnumSpec`, `FnSpec`, `GlobalSpec`,
`ImplSpec`) emit a declaration into a writer; a body is mostly plain lines.

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

Build a body in its own writer and `append` it; its text alone loses the calls
and names `prune` reads.

## Pruning Functions

`prune` drops the functions nothing uses and forwards calls through the ones
that only forward. It does not read the text, so the generator states each use:
`call(CallStmt)` for a call statement, `stmt(text)` for a line it may move into
a caller, `ret()` for a bare `return;`, and `note(name)` for a reference in
plain text. Only a function marked `FnSpec::set_prunable` may be dropped, and
every use of one must be a `call` or a `note`. `references()` lists what kept
code still names.

A name minted from input must not be `is_reserved`; a qualified one (an enum
case) need only avoid `is_keyword`. `src/vocabulary.wado` is generated: run
`mise run update-wadopoet-vocabulary`.

## Escaping with the `wado` Tag

`wado` is a tagged template that escapes each hole for the literal it sits in,
tracking what the template's own text has opened (code, string, char, template,
comment). A hole in code is written as it renders.

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

`byte_string_literal(bytes)` and `char_range_pat(range)` write what a hole
cannot. No helper writes a non-finite float: a hole holding one renders as
`Display` does, which is not Wado source.

## Testing

Check the layout by string match, and the meaning by running the text with
[`core:eval`](../docs/stdlib-core-eval.md): a string match passes a wrong escape
against an equally wrong expectation. `src/wadopoet_test.wado`'s `printed`
helper runs a body under `run()` and returns what it printed. Write a body that
holds a template as a `"..."` literal, or the test fills in its holes.
