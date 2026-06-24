# Gale — Grammar Adaptive LL Engine

Gale turns an ANTLR4 `.g4` grammar into a single, self-contained Wado parser.
You write a grammar, point a Wado `use` at it, and the compiler hands you a
parser — lexer, recursive-descent parser, and an error-resilient parse tree —
with no runtime to install and no version to keep in sync.

The `.g4` format is ANTLR4's; for the full grammar language, see ANTLR4's
[documentation](https://github.com/antlr/antlr4/tree/master/doc). Gale accepts
every grammar ANTLR4 accepts — and a few it rejects, where the meaning is
unambiguous (see [Design](#design)).

## Design

Behavioral ANTLR4 compatibility, with a small superset. The goal is not just to
_accept_ the grammars ANTLR4 accepts, but to _parse like ANTLR4 does_ — same
precedence, same ambiguity resolution, same parse trees. A `.g4` that the
upstream `antlr4` tool accepts should parse identically through Gale. Gale also
accepts a few grammars ANTLR4 _rejects_, but only where precedence climbing
fixes the meaning with no remaining choice — e.g. a `.`- or `~X`-led
left-recursive suffix like `e ~';' e`, which ANTLR4 errors on (no operator
token to climb). Where the meaning is not uniquely determined, Gale rejects
loudly rather than guessing.

Self-contained output, no version drift. Gale inlines its entire runtime into
every generated parser. There is no `gale-runtime` package to keep aligned with
the generator: each generated file carries the exact runtime it was built with,
and regenerating upgrades it.

Adaptive LL with a runtime ATN simulator. Most decisions are resolved with
fast static lookahead. When a decision genuinely needs unbounded,
full-context lookahead — the ALL(\*) cases that defeat any fixed-`k` LL parser
— Gale emits a runtime ATN simulator for exactly those decisions and leaves
the rest of the parser on the fast static path. (See the ALL(\*) example
below.)

Error-resilient parsing. A generated parser never bails on a syntax error. It
always returns a tree plus a list of diagnostics, recovering locally and
recording every edit, so a broken input still yields a usable tree (handy for
editors, linters, and language servers). A clean parse simply has an empty
diagnostic list.

Built-in diagnostics and debugging. `gale dump` shows the prediction decision
for every rule (and `--atn` shows the simulator's automaton); the `trace`
option makes a generated parser log its recursive descent. See
[The `gale` command](#the-gale-command).

## Tutorial: a four-function calculator

We will build an interpreter for arithmetic like `2 * (3 + 4)`. The finished
code is in [`example/`](./example): `Arith.g4`, `eval.wado` (the interpreter),
`main.wado` (a CLI), and `eval_test.wado` (tests).

### 1. The grammar (a `.g4` crash course)

[`example/Arith.g4`](./example/Arith.g4):

```antlr4
grammar Arith;

prog : expr EOF ;

expr : expr ('*' | '/') expr   # MulDiv
     | expr ('+' | '-') expr   # AddSub
     | '(' expr ')'            # Paren
     | INT                     # Num
     ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
```

Everything you need to read this:

- A `.g4` describes the lexer and the parser together. By convention,
  UPPERCASE rules are lexer (token) rules and lowercase rules are parser rules.
- `INT : [0-9]+ ;` is a token rule: `[...]` is a character class, and `+`/`*`/`?`
  are "one or more / zero or more / optional", as in a regex. `'...'` is a
  literal. `-> skip` throws a token away — here, whitespace.
- In a parser rule, `|` separates alternatives, `( )` groups, and `EOF` matches
  the end of input.
- `# Label` names an alternative. Gale uses these labels to give you a typed
  way to tell the alternatives apart (you'll see `ExprAlt` below).
- `expr` is left-recursive (`expr ... expr`). That is allowed and idiomatic:
  alternatives listed _earlier_ bind tighter, and operators grouped in one
  alternative (`('*' | '/')`) share a precedence level and associate left. So
  `*`/`/` bind tighter than `+`/`-`, and `6 / 2 * 3` is `(6 / 2) * 3`. Gale
  rewrites the left recursion into a precedence-climbing parser for you.

### 2. Generating the parser

Point a Wado `use` at the grammar and Gale generates the parser as part of the
compile — no separate build step. This is the opening of
[`example/eval.wado`](./example/eval.wado):

```wado
use arith from "./Arith.g4"
    with {
        generator: {
            module: "../src/generator.wado",   // the Gale generator
            options: { highlight: false },
        },
    };
```

`arith` is now an ordinary Wado module. It exports `parse`, the parse-tree
types, and — because the rules are labeled — an `ExprAlt` enum with an
`expr_alt` accessor. The generated source defaults to `build/kiln/<id>/`
(gitignored, regenerated when the grammar changes); add an `output_dir`
(resolved relative to this file) to commit it to a tracked path instead.

### 3. Walking the tree: the interpreter

`arith::parse(input)` returns a `ParseResult`. A clean parse has
`result.ok() == true`; `result.root` is the tree and `result.tokens` holds the
terminals it indexes.

The tree is one uniform `CstNode` type:

```wado
struct CstNode { kind: NodeKind, span: Span, children: List<CstChild>, ... }
variant CstChild { Token(i32), Missing(i32), Skipped(i32), Node(CstNode) }
```

Because `expr`'s alternatives are labeled, the parser stamps which one matched
onto each node, and exposes it as a per-rule enum plus an accessor:

```wado
pub enum ExprAlt { MulDiv, AddSub, Paren, Num }
pub fn expr_alt(node: &CstNode) -> Option<ExprAlt>
```

The accessor returns `Option`: a node that isn't a parsed `expr` (an
error-recovery node, say) is `None`, never a silent first alternative. So the
interpreter dispatches on the label — no need to inspect the tree shape.
Precedence and associativity are already baked into the tree, so evaluation is
a plain fold (from [`example/eval.wado`](./example/eval.wado)):

```wado
fn eval_expr(node: &arith::CstNode, toks: &arith::TokenStream) -> Result<i64, String> {
    return match arith::expr_alt(node) {
        Some(Num) => int_value(node, toks),
        Some(Paren) => eval_first_child(node, toks),
        Some(MulDiv) => fold_binary(node, toks),
        Some(AddSub) => fold_binary(node, toks),
        None => panic("expr: node has no stamped alternative"),
    };
}
```

`fold_binary` walks the node's children — sub-`expr` nodes (`CstChild::Node`)
and the operator token (`CstChild::Token`, read with `toks.token_text(i)`) —
and applies the operator. `int_value` reads a `Num` node's single `INT` token.
The public entry point surfaces a syntax error — or a runtime error like
division by zero — as `Err` instead of trapping:

```wado
pub fn eval(input: &String) -> Result<i64, String> {
    let result = arith::parse(input);
    if !result.ok() {
        let d = &result.diagnostics[0];
        return Result::Err(`{d.line}:{d.col}: {d.message}`);
    }
    return eval_first_child(&result.root, &result.tokens);
}
```

### 4. A CLI and tests

[`example/main.wado`](./example/main.wado) is a one-screen CLI:

```sh
wado run package-gale/example/main.wado '2 * (3 + 4)'   # -> 14
wado run package-gale/example/main.wado '6 / 2 * 3'     # -> 9
```

Tests import `eval` and check results; they also exercise the generator, since
the `use ... with` above regenerates the parser at compile time:

```sh
wado test package-gale/example/eval_test.wado
```

A handy debugging aid: `to_string_tree(&result)` renders the tree as an
ANTLR4-style S-expression, e.g. `1 + 2 * 3` →
`(prog (expr (expr (expr 1)) + (expr (expr (expr 2)) * (expr (expr 3)))))`.

## Beyond LL: an ALL(\*) example

Some grammars cannot be parsed by any fixed-lookahead LL parser; they need
full-context (ALL(\*)) decisions. Gale handles them with no extra effort from
you. [`example/Between.g4`](./example/Between.g4) is a small one:

```antlr4
expr : INT                          # Num
     | expr 'and' expr              # And
     | expr 'or' expr               # Or
     | 'between' expr 'and' expr    # Between
     ;
```

The keyword `and` is used two ways: as the binary operator in `A and B`, and as
the mandatory separator inside `between LO and HI`. At `between LO ⟨and⟩ …`, the
parser must decide whether that `and` closes the `between` or continues `LO` as
a binary `and` — and no fixed amount of lookahead settles it, because `LO` can
be arbitrarily large. Gale detects this and emits its runtime ATN simulator for
just this rule; the parser binds the `and` correctly with full context.

The interpreter ([`example/logic.wado`](./example/logic.wado)) is the same
shape as the calculator — dispatch on the generated `ExprAlt` enum
(`Num`/`And`/`Or`/`Between`) and fold. You never touch the simulator; it is an
implementation detail of the generated parser. Run the tests:

```sh
wado test package-gale/example/logic_test.wado
```

You can confirm a rule is ALL(\*)-class with `gale dump` (it prints
`Ambiguous(...)` for such a decision) and inspect the automaton with
`gale dump --atn`.

## Resilient parsing

A generated parser is **infallible**: it never throws and never bails on a
syntax error. It always returns a tree plus a list of diagnostics, repairing
the input locally and recording every repair _in the tree_ — so a broken file
still yields a usable, complete tree. This is what editors, linters, and
language servers need: they run on half-typed code far more often than on
valid code.

Feed the calculator a trailing operator and parse still succeeds, with the
stray `*` captured and one diagnostic:

```wado
let r = arith::parse(&"1 + 2 *");
// r.ok() == false
// arith::to_string_tree(&r) == "(prog (expr (expr 1) + (expr 2)) <skip *>)"
// r.diagnostics[0]: code ExtraToken at 1:6  ("extraneous input \"*\"")
```

Recovery is three first-class edits, each representable in the tree, so the
original tokens always round-trip:

| Edit                       | Tree                                | Diagnostic code   |
| -------------------------- | ----------------------------------- | ----------------- |
| delete a spurious terminal | `<skip x>` (`CstChild::Skipped`)    | `ExtraToken`      |
| insert a missing terminal  | `<missing X>` (`CstChild::Missing`) | `MissingToken`    |
| skip an unrecoverable run  | `<error>` region (`K_ERROR` node)   | `UnexpectedToken` |

A token that doesn't start any alternative produces a `NoViableAlternative`
diagnostic; the parser then folds the open nodes closed and carries on. The
`parse(input, max_errors)` overload caps how many diagnostics are collected
before recovery stops and folds the rest (`max_errors` defaults to unbounded
and must be `>= 1`); `<= 1` is effectively fail-fast while still returning a
partial tree.

## The generated parser API

Every generated parser module exports, at minimum:

| Item                                                       | What it is                                                     |
| ---------------------------------------------------------- | -------------------------------------------------------------- |
| `parse(input: &String) -> ParseResult`                     | parse from the start rule                                      |
| `parse_<rule>(input) -> ParseResult`                       | parse starting from any rule                                   |
| `tokenize(input: &String) -> TokenStream`                  | run only the lexer                                             |
| `to_string_tree(result: &ParseResult) -> String`           | ANTLR4-style S-expression of the tree                          |
| `ParseResult { root, tokens, diagnostics }`                | `.ok()` is true on a clean parse                               |
| `CstNode` / `CstChild`                                     | the uniform parse tree (see above)                             |
| `RK_<RULE>: NodeKind`                                      | node-kind constant for each rule (match on `node.kind`)        |
| `<Rule>Alt` enum + `<rule>_alt(node) -> Option<<Rule>Alt>` | the matched `# Label`, for labeled rules (`None` if unstamped) |

`Diagnostic` carries `line`, `col`, `message`, a `code`, and a severity
(`is_error()`); a resilient parse reports recovery edits as `Missing` /
`Skipped` children and `<error>` region nodes so the original input round-trips.

## The `gale` command

During development the CLI is `wado run package-gale <subcommand> …` (`gale` is
shorthand). The `use ... with { generator: ... }` above is the usual path; the
CLI is for ad-hoc generation and debugging.

```sh
# Generate a parser to stdout, or to a file with -o.
wado run package-gale gen Grammar.g4
wado run package-gale gen -o Grammar_parser.wado Grammar.g4

# Options:
#   --highlight   emit a syntax-highlighting table in the parser
#   --trace       emit a parser that logs its recursive descent to stderr
wado run package-gale gen --trace Grammar.g4

# Inspect the prediction decision for every rule (add --atn for the automaton).
wado run package-gale dump Grammar.g4
wado run package-gale dump --atn Grammar.g4
```

Multiple `.g4` files are merged (e.g. a split lexer/parser grammar). The same
`highlight` / `trace` options are available on the `use ... with` generator
config.

## Compatibility and further reading

Gale targets the full ANTLR4 `.g4` grammar syntax, plus the small superset
described under [Design](#design).

- [WEP: Gale](../docs/wep-2026-03-02-gale.md) — design and architecture.
- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility
  contract, the descriptor-based test suite, and how to run it.
- [WEP: Kiln](../docs/wep-2026-04-12-kiln.md) — the `use ... with { generator }`
  mechanism Gale plugs into.
- [WEP: Compile-Time File Inclusion](../docs/wep-2026-03-02-include-str.md) —
  `#include_str`, used to inline the runtime.
