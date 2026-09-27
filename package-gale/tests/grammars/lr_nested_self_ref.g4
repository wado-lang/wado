// A self-reference nested inside a subrule of a left-recursive alternative.
// ANTLR4 rewrites only a direct trailing self-reference to `e[p]`; one inside
// a subrule stays a plain `e`, which is `e[0]`. It climbs every operator, and
// prediction decides where it stops: it leaves an operator to the enclosing
// alternative only when taking it would strand what that alternative still
// owes. So `ce`'s nested operand nests to the right, and `se`'s list operand
// stops before the `'>>'` its alternative needs, yet takes one when another
// `'>>'` is left over. A fixed precedence floor gets one of the two wrong.
grammar LrNestedSelfRef;

comma : ce EOF ;
ce : ce ',' (ce | INT) | INT ;

send : se EOF ;
se : se '*' se
   | se (',' se)* '>>' se
   | ID
   ;

plus : pe EOF ;
pe : pe ('+' pe)+ | ID ;

// Rust's `expression DOTDOT expression?`: an optional operand is nested too.
range : re EOF ;
re : re '..' re? | re '+' re | ID ;

INT : [0-9]+ ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
