# Gale TODO

## CST Group Fix

CST Group support (storing `(rule_a | rule_b)*` results in the parse tree) is implemented but blocked by a compiler limitation.

### What's done

- **generator.wado**: Group variant type generation (`SqlStmtListOrErrorGroup` etc.)
- **parser_gen.wado**: Parser stores Group results in variant fields
- **visitor_gen.wado**: Walker generates match dispatch for Group variants
- **All code is tested and works** when compiled standalone

### What's blocked

The walker generates `for let item of &node.group_list` which iterates `&Array<GroupVariant>`. This requires `IntoIterator for &Array<T>` → `ArrayRefIter<T>` → `Iterator` trait impl.

`ArrayRefIter<T>` now implements `Iterator` (e2e tests pass), but the monomorphizer **eagerly instantiates all default methods** (collect, map, filter, fold, etc.) for every `T`, causing OOM when compiling large programs like gale itself.

### Fix path

```
1. Lazy monomorphization of trait default methods
   (only instantiate methods that are actually called)
     ↓
2. Gale compiles without OOM (ArrayRefIter<T> already implements Iterator)
     ↓
3. CST Group fix can use `for let item of &node.group_list`
     ↓
4. SQLite parser's to_tree() test passes
```

### Workaround (if lazy monomorphization is too large)

Change the walker to use value iteration (`for let item of node.group_list` without `&`). This copies array elements but avoids the `&Array<T>` IntoIterator issue. The walker already works with value semantics for non-Group fields.
