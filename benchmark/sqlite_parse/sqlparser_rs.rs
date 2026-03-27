// Rust sqlparser-rs benchmark for SQLite parsing.
// Comparison baseline for Gale-generated SQLite parser.

use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;
use std::time::Instant;

fn main() {
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/queries.sql"
    ))
    .expect("Failed to read queries.sql");

    let iterations = 100;
    let dialect = SQLiteDialect {};

    println!(
        "sqlite-parse (sqlparser-rs): {} bytes, {} iterations",
        sql.len(),
        iterations
    );

    // Warm up
    let stmts = Parser::parse_sql(&dialect, &sql).expect("Parse error in warm-up");
    let stmt_count = stmts.len();

    let start = Instant::now();
    for _ in 0..iterations {
        let stmts = Parser::parse_sql(&dialect, &sql).expect("Parse error");
        assert_eq!(stmts.len(), stmt_count);
    }
    let elapsed = start.elapsed();

    let elapsed_us = elapsed.as_micros();
    let per_iter_us = elapsed_us / iterations as u128;

    println!("Parsed {} statements per iteration", stmt_count);
    println!(
        "Elapsed: {}.{:03} ms ({} iterations)",
        elapsed.as_millis(),
        elapsed.as_micros() % 1000,
        iterations
    );
    println!(
        "Per iteration: {}.{:03} us",
        per_iter_us,
        (elapsed_us * 1000 / iterations as u128) % 1000
    );
}
