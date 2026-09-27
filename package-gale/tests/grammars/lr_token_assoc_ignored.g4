// ANTLR4 issue #4841: `<assoc=right>` on a token reference is the pre-4.2
// spelling. `doc/left-recursion.md` says it is still accepted but ignored, so
// only the alternative-level option makes `^` right-associative.
grammar LrTokenAssocIgnored;

s : e EOF ;

e
  : e '^'<assoc=right> e
  | <assoc=right> e '**' e
  | INT
  ;

INT : [0-9]+ ;
WS : [ \t\r\n]+ -> skip ;
