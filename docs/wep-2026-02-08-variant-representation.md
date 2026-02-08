# WEP: Variant Wasm GC Representation

## Context

Wado's variants compile to Wasm GC types. The compiler must decide how each variant is laid out at the Wasm level. This decision should be driven by the **shape** of the variant (number of cases, payload types), not by the **name** of the variant. `Option` and `Result` follow the same rules as any user-defined variant.

### Wasm GC Primitives

Wasm GC provides two key mechanisms relevant to variant representation:

- **Nullable references**: `(ref null $T)` can be either a valid reference or `ref.null`. Testing is a single `ref.is_null` instruction.
- **Struct subtyping**: A struct type can extend another. `ref.test` checks whether a reference is an instance of a subtype. `ref.cast` narrows.

These map to two natural representations for variants.

## Decision

### Two Representations

Every variant type is assigned one of two Wasm GC representations:

#### NullableRef

The variant is represented as a single nullable reference. One case maps to null, the other to a non-null value.

```wat
;; Option<String> → (ref null $String)
;; Some(s) → non-null ref to s
;; None    → ref.null

;; Option<i32> → (ref null $box_i32)
;; Some(n) → struct.new $box_i32 n
;; None    → ref.null
```

Operations:

| Operation              | Wasm Instructions                        |
| ---------------------- | ---------------------------------------- |
| Construct payload case | value (box if primitive)                 |
| Construct unit case    | `ref.null`                               |
| Test payload case      | `ref.is_null` + `i32.eqz`                |
| Test unit case         | `ref.is_null`                            |
| Extract payload        | `ref.as_non_null` (+ unbox if primitive) |

#### SubtypeHierarchy

The variant is represented as a base struct with a tag field, extended by per-case subtypes that add payload fields.

```wat
;; Result<i32, String>
(type $Result<i32,String>        (struct (field $tag i32)))
(type $Result<i32,String>::Ok    (sub $Result<i32,String> (struct (field $tag i32) (field $payload i32))))
(type $Result<i32,String>::Err   (sub $Result<i32,String> (struct (field $tag i32) (field $payload (ref $String)))))
```

Operations:

| Operation                | Wasm Instructions                                   |
| ------------------------ | --------------------------------------------------- |
| Construct case           | `i32.const <tag>`, push payload, `struct.new $Case` |
| Test case (with payload) | `ref.test $Case`                                    |
| Test case (unit)         | `struct.get $tag` + `i32.eq`                        |
| Extract payload          | `ref.cast $Case` + `struct.get $payload`            |
| Get tag                  | `struct.get $tag`                                   |

### Selection Rules

The representation is determined by the variant's structure, not its name. The rules are applied in order:

1. **NullableRef** if ALL of the following hold:
   - Exactly 2 cases
   - Exactly 1 unit case (no payload)
   - The other case has a payload whose Wasm type is a non-nullable reference, either:
     - A reference type (struct, array, string) directly, OR
     - A primitive type via boxing (`$box_i32`, `$box_i64`, `$box_f32`, `$box_f64`)
   - The payload type is not itself nullable (not another NullableRef variant)

2. **SubtypeHierarchy** otherwise.

#### Examples

| Variant                                           | Representation   | Reason                                      |
| ------------------------------------------------- | ---------------- | ------------------------------------------- |
| `Option<String>`                                  | NullableRef      | 2 cases, 1 unit, payload is ref             |
| `Option<i32>`                                     | NullableRef      | 2 cases, 1 unit, payload is boxed primitive |
| `Option<MyStruct>`                                | NullableRef      | 2 cases, 1 unit, payload is ref             |
| `Option<Option<i32>>`                             | SubtypeHierarchy | payload is itself nullable                  |
| `Result<i32, String>`                             | SubtypeHierarchy | 2 payload cases, no unit case               |
| `variant Shape { Circle(f64), Point }`            | NullableRef      | 2 cases, 1 unit, payload is boxed f64       |
| `variant Shape { Circle(f64), Rect(f64), Point }` | SubtypeHierarchy | 3 cases                                     |
| `variant Token { Eof }`                           | SubtypeHierarchy | 1 case (not exactly 2)                      |
| `variant Maybe<T> { Just(T), Nothing }`           | NullableRef      | same shape as Option                        |

The last example is important: a user-defined `Maybe<T>` gets the same optimized representation as `Option<T>` because the rules are structural, not name-based.

### Boxing

Wasm GC value types (i32, i64, f32, f64) cannot be null. NullableRef variants with primitive payloads use box structs:

```wat
(type $box_i32 (struct (field i32)))    ;; wraps i32, i16, i8, u32, u16, u8, bool, char
(type $box_i64 (struct (field i64)))    ;; wraps i64, u64
(type $box_f32 (struct (field f32)))    ;; wraps f32
(type $box_f64 (struct (field f64)))    ;; wraps f64
```

Box types are shared across all variants. `Option<i32>` and `variant Maybe { Just(i32), Nothing }` use the same `$box_i32`.

### Representation in TIR

The representation is computed in the lower phase and encoded in the TIR nodes emitted. Codegen does not re-derive the representation.

For NullableRef variants, the lower phase emits:

- `NullableRefWrap { value }` — construct the payload case (box if needed)
- `Null` — construct the unit case
- `IsNotNull { expr }` — test for payload case
- `NullableRefUnwrap { expr, inner_type }` — extract payload (unbox if needed)

For SubtypeHierarchy variants, the lower phase emits:

- `VariantConstruct { variant_type, case_index, case_name, payload }` — construct any case
- `VariantTest { expr, case_index, case_name }` — test for a specific case
- `VariantPayload { expr, case_index, payload_type }` — extract payload
- `VariantTag { expr }` — get discriminant

Codegen handles each node kind independently. It never inspects the variant name to choose behavior.

### Option and Result in the Type System

`Option<T>` and `Result<T, E>` are regular generic variants defined in `core:prelude`:

```wado
pub variant Option<T> {
    Some(T),
    None,
}

pub variant Result<T, E> {
    Ok(T),
    Err(E),
}
```

They have no dedicated type constructors. In the type system, `Option<i32>` is a `GenericInstance` of the `Option` variant, identical in representation to any other monomorphized generic variant.

The `null` literal coerces to the unit case of any NullableRef variant in the appropriate type context. This is a property of the representation, not of the name `Option`:

```wado
variant Maybe<T> { Just(T), Nothing }

let a: Option<i32> = null;  // coerces to Option::None
let b: Maybe<i32> = null;   // coerces to Maybe::Nothing
```

## Consequences

### Benefits

- No type-name-based special casing in the compiler. Custom variants with the same shape as `Option` get the same optimized representation.
- Codegen is mechanical: it reads TIR nodes and emits corresponding Wasm instructions without analyzing variant structure.
- The representation rules are simple and predictable. A user can reason about the Wasm representation of their variant by looking at its shape.

### Trade-offs

- NullableRef is limited to 2-case variants with exactly 1 unit case. A variant like `variant Tri { A(i32), B, C }` uses SubtypeHierarchy even though one might imagine a more compact encoding. This is acceptable: simplicity of rules outweighs marginal optimization.
- `Option<Option<T>>` falls back to SubtypeHierarchy. This is necessary to distinguish `Some(None)` from `None`, and matches Rust's behavior where `Option<Option<NonNull<T>>>` loses the niche optimization.
- Boxing for NullableRef primitive payloads adds one allocation. This is the same cost as the current implementation and is unavoidable given Wasm GC's type system.
