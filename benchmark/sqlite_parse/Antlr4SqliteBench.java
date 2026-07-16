// ANTLR4 Java SQLite parser benchmark: the runtime reference for Gale's
// generated parser — same SQLite.g4, same queries.sql, on the JVM.
// SQLiteLexer/SQLiteParser are generated at build time (see antlr4_java_bench.mjs),
// not vendored. Throughput auto-calibrates to ~1s like the other harnesses.
// Runs default full-context LL: SLL bails on this grammar, so LL is ANTLR4's
// best case here.

import org.antlr.v4.runtime.CharStreams;
import org.antlr.v4.runtime.CommonTokenStream;
import java.nio.file.Files;
import java.nio.file.Paths;

public class Antlr4SqliteBench {
    static void parseOnce(String sql) {
        SQLiteLexer lexer = new SQLiteLexer(CharStreams.fromString(sql));
        lexer.removeErrorListeners();
        SQLiteParser parser = new SQLiteParser(new CommonTokenStream(lexer));
        parser.removeErrorListeners();
        parser.parse();
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.err.println("usage: Antlr4SqliteBench <queries.sql>");
            System.exit(1);
        }
        String sql = new String(Files.readAllBytes(Paths.get(args[0])), "UTF-8");
        int size = sql.getBytes("UTF-8").length;

        // Warm the JIT and ANTLR's DFA cache to steady state (per-parse time
        // flattens after ~50 parses); a time and iteration floor covers any machine.
        long warmUntil = System.nanoTime() + 8_000_000_000L;
        long warmIters = 0;
        while (System.nanoTime() < warmUntil || warmIters < 60) {
            parseOnce(sql);
            warmIters++;
        }

        final long target = 1_000_000_000L; // ~1s measured window
        long n = 1, elapsed = 0;
        while (true) {
            long start = System.nanoTime();
            for (long i = 0; i < n; i++) {
                parseOnce(sql);
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
        System.out.printf("antlr4-java (parse): %.2f MB/s   (%.3f ms/iter, %d iter)%n",
            rate / 1e6, msPerIter, n);
    }
}
