lexer grammar L;
BeginString
   :   '\'' -> more, pushMode(StringMode)
   ;
mode StringMode;
   StringMode_X : 'x' -> more;
   StringMode_Done : -> more, mode(EndStringMode);
mode EndStringMode;
   EndString : '\'' -> popMode;
