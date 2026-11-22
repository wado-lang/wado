# Wadopoet

Wadopoet builds Wado source text for a code generator. Grog and Loam emit through
it. A package names it under `[dependencies]` and imports it from
`lib:wadopoet`.

## Building Source

`CodeWriter` holds the text and its indentation. `begin` opens a block, `end`
closes it, and `line` writes one line at the current depth. Most of a function
body is plain lines.

A spec builds one declaration and emits it into a writer: `StructSpec`,
`VariantSpec`, `EnumSpec`, `FlagsSpec`, `NewtypeSpec`, `GlobalSpec`, `FnSpec`
and `ImplSpec`. Every spec is built the same way:

- `new(name, vis)` creates it. `vis` defaults to file-private.
- `set_doc` sets the doc comment and `add_attr` adds an attribute.
- `add_generic` adds a type parameter, where the declaration takes one.
- `add_field`, `add_case`, `add_member` and `add_param` add a member by its
  name and type. Each has a `_spec` form taking a `FieldSpec`, `CaseSpec`,
  `MemberSpec` or `ParamSpec`, for a member with a default, a payload, a doc
  comment or attributes.

Every setter returns the spec, so calls chain.

`emit(w)` writes the declaration. `FnSpec` and `ImplSpec` take the body as a
closure too, `emit(w, |w| …)`, which writes it into the writer it is handed and
returns nothing. Compute what may fail before the declaration, not inside the
closure.

```wado
use { CodeWriter, FieldSpec, FnSpec, StructSpec, Vis } from "lib:wadopoet";

let mut w = CodeWriter::new();
let mut s = StructSpec::new("Point", Vis::Pub);
s.set_doc("A point in the plane.");
s.add_field("x", "i32", Vis::Pub).add_field("y", "i32", Vis::Pub);
s.add_field_spec(FieldSpec { name: "label", type_str: "String", default: Option::Some("\"\"") });
s.emit(&mut w);
w.blank();

FnSpec::new("sum").add_param("p", "&Point").set_return_type("i32").emit(&mut w, |w| {
    w.line("return p.x + p.y;");
});

let source = w.to_string();
```

Write a declaration through its spec, not as a line of text. `prune` reads a
function only through `FnSpec`, and a doc comment `FnSpec` writes goes where the
function goes.

`append` splices in what another writer holds, indented to the current depth.
Build a body in its own writer and append it, rather than printing it with
`to_string` and writing the text: text loses the calls and names below.

## Opening a Module

`ModuleSpec` writes the `#![generated]` header a generated module opens with.
Kiln refuses a generator's file without one. A Kiln generator builds the spec
from its request's origin, taken before an input is read, since reading moves
the input's content out of the request. A script that writes a module of its
own names itself with `ModuleSpec::new`.

```wado
let module = ModuleSpec::from_origin(&req.origin());
let mut w = CodeWriter::new();
module.emit(&mut w);
```

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

## Minting Names

A name the generator mints from its input must not be reserved. Check it with
`is_reserved`. An enum case or another qualified name need only avoid
`is_keyword`.

`NameRegistry` holds the names one scope declares, past the reserved ones and
those `bind` adds, such as the module's imports. `claim(name, owner)` takes a
`_` after a bound name and refuses a held one with its owner, so whether two
names meet does not depend on which came first. `fresh(wanted)` takes the first
of `wanted`, `wanted_`, … that is free.

```wado
let mut names = NameRegistry::new();
names.bind("grog");
assert names.claim("List", "pkg.List") == Ok("List_");
assert names.claim("List", "other.List") == Err("pkg.List");
assert names.fresh("grog") == "grog_";
```

`IdGen` hands out 0, 1, 2, …, for the locals a generator suffixes so two never
meet.

`hoist_fn(key, prefix, |w, name| …)` defines a top-level function once per
distinct `key`. The closure writes it under the name it is handed, and
`write_hoisted` places what every closure wrote.

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
