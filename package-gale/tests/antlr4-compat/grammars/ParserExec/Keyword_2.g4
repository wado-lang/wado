grammar T;

program : sempred*{<writeln("$text")>} EOF ;
sempred: 'break;' | 'continue;' | 'return;' ;
