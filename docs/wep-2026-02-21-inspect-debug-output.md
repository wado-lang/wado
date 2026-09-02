# WEP: Inspect (Debug Output)

## Context

`${x:?}` and `${x:#?}` render a value for debugging. Every type must be
inspectable, including one whose author wrote no impl. This WEP defines the
output per type and how an impl reaches a type that declared none.

`${x}` does not fall back here. `Display` is not derived for arbitrary types,
so `${x}` on a type without one is a compile error naming `${x:?}` — see
[WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

## Decision

### The trait

```wado
internal trait Inspect { fn inspect(&self, f: &mut Formatter); }
```

`internal`: it is the compiler's dispatch target for `?`, not a name to write
in a bound. A type may write an `impl Inspect` to override the derived one.
The `Formatter` and the rest of the trait family are in
[WEP: Format Traits](./wep-2026-02-01-format-traits.md).

`Inspect` is total. Three sources cover the type system:

| Source                               | Covers                                                                                                     |
| ------------------------------------ | ---------------------------------------------------------------------------------------------------------- |
| Hand-written in `core:prelude`       | primitives, `()`, `!`, `String`, `char`, `&T` / `&mut T`, sequences, tuples, `TreeMap`, `TreeSet`, `Value` |
| Blanket impl over a `Reflect*` bound | struct, variant, enum, flags, newtype                                                                      |
| Synthesised per type                 | resource, `fn(..)` dispatch stub                                                                           |

### Output format

Output follows Wado literal syntax where it has one.

| Type                       | Output                                 | Example                       |
| -------------------------- | -------------------------------------- | ----------------------------- |
| `i32`, `i64`, etc.         | Decimal number                         | `42`                          |
| `u8`, `u16`, etc.          | Decimal number                         | `255`                         |
| `f32`, `f64`               | Float number                           | `3.14`, `inf`, `-0.0`         |
| `bool`                     | `true` / `false`                       | `true`                        |
| `char`                     | Quoted character                       | `'A'`                         |
| `String`                   | Escaped, quoted string                 | `"hello\"world"`              |
| `()` (unit)                | `()`                                   | `()`                          |
| Struct                     | `Name { field: value, ... }`           | `Point { x: 10, y: 20 }`      |
| Struct (generic)           | `Name { field: value }` (no type args) | `Box { value: 42 }`           |
| Struct (`#[secret]` field) | Field omitted, `..` appended           | `Foo { visible: 1, .. }`      |
| Tuple                      | `[elem, ...]`                          | `[1, "a", true]`              |
| `List<T>`                  | `[elem, ...]`                          | `[1, 2, 3]`                   |
| `TreeMap<K, V>`            | `{key: value, ...}`                    | `{"a": 1, "b": 2}`            |
| `TreeSet<T>`               | `{elem, ...}`                          | `{10, 20, 30}`                |
| `Value` (json\_value)      | JSON-like representation               | `{"key": "val"}`              |
| Enum                       | `TypeName::CaseName`                   | `Color::Red`                  |
| Variant (no payload)       | `TypeName::CaseName`                   | `Shape::Dot`                  |
| Variant (with payload)     | `TypeName::CaseName(inspect(payload))` | `Shape::Circle(5.0)`          |
| `Option<T>`                | As any variant                         | `Option::Some(42)`            |
| Flags                      | `TypeName::MemberName \| ...`          | `Perms::Read \| Perms::Write` |
| Flags (none)               | `TypeName::none()`                     | `Perms::none()`               |
| Newtype                    | `value as TypeName`                    | `1.5 as Meters`               |
| Resource (opaque handle)   | `TypeName#0xHH`                        | `Fields#0x01`                 |
| `&T`                       | `&inspect(inner)`                      | `&42`                         |
| `&mut T`                   | `&mut inspect(inner)`                  | `&mut Point { x: 1, y: 2 }`   |
| Closure (default)          | Signature only                         | `\|i32\| -> i32`              |
| Closure (`#` alternate)    | TIR unparsed source                    | `\|x: i32\| (x + 1)`          |

`String` escaping covers `"`, `\`, and `\n` / `\r` / `\t`; any other control
char (`< 0x20` or `0x7f`) is rendered as `\u{HEX}`. Printable non-ASCII is
emitted verbatim.

Rules the table does not carry:

- Struct fields are written in declaration order. A `#[secret]` field is
  dropped and `..` appended; a struct whose fields are all secret renders
  `Name { .. }`.
- Enum, variant and flags cases are always type-qualified — `Color::Red`, not
  `Red` — which is what construction syntax spells. `Option` is an ordinary
  variant and gets no short form.
- A newtype inspects its base value, then appends `as` and the type's own
  name, mirroring the cast that builds it.
- A resource is an opaque handle, so it renders as the type name and the
  handle in lowercase hex through `LowerHex` — never as a constructible value.
- A reference prefixes `&` or `&mut` and inspects the referent. References
  are GC-managed, so the dereference is always safe.

### Alternate form

`${x:#?}` sets `Formatter.alternate`; every implementation reads the one flag
rather than dispatching to a second trait. The composite implementations
pretty-print with one element per line, tracking depth through the
`Formatter`'s `indent` field (`open_brace` / `close_brace` /
`write_newline_indent`):

```wado
let arr: List<i32> = [1, 2, 3];
println(`${arr:#?}`);
// [
//   1,
//   2,
//   3,
// ]
```

A closure is the one type whose `Display` delegates to `Inspect`, so `${f}`
and `${f:?}` both write the signature and `${f:#}` and `${f:#?}` the source.

### Truncation

`Inspect` of a `String` or a sequence caps its length at `DEFAULT_SEQ_LIMIT`
(256) with no precision in the spec, and marks what it dropped — `"hello"...`
for a string, `[1, 2, ...]` for a sequence. `Display` neither caps nor marks.
See
[WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md).

### Dispatch

`${x:?}` synthesises an ordinary `Inspect::inspect` call. There is no marker
call and no inspect-specific pipeline phase: trait resolution, the
monomorphizer, lowering and DCE handle it as they handle any other trait call,
so an unreached inspect path is dropped like any other dead code.

The derived impls come from two places. Struct, variant, enum, flags and
newtype types are covered by the blanket impls over `ReflectStruct` /
`ReflectVariant` / `ReflectEnum` / `ReflectFlags` / `ReflectNewtype` in
`core:prelude/traits`, so nothing is emitted per type. What reflection does not
reach — resources, tuple and generic-resource instances, and the `fn(..)`
dispatch stubs — `synthesis::traits` emits alongside the other auto-derived
traits, skipping any receiver that has a methodful impl of its own.

### Closure inspect via runtime dispatch

`Inspect` on a closure value must produce per-literal output (for `:#?`, the
closure's own source) regardless of how the value reaches the call site —
directly through a local, or indirectly through a function parameter, struct
field, or global. Indirect dispatch rules out a pure compile-time
substitution: the per-literal information must travel with the value.

Wado closures lower to two complementary representations (see
[WEP: Closure Implementation](./wep-2026-01-16-closure-implementation.md)):

1. Specialised: the local has type `&__Closure_N` (the per-literal functor
   struct). Used when every reference to the local is in callee position.
2. Canonical: the value is wrapped in `CanonicalClosure_K` so any holder of a
   `fn(..)` value can invoke or inspect it. The lowering escape analysis
   demotes a local to canonical as soon as it appears in any non-callee
   position (struct field, fn argument, return value, global assignment, or
   rebinding).

The canonical struct carries the runtime vtable for inspectable signatures. To
make a single dispatch stub serve every parameter shape with the same
`(N, Ret)`, all inspectable canonical structs share a Wasm GC supertype:

```wat
(type $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))))

(type $CanonicalClosure_K (sub $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))
  (field $func        (ref $canonical_fn_K)))))
```

`$canonical_callback_fn = (env: structref, f: structref) -> ()` is uniform
across signatures. The supertype prefix means `ref.cast self to
$canonical_inspectable_base` succeeds for any inspectable closure value,
regardless of `K` — so two distinct function types like `fn(i32) -> i32` and
`fn(String) -> i32` (same `(arity, return_type)`, different parameter types)
reach the same dispatch stub without per-signature tables.

Per-literal artifacts, synthesised at lower time:

1. `__Closure_N` struct — holds captures.
2. `__call` method — the closure body.
3. `__Closure_N^Inspect::inspect(&self, &mut Formatter)` — writes the
   signature, e.g. `|i32, i32| -> i32`, or under `f.alternate` the
   TIR-unparsed source, e.g. `|x: i32| (x + 1)`. A capturing closure's
   captured bindings appear as free variables in that source.
4. `__Closure_N^Display::fmt` — delegates to the `Inspect` impl.

Per-literal canonical-path wrappers, registered in WIR build for inspectable
signatures only:

1. `__closure_wrapper_N` — casts env, calls `__call`.
2. `__closure_inspect_wrapper_N` — casts env, calls
   `__Closure_N^Inspect::inspect`.

Dispatch stubs, one per inspectable `(N, Ret)`:

- `fn(..)^Inspect::inspect(&self, &mut Formatter)`: cast `self` to
  `$canonical_inspectable_base`, load `inspect`, `call_ref` with `(env, f)`.
- The stub is emitted as `FunctionKind::FnCanonicalDispatch` with a bodyless
  TIR placeholder; WIR build installs the instructions directly. Bodyless
  functions bypass the inliner and other TIR-body walkers, so no
  `inline(never)` workaround is needed.

The specialised path takes a redirect at lowering: `fn(..)^Inspect` calls on a
known-local closure receiver rewrite to direct calls on `__Closure_N^Inspect`.
The dispatch stub and canonical vtable are bypassed entirely; standard DCE then
removes the per-literal impls when no inspect call site survives.

### Zero overhead when unused

Two whole-program gates keep programs that do not inspect closures from paying
for the runtime-dispatch machinery:

1. Schema gate, per `(N, Ret)`: only call shapes with a reachable
   `fn(..)^Inspect` dispatch stub get the inspectable canonical layout. Other
   signatures use the slim `(struct env func)` shape with no shared supertype,
   no inspect field, and no per-literal wrappers.
2. Per-functor gate, per `(N, Ret)`: a pre-DCE scan collects the
   `(arity, return_type)` signatures an `Inspect` call actually reaches. TIR
   DCE roots `__Closure_N^Inspect` from `ClosureToCanonical` only for those, so
   a program that never prints a closure of that shape drops the impl and its
   per-literal strings.

The schema gate is a lowering decision rather than a DCE decision — `ref.func`
initialisers baked into the canonical struct's `inspect` field would otherwise
keep the wrappers reachable and defeat post-emission DCE.

### Bare function references

A bare `&fn_name` lowers to a synthetic zero-capture closure (a `__Closure_N`
whose body forwards every parameter to `fn_name`) so that fn-typed slots accept
it uniformly with user-written closures. For inspect output, that synthetic body
is rendered as `&fn_name` rather than the lowering-internal forwarder text —
`:?` still produces the canonical signature string, and `:#?` the user-readable
expression.

## Consequences

Inspect output is derived, so every type has one without its author writing
code, and adding a field or a case changes the debug output with no separate
edit. The cost is code size: each inspected type instantiates a blanket or a
synthesised impl, and a deep struct hierarchy instantiates one per type it
reaches. DCE and the two closure gates keep what is never printed out of the
binary.

A closure's inspectability reaches the ABI. For each `(N, Ret)` whose
`Fn^Inspect` is referenced anywhere in the program, the canonical closure struct
grows from the slim `{ env, func }` to `{ env, inspect, func }` — the
env-and-vtable prefix shared with `$canonical_inspectable_base`, typed `func`
slot last — costing one extra ref per canonical closure value, plus one small
wrapper function and the signature and source strings per literal.

### Known gaps

- [ ] Depth limit: a recursive type inspects until it runs out of stack.
      Nothing caps nesting depth the way `DEFAULT_SEQ_LIMIT` caps length.

## References

- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [WEP: Format Traits](./wep-2026-02-01-format-traits.md)
- [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)
- [WEP: Closure Implementation](./wep-2026-01-16-closure-implementation.md)
- [Elixir Inspect Protocol](https://hexdocs.pm/elixir/Inspect.html)
- [Rust Debug trait](https://doc.rust-lang.org/std/fmt/trait.Debug.html)
