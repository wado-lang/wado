grammar T;
a : t=~('x'|'y') 'z' {<writeln("$t.text")>} ;
