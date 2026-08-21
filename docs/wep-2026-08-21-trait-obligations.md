# WEP: Trait Obligations — One Solve, One Answer

How the compiler answers "does this type satisfy this bound, and what does that
give me". Completes [Declaration Identity](./wep-2026-08-12-declaration-identity.md)
for the trait layer.

## Context

Four questions are asked of a bound, and each has grown its own implementation:

| Question                               | Implementations                                                                                                                                                                      |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Does `T` satisfy `B`?                  | `type_implements_trait`, `find_trait_impl_for_subject`, `blanket_trait_impl_applies`, `primitive_satisfies_builtin_trait`, `check_impl_block_bounds`, `has_real_trait_impl_for_type` |
| What is `<T as B>::Assoc`?             | `trait_assoc_answers`, `frame_projection`, `make_frame_projection`, `resolve_assoc_type_of_trait`, `resolve_trait_assoc_type_of_instance`, `resolve_generic_assoc_type_mono`         |
| Which method does `t.m()` dispatch to? | `find_method_in_trait_bounds`, `trait_method_of`, `resolve_trait_method_for_op`                                                                                                      |
| Which trait is this?                   | `classify_on_bound_trait`, `is_display_trait`, `is_prelude_trait_decl`, `trait_spelling`                                                                                             |

They disagree, one pair at a time. A review round finds the pair; the pair is
merged; the next round finds another. Six rounds have not converged, and two of
the merges introduced defects worse than what they replaced.

The disagreements are not arbitrary. Every one is an implementation reading
something the other does not have:

- a **spelling** where the other has an identity, so a module shadowing `Ord`
  changes the answer;
- a **registry** where the other has an impl, so a generic impl and a concrete
  one answer differently;
- a **frame**, where the other resolved the same syntax somewhere else, so a
  supertrait's `Item = A` binds to the caller's `A`.

`type_implements_trait` already carries the first correction at the outer
boundary: "The trait is an identity and nothing else." The inner layers still
take `trait_name: &str` beside it.

## Decision

One obligation, solved once, and every later answer read off the solution.

```rust
/// A trait at the arguments it was instantiated at. `args` holds only what an
/// impl writes beyond the declared defaults, so a bound — which cannot write
/// arguments — is always the empty list.
struct TraitRef { decl: DefId, args: Vec<TypeId> }

/// `self_ty` must satisfy `trait_ref`, asked from `frame`.
struct Obligation { self_ty: TypeId, trait_ref: TraitRef, frame: FrameId }

/// What satisfied it. Carries enough to answer everything else.
enum Solution {
    Impl { header: ImplKey, subst: Substitution },
    Builtin(BuiltinKind),   // primitive arithmetic, `Eq`/`Ord` identity, a reference's inheritance
    Derived(DeriveKind),    // structural `Eq` / `Ord` / `Serialize` / reflection
}
```

`solve(&Obligation) -> Result<Solution, Unsatisfied>` is the only implementation.
The other three questions consume its output:

- `Solution::assoc(name)` — an `Impl` substitutes its own `type X = …`; a
  `Builtin` answers by its kind (`Add::Output` on a primitive is the primitive);
  a `Derived` answers from the derive.
- `Solution::method(name)` — the same three ways.
- "Which trait is this" is not a question. `TraitRef.decl` is an input, and a
  compiler item is recognised by a `DefId → CompilerItem` map, never by name in
  the asking scope.

### Why this ends the disagreements

A disagreement needs two implementations. Reading an answer off the solution
that satisfied the bound makes the second one impossible to write: there is
nothing to key independently, so nothing can be keyed differently.

The registries stay, as caches of `solve`. A cache that misses recomputes; it
cannot answer differently, because the recomputation is the same function.

### Frames

A bound is syntax, and syntax means what the frame that wrote it says. A
supertrait bound reached through `T: Derived` was written in `Derived`'s frame,
so `Item = A` there is `Derived`'s `A`, not the caller's. `Obligation.frame`
travels with the bound; nothing resolves a bound's right-hand side in a frame
that did not write it.

### Termination

Two associated types bounded through each other have no fixpoint
(`assoc_projection_recursive_bounds.wado`). `solve` carries the obligation stack
it is already working on; a repeat is not an answer, and the binding stays
abstract. The existing recursion guard on `(type, trait)` becomes one case of
this.

## Consequences

- The 30 entry points collapse to `solve` plus readers of `Solution`.
- A trait shadowing a prelude name cannot change what a primitive implements,
  and cannot capture an operator: `trait Scale { fn neg(&self) }` no longer
  answers `-a`.
- A generic impl and a concrete one answer associated types the same way,
  because both are `Solution::Impl`.
- Diagnostics improve: `Unsatisfied` carries why, so the reason chain stops
  being a second walk that re-derives it.
- The migration is mechanical per call site but wide. It lands in stages, each
  one deleting the implementation it replaces rather than adding beside it.

## Roadmap

- [x] Recognise a compiler item by `DefId`, and drop `trait_name: &str` from the
      satisfaction path.
- [x] `TraitRef` at every bound, replacing the `(DefId, Vec<TypeId>)` pairs
      threaded by hand.
- [ ] `solve` returning `Solution`; associated types and methods read off it.
- [ ] Frames on obligations.
- [ ] Delete the registries' independent keys, leaving them caches.
