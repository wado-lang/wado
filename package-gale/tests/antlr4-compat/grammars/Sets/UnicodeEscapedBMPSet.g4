grammar T;
a : LETTERS {System.out.println(_input.getText());} ;
// Note the double-backslash to avoid Java passing
// unescaped values as part of the grammar.
LETTERS : ('a'|'\u00E4'|'\u4E9C'|'\u3042')* 'c';
