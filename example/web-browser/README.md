# web-browser — One Program, Run in the Browser

`src/main.wado` greets whoever the page's input names, through the `web:dom`
API. In a browser, `package-web`'s glue answers those calls from the page's own
DOM. Under `wado test`, `SurfaceDom` answers them instead.

```sh
mise run web-browser-demo   # build, then serve on http://127.0.0.1:8089
mise run test-web-glue      # run it on Node against jsdom
wado test                   # run its tests against SurfaceDom
```

`build.sh` compiles the program, transpiles it with jco, and bundles it with
jco's browser shims into `build/app.js`, which `index.html` loads. The browser
needs JSPI and Wasm GC, as Chromium 137 and later have.

The demo serves this directory with `wado serve` and
`example/static_server.wado`, a static file server written in Wado. From the
repository root:

```sh
wado serve --addr 127.0.0.1:8089 --dir example/web-browser example/static_server.wado
```
