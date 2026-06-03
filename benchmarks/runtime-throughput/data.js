window.BENCHMARK_DATA = {
  "lastUpdate": 1780446844201,
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
      }
    ]
  }
}