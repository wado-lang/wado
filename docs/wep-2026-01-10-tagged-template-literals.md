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
struct type, unique to that template's static shape, holding one reference per
hole. The shape is the tuple of (cooked segments, raw segments, specifiers,
hole source texts, hole types): two templates with the same shape denote the
same type and share every instantiation of the tag. The type is anonymous —
nothing in the source can name it — and is reached only through the bound the
tag declares.

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

    /// The hole's value in `t`, by reference.
    pub fn get(&self, t: &T) -> &V;

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
`String::raw` is a tag that reads `raw()` and `raw_tail()`.

### Desugaring

```wado
sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`
```

lowers to

```wado
__tmpl: {
    let __h1 = user.name;
    sql(__Tmpl_a3f1 { h0: &id, h1: &__h1 })
}
```

A hole that is a place — a local, a global, a field path — is borrowed where it
stands. Any other hole expression is evaluated once, left to right, into a local
of the block, which is then borrowed. The holes are evaluated before the tag is
called and in source order, the same order an untagged template evaluates them.

The struct literal is the only place the synthesized type is built. It holds
references and nothing else, so passing it by value copies pointers; the values
behind the holes are never copied, whatever the tag does with them.

The synthesized `impl ReflectTemplate` carries the segments, specifiers and
sources as literals in its `members()` and `tail()` bodies, and `Hole::get`
projects the field at the hole's index.

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
  folded to a direct field read after inlining, as `StructField::get` is.
- Holes are references, so no value crosses into the tag by copy, and no
  last-use analysis is asked to elide one.

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
  element and body locations the variadic WEP already asks for.
- The binary grows by one tag instantiation per template shape, as it would by
  inlining a builder at each site.

## Known gaps

- The parser accepts a template only in primary position; a path followed by a
  backtick is a syntax error. Closing this is a postfix arm in the expression
  parser and one rule in `Wado.g4`, which already has the template lexer mode.
- `ReflectTemplate` and `Hole` do not exist. Closing this follows the struct
  kind: a synthesized impl per shape, the handle in `prelude/traits.wado`, and
  `Hole::fmt` reusing the specifier lowering the untagged template has.
- The desugaring — place holes borrowed in situ, other holes bound to block
  locals, the reference-only struct literal — is unwritten.
- The `format` tag, `String::raw`, and the equivalence fixture between an
  untagged template and `format` do not exist.
- No `wir_expect` fixture yet holds a tag to the cost above; the first `core:*`
  tag lands with one.
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
