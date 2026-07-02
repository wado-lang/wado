grammar T;
a : LETTERS {System.out.println(_input.getText());} ;
LETTERS : 'a' ~('b')+ 'c';
