// HTTP routing benchmark server — Hono on Node.js.
//
// Routing logic lives in app.routes.js, shared with the Bun entry
// point (app.bun.js) so both runtimes run identical routes.
//
//   WORKERS=4 node benchmark/http_routing/app.js

import cluster from 'node:cluster'
import { serve } from '@hono/node-server'
import { createApp } from './app.routes.js'

const port = Number(process.env.PORT ?? 3000)
const workers = Number(process.env.WORKERS ?? 1)

// The default SCHED_RR routes every connection through the primary, capping
// throughput well below what the workers can serve.
cluster.schedulingPolicy = cluster.SCHED_NONE

if (workers > 1 && cluster.isPrimary) {
  for (let i = 0; i < workers; i++) cluster.fork()
  const shutdown = () => {
    for (const w of Object.values(cluster.workers ?? {})) w.kill()
    process.exit(0)
  }
  process.on('SIGTERM', shutdown)
  process.on('SIGINT', shutdown)
} else {
  serve({ fetch: createApp().fetch, port })
}
