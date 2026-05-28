// honest_bench workload 4 — http_fastify_text (perry).
//
// Plain-string response. Isolates the primitive response fast path
// (fix #1 in the Fastify-perf workstream): no JSON.stringify, no
// object allocation per response. The biggest improvement after #1
// should show up here.

import Fastify from 'fastify';

const app = Fastify({ logger: false });

app.get('/', async (_req, reply) => {
  reply.type('text/plain');
  return 'pong';
});

app.listen({ port: 18080, host: '127.0.0.1' });
