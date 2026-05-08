# WEP: Operator Overloading

## Context

Wado needs a design for operator overloading to support custom types with operators. This is particularly important for:

1. **SIMD types**: Vector types like `Vec3`, `Vec4`, `Mat4` need arithmetic operators
2. **Custom numeric types**: BigInt, Complex, Rational numbers
3. **Index access**: Custom containers need `[]` operator
4. **Display**: Custom string representation (already handled by Display trait)
5. **Equality**: Custom equality semantics (though field-by-field comparison usually suffices)

### Language Survey

Different languages take different approaches:

#### Rust: Trait-Based

Rust uses traits for operator overloading. Each operator has a corresponding trait (e.g., `Add`, `Sub`, `Mul`):

```rust
impl Add for Point {
    type Output = Point;
    fn add(self, other: Point) -> Point {
        Point { x: self.x + other.x, y: self.y + other.y }
    }
}
```

**Pros**:

- Explicit and discoverable
- Type-safe with associated types
- Integrates with trait system
- Generic code can bound on operator traits

**Cons**:

- Verbose for simple cases
- Need to import traits to use operators

#### C++: `operator` Keyword

C++ uses special `operator` function names with flexible syntax (member or friend):

```cpp
Point operator+(const Point& other) const {
    return Point{x + other.x, y + other.y};
}
```

**Pros**:

- Compact syntax
- Flexible (member vs. friend functions)

**Cons**:

- Complex lookup rules
- Can be member or non-member (confusing)
- No trait-like abstraction

#### Kotlin: `operator` Modifier

Kotlin uses the `operator` keyword modifier on predefined function names:

```kotlin
operator fun plus(other: Point): Point {
    return Point(x + other.x, y + other.y)
}
```

**Pros**:

- Clear opt-in with `operator` keyword
- Fixed function names prevent abuse

**Cons**:

- No trait abstraction for generic code
- Less discoverable than explicit traits

#### Swift: Protocol-Based

Swift uses protocols (similar to traits) with static functions:

```swift
extension Point: Equatable {
    static func == (lhs: Point, rhs: Point) -> Bool {
        return lhs.x == rhs.x && lhs.y == rhs.y
    }
}
```

**Pros**:

- Protocol-oriented
- Type-safe

**Cons**:

- Must be static functions (not methods)
- Syntax can be awkward

#### Zig: No Operator Overloading

Zig explicitly rejects operator overloading to avoid "hidden control flow":

```zig
// Zig - explicit function calls only
const sum = vec_add(a, b);  // Not: a + b
```

**Pros**:

- No surprises, all control flow explicit
- Simple implementation

**Cons**:

- Verbose for math-heavy code
- Poor ergonomics for SIMD/vector math
- Community repeatedly requests this feature

#### Python: Dunder Methods

Python uses special "dunder" (double underscore) methods:

```python
def __add__(self, other):
    return Point(self.x + other.x, self.y + other.y)
```

**Pros**:

- Simple and clear
- Well-established convention

**Cons**:

- No compile-time checking
- Not type-safe
- Verbose naming

### Design Goals

1. **Consistency**: Align with Wado's existing trait system
2. **Discoverability**: Clear which operators are overloaded
3. **Safety**: Prevent abuse and surprising behavior
4. **Ergonomics**: Good experience for math-heavy code (SIMD)
5. **Wasm GC compatibility**: Map to efficient Wasm code

## Implementation Status

**IMPORTANT**: The trait system itself is not yet implemented in Wado. This WEP documents the design for operator overloading that will be implemented once the trait system is in place.

The trait syntax and semantics will follow Rust's design closely (as documented in `wep-2026-01-13-struct-and-trait.md`). This WEP extends that design to specify which traits correspond to which operators.

## Decision

### 1. Use Trait-Based Operator Overloading (Rust Style)

Wado adopts Rust's trait-based approach for operator overloading. Each operator maps to a trait with a specific method name.

**Rationale**:

- Consistent with Wado's existing trait system (see `wep-2026-01-13-struct-and-trait.md`)
- Explicit and type-safe
- Enables generic programming with operator bounds
- Well-proven design
- Familiar to Rust developers

### 2. Overloadable Operators

Not all operators should be overloadable. Here's what Wado allows:

#### Arithmetic Operators (Essential for SIMD)

```wado
trait Add {
    type Output;
    fn add(self, rhs: Self) -> Self::Output;
}

trait Sub {
    type Output;
    fn sub(self, rhs: Self) -> Self::Output;
}

trait Mul {
    type Output;
    fn mul(self, rhs: Self) -> Self::Output;
}

trait Div {
    type Output;
    fn div(self, rhs: Self) -> Self::Output;
}

trait Rem {
    type Output;
    fn rem(self, rhs: Self) -> Self::Output;
}
```

**Usage**:

```wado
impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, other: Vec3) -> Vec3 {
        return Vec3 {
            x: self.x + other.x,
            y: self.y + other.y,
            z: self.z + other.z,
        };
    }
}

let v1 = Vec3 { x: 1.0, y: 2.0, z: 3.0 };
let v2 = Vec3 { x: 4.0, y: 5.0, z: 6.0 };
let v3 = v1 + v2;  // Desugars to: v1.add(v2)
```

**Note**: `self` consumes the value (ownership transfer). For types that should preserve the original, implement `Clone` and clone before the operation, or take `&self` if the trait signature allows it.

#### Negation (Unary Minus)

```wado
trait Neg {
    type Output;
    fn neg(self) -> Self::Output;
}
```

**Usage**:

```wado
impl Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        return Vec3 {
            x: -self.x,
            y: -self.y,
            z: -self.z,
        };
    }
}

let v = Vec3 { x: 1.0, y: 2.0, z: 3.0 };
let negated = -v;  // Desugars to: v.neg()
```

#### Bitwise Operators

```wado
trait BitAnd {
    type Output;
    fn bitand(self, rhs: Self) -> Self::Output;
}

trait BitOr {
    type Output;
    fn bitor(self, rhs: Self) -> Self::Output;
}

trait BitXor {
    type Output;
    fn bitxor(self, rhs: Self) -> Self::Output;
}

trait BitNot {
    type Output;
    fn bitnot(self) -> Self::Output;
}

trait Shl {
    type Output;
    fn shl(self, rhs: u32) -> Self::Output;
}

trait Shr {
    type Output;
    fn shr(self, rhs: u32) -> Self::Output;
}
```

**Use case**: Custom bit flags, SIMD masks, etc.

#### Compound Assignment Operators (Essential for Efficiency)

Compound assignment operators enable in-place mutation, avoiding temporary object creation. This is especially important for `String`, `Array`, and other types where cloning is expensive.

```wado
trait AddAssign<Rhs = Self> {
    fn add_assign(&mut self, rhs: Rhs);
}

trait SubAssign<Rhs = Self> {
    fn sub_assign(&mut self, rhs: Rhs);
}

trait MulAssign<Rhs = Self> {
    fn mul_assign(&mut self, rhs: Rhs);
}

trait DivAssign<Rhs = Self> {
    fn div_assign(&mut self, rhs: Rhs);
}

trait RemAssign<Rhs = Self> {
    fn rem_assign(&mut self, rhs: Rhs);
}

trait BitAndAssign<Rhs = Self> {
    fn bitand_assign(&mut self, rhs: Rhs);
}

trait BitOrAssign<Rhs = Self> {
    fn bitor_assign(&mut self, rhs: Rhs);
}

trait BitXorAssign<Rhs = Self> {
    fn bitxor_assign(&mut self, rhs: Rhs);
}

trait ShlAssign<Rhs = u32> {
    fn shl_assign(&mut self, rhs: Rhs);
}

trait ShrAssign<Rhs = u32> {
    fn shr_assign(&mut self, rhs: Rhs);
}
```

**Usage**:

```wado
impl AddAssign for Vec3 {
    fn add_assign(&mut self, other: Vec3) {
        self.x += other.x;
        self.y += other.y;
        self.z += other.z;
    }
}

let mut v1 = Vec3 { x: 1.0, y: 2.0, z: 3.0 };
let v2 = Vec3 { x: 4.0, y: 5.0, z: 6.0 };
v1 += v2;  // Desugars to: v1.add_assign(v2)
// v1 is now Vec3 { x: 5.0, y: 7.0, z: 9.0 }
```

**Design notes**:

- Takes `&mut self` for in-place mutation
- RHS is taken by value (more flexible than by reference)
- No return value (`Output` type) - always mutates in place
- Useful for `String` concatenation, `Array` extension, numeric types

**Example with String**:

```wado
impl AddAssign for String {
    fn add_assign(&mut self, other: String) {
        self.push_str(&other);  // Efficient in-place append
    }
}

let mut s = "Hello";
s += ", World!";  // No temporary String created
```

#### Index Access (Essential for Containers)

```wado
trait Index<Idx> {
    type Output;
    fn index(&self, index: Idx) -> &Self::Output;
}

trait IndexMut<Idx> {
    type Output;
    fn index_mut(&mut self, index: Idx) -> &mut Self::Output;
}
```

**Usage**:

```wado
struct Matrix {
    data: Array<f64>,
    rows: u32,
    cols: u32,
}

impl Index<[u32, u32]> for Matrix {
    type Output = f64;
    fn index(&self, idx: [u32, u32]) -> &f64 {
        let [row, col] = idx;
        return &self.data[(row * self.cols + col) as usize];
    }
}

impl IndexMut<[u32, u32]> for Matrix {
    type Output = f64;
    fn index_mut(&mut self, idx: [u32, u32]) -> &mut f64 {
        let [row, col] = idx;
        return &mut self.data[(row * self.cols + col) as usize];
    }
}

let mut m = Matrix::new(3, 3);
m[[0, 0]] = 1.0;        // Desugars to: *m.index_mut([0, 0]) = 1.0
let val = m[[0, 0]];    // Desugars to: *m.index([0, 0])
```

#### Equality and Comparison (Already Exist as Traits)

These traits already exist in Wado (see `wep-2026-01-13-struct-and-trait.md`):

```wado
trait Eq {
    fn eq(&self, other: &Self) -> bool;
}

variant Ordering {
    Less,
    Equal,
    Greater,
}

trait Ord {
    fn cmp(&self, other: &Self) -> Ordering;
}
```

**Design note**: Most types should use the default field-by-field equality and comparison. Only override when custom semantics are needed (e.g., case-insensitive string comparison, floating-point with epsilon).

#### Display and Debug (Already Exist as Traits)

These traits already exist:

```wado
trait Display {
    fn display(&self) -> String;
}

trait Debug {
    fn debug(&self) -> String;
}
```

**Usage**:

- `Display` is for user-facing output, used in template strings: `{value}`
- `Debug` is for developer-facing output, used in debug printing

### 3. Non-Overloadable Operators

The following operators are NOT overloadable:

#### Logical Operators: `&&`, `||`, `!`

**Reason**: These have short-circuit evaluation semantics that cannot be preserved with custom implementations. `!` is logical NOT, not overloadable (bitwise NOT `~` is via `BitNot` trait).

```wado
let result = a && b;  // ❌ Cannot overload
                       // Always short-circuits: if a is false, b is never evaluated
```

#### Assignment Operator: `=`

**Reason**: The basic assignment operator `=` has special semantics in Wado (value semantics, clone/move). It cannot be overloaded.

**Note**: Compound assignment operators (`+=`, `-=`, etc.) ARE overloadable via traits like `AddAssign` (see section above).

#### Range Operators: `..<`, `..=`

**Reason**: These construct `RangeExclusive` and `RangeInclusive` types, not user-overloadable.

#### Member Access: `.`, `::`

**Reason**: Core language syntax, not overloadable.

#### Function Call: `()`

**Reason**: Not yet designed. Callable objects may be added in the future via `Fn`/`FnMut`/`FnOnce` traits (similar to Rust), but this is out of scope for this WEP.

### 4. Operator Trait Naming Convention

Following Rust's convention:

| Operator | Trait          | Method          | Expression | Desugars to            |
| -------- | -------------- | --------------- | ---------- | ---------------------- |
| `+`      | `Add`          | `add`           | `a + b`    | `a.add(b)`             |
| `-`      | `Sub`          | `sub`           | `a - b`    | `a.sub(b)`             |
| `*`      | `Mul`          | `mul`           | `a * b`    | `a.mul(b)`             |
| `/`      | `Div`          | `div`           | `a / b`    | `a.div(b)`             |
| `%`      | `Rem`          | `rem`           | `a % b`    | `a.rem(b)`             |
| `-` (un) | `Neg`          | `neg`           | `-a`       | `a.neg()`              |
| `&`      | `BitAnd`       | `bitand`        | `a & b`    | `a.bitand(b)`          |
| `\|`     | `BitOr`        | `bitor`         | `a \| b`   | `a.bitor(b)`           |
| `^`      | `BitXor`       | `bitxor`        | `a ^ b`    | `a.bitxor(b)`          |
| `~`      | `BitNot`       | `bitnot`        | `~a`       | `a.bitnot()`           |
| `<<`     | `Shl`          | `shl`           | `a << b`   | `a.shl(b)`             |
| `>>`     | `Shr`          | `shr`           | `a >> b`   | `a.shr(b)`             |
| `[]`     | `Index`        | `index`         | `a[b]`     | `*a.index(b)`          |
| `[]` (m) | `IndexMut`     | `index_mut`     | `a[b] = c` | `*a.index_mut(b)`      |
| `==`     | `Eq`           | `eq`            | `a == b`   | `a.eq(&b)`             |
| `!=`     | `Eq`           | (negated `eq`)  | `a != b`   | `!a.eq(&b)`            |
| `<`      | `Ord`          | `cmp`           | `a < b`    | `a.cmp(&b) == Less`    |
| `<=`     | `Ord`          | `cmp`           | `a <= b`   | `a.cmp(&b) != Greater` |
| `>`      | `Ord`          | `cmp`           | `a > b`    | `a.cmp(&b) == Greater` |
| `>=`     | `Ord`          | `cmp`           | `a >= b`   | `a.cmp(&b) != Less`    |
| `+=`     | `AddAssign`    | `add_assign`    | `a += b`   | `a.add_assign(b)`      |
| `-=`     | `SubAssign`    | `sub_assign`    | `a -= b`   | `a.sub_assign(b)`      |
| `*=`     | `MulAssign`    | `mul_assign`    | `a *= b`   | `a.mul_assign(b)`      |
| `/=`     | `DivAssign`    | `div_assign`    | `a /= b`   | `a.div_assign(b)`      |
| `%=`     | `RemAssign`    | `rem_assign`    | `a %= b`   | `a.rem_assign(b)`      |
| `&=`     | `BitAndAssign` | `bitand_assign` | `a &= b`   | `a.bitand_assign(b)`   |
| `\|=`    | `BitOrAssign`  | `bitor_assign`  | `a \|= b`  | `a.bitor_assign(b)`    |
| `^=`     | `BitXorAssign` | `bitxor_assign` | `a ^= b`   | `a.bitxor_assign(b)`   |
| `<<=`    | `ShlAssign`    | `shl_assign`    | `a <<= b`  | `a.shl_assign(b)`      |
| `>>=`    | `ShrAssign`    | `shr_assign`    | `a >>= b`  | `a.shr_assign(b)`      |

**Note**: Method names use lowercase (e.g., `bitand`, not `bit_and`) for consistency with Rust.

### 5. Generic Programming with Operators

Traits enable generic functions with operator bounds:

```wado
// Generic add function
fn add_values<T: Add<Output = T>>(a: T, b: T) -> T {
    return a + b;
}

// Works with any type that implements Add
let sum_i32 = add_values(1, 2);              // i32
let sum_f64 = add_values(1.0, 2.0);          // f64
let sum_vec = add_values(v1, v2);            // Vec3

// Numeric trait bound (multiple operators)
fn dot_product<T>(a: Vec3<T>, b: Vec3<T>) -> T
where
    T: Mul<Output = T> + Add<Output = T> + Clone,
{
    return a.x.clone() * b.x.clone() +
           a.y.clone() * b.y.clone() +
           a.z * b.z;
}
```

### 6. Standard Library Implementations

Built-in types implement operator traits where appropriate:

```wado
// Primitives implement arithmetic
impl Add for i32 { ... }
impl Sub for i32 { ... }
// ... etc for i8, i16, i32, i64, u8, u16, u32, u64, f32, f64

// String concatenation (note: consider if this is desired)
impl Add for String {
    type Output = String;
    fn add(self, other: String) -> String {
        return self.concat(other);
    }
}

// Array indexing
impl<T> Index<usize> for Array<T> {
    type Output = T;
    fn index(&self, idx: usize) -> &T {
        // bounds checking
        return &self.data[idx];
    }
}

impl<T> IndexMut<usize> for Array<T> {
    type Output = T;
    fn index_mut(&mut self, idx: usize) -> &mut T {
        // bounds checking
        return &mut self.data[idx];
    }
}
```

### 7. Restrictions and Guidelines

#### Do Not Overload Operators with Surprising Behavior

Operator implementations should match intuitive expectations:

```wado
// ❌ BAD: Confusing behavior
impl Add for User {
    type Output = User;
    fn add(self, other: User) -> User {
        // Deletes both users from database?! 🤯
        return User::deleted();
    }
}

// ✅ GOOD: Use a named method for non-obvious operations
impl User {
    pub fn merge(self, other: User) -> User {
        // Clear intent
        return User::merged(self, other);
    }
}
```

**Guidelines**:

1. **Arithmetic operators** (`+`, `-`, `*`, `/`, `%`): Should behave like mathematical operations
2. **Comparison operators** (`<`, `>`, etc.): Should define a meaningful ordering
3. **Index operator** (`[]`): Should access elements, not perform unrelated operations
4. **When in doubt, use a named method**: If the operation is not obviously operator-like, use a method with a clear name

#### Operator Precedence Applies to Overloaded Operators

Overloaded operators follow the same precedence rules as built-in operators (see `wep-2026-01-11-operator-precedence.md`).

```wado
let result = a + b * c;  // Always: a + (b * c), even if + and * are overloaded
```

#### Associated Type Flexibility

Operators can return different types via `Output` associated type:

```wado
// Scalar multiplication returns a vector
impl Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, scalar: f64) -> Vec3 {
        return Vec3 {
            x: self.x * scalar,
            y: self.y * scalar,
            z: self.z * scalar,
        };
    }
}

let v = Vec3 { x: 1.0, y: 2.0, z: 3.0 };
let scaled = v * 2.0;  // Vec3
```

## Consequences

### Positive

1. **Consistency**: Aligns with Wado's trait system
2. **Type safety**: Associated types ensure type correctness
3. **Discoverability**: Clear which types support which operators via traits
4. **Generic programming**: Can write generic functions with operator bounds
5. **SIMD/math ergonomics**: Natural syntax for vector/matrix math
6. **Familiar**: Rust developers will recognize the pattern
7. **Explicit**: Trait imports make dependencies clear
8. **Wasm mapping**: Trait dispatch maps to Wasm vtables efficiently

### Negative

1. **Verbosity**: More verbose than C++ or Kotlin for simple cases
   - **Mitigation**: Most users consume operator-overloaded types, not implement them
2. **No compound assignment**: Cannot overload `+=`, `-=`, etc.
   - **Mitigation**: Compiler can optimize `a = a + b` to in-place operation
   - **Alternative**: Provide explicit `add_assign()` methods when needed
3. **Trait import required**: Need to import trait to use overloaded operators
   - **Mitigation**: Standard traits in prelude (Add, Sub, etc.)
4. **Different from C++/Python**: Developers from those languages need to learn traits
   - **Mitigation**: Traits are a core Wado concept (already learned for other features)

### Trade-offs

| Aspect              | Rust/Wado (Traits)         | C++/Kotlin (operator keyword) | Zig (No overloading) |
| ------------------- | -------------------------- | ----------------------------- | -------------------- |
| Discoverability     | ✅ High (via traits)       | ⚠️ Medium (special syntax)    | ✅ N/A               |
| Type safety         | ✅ High (associated types) | ⚠️ Medium                     | ✅ N/A               |
| Generic programming | ✅ Yes (trait bounds)      | ⚠️ Limited (templates)        | ❌ No                |
| Verbosity           | ⚠️ Verbose                 | ✅ Concise                    | ✅ Explicit calls    |
| Consistency         | ✅ Part of trait system    | ⚠️ Special case               | ✅ No special cases  |
| Ergonomics (math)   | ✅ Good                    | ✅ Good                       | ❌ Poor              |

## Examples

### Complete SIMD Vector Example

```wado
use {println} from "core:cli";

struct Vec3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vec3 {
    pub fn new(x: f64, y: f64, z: f64) -> Vec3 {
        return Vec3 { x, y, z };
    }

    pub fn dot(&self, other: &Vec3) -> f64 {
        return self.x * other.x + self.y * other.y + self.z * other.z;
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, other: Vec3) -> Vec3 {
        return Vec3 {
            x: self.x + other.x,
            y: self.y + other.y,
            z: self.z + other.z,
        };
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, other: Vec3) -> Vec3 {
        return Vec3 {
            x: self.x - other.x,
            y: self.y - other.y,
            z: self.z - other.z,
        };
    }
}

impl Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, scalar: f64) -> Vec3 {
        return Vec3 {
            x: self.x * scalar,
            y: self.y * scalar,
            z: self.z * scalar,
        };
    }
}

impl Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        return Vec3 {
            x: -self.x,
            y: -self.y,
            z: -self.z,
        };
    }
}

impl Display for Vec3 {
    fn display(&self) -> String {
        return `Vec3({self.x}, {self.y}, {self.z})`;
    }
}

fn run() with Stdout {
    let v1 = Vec3::new(1.0, 2.0, 3.0);
    let v2 = Vec3::new(4.0, 5.0, 6.0);

    let sum = v1 + v2;
    println(`v1 + v2 = {sum}`);  // Vec3(5.0, 7.0, 9.0)

    let diff = v1 - v2;
    println(`v1 - v2 = {diff}`);  // Vec3(-3.0, -3.0, -3.0)

    let scaled = v1 * 2.0;
    println(`v1 * 2.0 = {scaled}`);  // Vec3(2.0, 4.0, 6.0)

    let negated = -v1;
    println(`-v1 = {negated}`);  // Vec3(-1.0, -2.0, -3.0)

    let dot = v1.dot(&v2);
    println(`v1 · v2 = {dot}`);  // 32.0
}
```

### Custom Matrix with Index Operator

```wado
struct Matrix {
    data: Array<f64>,
    rows: u32,
    cols: u32,
}

impl Matrix {
    pub fn new(rows: u32, cols: u32) -> Matrix {
        let size = rows * cols;
        let mut data: Array<f64> = [];
        for let mut i = 0; i < size; i += 1 {
            data.push(0.0);
        }
        return Matrix { data, rows, cols };
    }
}

impl Index<[u32, u32]> for Matrix {
    type Output = f64;
    fn index(&self, idx: [u32, u32]) -> &f64 {
        let [row, col] = idx;
        assert row < self.rows, "row index out of bounds";
        assert col < self.cols, "col index out of bounds";
        let offset = (row * self.cols + col) as usize;
        return &self.data[offset];
    }
}

impl IndexMut<[u32, u32]> for Matrix {
    type Output = f64;
    fn index_mut(&mut self, idx: [u32, u32]) -> &mut f64 {
        let [row, col] = idx;
        assert row < self.rows, "row index out of bounds";
        assert col < self.cols, "col index out of bounds";
        let offset = (row * self.cols + col) as usize;
        return &mut self.data[offset];
    }
}

fn run() {
    let mut m = Matrix::new(3, 3);

    // Write using index operator
    m[[0, 0]] = 1.0;
    m[[1, 1]] = 5.0;
    m[[2, 2]] = 9.0;

    // Read using index operator
    let diagonal_sum = m[[0, 0]] + m[[1, 1]] + m[[2, 2]];
    assert diagonal_sum == 15.0;
}
```

### Generic Numeric Function

```wado
// Works with any numeric type that implements Add
fn sum_three<T: Add<Output = T>>(a: T, b: T, c: T) -> T {
    return a + b + c;
}

fn run() {
    let int_sum = sum_three(1, 2, 3);           // 6 (i32)
    let float_sum = sum_three(1.0, 2.0, 3.0);   // 6.0 (f64)

    let v1 = Vec3::new(1.0, 0.0, 0.0);
    let v2 = Vec3::new(0.0, 1.0, 0.0);
    let v3 = Vec3::new(0.0, 0.0, 1.0);
    let vec_sum = sum_three(v1, v2, v3);        // Vec3(1.0, 1.0, 1.0)
}
```

## Implementation Notes

### Compiler Desugaring

The compiler desugars operator expressions to trait method calls during the AST → TIR phase:

```wado
// Source
let result = a + b;

// Desugared to
let result = a.add(b);
```

For index operators:

```wado
// Source
let value = matrix[[row, col]];
matrix[[row, col]] = 42.0;

// Desugared to
let value = *matrix.index([row, col]);
*matrix.index_mut([row, col]) = 42.0;
```

### Type Checking

The type checker verifies that:

1. The trait is implemented for the operand type(s)
2. The associated `Output` type matches the expected type
3. Generic bounds are satisfied

### Wasm Code Generation

Operator trait calls compile to:

1. **Monomorphization** for generic types (static dispatch, no overhead)
2. **Direct calls** for concrete types (inlined when possible)
3. **Vtable dispatch** for trait objects (`&dyn Add`)

## Future Considerations

### Callable Objects (`Fn` traits)

Future WEP will define `Fn`, `FnMut`, and `FnOnce` traits for callable objects (similar to Rust). This is separate from this WEP.

### Deref Coercion

Rust's `Deref` trait enables smart pointers and automatic coercion. This may be added in a future WEP.

## References

### Language Designs

- [Rust std::ops - Operator Overloading Traits](https://doc.rust-lang.org/std/ops/index.html)
- [Rust By Example - Operator Overloading](https://doc.rust-lang.org/rust-by-example/trait/ops.html)
- [Rust RFC 0953 - op-assign (Compound Assignment Operators)](https://rust-lang.github.io/rfcs/0953-op-assign.html)
- [Rust std::ops::AddAssign](https://doc.rust-lang.org/std/ops/trait.AddAssign.html)
- [C++ operator overloading](https://en.cppreference.com/w/cpp/language/operators.html)
- [Kotlin Operator Overloading](https://kotlinlang.org/docs/operator-overloading.html)
- [Swift Operator Overloading](https://www.dhiwise.com/post/swift-operator-overloading-enhancing-code-expression)
- [Python Dunder Methods](https://www.geeksforgeeks.org/python/dunder-magic-methods-python/)
- [Zig Issue #8567 - MathType for operator overloading](https://github.com/ziglang/zig/issues/8567)
- [Why Zig When There is Already Rust? - No operator overloading](https://ziglang.org/learn/why_zig_rust_d_cpp/)

### Related WEPs

- [WEP: Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) - Trait foundation
- [WEP: Operator Precedence](./wep-2026-01-11-operator-precedence.md) - Operator precedence rules
- [WEP: Value Semantics](./wep-2026-01-12-value-semantics-and-stores.md) - Value semantics and cloning

## Sources

- [Rust std::ops documentation](https://doc.rust-lang.org/std/ops/index.html)
- [Rust By Example - Operators and Overloading](https://doc.rust-lang.org/rust-by-example/trait/ops.html)
- [Rust RFC 0953 - Compound assignment operators](https://rust-lang.github.io/rfcs/0953-op-assign.html)
- [Rust AddAssign trait](https://doc.rust-lang.org/std/ops/trait.AddAssign.html)
- [C++ operator overloading reference](https://en.cppreference.com/w/cpp/language/operators.html)
- [Learn C++ - Overloading arithmetic operators using friend functions](https://www.learncpp.com/cpp-tutorial/overloading-the-arithmetic-operators-using-friend-functions/)
- [Kotlin operator overloading documentation](https://kotlinlang.org/docs/operator-overloading.html)
- [Swift operator overloading guide](https://www.dhiwise.com/post/swift-operator-overloading-enhancing-code-expression)
- [Python dunder methods](https://www.geeksforgeeks.org/python/dunder-magic-methods-python/)
- [Zig operator overloading discussion](https://github.com/ziglang/zig/issues/8567)
- [Zig design philosophy](https://ziglang.org/learn/why_zig_rust_d_cpp/)
