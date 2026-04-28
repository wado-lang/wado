grammar T;
a : LETTERS {<InputText():writeln()>} ;
LETTERS : 'a' ~('b')+ 'c';
