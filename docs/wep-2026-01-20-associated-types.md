# WEP: Associated Types in Traits

This WEP defines associated types for Wado's trait system.

## Context

Traits need to declare type members that implementations can bind to concrete types. This is essential for:

- Generic collection traits (e.g., `Index` with `type Output`)
- Iterator traits (e.g., `Iterator` with `type Item`)
- Type families where the associated type depends on the implementing type

## Decision

Support associated type declarations in traits and bindings in impl blocks.

### Syntax

```wado
// Declaration in trait
trait Container {
    type Item;

    fn get(&self, index: i32) -> &Self::Item;
}

// Binding in impl block
impl Container for IntArray {
    type Item = i32;

    fn get(&self, index: i32) -> &Self::Item {
        return &self.data[index];
    }
}
```

### Conformance

An associated type declares no default, so an impl binds every one its trait
declares, and binds no name the trait does not. An unbound type leaves the
projection with nothing to resolve to, so it reaches code generation
unsubstituted. A name the trait never declared is a typo for one it did, which
is how the real type came to be unbound.

A derivation request (`impl Trait for Type;`) writes no members at all, so it
owes none. A supertrait's associated type belongs to the impl answering
`T: Super`, not to the subtrait's: `impl Ord for T` neither owes nor may bind
one `Eq` declares ([Super Traits](./wep-2026-07-27-super-traits.md)).

### Resolution

Associated types are resolved via `Self::TypeName` syntax:

1. Inside a trait method signature, `Self::Item` refers to the abstract associated type
2. Inside an impl block, `Self::Item` resolves to the concrete bound type
3. Resolution happens during type checking, not parsing

### AST Representation

```rust
/// Associated type declaration in a trait: `type Output;`
pub struct AssociatedTypeDecl {
    pub name: String,
    pub span: Span,
}

/// Associated type binding in an impl block: `type Output = T;`
pub struct AssociatedTypeBinding {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}
```

### Elaborator Changes

The elaborator tracks current associated type bindings:

```rust
struct Elaborator {
    // ... other fields
    current_associated_type_bindings: HashMap<String, TypeId>,
}
```

When resolving `Self::TypeName`:

1. Look up `TypeName` in `current_associated_type_bindings`
2. If found, return the bound type
3. If not found, report an error

### Bounds

An associated type carries trait bounds, enforced against each impl's binding:

```wado
trait Container {
    type Item: Display;
}
```

A binding that does not satisfy them is rejected. A still-parametric binding is
left to the instantiation that settles it.

A caller reaching `Self::Item` through `T: Container` has no impl to read, so
the bound is all it may rely on. `FromStr::Err` and `TryFrom::Err` are both
`: Error` for that reason.

## Consequences

### Advantages

1. **Type-safe generic traits**: Traits can abstract over element types
2. **Self-documenting**: Associated types make trait contracts clear
3. **Familiar syntax**: Similar to Rust's associated types

### Trade-offs

1. **Single binding**: Each impl can only bind one type per associated type name
2. **No defaults**: A trait cannot supply a fallback, so every impl binds all of them

### Implementation Status

- [x] Parser: `type Name;` in traits, `type Name = Type;` in impl blocks
- [x] AST: `AssociatedTypeDecl`, `AssociatedTypeBinding`
- [x] Elaborator: `Self::TypeName` resolution
- [x] Desugar: Pass-through of associated types
- [x] Unparse: Output associated types
- [x] Trait bounds on associated types
- [x] Impl conformance: every declared type bound, and no other
- [ ] Default associated types

## Related

- [Indexing Traits](./wep-2026-01-20-indexing-traits.md) - Primary use case for associated types
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) - General trait design
