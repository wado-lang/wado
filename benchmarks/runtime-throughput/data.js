window.BENCHMARK_DATA = {
  "lastUpdate": 1780920768697,
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
          "id": "76894dbe9659fb56ee610198633fbeece078bcbf",
          "message": "Merge pull request #1269 from wado-lang/claude/inspiring-curie-KrRCP\n\nrefactor(elaborator): finish Stage 7-A and complete Stage 5 (reify is the sole TIR producer)",
          "timestamp": "2026-06-02T21:32:04+09:00",
          "tree_id": "56e7057ec6044e8c9b6b865dcbf41f828aa1726a",
          "url": "https://github.com/wado-lang/wado/commit/76894dbe9659fb56ee610198633fbeece078bcbf"
        },
        "date": 1780404044981,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.74,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.31,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.58,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.62,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 222.56,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 88.69,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 150.56,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.4,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.41,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.59,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.75,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.05,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 75.64,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 224.57,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.85,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.26,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.87,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.7,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.27,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.85,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.64,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 39.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 77.29,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 228.62,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 94.03,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 145.44,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.99,
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
          "id": "18abc74f67be5e48623d33c00c9428dda83cf49a",
          "message": "Merge pull request #1270 from wado-lang/claude/compassionate-ride-8NYul\n\noptimizer(niri): smarter field-env tracking + dead-bounds-check elimination",
          "timestamp": "2026-06-02T22:22:09+09:00",
          "tree_id": "58544874ebfa7e99b13eecff09f0ee182ebe85ec",
          "url": "https://github.com/wado-lang/wado/commit/18abc74f67be5e48623d33c00c9428dda83cf49a"
        },
        "date": 1780407079703,
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
            "value": 35.23,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.98,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.06,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.28,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 229.08,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 83.53,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 156.08,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.16,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.35,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.74,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.26,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.89,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.66,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 232.94,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.32,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.53,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.71,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.86,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.24,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.31,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 36.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.88,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 233.52,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.63,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 150.8,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.93,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.49,
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
          "id": "dd7a5d611ea2de3d9afce4baa308f128719148a7",
          "message": "Merge pull request #1271 from wado-lang/claude/wasmtime-vendor-drift-Ne3nH\n\nfix(hooks): reconcile vendor/wasmtime to pinned commit every session",
          "timestamp": "2026-06-03T01:09:01+09:00",
          "tree_id": "5ce2b77bb2894e5de7e808b4b6126efd63a09a48",
          "url": "https://github.com/wado-lang/wado/commit/dd7a5d611ea2de3d9afce4baa308f128719148a7"
        },
        "date": 1780416950751,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 10.68,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 7.56,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 109.79,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 12.61,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 34.81,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 82.6,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 386.96,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 150.21,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 293.01,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 9.86,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 4.44,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 10.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 7.57,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 245.94,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 15.64,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 65.83,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 123.72,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 386.94,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 158.48,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 284.19,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 12.17,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 5.32,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 10.77,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 7.57,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 249.61,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 15.15,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 65.95,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 124.12,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 385.65,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 159.67,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 284.25,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 9.21,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 4.72,
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
          "id": "733eb4c174d2c39208251bdc18152266e8bd11d5",
          "message": "Merge pull request #1272 from wado-lang/claude/wir-build-refactor-WniKE\n\nrefactor(wir_build): tabulate mechanical builtin lowering and drop redundant literal copies",
          "timestamp": "2026-06-03T01:09:55+09:00",
          "tree_id": "175aa63dc2cef8866760be1dc06692991a92a8fc",
          "url": "https://github.com/wado-lang/wado/commit/733eb4c174d2c39208251bdc18152266e8bd11d5"
        },
        "date": 1780417207217,
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
            "value": 37.06,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.66,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.75,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 224.19,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 90.11,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 150.73,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.39,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.42,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.69,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.74,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.32,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.45,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 224.38,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 92.13,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 146.15,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.82,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.99,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 141.29,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.72,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.31,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 77.18,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 230.82,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.29,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.22,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.97,
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
          "id": "14a9dcdd127bbf22f6eb6d01286f6996c011bdfd",
          "message": "Merge pull request #1274 from wado-lang/claude/arxiv-wado-compiler-review-5DGc1\n\nRicher type/trait diagnostics: symmetric operator errors and trait-bound reason chains",
          "timestamp": "2026-06-03T02:53:28+09:00",
          "tree_id": "46506745d802a85b280f3baf0d1c56069bedeb47",
          "url": "https://github.com/wado-lang/wado/commit/14a9dcdd127bbf22f6eb6d01286f6996c011bdfd"
        },
        "date": 1780423340388,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 36.93,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.45,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.56,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.14,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 223.8,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 83.46,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 141.94,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.31,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.36,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 138.84,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.77,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 38.59,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.29,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 222.16,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.17,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 136.51,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.73,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.9,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.37,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.73,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.2,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 77.14,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 226.46,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 88.25,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 140.78,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.95,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.5,
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
          "id": "1aa1cce364d64343fce6e5c139c49ea15c294dc9",
          "message": "Merge pull request #1275 from wado-lang/claude/wado-compiler-parser-resilience-LhfDT\n\nfeat: make the wado-compiler parser error-recovering",
          "timestamp": "2026-06-03T06:39:53+09:00",
          "tree_id": "a88373c7128129064689de553b66ab66f38d6c87",
          "url": "https://github.com/wado-lang/wado/commit/1aa1cce364d64343fce6e5c139c49ea15c294dc9"
        },
        "date": 1780436908254,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.65,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.04,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.47,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.63,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.66,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 227.47,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 86.46,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 149.91,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.42,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.43,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.29,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.76,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.46,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.72,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 225.03,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.98,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 144.8,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.79,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.01,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.53,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.68,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.52,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 77.1,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 230.3,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.25,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.13,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.99,
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
          "id": "cb89a669878f0f53806c81de97ea7f462e0bba5a",
          "message": "Merge pull request #1273 from wado-lang/claude/gale-single-lowering-uZZOj\n\nLower once on gale gen; surface prediction diagnostics on the Kiln path",
          "timestamp": "2026-06-03T06:47:03+09:00",
          "tree_id": "b7454c445f973c1617902d580602b913e59b0c9e",
          "url": "https://github.com/wado-lang/wado/commit/cb89a669878f0f53806c81de97ea7f462e0bba5a"
        },
        "date": 1780437282973,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.73,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 6.21,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 43.08,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 8.9,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 23.27,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 58.27,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 293.25,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 102.04,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 200.38,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 6.67,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 3.04,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 198.26,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 11.59,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 47.77,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 94.38,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 301.1,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 115.52,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 194.8,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 8.7,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.81,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 198.43,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 11.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 48.1,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 96.19,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 303.46,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 116.65,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 195.22,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 6.08,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 3.26,
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
          "id": "87e1faa6ab6a2b1daded9b6bf52c930b4a137eb8",
          "message": "Merge pull request #1276 from wado-lang/claude/constant-object-globalization-gBQo3\n\nPromote constant struct/array globals to eager Wasm constants",
          "timestamp": "2026-06-03T08:45:54+09:00",
          "tree_id": "d06fc9af97471ddb198a8d87a17543e81abe789f",
          "url": "https://github.com/wado-lang/wado/commit/87e1faa6ab6a2b1daded9b6bf52c930b4a137eb8"
        },
        "date": 1780444475729,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 36.36,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.52,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.42,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.34,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 223.91,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 89.95,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 151.4,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.41,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.41,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.71,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.77,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.53,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.87,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 226.77,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.41,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 145.79,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.8,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.97,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.57,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.73,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.51,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 77.06,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 232.87,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 95,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.7,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.97,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.48,
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
          "id": "d6820624faa6c5f5b6f13e1da4f726d4297cba06",
          "message": "Merge pull request #1277 from wado-lang/claude/elaborator-refactor-stage-7-d81g5\n\nelaborator: Stage 7-B — make the call / operator surface record-only",
          "timestamp": "2026-06-03T09:25:29+09:00",
          "tree_id": "3280d3dede0b37db124556e010b5e5d74fa191da",
          "url": "https://github.com/wado-lang/wado/commit/d6820624faa6c5f5b6f13e1da4f726d4297cba06"
        },
        "date": 1780446843655,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.62,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.42,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.46,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.4,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.35,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 225.44,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 90.63,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 151.12,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.41,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.41,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.96,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.77,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.51,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.7,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 227.94,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 90.96,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 145.39,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.8,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.02,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.68,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.31,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.73,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.32,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 77.23,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 233.04,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 93.25,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.27,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.01,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "a2c9a3f60dd01dd7a8799cd9c00afb691b0be271",
          "message": "Merge pull request #1278 from wado-lang/claude/builtin-array-redesign-z3zaY\n\nRename growable Array&lt;T&gt; → List&lt;T&gt; (Phase 0) + builtin::array redesign WEP",
          "timestamp": "2026-06-03T13:59:33+09:00",
          "tree_id": "616bae377dd633be0faa249df9c1ef9b8b2cc28a",
          "url": "https://github.com/wado-lang/wado/commit/a2c9a3f60dd01dd7a8799cd9c00afb691b0be271"
        },
        "date": 1780463289682,
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
            "value": 33.29,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.09,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.09,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.29,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 228.32,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 87.63,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 157.13,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.16,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.35,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.95,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.26,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.12,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.73,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 233.28,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.07,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.01,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.6,
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
            "value": 154.17,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.3,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.32,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.91,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 233.26,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 90.54,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 149.25,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.97,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.52,
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
          "id": "8daad84c370ad9c8a33537695c36e041230b67a1",
          "message": "Merge pull request #1279 from wado-lang/claude/cool-bell-LntfL\n\nfeat(compiler): builtin::array_* take &/&mut references (WEP Phase 1)",
          "timestamp": "2026-06-03T18:25:40+09:00",
          "tree_id": "c9446ff89db92ed384fdf49d1dc4a811202b2ac1",
          "url": "https://github.com/wado-lang/wado/commit/8daad84c370ad9c8a33537695c36e041230b67a1"
        },
        "date": 1780479257555,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.64,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.53,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.66,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.66,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.82,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 163.37,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 69.87,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 120.88,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.41,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.41,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 141.08,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.25,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.26,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.63,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 225.03,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.35,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.88,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.88,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.97,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 141.27,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.29,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 76.95,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 226.45,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.37,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.09,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.99,
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
          "id": "dea5f64ccdd6f5ceb7099be9a7110390232aa385",
          "message": "Merge pull request #1280 from wado-lang/claude/unobservable-effects-question-zsgA1\n\nfeat(effect): add #[benign(E)] for observationally-pure effects",
          "timestamp": "2026-06-03T21:36:01+09:00",
          "tree_id": "897b22f7034499d2fce400fd2129b3aa2e976d04",
          "url": "https://github.com/wado-lang/wado/commit/dea5f64ccdd6f5ceb7099be9a7110390232aa385"
        },
        "date": 1780490681273,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.68,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 34.12,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.16,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.09,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.38,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 159.54,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 67.13,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 117.56,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.17,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.35,
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
            "value": 153.8,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.13,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.77,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 223.72,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 90.23,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 150.08,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.56,
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
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.02,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.43,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.76,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.97,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 233.38,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 91.92,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 155.23,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.97,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.52,
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
          "id": "20e2b0c1d28f99f54c4c2d4e4ccdee36a25b9db7",
          "message": "Merge pull request #1281 from wado-lang/claude/sroa-refactor-smells-R4luC\n\nrefactor(sroa): migrate hand-written IR walks to visitor traits",
          "timestamp": "2026-06-03T22:31:35+09:00",
          "tree_id": "fce1b539075c066b37b39b502ee22347a78d57f6",
          "url": "https://github.com/wado-lang/wado/commit/20e2b0c1d28f99f54c4c2d4e4ccdee36a25b9db7"
        },
        "date": 1780494034158,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.27,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.48,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.74,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.51,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.7,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 162.13,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 71.91,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 118.95,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.35,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.36,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.67,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.76,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.44,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.79,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 226.15,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.84,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 144.48,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.79,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.84,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.41,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 39.36,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 76.87,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 227.29,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.69,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 142.24,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.9,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.52,
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
          "id": "6b572de873222d779a79c84e0d884d5763556aad",
          "message": "Merge pull request #1283 from wado-lang/claude/global-const-optimization-MI5xy\n\nConstant object globalization + eager short-string globals + const-global dedup",
          "timestamp": "2026-06-04T05:27:51+09:00",
          "tree_id": "4bd086297e8692a5f851c251fb4413195fe83938",
          "url": "https://github.com/wado-lang/wado/commit/6b572de873222d779a79c84e0d884d5763556aad"
        },
        "date": 1780519009419,
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
            "value": 33.86,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.06,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.23,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 158.84,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 67.05,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 118.43,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.18,
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
            "value": 153.38,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.25,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.27,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.01,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 226.64,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.38,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 148.87,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.75,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.74,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.82,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.02,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.33,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.67,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 75.01,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 217.35,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 87.16,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 136.06,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.79,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.36,
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
          "id": "394169b08986e967cc45e808e77b99b5dc428ce8",
          "message": "Merge pull request #1285 from wado-lang/claude/uuid-stdlib-versions-iZRZ2\n\nfeat(stdlib): add core:uuid (v4/v7), with two effect/SIMD compiler fixes",
          "timestamp": "2026-06-04T06:28:19+09:00",
          "tree_id": "b972632895ed48cee4c14b706ae43279c0b8fefc",
          "url": "https://github.com/wado-lang/wado/commit/394169b08986e967cc45e808e77b99b5dc428ce8"
        },
        "date": 1780522621681,
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
            "value": 33.22,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.26,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.12,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.29,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 159.11,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 67.47,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 118.18,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.16,
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
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.81,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.11,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.12,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.04,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 229.68,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.31,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 148.34,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.78,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.94,
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
            "value": 153.97,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.35,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.69,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.64,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 228.61,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.55,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 150.79,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.98,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.52,
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
          "id": "2c4813fd9dd50731ae2aa9051bf30a2581b69bb7",
          "message": "Merge pull request #1282 from wado-lang/claude/builtin-array-redesign-jpsvC\n\nfeat(compiler): expose raw GC array as first-class `Array<T>` (WEP Phase 2)",
          "timestamp": "2026-06-04T06:37:32+09:00",
          "tree_id": "771d2ddf2facf9aaed010ce22e7d02dbcadd05bc",
          "url": "https://github.com/wado-lang/wado/commit/2c4813fd9dd50731ae2aa9051bf30a2581b69bb7"
        },
        "date": 1780523297504,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.61,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.36,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.35,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.61,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.2,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 157.38,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 70.17,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 118.23,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.38,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.43,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.72,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.74,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.91,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.77,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 224.5,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 90.77,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.9,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.76,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.99,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.7,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 135.84,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.81,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.51,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.88,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 223.19,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 93.16,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 147.77,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "d8e39c3f381369373bd53de52303bab0736c02e6",
          "message": "Merge pull request #1284 from wado-lang/claude/sha256-wado-impl-PZS00\n\nSHA-256 example + core:digest, and inherent/trait impls on concrete generic instantiations",
          "timestamp": "2026-06-04T07:51:30+09:00",
          "tree_id": "fd2dffc6afa2c15f40e61036e49a87ecff912703",
          "url": "https://github.com/wado-lang/wado/commit/d8e39c3f381369373bd53de52303bab0736c02e6"
        },
        "date": 1780527601148,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.65,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.04,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.73,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.71,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.34,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 159.56,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 70.02,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 118.34,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.38,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.42,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 141.12,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.77,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.41,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 77.56,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 222.01,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 92.73,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 145.38,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.73,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.01,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.96,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.81,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.98,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 75.3,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 224.68,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 94.7,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 146.75,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.49,
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
          "id": "47fde7d305731c7e65528e3a924551c34ce8359e",
          "message": "Merge pull request #1286 from wado-lang/claude/arxiv-agentic-coding-papers-FLUvz\n\nOptimizer remarks: surface residual value-semantic copies for coding agents",
          "timestamp": "2026-06-04T09:56:44+09:00",
          "tree_id": "818758bbaaddb4ef683d83cdacb4248fc5cf1624",
          "url": "https://github.com/wado-lang/wado/commit/47fde7d305731c7e65528e3a924551c34ce8359e"
        },
        "date": 1780535114937,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.57,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.26,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 36.97,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.66,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.58,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.15,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 160.38,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 70.52,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 116.39,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.4,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.44,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.65,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.87,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.07,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 77.18,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 227.11,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 92.89,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 146.31,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.82,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.03,
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
            "value": 140.67,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.79,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.62,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.79,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 227.56,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 94.28,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.97,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.03,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "5a1aeaa3157bbd4413a39249892f89af3632c571",
          "message": "Merge pull request #1287 from wado-lang/claude/builtin-array-redesign-oJr0M\n\nUnified zero-copy Slice/iterators over &Array<T> (builtin-array-redesign Phase 3)",
          "timestamp": "2026-06-04T19:06:23+09:00",
          "tree_id": "8ae53969b770dfae5d8318d21dfcc7bbefb1ed20",
          "url": "https://github.com/wado-lang/wado/commit/5a1aeaa3157bbd4413a39249892f89af3632c571"
        },
        "date": 1780568113774,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.36,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.79,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.59,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.76,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 163.33,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 69.75,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 120.87,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.39,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.43,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.65,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 141.06,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.76,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.79,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 77.1,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 220.24,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 92.59,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.12,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.75,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.94,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.68,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.57,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.8,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.52,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.77,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 224.54,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 93.05,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.69,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "33783ac00c7dc48d278391ee92ee97aa5310d12f",
          "message": "Merge pull request #1290 from wado-lang/claude/wado-g4-development-hN8py\n\nGale: parser trace instrumentation + sharper parse errors",
          "timestamp": "2026-06-04T21:40:46+09:00",
          "tree_id": "ec3f4ee1aa5e6a25d66b4d6c5d28fff57af2bbcf",
          "url": "https://github.com/wado-lang/wado/commit/33783ac00c7dc48d278391ee92ee97aa5310d12f"
        },
        "date": 1780577359494,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 36.78,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.78,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.63,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.76,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 166.02,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 72.35,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 120.85,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.39,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.41,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.95,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.78,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.13,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 77.18,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 221.58,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 93.33,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 144.58,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.8,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.99,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.43,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.77,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.76,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.87,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 225.6,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 94.96,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.43,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.98,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "08b79a7b27885ae4f951f277d9d79268500bb880",
          "message": "Merge pull request #1289 from wado-lang/claude/body-globalization-gate-refactor-yF8mj\n\nrefactor(optimizer): unify NIR traversals behind NirRefVisitor; harden const-globalization",
          "timestamp": "2026-06-05T04:14:53+09:00",
          "tree_id": "6cd40a8d4b98358580959256e2554c6f418225ee",
          "url": "https://github.com/wado-lang/wado/commit/08b79a7b27885ae4f951f277d9d79268500bb880"
        },
        "date": 1780601303991,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 36.73,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.94,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.05,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 163.72,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 72.35,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 122.28,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.42,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.44,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.2,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.71,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.59,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.33,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.89,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 225.11,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 93.06,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 146.35,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.82,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.01,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.7,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.24,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.8,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.9,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 75.21,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 227.97,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 88.55,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 146.28,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.03,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.57,
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
          "id": "310e55aeb00cae2106d122720655e3ad077b8bb4",
          "message": "Merge pull request #1291 from wado-lang/claude/wado-item-deref-PNDdk\n\nfeat: iterate references by reference in for-of, including tuples",
          "timestamp": "2026-06-05T06:05:20+09:00",
          "tree_id": "16fecb939573cbb918964b50b979545390074114",
          "url": "https://github.com/wado-lang/wado/commit/310e55aeb00cae2106d122720655e3ad077b8bb4"
        },
        "date": 1780607648805,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.74,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 35.4,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.15,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.38,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.71,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 164.48,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 65.71,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 121.2,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.14,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.29,
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
            "value": 153.76,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.25,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.25,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 73.04,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 229.63,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.52,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 148.92,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.82,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.92,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.8,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.59,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.16,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.11,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 73.45,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 228.47,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 88.54,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 151.56,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.94,
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
          "id": "f493160e2f99a0dd4f51b9bd9801912779be7ed1",
          "message": "Merge pull request #1292 from wado-lang/claude/agents-md-review-VsBsD\n\ndocs: clarify the development test cycle and tidy AGENTS.md",
          "timestamp": "2026-06-05T07:40:34+09:00",
          "tree_id": "24fe1989f3ac2a2cde76d838e3630cfaefa5c2ab",
          "url": "https://github.com/wado-lang/wado/commit/f493160e2f99a0dd4f51b9bd9801912779be7ed1"
        },
        "date": 1780613346832,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.15,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.73,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.87,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.01,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 165.8,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 71.68,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 120.33,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.4,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.41,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.65,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.77,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.73,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 77.01,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 224.42,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 91.99,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 144.31,
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
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.68,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.8,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.65,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.85,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 223.54,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 93.93,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 147.4,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.95,
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
          "id": "b3f0f5a9d0f720ae8c123ddaad6436c8a0929e1b",
          "message": "Merge pull request #1294 from wado-lang/claude/unlikely-branch-cost-ey5Xj\n\nAdd builtin::cold_path branch-hint intrinsic",
          "timestamp": "2026-06-05T09:11:26+09:00",
          "tree_id": "d25796259275ba880a1abfb6bc6d2c4d7e005120",
          "url": "https://github.com/wado-lang/wado/commit/b3f0f5a9d0f720ae8c123ddaad6436c8a0929e1b"
        },
        "date": 1780618873705,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.58,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 35.23,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.32,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.81,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.43,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 163.68,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 70.98,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 122.03,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.37,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.44,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.64,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.6,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.74,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.86,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 77.02,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 223.52,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.27,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 144.56,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.8,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.96,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.68,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.77,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.85,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.3,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.79,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 223.3,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.64,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 142.82,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.01,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.57,
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
          "id": "02dfcc5b9c151809f7790b00539aeb40de215362",
          "message": "Merge pull request #1293 from wado-lang/claude/elaborator-refactoring-wep-UfxWY\n\nelaborator(7-B): combined walk stops building TIR — reify becomes the sole producer (Phase 1)",
          "timestamp": "2026-06-05T09:47:26+09:00",
          "tree_id": "bce061e27d9463c407da181e8fec41e2c012e9e2",
          "url": "https://github.com/wado-lang/wado/commit/02dfcc5b9c151809f7790b00539aeb40de215362"
        },
        "date": 1780620973809,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.43,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.8,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.85,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.9,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 162.08,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 70.39,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 119.16,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.39,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.43,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 138.84,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.76,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.9,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.87,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 222.8,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.49,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.5,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.83,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.98,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.83,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.84,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.49,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.91,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 223.18,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 88.42,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 144.55,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.03,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "e3d41c49efdc109625696a177b633ca165114c19",
          "message": "Merge pull request #1295 from wado-lang/claude/wizardly-darwin-lW1Rt\n\nrevert(ci): drop the tagpr CHANGELOG-format workaround",
          "timestamp": "2026-06-05T15:14:52+09:00",
          "tree_id": "6d3bce449fccab1fc86cb1d7b8aeb80288ff8807",
          "url": "https://github.com/wado-lang/wado/commit/e3d41c49efdc109625696a177b633ca165114c19"
        },
        "date": 1780640606180,
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
            "value": 33.15,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.28,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.02,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.66,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 164.59,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 66.96,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 120.83,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.1,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.35,
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
            "value": 153.81,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.21,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.31,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 72.81,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 230.41,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.7,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 148.78,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.77,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.95,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.62,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.4,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.35,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 73.58,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 231.33,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.16,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 149.34,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.05,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "4f8d2c2b5865da1ba321ff14ed0a95a34bbac8e4",
          "message": "Merge pull request #1297 from wado-lang/claude/wado-syntax-highlight-perf-auJuj\n\nperf(compiler): speed up debug-build compilation (~20% on wado_syntax_highlight)",
          "timestamp": "2026-06-05T19:04:57+09:00",
          "tree_id": "e9f3090ae51d39424c4eb1a9148c4fa77e0e6ae0",
          "url": "https://github.com/wado-lang/wado/commit/4f8d2c2b5865da1ba321ff14ed0a95a34bbac8e4"
        },
        "date": 1780654418049,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.63,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.61,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.79,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.81,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.9,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 165.29,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 69.54,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 120.44,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.41,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.41,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.27,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 138.64,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.78,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.04,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 76.94,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 225.29,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 93,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.42,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.85,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.99,
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
            "value": 139.86,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.85,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.4,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.8,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 226.6,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.74,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 145.1,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.02,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.57,
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
          "id": "bdc129b1d58a924aeda8d113f7b006aafc699495",
          "message": "Merge pull request #1299 from wado-lang/claude/jso-hello-wado-test-VUo1l\n\nfix(jco): run example/hello.wado on upstream jco b1f93c27",
          "timestamp": "2026-06-05T20:12:58+09:00",
          "tree_id": "69c32f850a8f1982d147d14785c7984bcfaca49f",
          "url": "https://github.com/wado-lang/wado/commit/bdc129b1d58a924aeda8d113f7b006aafc699495"
        },
        "date": 1780658505738,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.77,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 33.05,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.12,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.4,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.72,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 165.34,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 66.51,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 120.98,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.14,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.34,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.77,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.74,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.25,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.3,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 73.24,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 229.14,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.77,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 150.84,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.83,
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
            "value": 153.41,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.41,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.22,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 73.51,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 226.08,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.54,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 139.53,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.06,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.56,
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
          "id": "a91e8bae1b19cb79a6e499f1b526bb1882095656",
          "message": "Merge pull request #1301 from wado-lang/claude/elaborator-refactor-stage-7-tOL8R\n\nrefactor(compiler): elaborator stage 7-B — annotate records facts, reify is the sole TIR producer",
          "timestamp": "2026-06-05T23:17:54+09:00",
          "tree_id": "7eed374a439c8e73b94ce7e310a6f5305320ed8c",
          "url": "https://github.com/wado-lang/wado/commit/a91e8bae1b19cb79a6e499f1b526bb1882095656"
        },
        "date": 1780669734991,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.74,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 32.81,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.1,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.04,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 44.57,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 163.95,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 64.57,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 118.41,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.12,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.33,
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
            "value": 153.74,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.33,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.14,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 73.06,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 229.06,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 86.44,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.33,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.74,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.92,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.82,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.86,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.03,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 73.44,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 229.69,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 87.67,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 149.96,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.04,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.48,
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
          "id": "86cc513cf9ef667cb243264f897340366905a31b",
          "message": "Merge pull request #1302 from wado-lang/claude/wado-array-copy-option-o143c\n\nfeat(codegen): lower array.copy to the native Wasm instruction, add generic -f flag",
          "timestamp": "2026-06-05T23:24:21+09:00",
          "tree_id": "8dc2f7ae6b5093e6b4ad5f54a5e300229798c520",
          "url": "https://github.com/wado-lang/wado/commit/86cc513cf9ef667cb243264f897340366905a31b"
        },
        "date": 1780670427129,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.61,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.95,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.83,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.73,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 53.72,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 173.73,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 70.38,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 119.42,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.57,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.55,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.18,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.63,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.46,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.27,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 100.51,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 247.39,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 91.57,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 145,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7.02,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.2,
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
            "value": 139.62,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 7.29,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.37,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 102.26,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 243,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 91.78,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 147.21,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.23,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.7,
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
          "id": "788d2f0ad38c06cac159466ea89f4750930327b2",
          "message": "Merge pull request #1300 from wado-lang/claude/core-cbor-design-pPn6m\n\ncore:cbor design + byte-buffer/serde groundwork",
          "timestamp": "2026-06-05T23:51:56+09:00",
          "tree_id": "e2f8ed760436a67f0dcc2b6a28f7f8cec3dd4b81",
          "url": "https://github.com/wado-lang/wado/commit/788d2f0ad38c06cac159466ea89f4750930327b2"
        },
        "date": 1780671692658,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.62,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.53,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.79,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.77,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 53.54,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 173.61,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 69.95,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 117.24,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.49,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.48,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.64,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.44,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.5,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.15,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 100.2,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 244.85,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 90.38,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.1,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.83,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.11,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.63,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.83,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.65,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 102.54,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 241.02,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.12,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 143.16,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.09,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.63,
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
          "id": "b3ed65540004a77bd260b4b9689bc9c354a40179",
          "message": "Merge pull request #1303 from wado-lang/claude/benchmark-workflow-html-Yf8ip\n\nImprove benchmark dashboard HTML and workflow",
          "timestamp": "2026-06-06T00:20:48+09:00",
          "tree_id": "74a9b60c6f326ed7bd50d6901194351632b95619",
          "url": "https://github.com/wado-lang/wado/commit/b3ed65540004a77bd260b4b9689bc9c354a40179"
        },
        "date": 1780673361408,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.64,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.27,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.66,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.8,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.83,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 53.82,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 171.27,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 73.59,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 119.45,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.53,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.51,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.95,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.55,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.07,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 99.91,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 245.04,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 86.32,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.48,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.88,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.12,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.24,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.83,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.77,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 102.06,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 242.46,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 94.57,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 146.5,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.09,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.69,
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
          "id": "de7067ce5ef607b2624068507b19391e1db7a0c9",
          "message": "Merge pull request #1307 from wado-lang/claude/benchmark-workflow-graph-scroll-LaP4c\n\nfix(benchmark): fixed header/legend/axis on combined benchmark pages",
          "timestamp": "2026-06-06T07:27:55+09:00",
          "tree_id": "b4a3d68a2cf58c82224e96b5f69f8d514412fac1",
          "url": "https://github.com/wado-lang/wado/commit/de7067ce5ef607b2624068507b19391e1db7a0c9"
        },
        "date": 1780699003179,
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
            "value": 34.18,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.24,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.46,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 53.31,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 167.86,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 66.89,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 121.66,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.27,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.48,
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
            "value": 153.6,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.22,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.65,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 99.8,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 251.9,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.85,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.16,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.99,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.07,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.82,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.82,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.92,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.4,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.84,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 100.04,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 250.52,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.6,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 157.15,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.18,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.68,
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
          "id": "1194f10083642454a0d6eb6c628cef985f147fce",
          "message": "Merge pull request #1306 from wado-lang/claude/cross-module-struct-literal-PT0nv\n\ncompiler: fix cross-module struct-literal default synthesis (#1263)",
          "timestamp": "2026-06-06T07:55:55+09:00",
          "tree_id": "559b53424df1d149a2498bb3bb21c6132e304507",
          "url": "https://github.com/wado-lang/wado/commit/1194f10083642454a0d6eb6c628cef985f147fce"
        },
        "date": 1780700804216,
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
            "value": 32.97,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.16,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.46,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 53.25,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 172.08,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 67.91,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 121.59,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.32,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.48,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.83,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.97,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.24,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.59,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 99.5,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 255.16,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 85.71,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.94,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7.05,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.09,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.78,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.97,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.38,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 38.21,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 100.33,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 251.83,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.2,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 150.72,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.22,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.69,
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
          "id": "aab4d044143477f668a3a10d4f16b2630e94f79c",
          "message": "Merge pull request #1304 from wado-lang/claude/atn-grammars-prerequisites-ZLxTW\n\ngale: refresh TODO — correct stale Stage B' status, tighten prose",
          "timestamp": "2026-06-06T08:21:52+09:00",
          "tree_id": "7d21d6e7806a0f55b007acc2a58683236531968e",
          "url": "https://github.com/wado-lang/wado/commit/aab4d044143477f668a3a10d4f16b2630e94f79c"
        },
        "date": 1780702225491,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.24,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 62.73,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.24,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 21.33,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 60.55,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 172,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 66.14,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 123.54,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.56,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.52,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.24,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 123.83,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 10.07,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 35.93,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 92.61,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 246.25,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 79.42,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 151.24,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.88,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.04,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.2,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 126.77,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 10.29,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 35.96,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 92.64,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 241.65,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 85.29,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 149.98,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.17,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.74,
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
          "id": "d3d2fb33719dc3c3b568553d976550384fd222b5",
          "message": "Merge pull request #1305 from wado-lang/claude/wado-elaborator-method-resolution-agsx4\n\nelaborator: index-driven, cache-free method resolution (+ cross-module inherent fix)",
          "timestamp": "2026-06-06T08:53:29+09:00",
          "tree_id": "40c6beb72b6258b0bf3df11ea80f019bb88e97fc",
          "url": "https://github.com/wado-lang/wado/commit/d3d2fb33719dc3c3b568553d976550384fd222b5"
        },
        "date": 1780704113811,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.21,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.27,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 62.52,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 21.33,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 60.6,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 171.65,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 66.42,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 122.08,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.61,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.43,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.24,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 123.1,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 10.07,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 35.78,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 92.1,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 245.74,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 74.95,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 148.79,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.82,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.05,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.19,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 126.44,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 10.28,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 36,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 92.7,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 242.77,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 85.76,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 147.8,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.25,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.74,
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
          "id": "c8a446da1fec45866b8d1b3a81d57f0c22cefe6d",
          "message": "Merge pull request #1310 from wado-lang/claude/robust-ns-imports-OlAeg\n\nMake namespace imports robust across all symbol kinds",
          "timestamp": "2026-06-06T13:13:31+09:00",
          "tree_id": "3aa0ec2134b6dd25d644e5ca4db767d2c259e85b",
          "url": "https://github.com/wado-lang/wado/commit/c8a446da1fec45866b8d1b3a81d57f0c22cefe6d"
        },
        "date": 1780719717229,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.65,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.26,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.78,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.87,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 52.97,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 168.68,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 72.64,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 121.35,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.52,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.53,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.76,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.39,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 101.72,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 248.01,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 91.58,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 143.98,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.91,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.15,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.95,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.84,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 41.31,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 100.78,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 241.53,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.59,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 144.11,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.2,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.68,
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
          "id": "d8e41d62270549438f1f6728e659e104579a419e",
          "message": "Merge pull request #1311 from wado-lang/claude/wir-optimize-peephole-wlpej\n\nfeat(wir-optimize): Wasm-instruction-level peephole rewrites",
          "timestamp": "2026-06-06T13:34:24+09:00",
          "tree_id": "e6014c71b5dadb760cf32e892f4d544311bbde26",
          "url": "https://github.com/wado-lang/wado/commit/d8e41d62270549438f1f6728e659e104579a419e"
        },
        "date": 1780720985451,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.76,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 33.64,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.13,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.37,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 52.77,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 172.84,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 66.45,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 118.74,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.3,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.49,
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
            "value": 153.31,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.47,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 99.36,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 253.2,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 85.87,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.82,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7.11,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.17,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.82,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.43,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.37,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 38.43,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 100.3,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 248.58,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 92.43,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 153,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.17,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.7,
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
          "id": "a04807a712b835400cdb6df0d8a5d1eed1b43295",
          "message": "Merge pull request #1309 from wado-lang/claude/cbor-wep-setup-ytwND\n\nBytes-primary serde groundwork for core:cbor (+ P0 codegen fix)",
          "timestamp": "2026-06-06T14:06:42+09:00",
          "tree_id": "02149bf712e303600b4351e96ba77662be4e5dca",
          "url": "https://github.com/wado-lang/wado/commit/a04807a712b835400cdb6df0d8a5d1eed1b43295"
        },
        "date": 1780722934820,
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
            "value": 34.12,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.36,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 52.67,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 110.13,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 54.78,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 97.02,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.28,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.4,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.82,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.43,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.19,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.41,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 99.19,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 202.66,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 83.81,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 156.64,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7.08,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.15,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.8,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.94,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 38.35,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 100.27,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 205.72,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 84.8,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 159.45,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.23,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.69,
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
          "id": "1f315e841dcd9d086785a0d14ecefb78722f6108",
          "message": "Merge pull request #1312 from wado-lang/claude/tc39-temporal-spec-72fcb\n\nfeat(core:temporal): Instant and ZonedDateTime with formatting, parsing, and serde",
          "timestamp": "2026-06-06T14:49:36+09:00",
          "tree_id": "fdc629a0f3c81a01e408ef2781308f6df5d9ff13",
          "url": "https://github.com/wado-lang/wado/commit/1f315e841dcd9d086785a0d14ecefb78722f6108"
        },
        "date": 1780725419324,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.6,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 43.45,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.9,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 23.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 68.03,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 145.23,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 72.86,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 131.28,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 6.86,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 3.22,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 6.23,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 198.47,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 11.56,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 48.44,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 127.82,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 262.77,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 111.17,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 218.47,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 9.09,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.97,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 6.23,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 193.69,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 11.25,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 48.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 128.71,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 265.56,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 107.56,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 217.46,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 6.73,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 3.47,
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
          "id": "b3921cb519f66edd45fcc7ce6e218956eb8cd12d",
          "message": "Merge pull request #1313 from wado-lang/claude/benchmark-wasm-size-data-8SbVo\n\nfix(benchmark): render wasm-size graphs and serialize gh-pages writes",
          "timestamp": "2026-06-06T16:15:46+09:00",
          "tree_id": "fd7572ac9287d4ca5fe6af4614eba595bf01e0ed",
          "url": "https://github.com/wado-lang/wado/commit/b3921cb519f66edd45fcc7ce6e218956eb8cd12d"
        },
        "date": 1780730654297,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.47,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 39.56,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.75,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.67,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 52.74,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 115.99,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 63.47,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 101.93,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.51,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.55,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.25,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.14,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 102.32,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 194.22,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 91.41,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 165.83,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.93,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.19,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.04,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 41.64,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 99.49,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 198.26,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 91.45,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 160.18,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.15,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.68,
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
          "id": "b17e0b2e1288df678644c320cea29328284d851a",
          "message": "Merge pull request #1314 from wado-lang/claude/elaborator-coverage-gaps-ZJLqT\n\nelaborator/monomorphize: coverage cleanup, soundness audit, and variadic for-of P0 fixes",
          "timestamp": "2026-06-06T17:23:25+09:00",
          "tree_id": "5a187107e5796756708fb935e436e78b62cbfd72",
          "url": "https://github.com/wado-lang/wado/commit/b17e0b2e1288df678644c320cea29328284d851a"
        },
        "date": 1780734649665,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 6.19,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 42.51,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.95,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 23.58,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 67.7,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 144.67,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 72.9,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 125.93,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 6.85,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 3.13,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 6.21,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 198.25,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 11.55,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 48.18,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 128.09,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 262.44,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 110.72,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 218.98,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 9.11,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 4.09,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.78,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 198.03,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 11.29,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 49.35,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 129.37,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 264.99,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 113.88,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 215.9,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 6.69,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 3.48,
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
          "id": "f36249639d2515fa6df1347ba1d08640275e335c",
          "message": "Merge pull request #1316 from wado-lang/claude/zlib-decompress-perf-XAtB2\n\nperf(zlib): speed up inflate by removing redundant sliding window",
          "timestamp": "2026-06-06T19:25:08+09:00",
          "tree_id": "7eb7025fa308d0f6d2c7dae7f3168cf336516d37",
          "url": "https://github.com/wado-lang/wado/commit/f36249639d2515fa6df1347ba1d08640275e335c"
        },
        "date": 1780742011480,
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
            "value": 32.77,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.08,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.44,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 124.95,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 108.62,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 56.42,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 99.93,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.32,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.49,
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
            "value": 153.91,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.24,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.6,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 213.37,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 198.69,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 87.87,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 168.95,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7.09,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.17,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.82,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.95,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.38,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 38.49,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 217.81,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 205.79,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.27,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 165.39,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.21,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.69,
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
          "id": "4bbdb04bbcab285c0dda49fdd1738299858250de",
          "message": "Merge pull request #1315 from wado-lang/claude/nir-rewrite-engine-feasibility-3M2nf\n\nperf(nir): run the optimizer on the arena Body, removing the per-pass tree bridge",
          "timestamp": "2026-06-06T20:09:32+09:00",
          "tree_id": "ec365ecd5eb29a94d95e4ef2eb2fbe4a0463fbe8",
          "url": "https://github.com/wado-lang/wado/commit/4bbdb04bbcab285c0dda49fdd1738299858250de"
        },
        "date": 1780744735231,
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
            "value": 33.35,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.14,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.41,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 125.67,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 112.75,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 56.21,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 94.89,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.32,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.47,
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
            "value": 153.6,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 8.73,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.59,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 211.34,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 201.98,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 86.78,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 167.33,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7.1,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.18,
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
            "value": 154.15,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.38,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 38.38,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 216.87,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 201.12,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 88.33,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 166.9,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.24,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.71,
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
          "id": "55e6bcf99b0009815c6c87080b2ef5da549e03ca",
          "message": "Merge pull request #1317 from wado-lang/claude/iso8601-core-temporal-QiAyO\n\nfeat(core:temporal): bridge wasi:clocks Instant with From, fixing cross-module same-name dispatch",
          "timestamp": "2026-06-06T20:58:00+09:00",
          "tree_id": "ab9acfc4c2166aab99b4f48ae515a2e3178f00f7",
          "url": "https://github.com/wado-lang/wado/commit/55e6bcf99b0009815c6c87080b2ef5da549e03ca"
        },
        "date": 1780747522848,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 6.23,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 42.58,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 8.05,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 23.77,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 160.99,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 145.05,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 72.68,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 130.78,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 6.84,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 3.21,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 198.31,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 11.55,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 48.27,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 272.55,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 263.74,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 111.15,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 217.95,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 9.16,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 4.08,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.78,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 198,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 11.28,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 49.58,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 279.07,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 264.84,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 114.21,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 216.64,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 6.76,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 3.47,
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
          "id": "0b274004a83084d46bddce88e465af68d66cbd13",
          "message": "Merge pull request #1318 from wado-lang/claude/wado-lsp-token-type-HSfLI\n\nLSP: classify semantic tokens by resolved symbol kind; unify keyword/operator registry",
          "timestamp": "2026-06-06T21:47:37+09:00",
          "tree_id": "70bce8a4a51956b2344e114db15a174b25c9fe8e",
          "url": "https://github.com/wado-lang/wado/commit/0b274004a83084d46bddce88e465af68d66cbd13"
        },
        "date": 1780750564775,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.59,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.49,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.76,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.89,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 135.75,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 118.08,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 64.2,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 99.38,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.58,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.54,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.51,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.8,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.41,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 223.95,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 197.57,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 92.74,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 166.07,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.2,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 141.08,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 41.83,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 229.52,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 197.22,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 91.82,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 161.8,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.2,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.7,
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
          "id": "36b1c5948831eb021faf5fc7128d82aaa50c1446",
          "message": "Merge pull request #1319 from wado-lang/claude/remove-builtin-array-qunaQ\n\nRemove legacy `builtin::array` type spelling in favor of `Array<T>`",
          "timestamp": "2026-06-06T23:52:26+09:00",
          "tree_id": "6b525c2fac70e07e4ba48f20dc979a47631be733",
          "url": "https://github.com/wado-lang/wado/commit/36b1c5948831eb021faf5fc7128d82aaa50c1446"
        },
        "date": 1780758070106,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 37.35,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.68,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.48,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 134.8,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 117.27,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 59.77,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 101.5,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.53,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.53,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.71,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.36,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 221.45,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 194.81,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.19,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 162.28,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.96,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.17,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.3,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.83,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 39.94,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 225.77,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 196.64,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 83.64,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 151.76,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.09,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.67,
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
          "id": "8d5d5b7551caf44ac08656886132370b2542dc8c",
          "message": "Merge pull request #1321 from wado-lang/claude/builtin-likely-unlikely-review-DSklW\n\nRemove builtin::likely/unlikely; consolidate on builtin::cold_path",
          "timestamp": "2026-06-07T01:40:15+09:00",
          "tree_id": "04bc0cdea345d47d1b9cad184d9ffbd30214dd46",
          "url": "https://github.com/wado-lang/wado/commit/8d5d5b7551caf44ac08656886132370b2542dc8c"
        },
        "date": 1780764541450,
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
            "value": 32.31,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.21,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.44,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 125.61,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 112.97,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 56.55,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 102.57,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.3,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.48,
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
            "value": 153.76,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 8.78,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 37.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 212.58,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 206.92,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 85.5,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 174.1,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 7.08,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.14,
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
            "value": 153.99,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 38.42,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 217.73,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 207.28,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 87.91,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 170.97,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.21,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.69,
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
          "id": "1d50cb3991bc73362d34a1c72ba752a1afa2d872",
          "message": "Merge pull request #1320 from wado-lang/claude/ll-prediction-gaps-lJDuj\n\ngale: thread the LL FOLLOW repair at runtime instead of cloning callees",
          "timestamp": "2026-06-07T06:04:55+09:00",
          "tree_id": "231a87718720c2fb16dcf63b5202c554db3bb118",
          "url": "https://github.com/wado-lang/wado/commit/1d50cb3991bc73362d34a1c72ba752a1afa2d872"
        },
        "date": 1780780442617,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 29.28,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.78,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.81,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 134.07,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 112.86,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 61.05,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 100.35,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.24,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.52,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.36,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.75,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 40.12,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 218.31,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 200.6,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.2,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 164.84,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.13,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.96,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.48,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.86,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 41.4,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 226.99,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 198.9,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 90.91,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 161.43,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.07,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.68,
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
          "id": "7016eb1e61b70a8a5a9182a512111fd8823c99d2",
          "message": "Merge pull request #1322 from wado-lang/claude/benchmark-json-catalog-perf-jABaw\n\nopt: speed up JSON deserialization via HFS deferred write-back and LICM arithmetic hoisting",
          "timestamp": "2026-06-07T09:42:25+09:00",
          "tree_id": "6797821a0637e88a4cc97131ba16c75ed58254a9",
          "url": "https://github.com/wado-lang/wado/commit/7016eb1e61b70a8a5a9182a512111fd8823c99d2"
        },
        "date": 1780793431557,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.73,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 43.49,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 7.92,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 23.74,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 162.94,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 150.34,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 70.84,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 130.58,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.29,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 3.18,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 198.29,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 11.52,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 47.46,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 276.75,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 267.09,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 107.94,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 218.38,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.62,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.72,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.75,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 6.24,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 196.42,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 11.23,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 48.21,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 280.97,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 267.41,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 106.24,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 215.44,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 6.56,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 3.43,
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
          "id": "6a78b8c2678874972eb9705da341d4cde33bd684",
          "message": "Merge pull request #1323 from wado-lang/claude/package-gale-perf-2LFOa\n\nGale: shrink generated parsers ~10% (parse-entry helpers + single-token CST compaction)",
          "timestamp": "2026-06-07T11:55:13+09:00",
          "tree_id": "55682a1fd6174ac3b2bdd1fb653303dfe2e1e80e",
          "url": "https://github.com/wado-lang/wado/commit/6a78b8c2678874972eb9705da341d4cde33bd684"
        },
        "date": 1780801464547,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.62,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.62,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.79,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.85,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 136.02,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 121.41,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 61.21,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 104.92,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.23,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.52,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 139.22,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.75,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 224.56,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 199.82,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 89.05,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 164.12,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.03,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.95,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.55,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.81,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 39.78,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 227.95,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 200.65,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 86.29,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 157.11,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.06,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.63,
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
          "id": "87daa297226c346af82c1a52d1d28b445ad04855",
          "message": "Merge pull request #1324 from wado-lang/claude/worklist-rewrite-engine-GQdMD\n\nrefactor(nir): migrate the NIR pipeline from the tree to the arena (Phase 5)",
          "timestamp": "2026-06-07T14:00:32+09:00",
          "tree_id": "a701ef2b54be46d5214d83c5250e131fc0bb5a79",
          "url": "https://github.com/wado-lang/wado/commit/87daa297226c346af82c1a52d1d28b445ad04855"
        },
        "date": 1780808950682,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.24,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 63.35,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.18,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 21.48,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 140.89,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 127.13,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 62.59,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 111.35,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.35,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.58,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.24,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 123.7,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 10.03,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 35.39,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 215.58,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 197.59,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 82.88,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 170.7,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.03,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.89,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.2,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 127.09,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 10.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 35.49,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 214.68,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 193.35,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 87.47,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 165.79,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.16,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.72,
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
          "id": "69537e9be2ecb660dd794274805f9d5a9053af44",
          "message": "Merge pull request #1325 from wado-lang/claude/nir-rewrite-engine-wep-gNSsE\n\nrefactor(nir): retire the NIR tree representation (arena-only)",
          "timestamp": "2026-06-07T19:09:59+09:00",
          "tree_id": "9c3f8b5d09407bd6720d6decc00e4e84a5f87aa0",
          "url": "https://github.com/wado-lang/wado/commit/69537e9be2ecb660dd794274805f9d5a9053af44"
        },
        "date": 1780827547011,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.8,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 33.44,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.35,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.42,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 126.99,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 116.92,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 54.31,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 103.25,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.12,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.44,
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
            "value": 153.09,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.24,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.95,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 216.94,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 207.86,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 83.74,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 173.61,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.1,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.97,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.82,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.06,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.42,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.62,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 220.03,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 207.5,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 84.71,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 166.53,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.13,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.69,
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
          "id": "77a93b5e4dccc67cf3b016e9040c82892ebe1c86",
          "message": "Merge pull request #1326 from wado-lang/claude/wado-unused-diagnostics-x0CeD\n\nUnused diagnostics: Design-B semantic checks, reify gating, and 3-way liveness",
          "timestamp": "2026-06-07T20:56:01+09:00",
          "tree_id": "138ef34935055381a73cda9751ff31919e5a2d40",
          "url": "https://github.com/wado-lang/wado/commit/77a93b5e4dccc67cf3b016e9040c82892ebe1c86"
        },
        "date": 1780833901815,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 8.23,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 61.58,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.22,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 21.18,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 140.46,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 126.73,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 59.73,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 108.96,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.21,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.52,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 8.23,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.27,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 121.16,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 10.07,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 35.15,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 213.76,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 193.34,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 77.7,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 157.01,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 4.91,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.81,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 8.19,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.27,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 123.66,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 10.32,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 35.29,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 212.71,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 192.71,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 80.94,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 152.52,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.08,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.68,
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
          "id": "197ad6419e83a8ffe4b6d539e365d92a7139165d",
          "message": "Merge pull request #1330 from wado-lang/claude/core-cbor-wep-review-P7KOY\n\nfeat(stdlib): add core:cbor — RFC 8949 binary serialization",
          "timestamp": "2026-06-08T10:11:45+09:00",
          "tree_id": "8e5a547e4a991c3388b9831b9884ede4ee013c21",
          "url": "https://github.com/wado-lang/wado/commit/197ad6419e83a8ffe4b6d539e365d92a7139165d"
        },
        "date": 1780881700744,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.67,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.69,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.78,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.84,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 136.24,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 115.93,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 60.98,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 106.92,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.27,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.54,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.66,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.17,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.8,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 227.44,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 201.15,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 90.33,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 166.51,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.12,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 3.02,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.68,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 140.14,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.82,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 40.1,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 232.38,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 198.4,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 88.32,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 163.56,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.13,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.71,
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
          "id": "93ebe1d9a2ef2d2517d84ecd99261f53b14ad568",
          "message": "Merge pull request #1329 from wado-lang/claude/fix-astid-synth-collision-ice\n\nfix(compiler): attribute method-signature use→def edges to the owning module",
          "timestamp": "2026-06-08T10:51:50+09:00",
          "tree_id": "22e904e4ef380b124e1ae879d2709813cf48b128",
          "url": "https://github.com/wado-lang/wado/commit/93ebe1d9a2ef2d2517d84ecd99261f53b14ad568"
        },
        "date": 1780884059249,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.74,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 34.08,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.2,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.23,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 127.04,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 117.36,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 55.75,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 102.22,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.07,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.46,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.76,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.94,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.24,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.89,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 217.27,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 207.13,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 84.33,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 170,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.07,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.95,
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
            "value": 153.85,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.34,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.47,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 219.99,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 205.2,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 84.75,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 164.38,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.13,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.67,
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
          "id": "c55b346a787593d383bb03f399eccf1f14be7e3c",
          "message": "Merge pull request #1331 from wado-lang/claude/fix-partial-turbofish-ice\n\nfix(compiler): infer trailing type params on a partial turbofish",
          "timestamp": "2026-06-08T17:46:50+09:00",
          "tree_id": "bd34d41f061a537bfcf5e934e2fe7f37935b7455",
          "url": "https://github.com/wado-lang/wado/commit/c55b346a787593d383bb03f399eccf1f14be7e3c"
        },
        "date": 1780908959174,
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
            "value": 31.48,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.26,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.43,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 127.19,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 116.51,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 56.64,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 99.03,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.11,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.43,
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
            "value": 154.13,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.25,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.77,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 217.65,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 204.63,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 85.25,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 166.93,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.12,
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
            "value": 154.33,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.36,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.6,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 220.96,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 206.74,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 85.45,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 165.83,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.13,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.65,
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
          "id": "e06f3ff13a0bbae4f6d7591561680c30505066ed",
          "message": "Merge pull request #1332 from wado-lang/claude/nir-rewrite-engine-cont-sTf6r\n\nperf(optimize): worklist rewrite engine + per-function dirty-set gating",
          "timestamp": "2026-06-08T20:46:52+09:00",
          "tree_id": "50968db7a9bd3e36658a5437bc46e266a9061503",
          "url": "https://github.com/wado-lang/wado/commit/e06f3ff13a0bbae4f6d7591561680c30505066ed"
        },
        "date": 1780919880912,
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
            "value": 34.36,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.28,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.43,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 126.93,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 116.47,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 56.71,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 100.28,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.09,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.48,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.8,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.87,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.27,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.83,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 216.01,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 206.93,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 84.13,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 169.01,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.12,
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
            "value": 153.55,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.38,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.2,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 219.64,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 207.05,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 84.8,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 166.9,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.11,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.66,
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
          "id": "27dd6747566fd64ffa5e9da29c68f1f039084f24",
          "message": "Merge pull request #1335 from wado-lang/gfx/pr_skills\n\nchore: cleanup skills",
          "timestamp": "2026-06-08T20:59:20+09:00",
          "tree_id": "54994a1b77cf5c1ff44faaf87b67cd634dd4418d",
          "url": "https://github.com/wado-lang/wado/commit/27dd6747566fd64ffa5e9da29c68f1f039084f24"
        },
        "date": 1780920768289,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 7.61,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 38.61,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.71,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 17.56,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 132.53,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 117.81,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 58.83,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 96.35,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 4.2,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.46,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 7.64,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 5.28,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 140.33,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.83,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 39.57,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 224.76,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 197.72,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 82.55,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 150,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 5.02,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.96,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 7.69,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 5.29,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 139.24,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.77,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 39.69,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 228.44,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 199.98,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 81.64,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 146.11,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 5.07,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.62,
            "unit": "MB/s"
          }
        ]
      }
    ]
  }
}