# Gale TODO

Known gaps to close on the road to **full ANTLR4 compatibility**. The
compatibility principle is documented in [AGENTS.md](./AGENTS.md): the g4
parser must accept any well-formed grammar that upstream `antlr4` accepts,
with the single exception that host-language action bodies are skipped.

The g4 parser already accepts the full ANTLR4 surface syntax (with that
exception). The remaining work is mostly about **propagating** parsed
information into the IR and **using** it in the code generator so that
generated parsers are semantically correct, not just syntactically accepted.

## Performance: sqlite-parse Benchmark

### Remaining Bottleneck: `Parser::last_end` (11.7%)

| Function           | Self-time |
| ------------------ | --------- |
| `Parser::last_end` | 11.7%     |

Called after every token consumption to compute node spans via
`Span::new(start, p.last_end())`. The function itself is trivial (array
index), but at millions of calls the overhead accumulates.

Possible improvement: cache `last_end` in a field on `Parser` updated by
`advance()`, avoiding repeated array indexing.

### Resolved

- **Backtracking (~44%)**: Eliminated by scan-then-parse optimization.
  Lightweight scan functions check token kinds to pick the correct
  alternative before calling the real parse function once.
- **`Parser::expect` error path (~22%)**: Largely eliminated — scan
  functions avoid speculative parse failures that triggered error
  construction.

## Code Quality

### `parser_gen.wado`

- **Duplicated branch merge logic**: SLL prediction tree building has
  similar merge/dedup patterns in multiple places. Could be consolidated.

## Generated Parser Bugs

(none currently)
