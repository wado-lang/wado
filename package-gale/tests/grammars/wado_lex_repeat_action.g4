// Source: Gale test fixture (Stage C lexer action under a repeat)
// License: same as the Gale package
//
// The action sits inside a `+`, so it runs once per iteration of the winning
// match — and never for a candidate that loses, or for an iteration that does
// not match. `B` reads the count the run left behind and retypes itself, so the
// token the parser sees says how many times the action ran.
grammar WadoLexRepeatAction;

options { language = Wado; }

@lexer::members {
    count: i32 = 0
}

s : A B { p.emit("ab") }
  | A C { p.emit("ac") }
  ;

t : D B { p.emit("db") }
  | D C { p.emit("dc") }
  ;

A : ( 'a' { lx.count += 1 } )+ ;
// The same action one level down, in a fragment the repeat inlines.
D : F+ ;
fragment F : 'd' { lx.count += 1 } ;
B : 'b' { if lx.count == 3 { lx.set_type(TK_C) } } ;
C : 'c' ;
