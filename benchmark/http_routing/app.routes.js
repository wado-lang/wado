// HTTP routing benchmark — shared Hono app definition.
//
// Mirrors benchmark/http_routing/app.wado: the same route set and the
// same `{ route, params }` JSON response shape, so the comparison
// isolates routing + request handling overhead.
//
// The route set is Hono's official router benchmark
// (honojs/hono, benchmarks/routers/src/tool.mts). Imported by the
// Node.js (app.js) and Bun (app.bun.js) entry points so both run the
// identical routing logic.

import { Hono } from 'hono'

const json = (route, params = []) => ({ route, params })

export function createApp() {
  const app = new Hono()

  app.get('/user', (c) => c.json(json('user')))
  app.get('/user/comments', (c) => c.json(json('user.comments')))
  app.get('/user/avatar', (c) => c.json(json('user.avatar')))
  app.get('/user/lookup/username/:username', (c) =>
    c.json(json('user.lookup.username', [c.req.param('username')])))
  app.get('/user/lookup/email/:address', (c) =>
    c.json(json('user.lookup.email', [c.req.param('address')])))
  app.get('/event/:id', (c) => c.json(json('event.show', [c.req.param('id')])))
  app.get('/event/:id/comments', (c) =>
    c.json(json('event.comments', [c.req.param('id')])))
  app.post('/event/:id/comment', (c) =>
    c.json(json('event.comment.create', [c.req.param('id')])))
  app.get('/map/:location/events', (c) =>
    c.json(json('map.events', [c.req.param('location')])))
  app.get('/status', (c) => c.json(json('status')))
  app.get('/very/deeply/nested/route/hello/there', (c) =>
    c.json(json('deeply.nested')))
  app.get('/static/*', (c) =>
    c.json(json('static', [c.req.path.replace(/^\/static\//, '')])))

  app.notFound((c) => c.json(json('not-found'), 404))

  return app
}
