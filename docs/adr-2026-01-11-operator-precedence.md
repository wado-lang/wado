# ADR: Operator Precedence and Associativity

**Date**: 2026-01-11
**Status**: Accepted

## Context

Wado needs to define operator precedence and associativity rules. The language aims to minimize learning costs while avoiding design mistakes present in older languages. Several key decisions were needed:

1. **Overall precedence table**: Which existing language to use as a baseline?
2. **Bitwise vs comparison operators**: Which should have higher precedence?
3. **Bitwise NOT operator**: Use `!` (Rust) or `~` (C/Java/Python)?
4. **Increment/decrement operators**: Include `++`/`--` or not?
5. **Power operator**: Include `**` or not?
6. **Comparison chaining**: Allow `a < b < c` or make it an error?

### C's Design Mistake

C has a well-known precedence design flaw: bitwise operators (`&`, `|`, `^`) have **lower** precedence than comparison operators (`==`, `<`, etc.). This causes bugs:

```c
// C/Java/JavaScript - Bug!
if (flags & MASK == EXPECTED)  // Parsed as: flags & (MASK == EXPECTED)
                               // Intended: (flags & MASK) == EXPECTED
```

**Historical reason**: Early C had no `&&`/`||`. When they were added, `&`/`|` precedence was left unchanged for backward compatibility, creating a permanent design flaw.

**Impact**: Requires excessive parentheses and is counterintuitive since `&` and `|` conceptually behave like arithmetic operators:
- `a + b == 7` correctly parses as `(a + b) == 7` ✅
- `a & b == 7` incorrectly parses as `a & (b == 7)` ❌

### Language Comparison

| Language       | Bitwise vs Comparison | Fixed C's Mistake? |
| -------------- | --------------------- | ------------------ |
| C/Java/JS      | Comparison > Bitwise  | ❌ No              |
| **Rust**       | **Bitwise > Comparison** | ✅ **Yes**      |
| **Go**         | **Bitwise > Comparison** | ✅ **Yes**      |
| Python         | Comparison > Bitwise  | ❌ No              |

Rust and Go fixed this by giving bitwise operators higher precedence than comparison operators.

### Increment/Decrement Operators

`++`/`--` cause serious issues:

```c
int x = 1;
int y = x + ++x;  // Undefined behavior!
                  // GCC: y = 4
                  // Clang: y = 3
```

**Problems**:
- Undefined behavior when used multiple times in same expression
- Side effect confusion
- Prefix vs postfix complexity

**Language responses**:
- **Rust**: Removed entirely ✅
- **Python**: Never had them ✅
- **Go**: Postfix only, statements only (not expressions) ⚠️
- C/Java/JS: Keep them (legacy) ❌

### Power Operator

Python's `**` has counterintuitive precedence:

```python
-1**2  # = -1 (not 1!)
# Parsed as: -(1**2)
```

**Language approaches**:
- **Python/JS**: Have `**` operator (with precedence quirks) ⚠️
- **C/Java/Rust/Go**: Use function: `pow()`, `Math.pow()`, `.pow()` ✅

### Comparison Chaining

Different languages handle chained comparisons differently:

```javascript
// JavaScript - Bug prone
1 < 2 < 3  // true (seems right)
3 > 2 > 1  // false (wait, what?)
// Evaluates as: (3 > 2) > 1 → true > 1 → 1 > 1 → false
```

**Language approaches**:
- **C/Java/Go/JS**: Left-associative (allows confusing bugs) ❌
- **Rust**: Non-associative (compile error) ✅
- **Python**: Special chaining syntax (`a < b < c` = `a < b and b < c`) ✅

## Decision

### 1. Use Rust's Precedence as Baseline

Wado adopts Rust's operator precedence table with minor modifications.

**Rationale**:
- Fixes C's bitwise precedence mistake
- No `++`/`--` (avoids undefined behavior)
- Well-designed and battle-tested
- Minimizes learning costs (developers familiar with Rust can transfer knowledge)

### 2. Bitwise Operators Have Higher Precedence Than Comparison

Following Rust and Go, bitwise operators (`&`, `|`, `^`) have **higher** precedence than comparison operators (`==`, `<`, etc.).

```wado
// Wado - Correct precedence
if flags & MASK == EXPECTED {  // Parsed as: (flags & MASK) == EXPECTED ✅
    // ...
}
```

**Rationale**: Aligns with arithmetic operator intuition, reduces need for parentheses.

### 3. Use `~` for Bitwise NOT

Wado uses `~` (tilde) for bitwise NOT, not `!` (Rust) or `^` (Go).

```wado
let x = 0b1010;
let y = ~x;  // Bitwise NOT
```

**Rationale**:
- More familiar to developers from C/Java/Python/JavaScript backgrounds
- Clear visual distinction between logical NOT (`!`) and bitwise NOT (`~`)
- Same precedence level (unary) as Rust's `!`, so no precedence issues
- Compiler will catch mistakes if Rust developers use `!` instead

### 4. No Increment/Decrement Operators

Wado does **not** have `++` or `--` operators.

```wado
let mut count = 0;
count++;     // ❌ Compile error
count += 1;  // ✅ Correct
```

**Rationale**:
- Avoids undefined behavior
- Eliminates side effect confusion
- Consistent with Rust and Python
- `x += 1` and `x -= 1` are clear and unambiguous

**Lexer behavior**: The lexer detects `++` and `--` tokens and produces an error, preventing expressions like `a--b` or `a---b` from being parsed incorrectly.

### 5. No Power Operator

Wado does **not** have a `**` power operator. Use the `pow()` function instead.

```wado
let result = x ** 2;      // ❌ Compile error
let result = pow(x, 2);   // ✅ Correct
```

**Rationale**:
- Wasm has no native power instruction (would compile to function call anyway)
- Avoids precedence ambiguity (Python's `-1**2 = -1` is counterintuitive)
- Explicit function call is clearer
- Consistent with Rust, Go, C, and Java

**Future note**: `**` is reserved for potential use as a dereference operator.

### 6. Comparison Operator Chaining: Equality Only

**Equality operators** (`==`, `!=`) are **left-associative** and can be chained:

```wado
a == b == c  // ✅ OK: (a == b) == c
a != b != c  // ✅ OK: (a != b) != c
```

**Inequality operators** (`<`, `>`, `<=`, `>=`) are **non-associative** and chaining is a **semantic error**:

```wado
a < b < c    // ❌ Semantic error
a > b > c    // ❌ Semantic error
a <= b <= c  // ❌ Semantic error
a >= b >= c  // ❌ Semantic error
a < b > c    // ❌ Semantic error (parser accepts, analyser rejects)
a == b < c   // ❌ Semantic error (mixing equality and inequality)
```

**Correct way** to express range checks:

```wado
// Use logical AND
if a < b && b < c {  // ✅ Correct
    // ...
}
```

**Rationale**:

1. **Equality chaining is rare but sometimes useful**:
   ```wado
   if x == y == z {  // All three are equal
       // ...
   }
   ```

2. **Inequality chaining is confusing**:
   - Unlike Python's mathematical chaining (`a < b < c` means `a < b AND b < c`), most C-family languages parse it as `(a < b) < c`
   - This creates confusion about whether `a < b < c` means "a less than b less than c" or "(a < b) less than c"
   - Rejecting it forces explicit intent with `&&`

3. **Consistency with the principle of explicitness**:
   - Wado favors explicitness over convenience
   - `a < b && b < c` is clearer than special chaining semantics
   - No need to remember special rules for comparison chaining

4. **Simpler implementation**:
   - No need for special semantic analysis for inequality chains
   - No need to distinguish between mathematical chaining and boolean chaining
   - Easier to reason about and teach

**Implementation**: The parser allows all comparison operators to be chained (left-associative). The semantic analyser then rejects chains containing inequality operators.

## Consequences

### Positive

1. **Fixes C's design mistake**: `flags & MASK == VALUE` works correctly without parentheses
2. **Avoids undefined behavior**: No `++`/`--` operators
3. **Clear and explicit**: `pow(x, y)` instead of ambiguous `**`
4. **Familiar to C/Java/Python developers**: `~` for bitwise NOT
5. **Prevents confusing comparisons**: `a < b < c` is an error, forcing `a < b && b < c`
6. **Consistent with Rust**: Minimal learning curve for Rust developers (except `~` vs `!`)
7. **Battle-tested**: Rust's precedence has been proven in production

### Negative

1. **Diverges from Rust on bitwise NOT**: Rust developers might use `!` by habit
   - **Mitigation**: Compiler error will catch this immediately
2. **No mathematical comparison chaining**: `a < b < c` requires `a < b && b < c`
   - **Mitigation**: Explicit is better than implicit; no ambiguity
3. **Equality chaining is still left-associative**: `a == b == c` is `(a == b) == c`, which may surprise some developers
   - **Mitigation**: This usage is rare; when needed, parentheses can clarify intent

### Precedence Table for Wado

From highest to lowest precedence:

| Level | Operators                              | Associativity   | Description                    |
| ----- | -------------------------------------- | --------------- | ------------------------------ |
| 1     | `::`, `.`, `()`                        | Left-to-right   | Paths, method calls, fields    |
| 2     | `?`                                    | N/A             | Error propagation              |
| 3     | `!`, `~`, `-`, `*`, `&`, `&mut`        | Right-to-left   | Unary operators                |
| 4     | `as`                                   | Left-to-right   | Type cast                      |
| 5     | `*`, `/`, `%`                          | Left-to-right   | Multiplicative                 |
| 6     | `+`, `-`                               | Left-to-right   | Additive                       |
| 7     | `<<`, `>>`                             | Left-to-right   | Bitwise shift                  |
| 8     | `&`                                    | Left-to-right   | Bitwise AND                    |
| 9     | `^`                                    | Left-to-right   | Bitwise XOR                    |
| 10    | `\|`                                   | Left-to-right   | Bitwise OR                     |
| 11    | `==`, `!=` (left-assoc), `<`, `>`, `<=`, `>=` (non-assoc) | **Mixed** | Comparison |
| 12    | `&&`                                   | Left-to-right   | Logical AND                    |
| 13    | `\|\|`                                 | Left-to-right   | Logical OR                     |
| 14    | `..`, `..=`                            | N/A             | Range operators                |
| 15    | `=`, `+=`, `-=`, etc.                  | Right-to-left   | Assignment                     |

**Key differences from Rust**:
- Level 3: Added `~` for bitwise NOT (Rust uses `!` only)
- Level 11: `==` and `!=` are left-associative (can chain), but `<`, `>`, `<=`, `>=` are non-associative (semantic error when chained)

## References

- [Operator precedence is broken - foonathan.net](https://www.foonathan.net/2017/07/operator-precedence/)
- [C Operator Precedence - cppreference.com](https://en.cppreference.com/w/c/language/operator_precedence.html)
- [Rust Reference - Expressions](https://doc.rust-lang.org/reference/expressions.html)
- [Go 101 - Common Operators](https://go101.org/article/operators.html)
- [Python Expressions Documentation](https://docs.python.org/3/reference/expressions.html)
- [MDN - JavaScript Operator Precedence](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators/Operator_precedence)
- [Learn C++ - Increment/decrement operators and side effects](https://www.learncpp.com/cpp-tutorial/increment-decrement-operators-and-side-effects/)

See also: `docs/operator-precedence-research.md` for detailed cross-language comparison.
