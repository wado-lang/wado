grammar T;
prog
@after {<ToStringTree("$ctx"):writeln()>}
: statement* EOF {};
statement: letterA | statement letterA 'b' ;
letterA: 'a';
