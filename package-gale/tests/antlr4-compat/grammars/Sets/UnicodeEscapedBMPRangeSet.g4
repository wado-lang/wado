grammar T;
a : LETTERS* 'd' {System.out.println(_input.getText());} ;
// Note the double-backslash to avoid Java passing
// unescaped values as part of the grammar.
LETTERS : ('a'|'\u00E0'..'\u00E5');
