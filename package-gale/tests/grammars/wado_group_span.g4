// Source: Gale test fixture (Stage C `$x.text` / `$x.ctx` across a group)
// License: same as the Gale package
//
// The locals a labeled rule call captures are read from two places a group can
// separate them from: a rule prequel (`@after`, outside every alternative) and
// an action inside an optional group. Both are emitted in the same fn as the
// call, so both must find the locals declared.
grammar WadoGroupSpan;

options { language = Wado; }

whole @after { p.emit($r.text) } : ( r=a B ) ;

opt : ( r=a { p.emit($r.ctx) } )? B ;

// An `@init` runs before the call it reads, and an `@after` outside the loop
// that holds it: neither sees a local declared in the alternative's own scope.
before @init { p.emit($r.text) } : r=a ;

loop_span @after { p.emit($r.text) } : ( r=a B )+ ;

// Two groups deep, under a shape-lookahead optional.
deep @after { p.emit($r.text) } : ( ( r=a B )? C )? D ;

// A rule-span special (`$text` / `$start` / `$stop`) reads a local codegen
// declares only for a rule whose actions ask for one, so the walk that decides
// has to reach a group's alternatives as the translator's substitution does.
group_text : ( A { p.emit($text) } | B ) C ;

loop_text : ( A { p.emit($text) } )+ C ;

// A rule-call argument is translated against the caller, so it reaches a
// capture by the same substitution an action does.
arg_reader : r=a arg_taker[$r.text] ;

arg_taker [ String s ] : B { p.emit($s) } ;

a : A A ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
WS : ' ' -> skip ;
