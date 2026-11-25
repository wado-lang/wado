# WEP: Variant Wasm GC Representation

## Context

A variant compiles to Wasm GC types, and the compiler chooses the layout. Wasm
GC offers two mechanisms that fit:

- A nullable reference, `(ref null $T)`, is a valid reference or `ref.null`.
  One `ref.is_null` tells them apart.
- A struct type may extend another. `ref.test` asks whether a reference is an
  instance of a subtype, and `ref.cast` narrows it.

## Decision

A variant's shape chooses its representation, never its name. `Option` and
`Result` follow the rules a user's variant does, so a `Maybe<T>` shaped as
`Option<T>` is laid out as `Option<T>` is.

### SubtypeHierarchy

The general representation. A base struct holds the discriminant, and one
subtype per case adds that case's payload fields.

```wat
;; Result<i32, String>
(type $Result<i32,String>      (struct (field $discriminant i32)))
(type $Result<i32,String>::Ok  (sub $Result<i32,String> (struct (field $discriminant i32) (field $payload i32))))
(type $Result<i32,String>::Err (sub $Result<i32,String> (struct (field $discriminant i32) (field $payload (ref $String)))))
```

| Operation       | Wasm instructions                                            |
| --------------- | ------------------------------------------------------------ |
| Construct       | `i32.const <discriminant>`, the payload, `struct.new $Case`  |
| Test a case     | `ref.test $Case`, or `struct.get $discriminant` and `i32.eq` |
| Extract payload | `ref.cast $Case`, then `struct.get $payload`                 |
| Discriminant    | `struct.get $discriminant`                                   |

### NullableRef

The variant is the payload reference itself, and the unit case is null. No Wasm
type is emitted for the variant.

```wat
;; Option<String> → (ref null $String)
;; Some(s) → s
;; None    → ref.null none
```

| Operation           | Wasm instructions |
| ------------------- | ----------------- |
| Construct payload   | the value         |
| Construct unit case | `ref.null none`   |
| Test unit case      | `ref.is_null`     |
| Extract payload     | `ref.as_non_null` |

### Selection

A variant is a NullableRef when all of these hold, and a SubtypeHierarchy
otherwise:

- It has exactly two cases.
- One case has no payload.
- The other has one payload, whose Wasm type is a non-nullable reference: a
  struct, an array, a string, a SubtypeHierarchy variant, `i128` or `u128`.
- That payload is not itself a NullableRef variant.

The last rule keeps `Some(None)` apart from `None`: were the inner `None` null,
the two would be one value. Rust loses the niche on `Option<Option<NonNull<T>>>`
for the same reason.

| Variant                                                    | Representation   | Reason                           |
| ---------------------------------------------------------- | ---------------- | -------------------------------- |
| `Option<String>`                                           | NullableRef      | two cases, one unit, a reference |
| `Maybe<String>` of `variant Maybe<T> { Just(T), Nothing }` | NullableRef      | the shape of `Option<String>`    |
| `Option<i32>`                                              | SubtypeHierarchy | the payload is not a reference   |
| `Option<Option<String>>`                                   | SubtypeHierarchy | the payload is itself nullable   |
| `Result<i32, String>`                                      | SubtypeHierarchy | no unit case                     |
| `variant Shape { Circle(f64), Rect(f64), Point }`          | SubtypeHierarchy | three cases                      |
| `variant Token { Eof }`                                    | SubtypeHierarchy | one case                         |

A primitive payload is not boxed to reach the niche. A box would cost an
allocation per value, which is what a SubtypeHierarchy case costs already.

A variant of one payload case and two or more unit cases, such as
`variant Tri { A(String), B, C }`, is a SubtypeHierarchy too. A denser encoding
exists, and the two-case rule was preferred for being predictable from the
declaration.

### Where the representation is chosen

Every phase up to WIR build sees one form: `VariantConstruct`, `VariantTest`,
`VariantPayload` and `VariantTag`, with every variant a SubtypeHierarchy.
`wir_optimize/nullable_ref.rs` then finds the eligible variants and rewrites
their types and every instruction over them. It runs at every optimization
level, since it replaces a type and so cannot be applied to some values and not
others. Codegen never reads a variant's name.

The Component Model's GC mode wants a subtype hierarchy for every sum type, so
a NullableRef variant converts at that boundary
([Migration to GC in Components](./wep-2026-03-28-gc-in-components.md)).
