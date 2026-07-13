# WEP: Local Item Definitions

## Context

`struct`/`enum`/`variant`/`flags`/`type`/`impl`/`trait` were module-level-only
declarations: a type needed only inside one function still had to live at
module scope, polluting the module's namespace and separating the type from
its one use site. Rust, Go, and TypeScript all allow a type declaration inside
a function body; Wado had no equivalent.

## Decision

Allow `struct`/`enum`/`variant`/`flags`/`type` (newtype)/`impl`/`trait` as a
statement inside a function or method body (`Stmt::Item`). `fn`/`use`/
`interface`/`global`/`world`/`test`/`resource` remain module-level only —
nesting a function inside a function is a separate, unrelated feature this
WEP does not attempt, and an effect interface has no meaningful function-local
scope to speak of. Each of these produces the same parse error a missing `}`
would ("expected `}`"), not a special-cased message — they were never valid
here.

Scoping rules, chosen for simplicity over expressiveness:

- **Function-scoped, not block-scoped.** A local item declared anywhere in a
  function body — including inside a nested `if`/`while`/`for` block — is
  visible for the rest of that function, not just the enclosing block. Two
  unrelated functions may declare same-named local items without collision;
  a closure literal inside the function does not currently see its enclosing
  function's local items (an unhandled edge case, not a deliberate
  restriction).
- **Sequential resolution, no hoisting.** A local item must be declared
  before its first use, exactly like `let` — no forward reference, including
  between two local items (`struct A` referring to a `struct B` declared
  later in the same function, or vice versa, is unsupported).
- **Always private.** A `pub`/`internal`/`export` prefix on a local item is a
  dedicated parse error (`at_visibility_prefixed_local_item_start`) rather
  than the generic "expected `}`" recovery every other invalid block-level
  keyword gets — the shape is common enough (a reasonable guess coming from
  module-level syntax) to warrant a clear, on-topic message instead of
  leaving it to recovery.

Implementation, staged as three units of work:

1. **Parsing** (`Stmt::Item(Box<Item>)`): a new lookahead,
   `at_local_item_start()`, disambiguates `flags`/`type` as item-start
   keywords from their use as plain identifiers (`let flags = 10;` still
   parses as a variable). `bind_item`/`unparse_item`/`visit_item` are reused
   unchanged for scope checking, formatting, and semantic tokens.
2. **Resolution**: each local item is minted under an internal mangled name
   (`{name}@{AstId:?}`) — the _storage_ identity — while the _declared_ name
   is registered in a new per-function ephemeral table
   (`ModuleDecls::fn_local_*`, cleared at the top of every function/method/
   test body) that `TypeLookup` consults with the highest precedence. This
   two-tier split is what makes the two scoping rules above hold at once:
   the ephemeral, bare-name tier gives correct sequential _visibility_
   during that one function's resolution, while the durable, mangled-name
   tier (an existing table, previously used only for anonymous struct
   literals) is what a _later, independent_ reify pass recovers a
   declaration's field/case info from, without needing to replay the
   per-function scoping. Mangling is what lets two sibling functions declare
   colliding names into the same shared, module-scoped durable table safely.
3. **TIR emission**: a local item has no `Item::Struct`/`Item::Newtype` entry
   in `module.items` for reify to walk (it lives inside a function body), so
   — mirroring the pre-existing anonymous-struct-literal mechanism — the
   full `TirStruct`/`TirNewtype` is built once during annotate and flushed
   into the module's TIR the same way anonymous structs already are.

A pre-existing early type-name validation pass (a fast pre-check that runs
before the real elaborator, built from a flat module-wide name set) had no
notion of function-scoped names and needed widening: it now also recognizes
any name reachable via a local item declaration anywhere in the enclosing
function, so it does not false-positive ahead of the real (correctly
sequential) resolution. This is a coarse over-approximation on purpose — it
does not itself enforce declare-before-use ordering; the real elaborator
still does.

Generic local structs (`struct Box<T> { value: T }`) reuse the module-level
generic-struct machinery unchanged: the monomorphizer already treats any
`TirStruct` with non-empty type params as a template to expand, regardless of
whether reify or this WEP's eager annotate-time emission produced it.

## Consequences

- Supported today: local `struct` (generic or not) and local non-generic
  `type` (newtype) — declare, construct, field/case access, comparison,
  `assert`, and auto-derived traits (`Display` etc.: structural for a
  struct, forwarded from the base type for a newtype).
- Not yet supported: parses, but a reference fails downstream with whatever
  error fits how it was referenced, not a dedicated message. Local
  `enum`/`variant`/`flags` surfaces as `unknown identifier` for a case path
  (e.g. `Color::Red`) or `unknown function` for variant construction. Local
  generic `type` surfaces as a type mismatch, since it needs a different
  mechanism — `GenericNewtypeInfo` + AST substitution, not the monomorphized
  template generic structs use — that this WEP does not wire up. Methods on
  any local type surface as `no method 'x' found on type`, since a local
  `impl`/`trait` block parses but is not connected to method dispatch, a
  module-wide registry built once before any function body is walked.
