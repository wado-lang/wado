# web-browser — One Program, Run in the Browser

`src/main.wado` greets whoever the page's input names, through the `web:dom`
API. In a browser, `package-web`'s glue answers those calls from the page's own
DOM. Under `wado test`, `SurfaceDom` answers them instead.

```sh
mise run web-browser-demo   # build, then serve on http://127.0.0.1:8089
mise run test-web-glue      # run it on Node against a DOM stub
wado test                   # run its tests against SurfaceDom
```

`build.sh` compiles the program, transpiles it with jco, and bundles it with
jco's browser shims into `build/app.js`, which `index.html` loads. The browser
needs JSPI and Wasm GC, as Chromium 137 and later have.
