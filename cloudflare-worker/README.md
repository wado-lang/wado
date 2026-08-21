# Wado on Cloudflare Workers

Serve a Wado `wasi:http/service` component from a Worker. `build.sh` compiles
the program and transpiles it with jco; `worker.mjs` bridges `fetch` to the
component's `handle`.

## Run

```sh
mise run jco-deps          # once, for the transpiler
npm install
./build.sh                 # defaults to ../example/http_bin.wado
npx wrangler dev
```

```sh
curl localhost:8787/get
curl localhost:8787/status/418
curl -X POST -d '{"a":1}' localhost:8787/post
```

`./build.sh path/to/service.wado` builds a different program. Deploy with
`npx wrangler deploy` once `wrangler.toml`'s `name` suits the account.

## What the platform requires

Four constraints shape this directory, each of which fails loudly if dropped.

- **`--instantiation async`.** jco's default output initializes under a
  top-level await and fetches its core modules by URL; a Worker rejects both
  (`Top-level await in module is unsettled`, `Invalid URL string`). The
  instantiation form hands the core modules in, and `wrangler.toml`'s
  `CompiledWasm` rule turns them into `WebAssembly.Module` imports.
- **One instance per request.** workerd gives each request its own I/O context
  while jco keeps async task state on the module, so a shared instance answers
  the first request and then stops settling.
- **`shims/http.js` reaches into `preview3-shim` by path.** Its `exports` map
  serves a Worker the browser build, whose every method throws `Todo`.
- **`-f no-wide-arithmetic`.** No V8 implements the proposal that Wado's float
  formatting emits.

`shims/clocks.js` reads `Date.now()`, which a Worker advances only on I/O — a
duration measured across pure computation reads as zero.

`shims/cli.js` sends the guest's stdout and stderr to `console`, but a write
issued **after** `task return` never arrives: the Worker returns as soon as
`handle` resolves, and the guest's remaining work goes unpumped. `http_bin`
logs its access line that way, so nothing of it reaches `wrangler tail`.
