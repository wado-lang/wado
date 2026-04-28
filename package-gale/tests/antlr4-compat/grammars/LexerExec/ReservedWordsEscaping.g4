lexer grammar L;

channels { break }

A: 'a' -> mode(for);

mode for;
B: 'b' -> channel(break);
