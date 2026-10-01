# Wadopoet Development Guide

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

The tag tracks what the template's own text has opened: code, a string, a char
literal, a template, or a comment. A hole's value never changes that state. A
hole in code is written as it renders, since there it is code itself. A format
specifier applies as usual (`${bits:x}`). Printable non-ASCII stays verbatim.

Two helpers cover literals a hole cannot express:

- `byte_string_literal(bytes)` writes a `b"..."` literal.
- `char_range_pat(range)` writes a char range pattern, or one literal when the
  range holds one char.

## Testing

### What to Check

A test of generated source has two kinds of check:

- A string match, for the layout: indentation, blank lines, where a block
  closes, how a signature reads. The exact text is the contract here.
- An `eval` from `core:eval`, for what the text means: a literal reads back as
  its value, and generated declarations compile and run. Match the text alone
  and a wrong escape passes, since it can match an equally wrong expectation.

### Running a Program with `eval`

`eval` compiles a source string as a command and runs it. It works only under
`wado test`. A program for any other world that imports `core:eval` does not
compile.

```wado
use { eval } from "core:eval";

test "the generated parser accepts its input" {
    let generated = generate(grammar);  // the generator under test
    let program = `use { println, Stdout } from "core:cli";
${generated}
export fn run() with Stdout { println(parse("a b")); }`;
    let out = eval(program).unwrap();
    assert out.status matches { Exited(0) }, `${out.status}\n${out.stderr}`;
    assert out.stdout == "ok\n";
}
```

The source is the whole program, in one module:

- It exports `run()`, as any `wasi:cli/command` does.
- Its `use` items may name `core:*` only. A relative path does not compile.
- It gets stdout, stderr and exit, and nothing else: no arguments, stdin,
  environment, files, clock or randomness.

`eval` returns `Result<Output, EvalError>`. `Err` means the program never ran:

- `CompileFailed(failure)`: `failure.rendered` is the report `wado compile`
  prints, and `failure.codes` the code of each error.
- `CompileTimedOut`: the compile ran past its time limit.
- `Unavailable(name)`: the program imports an interface `eval` does not link.

`Ok(out)` means it ran, however it ended. `out.stdout` and `out.stderr` hold what
it wrote, even when it trapped. A `panic` writes its message to stderr before it
traps. `out.status` says how it ended:

- `Exited(code)`: `run` returned (code 0) or called `exit(code)`.
- `Trapped(kind)`: it trapped, and `kind` says which trap.
- `OutOfFuel`: it used up its fuel.
- `OutOfMemory`: a memory or a table grew past the runner's 1 GiB ceiling.

To test that a generator rejects input, check the codes as an e2e fixture does:

```wado
let result = eval(program);
let Err(CompileFailed(failure)) = result else {
    panic(`expected a compile failure, got ${result:?}`);
};
assert failure.codes == ["TYPE_MISMATCH"], failure.rendered;
```

The second argument is the fuel, 10⁹ by default. Pass a small one to show that
a loop does not end: `eval(program, 10_000)` returns `OutOfFuel`.

The program compiles at the test's `-O` and with its `-f` flags. Time inside
`eval` does not count against the test's timeout. Outcomes are cached in
`build/eval/` under the package root, keyed by the compiler, the flags, the fuel
and the source, so a later run pays nothing for an unchanged program.
`--no-cache` skips reading the cache. See
[`core:eval`](../docs/stdlib-core-eval.md).

### Round Trips in This Package

`src/wadopoet_test.wado` has a `printed` helper. It wraps a body in a `run()`
that may call `print`, evaluates it, asserts it exited with 0, and returns
what it printed. A round trip reads like this:

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

When a body holds a template, write the body as a `"..."` literal. Written as a
template of the test's own, its `${...}` would be filled in by the test.
