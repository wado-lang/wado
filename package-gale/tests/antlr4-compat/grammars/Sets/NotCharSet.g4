grammar T;
a : A {<writeln("$A.text")>} ;
A : ~('b'|'c') ;
