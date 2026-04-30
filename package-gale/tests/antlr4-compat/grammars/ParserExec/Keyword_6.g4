grammar T;

program : reset*{<writeln("$text")>} EOF ;
reset: 'break;' | 'continue;' | 'return;' ;
