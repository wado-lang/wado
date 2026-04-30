grammar T;
a : 'a' x='b' {<Append("\"conjured=\"","$x"):writeln()>} 'c' ;
