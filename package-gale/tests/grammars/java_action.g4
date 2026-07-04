// Source: Gale test fixture (Stage C java2wado action execution)
// License: same as the Gale package
//
// A `language = Java` grammar (Java is the default) with print-style actions.
// java2wado translates the Java bodies (`System.out.print(...)` -> `p.emit(...)`)
// and they execute during the parse, landing in `ParseResult.output`.
grammar JavaAction;

r : A {System.out.print("hi");} B {System.out.print("!");} ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;
