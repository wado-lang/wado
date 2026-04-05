# Gale TODO

## Code Quality

### parser_gen.wado

- **Duplicated branch merge logic**: SLL prediction tree building has similar merge/dedup patterns in multiple places. Could be consolidated.

### Tests

- **No negative test cases**: No tests for malformed `.g4` input (syntax errors, missing rules, duplicate rule names). Would improve robustness.

## Generated Parser Bugs

### TypeScript `>>>` (unsigned right shift) fails to parse

`= 1 >>> 2` fails because the LR overlap dispatch for `>` tries the `> >` (signed right shift) alternative via backtracking before `> > >`. The backtracking consumes `> >` and leaves the parser in a bad state. Needs greedy/longest-match handling for overlapping token sequences in LR dispatch.

### TypeScript nested ternary fails to parse

`= 1 ? 2 ? 3 : 4 : 5` produces "expected ':', got '?'". The ternary expression rule likely doesn't handle recursive nesting correctly in the generated parser.
