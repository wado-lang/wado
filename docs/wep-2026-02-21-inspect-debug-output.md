# WEP: Inspect (Debug Output)

## Context

Wado's type stringification system (WEP: Type Stringification) specifies that `builtin::inspect()` converts any value to its debug string representation. Template strings use it for `{expr:?}` (always inspect) and as the fallback for `{expr}` when no `Display` trait is implemented.

This WEP defines the output format, the user-facing name, and the compiler implementation strategy.

### Design Goals

1. **Wado-syntax output**: Inspect output should look like Wado source code — familiar, copy-pasteable
2. **No new TIR node**: Reuse existing TIR infrastructure; the compiler synthesizes inspect bodies from type metadata
3. **Works for all types**: Every type is inspectable without requiring trait implementations
4. **Formatter integration**: Writes to `&mut Formatter` like all format traits, supporting width/alignment/alternate flag

## Decision

### Name: `inspect`

The feature is called **inspect** throughout:

- `builtin::inspect(expr, &mut f)` — the compiler marker in the elaborator
- `{expr:?}` — template string syntax (inspect specifier)
- `{expr:#?}` — alternate (pretty-print) inspect with indented multi-line output

### Output Format by Type

Inspect output follows Wado literal syntax where possible:

| Type                       | Output                                 | Example                       |
| -------------------------- | -------------------------------------- | ----------------------------- |
| `i32`, `i64`, etc.         | Decimal number                         | `42`                          |
| `u8`, `u16`, etc.          | Decimal number                         | `255`                         |
| `f32`, `f64`               | Float number                           | `3.14`                        |
| `bool`                     | `true` / `false`                       | `true`                        |
| `char`                     | Quoted character                       | `'A'`                         |
| `String`                   | Escaped, quoted string                 | `"hello\"world"`              |
| `()` (unit)                | `()`                                   | `()`                          |
| Struct                     | `Name { field: value, ... }`           | `Point { x: 10, y: 20 }`      |
| Struct (generic)           | `Name { field: value }` (no type args) | `Box { value: 42 }`           |
| Struct (`#[hidden]` field) | Field omitted, `..` appended           | `Foo { visible: 1, .. }`      |
| Tuple                      | `[elem, ...]`                          | `[1, "a", true]`              |
| `List<T>`                  | `[elem, ...]`                          | `[1, 2, 3]`                   |
| `TreeMap<K, V>`            | `{key: value, ...}`                    | `{"a": 1, "b": 2}`            |
| `TreeSet<T>`               | `{elem, ...}`                          | `{10, 20, 30}`                |
| `Value` (json\_value)      | JSON-like representation               | `{"key": "val"}`              |
| `Option::Some(v)`          | `Some(inspect(v))`                     | `Some(42)`                    |
| `Option::None` / `null`    | `null`                                 | `null`                        |
| Enum                       | `TypeName::CaseName`                   | `Color::Red`                  |
| Variant (no payload)       | `TypeName::CaseName`                   | `Shape::Point`                |
| Variant (with payload)     | `TypeName::CaseName(inspect(payload))` | `Shape::Circle(5.0)`          |
| Flags                      | `TypeName::MemberName \| ...`          | `Perms::Read \| Perms::Write` |
| Flags (none)               | `TypeName::none()`                     | `Perms::none()`               |
| Newtype                    | `value as TypeName`                    | `1.5 as Meters`               |
| Resource (opaque handle)   | `TypeName#0xHH`                        | `Fields#0x01`                 |
| `&T`                       | `&inspect(inner)`                      | `&42`                         |
| `&mut T`                   | `&mut inspect(inner)`                  | `&mut Point { x: 1, y: 2 }`   |
| Closure (default)          | Signature only                         | `\|x: i32\| -> i32`           |
| Closure (`#` alternate)    | TIR unparsed source                    | `\|x: i32\| x + 1`            |

#### Detailed Format Rules

**Struct fields**: Iterate fields in declaration order. Skip fields annotated with `#[hidden]`, but append `..` to indicate their presence. If all fields are hidden, output `Name { .. }`. Recursively inspect each visible field value.

**Enum/Variant/Flags type name**: Always include the type name prefix (`Color::Red`, not just `Red`). This is unambiguous and matches construction syntax.

**Newtype**: Inspect the inner value using the base type's inspect, then append `as TypeName`. This mirrors Wado's cast syntax: `1.5 as Meters`.

**Resource**: Resources are opaque handles. Display as `TypeName#0xHH` where `HH` is the handle value (i32) in lowercase hex. This makes it clear the value is an opaque handle, not a constructible value.

**Flags**: Decompose the bitmask into individual set members joined by `|`. If no bits are set, output `TypeName::none()`. This matches the construction syntax.

**References**: Prefix with `&` or `&mut`, then inspect the referent. Since Wado uses GC-managed references, dereferencing is always safe.

**Closures**: By default, show only the parameter types and return type (the signature). With the `#` alternate flag (`{closure:#?}`), show the TIR-unparsed source code of the closure body. The signature-only default avoids potentially large output for complex closures.

**Option special handling**: `Option::Some(v)` renders as `Some(v)` (short form), `Option::None` renders as `null` (matching Wado's null literal).

### Implementation Strategy

#### No New TIR Node

Inspect is implemented without adding a new TIR expression kind. Instead:

1. **Elaborator phase**: When the elaborator encounters `{expr:?}` or falls back to inspect for `{expr}`, it emits a `StaticCall` to `builtin::inspect(expr, &mut f)`. This acts as a **marker** — the function doesn't exist as real code.

2. **Synthesize phase** (`synthesize_inspect`): A new pass runs after TIR resolution and before CM binding synthesis. It scans the TIR for `builtin::inspect` calls and replaces each one with synthesized TIR that writes the formatted output to the Formatter. The synthesized code uses the same TIR nodes as hand-written Wado code (method calls, match expressions, struct field access, etc.).

3. **Normal pipeline**: The synthesized TIR flows through the rest of the pipeline (CM binding → monomorphize → lower → optimize → codegen) like any other code.

#### Pipeline Position

```
parse → desugar → modules → symbols → tir (resolve)
                                          ↓
                                     effect_check
                                          ↓
                                     synthesize_inspect  ← NEW
                                          ↓
                                     cm_binding_gen
                                          ↓
                          monomorphize → lower → optimize → wasm_plan → codegen
```

The phase runs **after effect checking** and **before CM binding synthesis** because:

- It needs resolved type information (available after TIR)
- It generates closures and match expressions that must go through lowering
- It must run before CM bindings because adapter-generated code should not contain inspect markers
- Closures generated by inspect synthesis need to go through the normal closure lowering pipeline

#### Synthesis Algorithm

For a `builtin::inspect(expr, &mut f)` call where `expr` has type `T`:

```
fn synthesize_inspect_for_type(T, expr, f_ref) -> Vec<TirStmt>:
    match T:
        Primitive(i32/i64/u8/..) →
            // Reuse Display::fmt — primitives inspect the same as display
            expr.fmt(f_ref)

        Primitive(bool) →
            expr.fmt(f_ref)  // "true" / "false"

        Primitive(char) →
            // Wrap in single quotes: 'A'
            f.write_char('\'')
            expr.fmt(f_ref)
            f.write_char('\'')

        String →
            // Wrap in escaped quotes: "hello\"world"
            f.write_char('"')
            // write escaped string content
            f.write_char('"')

        Struct { name, fields } →
            f.write_str("Name { ")
            for each (i, field) in fields (skip #[hidden]):
                if i > 0: f.write_str(", ")
                f.write_str("field_name: ")
                synthesize_inspect_for_type(field.type, expr.field, f_ref)
            if has_hidden_fields: f.write_str(", ..")
            f.write_str(" }")
            // All fields hidden → "Name { .. }"

        Tuple(element_types) →
            f.write_char('[')
            for each (i, elem_type) in element_types:
                if i > 0: f.write_str(", ")
                synthesize_inspect_for_type(elem_type, expr.i, f_ref)
            f.write_char(']')

        List(elem_type) →
            // Generate a loop that iterates and inspects each element
            f.write_char('[')
            for-of loop with index tracking, comma separation
            f.write_char(']')

        Option(inner_type) →
            match expr:
                Some(v) → f.write_str("Some("); inspect(v, f); f.write_char(')')
                None → f.write_str("null")

        Enum { name, cases } →
            match expr:
                case_i → f.write_str("EnumName::CaseName")

        Variant { name, cases } →
            match expr:
                CaseName(payload) → f.write_str("Name::CaseName("); inspect(payload, f); f.write_char(')')
                CaseName → f.write_str("Name::CaseName")

        Flags { name, members } →
            // Test each bit, collect set member names joined by " | "
            if expr == 0: f.write_str("Name::none()")
            else: join with " | ": "Name::Member1 | Name::Member2"

        Newtype { name, base_type } →
            synthesize_inspect_for_type(base_type, expr as base_type, f_ref)
            f.write_str(" as Name")

        Resource { name } →
            f.write_str("Name#0x")
            // format handle as hex (the handle is i32)
            (expr as i32).fmt_hex(f_ref)

        Ref(inner) →
            f.write_char('&')
            synthesize_inspect_for_type(inner, *expr, f_ref)

        MutRef(inner) →
            f.write_str("&mut ")
            synthesize_inspect_for_type(inner, *expr, f_ref)

        Closure { params, return_type } →
            // Dispatched via the canonical closure vtable (see "Closure Inspect via
            // Runtime Dispatch" below). The per-literal __Closure_N^Inspect /
            // __Closure_N^InspectAlt impls write the signature / source string.
            self.vtable.inspect    (env, f) // {x:?}
            self.vtable.inspect_alt(env, f) // {x:#?}
```

#### Elaborator Changes

In `resolve_template_string`, when `trait_name` would be `"Display"` but the type has no Display impl (the current fallback path), or when the format spec is `?`:

```rust
// In resolve_template_string, after determining trait_name:
if is_inspect_specifier || (trait_name == "Display" && !has_display_impl) {
    // Emit: builtin::inspect(resolved_expr, &mut __f)
    let inspect_call = TirExpr::new(
        TirExprKind::StaticCall {
            func: FunctionRef::Builtin("inspect".to_string()),
            args: vec![resolved, fmt_mut_ref],
        },
        TypeTable::UNIT,
        span,
    );
    stmts.push(TirStmt::new(TirStmtKind::Expr(inspect_call), span));
}
```

The `FunctionRef::Builtin("inspect")` variant acts as the marker. The `synthesize_inspect` pass recognizes this and replaces the entire `StaticCall` with the synthesized TIR.

#### Closure Inspect via Runtime Dispatch

`Inspect` / `InspectAlt` on a closure value must produce per-literal output (for `:#?`, the closure's own source) regardless of how the value reaches the call site — directly through a local, or indirectly through a function parameter, struct field, or global. Indirect dispatch rules out a pure compile-time substitution: the per-literal information must travel with the value.

Wado closures lower to two complementary representations (see WEP: Closure Implementation):

1. Specialised: the local has type `&__Closure_N` (the per-literal functor struct). Used when every reference to the local is in callee position.
2. Canonical: the value is wrapped in `CanonicalClosure_K` so any holder of an `Fn<N, Ret>` value can invoke or inspect it. The lowering escape analysis demotes a local to canonical as soon as it appears in any non-callee position (struct field, fn argument, return value, global assignment, or rebinding).

The canonical struct carries the runtime vtable for inspectable signatures. To make a single dispatch stub serve every parameter shape with the same `(N, Ret)`, all inspectable canonical structs share a Wasm GC supertype:

```wat
(type $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))
  (field $inspect_alt (ref $canonical_callback_fn))))

(type $CanonicalClosure_K (sub $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))
  (field $inspect_alt (ref $canonical_callback_fn))
  (field $func        (ref $canonical_fn_K)))))
```

`$canonical_callback_fn = (env: structref, f: structref) -> ()` is uniform across signatures. The supertype prefix means `ref.cast self to $canonical_inspectable_base` succeeds for any inspectable closure value, regardless of `K` — so two distinct function types like `fn(i32) -> i32` and `fn(String) -> i32` (same `(arity, return_type)`, different parameter types) reach the same dispatch stub without per-signature tables.

Per-literal artifacts (synthesised at lower time):

1. `__Closure_N` struct (existing) — holds captures.
2. `__call` method (existing) — closure body.
3. `__Closure_N^Inspect::inspect(&self, &mut Formatter)` — writes the signature, e.g. `|i32, i32| -> i32`.
4. `__Closure_N^InspectAlt::inspect_alt(&self, &mut Formatter)` — writes the TIR-unparsed source, e.g. `|x: i32| (x + 1)` for non-capturing closures or `|x: i32| (x + n)` for capturing closures (captured bindings appear as free variables in the body; their values may be rendered alongside if a future inspect mode supports it).

Per-literal canonical-path wrappers (registered in WIR build for inspectable signatures only):

1. `__closure_wrapper_N` — casts env, calls `__call`.
2. `__closure_inspect_wrapper_N` — casts env, calls `__Closure_N^Inspect::inspect`.
3. `__closure_inspect_alt_wrapper_N` — casts env, calls `__Closure_N^InspectAlt::inspect_alt`.

Dispatch stubs (one pair per inspectable `(N, Ret)`):

- `Fn<N, Ret>^Inspect::inspect(&self, &mut Formatter)`: cast `self` to `$canonical_inspectable_base`, load `inspect`, `call_ref` with `(env, f)`. Same shape for `InspectAlt`.
- The stub is emitted as `FunctionKind::FnCanonicalDispatch` with a bodyless TIR placeholder; WIR build installs the instructions directly. Bodyless functions bypass the inliner and other TIR-body walkers, so no `inline(never)` workaround is needed.

The specialised path takes a redirect at lowering: `Fn<N, Ret>^Inspect[Alt]` calls on a known-local closure receiver rewrite to direct calls on `__Closure_N^Inspect[Alt]`. The dispatch stub and canonical vtable are bypassed entirely; standard DCE then removes the per-literal impls when no inspect call site survives.

#### Zero Overhead When Unused

Two whole-program gates keep programs that don't inspect closures from paying for the runtime-dispatch machinery:

1. Schema gate (per `(N, Ret)`): only signatures with a reachable `Fn<N, Ret>^Inspect` or `^InspectAlt` dispatch stub get the inspectable canonical layout. Other signatures use the slim `(struct env func)` shape with no shared supertype, no inspect/inspect\_alt fields, and no per-literal wrappers.
2. Per-functor gate (per `(N, Ret)`, per trait method): a pre-DCE scan classifies each `(arity, return_type)` as "inspected", "inspect-alt-ed", or both. The TIR DCE roots `__Closure_N^Inspect` from `ClosureToCanonical` only when the signature is "inspected", and `^InspectAlt` only when it is "inspect-alt-ed". A program that uses only `:?` drops every `__Closure_N^InspectAlt` impl and its per-literal source-string constant; the symmetric case applies to a `:#?`-only program.

The schema gate is a lowering decision rather than a DCE decision — `ref.func` initialisers baked into the canonical struct's `inspect` / `inspect_alt` fields would otherwise keep the wrappers reachable and defeat post-emission DCE.

#### Bare Function References

A bare `&fn_name` lowers to a synthetic zero-capture closure (a `__Closure_N` whose body forwards every parameter to `fn_name`) so that fn-typed slots accept it uniformly with user-written closures. For inspect output, that synthetic body is rendered as `&fn_name` rather than the lowering-internal forwarder text — `:?` still produces the canonical signature string, and `:#?` produces the user-readable expression.

### Interaction with Existing WEPs

| WEP                        | Interaction                                                    |
| -------------------------- | -------------------------------------------------------------- |
| Type Stringification       | Implements the `builtin::inspect` specified there              |
| Format Traits              | `:?` resolves to `builtin::inspect`, not a trait               |
| Template Format Specifiers | `{expr:?}` triggers inspect; `{expr:#?}` is the alternate flag |
| CM Binding Synthesis       | `synthesize_inspect` runs before CM bindings                   |

## Consequences

### Positive

1. **No TIR node proliferation**: Reuses existing `StaticCall` for the marker, no new expression kinds
2. **Full pipeline participation**: Synthesized inspect code goes through monomorphization, lowering, optimization — dead code elimination removes unused inspect paths
3. **Wado-native output**: Output mirrors Wado syntax, making it intuitive to read
4. **Type-complete**: Every type in the type system has a defined inspect representation
5. **Closure introspection**: Closures can show their source via `#` flag, leveraging TIR unparse

### Negative

1. **Code size**: Inspect synthesis generates code for every type that appears in `{:?}` — complex struct hierarchies produce substantial TIR
   - **Mitigation**: The optimizer and tree-shaking eliminate unused paths
2. **Synthesis complexity**: The `synthesize_inspect` pass must handle every `ResolvedType` variant
   - **Mitigation**: The synthesis is mechanical and type-driven, similar to CM binding synthesis
3. **String escaping**: Inspect of strings requires escape logic (quotes, backslashes, newlines)
   - **Mitigation**: Implement as a stdlib helper `internal::write_escaped_string(&String, &mut Formatter)`
4. **Closure ABI carries vtable slots when inspectable**: For each `(N, Ret)` whose `Fn^Inspect` / `Fn^InspectAlt` is referenced anywhere in the program, the canonical closure struct grows from the slim `{ env, func }` to the inspectable `{ env, inspect, inspect_alt, func }` (env-and-vtable prefix shared with the `$canonical_inspectable_base` supertype, typed `func` slot last), costing two extra refs per canonical closure value. Each affected literal also emits two small wrapper functions and one source-string constant.
   - **Mitigation**: A whole-program usage scan keeps the slim schema for `(N, Ret)` signatures whose `Fn^Inspect` / `Fn^InspectAlt` is unreachable, so production builds that don't print closures pay nothing.

### Future Extensions

1. **Depth limit**: Prevent infinite recursion on deeply nested or recursive types

### Implemented Extensions

1. **Pretty-print (`{:#?}`)**: Indented multi-line output via `InspectAlt` trait and `Formatter` indent tracking. Uses `open_brace`/`close_brace`/`write_newline_indent` on the `Formatter`. Example:

```wado
let arr: List<i32> = [1, 2, 3];
println(`{arr:#?}`);
// [
//   1,
//   2,
//   3,
// ]
```

2. **Custom inspect**: Types can override inspect behavior by implementing `Inspect` and/or `InspectAlt` traits. `TreeMap`, `TreeSet`, and `Value` (json\_value) provide custom implementations for cleaner output.

## Implementation Status

The core `synthesize_inspect` phase and the full pipeline integration are implemented. The following table tracks coverage by type:

| Type                       | Status | Notes                                                                    |
| -------------------------- | ------ | ------------------------------------------------------------------------ |
| `i32`, `i64`, etc.         | Done   | Delegates to `Display::fmt`                                              |
| `u8`, `u16`, etc.          | Done   | Delegates to `Display::fmt`                                              |
| `f32`, `f64`               | Done   | Includes special values (`inf`, `-inf`, `0.0`, `-0.0`)                   |
| `bool`                     | Done   | Delegates to `Display::fmt`                                              |
| `char`                     | Done   | Wrapped in single quotes                                                 |
| `String`                   | Done   | Escaped, quoted                                                          |
| `()` (unit)                | Done   | Outputs `()`                                                             |
| Struct                     | Done   | Fields in declaration order                                              |
| Struct (generic)           | Done   | Type args substituted for field types; type args omitted in name         |
| Struct (`#[hidden]` field) | Done   | Hidden fields skipped with `..` hint; `is_hidden` propagated through TIR |
| Tuple                      | Done   |                                                                          |
| `List<T>`                  | Done   | Loop-based with comma separation                                         |
| `Option::Some(v)`          | Done   | Renders as `Some(inspect(v))`                                            |
| `Option::None` / `null`    | Done   | Renders as `null`                                                        |
| Enum                       | Done   | `TypeName::CaseName`                                                     |
| Variant (no payload)       | Done   |                                                                          |
| Variant (with payload)     | Done   |                                                                          |
| Flags                      | Done   | Bitwise decomposition with `\|` join                                     |
| Newtype                    | Done   | `value as TypeName`                                                      |
| Resource (opaque handle)   | Done   | `TypeName#0xHH` using `LowerHex::fmt`                                    |
| `&T`                       | Done   | `&inspect(inner)`                                                        |
| `&mut T`                   | Done   | `&mut inspect(inner)`                                                    |
| Closure (default)          | Done   | Signature only; dispatched via canonical closure vtable                  |
| Closure (`#` alternate)    | Done   | TIR unparsed source; works for indirect calls (param/field/global)       |
| Display fallback           | Done   | `{expr}` falls back to inspect when no `Display` impl exists             |
| Nested structs/arrays      | Done   | Recursive inspect for composite fields                                   |
| `TreeMap<K, V>`            | Done   | Custom `Inspect`/`InspectAlt`: `{key: value, ...}` format                |
| `TreeSet<T>`               | Done   | Custom `Inspect`/`InspectAlt`: `{elem, ...}` format                      |
| `Value` (json\_value)      | Done   | Custom `Inspect`/`InspectAlt`: JSON-like format                          |
| Pretty-print (`:#?`)       | Done   | `InspectAlt` trait with `Formatter` indent tracking                      |
| `List<List<T>>`            | TODO   | Nested array codegen bug (tracked as TODO test)                          |

### Additional Fixes

- **Resource hex format**: Changed from decimal (`TypeName#N`) to hex (`TypeName#0xHH`) using `LowerHex::fmt` instead of `Display::fmt`, matching the WEP specification.
- **`#[hidden]` field support**: Added `is_hidden` flag to `TirField`, propagated through elaborator, monomorphizer, and lowerer. `synthesize_inspect` and `find_struct_fields` filter hidden fields.
- **Generic struct inspect**: Added `ResolvedType::GenericInstance` handling in `synth_body`, using `SubstitutionContext` to resolve type parameters to concrete types for field access.
- **Unit type `()` codegen fix**: Fixed invalid Wasm generation when `()` appears as a function parameter or local variable. Unit-type params are now filtered out during WIR function registration, and unit-type locals/assignments/reads emit `Nop` instead of invalid `local.get`/`local.set`. Unit-type arguments are also filtered from call sites.

## References

- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [WEP: Format Traits](./wep-2026-02-01-format-traits.md)
- [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [WEP: TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [Elixir Inspect Protocol](https://hexdocs.pm/elixir/Inspect.html)
- [Rust Debug trait](https://doc.rust-lang.org/std/fmt/trait.Debug.html)
