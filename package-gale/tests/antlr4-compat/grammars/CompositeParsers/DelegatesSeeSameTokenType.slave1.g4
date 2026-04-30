parser grammar T;
tokens { C, B, A } // reverse order
y : A {<writeln("\"T.y\"")>};
