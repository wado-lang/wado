// Source: Hand-authored for Gale's right-associative-operator coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Minimal left-recursive grammar that uses the `<assoc=right>` rule-element
// option on a single alternative (exponentiation `^`). ANTLR4 defaults to
// left associativity; `<assoc=right>` flips a single alternative. The
// distinguishing behavior:
//   - Default (left): `2 + 3 + 4` parses as `(2 + 3) + 4`.
//   - `<assoc=right>`: `2 ^ 3 ^ 4` parses as `2 ^ (3 ^ 4)`.
//
// Gale's `parser_gen.wado` has a distinct code path for right-associative
// alternatives (lifts `min_prec` to `own_prec` rather than `own_prec + 1`
// when recursing into the right operand), which no existing driver test
// exercises.

grammar right_assoc_gaps;

expr
    : <assoc=right> expr '^' expr  # Pow
    | expr '+' expr                # Add
    | INT                          # Int
    ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
