grammar T;
a : LETTERS* 'd' {System.out.println(_input.getText());} ;
// These are actually not escaped -- Java passes the
// raw unescaped Unicode values to the grammar compiler.
LETTERS : ('a'|'à'..'å');
