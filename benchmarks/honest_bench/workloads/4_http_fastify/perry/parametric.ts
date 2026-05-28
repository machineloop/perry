// honest_bench workload 4 — http_fastify_parametric (perry).
//
// Parametric route with a path parameter. Exercises the router
// (fix #3 radix tree), per-request params object allocation
// (fix #4 cached params), and Arc-shared request fields (fix #2
// per-request clones).

import Fastify from 'fastify';
import type { FastifyRequest } from 'fastify';

const app = Fastify({ logger: false });

app.get('/users/:id', async (req: FastifyRequest<{ Params: { id: string } }>) => ({
  id: req.params.id,
}));

app.listen({ port: 18080, host: '127.0.0.1' });
