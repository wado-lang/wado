// Source: Hand-authored for Gale's mode/more coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Minimal grammar that exercises lexer-command features uncovered by the
// existing driver-test grammars — see TODO.md "Driver-level verification
// (remaining gaps)":
//
//   - A3: bare `mode(X)` command (replaces top of stack, unlike pushMode).
//   - A4: `more` semantics when the mode is entered via a lexer-command-only
//     `pushMode` (so no action block stands between the command and the
//     generated lexer state transition).
//
// The grammar models a tiny string literal with backslash escapes:
//   - `foo` (PLAIN) is tokenized directly.
//   - `"hello"` enters IN_STRING via `pushMode(IN_STRING)` and accumulates
//     each character with `more` until the closing quote emits STRING.
//   - `"hi\nthere"` switches temporarily to ESC_IN_STRING via `mode(...)`
//     to consume the escape target, then switches back via `mode(IN_STRING)`
//     — exercising the "replace top of stack" semantics.

grammar mode_gaps;

text : (PLAIN | STRING)* EOF ;

PLAIN : [a-zA-Z]+ ;

// A4: enter IN_STRING via pushMode from a lexer-command-only rule. The
// opening quote itself is merged into the next emitted token via `more`
// so the final STRING token contains the full literal.
QUOTE : '"' -> more, pushMode(IN_STRING) ;

WS : [ \t\r\n]+ -> skip ;

mode IN_STRING;

// A3: `mode(ESC_IN_STRING)` *replaces* the top of the mode stack, so when
// ESC_IN_STRING finishes it returns here via another `mode(IN_STRING)`
// rather than by popping. This is the distinguishing behavior of `mode(X)`
// versus `pushMode(X)` / `popMode`.
STR_ESC_BEGIN : '\\' -> more, mode(ESC_IN_STRING) ;

// A4: accumulate regular string chars into the pending STRING.
STR_CHAR : ~[\\"] -> more ;

// Emit the accumulated STRING token and return to the default mode.
STRING : '"' -> popMode ;

mode ESC_IN_STRING;

// Consume exactly one escape target (any char), accumulate it, and return
// to IN_STRING via `mode(IN_STRING)` — exercising the "switch" command.
ESC_CHAR : . -> more, mode(IN_STRING) ;
