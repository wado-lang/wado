// HTTP routing benchmark server — Hono on Bun.
//
// Routing logic lives in app.routes.js, shared with the Node.js entry
// point (app.js) so both runtimes run identical routes.
//
//   bun run benchmark/http_routing/app.bun.js

import { createApp } from './app.routes.js'

const port = Number(process.env.PORT ?? 3002)
Bun.serve({ fetch: createApp().fetch, port })
