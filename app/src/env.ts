// Configuration and secrets, wherever the app runs. On Cloudflare (with nodejs_compat) secrets
// and vars appear on process.env as well; in the Node server they come from the environment (or
// app/.env). Read them when needed, not at module load: on Cloudflare they arrive with a request.

export const secret = (name: string): string | undefined => {
  const value = process.env[name];
  return value === undefined || value === '' ? undefined : value;
};

/** What the API needs from the platform it runs on. */
export interface Platform {
  /** Keeps work going after the response is returned (Cloudflare's ctx.waitUntil). */
  waitUntil(promise: Promise<unknown>): void;
  /**
   * Counts one request against a rate limit for `key` (the client's IP) and reports whether
   * it's allowed. Absent where the platform has no rate limiter (the Node server, local dev).
   */
  rateLimit?(limiter: 'render' | 'chat', key: string): Promise<boolean>;
}
