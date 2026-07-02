grammar T;
a : LETTERS {System.out.println(_input.getText());} ;
// These are actually not escaped -- Java passes the
// raw unescaped Unicode values to the grammar compiler.
LETTERS : ('a'|'ä'|'亜'|'あ')* 'c';
