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

Each of these fails loudly if dropped.

- **`--instantiation async`.** jco's default output initializes under a
  top-level await and fetches its core modules by URL; a Worker rejects both.
- **One instance per request.** A shared one answers a couple, then `handle`
  suspends on a stream read jco never drives (`JCO_DEBUG=1` shows
  `[StreamEnd#copy()] blocked`). Fixing that is jco's rendezvous, not this host.
- **`shims/http.js` reaches into `preview3-shim` by path.** Its `exports` map
  serves a Worker the browser build, whose every method throws `Todo`.
- **`ctx.waitUntil`.** A guest goes on writing after `task return` — `http_bin`
  logs its access line there — and returning ends that work unpumped.
- **`-f no-wide-arithmetic`.** No V8 implements the proposal that Wado's float
  formatting emits.

`shims/clocks.js` reads `Date.now()`, which a Worker advances only on I/O, and
its waits return at once.
