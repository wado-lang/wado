# Gale TODO

## Remove `store` parameter from `gen_element`

`gen_element` has a `store: bool` parameter that controls generated variable naming:
- `store=true`: `let field_name = parse_xxx(p)?;` (meaningful name for struct field assignment)
- `store=false`: `let tok = parse_xxx(p)?;` (throwaway name)

`store=false` is a workaround for a naming collision problem in `dedup_name`. The underlying issue is that `dedup_name` assumes sequential variable creation, but branching code (optionals, groups, prediction trees) creates variables in different scopes:

```
// gen_optional_with_lookahead generates:
if condition {
    let k_limit = p.expect(...)?;
    let expr = parse_expr(p)?;        // dedup count 0
    if grp_kind == TK_K_OFFSET { ... }
    let expr = parse_expr(p)?;        // dedup count 0 again → collision!
}
```

With `store=false`, both become `let tok = ...` / `let tok_2 = ...` (separate namespace), avoiding the collision. But this is a hack — all generated variables should use meaningful names.

### Root cause

`dedup_name` tracks a flat counter per name. Optional/group paths increment this counter even though their variables are scoped to if-blocks. When mandatory elements follow, `gen_field_assignments` expects names at count N but the parsing code produced count N+K (shifted by optional paths).

### Fix approach

1. **Scope-aware naming**: Track which variables are in which scope. Variables inside if/else blocks don't affect the parent scope's counter.
2. **Or**: Make `gen_field_assignments` use the same `name_counts` instance that the parsing code used, instead of replaying from scratch.
3. **Or**: Wrap all non-field parsing (groups, optionals, prediction branches) in labeled scope blocks (`__scope: { ... }`) and use fresh `name_counts` inside. This isolates variable names to their scope.

Approach 3 was partially implemented but incomplete — optional paths that share elements with the enclosing alt (e.g., `LIMIT expr ((OFFSET|',') expr)?` where `expr` appears both in the alt and inside the optional) need the parent's dedup counter to produce `expr_2` for the second occurrence.

A complete fix likely requires combining approaches: scope blocks for isolation + parent counter awareness for shared element types.

## Incomplete CST walker coverage

The XML unparse output is missing tokens/nodes for several patterns due to `store=false` and non-simple groups:

- **Repeated separators**: `(',' result_column)*` — the `','` and subsequent `result_column` are not stored, so `SELECT a, b` only shows `a`.
- **Token-only groups**: `(K_INSERT | K_REPLACE)` — groups with only TokenRef alternatives are not `is_simple_cst_group` (requires RuleRef alternatives), so `INSERT` keyword is missing from `insert_stmt`.
- **Multi-element single-alt groups**: `(';'+ sql_stmt)*` — the Star inner group has two elements (Plus and RuleRef), not a simple CST group, so second statements are not stored.

Fixing these requires either:
1. Extending `is_simple_cst_group` to handle token-only and mixed groups (generating variant types for tokens too).
2. Or making `gen_repeat` for Star/Plus store all inner elements (requires struct types for group alternatives with multiple elements).
