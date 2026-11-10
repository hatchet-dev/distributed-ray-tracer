// The app as one Cloudflare Worker: Hatchet's serverless endpoint for the workflows under
// /hatchet/*, the render API (app.ts), and the page (static assets from public/, served by
// Cloudflare before the Worker runs).

import { cloudflare, type HatchetEnv } from '@hatchet-dev/serverless/cloudflare';
import { handleApi } from './app';
import { workflows } from './workflows';

export default cloudflare<HatchetEnv>({
  workflows,
  fetch: async (req, _env, ctx) =>
    (await handleApi(req, { waitUntil: (p) => ctx.waitUntil(p) })) ?? new Response('Not Found', { status: 404 }),
});
