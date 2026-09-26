// A rule that matches exactly calling one that folds case. The call matches
// every case the callee does, so the lexer must try the caller on each of them.
grammar ci_rule_callee;

s : (A | B | Q)+ EOF ;

A : B 'x' ;

B options { caseInsensitive = true; } : 'qr' ;

Q : 'qz' ;
