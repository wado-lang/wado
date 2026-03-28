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

`ArrayRefIter<T>` cannot implement `Iterator` because the monomorphizer **eagerly instantiates all default methods** (collect, map, filter, fold, etc.) for every `T`, causing OOM on large programs like gale itself.

### Fix path

```
1. Lazy monomorphization of trait default methods
   (only instantiate methods that are actually called)
     ↓
2. ArrayRefIter<T> implements Iterator trait
     ↓
3. CST Group fix can use `for let item of &node.group_list`
     ↓
4. SQLite parser's to_tree() test passes
```

### Workaround (if lazy monomorphization is too large)

Change the walker to use value iteration (`for let item of node.group_list` without `&`). This copies array elements but avoids the `&Array<T>` IntoIterator issue. The walker already works with value semantics for non-Group fields.

## Cross-Module Type Identity (Loader)

When a sub-module imports the entry module back (e.g., `use { Shape } from "../entry.wado"`), the loader creates two `ModuleSource` identities for the same file: `EntryPoint` and `Local`. This causes duplicate type definitions in the WIR.

Test: `wado-compiler/tests/fixtures/cross_module_type_identity.wado` (TODO-marked)

Partial fix in `loader.rs` (entry canonical name check) but resolver symbol lookup needs work.
