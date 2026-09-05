# WEP: Tagged Template Literals

## Context

A template string `` `id = ${id}` `` renders every interpolation through
`Display`. That is the right default for text and the wrong one for every
structured sink: a SQL query wants `${id}` bound as a parameter, an HTML
fragment wants `${name}` escaped for the context the literal text put it in, a
log line wants `${user.id}` as a typed field beside the message, a URL wants
each segment percent-encoded. Each of these sinks is a function of two things
the untagged template throws away — the literal text around a hole, and the
hole's value in its own type.

A tag names that function: `` sql`SELECT * FROM users WHERE id = ${id}` ``
hands `sql` the literal segments and the typed values instead of a rendered
`String`. The surface is JavaScript's; the model underneath is Wado's own.

The original design of this feature was a compile-time decoder — a tag was an
effect-free `fn(String) -> T` run by the compiler, motivated by embedding
base64 blobs and validating regex literals. Both motivations have since been
served elsewhere: `b"…"` byte strings and `#include_bytes` embed binary as a
constant data segment, and [Kiln](./wep-2026-04-12-kiln.md) validates
file-shaped DSL inputs. What no other mechanism serves is the interpolation
case above, and it does not need compile-time execution at all. So this WEP
puts the tag's job where the remaining demand is, and treats folding at compile
time as an optimization the optimizer may perform, never as the semantics.

Three things the language has grown since decide the shape:

- [Variadic type parameters](./wep-2026-03-14-variadic-type-parameters.md)
  and [tuple enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md)
  walk a heterogeneous tuple with one body per element, unrolled.
- [Reflection](./wep-2026-06-13-reflect-derivation.md) exposes a declaration's
  structure as a sealed, compiler-synthesized trait whose member handles are
  constants, so a walk over them folds to the code a hand-written body would be.
- [Constant object globalization](./wep-2026-05-31-const-object-globalization.md)
  hoists a closed constant expression to a global, and `niri` projects fields
  back out of one.

Together they let the static part of a template — its literal segments, its
specifiers, its arity and the type at each hole — live in a _type_, so that a
tag body reading them is reading constants. That is what makes a tagged template
cost what the untagged one costs.

## Decision

### Surface

```wado
let q = sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`;
let s = String::raw`C:\path\${name}`;
```

A tag is a path expression — a function name or a static method path — written
directly before the backtick, with no whitespace between. The backtick is a
postfix on the path, at the precedence of a call. Only a path may be a tag: a
call result, a closure, or a parenthesized expression is not one, so
`` f()`x` `` and `` (g)`x` `` are syntax errors.

The template body is lexed exactly as an untagged one: `${expr}` and
`${expr:spec}` holes, `{` and `}` literal, `\$` the only escape a hole needs.
The tag changes nothing about how the literal reads, only about what it means.

A tag is an ordinary function — any effects, any return type — that takes one
argument satisfying `ReflectTemplate`. Nothing about being a tag appears on the
declaration; a function is a tag because a call site wrote it before a
backtick.

### The template type

Each template literal that carries a tag denotes a value of a compiler-synthesized
struct type, unique to that template's static shape, holding one field per
hole. The shape is the tuple of (raw segments, specifiers, hole source texts,
hole types): two templates with the same shape denote the same type and share
every instantiation of the tag. The type is anonymous — nothing in the source
can name it — and is reached only through the bound the tag declares.

A hole's field carries its value without copying it and without boxing it. A
type whose reference is a bare GC handle — `struct`, `List`, `String`, `i128`
— is held as `&V`; a type whose reference the compiler would box — a
primitive, an `enum`, `flags`, a `variant`, a function — is held as `V`, since
a scalar copy is free and a box is an allocation. One predicate decides, the one
[Reference Representation](./wep-2026-06-13-reference-representation.md)
boxes by. The tag never sees the difference: `Hole::get` answers `V` either
way.

The type is one of the reflected kinds, under the same seal as the five
declaration kinds: compiler-synthesized, a user `impl` a compile error,
callable only where `T` is concrete.

```wado
#[compiler_item("reflect_template")]
internal trait ReflectTemplate: Reflect {
    /// The hole types as a tuple `[V_0, V_1, …]` — the payload pack.
    type Holes;

    /// The per-hole member tuple `[Hole<Self, V_0>, Hole<Self, V_1>, …]`.
    type Members;

    /// Returns the per-hole members.
    fn members() -> Self::Members;

    /// The literal text after the last hole; the whole template when it has
    /// none. Escapes processed.
    fn tail() -> String;

    /// `tail()` with escapes preserved.
    fn raw_tail() -> String;
}
```

A hole is a member handle like `StructField`: sealed, fields private, minted only
by `members()`, and not itself reflectable.

```wado
#[compiler_item("hole")]
pub struct Hole<T, V> { … }

impl<T, V> Hole<T, V> {
    /// The literal text between the previous hole (or the start) and this one,
    /// escapes processed.
    pub fn lit(&self) -> String;

    /// `lit()` with escapes preserved: `\n` is a backslash and an `n`.
    pub fn raw(&self) -> String;

    /// The hole's value in `t`. A read-only use shares the storage, as a
    /// `StructField::get` does; only a mutation copies.
    pub fn get(&self, t: &T) -> V;

    /// The hole's expression as written: `"user.name"`.
    pub fn source(&self) -> String;

    /// Whether the hole wrote a `:spec`.
    pub fn has_spec(&self) -> bool;

    /// Renders the value as the untagged template would: the trait method the
    /// specifier's type selects (`Display::fmt` when it names none), on a
    /// `Formatter` carrying the specifier's fill, alignment, sign, width and
    /// precision over `f`'s buffer.
    pub fn fmt(&self, t: &T, f: &mut Formatter);
}
```

Every accessor but `get` and `fmt` returns a constant, and `fmt`'s body is
synthesized per hole, so the choice of trait method — a compile-time decision,
as in an untagged template — is closed inside it. The tag never sees a
pre-rendered `String` it did not ask for, and a tag that rejects specifiers
reads `has_spec()`.

The literal segments are the same string in two spellings; both are constants,
and the one a tag never reads is dead code. That is the whole cooked-versus-raw
mechanism: no mode, no marker type, and no inspection of the tag's signature.
`String::raw` is a tag that reads `raw()` and `raw_tail()`. A raw segment is
still a template segment, so it spells only the escapes the language has:
`` String::raw`a\nb` `` is `a\nb` as written, and `\p` is the error it is in
any template.

### Desugaring

```wado
sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`
```

lowers to

```wado
__tagged: {
    sql(__Tmpl_a3f1 { h0: id, h1: &user.name })
}
```

`id` is an `i32`, so its hole holds the value; `user.name` is a `String`, so
its hole holds the handle. A hole that is a place — a local, a global, a field
path rooted at one — is read or borrowed where it stands. Any other hole
expression is evaluated once, left to right, into a local of the block, which
the literal then reads or borrows. The holes are evaluated before the tag is
called and in source order, the same order an untagged template evaluates them.

The struct literal is the only place the synthesized type is built. Passing it
by value copies scalars and handles; the storage behind a hole is never copied,
whatever the tag does with it. Borrowing a local of a handle type marks nothing
on that local — only a box-target type's local is retagged by a borrow, and
those are the holes held by value.

The synthesized `impl ReflectTemplate` carries the segments, specifiers and
sources as literals in its `members()` and `tail()` bodies, and `Hole::get`
projects the field at the hole's index, dereferencing a handle field.

### Writing a tag

A tag binds its template through `ReflectTemplate`, binds the hole pack off
`Holes`, and walks `members()`:

```wado
fn sql<T: ReflectTemplate<Holes = [..V]>, ..V: ToSqlParam>(t: T) -> SqlQuery {
    let mut query = "";
    let mut params: List<SqlParam> = [];
    for let h of ReflectTemplate::<T>::members() {
        query.push_str(&h.lit());
        query.push_str(&"?");
        params.push(h.get(&t).to_sql_param());
    }
    query.push_str(&ReflectTemplate::<T>::tail());
    return SqlQuery { query, params };
}
```

`..V: ToSqlParam` is what makes `` sql`… ${some_struct}` `` a bound error
naming the type, rather than a missing-method error inside the unrolled body.
The walk is tuple `for-of`, so a state carried across holes is a local, and one
per hole is the `.enumerate()` index:

```wado
fn html<T: ReflectTemplate<Holes = [..V]>, ..V: Display>(t: T) -> Html {
    let mut out = "";
    let mut ctx = HtmlContext::Text;
    for let h of ReflectTemplate::<T>::members() {
        out.push_str(&h.lit());
        ctx = ctx.advance(&h.lit());      // attribute? script? text?
        ctx.escape_into(h.get(&t), &mut out);
    }
    out.push_str(&ReflectTemplate::<T>::tail());
    return Html { out };
}
```

`source()` gives a structured sink its field names for free:

```wado
fn info<T: ReflectTemplate<Holes = [..V]>, ..V: Serialize>(t: T) with Log {
    let mut msg = "";
    let mut fields = FieldMap::new();
    for let h of ReflectTemplate::<T>::members() {
        msg.push_str(&h.lit());
        h.fmt(&t, &mut Formatter::new(&mut msg));
        fields.insert(h.source(), h.get(&t));   // "user.id" => 42
    }
    msg.push_str(&ReflectTemplate::<T>::tail());
    emit(msg, fields);
}
```

A tag with no use for holes constrains the pack to the empty tuple.

```wado
fn regex<T: ReflectTemplate<Holes = ()>>(t: T) -> Regex {
    return Regex::compile(ReflectTemplate::<T>::tail()).unwrap();
}
```

### The untagged template is a tag

An untagged template means what the prelude's `format` tag means:

```wado
fn format<T: ReflectTemplate<Holes = [..V]>, ..V>(t: T) -> String {
    let mut out = "";
    for let h of ReflectTemplate::<T>::members() {
        out.push_str(&h.lit());
        h.fmt(&t, &mut Formatter::new(&mut out));
    }
    out.push_str(&ReflectTemplate::<T>::tail());
    return out;
}
```

The compiler keeps its direct lowering for the untagged case
([String Template Desugaring](./wep-2026-01-20-string-template-desugaring.md));
the equivalence is a property the fixtures hold it to, not a path it takes.

### Cost

What the tag body compiles to, once monomorphized, inlined and folded:

- `for let h of members()` is unrolled — one straight-line block per hole.
- `members()` is a closed constant expression: globalization hoists it, and
  every `lit()` / `raw()` / `source()` / `has_spec()` is a field projected out
  of that global, which `niri` folds to the literal. `push_str(&h.lit())` is
  the same instruction sequence as `push_str(&"SELECT …")`.
- `get()` is a projection of the template struct's field at a constant index,
  folded to a direct field read after inlining, as `StructField::get` is. Its
  result binds read-only in every tag above, so the value-copy planner shares
  the storage rather than copying it — the path `core:serde`'s `f.get(self)`
  already takes.
- A hole holds a handle or a scalar, so no storage crosses into the tag by
  copy, and no last-use analysis is asked to elide one.

The residue is the code a hand-written builder would be: one constant append
per segment and one typed operation per hole. This is the design's claim, and
`wir_expect` fixtures hold each tag in `core:*` to it. Where a read fails to
fold, that is the optimizer's gap to close, not a reason to move the segments
into a runtime value.

Each distinct template shape instantiates the tag once, as inlining a builder at
each site would. Two sites with the same shape share one instantiation.

### Compile-time evaluation

A tag call is a call. Whether it runs at compile time is decided the way every
other call is: `niri` folds a pure call over constant arguments when it can, and
a wasm-CTFE backend may fold more once it exists
([NIR Interpreter](./wep-2026-04-27-nir-interpreter.md)). A hole-less template
under a pure tag — `` regex`^[a-z]+$` `` — is a candidate; a template with
holes is a candidate only where the holes are constant. Nothing in the tag's
meaning depends on the outcome, and a tag may carry effects. A program that
wants a hole-less tag evaluated once writes a `global`, which the lazy-init
path already guarantees.

### Deliberate omissions

- No tag on a non-path expression, and no whitespace between the tag and the
  backtick: the grammar stays a postfix on a path, which is what the reserved
  `<` position and `wado format` can hold.
- No compile-time execution in the semantics, per the above.
- No `Template<..V>` runtime value holding the segments as an array. The
  segments as data cost an allocation and an indexed load per segment at every
  evaluation, and left the specifier as either a lost type or a pre-rendered
  string. The type carries them instead.
- No sink trait (`begin` / `lit` / `hole` / `finish`) as the tag protocol. It
  reaches the same cost, but a tag that needs the whole template — to count
  holes before emitting, or to read a later segment — cannot be written against
  it, and it would make a tag a type rather than a function.
- No literal index into `Members` or `Holes`: a pack is walked, never indexed
  (WEP 2026-03-14).

## Alternatives considered

Type-level strings. The segments could ride as type arguments —
`Template<"SELECT ", " AND ", [..V]>` — instead of in a synthesized nominal
type. It is the more general mechanism: a `Route<"/users/{id}">` or a
`Regex<"…">` would name its literal in its type, and `#[param]` could carry a
value parameter the same way. It needs value parameters in the type system and
a second pack alongside `..V`, neither of which exists, and it buys a tagged
template nothing the synthesized type does not. Should it arrive, the
synthesized type can be re-expressed as an instance of it without changing what
a tag can observe, since the type is unnameable.

Compile-time execution as the semantics — the original design. A tag was an
effect-free `fn(String) -> T` the compiler ran. It required a whole-program
evaluator the toolchain does not have, restricted tags to pure functions, and
solved a problem (`` base64`…` ``) that literals and `#include_bytes` since
solved better. Kept as an optimization opportunity, dropped as a promise.

## Consequences

- A tagged template costs what an untagged one costs: constant appends and
  typed operations, no runtime segment table, no value copies.
- Tag authors write one generic function, in Wado, against traits the language
  already has. SQL, HTML with contextual escaping, structured logging, URL
  building and raw strings are library code.
- The `ReflectTemplate` kind is a sixth entry under `Reflect`, synthesized per
  template shape rather than per declaration. `Hole` joins the four member
  handles under the same seal.
- Type checking of the walk body is at monomorphization, as it is for every
  pack walk. A pack bound (`..V: Trait`) makes the common failure a bound
  error at the call site; a failure inside the body needs the call-site,
  element and body locations the variadic WEP already asks for. Enforcing
  that bound closed a gap every projected pack had: the bound check ran
  before the projection made the pack concrete and never asked again, so
  `..F: Trait` off `FieldTypes` held for any struct.
- The binary grows by one tag instantiation per template shape, as it would by
  inlining a builder at each site.

## Implementation

The template type is an anonymous struct — the precedent is the struct literal
`{ x: 1, y: "s" }`, which already satisfies `ReflectStruct` through the same
machinery (`tests/fixtures/reflect_anon_struct.wado`) — and `ReflectTemplate`
follows `ReflectStruct` at every step of the reflect pipeline. Nothing below
introduces a mechanism; each item names the existing one it extends.

### Type table (`tir.rs`)

- `AnonShape::Template(TemplateShape)` beside `Fields` and `Synthetic`.
  `TemplateShape { segments: Vec<String>, holes: Vec<TemplateHole> }`, the
  segments raw, each hole `{ ty, spec: Option<String>, source }`. The shape is
  the interning key, so two sites of one shape reach one `AnonStructId` through
  `intern_shape`, as struct literals do.
- The struct's fields are derived from the shape: `h{k}` typed
  `hole_field_ty(V_k)` — `&V_k` where `V_k` is not a box target, `V_k` where it
  is. The box-target predicate moves out of `lower::plan::boxing`'s
  `create_needed_box_types` into one `TypeTable::is_boxed_reference_target`,
  which the boxing pass then consumes, so the two cannot drift — the single
  predicate Reference Representation calls for.
- `reflect_kind` answers `ReflectTemplate` for a `Struct { def: Anon(shape) }`
  whose shape is a template; `is_sealed_reflect_member` adds the `Hole` handle.
- `anon_struct_mangle` renders a template shape under a `$tmpl` prefix over the
  shape's hash, the spelling `Reflect::type_name()` then answers. The mangle is
  fixed when the shape is interned: it renders the hole types, and erasure
  later redirects a newtype among them to its base, so a second rendering
  would name a struct WIR never registered — the same failure a struct literal
  with a newtype field had.

### Prelude (`lib/core/prelude/traits.wado`, `lib/core/builtin.wado`)

- `#[compiler_item("reflect_template")] internal trait ReflectTemplate: Reflect`
  with `Holes`, `Members`, `members()`, `tail()`, `raw_tail()`, each method
  carrying its own `compiler_item`.
- `#[compiler_item("hole")] pub struct Hole<T, V> { index, lit, raw, source,
  has_spec }` with the constant accessors reading fields, and two bridged ones:
  `get` is `builtin::hole_get::<T, V>(t, self.index)`, `fmt` is
  `builtin::hole_fmt::<T>(t, self.index, f)`. Both builtins are bodyless
  markers beside `struct_field_get`.
- `compiler_item.rs` gains `ReflectTemplate`, `ReflectTemplateHole` and the
  three method items, in `ALL`, `attr_name`, `expected_kind`, `is_required`.
- `format` in `core:prelude` and `String::raw` in the string module, written as
  the tags above.

### Parser and AST

- `Expr::TaggedTemplate(Box<TaggedTemplateExpr { id, tag: IdentExpr, template:
  TemplateStringExpr, span }>)`. A new variant rather than a field on
  `TemplateStringExpr`: a tagged template is a call, and the arms that treat a
  template as a string literal — overload classification's `ArgClass::StrLit`,
  newtype literal coercion — must not see it. Every exhaustive match then names
  the arm; the visitor walks the tag so hover, references and comment
  attachment reach it.
- `parse_postfix_expr` takes a `TemplateStringLit` arm: the receiver must be
  `Expr::Ident` and `expr.span().end == peek().span.start`. Whitespace and
  comments never reach the token stream, so a byte gap is the adjacency test. A
  non-path receiver is a syntax error naming the rule; a gap falls through to
  the ordinary unexpected-token error.
- `unparse` prints the tag then the template with nothing between, in both
  printers. `Wado.g4` adds `taggedTemplate : (tagOwner '::')* tagName
  templateString` to `primary` and `primaryNoStruct`; the grammar cannot see
  the gap, so the whitespace case is a `compile_error` fixture under
  `check-grammar`'s second invariant. `Wado.highlights.scm` captures `tagName`
  as `@function`, and the LSP's call heuristic reads an adjacent template as a
  call, so both sides colour the tag alike.

### Annotate (`elaborator/tagged_template.rs`)

`resolve_tagged_template` records facts on the expression's `AstId` and returns
the tag's result type:

1. Resolve each hole expression with no expected type; validate segments through
   the same unescape gate `resolve_template_string` runs.
2. Build the shape, intern it, `make_struct(StructDef::Anon(shape))`, and on a
   first sighting register `StructFieldInfo` under `anon_struct_fields` and push
   the `TirStruct` onto `pending_anonymous_structs` — the minting
   `resolve_anonymous_struct_literal` does, factored into one helper both call.
3. Resolve the tag as a one-argument call whose argument type is known and has
   no AST: `resolve_call_with_args` is `resolve_call` over arguments already
   typed, keyed by the template's `AstId`, so signature lookup, instantiation,
   the projection of `..V` from `Holes`, the bound check and the
   `StaticMethodDispatch` fact are the ones a spelled call records. A tag of
   any other arity, or one that is not a function, is that path's diagnostic.
4. Record the template type under the template's `AstId`
   (`tagged_templates`); place-ness is reify's to read off the AST.
5. The tag identifier's use→def edge falls out of the call resolution.

### Reify

`reify_tagged_template` emits a labeled block: one `let __h{k}` per `Temp`
hole in order, then the call the dispatch fact names — the `Call` shape
`reify_call`'s static-dispatch arm builds, factored to take reified arguments —
over a `StructLiteral` whose field `k` is the place or temp, borrowed when the
field type is `&V_k`. The `TirStruct` reaches the module through the pending
list `reify` already drains.

### Reflect resolution (`elaborator/reflect.rs`, `solver_bridge.rs`, `trait_query.rs`)

- `ReflectDispatch::Template`, a `TemplateMethods::resolve` reading the method
  names off the registry, and `is_reflect_template_trait_call`, wired into
  `reflect_dispatch_of` and `resolve_static_method_call`.
- `reflect_template_subject` reads hole types off the shape. The concrete
  resolver types `members()` as `payload_members_ty(ReflectTemplateHole, T,
  holes)` and `tail()` / `raw_tail()` as `String`. The generic resolver, for a
  body written against `T: ReflectTemplate<Holes = [..V]>`, types `members()`
  as the mapped pack `[..Hole<T, V>]` through `payload_member_pack_bound_ty`,
  and `emit_missing_pack_bound` spells `Holes = [..V]`.
- `concrete_reflect_assoc_type` gains the template arm, so a call site projects
  `Holes` and `Members` before synthesis has registered them.
- `solver_bridge`: `ReflectTemplate` joins `REFLECT`; a template shape lowers
  under its own `DeclKey::TemplateShape(AnonStructId)` head with no arguments,
  and `state_reflect_facts` states it visible from every module.
- `trait_query`: `OnBoundTrait::ReflectTemplate`, `classify_on_bound_trait`,
  and `reflect_members_visible` answering true.
- The seal list in `orchestration.rs` names `ReflectTemplate`.

### Synthesis (`synthesis/traits.rs`, `synthesis/template.rs`)

- `collect_reflect_targets` routes a template-shaped `TirStruct` to
  `generate_template_reflect_impls` instead of the struct kind. Per shape it
  emits `type_name` and `wire_name_policy` under the root, registers `Holes =
  [V_k]` and `Members = [Hole<T, V_k>]`, and emits `members()` through
  `generate_reflect_member_tuple_fn` with one `Hole` literal per hole
  (`lit` cooked through `unescape_template_string`, `raw` verbatim), plus
  `tail()` and `raw_tail()` returning literals. The impl is recorded into
  `TraitEnv` like the struct kind's.
- Two bridges per shape, minted beside `generate_field_bridge_helpers`:
  `$hole_get$<shape>$<V>` (one per erased hole type, `match index` over the
  holes of that type, dereferencing a handle field) and `$hole_fmt$<shape>`
  (`match index`, each arm the interpolation `build_template_block` would emit
  for that hole: `trait_fmt_call` on the hole's value with the `Formatter`
  `build_formatter_expr` builds over `f.buf`, or `f` itself when the specifier
  sets no field). `$hole_fmt` is minted inline-always: its index is a constant
  at every site `Hole::fmt` reaches once `members()` folds, so the splice keeps
  one arm where a call would keep the whole dispatch, which the threshold
  refuses.
- `lower/translate.rs` folds `builtin::hole_get` / `hole_fmt` calls to the
  bridge names beside its `struct_field_get` arm; `name.rs` owns both names.
- A template shape is never generic, so `reflect_bridge`'s post-monomorphization
  minting is not involved.

### Fixtures

- `tagged_template_sql.wado`: typed holes through a `ToSqlParam` pack bound,
  and `wir_expect:O2` holding the body to constant appends with no `$hole_get`
  call surviving.
- `tagged_template_html.wado`: state carried across holes.
- `tagged_template_members.wado`: every `Hole` accessor and the tails, with
  and without holes.
- `tagged_template_holes.wado`: hole evaluation order and count, a scalar
  hole's copy, a handle hole's sharing, two sites of one shape.
- `tagged_template_format_equiv.wado`: `format` against the untagged form over
  the specifier matrix.
- `tagged_template_string_raw.wado`: `String::raw` keeps escapes verbatim.
- `anon_struct_newtype_field.wado`, `reflect_pack_bound_free_fn_error.wado`:
  the two pre-existing defects the work surfaced, pinned.
- Errors: a non-path tag, whitespace before the backtick, a tag of the wrong
  arity, an unsatisfied `..V` bound naming the hole type, a hole whose type
  mentions a type parameter.

### Order

Prelude and registry first, so every later step compiles against the trait;
then the reflect resolution, since the call-site bound check needs the kind
before any tag call can be typed; then parser, annotate and reify; then
synthesis and the fold; then the prelude tags and fixtures.

## Known gaps

- A hole whose type mentions the enclosing function's type parameter. The
  shape would have to be generic over that parameter and instantiated with the
  function, which an anonymous struct is not today; until then the site is a
  diagnostic, and a generic body reaches a tag by passing a concrete value.
- A shape is interned per module, as struct literals are, so two modules
  writing one template instantiate the tag twice. Cross-module sharing is a
  size optimization the interner's key can take later.
- A tag with a turbofish (`` f::<T>`…` ``): the bare-turbofish path leaves the
  identifier's span at the name, so adjacency fails and the site is a syntax
  error. Nothing needs it yet.
- The monomorphization-time diagnostics the variadic WEP lists — call site,
  element index, body location — are what a failing tag body reports through,
  and are still open there.

## Related WEPs

- [String Template Desugaring](./wep-2026-01-20-string-template-desugaring.md) — the untagged lowering, and `Hole::fmt`'s per-specifier body
- [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md) — the walk
- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — the pack bound and pack binding
- [Library-Defined Derivation over `Reflect*`](./wep-2026-06-13-reflect-derivation.md) — the kind and member-handle model this joins
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md) — why `members()` is free
- [Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md) — what `has_spec()` and `fmt()` reflect
- [Markup Dialect](./wep-2026-08-29-markup-dialect.md) — the HTML consumer

## References

- [MDN: Template literals](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Template_literals)
- [Scala: String interpolation](https://docs.scala-lang.org/scala3/book/string-interpolation.html) — interpolators as library methods over a `StringContext`
