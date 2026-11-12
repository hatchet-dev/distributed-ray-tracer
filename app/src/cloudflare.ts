// The app as one Cloudflare Worker: Hatchet's serverless endpoint for the workflows under
// /hatchet/*, the render API (app.ts), and the page (static assets from public/, served by
// Cloudflare before the Worker runs).

import { cloudflare, type HatchetEnv } from '@hatchet-dev/serverless/cloudflare';
import { handleApi } from './app';
import { workflows } from './workflows';

/** A Workers rate limiting binding (`[[ratelimits]]` in wrangler.toml). */
interface RateLimiter {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}
type Env = HatchetEnv & { RENDER_LIMITER?: RateLimiter; CHAT_LIMITER?: RateLimiter };

export default cloudflare<Env>({
  workflows,
  fetch: async (req, env, ctx) =>
    (await handleApi(req, {
      waitUntil: (p) => ctx.waitUntil(p),
      rateLimit: async (limiter, key) => {
        const binding = limiter === 'chat' ? env.CHAT_LIMITER : env.RENDER_LIMITER;
        return binding ? (await binding.limit({ key })).success : true;
      },
    })) ?? new Response('Not Found', { status: 404 }),
});
