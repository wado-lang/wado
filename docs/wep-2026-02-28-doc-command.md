# WEP: Documentation Generation (`wado doc`)

## Context

Wado source files already use `///` doc comments extensively — the WASI stdlib alone has 500+ doc comment lines generated from WIT files, and core library files document builtins, traits, and type methods. However, these comments are currently treated as regular line comments by the lexer and have no semantic meaning to the compiler.

Languages with first-class doc tooling (Rust's `rustdoc`, Go's `go doc`, Elixir's `ex_doc`) enable a documentation culture from day one. Adding `wado doc` early ensures the stdlib ships with proper docs and sets conventions before the ecosystem grows.

### Current State

- The lexer collects all comments into a `CommentMap` (Go-style, indexed by byte position)
- `CommentKind` has `Line` and `Block` variants — no distinction for `///` or `//!`
- AST items have `attrs: Vec<Attribute>` and `span: Span` but no `doc` field
- The `CommentMap` already provides `leading_comments(span)` to find comments before an AST node

## Decision

### Doc Comment Syntax

```wado
//! Module-level documentation.
//! Appears at the top of a file, before any items.

/// Documents the following item (function, struct, trait, enum, variant, etc.)
/// Supports **markdown** formatting.
///
/// # Examples
///
/// ```wado
/// let p = Point { x: 1, y: 2 };
/// assert p.x == 1;
/// ```
pub struct Point {
    /// The x coordinate.
    x: i32,
    /// The y coordinate.
    y: i32,
}
```

**`///` (item doc):** Attaches to the immediately following item. Consecutive `///` lines are joined into a single doc string.

**`//!` (module doc):** Attaches to the enclosing module (file). Must appear before any items.

Doc comment text is extracted with the `/// ` prefix stripped (including the single space after `///`). A line containing only `///` becomes an empty line in the output.

### CLI Interface

```sh
wado doc [options] [file.wado...]

# Single file
wado doc lib/core/prelude/traits.wado     # print docs for one file

# Multiple files / glob
wado doc lib/core/**/*.wado               # print docs for matching files

# Project mode (with wado.toml)
wado doc                                  # document all project source files

# Output format
wado doc --format markdown file.wado      # markdown output (default)
wado doc --format json file.wado          # structured JSON output

# Filtering
wado doc --filter "TreeMap" file.wado     # show only items matching pattern
wado doc --pub-only file.wado             # show only pub/export items
```

| Flag                  | Description                                        |
| --------------------- | -------------------------------------------------- |
| `--format <fmt>`      | Output format: `markdown` (default), `json`        |
| `--filter <pattern>`  | Show only items whose name contains the pattern    |
| `--pub-only`          | Show only `pub` and `export` items                 |
| `--no-private`        | Alias for `--pub-only`                             |
| `-o <file>`           | Write output to file instead of stdout             |
| `--help`              | Show usage                                         |

### Extraction Strategy: CommentMap-Based (No AST Changes)

Doc comments are extracted **post-parse** using the existing `CommentMap` infrastructure. No changes to AST node structures are required.

The extraction algorithm:

1. Parse the source file (get AST + `CommentMap`)
2. For each top-level item, call `comment_map.leading_comments(&item.span)`
3. Filter results to consecutive `///` lines immediately before the item
4. Strip the `/// ` prefix and join into a markdown string
5. For `//!` lines, scan comments at the file start (before the first item's span)

This approach:
- Requires **zero changes** to the parser, AST, or any compilation phase
- Reuses the battle-tested `CommentMap` that the formatter already depends on
- Works with existing `///` comments in the WASI stdlib without any source changes

### CommentKind Extension

Extend `CommentKind` to distinguish doc comments from regular comments:

```rust
pub enum CommentKind {
    Line,         // `// ...`
    Block,        // `/* ... */`
    DocLine,      // `/// ...`
    ModuleDoc,    // `//! ...`
}
```

The lexer detects `///` and `//!` patterns after the initial `//` and assigns the appropriate kind. This enables the formatter to preserve doc comment style and the doc tool to efficiently filter for doc comments.

### Markdown Output Format

```markdown
# module_name

Module-level doc from `//!` comments.

## Functions

### `pub fn foo(x: i32) -> String`

Doc comment for foo.

### `export fn run()`

Doc comment for run.

## Structs

### `pub struct Point`

Doc comment for Point.

#### Fields

| Field | Type  | Description         |
| ----- | ----- | ------------------- |
| `x`   | `i32` | The x coordinate.   |
| `y`   | `i32` | The y coordinate.   |

## Traits

### `trait Eq`

Doc comment for Eq.

#### Methods

##### `fn eq(&self, other: &Self) -> bool`

Doc comment for eq.

## Enums

### `enum Color`

Doc comment for Color.

#### Cases

- `Red` — Doc for Red.
- `Green` — Doc for Green.

## Variants

### `variant Shape`

Doc comment for Shape.

#### Cases

- `Circle(f64)` — radius
- `Rectangle([f64, f64])` — width, height
- `Point` — no payload

## Effects

### `pub effect Stdout`

Doc comment for Stdout.

## Types

### `type Meters = f64`

Doc comment for Meters.

## Globals

### `pub global PI: f64`

Doc comment for PI.
```

Items without doc comments are still listed (with their signature) but have no description body.

### JSON Output Format

```json
{
  "module": "core/prelude/traits",
  "module_doc": "Core trait definitions.",
  "items": [
    {
      "kind": "trait",
      "name": "Eq",
      "visibility": "pub",
      "signature": "trait Eq",
      "doc": "Equality comparisons (== and !=).",
      "methods": [
        {
          "name": "eq",
          "signature": "fn eq(&self, other: &Self) -> bool",
          "doc": "Returns true if self equals other."
        }
      ]
    },
    {
      "kind": "struct",
      "name": "Point",
      "visibility": "pub",
      "signature": "struct Point",
      "doc": "A 2D point.",
      "fields": [
        { "name": "x", "type": "i32", "doc": "The x coordinate." },
        { "name": "y", "type": "i32", "doc": "The y coordinate." }
      ]
    }
  ]
}
```

JSON output enables downstream tools (static site generators, IDE tooltips, search indexes).

### Signature Rendering

Item signatures are rendered using the existing unparser infrastructure with a "signature-only" mode — function bodies, struct field initializers, and trait method bodies are omitted.

```
fn foo(x: i32, y: String) -> Result<i32, String>    // parameters + return type
struct Point { x: i32, y: i32 }                      // fields with types
trait Eq                                              // trait name + type params
enum Color { Red, Green, Blue }                       // all cases
variant Shape { Circle(f64), Point }                  // cases with payloads
type Meters = f64                                     // base type
```

### Scope and Item Ordering

Items in the output follow their source order (not alphabetical). This respects the author's intentional arrangement — often the most important item comes first.

Nested items (struct fields, trait methods, enum/variant cases, effect methods) are included under their parent.

`impl` blocks are folded into their target type's section. If `struct Point` has methods defined across multiple `impl Point` blocks, they appear together under the Point section.

## Implementation Plan

### Phase 1: CommentKind + Lexer (Minimal)

1. Add `DocLine` and `ModuleDoc` to `CommentKind`
2. Update lexer's `lex_line_comment()` to detect `///` and `//!`
3. Update formatter to preserve doc comment style (emit `///` not `//`)

### Phase 2: `wado doc` CLI (Core)

1. Add `Doc` to `Cmd` enum in `main.rs`
2. Create `wado-cli/src/doc.rs` with `DocOptions`, `parse_args()`, `run()`
3. Implement doc extraction using `CommentMap::leading_comments()`
4. Implement markdown output

### Phase 3: JSON Output + Filtering

1. Add `--format json` output
2. Add `--filter` and `--pub-only` options
3. Add `-o` file output

### Future

- HTML output with cross-references and search
- `wado doc --serve` for local doc server
- Integration with package registry for published docs
- Intra-doc links: `` [`Point`] `` resolves to the Point section

## Consequences

### Positive

- Zero AST changes — the existing `CommentMap` infrastructure handles everything
- 571 existing `///` lines in the stdlib work immediately
- Markdown output is both human-readable and pipeline-friendly
- JSON output enables IDE integration and static site generation
- `//!` module docs establish a file-level documentation convention early

### Negative

- Position-based doc extraction is fragile if comments are separated from items by blank lines (mitigated: require consecutive lines immediately before the item)
- No type-resolved links in Phase 1 (e.g., linking `Point` in a doc comment to the Point definition)

### Trade-offs

- **CommentMap vs AST embedding**: CommentMap avoids touching the parser and AST, keeping the change surface minimal. AST embedding would be more robust but requires modifying every item struct. CommentMap is the right choice for Phase 1; AST embedding can be added later if needed.
- **Markdown-first vs HTML-first**: Markdown is simpler to implement, renders well in terminals, and can be converted to HTML later. Starting with HTML would delay the initial release.
- **Source order vs alphabetical**: Source order respects author intent. Alphabetical is better for reference lookup. Source order is the default; alphabetical can be added as `--sort` later.
