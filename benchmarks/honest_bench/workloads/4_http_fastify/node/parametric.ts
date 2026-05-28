// honest_bench workload 4 — http_fastify_parametric (node).

import Fastify from 'fastify';
import type { FastifyRequest } from 'fastify';

const app = Fastify({ logger: false });

app.get('/users/:id', async (req: FastifyRequest<{ Params: { id: string } }>) => ({
  id: req.params.id,
}));

app.listen({ port: 18080, host: '127.0.0.1' });
