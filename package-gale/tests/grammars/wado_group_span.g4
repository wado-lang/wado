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

a : A A ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;
