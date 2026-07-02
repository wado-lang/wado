grammar T;
prog
@after {System.out.println($ctx.toStringTree(this));}
: statement* EOF {};
statement: letterA | statement letterA 'b' ;
letterA: 'a';
