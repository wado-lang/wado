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

### Visibility: Public API Only

`wado doc` outputs **only `pub` and `export` items**. Private items are implementation details and are excluded — there is no flag to include them.

For structs with private fields, the struct is documented but private fields are represented as `..`:

```wado
pub struct Config {
    pub name: String,
    secret: i32,       // private
}
```

Output signature: `pub struct Config { name: String, .. }`

The `..` signals that the struct cannot be constructed externally (some fields are hidden). Public fields are still documented individually.

### CLI Interface

```sh
wado doc [file.wado...]

# Single file
wado doc lib/core/prelude/traits.wado

# Multiple files / glob
wado doc lib/core/**/*.wado

# Project mode (with wado.toml)
wado doc

# Simple format (cheatsheet-style pseudo-code)
wado doc --simple lib/core/**/*.wado
```

| Flag       | Description                                        |
| ---------- | -------------------------------------------------- |
| `--simple` | Compact pseudo-code output (for cheatsheet generation) |
| `--help`   | Show usage                                         |

Default output is `full` (structured markdown). Output goes to stdout.

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

### Output Formats

Both formats use the same source as input:

```wado
//! Core trait definitions for equality and ordering.

/// Equality comparisons (== and !=).
pub trait Eq {
    /// Returns true if self equals other.
    fn eq(&self, other: &Self) -> bool;
}

/// Ordering comparisons (<, <=, >, >=).
pub trait Ord {
    fn cmp(&self, other: &Self) -> Ordering;
}

/// A 2D point.
pub struct Point {
    pub x: i32,
    pub y: i32,
}

pub struct Config {
    pub name: String,
    secret: i32,
}

impl Point {
    pub fn origin() -> Point { ... }
    pub fn distance(&self, other: &Point) -> f64 { ... }
    fn internal_helper(&self) { ... }
}

pub type Meters = f64;
pub global PI: f64 = 3.14159;

pub enum Color { Red, Green, Blue }

pub variant Shape {
    Circle(f64),
    Rectangle([f64, f64]),
    Point,
}

export fn run() { ... }
```

#### Full (default)

Structured markdown with doc comments, field descriptions, and method documentation. Suitable for reference documentation and static site generation.

Output (`wado doc file.wado`):

````markdown
# file

Core trait definitions for equality and ordering.

## Traits

### `pub trait Eq`

Equality comparisons (== and !=).

#### Methods

- `fn eq(&self, other: &Self) -> bool` — Returns true if self equals other.

### `pub trait Ord`

Ordering comparisons (<, <=, >, >=).

#### Methods

- `fn cmp(&self, other: &Self) -> Ordering`

## Structs

### `pub struct Point { x: i32, y: i32 }`

A 2D point.

#### Fields

- `x: i32`
- `y: i32`

#### Methods

- `pub fn origin() -> Point`
- `pub fn distance(&self, other: &Point) -> f64`

### `pub struct Config { name: String, .. }`

#### Fields

- `name: String`

## Types

### `pub type Meters = f64`

## Globals

### `pub global PI: f64`

## Enums

### `pub enum Color { Red, Green, Blue }`

## Variants

### `pub variant Shape`

#### Cases

- `Circle(f64)`
- `Rectangle([f64, f64])`
- `Point`

## Functions

### `export fn run()`
````

Rules for full format:
- Module doc becomes the description under the `#` heading
- Item doc comments appear as prose under their heading
- Fields and methods listed with doc if present, without if absent
- Same visibility rules (pub/export only, `..` for private fields)
- `impl` methods folded into their type's section

#### Simple (`--simple`)

Compact Wado pseudo-code showing the public API surface. No prose, no doc comments — just signatures. Designed for generating cheatsheets (like `docs/cheatsheet.md` stdlib sections).

Output (`wado doc --simple file.wado`):

```
// core trait definitions for equality and ordering.

pub trait Eq {
    fn eq(&self, other: &Self) -> bool;
}

pub trait Ord {
    fn cmp(&self, other: &Self) -> Ordering;
}

pub struct Point { x: i32, y: i32 }
pub struct Config { name: String, .. }

impl Point {
    pub fn origin() -> Point;
    pub fn distance(&self, other: &Point) -> f64;
}

pub type Meters = f64;
pub global PI: f64;

pub enum Color { Red, Green, Blue }

pub variant Shape {
    Circle(f64),
    Rectangle([f64, f64]),
    Point,
}

export fn run();
```

Rules for simple format:
- Module doc (`//!`) becomes a `//` comment at the top (lowercased first char)
- Item doc comments are omitted entirely
- Function bodies replaced with `;`
- Private fields replaced with `..`
- Private items and methods omitted
- `impl` blocks show only pub/export methods
- Globals omit initializers
- Source order preserved

### Signature Rendering

Both formats share the same signature rendering rules, using the existing unparser infrastructure with a "signature-only" mode:

```
pub fn foo(x: i32, y: String) -> Result<i32, String>;    // no body
pub struct Point { x: i32, y: i32 }                       // all fields public
pub struct Config { name: String, .. }                     // private fields hidden
pub trait Eq { ... }                                       // methods inside
pub enum Color { Red, Green, Blue }                        // all cases
pub variant Shape { Circle(f64), Point }                   // cases with payloads
pub type Meters = f64;                                     // base type
pub global PI: f64;                                        // no initializer
```

### Multi-File Output

When multiple files are given, their outputs are concatenated into a single document. Each file becomes a top-level section:

- **Full format**: each file gets a `# module_name` heading, then its items follow. Multiple files produce a single markdown document with multiple `#` sections.
- **Simple format**: files are separated by a `// --- module_name ---` comment line.

```sh
# Concatenates all core library docs into one document
wado doc lib/core/**/*.wado > stdlib-reference.md

# Cheatsheet for the entire stdlib
wado doc --simple lib/core/**/*.wado > stdlib-cheatsheet.wado
```

Files are output in the order they are given (or glob-expanded).

### Scope and Item Ordering

Items in the output follow their source order (not alphabetical). This respects the author's intentional arrangement — often the most important item comes first.

Nested items (struct fields, trait methods, enum/variant cases, effect methods) are included under their parent.

`impl` blocks are folded into their target type's section. If `struct Point` has methods defined across multiple `impl Point` blocks, they appear together under the Point section.

## Implementation Plan

### Phase 1: CommentKind + Lexer (Minimal)

1. Add `DocLine` and `ModuleDoc` to `CommentKind`
2. Update lexer's `lex_line_comment()` to detect `///` and `//!`
3. Update formatter to preserve doc comment style (emit `///` not `//`)

### Phase 2: `wado doc` CLI

1. Add `Doc` to `Cmd` enum in `main.rs`
2. Create `wado-cli/src/doc.rs` with `DocOptions`, `parse_args()`, `run()`
3. Implement doc extraction using `CommentMap::leading_comments()`
4. Implement full format (default): structured markdown with doc comments
5. Implement simple format (`--simple`): compact Wado pseudo-code for cheatsheets

### Future

- `--filter <pattern>` to show only items matching a name pattern
- `-o <file>` to write output to a file
- JSON output format for programmatic consumption
- HTML output with cross-references and search
- `wado doc --serve` for local doc server
- Integration with package registry for published docs
- Intra-doc links: `` [`Point`] `` resolves to the Point section

## Consequences

### Positive

- Zero AST changes — the existing `CommentMap` infrastructure handles everything
- 571 existing `///` lines in the stdlib work immediately
- Two formats serve different needs: simple for quick API lookup, full for reference docs
- `//!` module docs establish a file-level documentation convention early
- `..` for private fields clearly communicates construction constraints

### Negative

- Position-based doc extraction is fragile if comments are separated from items by blank lines (mitigated: require consecutive lines immediately before the item)
- No type-resolved links in Phase 1 (e.g., linking `Point` in a doc comment to the Point definition)

### Trade-offs

- **CommentMap vs AST embedding**: CommentMap avoids touching the parser and AST, keeping the change surface minimal. AST embedding would be more robust but requires modifying every item struct. CommentMap is the right choice for Phase 1; AST embedding can be added later if needed.
- **Full-default vs simple-default**: Full is the expected behavior for a `doc` command — complete documentation. Simple exists for a specific use case (cheatsheet generation) and is opt-in via `--simple`.
- **Source order vs alphabetical**: Source order respects author intent. Alphabetical is better for reference lookup. Source order is the default; alphabetical can be added as `--sort` later.
