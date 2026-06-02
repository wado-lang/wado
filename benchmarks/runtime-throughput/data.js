window.BENCHMARK_DATA = {
  "lastUpdate": 1780396797891,
  "repoUrl": "https://github.com/wado-lang/wado",
  "entries": {
    "Benchmark": [
      {
        "commit": {
          "author": {
            "email": "g.psy.va@gmail.com",
            "name": "FUJI Goro",
            "username": "gfx"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "eab7ec5746d1fc1f9de0c2f28ff91c2e2ce1703c",
          "message": "Merge pull request #1264 from wado-lang/claude/crore-benchmark-metrics-GWhcl\n\nbenchmark: throughput metrics with ~1s auto-calibration",
          "timestamp": "2026-06-01T23:27:00+09:00",
          "tree_id": "f47630c3afdaaeaadba35a6e053c6bc28f2f59da",
          "url": "https://github.com/wado-lang/wado/commit/eab7ec5746d1fc1f9de0c2f28ff91c2e2ce1703c"
        },
        "date": 1780324546511,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 33.87,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.93,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.02,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.33,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 229.8,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 87.28,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 156.13,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.14,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.37,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.78,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.88,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.27,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.85,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.65,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 231.56,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 90.06,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.41,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.73,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.97,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.62,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.37,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 36.82,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.91,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 232.75,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.41,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 151.55,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.75,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.54,
            "unit": "MB/s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "g.psy.va@gmail.com",
            "name": "FUJI Goro",
            "username": "gfx"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "ccb0e8a95bd237a73b677e0b94921f6ee49ad3ce",
          "message": "Merge pull request #1265 from wado-lang/claude/crore-benchmark-metrics-GWhcl\n\ndocs: point the runtime benchmark link at the throughput series",
          "timestamp": "2026-06-02T06:53:56+09:00",
          "tree_id": "6ca28d3363b28411238980e0cc4e71cfebfc5786",
          "url": "https://github.com/wado-lang/wado/commit/ccb0e8a95bd237a73b677e0b94921f6ee49ad3ce"
        },
        "date": 1780351408252,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.78,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 36.49,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.87,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.05,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.34,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 230.42,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 85.46,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 156.79,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.17,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.38,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 154.15,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.26,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.78,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 233.97,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.94,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 150.82,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.79,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.93,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.13,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.38,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.24,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.87,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 233.01,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 90.48,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.63,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.95,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.55,
            "unit": "MB/s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "g.psy.va@gmail.com",
            "name": "FUJI Goro",
            "username": "gfx"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "925d9340e98d6f9e3bfeb9d0bd2f118dfc5add88",
          "message": "Merge pull request #1266 from wado-lang/claude/elaborator-refactor-stage-5-7-1jLGh\n\nelaborator: Stage 7-A — reify reads recorded signature facts instead of re-resolving",
          "timestamp": "2026-06-02T09:13:28+09:00",
          "tree_id": "3c2d45a0f0f9797f6c365a93aa13dda4f33c5bc9",
          "url": "https://github.com/wado-lang/wado/commit/925d9340e98d6f9e3bfeb9d0bd2f118dfc5add88"
        },
        "date": 1780359717185,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.63,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.36,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.65,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.8,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 226.1,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 89.67,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 152.71,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.39,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.44,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.3,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 142.07,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.79,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.41,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 75.99,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 226.89,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 93.18,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 128.15,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.87,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.91,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.7,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.6,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.64,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.3,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 77.51,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 229.28,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 96.12,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 147.84,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.01,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.58,
            "unit": "MB/s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "g.psy.va@gmail.com",
            "name": "FUJI Goro",
            "username": "gfx"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "995760ac85386aa9ff076689be89deabfbb3cc50",
          "message": "Merge pull request #1268 from wado-lang/claude/zealous-ritchie-DtczN\n\nfeat(gale): Stage B' — JVM-oracle-derived expected trees",
          "timestamp": "2026-06-02T19:27:01+09:00",
          "tree_id": "3d6d85b26b142653480856def68cb314757eb5c2",
          "url": "https://github.com/wado-lang/wado/commit/995760ac85386aa9ff076689be89deabfbb3cc50"
        },
        "date": 1780396549721,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.78,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 34.4,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.97,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.09,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.37,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 228.99,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 84.89,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 155.72,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.19,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.37,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.15,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.84,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.56,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 233.78,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.1,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 151.31,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.76,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.96,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 152.83,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.25,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.15,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 75.15,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 227.12,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.63,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 150.89,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.01,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.53,
            "unit": "MB/s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "g.psy.va@gmail.com",
            "name": "FUJI Goro",
            "username": "gfx"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "7793b34f6b85f7a3414c86a20c43dcc51cebf83a",
          "message": "Merge pull request #1267 from wado-lang/claude/inspiring-bohr-W63P4\n\nfeat(lexer): resilient tokenisation with bundled LexResult and per-error recovery",
          "timestamp": "2026-06-02T19:27:45+09:00",
          "tree_id": "2756ba2bf1f2d7217c4ba2b3b7068c73f30910a5",
          "url": "https://github.com/wado-lang/wado/commit/7793b34f6b85f7a3414c86a20c43dcc51cebf83a"
        },
        "date": 1780396797461,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 33.61,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.86,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.01,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.29,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 230.49,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 86.45,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 157.04,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.16,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.25,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.52,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.21,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.7,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.19,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 225.49,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.84,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 150.14,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.76,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.98,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.03,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.25,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.92,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 232.69,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.5,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 150.88,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.54,
            "unit": "MB/s"
          }
        ]
      }
    ]
  }
}