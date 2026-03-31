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

### gen_context.wado

- **O(n) rule lookup in hot paths**: `first_of_rule` and `rule_is_single_token` do linear scans over `parser_rules` to find a rule by name. Add a `TreeMap<String, usize>` index (or `rule_by_name()` helper) to GenContext for O(log n) lookup.
- **Repeated O(n²) dedup pattern**: Multiple methods (`first_of_rule`, `first_of_element`, `first_of_alt`, `first_of_elements_from`) use `array_contains_str` loops to deduplicate FIRST sets. Consider a `TreeSet`-based accumulator or a shared helper.
- **Redundant `found` variable in `rule_is_single_token`**: The `found` variable is set to `true` then used in `result = found && all_alts_ok`, but since `found` is always `true` at that point (we're inside the `if rule.name == *name` block), it's redundant.
- **Inconsistent visiting cleanup**: `first_of_rule` rebuilds the visiting array inline; `rule_is_single_token` calls `remove_from_visiting`. Unify on one approach.

### parser_gen.wado

- **Linear rule lookups**: Many functions iterate `ctx.parser_rules` to find a rule by name (e.g., `gen_parser_rule`, `build_sll_node`, `sll_advance`). Would benefit from the `rule_by_name()` index mentioned above.
- **Duplicated branch merge logic**: SLL prediction tree building has similar merge/dedup patterns in multiple places. Could be consolidated.
- **Double rule lookup** (~lines 201-233): `gen_parser_rule` looks up the rule, then `gen_single_alt_parser` may look it up again.
- **Unused function**: `_sll_closure_unused` is dead code (prefixed with `_` to suppress warnings).
- **Heavy `array_contains_str` usage**: 34+ occurrences across the file. A set-based approach would improve both readability and performance.

### gen_util.wado

- **Duplicated symbol mappings**: `literal_field_name()` and `literal_const_name()` both map punctuation characters to names (e.g., `+` → `plus`/`PLUS`) with separate, potentially divergent tables. Unify into a single mapping.
- **Duplicated escape logic**: String escaping patterns appear in multiple places; could be consolidated.
- **Overly long `literal_field_name()`**: The function is a long chain of if-else for each symbol. A table-driven approach would be more maintainable.

### Tests

- **`#[timeout_ms(240000)]` on `generate_sqlite_golden`**: After the FIRST set memoization, actual runtime is ~31s. The timeout (240s) is 7.7x higher. Reduce to ~60000-90000ms.
- **Skipped AST test files**: `driver_calculator_ast_test.wado`, `driver_json_ast_test.wado`, `driver_sexpression_ast_test.wado` have `.skip` suffix — investigate whether they should be fixed or removed.
- **No negative test cases**: No tests for malformed `.g4` input (syntax errors, missing rules, duplicate rule names). Would improve robustness.
