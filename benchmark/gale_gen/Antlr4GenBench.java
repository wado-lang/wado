// ANTLR4 `generate` benchmark: the generate-time reference for Gale, over the
// same RustLexer.g4 + RustParser.g4. Loops org.antlr.v4.Tool in-process, so the
// row measures generation rather than a cold JVM.

import org.antlr.v4.Tool;
import java.io.File;

public class Antlr4GenBench {
    static String[] toolArgs(String outDir, String[] grammars) {
        String[] args = new String[grammars.length + 4];
        args[0] = "-Dlanguage=Java";
        // Gale emits a parser only; without this ANTLR also writes listeners.
        args[1] = "-no-listener";
        args[2] = "-o";
        args[3] = outDir;
        System.arraycopy(grammars, 0, args, 4, grammars.length);
        return args;
    }

    static void generateOnce(String outDir, String[] grammars) {
        Tool tool = new Tool(toolArgs(outDir, grammars));
        tool.removeListeners();
        tool.processGrammarsOnCommandLine();
        if (tool.errMgr.getNumErrors() > 0) {
            throw new RuntimeException("antlr4: generation reported errors");
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: Antlr4GenBench <outDir> <grammar.g4>...");
            System.exit(1);
        }
        String outDir = args[0];
        String[] grammars = new String[args.length - 1];
        System.arraycopy(args, 1, grammars, 0, grammars.length);

        long size = 0;
        for (String g : grammars) {
            size += new File(g).length();
        }

        // The first generations run interpreted.
        long warmUntil = System.nanoTime() + 8_000_000_000L;
        long warmIters = 0;
        while (System.nanoTime() < warmUntil || warmIters < 20) {
            generateOnce(outDir, grammars);
            warmIters++;
        }

        final long target = 1_000_000_000L; // ~1s measured window
        long n = 1, elapsed = 0;
        while (true) {
            long start = System.nanoTime();
            for (long i = 0; i < n; i++) {
                generateOnce(outDir, grammars);
            }
            elapsed = System.nanoTime() - start;
            if (elapsed >= target) {
                break;
            }
            long next = n * target / Math.max(elapsed, 1);
            if (next <= n) {
                break;
            }
            n = Math.min(next, n * 100);
        }

        double rate = (double) size * n / (elapsed / 1e9);
        double msPerIter = elapsed / 1e6 / n;
        System.out.printf("antlr4 (generate): %.2f KB/s   (%.3f ms/iter, %d iter)%n",
            rate / 1e3, msPerIter, n);
    }
}
