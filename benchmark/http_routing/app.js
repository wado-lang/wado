// HTTP routing benchmark server — Hono on Node.js.
//
// Routing logic lives in app.routes.js, shared with the Bun entry
// point (app.bun.js) so both runtimes run identical routes.
//
//   node benchmark/http_routing/app.js

import { serve } from '@hono/node-server'
import { createApp } from './app.routes.js'

const port = Number(process.env.PORT ?? 3000)
serve({ fetch: createApp().fetch, port })
