// The app as a Node server (for a container, e.g. on Azure via Porter): Hatchet's serverless
// endpoint for the workflows under /hatchet/*, the render API (app.ts), and the page (public/).
//
//   PORT          where to listen (default 8080)
//   PUBLIC_DIR    the page's files (default ./public next to the working directory)
//   plus the secrets the Worker reads: HATCHET_CLIENT_TOKEN, HATCHET_SIGNING_SECRET, OPENAI_API_KEY.
// A .env file in the working directory is loaded first, if there is one.

import { existsSync } from 'node:fs';
import { readFile, stat } from 'node:fs/promises';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { extname, join, normalize, resolve, sep } from 'node:path';
import { Readable } from 'node:stream';
import { nodeHandler } from '@hatchet-dev/serverless/node';

if (existsSync('.env')) process.loadEnvFile('.env');

// Imported after .env is loaded, in case anything reads the environment at load time.
const { handleApi } = await import('./app');
const { workflows } = await import('./workflows');

const PORT = Number(process.env.PORT ?? 8080);
const PUBLIC_DIR = resolve(process.env.PUBLIC_DIR ?? 'public');

const hatchet = nodeHandler({ workflows, basePath: '/hatchet', runtime: { name: 'node' } });
/** Work the API keeps going after a response (stream relays); logged if it fails. */
const background = new Set<Promise<unknown>>();
const waitUntil = (p: Promise<unknown>) => {
  const tracked = p.catch((err) => console.error('background work failed', err)).finally(() => background.delete(tracked));
  background.add(tracked);
};

const server = createServer(async (req, res) => {
  try {
    if (hatchet.matches(req)) return hatchet(req, res);
    if (req.url === '/healthz') return void res.writeHead(200).end('ok'); // the platform's health probe
    const request = toRequest(req, res);
    const response = await handleApi(request, { waitUntil });
    if (response) return await send(response, res);
    if (req.method === 'GET' || req.method === 'HEAD') return await serveStatic(req, res);
    res.writeHead(404).end('Not Found');
  } catch (err) {
    console.error(err);
    if (!res.headersSent) res.writeHead(500).end('Internal Server Error');
    else res.destroy();
  }
});
server.on('upgrade', (req, socket, head) => {
  if (!hatchet.upgrade(req, socket, head)) socket.destroy();
});
// Renders stream for minutes: no request or keep-alive timeouts of Node's own.
server.requestTimeout = 0;
server.listen(PORT, () => console.log(`listening on :${PORT}`));

/** A fetch Request for a Node request; its signal aborts when the client disconnects. */
function toRequest(req: IncomingMessage, res: ServerResponse): Request {
  const disconnected = new AbortController();
  res.on('close', () => {
    if (!res.writableFinished) disconnected.abort();
  });
  const headers = new Headers();
  for (const [name, value] of Object.entries(req.headers)) {
    if (Array.isArray(value)) value.forEach((v) => headers.append(name, v));
    else if (value !== undefined) headers.set(name, value);
  }
  const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
  return new Request(`http://${req.headers.host ?? 'localhost'}${req.url}`, {
    method: req.method,
    headers,
    body: hasBody ? (Readable.toWeb(req) as ReadableStream) : undefined,
    signal: disconnected.signal,
    // @ts-expect-error: Node needs this for streaming request bodies.
    duplex: 'half',
  });
}

/** Writes a fetch Response, streaming its body (Server-Sent Events flush as they come). */
async function send(response: Response, res: ServerResponse) {
  res.writeHead(response.status, Object.fromEntries(response.headers));
  res.flushHeaders();
  if (!response.body) return void res.end();
  const reader = response.body.getReader();
  res.on('close', () => reader.cancel().catch(() => {}));
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    if (!res.write(value)) await new Promise((ok) => res.once('drain', ok));
  }
  res.end();
}

const TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
  '.txt': 'text/plain; charset=utf-8',
};

/** The page's files, as Cloudflare's asset server would: `/` is index.html. */
async function serveStatic(req: IncomingMessage, res: ServerResponse) {
  const path = decodeURIComponent(new URL(req.url ?? '/', 'http://x').pathname);
  let file = normalize(join(PUBLIC_DIR, path));
  if (file !== PUBLIC_DIR && !file.startsWith(PUBLIC_DIR + sep)) return void res.writeHead(404).end('Not Found');
  const info = await stat(file).catch(() => null);
  if (info?.isDirectory()) file = join(file, 'index.html');
  const body = await readFile(file).catch(() => null);
  if (!body) return void res.writeHead(404).end('Not Found');
  res.writeHead(200, { 'Content-Type': TYPES[extname(file)] ?? 'application/octet-stream', 'Content-Length': body.length });
  res.end(req.method === 'HEAD' ? undefined : body);
}
