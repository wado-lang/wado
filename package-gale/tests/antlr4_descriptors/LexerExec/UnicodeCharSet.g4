lexer grammar L;
ID : ([A-Z_]|'Ā'..'\uFFFC') ([A-Z_0-9]|'Ā'..'\uFFFC')*; // FFFD+ are not valid char
