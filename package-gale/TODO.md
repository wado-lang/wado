# Gale TODO

## Code Quality

### parser_gen.wado

- **Duplicated branch merge logic**: SLL prediction tree building has similar merge/dedup patterns in multiple places. Could be consolidated.

### Tests

- **No negative test cases**: No tests for malformed `.g4` input (syntax errors, missing rules, duplicate rule names). Would improve robustness.
