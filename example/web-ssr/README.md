# web-ssr — One Page, Rendered on the Server

`src/app.wado` builds a page through the `wado-lang:web` DOM API. On the server,
`SurfaceDom` from `wado-lang:web` answers those calls, and `to_html()`
serializes the result.

The package has two entries, and both render the same page:

```sh
wado run     # src/main.wado prints the page
wado serve   # src/service.wado serves it for every path
```
