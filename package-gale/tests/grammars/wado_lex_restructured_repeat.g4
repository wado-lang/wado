// Source: Gale test fixture (Stage C lexer action under a restructured repeat)
// License: same as the Gale package
//
// Three repeats the emitter restructures the sequence around, each leaving
// `count` at 2 when its actions run where they are written:
//
//   A  lookahead-aware: the scan overshoots and gives the chars back, so the
//      action runs for the two iterations kept, not the four tried.
//   D  non-greedy: the exit try is match-only, so the rest's action runs once —
//      from the iteration that ends the loop, not from every one attempted.
//   E  placement: the action sits between the repeat and the suffix, so it reads
//      the cursor where the repeat stopped, not where the token ends.
//
// `M` reads the count the run left behind and retypes itself, so the token the
// parser sees says whether the actions ran the right number of times.
grammar WadoLexRestructuredRepeat;

options { language = Wado; }

@lexer::members {
    count: i32 = 0
}

s : A M { p.emit("a-bad") }
  | A N { p.emit("a-ok") }
  ;

t : D M { p.emit("d-bad") }
  | D N { p.emit("d-ok") }
  ;

u : E M { p.emit("e-bad") }
  | E N { p.emit("e-ok") }
  ;

v : G M { p.emit("g-bad") }
  | G N { p.emit("g-ok") }
  ;

A : 'a' ( ~'b' { lx.count += 1 } )+ 'c' ;
D : 'd' ( ~'b' { lx.count += 1 } )*? ( { lx.count += 1 } 'c' ) ;
E : 'e' ~('b')+ { lx.count = pos - start } 'c' ;
// The exit try is a decision, and only the predicate keeps the loop past the
// first `'c'`: a replay that dropped it would end the loop two chars early and
// run the action at a cursor the match never stopped at.
G : 'g' .*? { pos > start + 2 }? 'c' { lx.count = pos - start - 2 } ;
M : 'm' { if lx.count == 2 { lx.set_type(TK_N) } } ;
N : 'n' ;
