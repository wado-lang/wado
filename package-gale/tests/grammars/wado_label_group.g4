// Source: Gale test fixture (labelled transparent group)
// License: same as the Gale package
//
// `(e)` is a purely syntactic wrapper, so `x=(ID)` binds exactly what `x=ID`
// binds. The label has to land on the inner leaf — a transparent group has no
// binding slot of its own.
grammar WadoLabelGroup;

options { language = Wado; }

r : x=(ID) '+' y=(ID) { p.emit(`${$x.text}-${$y.text}`); } ;

ID : [a-z]+ ;
WS : ' ' -> skip ;
