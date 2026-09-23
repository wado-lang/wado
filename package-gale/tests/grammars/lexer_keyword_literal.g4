// A keyword the classifier reclassifies out of a carrier's match is compared
// as written: `'Q'` and `'self'` / `'Self'` are case-sensitive, and a literal of
// any length is one keyword, not a sequence the dispatch has to try. `INNER`'s
// `TEXT` covers the same spans in a mode no keyword belongs to, so its match
// must never be reclassified. `HIDE` wins `do` from `ID` and hides it, but `DO`
// wins it from both, so `DO` is not hidden. `NUM` starts where `K1` does but
// cannot match all of it, so `K1` keeps its own matcher.
lexer grammar LexerKeywordLiteral;

AS : 'as' ;
SELF_TYPE : 'Self' ;
SELF_VALUE : 'self' ;
Q : 'Q' ;
DO : 'do' ;
HIDE : 'd' [a-z]* -> channel(HIDDEN) ;
ID : [a-zA-Z]+ ;
OPEN : '<' -> pushMode(INNER) ;
WS : ' ' -> skip ;
K1 : '1a' ;
NUM : [0-9]+ ;

mode INNER;
TEXT : [a-zA-Z]+ ;
CLOSE : '>' -> popMode ;
