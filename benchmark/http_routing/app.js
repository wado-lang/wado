// HTTP routing benchmark server — Hono on Node.js.
//
// Mirrors benchmark/http_routing/app.wado: the same route set and the
// same `{ route, params }` JSON response shape, so the comparison
// isolates routing + request handling overhead.
//
//   node benchmark/http_routing/app.js

import { serve } from '@hono/node-server'
import { Hono } from 'hono'

const app = new Hono()

const json = (route, params = []) => ({ route, params })

// Static routes — shallow.
app.get('/', (c) => c.json(json('root')))
app.get('/health', (c) => c.json(json('health')))
app.get('/version', (c) => c.json(json('version')))
app.get('/metrics', (c) => c.json(json('metrics')))
app.get('/ping', (c) => c.json(json('ping')))

// Static routes — medium / deep.
app.get('/api/v1/health', (c) => c.json(json('api.health')))
app.get('/api/v1/version', (c) => c.json(json('api.version')))
app.get('/api/v1/status', (c) => c.json(json('api.status')))
app.get('/api/v1/users/list', (c) => c.json(json('users.list')))
app.get('/api/v1/posts/list', (c) => c.json(json('posts.list')))
app.get('/api/v1/products/list', (c) => c.json(json('products.list')))
app.get('/api/v1/orders/list', (c) => c.json(json('orders.list')))
app.get('/api/v1/comments/list', (c) => c.json(json('comments.list')))
app.get('/api/v1/admin/users/list', (c) => c.json(json('admin.users.list')))
app.get('/api/v1/admin/posts/list', (c) => c.json(json('admin.posts.list')))
app.get('/api/v1/admin/stats/revenue', (c) => c.json(json('admin.stats.revenue')))
app.get('/api/v1/admin/stats/traffic', (c) => c.json(json('admin.stats.traffic')))
app.get('/api/v1/admin/system/cache/stats', (c) => c.json(json('admin.system.cache.stats')))
app.get('/api/v1/admin/system/logs/recent', (c) => c.json(json('admin.system.logs.recent')))

// Single-parameter routes.
app.get('/api/v1/users/:id', (c) => c.json(json('users.show', [c.req.param('id')])))
app.get('/api/v1/posts/:id', (c) => c.json(json('posts.show', [c.req.param('id')])))
app.get('/api/v1/products/:id', (c) => c.json(json('products.show', [c.req.param('id')])))
app.get('/api/v1/orders/:id', (c) => c.json(json('orders.show', [c.req.param('id')])))
app.get('/api/v1/categories/:slug', (c) => c.json(json('categories.show', [c.req.param('slug')])))

// Multi-parameter routes.
app.get('/api/v1/users/:id/posts/:pid', (c) =>
  c.json(json('users.posts.show', [c.req.param('id'), c.req.param('pid')])))
app.get('/api/v1/users/:id/comments/:cid', (c) =>
  c.json(json('users.comments.show', [c.req.param('id'), c.req.param('cid')])))
app.get('/api/v1/posts/:id/comments/:cid', (c) =>
  c.json(json('posts.comments.show', [c.req.param('id'), c.req.param('cid')])))
app.get('/api/v1/users/:id/posts/:pid/comments/:cid', (c) =>
  c.json(json('users.posts.comments.show', [
    c.req.param('id'), c.req.param('pid'), c.req.param('cid'),
  ])))
app.get('/api/v1/categories/:slug/products/:pid/reviews/:rid', (c) =>
  c.json(json('categories.products.reviews.show', [
    c.req.param('slug'), c.req.param('pid'), c.req.param('rid'),
  ])))

// Mutating parametric routes.
app.post('/api/v1/users/:id', (c) => c.json(json('users.update', [c.req.param('id')])))
app.post('/api/v1/posts/:id', (c) => c.json(json('posts.update', [c.req.param('id')])))

// Wildcard route.
app.get('/static/*', (c) =>
  c.json(json('static', [c.req.path.replace(/^\/static\//, '')])))

app.notFound((c) => c.json(json('not-found'), 404))

const port = Number(process.env.PORT ?? 3000)
serve({ fetch: app.fetch, port })
