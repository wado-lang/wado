# Wadopoet

Wadopoet builds Wado source text for a code generator. Grog and Loam emit through
it. A package names it under `[dependencies]` and imports it from
`lib:wadopoet`.

## Building Source

`CodeWriter` holds the text and its indentation. `begin` opens a block, `end`
closes it, and `line` writes one line at the current depth. The specs
(`StructSpec`, `VariantSpec`, `EnumSpec`, `FnSpec`, `GlobalSpec`, `ImplSpec`)
build a declaration and emit it into a writer. Most of a function body is
plain lines.

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

`append` splices in what another writer holds, indented to the current depth.
Build a body in its own writer and append it, rather than printing it with
`to_string` and writing the text: text loses the calls and names below.

## Pruning Functions

`prune` drops the functions nothing uses and forwards calls through the ones
that only forward. It reads what the writer holds apart from the text, so the
generator states each use:

- `call(CallStmt)` writes a call statement. `pre` holds statements that run just
  before it, and `form` says whether the result is discarded, returned, or bound
  (`CallForm::Bind("let x = ")`).
- `stmt(text)` writes a statement line `prune` may move into a caller: one that
  neither leaves the function nor binds a name.
- `ret()` writes a bare `return;`.
- `note(name)` records that the text around it references `name`.

`FnSpec::set_prunable` marks a function `prune` may drop. Every use of one must
be a `call` or a `note`, since `prune` does not read the text. Everything else at
the top level is kept, and so is everything it reaches.

A prunable function forwards when its body is one call it returns, one call then
`return;`, or one `stmt` line then `return;`. A call passing it its own
parameters by name gets that body instead. A call passing other arguments is
renamed to the inner callee, when the body passes every parameter on in order
and runs nothing first.

`references()` lists every name the writer still notes or calls. A generator
that builds tables only for what kept code names asks it after `prune`.

## Reserved Names

A name the generator mints from its input must not be reserved. Check it with
`is_reserved`. An enum case or another qualified name need only avoid
`is_keyword`.

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

The tag tracks what the template's own text has opened: code, a string, a char
literal, a template, or a comment. A hole's value never changes that state. A
hole in code is written as it renders, since there it is code itself. A format
specifier applies as usual (`${bits:x}`). Printable non-ASCII stays verbatim.

Two helpers cover literals a hole cannot express:

- `byte_string_literal(bytes)` writes a `b"..."` literal.
- `char_range_pat(range)` writes a char range pattern, or one literal when the
  range holds one char.

No helper writes a non-finite float. A hole holding one renders as `Display`
does, which is not Wado source.
