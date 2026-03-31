# Gale TODO

## Incomplete CST walker coverage

The XML unparse output is missing tokens/nodes for several patterns due to `store=false` and non-simple groups:

- **Repeated separators**: `(',' result_column)*` — the `','` and subsequent `result_column` are not stored, so `SELECT a, b` only shows `a`.
- **Token-only groups**: `(K_INSERT | K_REPLACE)` — groups with only TokenRef alternatives are not `is_simple_cst_group` (requires RuleRef alternatives), so `INSERT` keyword is missing from `insert_stmt`.
- **Multi-element single-alt groups**: `(';'+ sql_stmt)*` — the Star inner group has two elements (Plus and RuleRef), not a simple CST group, so second statements are not stored.

Fixing these requires either:

1. Extending `is_simple_cst_group` to handle token-only and mixed groups (generating variant types for tokens too).
2. Or making `gen_repeat` for Star/Plus store all inner elements (requires struct types for group alternatives with multiple elements).

## Code Quality

### parser_gen.wado

- **Duplicated branch merge logic**: SLL prediction tree building has similar merge/dedup patterns in multiple places. Could be consolidated.

### Tests

- **No negative test cases**: No tests for malformed `.g4` input (syntax errors, missing rules, duplicate rule names). Would improve robustness.
