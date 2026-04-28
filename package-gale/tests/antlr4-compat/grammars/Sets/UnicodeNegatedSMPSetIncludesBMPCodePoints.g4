grammar T;
a : LETTERS {<InputText():writeln()>} ;
LETTERS : 'a' ~('\\u{1F600}'..'\\u{1F943}')+ 'c';
