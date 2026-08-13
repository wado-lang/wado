// HTTP routing benchmark server — Hono on Node.js over h2c.
//
// Same app as app.js; only the server is different. `node:http` and
// `node:http2` are separate servers in Node, so h2c needs its own
// process and port rather than a preface sniff on the h1 one.
//
//   PORT=3003 node benchmark/http_routing/app.h2c.js

import { serve } from '@hono/node-server'
import { createServer } from 'node:http2'
import { createApp } from './app.routes.js'

const port = Number(process.env.PORT ?? 3003)
serve({ fetch: createApp().fetch, port, createServer })
