# Gale TODO

## Code Quality

### parser_gen.wado

- **Duplicated branch merge logic**: SLL prediction tree building has similar merge/dedup patterns in multiple places. Could be consolidated.

### Tests

- **No negative test cases**: No tests for malformed `.g4` input (syntax errors, missing rules, duplicate rule names). Would improve robustness.

## Performance: sqlite-parse Benchmark

Profiling the SQLite parser benchmark (13 KB SQL, 100 iterations, ~26 ms/iter) with exhaustive guest profiling (6.9M samples) reveals the following bottlenecks.

### Bottleneck 1: Backtracking (~44% inclusive)

The SLL prediction engine falls back to backtracking when it cannot disambiguate alternatives within depth-5 lookahead. The top backtracking functions by inclusive time:

| Function                       | Inclusive | Grammar alternatives                                 |
| ------------------------------ | --------- | ---------------------------------------------------- |
| `parse_result_column_bt_2`     | 12.2%     | `expr (K_AS? column_alias)?`                         |
| `parse_expr_bt_13`             | 10.2%     | `function_name '(' ... ')'`                          |
| `parse_expr_bt_2`              | 5.7%      | `((database_name '.')? table_name '.')? column_name` |
| `parse_table_or_subquery_bt_1` | 4.2%      | table reference with alias                           |
| `parse_result_column_bt_1`     | 4.1%      | `table_name '.' '*'`                                 |
| `parse_expr_bt_21`             | 4.0%      | `(NOT? EXISTS)? '(' select_stmt ')'`                 |
| `parse_table_or_subquery_bt_0` | 3.1%      | table reference                                      |

#### `result_column` — worst case

Grammar rule: `'*' | table_name '.' '*' | expr (K_AS? column_alias)?`

When an IDENTIFIER token appears, the generator cannot distinguish `table_name '.' '*'` from `expr` because both start with IDENTIFIER. The RuleRef `table_name` expands to `any_name` → IDENTIFIER, so a 2-token lookahead (IDENTIFIER + `.`) would suffice, but RuleRef expansion was reverted (see CLAUDE.md "Failed Approaches"). The generated code:

1. Tries `bt_1` (`table_name '.' '*'`) — parses `table_name`, expects `.`, fails, backtracks
2. Falls through to `bt_2` (`expr`) — succeeds

Every non-`*` result column pays for one full failed `table_name` parse.

#### `expr` with LPAREN — 4-way backtracking

When `(` appears in expression position, the generator tries 4 alternatives sequentially:

1. `bt_21`: `[NOT] EXISTS '(' select_stmt ')'` — fails, backtracks
2. `bt_14`: `'(' expr ')'` — fails, backtracks
3. `bt_13`: `function_name '(' ... ')'` — fails, backtracks
4. `bt_2`: `((db.)? table.)? column` — final fallback

Since `expr` is called from WHERE, HAVING, SELECT columns, etc., this 4-way backtracking multiplies across the entire parse.

#### `expr` with IDENTIFIER — common case, 2-3 way backtracking

Most expressions in SQL are column references (bare IDENTIFIERs). The generated code tries `bt_13` (function call: `function_name '(' ...`) before `bt_2` (column reference). Since `function_name` expands to IDENTIFIER, it always matches the first token, then fails at `(`, and backtracks. Every simple column reference pays for one wasted `function_name` parse.

### Bottleneck 2: `Parser::expect` error path (22% combined)

| Function                     | Self-time |
| ---------------------------- | --------- |
| `Parser::expect`             | 12.0%     |
| `String::push_str`           | 4.7%      |
| `LexerSlice^Display::fmt`    | 3.4%      |
| `String::push` (from expect) | ~2%       |

`expect` constructs a `ParseError` with template string and array literal on every failure. During backtracking, most `expect` calls are speculative — the error is immediately discarded when the caller backtracks. Constructing the error message (`ParseError::new(...)` with string interpolation and `Array<String>` allocation) is pure waste on the failure path.

Reducing backtracking (Bottleneck 1) would eliminate most of this cost.

### Bottleneck 3: `Parser::last_end` (11.7%)

| Function           | Self-time |
| ------------------ | --------- |
| `Parser::last_end` | 11.7%     |

Called after every token consumption to compute node spans via `Span::new(start, p.last_end())`. The function itself is trivial (array index), but at millions of calls the overhead accumulates.

Possible improvement: cache `last_end` in a field on `Parser` updated by `advance()`, avoiding repeated array indexing.

### Summary

| Category                                   | Estimated share | Priority                                  |
| ------------------------------------------ | --------------- | ----------------------------------------- |
| Backtracking                               | ~44% inclusive  | High                                      |
| expect error path (driven by backtracking) | ~22%            | High (addressed by reducing backtracking) |
| last_end overhead                          | ~12%            | Medium                                    |

## Generated Parser Bugs

(none currently)
