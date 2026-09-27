// An LR suffix that admits every token and still has a first set.
//
// `e : e (w | A) | C` with `w : .` names `a` as its suffix's first token, but
// the `w` beside it matches any token, so the loop entry has to continue on
// every token, not only on `a`. The wildcard hides behind a rule reference
// (`w`, `v`, `n`), behind a nullable prefix (`v : B? .`, `k B? .`), or beside
// a named token in a group (`h (. | A)`); in each the static first set is a
// strict subset of what the suffix admits. `s4` puts the loop under a caller
// whose continuation `d` the suffix also admits, so only the full context
// tells the loop to stop. In `s7` the open suffix contests `d` with a suffix
// that names it, and wins where `D D` cannot complete; `s8` lists it first.
grammar LrOpenEndedRuleSuffix;

s : e EOF ;
e : e ( w | A ) | C ;
w : . ;

s2 : f EOF ;
f : f ( v | A ) | C ;
v : B? . ;

s3 : g EOF ;
g : g ( n | A ) | C ;
n : ~A ;

s4 : B e D EOF ;

s5 : h EOF ;
h : h ( . | A ) | C ;

s6 : k EOF ;
k : k B? . | C ;

s7 : m EOF ;
m : m D D | m w | C ;

s8 : q EOF ;
q : q w | q D D | C ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
