// Abuse guards for POST /api/render, which costs real compute (tile servers) and, for chat
// turns, model calls: a per-IP rate limit (Cloudflare's rate limiting binding, when the platform
// has one) and a Cloudflare Turnstile check (when TURNSTILE_SECRET_KEY is set). Both are off
// where they aren't configured, so local development and the Node server work unchanged.

import { secret, type Platform } from './env';

const SITEVERIFY = 'https://challenges.cloudflare.com/turnstile/v0/siteverify';

/** GET /api/config: what the page needs to run the Turnstile check (null = no check). */
export const pageConfig = () => Response.json({ turnstileSiteKey: secret('TURNSTILE_SITE_KEY') ?? null });

/**
 * Checks a render request before it starts anything: null to go ahead, or the response that
 * refuses it. `chat` turns (which call the model) have their own, tighter limit.
 */
export async function guardRender(req: Request, body: Record<string, unknown>, chat: boolean, ctx: Platform): Promise<Response | null> {
  // Cloudflare sets the client's address; behind anything else every request shares one key.
  const ip = req.headers.get('CF-Connecting-IP') ?? 'unknown';

  if (ctx.rateLimit) {
    const allowed = (await ctx.rateLimit('render', ip)) && (!chat || (await ctx.rateLimit('chat', ip)));
    if (!allowed) {
      return new Response('Too many renders from your network. Wait a minute and try again.', {
        status: 429,
        headers: { 'Retry-After': '60' },
      });
    }
  }

  const turnstileSecret = secret('TURNSTILE_SECRET_KEY');
  if (turnstileSecret) {
    const token = body.turnstileToken;
    if (typeof token !== 'string' || !token || token.length > 2048) {
      return new Response('The bot check did not run. Reload the page and try again.', { status: 403 });
    }
    const form = new FormData();
    form.append('secret', turnstileSecret);
    form.append('response', token);
    if (ip !== 'unknown') form.append('remoteip', ip);
    const outcome = await fetch(SITEVERIFY, { method: 'POST', body: form })
      .then((r) => r.json() as Promise<{ success?: boolean; 'error-codes'?: string[] }>)
      .catch((err) => {
        console.error('turnstile siteverify failed', err);
        return { success: false, 'error-codes': ['siteverify-unreachable'] };
      });
    if (!outcome.success) {
      console.warn('turnstile rejected a render', outcome['error-codes']);
      return new Response('The bot check failed. Reload the page and try again.', { status: 403 });
    }
  }
  return null;
}
