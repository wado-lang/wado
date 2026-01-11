# Operator Precedence Research

Research on operator precedence across C, Java, Rust, Go, JavaScript, and Python for Wado language design.

## Executive Summary

**Key Findings:**

1. **C's Design Mistake**: Bitwise operators (`&`, `|`, `^`) have **lower** precedence than comparison operators (`==`, `<`, etc.), which is counterintuitive since they conceptually behave like arithmetic operators
2. **Rust Fixed This**: Rust gives bitwise operators **higher** precedence than comparison operators, aligning with intuition
3. **Go Also Fixed This**: Go groups bitwise AND with multiplication (highest), bitwise OR/XOR with addition (middle), and comparisons last (lowest)
4. **Python's Power Operator Quirk**: `**` binds tighter than unary on left, looser than unary on right, so `-1**2` = `-1`
5. **Increment/Decrement Problems**: `++`/`--` cause undefined behavior, side effect issues, and confusion - languages like Rust, Go, and Python omit them

## The C Design Mistake Explained

### The Problem

In C (and languages that inherited this design like Java and JavaScript), the expression:

```c
if (flags & MASK == EXPECTED)  // Bug! Parsed as: flags & (MASK == EXPECTED)
```

is parsed as `flags & (MASK == EXPECTED)`, not `(flags & MASK) == EXPECTED`.

This happens because **comparison operators have higher precedence than bitwise operators** in C.

### Why This Is Wrong

Conceptually, `&` and `|` are arithmetic-like operations:

- `a + b == 7` correctly parses as `(a + b) == 7`
- `a * b == 7` correctly parses as `(a * b) == 7`
- `a & b == 7` **incorrectly** parses as `a & (b == 7)` ❌

This violates the principle of least surprise and requires excessive parentheses.

### Historical Reason

According to the [Wikipedia article on C operators](https://en.wikipedia.org/wiki/Operators_in_C_and_C++) and [foonathan.net's analysis](https://www.foonathan.net/2017/07/operator-precedence/):

> Historically, there was no syntactic distinction between the bitwise and logical operators. In BCPL, B and early C, the operators `&&` `||` didn't exist. Instead `&` `|` had different meaning depending on whether they are used in a 'truth-value context'.

When `&&` and `||` were added, the precedence of `&` and `|` was left as-is for backward compatibility, creating this permanent design flaw.

### Real-World Impact

From [Microsoft's documentation](https://learn.microsoft.com/en-us/cpp/ide/lnt-comparison-bitwise-precedence?view=msvc-170):

> The comparison operator has a higher precedence than the bitwise operator. The comparison operator will be evaluated first. The result will be implicitly cast to an integer and used as an operand in the bitwise operation.

This causes bugs that are caught by static analyzers with warnings like "comparison operator has higher precedence than bitwise operator."

## Comprehensive Precedence Comparison

### 1. Bitwise vs Comparison Operators

| Language   | Bitwise Precedence           | Comparison Precedence | Which is Higher?          | Source                                                                                                 |
| ---------- | ---------------------------- | --------------------- | ------------------------- | ------------------------------------------------------------------------------------------------------ |
| **C**      | `&` (8), `^` (9), `\|` (10)  | `==`, `!=` (7)        | ❌ Comparison > Bitwise   | [cppreference](https://en.cppreference.com/w/c/language/operator_precedence.html)                      |
| **Java**   | `&` (8), `^` (9), `\|` (10)  | `==`, `!=` (7)        | ❌ Comparison > Bitwise   | [Programiz Java](https://www.programiz.com/java-programming/operator-precedence)                       |
| **Rust**   | `&` (8), `^` (9), `\|` (10)  | `==`, `!=` (11)       | ✅ Bitwise > Comparison   | [Rust Reference](https://doc.rust-lang.org/reference/expressions.html)                                 |
| **Go**     | `&` (5), `^`/`\|` (4)        | `==`, `!=` (3)        | ✅ Bitwise > Comparison   | [Go 101](https://go101.org/article/operators.html)                                                     |
| **JS**     | `&` (9), `^` (10), `\|` (11) | `==`, `!=` (10)       | ❌ Mixed (& > ==, \| < =) | [MDN](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators/Operator_precedence) |
| **Python** | `&` (9), `^` (10), `\|` (11) | `==`, `!=` (7)        | ❌ Comparison > Bitwise   | [Python Docs](https://docs.python.org/3/reference/expressions.html)                                    |

**Note**: Lower precedence number = higher priority (evaluated first). Table uses common numbering where 1 is highest.

### 2. Unary Operators

| Language   | Unary `-`, `+` | Bitwise NOT   | Notes                                             |
| ---------- | -------------- | ------------- | ------------------------------------------------- |
| **C**      | Level 2        | `~` (Level 2) | All unary at same level, right-to-left            |
| **Java**   | Level 2        | `~` (Level 2) | Same as C                                         |
| **Rust**   | Not applicable | `!` (Level 1) | Rust uses `!` for bitwise NOT, highest precedence |
| **Go**     | Level 2        | `^` (Level 2) | Go uses `^` for bitwise NOT (XOR with 1s)         |
| **JS**     | Level 3        | `~` (Level 3) | All unary at same level                           |
| **Python** | Level 3        | `~` (Level 3) | All unary at same level                           |

**Important**: Wado uses `~` for bitwise NOT (like C/Java/JS/Python), not `!` (Rust) or `^` (Go).

### 3. Increment/Decrement Operators (`++`, `--`)

| Language   | Has ++/--? | Prefix/Postfix? | Notes                                  |
| ---------- | ---------- | --------------- | -------------------------------------- |
| **C**      | ✅ Yes     | Both            | Side effects, undefined behavior risks |
| **Java**   | ✅ Yes     | Both            | Inherits C semantics                   |
| **Rust**   | ❌ No      | N/A             | Removed to avoid side effect issues    |
| **Go**     | ✅ Yes     | Postfix only    | Statements only, not expressions       |
| **JS**     | ✅ Yes     | Both            | Inherits C semantics                   |
| **Python** | ❌ No      | N/A             | Never had them, uses `+= 1` instead    |

**Problems with `++`/`--`** ([Learn C++](https://www.learncpp.com/cpp-tutorial/increment-decrement-operators-and-side-effects/)):

1. **Undefined Behavior**: `x + ++x` has different results in different compilers
2. **Side Effects**: Modifying a variable multiple times in same expression is UB
3. **Confusion**: Prefix vs postfix semantics are error-prone
4. **Performance Myth**: Postfix must copy original value (though optimizers fix this)

**Wado Decision**: ❌ **No `++`/`--` operators** (following Rust/Python). Use `x += 1` and `x -= 1` instead.

### 4. Power Operator (`**`)

| Language   | Has \*\*? | Precedence          | Notes                                            |
| ---------- | --------- | ------------------- | ------------------------------------------------ |
| **C**      | ❌ No     | N/A                 | Use `pow()` function                             |
| **Java**   | ❌ No     | N/A                 | Use `Math.pow()`                                 |
| **Rust**   | ❌ No     | N/A                 | Use `.pow()` method                              |
| **Go**     | ❌ No     | N/A                 | Use `math.Pow()`                                 |
| **JS**     | ✅ Yes    | Level 3 (very high) | Right-associative: `2**3**2` = `2**(3**2)` = 512 |
| **Python** | ✅ Yes    | Between unary       | `-1**2` = `-1` (unary binds looser on left)      |

**Python's Quirk** ([Python Docs](https://docs.python.org/3/reference/expressions.html)):

> The power operator binds more tightly than unary operators on its left; it binds less tightly than unary operators on its right.

This means:

- `-1**2` = `-(1**2)` = `-1` (not `(-1)**2` = `1`)
- `2**-1` = `2**(-1)` = `0.5` (unary on right binds first)

**Wado Decision**: ❌ **No `**` operator\*\*. Reasons:

1. Wasm has no native power instruction (would compile to function call anyway)
2. Ambiguous precedence (Python's approach is confusing)
3. Use explicit `pow(x, y)` function instead
4. Consistent with Rust/Go

## Detailed Precedence Tables

### Rust Precedence (Wado's Baseline)

From highest to lowest ([Rust Reference](https://doc.rust-lang.org/reference/expressions.html)):

| Level | Operators                                                 | Associativity | Description       |
| ----- | --------------------------------------------------------- | ------------- | ----------------- |
| 1     | Paths, method calls, field access                         | Left-to-right | `::`, `.`, `()`   |
| 2     | `?`                                                       | N/A           | Error propagation |
| 3     | Unary: `!`, `-`, `*`, `&`, `&mut`                         | Right-to-left | Prefix operators  |
| 4     | `as`                                                      | Left-to-right | Type cast         |
| 5     | `*`, `/`, `%`                                             | Left-to-right | Multiplicative    |
| 6     | `+`, `-`                                                  | Left-to-right | Additive          |
| 7     | `<<`, `>>`                                                | Left-to-right | Bitwise shift     |
| 8     | `&`                                                       | Left-to-right | Bitwise AND       |
| 9     | `^`                                                       | Left-to-right | Bitwise XOR       |
| 10    | `\|`                                                      | Left-to-right | Bitwise OR        |
| 11    | `==`, `!=` (left-assoc), `<`, `>`, `<=`, `>=` (non-assoc) | **Mixed**     | Comparison        |
| 12    | `&&`                                                      | Left-to-right | Logical AND       |
| 13    | `\|\|`                                                    | Left-to-right | Logical OR        |
| 14    | `..`, `..=`                                               | N/A           | Range operators   |
| 15    | `=`, `+=`, etc.                                           | Right-to-left | Assignment        |

**Key Points**:

- ✅ Bitwise operators (8-10) have **higher** precedence than comparison (11)
- ✅ Unary operators (3) are at the top, very high precedence
- ⚠️ **Wado modification**: Mathematical comparison chaining with validation:
  - Same-direction chains allowed: `a < b < c`, `a > b > c`, `a == b == c`
  - Mixed-direction chains rejected: `a < b > c`
  - `!=` cannot be chained: `a != b != c` is an error
- ✅ No `++`, `--`, or `**` operators

### Go Precedence (Alternative Approach)

From highest to lowest ([Go 101](https://go101.org/article/operators.html)):

| Level | Operators                            | Associativity | Description              |
| ----- | ------------------------------------ | ------------- | ------------------------ |
| 5     | `*`, `/`, `%`, `<<`, `>>`, `&`, `&^` | Left-to-right | Multiplicative + bit ops |
| 4     | `+`, `-`, `\|`, `^`                  | Left-to-right | Additive + bit ops       |
| 3     | `==`, `!=`, `<`, `<=`, `>`, `>=`     | Left-to-right | Comparison               |
| 2     | `&&`                                 | Left-to-right | Logical AND              |
| 1     | `\|\|`                               | Left-to-right | Logical OR               |

**Key Points**:

- ✅ Bitwise AND `&` is with multiplication (highest)
- ✅ Bitwise OR/XOR `|`/`^` are with addition (middle)
- ✅ Comparisons are separate and lower (level 3)
- 🔸 Note: `^` is bitwise XOR in binary, bitwise NOT in unary
- 🔸 `&^` is "bit clear" operator (AND NOT)

### C Precedence (The Problematic One)

From highest to lowest ([cppreference](https://en.cppreference.com/w/c/language/operator_precedence.html)):

| Level | Operators                                | Associativity | Description         |
| ----- | ---------------------------------------- | ------------- | ------------------- |
| 2     | `++`, `--`, `!`, `~`, `+`, `-`, `*`, `&` | Right-to-left | Unary               |
| 3     | `*`, `/`, `%`                            | Left-to-right | Multiplicative      |
| 4     | `+`, `-`                                 | Left-to-right | Additive            |
| 5     | `<<`, `>>`                               | Left-to-right | Bitwise shift       |
| 6     | `<`, `<=`, `>`, `>=`                     | Left-to-right | Relational          |
| 7     | `==`, `!=`                               | Left-to-right | Equality            |
| 8     | `&`                                      | Left-to-right | Bitwise AND         |
| 9     | `^`                                      | Left-to-right | Bitwise XOR         |
| 10    | `\|`                                     | Left-to-right | Bitwise OR          |
| 11    | `&&`                                     | Left-to-right | Logical AND         |
| 12    | `\|\|`                                   | Left-to-right | Logical OR          |
| 13    | `?:`                                     | Right-to-left | Ternary conditional |
| 14    | `=`, `+=`, etc.                          | Right-to-left | Assignment          |

**Problems**:

- ❌ Comparison (6-7) has **higher** precedence than bitwise (8-10)
- ❌ `flags & MASK == VALUE` parses as `flags & (MASK == VALUE)`
- ❌ Requires excessive parentheses: `(flags & MASK) == VALUE`

## Associativity Comparison

| Language   | Comparison Associativity | Notes                                        |
| ---------- | ------------------------ | -------------------------------------------- |
| **C**      | Left-to-right            | `a < b < c` = `(a < b) < c` (probably wrong) |
| **Java**   | Left-to-right            | Same as C                                    |
| **Rust**   | **Non-associative**      | `a < b < c` is **compile error** ✅          |
| **Go**     | Left-to-right            | But rarely chained in practice               |
| **JS**     | Left-to-right            | `1 < 2 < 3` = `true` (confusing!)            |
| **Python** | **Chained comparisons**  | `a < b < c` = `(a < b) and (b < c)` ✅       |

**Analysis**:

- **C/Java/JS**: Allow meaningless chains like `1 < 2 < 3` which evaluate to `true < 3` → `1 < 3` → `true`
- **Rust**: Rejects chained comparisons at compile time (best for correctness)
- **Python**: Special syntax for chained comparisons (elegant but complex to implement)

### Wado's Decision (2026-01-11)

Wado supports **mathematical comparison chaining** similar to Python, with stricter validation rules.

**Valid chains** (same direction):

- `a < b < c` → `a < b AND b < c` ✅ (ascending)
- `a > b > c` → `a > b AND b > c` ✅ (descending)
- `a <= b <= c` → `a <= b AND b <= c` ✅
- `a >= b >= c` → `a >= b AND b >= c` ✅
- `a == b == c` → `a == b AND b == c` ✅

**Invalid chains** (semantic error):

- `a < b > c` → **semantic error** ❌ (mixed directions)
- `a > b < c` → **semantic error** ❌ (mixed directions)
- `a < b >= c` → **semantic error** ❌ (mixing `<` and `>=`)
- `a == b < c` → **semantic error** ❌ (mixing `==` and inequality)
- `a != b != c` → **semantic error** ❌ (`!=` chaining not allowed)

**Chaining rules**:

1. Same-direction inequality: `<`/`<=` can only chain with `<`/`<=`, and `>`/`>=` can only chain with `>`/`>=`
2. Equality chaining: `==` can only chain with `==`
3. No `!=` chaining: `!=` cannot be chained (ambiguous meaning)
4. No mixing: Cannot mix equality operators with inequality operators

**Rationale**:

- Mathematical intuition: `0 <= x <= 100` is natural and readable
- Python-like: Familiar to Python developers
- Rejects ambiguous cases: `a < b > c` and `a != b != c` are unclear
- Clearer than `&&`: `a < b < c` is more readable than `a < b && b < c`

## Concrete Examples of Issues

### Example 1: C's Bitwise Precedence Bug

```c
// C code - Bug lurking!
#define READ_FLAG  0x01
#define WRITE_FLAG 0x02

// Intended: check if BOTH flags are set
if (permissions & READ_FLAG == READ_FLAG) {  // ❌ WRONG!
    // This checks: permissions & (READ_FLAG == READ_FLAG)
    //            = permissions & 1
    //            = permissions & true
    // Never what you want!
}

// Correct version (requires parentheses)
if ((permissions & READ_FLAG) == READ_FLAG) {  // ✅ Correct
    // Do something
}
```

### Example 2: Python's Power Operator Confusion

```python
# Python - Confusing precedence
print(-1**2)   # Output: -1 (not 1!)
# Parsed as: -(1**2) = -(1) = -1

print((-1)**2) # Output: 1
# Explicitly grouped

# With variables, even more confusing
x = 2
print(-x**2)   # Output: -4 (parsed as -(x**2))
```

### Example 3: C's Increment Operator Undefined Behavior

```c
// C code - Undefined behavior!
int x = 1;
int y = x + ++x;  // UB! Different compilers give different results
// GCC/Visual Studio: y = 2 + 2 = 4
// Clang: y = 1 + 2 = 3

// Also problematic
int arr[3] = {1, 2, 3};
int i = 0;
arr[i] = i++;  // UB! Order of evaluation not specified
```

### Example 4: JavaScript's Chained Comparison Confusion

```javascript
// JavaScript - Logically wrong but syntactically valid
console.log(1 < 2 < 3); // true (seems right?)
console.log(3 > 2 > 1); // false (wait, what?)

// What's actually happening:
// 1 < 2 < 3
// = (1 < 2) < 3
// = true < 3
// = 1 < 3
// = true

// 3 > 2 > 1
// = (3 > 2) > 1
// = true > 1
// = 1 > 1
// = false
```

## Recommendations for Wado

### ✅ Use Rust's Precedence as Baseline

**Reasons**:

1. **Fixes C's bitwise mistake**: Bitwise operators > Comparison operators
2. **No increment/decrement**: Avoids undefined behavior and side effect issues
3. **Non-associative comparisons**: Prevents `a < b < c` bugs at compile time
4. **No power operator**: Avoids precedence ambiguity
5. **Well-designed**: Rust's precedence has been carefully thought through

### ✅ Wado-Specific Decisions Align Well

From the spec, Wado has already decided:

- ✅ Use `~` for bitwise NOT (like C/Java/Python, not Rust's `!`)
- ✅ No `++`/`--` operators (like Rust/Python)
- ✅ No `**` operator (like Rust/Go/C/Java)
- ✅ Use Rust precedence as baseline

### ⚠️ Potential Pitfall: Bitwise NOT Symbol

**The Issue**: Wado uses `~` for bitwise NOT, while Rust uses `!`.

| Operation   | Rust | Wado | Issue                  |
| ----------- | ---- | ---- | ---------------------- |
| Bitwise NOT | `!x` | `~x` | Different symbol       |
| Logical NOT | `!x` | `!x` | Same                   |
| Precedence  | Same | Same | Both are unary level 3 |

**Analysis**:

- ✅ **Not a problem**: Precedence is the same (unary level)
- ✅ `~` is familiar to C/Java/JS/Python developers
- ✅ Clear distinction between logical (`!`) and bitwise (`~`)
- ⚠️ **Minor**: Rust developers might use `!` by habit, but compiler error will catch it

### 📋 Recommended Precedence Table for Wado

Based on Rust, with Wado-specific operators:

| Level | Operators                                                 | Associativity | Description       |
| ----- | --------------------------------------------------------- | ------------- | ----------------- |
| 1     | Paths, method calls, field access                         | Left-to-right | `::`, `.`, `()`   |
| 2     | `?`                                                       | N/A           | Error propagation |
| 3     | Unary: `!`, `~`, `-`, `*`, `&`, `&mut`                    | Right-to-left | Prefix operators  |
| 4     | `as`                                                      | Left-to-right | Type cast         |
| 5     | `*`, `/`, `%`                                             | Left-to-right | Multiplicative    |
| 6     | `+`, `-`                                                  | Left-to-right | Additive          |
| 7     | `<<`, `>>`                                                | Left-to-right | Bitwise shift     |
| 8     | `&`                                                       | Left-to-right | Bitwise AND       |
| 9     | `^`                                                       | Left-to-right | Bitwise XOR       |
| 10    | `\|`                                                      | Left-to-right | Bitwise OR        |
| 11    | `==`, `!=` (left-assoc), `<`, `>`, `<=`, `>=` (non-assoc) | **Mixed**     | Comparison        |
| 12    | `&&`                                                      | Left-to-right | Logical AND       |
| 13    | `\|\|`                                                    | Left-to-right | Logical OR        |
| 14    | `..`, `..=`                                               | N/A           | Range operators   |
| 15    | `=`, `+=`, etc.                                           | Right-to-left | Assignment        |

**Key differences from Rust**:

- Level 3: Added `~` for bitwise NOT (Rust uses `!` only)
- Level 11: Mathematical comparison chaining allowed (Rust rejects all chaining):
  - Same-direction chains: `a < b < c`, `a > b > c`, `a == b == c`
  - Mixed-direction and `!=` chains are semantic errors

### 🎯 Verification Examples

To ensure Wado's precedence is correct:

```wado
// Example 1: Bitwise vs Comparison (Rust-style, correct)
let flags = 0b1010;
let mask = 0b0010;

if flags & mask == mask {  // ✅ Parses as: (flags & mask) == mask
    println("Flag is set");
}

// Example 2: Bitwise NOT
let x = 0b1010;
let y = ~x;  // ✅ Bitwise NOT (Wado uses ~, not !)

// Example 3: Comparison chaining rules
if a < b < c {  // ✅ OK (mathematical chaining: a < b AND b < c)
    println("a < b < c");
}

if 0 <= x <= 100 {  // ✅ OK (range check)
    println("x is in range [0, 100]");
}

if a < b > c {  // ❌ Semantic error (mixed directions)
}

if a != b != c {  // ❌ Semantic error (!= chaining not allowed)
}

if a == b == c {  // ✅ OK (equality chaining)
    println("All three are equal");
}

// Example 4: No increment operators
let mut count = 0;
count++;  // ❌ Compile error (no ++ operator)
count += 1;  // ✅ Correct way

// Example 5: No power operator
let result = x ** 2;  // ❌ Compile error (no ** operator)
let result = pow(x, 2);  // ✅ Use function instead
```

## Summary of Design Decisions

| Feature                    | C/Java | Rust | Wado     | Rationale                                     |
| -------------------------- | ------ | ---- | -------- | --------------------------------------------- |
| Bitwise > Comparison       | ❌     | ✅   | ✅       | Fixes C's counterintuitive precedence         |
| Bitwise NOT symbol         | `~`    | `!`  | `~`      | Familiar to most developers                   |
| Increment/decrement (`++`) | ✅     | ❌   | ❌       | Avoids undefined behavior & side effects      |
| Power operator (`**`)      | ❌     | ❌   | ❌       | No native Wasm instruction, use `pow()`       |
| Chained comparisons        | Left   | Non  | **Math** | Same-direction chains OK, mixed/`!=` rejected |
| Unary precedence           | High   | High | High     | Consistent across languages                   |

## Sources

1. [Operator precedence is broken - foonathan.net](https://www.foonathan.net/2017/07/operator-precedence/)
2. [Microsoft Learn - Precedence and order of evaluation](https://learn.microsoft.com/en-us/cpp/c-language/precedence-and-order-of-evaluation?view=msvc-170)
3. [Operators in C and C++ - Wikipedia](https://en.wikipedia.org/wiki/Operators_in_C_and_C++)
4. [Microsoft Learn - lnt-comparison-bitwise-precedence](https://learn.microsoft.com/en-us/cpp/ide/lnt-comparison-bitwise-precedence?view=msvc-170)
5. [C Operator Precedence - cppreference.com](https://en.cppreference.com/w/c/language/operator_precedence.html)
6. [Rust Reference - Expressions](https://doc.rust-lang.org/reference/expressions.html)
7. [Operator expressions - The Rust Reference](https://doc.rust-lang.org/reference/expressions/operator-expr.html)
8. [Operator Precedence: We can do better - Adamant Programming Language Blog](https://blog.adamant-lang.org/2019/operator-precedence/)
9. [Python 3 Expressions Documentation](https://docs.python.org/3/reference/expressions.html)
10. [Go 101 - Common Operators](https://go101.org/article/operators.html)
11. [Operator precedence in Go - Programming.Guide](https://programming.guide/go/operator-priority.html)
12. [MDN - JavaScript Operator Precedence](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators/Operator_precedence)
13. [Learn C++ - Increment/decrement operators and side effects](https://www.learncpp.com/cpp-tutorial/increment-decrement-operators-and-side-effects/)
14. [Programiz - Increment and Decrement Operators](https://www.programiz.com/article/increment-decrement-operator-difference-prefix-postfix)
15. [Increment and decrement operators - Wikipedia](https://en.wikipedia.org/wiki/Increment_and_decrement_operators)

---

**Date**: 2026-01-11
**Status**: Research Complete
**Decision**: See `docs/adr-2026-01-11-operator-precedence.md` for the final decision

**Summary**: ✅ Use Rust's precedence as baseline with the following modifications:

- Use `~` for bitwise NOT (not Rust's `!`)
- Mathematical comparison chaining (like Python):
  - Same-direction chains allowed: `a < b < c`, `a > b > c`, `a == b == c`
  - Mixed-direction chains rejected: `a < b > c`
  - `!=` chaining rejected: `a != b != c`
