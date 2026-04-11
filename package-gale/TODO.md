# Gale TODO

## Code Quality

### parser_gen.wado

- **Duplicated branch merge logic**: SLL prediction tree building has similar merge/dedup patterns in multiple places. Could be consolidated.

### Tests

- **No negative test cases**: No tests for malformed `.g4` input (syntax errors, missing rules, duplicate rule names). Would improve robustness.

## Performance: sqlite-parse Benchmark

### Remaining Bottleneck: `Parser::last_end` (11.7%)

| Function           | Self-time |
| ------------------ | --------- |
| `Parser::last_end` | 11.7%     |

Called after every token consumption to compute node spans via `Span::new(start, p.last_end())`. The function itself is trivial (array index), but at millions of calls the overhead accumulates.

Possible improvement: cache `last_end` in a field on `Parser` updated by `advance()`, avoiding repeated array indexing.

### Resolved

- **Backtracking (~44%)**: Eliminated by scan-then-parse optimization. Lightweight scan functions check token kinds to pick the correct alternative before calling the real parse function once.
- **`Parser::expect` error path (~22%)**: Largely eliminated — scan functions avoid speculative parse failures that triggered error construction.

## Generated Parser Bugs

(none currently)
