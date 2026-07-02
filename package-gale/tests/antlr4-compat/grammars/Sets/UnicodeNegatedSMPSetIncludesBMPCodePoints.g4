grammar T;
a : LETTERS {System.out.println(_input.getText());} ;
LETTERS : 'a' ~('\u{1F600}'..'\u{1F943}')+ 'c';
