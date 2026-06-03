# Research: Rust Reference Semantics Edge Cases

This document catalogs Rust's reference-related behaviors and edge cases, with analysis of how each applies (or doesn't) to Wado's GC-based memory model.

## 1. References and Iteration (`for` loops)

### Rust behavior

`for x in expr` calls `IntoIterator::into_iter(expr)`. Three forms for `Vec<T>`:

```rust
// &Vec<T> → yields &T. vec is usable after loop.
for x in &vec { /* x: &i32 */ }

// &mut Vec<T> → yields &mut T. Can modify elements in-place.
for x in &mut vec { *x += 1; }

// Vec<T> → yields T (consuming). vec is moved, unusable after.
for x in vec { /* x: i32 */ }
```

Slices:

```rust
for x in &[1, 2, 3] { /* x: &i32 */ }  // &[i32; 3] coerces to &[i32]
for x in [1, 2, 3] { /* x: i32 */ }     // since Rust 2021, array IntoIterator yields T
```

`Option` and `Result` also implement `IntoIterator` (0-or-1 element iterator):

```rust
for x in &opt { /* x: &T, runs 0 or 1 times */ }
for x in opt { /* x: T, consumes opt */ }
```

### `iter()` vs `into_iter()` vs `iter_mut()`

```rust
v.iter()       // borrows, yields &T
v.iter_mut()   // mutably borrows, yields &mut T
v.into_iter()  // consumes, yields T
```

Pitfall: `(&v).into_iter()` yields `&T`, not `T` (calls `IntoIterator` on the reference type).

### Wado implications

Wado has no ownership/move semantics, so the "consuming" `into_iter()` concept doesn't apply. Currently `for-of` on `&List<T>` yields `T` by value (copy). Key decisions:

- **Should `for let x of &arr` yield `&T` (like Rust) or `T` (copy, current behavior)?**
  - With value semantics and GC, yielding `T` (copy) is simpler and consistent.
  - Yielding `&T` would be more Rust-like but adds complexity for little gain.
- **Should `for let x of &mut arr` yield `&mut T`?** Useful for in-place mutation of array elements.
- No need for `into_iter()` distinction. One `iter()` method suffices.

## 2. References and Option/Result

### `Option<&T>` vs `&Option<T>` in Rust

```rust
// Option<&T>: owned Option holding a reference. Niche-optimized (None = null pointer).
let opt_ref: Option<&i32> = Some(&x);  // same size as &i32

// &Option<T>: reference to an owned Option.
let ref_opt: &Option<i32> = &opt;      // non-null pointer to the Option
```

### `as_ref()` / `as_mut()` — converting between the two

```rust
let opt = Some(String::from("hello"));
let opt_ref: Option<&String> = opt.as_ref();   // &Option<T> → Option<&T>
opt.as_ref().map(|s| s.len());                 // borrow, don't consume

let mut opt = Some(String::from("hello"));
if let Some(s) = opt.as_mut() { s.push('!'); } // mutate in-place
```

Forgetting `as_ref()` before `.map()` consumes the Option (ownership pitfall).

`Option<&T>` has `.copied()` and `.cloned()`:

```rust
let opt: Option<&i32> = Some(&42);
let owned: Option<i32> = opt.copied();  // Option<&T> → Option<T> for Copy types
```

### Pattern matching on `&Option<T>`

```rust
let opt = Some(42);
if let Some(x) = &opt {
    // x: &i32 (match ergonomics gives a reference)
}

// Equivalent explicit form:
match &opt {
    &Some(ref x) => { /* x: &i32 */ }
    &None => {}
}
```

### Wado implications

Wado already optimizes `Option<&T>` to nullable ref (null = None). Key decisions:

- **`as_ref()` and `as_mut()`**: With value semantics, `opt.map(...)` doesn't "consume" opt (it copies). So `as_ref()` is less critical in Wado. But for `&Option<T>` → `Option<&T>` conversion, it's still useful.
- **Match ergonomics on `&Option<T>`**: Wado already peels references in pattern matching (`if let Some(x) = &opt` gives `x: &i32`). This is consistent with Rust.
- **`.copied()` / `.cloned()`**: Not needed in Wado since value semantics means `Option<&T>.map(|x| *x)` naturally copies.

## 3. `mut` and `let` Bindings with References

### The four combinations in Rust

```rust
let x = &val;           // immutable binding, immutable ref: can't rebind, can't mutate through
let mut x = &val;       // MUTABLE binding, immutable ref: CAN rebind (x = &other), can't mutate through
let x = &mut val;       // immutable binding, MUTABLE ref: can't rebind, CAN mutate through (*x = ...)
let mut x = &mut val;   // mutable binding, mutable ref: can rebind AND mutate through
```

The critical distinction: `mut` before the name = rebindable. `mut` after `&` = mutable reference.

### `ref` and `ref mut` in patterns

```rust
let ref x = val;         // equivalent to: let x = &val

let opt = Some(String::from("hello"));
match opt {
    Some(ref s) => { /* s: &String, borrows instead of moving */ }
    None => {}
}
// opt is still valid

match opt {
    Some(s) => { /* s: String, opt is partially moved */ }
    None => {}
}
```

With match ergonomics (Rust 2018+), `ref` is less necessary because `match &opt` automatically gives references.

### `mut` in destructuring patterns

```rust
if let Some(mut x) = opt {
    x += 1;  // x is a mutable local COPY (i32 is Copy)
    // opt is unchanged
}

let mut opt = Some(String::from("hello"));
if let Some(ref mut x) = opt {
    x.push('!');  // modifies the String INSIDE opt
}
```

### Wado implications

Wado already has:

- `let mut x = ...` for mutable bindings
- `&T` and `&mut T` for references
- `let mut { x, y } = p` for mutable destructuring

Key decisions:

- **`ref` in patterns**: Unnecessary in Wado. With value semantics, matching on an owned value copies it. `ref` exists in Rust to avoid moves; Wado has no moves.
- **`mut` in individual destructured fields**: Rust allows `let Some(mut x) = ...`. Wado currently uses `let mut { x, y } = ...` (all-or-nothing). Per-field `mut` is less critical without ownership.
- **`let mut x = &val` vs `let x = &mut val`**: This distinction is valid in Wado and should be maintained. But the need for `let mut x = &val` (rebinding a reference) is rare.

## 4. Auto-deref and Method Resolution

### Rust's `.` operator deref chain

```rust
let s = String::from("hello");
let r = &s;
let rr = &&s;
let rrr = &&&s;

// All work — auto-deref peels off & layers:
s.len();     // String → deref to str → str::len
r.len();     // &String → deref
rr.len();    // &&String → deref × 2
rrr.len();   // &&&String → deref × 3
```

### Deref coercion chains

```rust
fn takes_str(s: &str) {}
let s = String::from("hello");
takes_str(&s);  // &String → &str (via Deref trait)

fn takes_slice(s: &[i32]) {}
let v = vec![1, 2, 3];
takes_slice(&v);  // &Vec<i32> → &[i32]
```

### When auto-deref does NOT happen

```rust
// 1. Operators don't auto-deref the same way
let a = &5;
let b = &10;
// let c = a + b;  // ERROR: no impl for &i32 + &i32
let c = *a + *b;   // OK

// 2. Generic type inference — no coercion through generics
fn generic<T>(x: T) {}
generic(&s);  // T = &String, NOT &str

// 3. Method resolution priority: closer type wins
// If both MyWrapper and inner type have foo(), MyWrapper::foo wins.
```

### Wado implications

Wado already auto-derefs for method calls (`(&point).sum()` works). Key decisions:

- **Deref coercion chains**: Wado doesn't have `Deref` trait. `String` and `List` are primitive types, not wrappers. So `&String → &str` coercion isn't relevant.
- **Operators with references**: Currently `&i32 + &i32` — should this auto-deref? Rust says no. Wado should probably auto-deref for arithmetic operators on primitive references for ergonomics.
- **Multi-level deref (`&&T`, `&&&T`)**: Wado peels references for method calls. Should also work for comparison operators.

## 5. Reference Coercions

### `&mut T` → `&T` (implicit, one-way)

```rust
fn takes_ref(x: &i32) {}
let mut val = 42;
takes_ref(&mut val);  // &mut i32 → &i32 implicitly
// Reverse does NOT work: &T → &mut T is never allowed
```

### Double/triple references

```rust
let x = 42;
let r: &i32 = &x;
let rr: &&i32 = &r;
let rrr: &&&i32 = &rr;

let val: i32 = ***rrr;  // explicit deref needed for operators
// But comparison operators auto-deref:
assert_eq!(r, &42);     // compares i32 values, not addresses
```

### Deref coercions chain transitively

```rust
fn takes_ref(x: &i32) {}
let r: &&&i32 = &&&42;
takes_ref(r);  // OK: &&&i32 → &&i32 → &i32 (transitive deref coercion)
```

Deref coercions chain at coercion sites (function arguments, `let` bindings, struct field init, return expressions). But NOT in generic type parameter positions:

```rust
// Coercion works at call sites with known target types:
fn takes_str(s: &str) {}
takes_str(&String::from("hi"));  // OK: coercion site

// Does NOT work through generics:
fn identity<T>(x: T) -> T { x }
let r = identity(&String::from("hi"));  // r: &String, NOT &str

// Does NOT work inside generic containers:
let v: Vec<&str> = vec![];
// v.push(&String::from("hi"));  // ERROR: no coercion site for generic type arg
```

### Wado implications

- **`&mut T → &T` coercion**: Already implemented in Wado. Correct behavior.
- **`&&T` (double references)**: These arise naturally. Auto-deref for method calls handles them. For operators, explicit `*` should be required (consistent with Rust).
- **No `Deref` trait**: Wado's coercion is simpler — only `&mut → &` and auto-deref on method calls.

## 6. References in Struct Fields and Lifetimes

### Rust requires lifetime annotations

```rust
// Won't compile without lifetime:
// struct Holder { data: &i32 }  // ERROR

struct Holder<'a> {
    data: &'a i32,
}
```

### `&'static T` and temporary lifetime extension

```rust
let r = &String::from("hello");  // temporary extended to live as long as r
// But NOT in all positions — struct construction, vec!, etc. may not extend
```

### Wado implications

**None of this applies to Wado.** GC-based memory means:

- No lifetime annotations needed (Wado uses `stores[...]` for escape analysis instead)
- No temporary lifetime concerns — GC keeps values alive
- `&T` in struct fields just works

## 7. Reborrowing

### What is reborrowing?

Creates a new, shorter-lived `&mut T` from an existing one, without moving:

```rust
fn takes_mut(x: &mut i32) { *x += 1; }
let mut val = 42;
let r = &mut val;

// Function calls implicitly reborrow:
takes_mut(r);  // implicit: takes_mut(&mut *r)
takes_mut(r);  // r is still valid!

// But variable assignment MOVES:
let r2 = r;    // r is MOVED (not reborrowed)
// r is invalid
```

The traditional rule: `&mut T` to function parameter → reborrow. `&mut T` to `let` binding → move.

However, modern Rust (NLL) is more nuanced: `let r2: &mut i32 = r` actually reborrows when the target type is known to be `&mut T`. The original `r` is "frozen" while `r2` is alive but becomes usable again after `r2` is dropped:

```rust
let mut val = 42;
let r = &mut val;
let r2: &mut i32 = r;  // REBORROW (not move) in modern Rust
*r2 = 100;
// r2's borrow ends here (NLL)
*r = 200;  // r is usable again!
```

Reborrowing does NOT happen through generics:

```rust
fn generic<T>(x: T) -> T { x }
let r = &mut val;
let r2 = generic(r);  // MOVES r (T = &mut i32, no reborrow)
```

### Wado implications

**Reborrowing is unnecessary in Wado.** It exists in Rust because `&mut T` is not `Copy` (to ensure uniqueness). Wado has no borrow checker and allows multiple `&mut T`. References can be freely copied/assigned.

## 8. Match Ergonomics (RFC 2005)

### Binding modes

Three modes: **by-move**, **by-ref**, **by-ref-mut**. Default is by-move. When scrutinee is `&T`, all bindings switch to by-ref. `&mut T` → by-ref-mut. `&` on top of `&mut` degrades to by-ref.

```rust
// by-move (owned scrutinee):
match opt { Some(s) => { /* s: String, moved */ } ... }

// by-ref (&scrutinee):
match &opt { Some(s) => { /* s: &String */ } ... }

// by-ref-mut (&mut scrutinee):
match &mut opt { Some(s) => { /* s: &mut String */ } ... }
```

### Edge cases

```rust
// 1. Iterating over &Vec<Option<T>> — double reference peeling
let v = vec![Some(1), None, Some(3)];
for x in &v {  // x: &Option<i32>
    match x {
        Some(n) => { /* n: &i32, NOT i32! */ }
        None => {}
    }
}

// 2. `mut` in pattern with & scrutinee — mut applies to BINDING, not ref
if let Some(mut x) = &opt {
    // x: &i32. `mut` means x is rebindable, not that *x is mutable.
}

// 3. Nested references compound
match &Some(&42) {
    Some(y) => { /* y: &&i32 */ }
    None => {}
}

// 4. Copy types still get & in match ergonomics
match &Point { a: "hi", b: 42 } {
    Point { a, b } => { /* b: &i32, even though i32 is Copy */ }
}
```

### Wado implications

Wado already implements match ergonomics that peel references. Key decisions:

- **Should match ergonomics peel ALL reference layers or just one?** Rust peels one `&` layer per scrutinee reference. Wado should be consistent.
- **Copy types under match ergonomics**: In Rust, even Copy types get `&T` bindings when matching on `&`. Wado with value semantics could auto-copy instead, yielding `T` for primitives. This would diverge from Rust but be more ergonomic.
- **`mut` in patterns with `&` scrutinee**: The Rust behavior (mut applies to binding, not pointee) is confusing. Wado should either match Rust or disallow this combination.

## 9. Copy Semantics for References

### Rust

```rust
// &T is always Copy:
let r = &x;
let r2 = r;   // copy
let r3 = r;   // r still valid

// &mut T is NOT Copy (move semantics to ensure uniqueness):
let r = &mut x;
let r2 = r;    // MOVE
// r is invalid
```

### Wado implications

**Both `&T` and `&mut T` should be Copy-like in Wado.** Since Wado has no borrow checker and allows multiple mutable references, there's no uniqueness invariant to protect. This simplifies:

- Closures capturing `&mut T` don't need special handling
- No need for reborrowing
- `let r2 = r` where `r: &mut T` doesn't invalidate `r`

## 10. Closures and References

### Rust: `Fn` / `FnMut` / `FnOnce` hierarchy

```rust
// Fn: borrows immutably (&self). Can call many times.
let greet = || println!("{}", name);

// FnMut: borrows mutably (&mut self). Can call many times but not concurrently.
let mut inc = || { count += 1; };

// FnOnce: consumes captures (self). Can call only once.
let consume = || { drop(name); };

// Hierarchy: Fn: FnMut: FnOnce
```

### Capture modes

```rust
// Default: smallest possible borrow
let closure = || println!("{}", s);  // captures &s

// move: force by-value capture
let closure = move || println!("{}", s);  // s is moved in

// move with Copy types: copies, not moves
let x = 42;
let closure = move || x;  // x is still valid outside
```

Subtle: `move` closure capturing `&T` copies the reference (since `&T` is `Copy`):

```rust
let r = &x;
let closure = move || *r;  // r (the reference) is copied into closure
// r is still valid
```

### `&mut` uniqueness in closures

```rust
let mut val = 42;
let mut c = || { val += 1; };
// println!("{}", val);  // ERROR: val is mutably borrowed by c
c();
println!("{}", val);  // OK after last use of c (NLL)
```

### Wado implications

Wado's closure design is simpler than Rust's thanks to GC and the absence of a borrow checker. See [Closure Implementation](./wep-2026-01-16-closure-implementation.md) for the full spec; key differences:

- **`fn` / `fn mut` split but no `FnOnce`**: Wado has two closure type constructors with `fn <: fn mut` sub-typing, so the optimizer keeps mutation info on type-erased call paths. `FnOnce` is unmotivated under Wado's value semantics (calls never consume captures).
- **No user-visible `impl` / `dyn`**: closure types use a single bare form in every position. The compiler picks specialised vs canonical dispatch via escape analysis.
- **Auto-capture by reference**: per-binding `&T` or `&mut T` is inferred from body usage. No `move` keyword; intermediate locals snapshot values when needed.
- **No uniqueness conflicts**: multiple closures may capture the same `&mut T` reference value and writes through any of them are visible to the others (references are the only aliasing types in Wado). This is a deliberate design choice (no borrow checker).

## Summary: What Wado Should Adopt vs Skip

### Adopt (Rust-compatible)

| Feature                                   | Notes               |
| ----------------------------------------- | ------------------- |
| `&mut T → &T` coercion                    | Already implemented |
| Auto-deref for method calls               | Already implemented |
| Match ergonomics (peel `&` in patterns)   | Already implemented |
| `if let Some(x) = &opt` gives `x: &i32`   | Already implemented |
| `Option<&T>` nullable optimization        | Already implemented |
| `let mut x = &val` (rebindable reference) | Standard meaning    |

### Skip (Rust-specific, unnecessary with GC)

| Feature                       | Reason                                                            |
| ----------------------------- | ----------------------------------------------------------------- |
| Lifetime annotations (`'a`)   | GC manages memory; `stores[...]` handles escape analysis          |
| Reborrowing                   | `&mut T` is Copy-like in Wado; no uniqueness invariant            |
| `ref` / `ref mut` in patterns | No move semantics; pattern matching always copies values          |
| `FnOnce` (consume-only)       | No move semantics; calls never consume captures (see closure WEP) |
| `into_iter()` (consuming)     | No ownership transfer; one iteration mode suffices                |
| Temporary lifetime extension  | GC keeps values alive                                             |
| `&mut T` is non-Copy          | No borrow checker; both `&T` and `&mut T` are freely copyable     |

### Open Design Questions

1. **`for let x of &arr` — should `x` be `T` (copy) or `&T`?**
   - Current: yields `T` (copy). This is simpler but diverges from Rust.
   - Rust: `for x in &vec` yields `&T`.
   - Recommendation: Keep yielding `T` for value types, `&T` for reference types. Simpler mental model.

2. **Operators on references (`&i32 + &i32`)**
   - Rust: does NOT auto-deref for operators.
   - Wado: could auto-deref for ergonomics. Primitives behind references are common when iterating.
   - Recommendation: Auto-deref for arithmetic/comparison operators on primitive references.

3. **`as_ref()` / `as_mut()` for Option/Result**
   - Less critical in Wado (value semantics means `.map()` doesn't consume).
   - Still useful for `&Option<T> → Option<&T>` conversion.
   - Recommendation: Implement for completeness but it's lower priority.

4. **Match ergonomics for Copy types under `&`**
   - Rust: `match &point { Point { b, .. } => ...}` gives `b: &i32` even for Copy types.
   - Wado could yield `i32` (auto-copy) for better ergonomics.
   - Recommendation: Yield `T` (copy) for value types, `&T` for reference types in match ergonomics.

5. **`let mut x = &mut val` — both rebindable and mutable deref**
   - This is valid syntax and has clear semantics.
   - Rarely needed in practice but should work correctly.
