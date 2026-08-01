// Source: Gale test fixture (labelled transparent group)
// License: same as the Gale package
//
// `(e)` is a purely syntactic wrapper, so `x=(ID)` binds exactly what `x=ID`
// binds and `y+=(ID)+` repeats exactly what `y+=ID+` repeats. The wrapper has
// no binding slot of its own: left in place it either drops the binding or
// leaves the lowered op and the surface element disagreeing about the shape,
// which used to abort codegen outright for the list form.
grammar WadoLabelGroup;

options { language = Wado; }

r : x=(ID) '+' y=(ID) { p.emit(`${$x.text}-${$y.text}`); } ;

s : items+=(ID)+ EOF ;

ID : [a-z]+ ;
WS : ' ' -> skip ;
