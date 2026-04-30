grammar T;

program : action*{<writeln("$text")>} EOF ;
action: 'break;' | 'continue;' | 'return;' ;
