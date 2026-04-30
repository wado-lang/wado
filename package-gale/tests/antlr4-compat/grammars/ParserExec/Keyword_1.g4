grammar T;

program : state*{<writeln("$text")>} EOF ;
state: 'break;' | 'continue;' | 'return;' ;
