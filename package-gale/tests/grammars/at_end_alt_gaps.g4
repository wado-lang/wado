// Regression fixtures for the "at-end alt dropped from dispatch" bug.
//
// Pattern: a rule has alts that share a prefix, where one alt extends the
// shared prefix with optional additional elements:
//
//     typespec : ID         // alt 0 — at end after consuming ID
//              | ID '[' ']' // alt 1 — extends with '[' ']'
//              ;
//
// Before the fix, the predictor built a Dispatch for typespec at depth=1
// (after consuming ID) with only the LBRACKET branch (alt 1). Alt 0 was
// silently dropped because its FIRST set at depth=1 is empty (at-end), so
// the generated code committed to alt 1 whenever the next token was
// LBRACKET — even when LBRACKET legitimately belonged to the calling
// context (e.g. `new T[ <expr> ]`).
//
// After the fix, this case routes through Backtrack scan-dispatch: the
// scanner picks the alt whose body actually parses (longest match), so
// `(T)` picks alt 0 and `(T[])` picks alt 1, while `new T[1]` correctly
// picks alt 0 because alt 1's body `[ ]` cannot match `[ 1`.
grammar AtEndAltGaps;

prog : entry+ EOF ;

// Each `entry` exercises one of the three caller contexts:
//   * `(T)`        — paren caller, typespec must commit to alt 0
//   * `(T[])`      — paren caller with extension, typespec commits to alt 1
//   * `new T[1]`   — index caller, typespec must commit to alt 0 even though
//                     LBRACKET follows ID (alt 1's body would fail at `[1`)
entry : '(' typespec ')'
      | 'new' typespec '[' INT ']'
      ;

typespec
    : ID
    | ID '[' ']'
    ;

ID  : 'A'..'Z' ;
INT : '0'..'9'+ ;
WS  : (' ' | '\n') -> skip ;
