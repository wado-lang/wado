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
