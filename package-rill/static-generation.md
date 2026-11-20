# Static Generation

A Rill app is published as static files: HTML rendered at build time, and the
same program compiled for the browser, which takes over that HTML in place.
The first target is GitHub Pages, which serves files and nothing else.

## Terms

- Static generation (SSG): the page is rendered once, at build time, into an
  HTML file.
- Server-side rendering (SSR): the page is rendered per request. GitHub Pages
  runs no code, so SSR is out of scope; it differs from SSG only in when the
  render runs.
- Hydration: the browser program walks the HTML it was served and binds its
  signals to those nodes, instead of building new ones.

## The ideal

One `mount` runs on two hosts. At build time it runs on `SurfaceDom`, and
`to_html()` gives the page. In the browser it runs on the real DOM in claim
mode: each element it would create, it takes from the served HTML instead. The
user sees the page before the Wasm loads, and nothing is rebuilt when it does.

Hydration is correct only when the browser starts from the state the build
rendered. The build renders the app's initial state, with no session run
against it. State the build reads from outside (a file, a fetch) has to reach
the browser too; see [Known gaps](#known-gaps).

Resumability, where the browser picks up serialized state without running
`mount` again, is out. Rill's state holds closures (every binding and listener
is one), and a closure cannot be serialized.

## Decisions

### Build order: parent first, in document order

Claim mode walks the served DOM from the root down, so `mount` has to build in
that order too. A `View` therefore describes the tree rather than building it:
each method adds a step, and `build` or `hydrate` runs the steps in one pass,
parent before children, siblings in order. A node exists only once that pass
reaches it, so code that needs one takes it from `with_node`.

This is a prototype, not a final decision. The API may change: Rill has no
users outside this repository.

### Mismatch: trap in development, rebuild in production

When the served HTML does not match what `mount` expects, a development build
traps, naming the node. A production build discards the subtree that differs
and builds it anew. A mismatch is a bug in the app or the build, so
development reports it, and production keeps the page working.

### The site build is a Wado program

A Wado program run with `wado run` builds the site: it renders the page on
`SurfaceDom`, writes the HTML, and compiles and bundles the browser program.
Once its shape settles, `wado build` can take the job over; that is a proposal
for later, not part of this work.

## Claim mode

A node's identity matters where something holds the node: a listener, a
binding, focus, an input's value. That is an element. Text has none of it, and
the HTML round trip does not keep text nodes apart: adjacent texts merge, and
an empty one (a `bind_text` before its first run, an anchor) is not serialized
at all.

So claim mode claims elements and builds text afresh.

- An element claims the next element child and checks its tag. Its static
  attributes are checked against what was served, a mismatch like any other.
- A `text` or `bind_text`, and the anchor of a `show` or `switch`, creates its
  node. The served text between two elements is removed once the elements
  around it are claimed.
- A bound attribute or text runs its first computation as usual. Writing a
  value the node already holds is harmless.
- A listener is attached as usual.
- `each` claims one row per initial item, in order. `show` and `switch` claim
  the subtree they show at first.

### Testing it

Hydration is tested on `SurfaceDom` alone. Build the page, join and drop its
text as the HTML parser would, then hydrate it with signals of its own. Claim
mode must create no element and keep every element's identity, and a later
write to a signal must reach the claimed node. `SurfaceDom` parses no HTML, so
a test builds the served tree by hand.

## GitHub Pages

- Pages serves no custom headers. Rill runs on one thread, so it needs none
  (no COOP or COEP).
- This repository already serves the `gh-pages` branch, for the benchmarks.
  The app goes in a directory of it, deployed in the same concurrency group,
  so every URL the page uses is relative.
- The branch needs `.nojekyll`, or files whose names start with `_` are not
  served.
- A single page needs no `404.html`. A router would, to serve its paths.

## Browser support

The browser program needs Wasm GC and JSPI, which jco's transpiled output uses
to call the DOM synchronously. Chrome 137 and Safari 27 have both. Firefox
153's JSPI is not confirmed. A browser without them still shows the served
HTML; only the bindings are missing.

## Roadmap

- [ ] Deploy the to-do example as a client-rendered app: a workflow compiles it
  for the browser, as [`example/web-browser/build.sh`](../example/web-browser/build.sh) does, and publishes it to
  `gh-pages`.
- [ ] Render the initial page at build time, and have the browser program
  replace it with a fresh `mount`. The page shows before the Wasm loads.
- [x] Make `View` describe the tree, and build it in one pass in document
  order.
- [x] Add claim mode to that pass.
- [ ] Hydrate the deployed page instead of replacing it.
- [ ] Apply the mismatch policy. A mismatch traps in every build so far.

## Known gaps

- State the build reads from outside has no way into the browser program. The
  page would need to carry it, serialized, for `mount` to start from it.
- A page with more than one route needs a page per path, or a `404.html` that
  serves the app.
